//! Explicit-opt-in authenticated companion transport for one authoritative host.
//!
//! This module deliberately owns Iroh endpoint identity, pairing capabilities,
//! device admission, and QUIC framing. The shared Axum application router is
//! reached only after an endpoint has resolved to a non-revoked device.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Method, Request};
use axum::Router;
use http_body_util::BodyExt;
use iroh::endpoint::{TransportConfig, VarInt};
use iroh::{Endpoint, RelayMap, RelayMode, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot, Semaphore};
use tokio::task::JoinSet;
use tower::ServiceExt;
use ygg_companion_protocol::{
    expect_end, finish_body, pairing_verification_phrase, write_body, write_head, write_record,
    CompanionCatalog, DevicePlatform, DeviceSummary, HttpMethod, PairingDecision,
    PairingDeviceClaim, PairingInvitation, PairingOperation, PairingReply, PairingStatusRequest,
    PairingTicket, PendingPairingState, PendingPairingSummary, RequestHead, ResponseHead,
    ResponseHeader, RouteLimits, Secret32, COMPANION_ALPN, EVENT_HEARTBEAT_RECORD, MAX_EVENT_BYTES,
    PROTOCOL_VERSION, RESET_CANCELLED, RESET_FRAME_INVALID, RESET_INTERNAL,
    RESET_PROTOCOL_MISMATCH, RESET_REPLAY_REQUIRED, RESET_REVOKED, RESET_UNAUTHORIZED,
};
use zeroize::Zeroizing;

use crate::transport::{CompanionApplication, TransportPrincipal};
use crate::{
    DeviceId, HostCommandEnvelope, LoopbackServer, ProtocolValidation, SessionCommandEnvelope,
};

const STATE_DIRECTORY: &str = "companion-v1";
const ENDPOINT_KEY_FILE: &str = "endpoint-key-v1";
const DEVICE_REGISTRY_FILE: &str = "devices-v1.json";
const MAX_REGISTRY_BYTES: usize = 256 * 1024;
const MAX_DEVICES: usize = 128;
const MAX_PENDING_PAIRINGS: usize = 3;
const INVITATION_TTL_MS: u64 = 120_000;
const MAX_HANDSHAKES: usize = 16;
const MAX_CONNECTIONS: usize = 16;
const MAX_UNPAIRED_CONNECTIONS: usize = MAX_PENDING_PAIRINGS;
const MAX_CONNECTIONS_PER_DEVICE: usize = 4;
const MAX_STREAMS: usize = 32;
const MAX_UNPAIRED_STREAMS: usize = MAX_PENDING_PAIRINGS;
const MAX_STREAMS_PER_CONNECTION: usize = 8;
const MAX_STREAMS_PER_DEVICE: usize = MAX_STREAMS_PER_CONNECTION;
const MAX_UNPAIRED_OPERATIONS_PER_CONNECTION: usize = 96;
const UNPAIRED_CONNECTION_LIFETIME: Duration = Duration::from_secs(150);
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_WRITE_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(not(test))]
const EVENT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(test)]
const EVENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(50);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const RESPONSE_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_BODY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const APPLICATION_TIMEOUT: Duration = Duration::from_secs(60);
const RELAY_ONLINE_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const LAST_SEEN_WRITE_INTERVAL_MS: u64 = 60_000;
const PAIRING_RESPONSE_BYTES: usize = 16 * 1024;

/// Explicit relay selection for companion mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompanionRelay {
    /// The reviewed n0 production relay locations, constructed as a custom map.
    N0,
}

/// Persistent and public host inputs needed by companion mode.
#[derive(Clone, Debug)]
pub struct CompanionConfig {
    /// Existing protected Serve state directory (`<session-root>/.serve`).
    pub serve_state_dir: PathBuf,
    /// Stable public Ygg host identifier.
    pub host_id: String,
    /// Explicitly selected relay provider.
    pub relay: CompanionRelay,
}

/// Companion startup or durable-state failure.
#[derive(Debug, thiserror::Error)]
pub enum CompanionError {
    /// Companion state could not be safely opened or committed.
    #[error("companion state is unavailable")]
    State,
    /// Persisted companion state is malformed or inconsistent.
    #[error("companion state is invalid")]
    InvalidState,
    /// The explicitly selected relay transport could not become available.
    #[error("companion relay transport is unavailable")]
    RelayUnavailable,
    /// The companion runtime task failed.
    #[error("companion runtime task failed")]
    Task,
}

/// Owner-facing pairing or registry operation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompanionControlError {
    /// Companion networking is not currently healthy.
    #[error("companion networking is unavailable")]
    Unavailable,
    /// The requested pairing or device does not exist.
    #[error("the companion record was not found")]
    NotFound,
    /// Pairing capacity has been reached.
    #[error("companion pairing is at capacity")]
    Capacity,
    /// The requested transition conflicts with current state.
    #[error("the companion transition conflicts with current state")]
    Conflict,
    /// A durable registry update failed.
    #[error("the companion registry update failed")]
    Storage,
}

/// Cloneable owner control plane. This type never exposes endpoint key bytes.
#[derive(Clone)]
pub struct CompanionControl {
    inner: Arc<ControlInner>,
}

impl fmt::Debug for CompanionControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionControl")
            .field("host_id", &self.inner.host_id)
            .field("endpoint_id", &self.inner.endpoint_id)
            .finish_non_exhaustive()
    }
}

struct ControlInner {
    host_id: String,
    endpoint_id: String,
    endpoint_secret: EndpointSecret,
    relay: CompanionRelay,
    state_dir: PathBuf,
    state: StdMutex<ControlState>,
    endpoint_info: StdMutex<Option<EndpointInfo>>,
    active_connections: StdMutex<HashMap<String, ActiveDeviceAdmission>>,
    revoked: broadcast::Sender<String>,
}

#[derive(Clone)]
struct ActiveDeviceAdmission {
    connections: usize,
    streams: Arc<Semaphore>,
}

struct EndpointSecret(SecretKey);

impl EndpointSecret {
    fn clone_for_endpoint(&self) -> SecretKey {
        self.0.clone()
    }
}

impl fmt::Debug for EndpointSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EndpointSecret([REDACTED])")
    }
}

#[derive(Clone)]
struct EndpointInfo {
    relay_urls: Vec<String>,
    direct_addresses: Vec<String>,
}

struct ControlState {
    registry: RegistryDocument,
    invitation: Option<InvitationState>,
    pending: BTreeMap<String, PendingState>,
    uncommitted_revocations: BTreeSet<String>,
}

struct InvitationState {
    secret: Secret32,
    ticket: Zeroizing<String>,
    expires_at_ms: u64,
    deadline: Instant,
}

struct PendingState {
    request_id: String,
    endpoint_id: String,
    client_nonce: String,
    poll_token: Secret32,
    device: PairingDeviceClaim,
    phrase: String,
    expires_at_ms: u64,
    deadline: Instant,
    decision: PendingDecision,
}

#[derive(Clone)]
enum PendingDecision {
    Pending,
    Approved {
        device_id: DeviceId,
        approved_at_ms: u64,
    },
    Denied,
    Cancelled,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryDocument {
    version: u16,
    host_id: String,
    host_endpoint_id: String,
    revision: u64,
    devices: Vec<RegistryDevice>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegistryDevice {
    id: DeviceId,
    endpoint_id: String,
    name: String,
    platform: DevicePlatform,
    paired_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revoked_at_ms: Option<u64>,
}

impl RegistryDevice {
    fn summary(&self, connected: bool) -> DeviceSummary {
        DeviceSummary {
            id: self.id.to_string(),
            name: self.name.clone(),
            platform: self.platform,
            paired_at_ms: self.paired_at_ms,
            last_seen_at_ms: self.last_seen_at_ms,
            revoked_at_ms: self.revoked_at_ms,
            connected: connected && self.revoked_at_ms.is_none(),
        }
    }
}

impl CompanionControl {
    /// Reports whether this Serve state root already contains durable companion state.
    ///
    /// Callers use this before creating a replacement host identity: once the companion
    /// directory exists, a missing host identity must fail closed instead of being regenerated.
    pub fn has_persisted_state(serve_state_dir: &Path) -> Result<bool, CompanionError> {
        let state_dir = serve_state_dir.join(STATE_DIRECTORY);
        match state_dir.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                Ok(true)
            }
            Ok(_) => Err(CompanionError::InvalidState),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(CompanionError::State),
        }
    }

    /// Opens or creates protected companion state without starting networking.
    pub fn open(config: CompanionConfig) -> Result<Self, CompanionError> {
        let state_dir = config.serve_state_dir.join(STATE_DIRECTORY);
        ensure_private_directory(&state_dir).map_err(|_| CompanionError::State)?;

        let registry_path = state_dir.join(DEVICE_REGISTRY_FILE);
        let loaded_registry = load_registry(&registry_path)?;
        let commit_initial_registry = loaded_registry.is_none();
        let endpoint_secret = load_or_create_endpoint_secret(
            &state_dir.join(ENDPOINT_KEY_FILE),
            loaded_registry.is_some(),
        )?;
        let endpoint_id = endpoint_secret.0.public().to_string();
        let registry = match loaded_registry {
            Some(registry) => {
                if registry.host_id != config.host_id || registry.host_endpoint_id != endpoint_id {
                    return Err(CompanionError::InvalidState);
                }
                registry
            }
            None => RegistryDocument {
                version: PROTOCOL_VERSION,
                host_id: config.host_id.clone(),
                host_endpoint_id: endpoint_id.clone(),
                revision: 0,
                devices: Vec::new(),
            },
        };
        validate_registry(&registry)?;
        if commit_initial_registry {
            persist_registry(&state_dir, &registry).map_err(|_| CompanionError::State)?;
        }
        let (revoked, _) = broadcast::channel(MAX_DEVICES);

        Ok(Self {
            inner: Arc::new(ControlInner {
                host_id: config.host_id,
                endpoint_id,
                endpoint_secret,
                relay: config.relay,
                state_dir,
                state: StdMutex::new(ControlState {
                    registry,
                    invitation: None,
                    pending: BTreeMap::new(),
                    uncommitted_revocations: BTreeSet::new(),
                }),
                endpoint_info: StdMutex::new(None),
                active_connections: StdMutex::new(HashMap::new()),
                revoked,
            }),
        })
    }

    /// Returns whether the endpoint has successfully reached its selected relay.
    pub fn is_healthy(&self) -> bool {
        self.inner
            .endpoint_info
            .lock()
            .expect("companion endpoint info poisoned")
            .is_some()
    }

    /// Returns authoritative owner-visible device and pending-pairing state.
    pub fn catalog(&self) -> CompanionCatalog {
        let now = now_ms();
        let monotonic_now = Instant::now();
        let active = self
            .inner
            .active_connections
            .lock()
            .expect("companion connection state poisoned")
            .clone();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        cleanup_expired(&mut state, now, monotonic_now);
        CompanionCatalog {
            revision: state.registry.revision,
            devices: state
                .registry
                .devices
                .iter()
                .map(|device| {
                    device.summary(
                        active
                            .get(&device.endpoint_id)
                            .is_some_and(|admission| admission.connections > 0),
                    )
                })
                .collect(),
            pending: state
                .pending
                .values()
                .filter(|pending| {
                    matches!(
                        pending.decision,
                        PendingDecision::Pending | PendingDecision::Approved { .. }
                    )
                })
                .map(|pending| PendingPairingSummary {
                    request_id: pending.request_id.clone(),
                    device: pending.device.clone(),
                    state: match &pending.decision {
                        PendingDecision::Pending => PendingPairingState::Pending,
                        PendingDecision::Approved { .. } => PendingPairingState::Approved,
                        PendingDecision::Denied | PendingDecision::Cancelled => {
                            unreachable!("terminal pairing states were filtered")
                        }
                    },
                    phrase: pending.phrase.clone(),
                    expires_at_ms: pending.expires_at_ms,
                })
                .collect(),
            invitation_expires_at_ms: state
                .invitation
                .as_ref()
                .map(|invitation| invitation.expires_at_ms),
        }
    }

    /// Opens one idempotent, short-lived invitation.
    pub fn open_pairing(&self) -> Result<PairingInvitation, CompanionControlError> {
        let endpoint_info = self
            .inner
            .endpoint_info
            .lock()
            .expect("companion endpoint info poisoned")
            .clone()
            .ok_or(CompanionControlError::Unavailable)?;
        let now = now_ms();
        let monotonic_now = Instant::now();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        cleanup_expired(&mut state, now, monotonic_now);
        if let Some(invitation) = &state.invitation {
            return Ok(PairingInvitation {
                ticket: invitation.ticket.as_str().to_owned(),
                expires_at_ms: invitation.expires_at_ms,
            });
        }
        let expires_at_ms = now.saturating_add(INVITATION_TTL_MS);
        let deadline = monotonic_now + Duration::from_millis(INVITATION_TTL_MS);
        let secret = random_secret().map_err(|_| CompanionControlError::Storage)?;
        let ticket = PairingTicket {
            protocol: PROTOCOL_VERSION,
            host_id: self.inner.host_id.clone(),
            host_endpoint_id: self.inner.endpoint_id.clone(),
            relay_urls: endpoint_info.relay_urls,
            direct_addresses: endpoint_info.direct_addresses,
            invitation: secret.clone(),
            expires_at_ms,
        };
        let encoded = Zeroizing::new(
            ticket
                .encode()
                .map_err(|_| CompanionControlError::Storage)?,
        );
        state.invitation = Some(InvitationState {
            secret,
            ticket: encoded.clone(),
            expires_at_ms,
            deadline,
        });
        Ok(PairingInvitation {
            ticket: encoded.as_str().to_owned(),
            expires_at_ms,
        })
    }

    /// Closes the invitation and cancels all unacknowledged requests.
    pub fn close_pairing(&self) {
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        state.invitation = None;
        for pending in state.pending.values_mut() {
            if matches!(
                pending.decision,
                PendingDecision::Pending | PendingDecision::Approved { .. }
            ) {
                pending.decision = PendingDecision::Cancelled;
            }
        }
    }

    /// Applies an idempotent local-owner pairing decision.
    pub fn decide_pairing(
        &self,
        request_id: &str,
        decision: PairingDecision,
    ) -> Result<(), CompanionControlError> {
        let now = now_ms();
        let monotonic_now = Instant::now();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        cleanup_expired(&mut state, now, monotonic_now);
        let pending = state
            .pending
            .get_mut(request_id)
            .ok_or(CompanionControlError::NotFound)?;
        match (&pending.decision, decision) {
            (PendingDecision::Pending, PairingDecision::Approve) => {
                let device_id = DeviceId::new(format!(
                    "device-{}",
                    random_hex(16).map_err(|_| CompanionControlError::Storage)?
                ))
                .map_err(|_| CompanionControlError::Storage)?;
                pending.decision = PendingDecision::Approved {
                    device_id,
                    approved_at_ms: now,
                };
                Ok(())
            }
            (PendingDecision::Pending, PairingDecision::Deny) => {
                pending.decision = PendingDecision::Denied;
                Ok(())
            }
            (PendingDecision::Approved { .. }, PairingDecision::Approve)
            | (PendingDecision::Denied, PairingDecision::Deny) => Ok(()),
            (PendingDecision::Cancelled, _) => Err(CompanionControlError::Conflict),
            _ => Err(CompanionControlError::Conflict),
        }
    }

    /// Durably revokes one assigned device before active connections are closed.
    pub fn revoke_device(&self, device_id: &str) -> Result<(), CompanionControlError> {
        let now = now_ms();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        let Some((endpoint_id, already_revoked)) = state
            .registry
            .devices
            .iter()
            .find(|device| device.id.as_str() == device_id)
            .map(|device| (device.endpoint_id.clone(), device.revoked_at_ms.is_some()))
        else {
            return Err(CompanionControlError::NotFound);
        };
        if already_revoked {
            if !state.uncommitted_revocations.contains(&endpoint_id) {
                return Ok(());
            }
            let registry = state.registry.clone();
            persist_registry(&self.inner.state_dir, &registry)
                .map_err(|_| CompanionControlError::Storage)?;
            state.uncommitted_revocations.remove(&endpoint_id);
            return Ok(());
        }
        let mut next = state.registry.clone();
        let device = next
            .devices
            .iter_mut()
            .find(|device| device.id.as_str() == device_id)
            .expect("registry device disappeared from clone");
        device.revoked_at_ms = Some(now);
        next.revision = next.revision.saturating_add(1);
        let persisted = persist_registry(&self.inner.state_dir, &next);
        state.registry = next;
        if persisted.is_err() {
            state.uncommitted_revocations.insert(endpoint_id.clone());
        } else {
            state.uncommitted_revocations.remove(&endpoint_id);
        }
        drop(state);

        // A storage failure must not leave the device authorized in this
        // process. The owner sees the failure and can retry the same operation
        // to durably commit the already-effective revocation.
        let _ = self.inner.revoked.send(endpoint_id);
        persisted.map_err(|_| CompanionControlError::Storage)
    }

    fn set_endpoint_info(&self, info: EndpointInfo) {
        *self
            .inner
            .endpoint_info
            .lock()
            .expect("companion endpoint info poisoned") = Some(info);
    }

    fn clear_endpoint_info(&self) {
        *self
            .inner
            .endpoint_info
            .lock()
            .expect("companion endpoint info poisoned") = None;
    }

    fn resolve_endpoint(&self, endpoint_id: &str) -> Option<DeviceId> {
        let state = self.inner.state.lock().expect("companion state poisoned");
        state
            .registry
            .devices
            .iter()
            .find(|device| device.endpoint_id == endpoint_id && device.revoked_at_ms.is_none())
            .map(|device| device.id.clone())
    }

    fn endpoint_is_revoked(&self, endpoint_id: &str) -> bool {
        let state = self.inner.state.lock().expect("companion state poisoned");
        state
            .registry
            .devices
            .iter()
            .any(|device| device.endpoint_id == endpoint_id && device.revoked_at_ms.is_some())
    }

    fn admit_connection(&self, endpoint_id: &str) -> ConnectionAdmission {
        let Some(device_id) = self.resolve_endpoint(endpoint_id) else {
            return ConnectionAdmission::Unpaired;
        };
        let mut active = self
            .inner
            .active_connections
            .lock()
            .expect("companion connection state poisoned");
        let admission =
            active
                .entry(endpoint_id.to_owned())
                .or_insert_with(|| ActiveDeviceAdmission {
                    connections: 0,
                    streams: Arc::new(Semaphore::new(MAX_STREAMS_PER_DEVICE)),
                });
        if admission.connections >= MAX_CONNECTIONS_PER_DEVICE {
            return ConnectionAdmission::AtCapacity;
        }
        admission.connections += 1;
        let streams = Arc::clone(&admission.streams);
        drop(active);
        self.mark_seen(endpoint_id);
        ConnectionAdmission::Paired(ConnectionGuard {
            control: self.clone(),
            endpoint_id: endpoint_id.to_owned(),
            device_id,
            streams,
        })
    }

    fn mark_seen(&self, endpoint_id: &str) {
        let now = now_ms();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        let Some(current) = state
            .registry
            .devices
            .iter()
            .find(|device| device.endpoint_id == endpoint_id)
        else {
            return;
        };
        if current
            .last_seen_at_ms
            .is_some_and(|last| now.saturating_sub(last) < LAST_SEEN_WRITE_INTERVAL_MS)
        {
            return;
        }
        let mut next = state.registry.clone();
        let Some(device) = next
            .devices
            .iter_mut()
            .find(|device| device.endpoint_id == endpoint_id)
        else {
            return;
        };
        device.last_seen_at_ms = Some(now);
        next.revision = next.revision.saturating_add(1);
        if persist_registry(&self.inner.state_dir, &next).is_ok() {
            state.registry = next;
        }
    }

    fn pair(
        &self,
        endpoint_id: &str,
        operation: PairingOperation,
    ) -> Result<PairingReply, PairingWireError> {
        let now = now_ms();
        let monotonic_now = Instant::now();
        let mut state = self.inner.state.lock().expect("companion state poisoned");
        cleanup_expired(&mut state, now, monotonic_now);
        if matches!(
            &operation,
            PairingOperation::Status(_) | PairingOperation::Ack(_) | PairingOperation::Cancel(_)
        ) {
            if let Some(device) = state
                .registry
                .devices
                .iter()
                .find(|device| device.endpoint_id == endpoint_id)
            {
                if device.revoked_at_ms.is_some() {
                    return Err(PairingWireError::Revoked);
                }
                return Ok(PairingReply::Acknowledged {
                    device: device.summary(false),
                });
            }
        }
        match operation {
            PairingOperation::Request(request) => {
                if self.endpoint_is_revoked_locked(&state, endpoint_id) {
                    return Err(PairingWireError::Revoked);
                }
                if state
                    .registry
                    .devices
                    .iter()
                    .any(|device| device.endpoint_id == endpoint_id)
                {
                    return Err(PairingWireError::Conflict);
                }
                if let Some(existing) = state.pending.values().find(|pending| {
                    pending.endpoint_id == endpoint_id
                        && pending.client_nonce == request.client_nonce
                }) {
                    return Ok(PairingReply::PendingRequest {
                        request_id: existing.request_id.clone(),
                        poll_token: existing.poll_token.clone(),
                        phrase: existing.phrase.clone(),
                        expires_at_ms: existing.expires_at_ms,
                    });
                }
                if state.pending.len() >= MAX_PENDING_PAIRINGS {
                    return Err(PairingWireError::Capacity);
                }
                if request.observed_host_id != self.inner.host_id
                    || request.observed_host_endpoint_id != self.inner.endpoint_id
                {
                    return Err(PairingWireError::IdentityMismatch);
                }
                let Some(invitation) = state.invitation.as_ref() else {
                    return Err(PairingWireError::InvalidCapability);
                };
                if pairing_expired(
                    invitation.expires_at_ms,
                    invitation.deadline,
                    now,
                    monotonic_now,
                ) || !invitation.secret.constant_time_eq(&request.invitation)
                {
                    return Err(PairingWireError::InvalidCapability);
                }
                let phrase = pairing_verification_phrase(
                    &self.inner.host_id,
                    &self.inner.endpoint_id,
                    endpoint_id,
                    &request.client_nonce,
                    &invitation.secret,
                );
                let request_id = format!(
                    "pair-{}",
                    random_hex(16).map_err(|_| PairingWireError::Internal)?
                );
                let poll_token = random_secret().map_err(|_| PairingWireError::Internal)?;
                let invitation = state
                    .invitation
                    .take()
                    .expect("validated invitation disappeared");
                let pending = PendingState {
                    request_id: request_id.clone(),
                    endpoint_id: endpoint_id.to_owned(),
                    client_nonce: request.client_nonce,
                    poll_token: poll_token.clone(),
                    device: request.device,
                    phrase: phrase.clone(),
                    expires_at_ms: invitation.expires_at_ms,
                    deadline: invitation.deadline,
                    decision: PendingDecision::Pending,
                };
                state.pending.insert(request_id.clone(), pending);
                Ok(PairingReply::PendingRequest {
                    request_id,
                    poll_token,
                    phrase,
                    expires_at_ms: invitation.expires_at_ms,
                })
            }
            PairingOperation::Status(proof) => {
                pairing_status(&state, endpoint_id, &proof, now, monotonic_now)
            }
            PairingOperation::Cancel(proof) => {
                let pending =
                    pairing_pending_mut(&mut state, endpoint_id, &proof, now, monotonic_now)?;
                if matches!(
                    pending.decision,
                    PendingDecision::Pending | PendingDecision::Approved { .. }
                ) {
                    pending.decision = PendingDecision::Cancelled;
                }
                Ok(PairingReply::Cancelled)
            }
            PairingOperation::Ack(proof) => {
                let (device_id, approved_at_ms, device_claim) = {
                    let pending =
                        pairing_pending_mut(&mut state, endpoint_id, &proof, now, monotonic_now)?;
                    let (device_id, approved_at_ms) = match &pending.decision {
                        PendingDecision::Approved {
                            device_id,
                            approved_at_ms,
                        } => (device_id.clone(), *approved_at_ms),
                        PendingDecision::Denied => return Ok(PairingReply::Denied),
                        PendingDecision::Cancelled => return Ok(PairingReply::Cancelled),
                        PendingDecision::Pending => return Err(PairingWireError::NotApproved),
                    };
                    (device_id, approved_at_ms, pending.device.clone())
                };
                if state.registry.devices.len() >= MAX_DEVICES {
                    return Err(PairingWireError::Capacity);
                }
                if state
                    .registry
                    .devices
                    .iter()
                    .any(|device| device.endpoint_id == endpoint_id || device.id == device_id)
                {
                    return Err(PairingWireError::Conflict);
                }
                let mut next = state.registry.clone();
                next.devices.push(RegistryDevice {
                    id: device_id.clone(),
                    endpoint_id: endpoint_id.to_owned(),
                    name: device_claim.name,
                    platform: device_claim.platform,
                    paired_at_ms: approved_at_ms,
                    last_seen_at_ms: Some(now),
                    revoked_at_ms: None,
                });
                next.revision = next.revision.saturating_add(1);
                persist_registry(&self.inner.state_dir, &next)
                    .map_err(|_| PairingWireError::Internal)?;
                let summary = next
                    .devices
                    .iter()
                    .find(|device| device.id == device_id)
                    .expect("just-persisted device missing")
                    .summary(false);
                state.registry = next;
                let consumed = state
                    .pending
                    .remove(&proof.request_id)
                    .expect("validated pairing request missing");
                drop(consumed);
                Ok(PairingReply::Acknowledged { device: summary })
            }
        }
    }

    fn endpoint_is_revoked_locked(&self, state: &ControlState, endpoint_id: &str) -> bool {
        state
            .registry
            .devices
            .iter()
            .any(|device| device.endpoint_id == endpoint_id && device.revoked_at_ms.is_some())
    }
}

enum ConnectionAdmission {
    Unpaired,
    Paired(ConnectionGuard),
    AtCapacity,
}

struct ConnectionGuard {
    control: CompanionControl,
    endpoint_id: String,
    device_id: DeviceId,
    streams: Arc<Semaphore>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut active = self
            .control
            .inner
            .active_connections
            .lock()
            .expect("companion connection state poisoned");
        let remove = if let Some(admission) = active.get_mut(&self.endpoint_id) {
            admission.connections = admission.connections.saturating_sub(1);
            admission.connections == 0
        } else {
            false
        };
        if remove {
            active.remove(&self.endpoint_id);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum PairingWireError {
    InvalidCapability,
    IdentityMismatch,
    NotFound,
    Expired,
    NotApproved,
    Capacity,
    Conflict,
    Revoked,
    Internal,
}

impl PairingWireError {
    fn status(self) -> u16 {
        match self {
            Self::InvalidCapability => 401,
            Self::IdentityMismatch => 409,
            Self::NotFound => 404,
            Self::Expired => 410,
            Self::NotApproved | Self::Conflict => 409,
            Self::Capacity => 429,
            Self::Revoked => 403,
            Self::Internal => 500,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::InvalidCapability => "invalidCapability",
            Self::IdentityMismatch => "identityMismatch",
            Self::NotFound => "notFound",
            Self::Expired => "expired",
            Self::NotApproved => "approvalRequired",
            Self::Capacity => "capacity",
            Self::Conflict => "conflict",
            Self::Revoked => "revoked",
            Self::Internal => "internal",
        }
    }
}

fn pairing_pending_mut<'a>(
    state: &'a mut ControlState,
    endpoint_id: &str,
    proof: &PairingStatusRequest,
    now: u64,
    monotonic_now: Instant,
) -> Result<&'a mut PendingState, PairingWireError> {
    let pending = state
        .pending
        .get_mut(&proof.request_id)
        .ok_or(PairingWireError::NotFound)?;
    if pairing_expired(pending.expires_at_ms, pending.deadline, now, monotonic_now) {
        return Err(PairingWireError::Expired);
    }
    if pending.endpoint_id != endpoint_id || !pending.poll_token.constant_time_eq(&proof.poll_token)
    {
        return Err(PairingWireError::InvalidCapability);
    }
    Ok(pending)
}

fn pairing_status(
    state: &ControlState,
    endpoint_id: &str,
    proof: &PairingStatusRequest,
    now: u64,
    monotonic_now: Instant,
) -> Result<PairingReply, PairingWireError> {
    let Some(pending) = state.pending.get(&proof.request_id) else {
        // Expired requests are pruned eagerly. Registered endpoints were handled
        // before this lookup, so absence now means there is no durable authority
        // to recover and is safely terminal for the pending mobile operation.
        return Ok(PairingReply::Expired);
    };
    if pairing_expired(pending.expires_at_ms, pending.deadline, now, monotonic_now) {
        return Ok(PairingReply::Expired);
    }
    if pending.endpoint_id != endpoint_id || !pending.poll_token.constant_time_eq(&proof.poll_token)
    {
        return Err(PairingWireError::InvalidCapability);
    }
    match &pending.decision {
        PendingDecision::Pending => Ok(PairingReply::Pending {
            phrase: pending.phrase.clone(),
            expires_at_ms: pending.expires_at_ms,
        }),
        PendingDecision::Approved {
            device_id,
            approved_at_ms,
        } => Ok(PairingReply::Approved {
            device: DeviceSummary {
                id: device_id.to_string(),
                name: pending.device.name.clone(),
                platform: pending.device.platform,
                paired_at_ms: *approved_at_ms,
                last_seen_at_ms: None,
                revoked_at_ms: None,
                connected: false,
            },
        }),
        PendingDecision::Denied => Ok(PairingReply::Denied),
        PendingDecision::Cancelled => Ok(PairingReply::Cancelled),
    }
}

fn pairing_expired(
    expires_at_ms: u64,
    deadline: Instant,
    now: u64,
    monotonic_now: Instant,
) -> bool {
    expires_at_ms <= now || deadline <= monotonic_now
}

fn cleanup_expired(state: &mut ControlState, now: u64, monotonic_now: Instant) {
    if state.invitation.as_ref().is_some_and(|invitation| {
        pairing_expired(
            invitation.expires_at_ms,
            invitation.deadline,
            now,
            monotonic_now,
        )
    }) {
        state.invitation = None;
    }
    state.pending.retain(|_, pending| {
        !pairing_expired(pending.expires_at_ms, pending.deadline, now, monotonic_now)
    });
}

/// Running Iroh accept loop. Drop requests local shutdown; [`shutdown`](Self::shutdown)
/// additionally waits for endpoint closure.
pub struct CompanionRuntime {
    endpoint: Endpoint,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    control: CompanionControl,
}

impl fmt::Debug for CompanionRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompanionRuntime")
            .field("endpoint_id", &self.endpoint.id())
            .finish_non_exhaustive()
    }
}

impl CompanionRuntime {
    /// Binds the explicit custom relay endpoint and starts bounded admission.
    pub async fn start(
        control: CompanionControl,
        server: &LoopbackServer,
    ) -> Result<Self, CompanionError> {
        let application = server.companion_application();
        let relay_map = explicit_relay_map(control.inner.relay);
        let endpoint = Endpoint::empty_builder(RelayMode::Custom(relay_map))
            .secret_key(control.inner.endpoint_secret.clone_for_endpoint())
            .alpns(vec![COMPANION_ALPN.to_vec()])
            .transport_config(companion_transport_config())
            .bind()
            .await
            .map_err(|_| CompanionError::RelayUnavailable)?;
        tokio::time::timeout(RELAY_ONLINE_TIMEOUT, endpoint.online())
            .await
            .map_err(|_| CompanionError::RelayUnavailable)?;
        let endpoint_addr = endpoint.addr();
        let relay_urls = endpoint_addr
            .relay_urls()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if relay_urls.is_empty() {
            endpoint.close().await;
            return Err(CompanionError::RelayUnavailable);
        }
        control.set_endpoint_info(EndpointInfo {
            relay_urls,
            direct_addresses: endpoint_addr
                .ip_addrs()
                .take(16)
                .map(ToString::to_string)
                .collect(),
        });

        let (shutdown, shutdown_rx) = oneshot::channel();
        let task_endpoint = endpoint.clone();
        let task_control = control.clone();
        let task = tokio::spawn(async move {
            accept_loop(task_endpoint, task_control, application, shutdown_rx).await;
        });
        Ok(Self {
            endpoint,
            shutdown: Some(shutdown),
            task: Some(task),
            control,
        })
    }

    /// Returns the persistent host endpoint identity.
    pub fn endpoint_id(&self) -> String {
        self.endpoint.id().to_string()
    }

    /// Stops admission, closes all connections, and waits for the accept task.
    pub async fn shutdown(mut self) -> Result<(), CompanionError> {
        self.signal_shutdown();
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, self.endpoint.close()).await;
        self.control.clear_endpoint_info();
        let Some(mut task) = self.task.take() else {
            return Ok(());
        };
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task).await {
            Ok(result) => result.map_err(|_| CompanionError::Task),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(CompanionError::Task)
            }
        }
    }

    fn signal_shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

impl Drop for CompanionRuntime {
    fn drop(&mut self) {
        self.control.clear_endpoint_info();
        self.signal_shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn accept_loop(
    endpoint: Endpoint,
    control: CompanionControl,
    application: CompanionApplication,
    mut shutdown: oneshot::Receiver<()>,
) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let unpaired_connections = Arc::new(Semaphore::new(MAX_UNPAIRED_CONNECTIONS));
    let streams = Arc::new(Semaphore::new(MAX_STREAMS));
    let unpaired_streams = Arc::new(Semaphore::new(MAX_UNPAIRED_STREAMS));
    let mut connection_tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            completed = connection_tasks.join_next(), if !connection_tasks.is_empty() => {
                let _ = completed;
            }
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let Ok(handshake_permit) = handshakes.clone().try_acquire_owned() else {
                    incoming.ignore();
                    continue;
                };
                let control = control.clone();
                let application = application.clone();
                let connections = connections.clone();
                let unpaired_connections = unpaired_connections.clone();
                let streams = streams.clone();
                let unpaired_streams = unpaired_streams.clone();
                connection_tasks.spawn(async move {
                    let Ok(Ok(connection)) =
                        tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming).await
                    else {
                        return;
                    };
                    drop(handshake_permit);
                    handle_connection(
                        connection,
                        control,
                        application,
                        connections,
                        unpaired_connections,
                        streams,
                        unpaired_streams,
                    )
                    .await;
                });
            }
        }
    }
    endpoint.close().await;
    connection_tasks.abort_all();
    while connection_tasks.join_next().await.is_some() {}
}

async fn handle_connection(
    connection: iroh::endpoint::Connection,
    control: CompanionControl,
    application: CompanionApplication,
    global_connections: Arc<Semaphore>,
    unpaired_connections: Arc<Semaphore>,
    global_streams: Arc<Semaphore>,
    unpaired_streams: Arc<Semaphore>,
) {
    let endpoint_id = connection.remote_id().to_string();
    let mut revoked = control.inner.revoked.subscribe();
    if control.endpoint_is_revoked(&endpoint_id) {
        connection.close(RESET_REVOKED.into(), b"revoked");
        return;
    }
    let (connection_guard, device_id, device_streams, unpaired_connection_permit) =
        match control.admit_connection(&endpoint_id) {
            ConnectionAdmission::Unpaired => {
                let Ok(permit) = unpaired_connections.try_acquire_owned() else {
                    connection.close(RESET_UNAUTHORIZED.into(), b"unpaired capacity");
                    return;
                };
                (None, None, None, Some(permit))
            }
            ConnectionAdmission::Paired(guard) => {
                let device_id = guard.device_id.clone();
                let streams = Arc::clone(&guard.streams);
                (Some(guard), Some(device_id), Some(streams), None)
            }
            ConnectionAdmission::AtCapacity => {
                connection.close(RESET_INTERNAL.into(), b"connection capacity");
                return;
            }
        };
    let Ok(connection_permit) = global_connections.try_acquire_owned() else {
        connection.close(RESET_INTERNAL.into(), b"connection capacity");
        return;
    };
    if control.endpoint_is_revoked(&endpoint_id) {
        connection.close(RESET_REVOKED.into(), b"revoked");
        return;
    }

    let is_unpaired = device_id.is_none();
    let local_stream_limit = if is_unpaired {
        1
    } else {
        MAX_STREAMS_PER_CONNECTION
    };
    let local_streams = Arc::new(Semaphore::new(local_stream_limit));
    let connection_lifetime = async move {
        if is_unpaired {
            tokio::time::sleep(UNPAIRED_CONNECTION_LIFETIME).await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(connection_lifetime);
    let mut unpaired_operations = 0usize;
    let mut stream_tasks = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut connection_lifetime => {
                connection.close(RESET_UNAUTHORIZED.into(), b"unpaired lifetime");
                break;
            }
            completed = stream_tasks.join_next(), if !stream_tasks.is_empty() => {
                let _ = completed;
            }
            revoked_endpoint = revoked.recv() => {
                match revoked_endpoint {
                    Ok(revoked_endpoint) if revoked_endpoint == endpoint_id => {
                        connection.close(RESET_REVOKED.into(), b"revoked");
                        break;
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                        if control.endpoint_is_revoked(&endpoint_id) {
                            connection.close(RESET_REVOKED.into(), b"revoked");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            stream = connection.accept_bi() => {
                let Ok((send, recv)) = stream else { break };
                if is_unpaired {
                    if unpaired_operations >= MAX_UNPAIRED_OPERATIONS_PER_CONNECTION {
                        reset_stream(send, recv, RESET_UNAUTHORIZED);
                        connection.close(RESET_UNAUTHORIZED.into(), b"unpaired operation limit");
                        break;
                    }
                    unpaired_operations += 1;
                }
                let Ok(global_permit) = global_streams.clone().try_acquire_owned() else {
                    reset_stream(send, recv, RESET_INTERNAL);
                    continue;
                };
                let Ok(local_permit) = local_streams.clone().try_acquire_owned() else {
                    reset_stream(send, recv, RESET_INTERNAL);
                    continue;
                };
                let admission_permit = if let Some(device_streams) = &device_streams {
                    let Ok(permit) = device_streams.clone().try_acquire_owned() else {
                        reset_stream(send, recv, RESET_INTERNAL);
                        continue;
                    };
                    permit
                } else {
                    let Ok(permit) = unpaired_streams.clone().try_acquire_owned() else {
                        reset_stream(send, recv, RESET_UNAUTHORIZED);
                        continue;
                    };
                    permit
                };
                let control = control.clone();
                let application = application.clone();
                let endpoint_id = endpoint_id.clone();
                let device_id = device_id.clone();
                stream_tasks.spawn(async move {
                    let _global_permit = global_permit;
                    let _local_permit = local_permit;
                    let _admission_permit = admission_permit;
                    handle_stream(send, recv, endpoint_id, device_id, control, application).await;
                });
            }
        }
    }
    stream_tasks.abort_all();
    while stream_tasks.join_next().await.is_some() {}
    drop(connection_guard);
    drop(unpaired_connection_permit);
    drop(connection_permit);
}

async fn handle_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    endpoint_id: String,
    connection_device_id: Option<DeviceId>,
    control: CompanionControl,
    application: CompanionApplication,
) {
    let head = match tokio::time::timeout(
        REQUEST_READ_TIMEOUT,
        ygg_companion_protocol::read_head::<_, RequestHead>(&mut recv),
    )
    .await
    {
        Ok(Ok(head)) => head,
        Ok(Err(ygg_companion_protocol::ProtocolError::ProtocolMismatch)) => {
            reset_stream(send, recv, RESET_PROTOCOL_MISMATCH);
            return;
        }
        _ => {
            reset_stream(send, recv, RESET_FRAME_INVALID);
            return;
        }
    };
    let request_id = head.request_id().to_owned();
    let limits = match head.validate() {
        Ok(limits) => limits,
        Err(ygg_companion_protocol::ProtocolError::ProtocolMismatch) => {
            reset_stream(send, recv, RESET_PROTOCOL_MISMATCH);
            return;
        }
        Err(_) => {
            reset_stream(send, recv, RESET_FRAME_INVALID);
            return;
        }
    };

    match head {
        RequestHead::Pairing { operation, .. } => {
            if !request_finished(&mut recv).await {
                reset_stream(send, recv, RESET_FRAME_INVALID);
                return;
            }
            let result = control.pair(&endpoint_id, operation);
            let write_result = match result {
                Ok(reply) => send_json_response(&mut send, &request_id, 200, &reply).await,
                Err(error) => {
                    let body = serde_json::json!({"error": {"code": error.code()}});
                    send_json_response(&mut send, &request_id, error.status(), &body).await
                }
            };
            if write_result.is_err() || send.finish().is_err() {
                reset_stream(send, recv, RESET_INTERNAL);
            }
        }
        RequestHead::Events { .. } => {
            let Some(device_id) = authorize_stream(
                &control,
                &endpoint_id,
                connection_device_id,
                &mut send,
                &mut recv,
                &request_id,
            )
            .await
            else {
                return;
            };
            stream_events(
                send,
                recv,
                request_id,
                endpoint_id,
                device_id,
                control,
                application,
            )
            .await;
        }
        RequestHead::Http {
            method,
            path,
            content_type,
            ..
        } => {
            let Some(device_id) = authorize_stream(
                &control,
                &endpoint_id,
                connection_device_id,
                &mut send,
                &mut recv,
                &request_id,
            )
            .await
            else {
                return;
            };
            let Some(limits) = limits else {
                reset_stream(send, recv, RESET_INTERNAL);
                return;
            };
            proxy_http(
                send,
                recv,
                request_id,
                method,
                path,
                content_type,
                limits,
                endpoint_id,
                device_id,
                control,
                application.router,
            )
            .await;
        }
    }
}

async fn authorize_stream(
    control: &CompanionControl,
    endpoint_id: &str,
    connection_device_id: Option<DeviceId>,
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    request_id: &str,
) -> Option<DeviceId> {
    if control.endpoint_is_revoked(endpoint_id) {
        let _ = send.reset(RESET_REVOKED.into());
        let _ = recv.stop(RESET_REVOKED.into());
        return None;
    }
    let current = control.resolve_endpoint(endpoint_id);
    if current.is_none() || current != connection_device_id {
        let write_result = send_json_response(
            send,
            request_id,
            401,
            &serde_json::json!({"error":{"code":"unauthorized"}}),
        )
        .await;
        if write_result.is_err() || send.finish().is_err() {
            let _ = send.reset(RESET_INTERNAL.into());
            let _ = recv.stop(RESET_INTERNAL.into());
        } else {
            let _ = recv.stop(RESET_UNAUTHORIZED.into());
        }
        return None;
    }
    current
}

async fn request_finished(recv: &mut iroh::endpoint::RecvStream) -> bool {
    matches!(
        tokio::time::timeout(REQUEST_READ_TIMEOUT, expect_end(recv)).await,
        Ok(Ok(()))
    )
}

#[allow(clippy::too_many_arguments)]
async fn proxy_http(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    request_id: String,
    method: HttpMethod,
    path: String,
    content_type: Option<String>,
    limits: RouteLimits,
    endpoint_id: String,
    device_id: DeviceId,
    control: CompanionControl,
    router: Router,
) {
    let body = match tokio::time::timeout(
        REQUEST_READ_TIMEOUT,
        ygg_companion_protocol::read_body(&mut recv, limits.request_bytes),
    )
    .await
    {
        Ok(Ok(body)) => body,
        _ => {
            reset_stream(send, recv, RESET_FRAME_INVALID);
            return;
        }
    };
    if !request_finished(&mut recv).await {
        reset_stream(send, recv, RESET_FRAME_INVALID);
        return;
    }
    if control.resolve_endpoint(&endpoint_id).as_ref() != Some(&device_id) {
        let code = if control.endpoint_is_revoked(&endpoint_id) {
            RESET_REVOKED
        } else {
            RESET_UNAUTHORIZED
        };
        reset_stream(send, recv, code);
        return;
    }
    if command_device_mismatch(&path, &body, &device_id) {
        let write_result = send_json_response(
            &mut send,
            &request_id,
            403,
            &serde_json::json!({"error": {"code": "unauthorized", "message": "The command device does not match the authenticated companion."}}),
        )
        .await;
        if write_result.is_err() || send.finish().is_err() {
            reset_stream(send, recv, RESET_INTERNAL);
        }
        return;
    }

    let mut request = match Request::builder()
        .method(http_method(method))
        .uri(&path)
        .body(Body::from(body))
    {
        Ok(request) => request,
        Err(_) => {
            reset_stream(send, recv, RESET_FRAME_INVALID);
            return;
        }
    };
    if let Some(content_type) = content_type {
        let Ok(value) = HeaderValue::from_str(&content_type) else {
            reset_stream(send, recv, RESET_FRAME_INVALID);
            return;
        };
        request.headers_mut().insert("content-type", value);
    }
    request
        .extensions_mut()
        .insert(TransportPrincipal::Paired { device_id });

    let response = tokio::select! {
        stopped = send.stopped() => {
            let _ = stopped;
            reset_stream(send, recv, RESET_CANCELLED);
            return;
        }
        response = tokio::time::timeout(APPLICATION_TIMEOUT, router.oneshot(request)) => {
            match response {
                Ok(Ok(response)) => response,
                _ => {
                    reset_stream(send, recv, RESET_INTERNAL);
                    return;
                }
            }
        }
    };
    let status = response.status().as_u16();
    let headers = match filtered_response_headers(response.headers()) {
        Ok(headers) => headers,
        Err(()) => {
            reset_stream(send, recv, RESET_INTERNAL);
            return;
        }
    };
    let response_head = ResponseHead {
        protocol: PROTOCOL_VERSION,
        request_id: request_id.clone(),
        status,
        headers,
    };
    if response_head.validate(&request_id).is_err() {
        reset_stream(send, recv, RESET_INTERNAL);
        return;
    }
    let expected_length = match response_head.content_length(limits.response_bytes) {
        Ok(expected_length) => expected_length,
        Err(_) => {
            reset_stream(send, recv, RESET_INTERNAL);
            return;
        }
    };
    let response_deadline = tokio::time::Instant::now() + RESPONSE_BODY_TIMEOUT;
    if !matches!(
        tokio::time::timeout_at(
            bounded_deadline(response_deadline, STREAM_WRITE_TIMEOUT),
            write_head(&mut send, &response_head),
        )
        .await,
        Ok(Ok(()))
    ) {
        reset_stream(send, recv, RESET_INTERNAL);
        return;
    }

    let mut total = 0usize;
    let mut body = response.into_body();
    loop {
        let frame = tokio::select! {
            stopped = send.stopped() => {
                let _ = stopped;
                reset_stream(send, recv, RESET_CANCELLED);
                return;
            }
            frame = tokio::time::timeout_at(
                bounded_deadline(response_deadline, RESPONSE_FRAME_TIMEOUT),
                body.frame(),
            ) => frame,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(_) => {
                reset_stream(send, recv, RESET_INTERNAL);
                return;
            }
        };
        let Ok(frame) = frame else {
            reset_stream(send, recv, RESET_INTERNAL);
            return;
        };
        let data = match frame.into_data() {
            Ok(data) => data,
            Err(_) => {
                reset_stream(send, recv, RESET_FRAME_INVALID);
                return;
            }
        };
        total = match total.checked_add(data.len()) {
            Some(total) if total <= limits.response_bytes => total,
            _ => {
                reset_stream(send, recv, RESET_FRAME_INVALID);
                return;
            }
        };
        for chunk in data.chunks(ygg_companion_protocol::MAX_CHUNK_BYTES) {
            if !matches!(
                tokio::time::timeout_at(
                    bounded_deadline(response_deadline, STREAM_WRITE_TIMEOUT),
                    write_chunk(&mut send, chunk),
                )
                .await,
                Ok(Ok(()))
            ) {
                reset_stream(send, recv, RESET_INTERNAL);
                return;
            }
        }
    }
    if expected_length.is_some_and(|expected| expected != total) {
        reset_stream(send, recv, RESET_FRAME_INVALID);
        return;
    }
    if !matches!(
        tokio::time::timeout_at(
            bounded_deadline(response_deadline, STREAM_WRITE_TIMEOUT),
            finish_body(&mut send),
        )
        .await,
        Ok(Ok(()))
    ) {
        reset_stream(send, recv, RESET_INTERNAL);
        return;
    }
    if send.finish().is_err() {
        reset_stream(send, recv, RESET_INTERNAL);
    }
}

async fn stream_events(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    request_id: String,
    endpoint_id: String,
    _device_id: DeviceId,
    control: CompanionControl,
    application: CompanionApplication,
) {
    let mut events = (application.subscribe_events)();
    let mut revoked = control.inner.revoked.subscribe();
    if !request_finished(&mut recv).await {
        reset_stream(send, recv, RESET_FRAME_INVALID);
        return;
    }
    if control.endpoint_is_revoked(&endpoint_id) {
        reset_stream(send, recv, RESET_REVOKED);
        return;
    }
    let response = ResponseHead {
        protocol: PROTOCOL_VERSION,
        request_id: request_id.clone(),
        status: 200,
        headers: vec![ResponseHeader {
            name: "content-type".to_owned(),
            value: "application/json".to_owned(),
        }],
    };
    if !matches!(
        tokio::time::timeout(STREAM_WRITE_TIMEOUT, write_head(&mut send, &response)).await,
        Ok(Ok(()))
    ) {
        reset_stream(send, recv, RESET_INTERNAL);
        return;
    }
    let heartbeat = tokio::time::sleep(EVENT_HEARTBEAT_INTERVAL);
    tokio::pin!(heartbeat);
    loop {
        enum Next {
            Stopped,
            Heartbeat,
            Revoked(Result<String, broadcast::error::RecvError>),
            Event(Result<Box<crate::HostStreamEvent>, broadcast::error::RecvError>),
        }
        let next = tokio::select! {
            _ = send.stopped() => Next::Stopped,
            _ = &mut heartbeat => Next::Heartbeat,
            revoked_endpoint = revoked.recv() => Next::Revoked(revoked_endpoint),
            event = events.recv() => Next::Event(event.map(Box::new)),
        };
        match next {
            Next::Stopped => {
                reset_stream(send, recv, RESET_CANCELLED);
                return;
            }
            Next::Heartbeat => {
                if !matches!(
                    tokio::time::timeout(
                        STREAM_WRITE_TIMEOUT,
                        write_record(&mut send, EVENT_HEARTBEAT_RECORD, MAX_EVENT_BYTES),
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    reset_stream(send, recv, RESET_INTERNAL);
                    return;
                }
                heartbeat
                    .as_mut()
                    .reset(tokio::time::Instant::now() + EVENT_HEARTBEAT_INTERVAL);
            }
            Next::Revoked(Ok(revoked_endpoint)) if revoked_endpoint == endpoint_id => {
                reset_stream(send, recv, RESET_REVOKED);
                return;
            }
            Next::Revoked(Ok(_)) => {}
            Next::Revoked(Err(broadcast::error::RecvError::Lagged(_))) => {
                if control.endpoint_is_revoked(&endpoint_id) {
                    reset_stream(send, recv, RESET_REVOKED);
                    return;
                }
            }
            Next::Revoked(Err(broadcast::error::RecvError::Closed)) => {
                reset_stream(send, recv, RESET_INTERNAL);
                return;
            }
            Next::Event(Err(broadcast::error::RecvError::Lagged(_))) => {
                reset_stream(send, recv, RESET_REPLAY_REQUIRED);
                return;
            }
            Next::Event(Err(broadcast::error::RecvError::Closed)) => {
                reset_stream(send, recv, RESET_INTERNAL);
                return;
            }
            Next::Event(Ok(event)) => {
                if event.validate().is_err() {
                    reset_stream(send, recv, RESET_INTERNAL);
                    return;
                }
                let encoded = match serde_json::to_vec(&event) {
                    Ok(encoded) if !encoded.is_empty() && encoded.len() <= MAX_EVENT_BYTES => {
                        encoded
                    }
                    _ => {
                        reset_stream(send, recv, RESET_INTERNAL);
                        return;
                    }
                };
                if !matches!(
                    tokio::time::timeout(
                        STREAM_WRITE_TIMEOUT,
                        write_record(&mut send, &encoded, MAX_EVENT_BYTES),
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    reset_stream(send, recv, RESET_INTERNAL);
                    return;
                }
                heartbeat
                    .as_mut()
                    .reset(tokio::time::Instant::now() + EVENT_HEARTBEAT_INTERVAL);
            }
        }
    }
}

fn command_device_mismatch(path: &str, body: &[u8], device_id: &DeviceId) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/api/v1/commands/host" => serde_json::from_slice::<HostCommandEnvelope>(body)
            .map(|envelope| envelope.device_id != *device_id)
            .unwrap_or(false),
        "/api/v1/commands/session" => serde_json::from_slice::<SessionCommandEnvelope>(body)
            .map(|envelope| envelope.device_id != *device_id)
            .unwrap_or(false),
        _ => false,
    }
}

fn http_method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
    }
}

fn filtered_response_headers(headers: &axum::http::HeaderMap) -> Result<Vec<ResponseHeader>, ()> {
    const ALLOWED: [&str; 8] = [
        "content-type",
        "content-disposition",
        "content-length",
        "cache-control",
        "etag",
        "x-content-type-options",
        "referrer-policy",
        "cross-origin-resource-policy",
    ];
    let mut filtered = Vec::new();
    for name in ALLOWED {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| ())?;
        for value in headers.get_all(header_name).iter() {
            let value = value.to_str().map_err(|_| ())?;
            if value.is_empty() || value.len() > 4 * 1024 {
                return Err(());
            }
            filtered.push(ResponseHeader {
                name: name.to_owned(),
                value: value.to_owned(),
            });
            if filtered.len() > 12 {
                return Err(());
            }
        }
    }
    Ok(filtered)
}

async fn send_json_response<T: Serialize>(
    send: &mut iroh::endpoint::SendStream,
    request_id: &str,
    status: u16,
    value: &T,
) -> Result<(), ()> {
    let body = Zeroizing::new(serde_json::to_vec(value).map_err(|_| ())?);
    if body.len() > PAIRING_RESPONSE_BYTES {
        return Err(());
    }
    let head = ResponseHead {
        protocol: PROTOCOL_VERSION,
        request_id: request_id.to_owned(),
        status,
        headers: vec![
            ResponseHeader {
                name: "content-type".to_owned(),
                value: "application/json".to_owned(),
            },
            ResponseHeader {
                name: "content-length".to_owned(),
                value: body.len().to_string(),
            },
        ],
    };
    tokio::time::timeout(STREAM_WRITE_TIMEOUT, async {
        write_head(send, &head).await.map_err(|_| ())?;
        write_body(send, &body).await.map_err(|_| ())
    })
    .await
    .map_err(|_| ())?
}

fn bounded_deadline(
    aggregate: tokio::time::Instant,
    idle_timeout: Duration,
) -> tokio::time::Instant {
    std::cmp::min(aggregate, tokio::time::Instant::now() + idle_timeout)
}

async fn write_chunk(send: &mut iroh::endpoint::SendStream, bytes: &[u8]) -> Result<(), io::Error> {
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(STREAM_WRITE_TIMEOUT, async {
        send.write_u32(bytes.len() as u32).await?;
        if !bytes.is_empty() {
            send.write_all(bytes).await?;
        }
        send.flush().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "companion stream write timed out"))?
}

fn reset_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    code: u32,
) {
    let _ = send.reset(code.into());
    let _ = recv.stop(code.into());
}

fn companion_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.keep_alive_interval(Some(Duration::from_secs(1)));
    config.max_concurrent_bidi_streams(VarInt::from_u32(MAX_STREAMS_PER_CONNECTION as u32));
    config.max_concurrent_uni_streams(VarInt::from_u32(0));
    config
}

fn explicit_relay_map(relay: CompanionRelay) -> RelayMap {
    match relay {
        CompanionRelay::N0 => RelayMap::from_iter([
            iroh::defaults::prod::default_na_east_relay(),
            iroh::defaults::prod::default_na_west_relay(),
            iroh::defaults::prod::default_eu_relay(),
            iroh::defaults::prod::default_ap_relay(),
        ]),
    }
}

fn random_secret() -> io::Result<Secret32> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut *bytes).map_err(io::Error::other)?;
    Ok(Secret32::from_bytes(*bytes))
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut random = vec![0u8; bytes];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in random {
        use fmt::Write as _;
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    let created = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe directory",
                ));
            }
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            true
        }
        Err(error) => return Err(error),
    };
    set_private_directory_mode(path)?;
    if created {
        sync_parent(path)?;
    }
    Ok(())
}

fn load_or_create_endpoint_secret(
    path: &Path,
    registry_exists: bool,
) -> Result<EndpointSecret, CompanionError> {
    match open_nofollow(path, false) {
        Ok(mut file) => {
            if !registry_exists {
                return Err(CompanionError::InvalidState);
            }
            let metadata = file.metadata().map_err(|_| CompanionError::State)?;
            if !is_private_regular_file(&metadata) || metadata.len() != 32 {
                return Err(CompanionError::InvalidState);
            }
            let mut bytes = Zeroizing::new([0u8; 32]);
            file.read_exact(&mut *bytes)
                .map_err(|_| CompanionError::InvalidState)?;
            let mut extra = [0u8; 1];
            if file.read(&mut extra).map_err(|_| CompanionError::State)? != 0 {
                return Err(CompanionError::InvalidState);
            }
            Ok(EndpointSecret(SecretKey::from_bytes(&bytes)))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !registry_exists => {
            let mut bytes = Zeroizing::new([0u8; 32]);
            getrandom::fill(&mut *bytes).map_err(|_| CompanionError::State)?;
            let secret = EndpointSecret(SecretKey::from_bytes(&bytes));
            persist_new_endpoint_secret(path, &bytes).map_err(|_| CompanionError::State)?;
            Ok(secret)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(CompanionError::InvalidState),
        Err(_) => Err(CompanionError::State),
    }
}

fn persist_new_endpoint_secret(path: &Path, bytes: &[u8; 32]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "key has no parent"))?;
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "key name is invalid"))?;
    let temporary = parent.join(format!(".{filename}.{}.tmp", random_hex(8)?));
    let result = (|| {
        let mut file = create_new_nofollow(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        rename_noreplace(&temporary, path)?;
        sync_parent(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
))]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    match renameat_with(CWD, source, CWD, target, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP) => {
            link_noreplace(source, target)
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "redox"
)))]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    link_noreplace(source, target)
}

fn link_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(source, target)?;
    fs::remove_file(source)
}

fn load_registry(path: &Path) -> Result<Option<RegistryDocument>, CompanionError> {
    let file = match open_nofollow(path, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(CompanionError::State),
    };
    let metadata = file.metadata().map_err(|_| CompanionError::State)?;
    if !is_private_regular_file(&metadata) || metadata.len() as usize > MAX_REGISTRY_BYTES {
        return Err(CompanionError::InvalidState);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_REGISTRY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CompanionError::State)?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(CompanionError::InvalidState);
    }
    let registry: RegistryDocument =
        serde_json::from_slice(&bytes).map_err(|_| CompanionError::InvalidState)?;
    validate_registry(&registry)?;
    Ok(Some(registry))
}

fn validate_registry(registry: &RegistryDocument) -> Result<(), CompanionError> {
    if registry.version != PROTOCOL_VERSION
        || registry.host_id.is_empty()
        || registry.host_id.len() > 128
        || !registry
            .host_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        || registry.host_endpoint_id.is_empty()
        || registry.host_endpoint_id.len() > 128
        || registry.devices.len() > MAX_DEVICES
    {
        return Err(CompanionError::InvalidState);
    }
    let mut ids = BTreeSet::new();
    let mut endpoint_ids = BTreeSet::new();
    for device in &registry.devices {
        if !ids.insert(device.id.as_str())
            || !endpoint_ids.insert(device.endpoint_id.as_str())
            || device.endpoint_id.is_empty()
            || device.endpoint_id.len() > 128
            || device.name.is_empty()
            || device.name.len() > 128
            || device.name.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(CompanionError::InvalidState);
        }
        device
            .summary(false)
            .validate()
            .map_err(|_| CompanionError::InvalidState)?;
    }
    Ok(())
}

fn persist_registry(state_dir: &Path, registry: &RegistryDocument) -> io::Result<()> {
    validate_registry(registry).map_err(|_| io::Error::other("invalid registry"))?;
    let bytes = serde_json::to_vec(registry).map_err(io::Error::other)?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "registry exceeds bound",
        ));
    }
    let target = state_dir.join(DEVICE_REGISTRY_FILE);
    let temporary = state_dir.join(format!(".{DEVICE_REGISTRY_FILE}.{}.tmp", random_hex(8)?));
    let result = (|| {
        let mut file = create_new_nofollow(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)?;
        sync_parent(&target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn is_private_regular_file(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    metadata.is_file() && metadata.permissions().mode() & 0o077 == 0 && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn is_private_regular_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

#[cfg(unix)]
fn open_nofollow(path: &Path, write: bool) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW);
    options.open(path)
}

#[cfg(not(unix))]
fn open_nofollow(path: &Path, write: bool) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "unsafe file"));
    }
    OpenOptions::new().read(true).write(write).open(path)
}

#[cfg(unix)]
fn create_new_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn create_new_nofollow(path: &Path) -> io::Result<File> {
    OpenOptions::new().create_new(true).write(true).open(path)
}

#[cfg(unix)]
fn set_private_directory_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Extension, State};
    use axum::routing::{get, post};
    use tempfile::TempDir;
    use tokio::sync::Notify;

    fn control(temp: &TempDir) -> CompanionControl {
        CompanionControl::open(CompanionConfig {
            serve_state_dir: temp.path().to_path_buf(),
            host_id: "host-test".to_owned(),
            relay: CompanionRelay::N0,
        })
        .unwrap()
    }

    #[test]
    fn allowed_response_headers_fail_closed_instead_of_being_dropped() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            HeaderValue::from_bytes(&[0x80]).unwrap(),
        );
        assert!(filtered_response_headers(&headers).is_err());

        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            HeaderValue::from_static("not-a-length"),
        );
        let response = ResponseHead {
            protocol: PROTOCOL_VERSION,
            request_id: "request-1".to_owned(),
            status: 200,
            headers: filtered_response_headers(&headers).unwrap(),
        };
        assert!(response.validate("request-1").is_err());
    }

    #[test]
    fn endpoint_identity_persists_and_secret_debug_is_redacted() {
        let temp = TempDir::new().unwrap();
        let first = control(&temp);
        let endpoint_id = first.inner.endpoint_id.clone();
        assert!(!format!("{:?}", first.inner.endpoint_secret).contains(&endpoint_id));
        drop(first);
        assert_eq!(control(&temp).inner.endpoint_id, endpoint_id);
    }

    #[test]
    fn changed_host_identity_fails_closed_after_registry_commit() {
        let temp = TempDir::new().unwrap();
        drop(control(&temp));

        assert!(matches!(
            CompanionControl::open(CompanionConfig {
                serve_state_dir: temp.path().to_path_buf(),
                host_id: "host-replacement".to_owned(),
                relay: CompanionRelay::N0,
            }),
            Err(CompanionError::InvalidState)
        ));
    }

    #[test]
    fn endpoint_secret_creation_never_replaces_an_existing_key() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("endpoint-key");
        persist_new_endpoint_secret(&path, &[1; 32]).unwrap();

        assert!(persist_new_endpoint_secret(&path, &[2; 32]).is_err());
        assert_eq!(fs::read(&path).unwrap(), [1; 32]);
        assert_eq!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            0
        );
    }

    #[test]
    fn missing_key_fails_closed_after_initial_identity_commit() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        let state_dir = control.inner.state_dir.clone();
        assert!(state_dir.join(DEVICE_REGISTRY_FILE).is_file());
        fs::remove_file(state_dir.join(ENDPOINT_KEY_FILE)).unwrap();
        drop(control);

        assert!(matches!(
            CompanionControl::open(CompanionConfig {
                serve_state_dir: temp.path().to_path_buf(),
                host_id: "host-test".to_owned(),
                relay: CompanionRelay::N0,
            }),
            Err(CompanionError::InvalidState)
        ));
    }

    #[test]
    fn missing_registry_fails_closed_after_identity_commit() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        let state_dir = control.inner.state_dir.clone();
        assert!(state_dir.join(ENDPOINT_KEY_FILE).is_file());
        fs::remove_file(state_dir.join(DEVICE_REGISTRY_FILE)).unwrap();
        drop(control);

        assert!(matches!(
            CompanionControl::open(CompanionConfig {
                serve_state_dir: temp.path().to_path_buf(),
                host_id: "host-test".to_owned(),
                relay: CompanionRelay::N0,
            }),
            Err(CompanionError::InvalidState)
        ));
    }

    #[test]
    fn failed_revocation_persistence_fails_closed_and_retry_commits() {
        let temp = TempDir::new().unwrap();
        let host = control(&temp);
        let device_id = DeviceId::new("device-one").unwrap();
        let endpoint_id = "endpoint-one".to_owned();
        let mut registry = host.inner.state.lock().unwrap().registry.clone();
        registry.revision = 1;
        registry.devices.push(RegistryDevice {
            id: device_id.clone(),
            endpoint_id: endpoint_id.clone(),
            name: "Phone".to_owned(),
            platform: DevicePlatform::Ios,
            paired_at_ms: 1,
            last_seen_at_ms: None,
            revoked_at_ms: None,
        });
        persist_registry(&host.inner.state_dir, &registry).unwrap();
        host.inner.state.lock().unwrap().registry = registry;

        let state_dir = host.inner.state_dir.clone();
        let backup = temp.path().join("companion-v1-backup");
        fs::rename(&state_dir, &backup).unwrap();
        fs::write(&state_dir, b"block registry writes").unwrap();

        assert_eq!(
            host.revoke_device(device_id.as_str()),
            Err(CompanionControlError::Storage)
        );
        assert!(host.resolve_endpoint(&endpoint_id).is_none());
        assert!(host
            .inner
            .state
            .lock()
            .unwrap()
            .uncommitted_revocations
            .contains(&endpoint_id));

        fs::remove_file(&state_dir).unwrap();
        fs::rename(&backup, &state_dir).unwrap();
        host.revoke_device(device_id.as_str()).unwrap();
        assert!(host
            .inner
            .state
            .lock()
            .unwrap()
            .uncommitted_revocations
            .is_empty());
        drop(host);

        let reopened = control(&temp);
        assert!(reopened.catalog().devices[0].revoked_at_ms.is_some());
    }

    #[test]
    fn missing_key_fails_closed_once_registry_has_devices() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        let mut registry = control.inner.state.lock().unwrap().registry.clone();
        registry.revision = 1;
        registry.devices.push(RegistryDevice {
            id: DeviceId::new("device-one").unwrap(),
            endpoint_id: "endpoint-one".to_owned(),
            name: "Phone".to_owned(),
            platform: DevicePlatform::Ios,
            paired_at_ms: 1,
            last_seen_at_ms: None,
            revoked_at_ms: None,
        });
        persist_registry(&control.inner.state_dir, &registry).unwrap();
        fs::remove_file(control.inner.state_dir.join(ENDPOINT_KEY_FILE)).unwrap();
        drop(control);
        assert!(matches!(
            CompanionControl::open(CompanionConfig {
                serve_state_dir: temp.path().to_path_buf(),
                host_id: "host-test".to_owned(),
                relay: CompanionRelay::N0,
            }),
            Err(CompanionError::InvalidState)
        ));
    }

    #[test]
    fn invitation_expires_monotonically_after_wall_clock_rollback() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        control.set_endpoint_info(EndpointInfo {
            relay_urls: vec!["https://relay.example".to_owned()],
            direct_addresses: Vec::new(),
        });
        let invitation = control.open_pairing().unwrap();
        assert!(invitation.expires_at_ms > 0);

        let mut state = control.inner.state.lock().unwrap();
        let deadline = state.invitation.as_ref().unwrap().deadline;
        cleanup_expired(&mut state, 0, deadline);
        assert!(state.invitation.is_none());
    }

    #[test]
    fn pending_pairing_expires_monotonically_after_wall_clock_rollback() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        control.set_endpoint_info(EndpointInfo {
            relay_urls: vec!["https://relay.example".to_owned()],
            direct_addresses: Vec::new(),
        });
        let invitation = control.open_pairing().unwrap();
        let ticket = PairingTicket::decode(&invitation.ticket).unwrap();
        let reply = control
            .pair(
                "mobile-endpoint",
                PairingOperation::Request(ygg_companion_protocol::PairingRequest {
                    invitation: ticket.invitation,
                    client_nonce: "nonce-monotonic".to_owned(),
                    observed_host_id: "host-test".to_owned(),
                    observed_host_endpoint_id: control.inner.endpoint_id.clone(),
                    device: PairingDeviceClaim {
                        name: "Phone".to_owned(),
                        platform: DevicePlatform::Ios,
                        app_version: "1.0.0".to_owned(),
                    },
                }),
            )
            .unwrap();
        let (request_id, poll_token) = match reply {
            PairingReply::PendingRequest {
                request_id,
                poll_token,
                ..
            } => (request_id, poll_token),
            _ => panic!("unexpected pairing reply"),
        };
        let proof = PairingStatusRequest {
            request_id: request_id.clone(),
            poll_token,
        };

        let mut state = control.inner.state.lock().unwrap();
        let deadline = state.pending.get(&request_id).unwrap().deadline;
        assert!(matches!(
            pairing_status(&state, "mobile-endpoint", &proof, 0, deadline).unwrap(),
            PairingReply::Expired
        ));
        cleanup_expired(&mut state, 0, deadline);
        assert!(!state.pending.contains_key(&request_id));
    }

    #[test]
    fn decision_and_ack_require_matching_endpoint_and_poll_capability() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        control.set_endpoint_info(EndpointInfo {
            relay_urls: vec!["https://relay.example".to_owned()],
            direct_addresses: Vec::new(),
        });
        let invitation = control.open_pairing().unwrap();
        let ticket = PairingTicket::decode(&invitation.ticket).unwrap();
        let request = PairingOperation::Request(ygg_companion_protocol::PairingRequest {
            invitation: ticket.invitation,
            client_nonce: "nonce-one".to_owned(),
            observed_host_id: "host-test".to_owned(),
            observed_host_endpoint_id: control.inner.endpoint_id.clone(),
            device: PairingDeviceClaim {
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                app_version: "1.0.0".to_owned(),
            },
        });
        let reply = control.pair("mobile-endpoint", request).unwrap();
        let (request_id, poll_token) = match reply {
            PairingReply::PendingRequest {
                request_id,
                poll_token,
                ..
            } => (request_id, poll_token),
            _ => panic!("unexpected pairing reply"),
        };
        control
            .decide_pairing(&request_id, PairingDecision::Approve)
            .unwrap();
        let proof = PairingStatusRequest {
            request_id,
            poll_token,
        };
        assert!(matches!(
            control.pair("other-endpoint", PairingOperation::Ack(proof.clone())),
            Err(PairingWireError::InvalidCapability)
        ));
        let reply = control
            .pair("mobile-endpoint", PairingOperation::Ack(proof.clone()))
            .unwrap();
        assert!(matches!(reply, PairingReply::Acknowledged { .. }));
        let catalog = control.catalog();
        assert_eq!(catalog.devices.len(), 1);
        assert!(catalog.pending.is_empty());
        assert!(matches!(
            control
                .pair("mobile-endpoint", PairingOperation::Status(proof.clone()))
                .unwrap(),
            PairingReply::Acknowledged { .. }
        ));
        assert!(matches!(
            control
                .pair("mobile-endpoint", PairingOperation::Cancel(proof.clone()))
                .unwrap(),
            PairingReply::Acknowledged { .. }
        ));
        assert_eq!(control.catalog().devices.len(), 1);
        drop(control);

        let reopened = CompanionControl::open(CompanionConfig {
            serve_state_dir: temp.path().to_path_buf(),
            host_id: "host-test".to_owned(),
            relay: CompanionRelay::N0,
        })
        .unwrap();
        assert!(matches!(
            reopened
                .pair("mobile-endpoint", PairingOperation::Status(proof))
                .unwrap(),
            PairingReply::Acknowledged { .. }
        ));
    }

    #[test]
    fn missing_pairing_status_is_terminal_when_no_authority_was_committed() {
        let temp = TempDir::new().unwrap();
        let control = control(&temp);
        let reply = control
            .pair(
                "mobile-endpoint",
                PairingOperation::Status(PairingStatusRequest {
                    request_id: "pair-expired".to_owned(),
                    poll_token: Secret32::from_bytes([9; 32]),
                }),
            )
            .unwrap();
        assert!(matches!(reply, PairingReply::Expired));
    }

    #[tokio::test]
    async fn quiet_event_stream_emits_liveness_heartbeats() {
        let temp = TempDir::new().unwrap();
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, Router::new(), events).await;
        harness.pair_client().await;

        let (_event_send, mut event_recv) = open_event_stream(&harness.connection).await;
        let heartbeat = tokio::time::timeout(
            Duration::from_secs(1),
            ygg_companion_protocol::read_record(&mut event_recv, MAX_EVENT_BYTES),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(heartbeat, EVENT_HEARTBEAT_RECORD);

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn direct_transport_pairs_binds_principal_replays_and_revokes_durably() {
        let temp = TempDir::new().unwrap();
        let dispatches = Arc::new(AtomicUsize::new(0));
        let router_state = TestRouterState {
            dispatches: dispatches.clone(),
        };
        let router = Router::new()
            .route("/api/v1/bootstrap", get(test_bootstrap))
            .route("/api/v1/commands/host", post(test_host_command))
            .with_state(router_state);
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events.clone()).await;
        let device = harness.pair_client().await;

        let wrong_command = command_body("device-other");
        let (head, body) = http_round_trip(
            &harness.connection,
            HttpMethod::Post,
            "/api/v1/commands/host",
            &wrong_command,
        )
        .await;
        assert_eq!(head.status, 403);
        assert!(body
            .windows(b"unauthorized".len())
            .any(|part| part == b"unauthorized"));
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);

        let matching_command = command_body(&device.id);
        let (head, body) = http_round_trip(
            &harness.connection,
            HttpMethod::Post,
            "/api/v1/commands/host",
            &matching_command,
        )
        .await;
        assert_eq!(head.status, 200);
        assert_eq!(body, b"dispatched");
        assert_eq!(dispatches.load(Ordering::SeqCst), 1);

        let (head, body) = http_round_trip(
            &harness.connection,
            HttpMethod::Get,
            "/api/v1/bootstrap",
            &[],
        )
        .await;
        assert_eq!(head.status, 200);
        assert_eq!(body, b"paired bootstrap");

        let (_event_send, mut event_recv) = open_event_stream(&harness.connection).await;
        for sequence in 1..=4 {
            events.send(test_event(sequence)).unwrap();
        }
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_record(&mut event_recv, MAX_EVENT_BYTES),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_REPLAY_REQUIRED));

        let (_event_send, mut revoked_recv) = open_event_stream(&harness.connection).await;
        harness.control.revoke_device(&device.id).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_record(&mut revoked_recv, MAX_EVENT_BYTES),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_REVOKED));
        assert!(harness.control.catalog().devices[0].revoked_at_ms.is_some());

        harness.shutdown().await;
        let reloaded = control(&temp);
        let persisted = reloaded.catalog().devices;
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].id, device.id);
        assert!(persisted[0].revoked_at_ms.is_some());
        assert!(!persisted[0].connected);
    }

    #[tokio::test]
    async fn client_cancellation_drops_an_in_flight_application_request() {
        let temp = TempDir::new().unwrap();
        let cancellation = CancellationState::default();
        let router = Router::new()
            .route("/api/v1/bootstrap", get(wait_for_cancellation))
            .with_state(cancellation.clone());
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events).await;
        harness.pair_client().await;

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "request-cancel".to_owned(),
            method: HttpMethod::Get,
            path: "/api/v1/bootstrap".to_owned(),
            content_type: None,
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::write_body(&mut send, &[])
            .await
            .unwrap();
        let _ = send.finish();
        tokio::time::timeout(Duration::from_secs(5), cancellation.started.notified())
            .await
            .unwrap();
        recv.stop(ygg_companion_protocol::RESET_CANCELLED.into())
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), cancellation.dropped.notified())
            .await
            .unwrap();

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn revocation_drops_an_in_flight_application_request() {
        let temp = TempDir::new().unwrap();
        let cancellation = CancellationState::default();
        let router = Router::new()
            .route("/api/v1/bootstrap", get(wait_for_cancellation))
            .with_state(cancellation.clone());
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events).await;
        let device = harness.pair_client().await;

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "request-revoke".to_owned(),
            method: HttpMethod::Get,
            path: "/api/v1/bootstrap".to_owned(),
            content_type: None,
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::write_body(&mut send, &[])
            .await
            .unwrap();
        send.finish().unwrap();
        tokio::time::timeout(Duration::from_secs(5), cancellation.started.notified())
            .await
            .unwrap();

        harness.control.revoke_device(&device.id).unwrap();
        tokio::time::timeout(Duration::from_secs(5), cancellation.dropped.notified())
            .await
            .unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_head::<_, ResponseHead>(&mut recv),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_REVOKED));

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn transport_limits_remote_stream_directions() {
        let host = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![COMPANION_ALPN.to_vec()])
            .transport_config(companion_transport_config())
            .bind()
            .await
            .unwrap();
        let client = Endpoint::empty_builder(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let host_addr = host.addr();
        let connect = tokio::spawn({
            let client = client.clone();
            async move { client.connect(host_addr, COMPANION_ALPN).await.unwrap() }
        });
        let server_connection = host.accept().await.unwrap().await.unwrap();
        let client_connection = connect.await.unwrap();

        let mut held_streams = Vec::new();
        for _ in 0..MAX_STREAMS_PER_CONNECTION {
            held_streams.push(
                tokio::time::timeout(Duration::from_secs(2), client_connection.open_bi())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(200), client_connection.open_bi())
                .await
                .is_err()
        );
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(200), client_connection.open_uni()).await,
            Ok(Ok(_))
        ));

        client_connection.close(RESET_CANCELLED.into(), b"test complete");
        drop(held_streams);
        drop(server_connection);
        client.close().await;
        host.close().await;
    }

    #[tokio::test]
    async fn per_device_stream_capacity_is_shared_across_connections() {
        let temp = TempDir::new().unwrap();
        let cancellation = CancellationState::default();
        let router = Router::new()
            .route("/api/v1/bootstrap", get(wait_for_cancellation))
            .route("/api/v1/projects", get(test_principal))
            .with_state(cancellation.clone());
        let owner_router = router.clone();
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events).await;
        harness.pair_client().await;
        let second = harness
            .client
            .connect(harness.host.addr(), COMPANION_ALPN)
            .await
            .unwrap();
        let mut held = Vec::new();

        for index in 0..MAX_STREAMS_PER_DEVICE {
            let started = cancellation.started.notified();
            let connection = if index % 2 == 0 {
                &harness.connection
            } else {
                &second
            };
            held.push(open_blocked_request(connection, index).await);
            tokio::time::timeout(Duration::from_secs(5), started)
                .await
                .unwrap();
        }

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "device-stream-overflow".to_owned(),
            method: HttpMethod::Get,
            path: "/api/v1/bootstrap".to_owned(),
            content_type: None,
        };
        let _ = ygg_companion_protocol::write_head(&mut send, &head).await;
        let _ = ygg_companion_protocol::finish_body(&mut send).await;
        let _ = send.finish();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_head::<_, ResponseHead>(&mut recv),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_INTERNAL));

        let other_client = Endpoint::empty_builder(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let pairing_connection = other_client
            .connect(harness.host.addr(), COMPANION_ALPN)
            .await
            .unwrap();
        pair_additional_client(&harness, &other_client, &pairing_connection).await;
        pairing_connection.close(RESET_CANCELLED.into(), b"pairing complete");
        let other_connection = other_client
            .connect(harness.host.addr(), COMPANION_ALPN)
            .await
            .unwrap();
        let (head, body) =
            http_round_trip(&other_connection, HttpMethod::Get, "/api/v1/projects", &[]).await;
        assert_eq!(head.status, 200);
        assert_eq!(body, b"paired");

        let owner_response = owner_router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/projects")
                    .extension(TransportPrincipal::LoopbackOwner)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let owner_body = owner_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(owner_body.as_ref(), b"owner");

        other_connection.close(RESET_CANCELLED.into(), b"test complete");
        other_client.close().await;
        for (mut send, mut recv) in held {
            let _ = send.reset(RESET_CANCELLED.into());
            let _ = recv.stop(RESET_CANCELLED.into());
        }
        second.close(RESET_CANCELLED.into(), b"test complete");
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn unpaired_connections_have_separate_admission_capacity() {
        let temp = TempDir::new().unwrap();
        let (events, _) = broadcast::channel(1);
        let harness = DirectHarness::start(&temp, Router::new(), events).await;
        let mut admitted = Vec::new();

        for _ in 1..MAX_UNPAIRED_CONNECTIONS {
            let endpoint = Endpoint::empty_builder(RelayMode::Disabled)
                .bind()
                .await
                .unwrap();
            let connection = endpoint
                .connect(harness.host.addr(), COMPANION_ALPN)
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(connection.close_reason().is_none());
            admitted.push((endpoint, connection));
        }

        let overflow_endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let overflow = overflow_endpoint
            .connect(harness.host.addr(), COMPANION_ALPN)
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), overflow.closed())
            .await
            .unwrap();
        let iroh::endpoint::ConnectionError::ApplicationClosed(close) = error else {
            panic!("unexpected connection close: {error}");
        };
        assert_eq!(close.error_code.into_inner() as u32, RESET_UNAUTHORIZED);

        for (endpoint, connection) in admitted {
            connection.close(RESET_CANCELLED.into(), b"test complete");
            endpoint.close().await;
        }
        overflow_endpoint.close().await;
        harness.shutdown().await;
    }

    #[tokio::test]
    async fn response_trailers_fail_closed() {
        let temp = TempDir::new().unwrap();
        let router = Router::new().route("/api/v1/bootstrap", get(test_response_with_trailers));
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events).await;
        harness.pair_client().await;

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "response-trailers".to_owned(),
            method: HttpMethod::Get,
            path: "/api/v1/bootstrap".to_owned(),
            content_type: None,
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::finish_body(&mut send)
            .await
            .unwrap();
        send.finish().unwrap();

        match ygg_companion_protocol::read_head::<_, ResponseHead>(&mut recv).await {
            Ok(response) => {
                response.validate(head.request_id()).unwrap();
                let error = ygg_companion_protocol::read_body(&mut recv, 1024)
                    .await
                    .unwrap_err();
                assert_eq!(protocol_reset_code(&error), Some(RESET_FRAME_INVALID));
            }
            Err(error) => {
                assert_eq!(protocol_reset_code(&error), Some(RESET_FRAME_INVALID));
            }
        }

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn trailing_pairing_bytes_do_not_consume_the_invitation() {
        let temp = TempDir::new().unwrap();
        let (events, _) = broadcast::channel(1);
        let harness = DirectHarness::start(&temp, Router::new(), events).await;
        let invitation = harness.control.open_pairing().unwrap();
        let ticket = PairingTicket::decode(&invitation.ticket).unwrap();

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Pairing {
            protocol: PROTOCOL_VERSION,
            request_id: "pairing-trailing".to_owned(),
            operation: PairingOperation::Request(ygg_companion_protocol::PairingRequest {
                invitation: ticket.invitation,
                client_nonce: "trailing-pairing-nonce".to_owned(),
                observed_host_id: "host-test".to_owned(),
                observed_host_endpoint_id: harness.host.id().to_string(),
                device: PairingDeviceClaim {
                    name: "Test phone".to_owned(),
                    platform: DevicePlatform::Ios,
                    app_version: "0.1.0".to_owned(),
                },
            }),
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut send, b"trailing")
            .await
            .unwrap();
        send.finish().unwrap();

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_head::<_, ResponseHead>(&mut recv),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_FRAME_INVALID));
        let catalog = harness.control.catalog();
        assert!(catalog.pending.is_empty());
        assert_eq!(
            catalog.invitation_expires_at_ms,
            Some(invitation.expires_at_ms)
        );

        harness.shutdown().await;
    }

    #[tokio::test]
    async fn trailing_request_bytes_are_rejected_before_application_dispatch() {
        let temp = TempDir::new().unwrap();
        let dispatches = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/api/v1/commands/host", post(test_host_command))
            .with_state(TestRouterState {
                dispatches: dispatches.clone(),
            });
        let (events, _) = broadcast::channel(1);
        let mut harness = DirectHarness::start(&temp, router, events).await;
        let device = harness.pair_client().await;

        let (mut send, mut recv) = harness.connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "request-trailing".to_owned(),
            method: HttpMethod::Post,
            path: "/api/v1/commands/host".to_owned(),
            content_type: Some("application/json".to_owned()),
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::write_body(&mut send, &command_body(&device.id))
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut send, b"trailing")
            .await
            .unwrap();
        send.finish().unwrap();

        let error = tokio::time::timeout(
            Duration::from_secs(5),
            ygg_companion_protocol::read_head::<_, ResponseHead>(&mut recv),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(protocol_reset_code(&error), Some(RESET_FRAME_INVALID));
        assert_eq!(dispatches.load(Ordering::SeqCst), 0);

        harness.shutdown().await;
    }

    #[derive(Clone)]
    struct TestRouterState {
        dispatches: Arc<AtomicUsize>,
    }

    async fn test_response_with_trailers() -> axum::response::Response {
        let mut trailers = axum::http::HeaderMap::new();
        trailers.insert("x-test-trailer", HeaderValue::from_static("present"));
        let body = http_body_util::Full::new(axum::body::Bytes::from_static(b"body"))
            .with_trailers(async move { Some(Ok(trailers)) });
        axum::response::Response::new(Body::new(body))
    }

    async fn test_bootstrap(Extension(principal): Extension<TransportPrincipal>) -> &'static str {
        assert!(matches!(principal, TransportPrincipal::Paired { .. }));
        "paired bootstrap"
    }

    async fn test_principal(Extension(principal): Extension<TransportPrincipal>) -> &'static str {
        match principal {
            TransportPrincipal::LoopbackOwner => "owner",
            TransportPrincipal::Paired { .. } => "paired",
        }
    }

    async fn test_host_command(
        State(state): State<TestRouterState>,
        Extension(principal): Extension<TransportPrincipal>,
    ) -> &'static str {
        assert!(matches!(principal, TransportPrincipal::Paired { .. }));
        state.dispatches.fetch_add(1, Ordering::SeqCst);
        "dispatched"
    }

    #[derive(Clone, Default)]
    struct CancellationState {
        started: Arc<Notify>,
        dropped: Arc<Notify>,
    }

    struct CancellationGuard(Arc<Notify>);

    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    async fn wait_for_cancellation(
        State(state): State<CancellationState>,
        Extension(principal): Extension<TransportPrincipal>,
    ) -> std::convert::Infallible {
        assert!(matches!(principal, TransportPrincipal::Paired { .. }));
        let _guard = CancellationGuard(state.dropped.clone());
        state.started.notify_one();
        std::future::pending().await
    }

    struct DirectHarness {
        control: CompanionControl,
        host: Endpoint,
        client: Endpoint,
        connection: iroh::endpoint::Connection,
        shutdown: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl DirectHarness {
        async fn start(
            temp: &TempDir,
            router: Router,
            events: broadcast::Sender<crate::HostStreamEvent>,
        ) -> Self {
            let control = control(temp);
            let host = Endpoint::empty_builder(RelayMode::Disabled)
                .secret_key(control.inner.endpoint_secret.clone_for_endpoint())
                .alpns(vec![COMPANION_ALPN.to_vec()])
                .transport_config(companion_transport_config())
                .bind()
                .await
                .unwrap();
            let direct_addresses = host.addr().ip_addrs().map(ToString::to_string).collect();
            control.set_endpoint_info(EndpointInfo {
                relay_urls: vec![iroh::defaults::prod::default_na_east_relay()
                    .url
                    .to_string()],
                direct_addresses,
            });
            let subscribe_events = Arc::new(move || events.subscribe());
            let application = CompanionApplication {
                router,
                subscribe_events,
            };
            let (shutdown, shutdown_rx) = oneshot::channel();
            let task = tokio::spawn(accept_loop(
                host.clone(),
                control.clone(),
                application,
                shutdown_rx,
            ));
            let client = Endpoint::empty_builder(RelayMode::Disabled)
                .bind()
                .await
                .unwrap();
            let connection = client.connect(host.addr(), COMPANION_ALPN).await.unwrap();
            Self {
                control,
                host,
                client,
                connection,
                shutdown: Some(shutdown),
                task,
            }
        }

        async fn pair_client(&mut self) -> DeviceSummary {
            let invitation = self.control.open_pairing().unwrap();
            let ticket = PairingTicket::decode(&invitation.ticket).unwrap();
            let client_nonce = "direct-test-nonce";
            let expected_phrase =
                ticket.verification_phrase(&self.client.id().to_string(), client_nonce);
            let reply = pairing_round_trip(
                &self.connection,
                PairingOperation::Request(ygg_companion_protocol::PairingRequest {
                    invitation: ticket.invitation,
                    client_nonce: client_nonce.to_owned(),
                    observed_host_id: "host-test".to_owned(),
                    observed_host_endpoint_id: self.host.id().to_string(),
                    device: PairingDeviceClaim {
                        name: "Test phone".to_owned(),
                        platform: DevicePlatform::Ios,
                        app_version: "0.1.0".to_owned(),
                    },
                }),
            )
            .await;
            let (request_id, poll_token) = match reply {
                PairingReply::PendingRequest {
                    request_id,
                    poll_token,
                    phrase,
                    expires_at_ms,
                } => {
                    assert_eq!(phrase, expected_phrase);
                    assert_eq!(expires_at_ms, invitation.expires_at_ms);
                    (request_id, poll_token)
                }
                _ => panic!("unexpected pairing request response"),
            };
            self.control
                .decide_pairing(&request_id, PairingDecision::Approve)
                .unwrap();
            let proof = PairingStatusRequest {
                request_id,
                poll_token,
            };
            let approved =
                pairing_round_trip(&self.connection, PairingOperation::Status(proof.clone())).await;
            let approved_device = match approved {
                PairingReply::Approved { device } => device,
                _ => panic!("unexpected approved pairing response"),
            };
            let acknowledged =
                pairing_round_trip(&self.connection, PairingOperation::Ack(proof)).await;
            let device = match acknowledged {
                PairingReply::Acknowledged { device } => device,
                _ => panic!("unexpected pairing acknowledgement"),
            };
            assert_eq!(device.id, approved_device.id);

            self.connection.close(
                ygg_companion_protocol::RESET_CANCELLED.into(),
                b"pairing complete",
            );
            self.connection = self
                .client
                .connect(self.host.addr(), COMPANION_ALPN)
                .await
                .unwrap();
            device
        }

        async fn shutdown(mut self) {
            self.connection.close(
                ygg_companion_protocol::RESET_CANCELLED.into(),
                b"test shutdown",
            );
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            tokio::time::timeout(Duration::from_secs(5), &mut self.task)
                .await
                .unwrap()
                .unwrap();
            self.client.close().await;
        }
    }

    async fn open_blocked_request(
        connection: &iroh::endpoint::Connection,
        index: usize,
    ) -> (iroh::endpoint::SendStream, iroh::endpoint::RecvStream) {
        let (mut send, recv) = connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: format!("blocked-{index}"),
            method: HttpMethod::Get,
            path: "/api/v1/bootstrap".to_owned(),
            content_type: None,
        };
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::finish_body(&mut send)
            .await
            .unwrap();
        send.finish().unwrap();
        (send, recv)
    }

    async fn pair_additional_client(
        harness: &DirectHarness,
        client: &Endpoint,
        connection: &iroh::endpoint::Connection,
    ) -> DeviceSummary {
        let invitation = harness.control.open_pairing().unwrap();
        let ticket = PairingTicket::decode(&invitation.ticket).unwrap();
        let client_nonce = "additional-client-nonce";
        let expected_phrase = ticket.verification_phrase(&client.id().to_string(), client_nonce);
        let reply = pairing_round_trip(
            connection,
            PairingOperation::Request(ygg_companion_protocol::PairingRequest {
                invitation: ticket.invitation,
                client_nonce: client_nonce.to_owned(),
                observed_host_id: "host-test".to_owned(),
                observed_host_endpoint_id: harness.host.id().to_string(),
                device: PairingDeviceClaim {
                    name: "Other phone".to_owned(),
                    platform: DevicePlatform::Android,
                    app_version: "0.1.0".to_owned(),
                },
            }),
        )
        .await;
        let (request_id, poll_token) = match reply {
            PairingReply::PendingRequest {
                request_id,
                poll_token,
                phrase,
                expires_at_ms,
            } => {
                assert_eq!(phrase, expected_phrase);
                assert_eq!(expires_at_ms, invitation.expires_at_ms);
                (request_id, poll_token)
            }
            _ => panic!("unexpected pairing request response"),
        };
        harness
            .control
            .decide_pairing(&request_id, PairingDecision::Approve)
            .unwrap();
        let proof = PairingStatusRequest {
            request_id,
            poll_token,
        };
        assert!(matches!(
            pairing_round_trip(connection, PairingOperation::Status(proof.clone())).await,
            PairingReply::Approved { .. }
        ));
        match pairing_round_trip(connection, PairingOperation::Ack(proof)).await {
            PairingReply::Acknowledged { device } => device,
            _ => panic!("unexpected pairing acknowledgement"),
        }
    }

    async fn pairing_round_trip(
        connection: &iroh::endpoint::Connection,
        operation: PairingOperation,
    ) -> PairingReply {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let head = RequestHead::Pairing {
            protocol: PROTOCOL_VERSION,
            request_id: format!("pairing-{}", now_ms()),
            operation,
        };
        let request_id = head.request_id().to_owned();
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        let _ = send.finish();
        let response: ResponseHead = ygg_companion_protocol::read_head(&mut recv).await.unwrap();
        response.validate(&request_id).unwrap();
        let body = ygg_companion_protocol::read_body(&mut recv, PAIRING_RESPONSE_BYTES)
            .await
            .unwrap();
        ygg_companion_protocol::expect_end(&mut recv).await.unwrap();
        assert_eq!(response.status, 200, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    async fn http_round_trip(
        connection: &iroh::endpoint::Connection,
        method: HttpMethod,
        path: &str,
        body: &[u8],
    ) -> (ResponseHead, Vec<u8>) {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: format!("http-{}", now_ms()),
            method,
            path: path.to_owned(),
            content_type: (!body.is_empty()).then(|| "application/json".to_owned()),
        };
        let request_id = head.request_id().to_owned();
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        ygg_companion_protocol::write_body(&mut send, body)
            .await
            .unwrap();
        let _ = send.finish();
        let response: ResponseHead = ygg_companion_protocol::read_head(&mut recv).await.unwrap();
        response.validate(&request_id).unwrap();
        let body = ygg_companion_protocol::read_body(&mut recv, MAX_EVENT_BYTES)
            .await
            .unwrap();
        ygg_companion_protocol::expect_end(&mut recv).await.unwrap();
        (response, body)
    }

    async fn open_event_stream(
        connection: &iroh::endpoint::Connection,
    ) -> (iroh::endpoint::SendStream, iroh::endpoint::RecvStream) {
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        let head = RequestHead::Events {
            protocol: PROTOCOL_VERSION,
            request_id: format!("events-{}", now_ms()),
            path: "/api/v1/events".to_owned(),
        };
        let request_id = head.request_id().to_owned();
        ygg_companion_protocol::write_head(&mut send, &head)
            .await
            .unwrap();
        let _ = send.finish();
        let response: ResponseHead = ygg_companion_protocol::read_head(&mut recv).await.unwrap();
        response.validate(&request_id).unwrap();
        assert_eq!(response.status, 200);
        (send, recv)
    }

    fn command_body(device_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "hostId": "host-test",
            "deviceId": device_id,
            "commandId": "command-direct-test",
            "issuedAtMs": 1,
            "command": { "type": "project.clearDefault" }
        }))
        .unwrap()
    }

    fn test_event(sequence: u64) -> crate::HostStreamEvent {
        crate::HostStreamEvent::new(
            sequence,
            crate::EventEnvelope::new(
                crate::SessionId::new("session-direct-test").unwrap(),
                crate::SessionCursor {
                    actor_generation: 1,
                    sequence,
                },
                sequence,
                crate::EventPayload::SessionStateChanged {
                    state: crate::SessionLiveState::Idle,
                    active_run_id: None,
                },
            ),
        )
    }

    fn protocol_reset_code(error: &ygg_companion_protocol::ProtocolError) -> Option<u32> {
        let ygg_companion_protocol::ProtocolError::Io(error) = error else {
            return None;
        };
        let read_error = error
            .get_ref()?
            .downcast_ref::<iroh::endpoint::ReadError>()?;
        let code = match read_error {
            iroh::endpoint::ReadError::Reset(code) => *code,
            iroh::endpoint::ReadError::ConnectionLost(
                iroh::endpoint::ConnectionError::ApplicationClosed(close),
            ) => close.error_code,
            _ => return None,
        };
        Some(code.into_inner() as u32)
    }
}
