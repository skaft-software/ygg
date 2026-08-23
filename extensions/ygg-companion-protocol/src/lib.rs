//! Bounded, transport-neutral framing shared by the Ygg companion host and
//! native shell.
//!
//! This crate deliberately contains no Iroh, Tauri, Ygg-agent, filesystem, or
//! private-key type. It owns only the audited route allowlist, public pairing
//! payloads, and length-delimited records carried inside authenticated QUIC.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fmt;
use std::io;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Companion protocol major.
pub const PROTOCOL_VERSION: u16 = 1;
/// Iroh ALPN accepted by companion endpoints.
pub const COMPANION_ALPN: &[u8] = b"ygg/companion/1";
/// Maximum encoded request or response head.
pub const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Maximum bytes in one ordinary body frame.
pub const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// Maximum bytes in one validated host event.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// Reserved event-stream record sent while the host has no application event.
///
/// This record is never forwarded to the bundled web application. It keeps the
/// application-level idle deadline live while QUIC transport keepalives remain
/// transport-only.
pub const EVENT_HEARTBEAT_RECORD: &[u8] = b"\n";
/// Maximum bytes in the raw path component, excluding the query.
pub const MAX_PATH_BYTES: usize = 8 * 1024;
/// Maximum query bytes accepted by ordinary routes.
pub const MAX_QUERY_BYTES: usize = 4 * 1024;
/// Maximum encoded pairing ticket bytes.
pub const MAX_PAIRING_TICKET_BYTES: usize = 4 * 1024;
/// Maximum ordinary command/search/goal request.
pub const MAX_COMMAND_BYTES: usize = 512 * 1024;
/// Maximum attachment upload/response.
pub const MAX_ATTACHMENT_BYTES: usize = 5 * 1024 * 1024;
/// Maximum document upload.
pub const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum project-file write request after JSON escaping.
pub const MAX_PROJECT_FILE_WRITE_REQUEST_BYTES: usize = 1024 * 1024 * 6 + 2_048 * 6 + 1024;
/// Maximum snapshot/resource response.
pub const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum bootstrap response.
pub const MAX_BOOTSTRAP_BYTES: usize = 12 * 1024 * 1024;
/// Maximum redacted session export response.
pub const MAX_EXPORT_BYTES: usize = 64 * 1024 * 1024;
/// Maximum retained devices in one host registry/catalog.
pub const MAX_DEVICES: usize = 128;
/// Maximum simultaneous pending pairing requests.
pub const MAX_PENDING_PAIRINGS: usize = 3;

/// Application reset code for caller cancellation.
pub const RESET_CANCELLED: u32 = 0x10;
/// Application reset code for malformed or oversized framing.
pub const RESET_FRAME_INVALID: u32 = 0x11;
/// Application reset code for protocol mismatch.
pub const RESET_PROTOCOL_MISMATCH: u32 = 0x12;
/// Application reset code for an unknown endpoint.
pub const RESET_UNAUTHORIZED: u32 = 0x13;
/// Application reset code for a revoked endpoint.
pub const RESET_REVOKED: u32 = 0x14;
/// Application reset code requiring event replay.
pub const RESET_REPLAY_REQUIRED: u32 = 0x15;
/// Application reset code for a sanitized internal failure.
pub const RESET_INTERNAL: u32 = 0x16;

/// One HTTP method admitted by the remote operational boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// Read an operational resource.
    Get,
    /// Submit one bounded mutation or upload.
    Post,
}

/// Audited class of one operational route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    /// Host bootstrap.
    Bootstrap,
    /// Ordinary bounded JSON request/response.
    Json,
    /// Session snapshot or replay.
    Snapshot,
    /// Image attachment upload or retrieval.
    Attachment,
    /// Prompt-document upload or listing.
    Document,
    /// Trusted project full-file replacement.
    ProjectFileWrite,
    /// Opaque evidence resource.
    Resource,
    /// Redacted session export.
    Export,
}

/// Aggregate limits associated with an audited route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteLimits {
    /// Route classification.
    pub class: RouteClass,
    /// Maximum aggregate request body bytes.
    pub request_bytes: usize,
    /// Maximum aggregate response body bytes.
    pub response_bytes: usize,
}

impl RouteLimits {
    const fn no_body(class: RouteClass, response_bytes: usize) -> Self {
        Self {
            class,
            request_bytes: 0,
            response_bytes,
        }
    }

    const fn body(class: RouteClass, request_bytes: usize, response_bytes: usize) -> Self {
        Self {
            class,
            request_bytes,
            response_bytes,
        }
    }
}

/// Returns route-specific bounds only for an explicitly audited, non-terminal
/// operational route.
pub fn classify_operational_route(
    method: HttpMethod,
    raw_path: &str,
) -> Result<RouteLimits, ProtocolError> {
    let (path, query) = validate_path(raw_path)?;
    let segments = path
        .strip_prefix('/')
        .expect("validated absolute path")
        .split('/')
        .collect::<Vec<_>>();

    let route = match (method, segments.as_slice()) {
        (HttpMethod::Get, ["api", "v1", "bootstrap"]) => {
            RouteLimits::no_body(RouteClass::Bootstrap, MAX_BOOTSTRAP_BYTES)
        }
        (HttpMethod::Get, ["api", "v1", "projects"])
        | (HttpMethod::Get, ["api", "v1", "usage", "stats"])
        | (HttpMethod::Get, ["api", "v1", "usage", "lifetime"])
        | (HttpMethod::Get, ["api", "v1", "usage", "activity"])
        | (HttpMethod::Get, ["api", "v1", "projects", _, "context"])
        | (HttpMethod::Get, ["api", "v1", "sessions", _, "commands"])
        | (HttpMethod::Get, ["api", "v1", "sessions", _, "goal"])
        | (HttpMethod::Get, ["api", "v1", "sessions", _, "documents"])
        | (HttpMethod::Get, ["api", "v1", "projects", _, "files"])
        | (HttpMethod::Get, ["api", "v1", "projects", _, "files", "search"])
        | (HttpMethod::Get, ["api", "v1", "projects", _, "files", _])
        | (HttpMethod::Get, ["api", "v1", "fs", _, "tree"])
        | (HttpMethod::Get, ["api", "v1", "fs", _, "read"])
        | (HttpMethod::Get, ["api", "v1", "fs", _, "search"]) => {
            RouteLimits::no_body(RouteClass::Json, MAX_BOOTSTRAP_BYTES)
        }
        (HttpMethod::Get, ["api", "v1", "sessions", _])
        | (HttpMethod::Get, ["api", "v1", "sessions", _, "replay"]) => {
            RouteLimits::no_body(RouteClass::Snapshot, MAX_SNAPSHOT_BYTES)
        }
        (HttpMethod::Get, ["api", "v1", "sessions", _, "export"]) => {
            if query.is_some() {
                return Err(ProtocolError::InvalidPath);
            }
            RouteLimits::no_body(RouteClass::Export, MAX_EXPORT_BYTES)
        }
        (HttpMethod::Get, ["api", "v1", "attachments", _]) => {
            if query.is_some() {
                return Err(ProtocolError::InvalidPath);
            }
            RouteLimits::no_body(RouteClass::Attachment, MAX_ATTACHMENT_BYTES)
        }
        (HttpMethod::Get, ["api", "v1", "sessions", _, "resources", _]) => {
            if query.is_some() {
                return Err(ProtocolError::InvalidPath);
            }
            RouteLimits::no_body(RouteClass::Resource, MAX_SNAPSHOT_BYTES)
        }
        (HttpMethod::Post, ["api", "v1", "commands", "host"])
        | (HttpMethod::Post, ["api", "v1", "commands", "session"])
        | (HttpMethod::Post, ["api", "v1", "search"])
        | (HttpMethod::Post, ["api", "v1", "sessions", _, "goal"]) => {
            RouteLimits::body(RouteClass::Json, MAX_COMMAND_BYTES, MAX_BOOTSTRAP_BYTES)
        }
        (HttpMethod::Post, ["api", "v1", "attachments"]) => RouteLimits::body(
            RouteClass::Attachment,
            MAX_ATTACHMENT_BYTES,
            MAX_COMMAND_BYTES,
        ),
        (HttpMethod::Post, ["api", "v1", "sessions", _, "documents"]) => {
            RouteLimits::body(RouteClass::Document, MAX_DOCUMENT_BYTES, MAX_COMMAND_BYTES)
        }
        (HttpMethod::Post, ["api", "v1", "fs", _, "write"]) => RouteLimits::body(
            RouteClass::ProjectFileWrite,
            MAX_PROJECT_FILE_WRITE_REQUEST_BYTES,
            MAX_BOOTSTRAP_BYTES,
        ),
        _ => return Err(ProtocolError::RouteNotAllowed),
    };

    Ok(route)
}

/// Validates the sole companion event-subscription path.
pub fn validate_event_path(raw_path: &str) -> Result<(), ProtocolError> {
    let (path, query) = validate_path(raw_path)?;
    if path == "/api/v1/events" && query.is_none() {
        Ok(())
    } else {
        Err(ProtocolError::RouteNotAllowed)
    }
}

fn validate_path(raw_path: &str) -> Result<(&str, Option<&str>), ProtocolError> {
    if raw_path.is_empty()
        || !raw_path.starts_with('/')
        || !raw_path.is_ascii()
        || raw_path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        || raw_path.contains('#')
        || raw_path.contains('\\')
        || raw_path.contains("://")
    {
        return Err(ProtocolError::InvalidPath);
    }
    let (path, query) = match raw_path.split_once('?') {
        Some((path, query)) => {
            if query.is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('?') {
                return Err(ProtocolError::InvalidPath);
            }
            (path, Some(query))
        }
        None => (raw_path, None),
    };
    if path.len() > MAX_PATH_BYTES {
        return Err(ProtocolError::InvalidPath);
    }
    let lower_path = path.to_ascii_lowercase();
    if path.contains("//")
        || lower_path.contains("%2f")
        || lower_path.contains("%5c")
        || lower_path.contains("%2e")
        || path
            .strip_prefix('/')
            .expect("absolute path")
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(ProtocolError::InvalidPath);
    }
    Ok((path, query))
}

/// A zeroizing 256-bit pairing capability.
///
/// Its only serialization is the explicit base64url wire representation used
/// by pairing records. Debug output is always redacted and Display is absent.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Secret32([u8; 32]);

impl Secret32 {
    /// Wraps freshly generated random bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses the canonical unpadded base64url wire representation.
    pub fn parse(encoded: &str) -> Result<Self, ProtocolError> {
        let bytes = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| ProtocolError::InvalidSecret)?,
        );
        if bytes.len() != 32 {
            return Err(ProtocolError::InvalidSecret);
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        Ok(Self(secret))
    }

    /// Constant-time equality for capability checks.
    pub fn constant_time_eq(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }

    fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(self.0))
    }
}

impl fmt::Debug for Secret32 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret32([REDACTED])")
    }
}

impl PartialEq for Secret32 {
    fn eq(&self, other: &Self) -> bool {
        self.constant_time_eq(other)
    }
}

impl Eq for Secret32 {}

mod secret32_wire {
    use super::{Deserializer, Secret32, Serializer, Zeroizing};
    use serde::Deserialize as _;

    pub(super) fn serialize<S>(secret: &Secret32, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let encoded = secret.encode();
        serializer.serialize_str(encoded.as_str())
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Secret32, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = Zeroizing::new(String::deserialize(deserializer)?);
        Secret32::parse(encoded.as_str()).map_err(serde::de::Error::custom)
    }
}

/// Public platform label supplied by a pairing device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DevicePlatform {
    /// Apple mobile device.
    Ios,
    /// Android mobile device.
    Android,
    /// Apple desktop development shell.
    Macos,
    /// Other development harness.
    Other,
}

/// Untrusted, bounded device presentation metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingDeviceClaim {
    /// Owner-visible device name.
    pub name: String,
    /// Native platform.
    pub platform: DevicePlatform,
    /// Bounded native app version.
    pub app_version: String,
}

impl PairingDeviceClaim {
    /// Validates presentation bounds and control characters.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_public_text(&self.name, 128, false)?;
        validate_public_text(&self.app_version, 64, false)
    }
}

/// First pairing request made by an authenticated but not-yet-authorized
/// endpoint.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingRequest {
    /// One-use invitation capability.
    #[serde(with = "secret32_wire")]
    pub invitation: Secret32,
    /// Random idempotency nonce chosen by the device.
    pub client_nonce: String,
    /// Host ID observed in the ticket.
    pub observed_host_id: String,
    /// Host endpoint ID observed in the ticket.
    pub observed_host_endpoint_id: String,
    /// Device presentation claim.
    pub device: PairingDeviceClaim,
}

impl PairingRequest {
    /// Validates public fields. Secret validity is enforced while decoding.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier(&self.client_nonce, 128)?;
        validate_identifier(&self.observed_host_id, 128)?;
        validate_identifier(&self.observed_host_endpoint_id, 128)?;
        self.device.validate()
    }
}

/// Polls an existing pairing request with its independent capability.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingStatusRequest {
    /// Host-created request ID.
    pub request_id: String,
    /// Independent status capability.
    #[serde(with = "secret32_wire")]
    pub poll_token: Secret32,
}

impl PairingStatusRequest {
    /// Validates public fields.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier(&self.request_id, 128)
    }
}

/// Acknowledges durable native storage or cancels pairing.
pub type PairingRequestProof = PairingStatusRequest;

/// Pairing operation carried in a request head.
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "payload",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum PairingOperation {
    /// Create a pending request.
    Request(PairingRequest),
    /// Poll pending/approved state.
    Status(PairingStatusRequest),
    /// Confirm that native protected storage succeeded.
    Ack(PairingRequestProof),
    /// Cancel a pending/approved request.
    Cancel(PairingRequestProof),
}

impl PairingOperation {
    /// Validates all public fields.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Request(request) => request.validate(),
            Self::Status(request) | Self::Ack(request) | Self::Cancel(request) => {
                request.validate()
            }
        }
    }
}

/// One application-level request stream head.
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RequestHead {
    /// One allowlisted HTTP-shaped operation.
    Http {
        /// Protocol major.
        protocol: u16,
        /// Caller-generated request ID.
        request_id: String,
        /// HTTP method.
        method: HttpMethod,
        /// Root-relative path and optional query.
        path: String,
        /// Sole forwarded request header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
    },
    /// Long-lived host event subscription.
    Events {
        /// Protocol major.
        protocol: u16,
        /// Caller-generated request ID.
        request_id: String,
        /// Exact event path.
        path: String,
    },
    /// Pairing control operation.
    Pairing {
        /// Protocol major.
        protocol: u16,
        /// Caller-generated request ID.
        request_id: String,
        /// Typed operation.
        operation: PairingOperation,
    },
}

impl RequestHead {
    /// Protocol major in this request.
    pub fn protocol(&self) -> u16 {
        match self {
            Self::Http { protocol, .. }
            | Self::Events { protocol, .. }
            | Self::Pairing { protocol, .. } => *protocol,
        }
    }

    /// Request ID used to bind the response.
    pub fn request_id(&self) -> &str {
        match self {
            Self::Http { request_id, .. }
            | Self::Events { request_id, .. }
            | Self::Pairing { request_id, .. } => request_id,
        }
    }

    /// Validates the request and returns route limits for HTTP operations.
    pub fn validate(&self) -> Result<Option<RouteLimits>, ProtocolError> {
        if self.protocol() != PROTOCOL_VERSION {
            return Err(ProtocolError::ProtocolMismatch);
        }
        validate_identifier(self.request_id(), 128)?;
        match self {
            Self::Http {
                method,
                path,
                content_type,
                ..
            } => {
                if let Some(content_type) = content_type {
                    validate_content_type(content_type)?;
                }
                classify_operational_route(*method, path).map(Some)
            }
            Self::Events { path, .. } => {
                validate_event_path(path)?;
                Ok(None)
            }
            Self::Pairing { operation, .. } => {
                operation.validate()?;
                Ok(None)
            }
        }
    }
}

/// One allowlisted response header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseHeader {
    /// Lowercase audited header name.
    pub name: String,
    /// Bounded visible header value.
    pub value: String,
}

/// Response head sent before HTTP chunks or event records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResponseHead {
    /// Protocol major.
    pub protocol: u16,
    /// Matching request ID.
    pub request_id: String,
    /// HTTP-like status code.
    pub status: u16,
    /// Audited response headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<ResponseHeader>,
}

impl ResponseHead {
    /// Validates protocol, request binding, status, and header allowlist.
    pub fn validate(&self, expected_request_id: &str) -> Result<(), ProtocolError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ProtocolError::ProtocolMismatch);
        }
        validate_identifier(&self.request_id, 128)?;
        if self.request_id != expected_request_id {
            return Err(ProtocolError::RequestIdMismatch);
        }
        if !(200..=599).contains(&self.status) || self.headers.len() > 12 {
            return Err(ProtocolError::InvalidResponse);
        }
        let mut names = BTreeSet::new();
        for header in &self.headers {
            if !matches!(
                header.name.as_str(),
                "content-type"
                    | "content-disposition"
                    | "content-length"
                    | "cache-control"
                    | "etag"
                    | "x-content-type-options"
                    | "referrer-policy"
                    | "cross-origin-resource-policy"
            ) || !names.insert(header.name.as_str())
                || header.value.is_empty()
                || header.value.len() > 4 * 1024
                || header
                    .value
                    .bytes()
                    .any(|byte| byte.is_ascii_control() && !matches!(byte, b'\t'))
                || (header.name == "content-length"
                    && (header.value.bytes().any(|byte| !byte.is_ascii_digit())
                        || header.value.parse::<u64>().is_err()))
            {
                return Err(ProtocolError::InvalidResponse);
            }
        }
        Ok(())
    }

    /// Returns a declared body length only when it fits the route bound.
    pub fn content_length(&self, maximum: usize) -> Result<Option<usize>, ProtocolError> {
        let mut values = self
            .headers
            .iter()
            .filter(|header| header.name == "content-length")
            .map(|header| header.value.as_str());
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some()
            || value.is_empty()
            || value.bytes().any(|byte| !byte.is_ascii_digit())
        {
            return Err(ProtocolError::InvalidResponse);
        }
        let length = value
            .parse::<u64>()
            .ok()
            .and_then(|length| usize::try_from(length).ok())
            .filter(|length| *length <= maximum)
            .ok_or(ProtocolError::InvalidResponse)?;
        Ok(Some(length))
    }
}

/// Public state of one paired or revoked device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeviceSummary {
    /// Assigned Ygg device ID.
    pub id: String,
    /// Owner-visible name.
    pub name: String,
    /// Native platform.
    pub platform: DevicePlatform,
    /// Pairing completion time.
    pub paired_at_ms: u64,
    /// Last observed stream time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at_ms: Option<u64>,
    /// Revocation time, if revoked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at_ms: Option<u64>,
    /// Whether an active connection currently exists.
    pub connected: bool,
}

impl DeviceSummary {
    /// Validates public device metadata.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier(&self.id, 128)?;
        validate_public_text(&self.name, 128, false)?;
        if self.revoked_at_ms.is_some() && self.connected {
            return Err(ProtocolError::InvalidPairing);
        }
        Ok(())
    }
}

/// Owner-visible state of an unacknowledged pairing request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingPairingState {
    /// The local owner has not decided yet.
    Pending,
    /// The owner approved and the client still needs to acknowledge storage.
    Approved,
}

/// Owner-visible pending request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingPairingSummary {
    /// Pairing request ID.
    pub request_id: String,
    /// Untrusted device metadata.
    pub device: PairingDeviceClaim,
    /// Current owner-visible pairing state.
    pub state: PendingPairingState,
    /// Human-verification phrase.
    pub phrase: String,
    /// Request expiry.
    pub expires_at_ms: u64,
}

/// Owner-visible catalog and pairing state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionCatalog {
    /// Durable device registry revision.
    pub revision: u64,
    /// Paired and retained revoked devices.
    pub devices: Vec<DeviceSummary>,
    /// In-memory pending requests.
    pub pending: Vec<PendingPairingSummary>,
    /// Active invitation expiry, if pairing is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invitation_expires_at_ms: Option<u64>,
}

/// Decision accepted by the owner-only loopback route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PairingDecision {
    /// Approve the pending endpoint.
    Approve,
    /// Deny the pending endpoint.
    Deny,
}

/// Exact owner decision body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingDecisionRequest {
    /// Owner decision.
    pub decision: PairingDecision,
}

/// Owner response when opening a one-use invitation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingInvitation {
    /// Complete import URI containing the short-lived secret.
    pub ticket: String,
    /// Expiry timestamp.
    pub expires_at_ms: u64,
}

/// Device-facing pairing state returned in a bounded response body.
#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PairingReply {
    /// Request was accepted and awaits the owner.
    PendingRequest {
        /// Host-created request ID.
        request_id: String,
        /// Independent polling capability.
        #[serde(with = "secret32_wire")]
        poll_token: Secret32,
        /// Human-verification phrase.
        phrase: String,
        /// Expiry timestamp.
        expires_at_ms: u64,
    },
    /// Owner has not decided yet.
    Pending {
        /// Human-verification phrase.
        phrase: String,
        /// Expiry timestamp.
        expires_at_ms: u64,
    },
    /// Approved identity awaiting native storage acknowledgement.
    Approved {
        /// Assigned device metadata.
        device: DeviceSummary,
    },
    /// Durable registration is active.
    Acknowledged {
        /// Assigned device metadata.
        device: DeviceSummary,
    },
    /// Owner denied the request.
    Denied,
    /// Request or invitation expired.
    Expired,
    /// Request was cancelled.
    Cancelled,
}

impl PairingReply {
    /// Validates every public field in a device-facing pairing reply.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::PendingRequest {
                request_id,
                phrase,
                expires_at_ms,
                ..
            } => {
                validate_identifier(request_id, 128)?;
                validate_public_text(phrase, 128, false)?;
                if *expires_at_ms == 0 {
                    return Err(ProtocolError::InvalidPairing);
                }
                Ok(())
            }
            Self::Pending {
                phrase,
                expires_at_ms,
            } => {
                validate_public_text(phrase, 128, false)?;
                if *expires_at_ms == 0 {
                    return Err(ProtocolError::InvalidPairing);
                }
                Ok(())
            }
            Self::Approved { device } | Self::Acknowledged { device } => {
                device.validate()?;
                if device.paired_at_ms == 0 || device.revoked_at_ms.is_some() {
                    return Err(ProtocolError::InvalidPairing);
                }
                Ok(())
            }
            Self::Denied | Self::Expired | Self::Cancelled => Ok(()),
        }
    }
}

/// Decoded application-owned host invitation.
pub struct PairingTicket {
    /// Protocol major.
    pub protocol: u16,
    /// Stable Ygg host ID.
    pub host_id: String,
    /// Pinned Iroh endpoint ID.
    pub host_endpoint_id: String,
    /// Bounded relay location hints.
    pub relay_urls: Vec<String>,
    /// Bounded direct socket hints.
    pub direct_addresses: Vec<String>,
    /// One-use invitation capability.
    pub invitation: Secret32,
    /// Expiry timestamp.
    pub expires_at_ms: u64,
}

impl fmt::Debug for PairingTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingTicket")
            .field("protocol", &self.protocol)
            .field("host_id", &self.host_id)
            .field("host_endpoint_id", &self.host_endpoint_id)
            .field("relay_urls", &self.relay_urls)
            .field("direct_addresses", &self.direct_addresses)
            .field("invitation", &"[REDACTED]")
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairingTicketWire {
    protocol: u16,
    host_id: String,
    host_endpoint_id: String,
    relay_urls: Vec<String>,
    direct_addresses: Vec<String>,
    #[serde(with = "secret32_wire")]
    invitation: Secret32,
    expires_at_ms: u64,
}

impl PairingTicket {
    /// Encodes the bounded canonical import URI.
    pub fn encode(&self) -> Result<String, ProtocolError> {
        self.validate()?;
        let payload = Zeroizing::new(
            serde_json::to_vec(&PairingTicketWire {
                protocol: self.protocol,
                host_id: self.host_id.clone(),
                host_endpoint_id: self.host_endpoint_id.clone(),
                relay_urls: self.relay_urls.clone(),
                direct_addresses: self.direct_addresses.clone(),
                invitation: self.invitation.clone(),
                expires_at_ms: self.expires_at_ms,
            })
            .map_err(|_| ProtocolError::InvalidJson)?,
        );
        let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(payload.as_slice()));
        let ticket = format!("ygg://pair/v1/{}", encoded.as_str());
        if ticket.len() > MAX_PAIRING_TICKET_BYTES {
            return Err(ProtocolError::AggregateTooLarge);
        }
        Ok(ticket)
    }

    /// Decodes and validates one import URI.
    pub fn decode(ticket: &str) -> Result<Self, ProtocolError> {
        if ticket.len() > MAX_PAIRING_TICKET_BYTES {
            return Err(ProtocolError::AggregateTooLarge);
        }
        let encoded = ticket
            .strip_prefix("ygg://pair/v1/")
            .ok_or(ProtocolError::InvalidPairing)?;
        let payload = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| ProtocolError::InvalidPairing)?,
        );
        if payload.len() > MAX_PAIRING_TICKET_BYTES {
            return Err(ProtocolError::AggregateTooLarge);
        }
        let wire: PairingTicketWire =
            serde_json::from_slice(payload.as_slice()).map_err(|_| ProtocolError::InvalidJson)?;
        let ticket = Self {
            protocol: wire.protocol,
            host_id: wire.host_id,
            host_endpoint_id: wire.host_endpoint_id,
            relay_urls: wire.relay_urls,
            direct_addresses: wire.direct_addresses,
            invitation: wire.invitation,
            expires_at_ms: wire.expires_at_ms,
        };
        ticket.validate()?;
        Ok(ticket)
    }

    /// Validates identity, location, and aggregate bounds.
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ProtocolError::ProtocolMismatch);
        }
        validate_identifier(&self.host_id, 128)?;
        validate_identifier(&self.host_endpoint_id, 128)?;
        if self.relay_urls.is_empty()
            || self.relay_urls.len() > 8
            || self.direct_addresses.len() > 16
            || self.expires_at_ms == 0
        {
            return Err(ProtocolError::InvalidPairing);
        }
        for relay in &self.relay_urls {
            if relay.len() > 512
                || !relay.starts_with("https://")
                || !relay.is_ascii()
                || relay.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err(ProtocolError::InvalidPairing);
            }
        }
        for address in &self.direct_addresses {
            if address.is_empty()
                || address.len() > 128
                || !address.is_ascii()
                || address.bytes().any(|byte| byte.is_ascii_control())
            {
                return Err(ProtocolError::InvalidPairing);
            }
        }
        Ok(())
    }

    /// Derives the phrase for one authenticated device pairing request.
    pub fn verification_phrase(&self, device_endpoint_id: &str, client_nonce: &str) -> String {
        pairing_verification_phrase(
            &self.host_id,
            &self.host_endpoint_id,
            device_endpoint_id,
            client_nonce,
            &self.invitation,
        )
    }
}

/// Returns a six-word phrase bound to the host, invitation, authenticated
/// device endpoint, and one client request nonce.
pub fn pairing_verification_phrase(
    host_id: &str,
    host_endpoint_id: &str,
    device_endpoint_id: &str,
    client_nonce: &str,
    invitation: &Secret32,
) -> String {
    const WORDS: [&str; 32] = [
        "amber", "birch", "cairn", "delta", "ember", "fern", "glade", "harbor", "iris", "juniper",
        "kite", "lantern", "maple", "north", "opal", "pine", "quartz", "river", "spruce", "tide",
        "umber", "vale", "willow", "xenon", "yarrow", "zephyr", "acorn", "brook", "cedar", "drift",
        "elm", "frost",
    ];
    let mut digest = Sha256::new();
    digest.update(b"ygg-companion-pairing-phrase-v1\0");
    for value in [
        host_id.as_bytes(),
        host_endpoint_id.as_bytes(),
        device_endpoint_id.as_bytes(),
        client_nonce.as_bytes(),
        invitation.0.as_slice(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    let digest = digest.finalize();
    let mut accumulator = 0u64;
    for byte in digest.iter().take(4) {
        accumulator = (accumulator << 8) | u64::from(*byte);
    }
    (0..6)
        .map(|index| WORDS[((accumulator >> (index * 5)) & 31) as usize])
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Writes one bounded JSON head.
pub async fn write_head<W, T>(writer: &mut W, value: &T) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = Zeroizing::new(serde_json::to_vec(value).map_err(|_| ProtocolError::InvalidJson)?);
    if bytes.is_empty() || bytes.len() > MAX_HEAD_BYTES {
        return Err(ProtocolError::HeadTooLarge);
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes.as_slice()).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one bounded JSON head.
pub async fn read_head<R, T>(reader: &mut R) -> Result<T, ProtocolError>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_HEAD_BYTES {
        return Err(ProtocolError::HeadTooLarge);
    }
    let mut bytes = Zeroizing::new(vec![0; length]);
    reader.read_exact(bytes.as_mut_slice()).await?;
    serde_json::from_slice(bytes.as_slice()).map_err(|_| ProtocolError::InvalidJson)
}

/// Writes a byte body as bounded chunks followed by an end record.
pub async fn write_body<W>(writer: &mut W, bytes: &[u8]) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
        write_chunk(writer, chunk).await?;
    }
    finish_body(writer).await
}

/// Writes one non-empty bounded body chunk.
pub async fn write_chunk<W>(writer: &mut W, bytes: &[u8]) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(ProtocolError::ChunkTooLarge);
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Writes the zero-length end-of-body record.
pub async fn finish_body<W>(writer: &mut W) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_u32(0).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one bounded body chunk, or `None` for the end-of-body record.
pub async fn read_chunk<R>(
    reader: &mut R,
    maximum_remaining: usize,
) -> Result<Option<Vec<u8>>, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 {
        return Ok(None);
    }
    if length > MAX_CHUNK_BYTES {
        return Err(ProtocolError::ChunkTooLarge);
    }
    if length > maximum_remaining {
        return Err(ProtocolError::AggregateTooLarge);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(bytes))
}

/// Reads and accumulates one bounded body through its end record.
pub async fn read_body<R>(reader: &mut R, maximum: usize) -> Result<Vec<u8>, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    while let Some(chunk) = read_chunk(reader, maximum.saturating_sub(body.len())).await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Requires transport EOF immediately after a complete framed message.
///
/// Framed terminators delimit a body, while the transport FIN authenticates
/// that no second message or trailing bytes were smuggled onto the stream.
pub async fn expect_end<R>(reader: &mut R) -> Result<(), ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing).await? == 0 {
        Ok(())
    } else {
        Err(ProtocolError::TrailingData)
    }
}

/// Writes one independent non-empty event record.
pub async fn write_record<W>(
    writer: &mut W,
    bytes: &[u8],
    maximum: usize,
) -> Result<(), ProtocolError>
where
    W: AsyncWrite + Unpin,
{
    if bytes.is_empty() || bytes.len() > maximum || bytes.len() > u32::MAX as usize {
        return Err(ProtocolError::AggregateTooLarge);
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads one independent non-empty record.
pub async fn read_record<R>(reader: &mut R, maximum: usize) -> Result<Vec<u8>, ProtocolError>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > maximum {
        return Err(ProtocolError::AggregateTooLarge);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

fn validate_identifier(value: &str, maximum: usize) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(ProtocolError::InvalidIdentifier)
    } else {
        Ok(())
    }
}

fn validate_public_text(
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), ProtocolError> {
    if value.len() > maximum
        || (!allow_empty && value.trim().is_empty())
        || value.chars().any(char::is_control)
    {
        Err(ProtocolError::InvalidPairing)
    } else {
        Ok(())
    }
}

fn validate_content_type(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > 255
        || !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'\r' | b'\n'))
    {
        Err(ProtocolError::InvalidContentType)
    } else {
        Ok(())
    }
}

/// Sanitized framing/contract error.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    /// Underlying bounded stream ended or failed.
    #[error("companion stream failed")]
    Io(#[from] io::Error),
    /// JSON did not match the strict wire shape.
    #[error("companion JSON is invalid")]
    InvalidJson,
    /// A JSON head exceeded its bound.
    #[error("companion head exceeds its limit")]
    HeadTooLarge,
    /// One ordinary body chunk exceeded its bound.
    #[error("companion chunk exceeds its limit")]
    ChunkTooLarge,
    /// An aggregate body, record, or ticket exceeded its bound.
    #[error("companion payload exceeds its limit")]
    AggregateTooLarge,
    /// Bytes followed a complete framed body instead of an immediate FIN.
    #[error("companion stream contains trailing data")]
    TrailingData,
    /// A root-relative path failed canonical validation.
    #[error("companion path is invalid")]
    InvalidPath,
    /// A well-formed route is not remotely exposed.
    #[error("companion route is not allowed")]
    RouteNotAllowed,
    /// Protocol majors differ.
    #[error("companion protocol mismatch")]
    ProtocolMismatch,
    /// Request/response IDs differ.
    #[error("companion response does not match its request")]
    RequestIdMismatch,
    /// An identifier failed its bounded grammar.
    #[error("companion identifier is invalid")]
    InvalidIdentifier,
    /// A request content type failed validation.
    #[error("companion content type is invalid")]
    InvalidContentType,
    /// A response status/header failed validation.
    #[error("companion response is invalid")]
    InvalidResponse,
    /// A pairing capability is malformed.
    #[error("pairing capability is invalid")]
    InvalidSecret,
    /// A pairing payload is malformed.
    #[error("pairing payload is invalid")]
    InvalidPairing,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(byte: u8) -> Secret32 {
        Secret32::from_bytes([byte; 32])
    }

    #[test]
    fn route_allowlist_is_exact_and_terminal_is_absent() {
        assert_eq!(
            classify_operational_route(HttpMethod::Get, "/api/v1/bootstrap?inventoryOnly=true")
                .unwrap()
                .class,
            RouteClass::Bootstrap
        );
        assert_eq!(
            classify_operational_route(
                HttpMethod::Get,
                "/api/v1/sessions/session-1/resources/resource-1"
            )
            .unwrap()
            .class,
            RouteClass::Resource
        );
        let snapshot =
            classify_operational_route(HttpMethod::Get, "/api/v1/sessions/session-1").unwrap();
        assert_eq!(snapshot.class, RouteClass::Snapshot);
        assert_eq!(snapshot.response_bytes, MAX_SNAPSHOT_BYTES);
        assert!(matches!(
            classify_operational_route(HttpMethod::Get, "/api/v1/terminal"),
            Err(ProtocolError::RouteNotAllowed)
        ));
        assert!(matches!(
            classify_operational_route(HttpMethod::Get, "/api/v1/companion/devices"),
            Err(ProtocolError::RouteNotAllowed)
        ));
        assert!(
            classify_operational_route(HttpMethod::Get, "https://host/api/v1/bootstrap").is_err()
        );
        assert!(classify_operational_route(HttpMethod::Get, "/api/v1/sessions/%2e%2e").is_err());
        assert!(classify_operational_route(HttpMethod::Get, "/api/v1/sessions/a%2fb").is_err());
        assert!(classify_operational_route(HttpMethod::Get, "/api/v1/bootstrap\r\n").is_err());
        assert!(classify_operational_route(HttpMethod::Get, "/api/v1/bootstrap?x=a b").is_err());
    }

    #[test]
    fn path_and_query_limits_are_independent() {
        const PREFIX: &str = "/api/v1/sessions/";
        const SUFFIX: &str = "/replay";
        let identifier = "a".repeat(MAX_PATH_BYTES - PREFIX.len() - SUFFIX.len());
        let query = format!("q={}", "b".repeat(MAX_QUERY_BYTES - 2));
        let target = format!("{PREFIX}{identifier}{SUFFIX}?{query}");
        assert_eq!(
            classify_operational_route(HttpMethod::Get, &target)
                .unwrap()
                .class,
            RouteClass::Snapshot
        );

        let oversized_path = format!("{PREFIX}{identifier}a{SUFFIX}?q=1");
        assert!(matches!(
            classify_operational_route(HttpMethod::Get, &oversized_path),
            Err(ProtocolError::InvalidPath)
        ));
        let oversized_query = format!("/api/v1/bootstrap?q={}", "b".repeat(MAX_QUERY_BYTES));
        assert!(matches!(
            classify_operational_route(HttpMethod::Get, &oversized_query),
            Err(ProtocolError::InvalidPath)
        ));
    }

    #[test]
    fn ticket_round_trip_pins_identity_and_redacts_secret() {
        let ticket = PairingTicket {
            protocol: PROTOCOL_VERSION,
            host_id: "host-example".into(),
            host_endpoint_id: "endpoint-example".into(),
            relay_urls: vec!["https://relay.example".into()],
            direct_addresses: vec!["127.0.0.1:1234".into()],
            invitation: secret(7),
            expires_at_ms: 42,
        };
        let encoded = ticket.encode().unwrap();
        let decoded = PairingTicket::decode(&encoded).unwrap();
        assert_eq!(decoded.host_id, "host-example");
        assert!(decoded.invitation.constant_time_eq(&secret(7)));
        assert!(!format!("{decoded:?}").contains(secret(7).encode().as_str()));
        let phrase = decoded.verification_phrase("device-endpoint-a", "client-nonce-a");
        assert_eq!(
            phrase,
            pairing_verification_phrase(
                "host-example",
                "endpoint-example",
                "device-endpoint-a",
                "client-nonce-a",
                &secret(7),
            )
        );
        assert_ne!(
            phrase,
            decoded.verification_phrase("device-endpoint-b", "client-nonce-a")
        );
        assert_ne!(
            phrase,
            decoded.verification_phrase("device-endpoint-a", "client-nonce-b")
        );
    }

    #[test]
    fn secret_debug_is_redacted_and_only_wire_dtos_serialize_it() {
        let value = secret(9);
        assert_eq!(format!("{value:?}"), "Secret32([REDACTED])");
        let proof = PairingStatusRequest {
            request_id: "pair-test".into(),
            poll_token: value.clone(),
        };
        let encoded = serde_json::to_string(&proof).unwrap();
        assert!(!format!("{value:?}").contains(value.encode().as_str()));
        let decoded: PairingStatusRequest = serde_json::from_str(&encoded).unwrap();
        assert!(decoded.poll_token.constant_time_eq(&value));
    }

    #[test]
    fn pairing_replies_validate_every_variant_field() {
        let valid = PairingReply::PendingRequest {
            request_id: "pair-request".into(),
            poll_token: secret(4),
            phrase: "amber · birch · cedar · drift · elm · frost".into(),
            expires_at_ms: 42,
        };
        valid.validate().unwrap();

        let invalid_request = PairingReply::PendingRequest {
            request_id: "invalid/request".into(),
            poll_token: secret(4),
            phrase: "amber · birch · cedar · drift · elm · frost".into(),
            expires_at_ms: 42,
        };
        assert!(matches!(
            invalid_request.validate(),
            Err(ProtocolError::InvalidIdentifier)
        ));
        let invalid_pending = PairingReply::Pending {
            phrase: "invalid\u{7f}phrase".into(),
            expires_at_ms: 42,
        };
        assert!(matches!(
            invalid_pending.validate(),
            Err(ProtocolError::InvalidPairing)
        ));
        let revoked_device = PairingReply::Acknowledged {
            device: DeviceSummary {
                id: "device-one".into(),
                name: "Phone".into(),
                platform: DevicePlatform::Ios,
                paired_at_ms: 1,
                last_seen_at_ms: None,
                revoked_at_ms: Some(2),
                connected: false,
            },
        };
        assert!(matches!(
            revoked_device.validate(),
            Err(ProtocolError::InvalidPairing)
        ));
    }

    #[test]
    fn response_headers_reject_duplicates_and_invalid_lengths() {
        let mut response = ResponseHead {
            protocol: PROTOCOL_VERSION,
            request_id: "request-1".into(),
            status: 200,
            headers: vec![ResponseHeader {
                name: "content-length".into(),
                value: "12".into(),
            }],
        };
        response.validate("request-1").unwrap();
        assert_eq!(response.content_length(12).unwrap(), Some(12));
        assert!(matches!(
            response.content_length(11),
            Err(ProtocolError::InvalidResponse)
        ));

        response.headers.push(ResponseHeader {
            name: "content-length".into(),
            value: "12".into(),
        });
        assert!(matches!(
            response.validate("request-1"),
            Err(ProtocolError::InvalidResponse)
        ));
        assert!(matches!(
            response.content_length(12),
            Err(ProtocolError::InvalidResponse)
        ));

        response.headers.truncate(1);
        response.headers[0].value = "not-a-length".into();
        assert!(matches!(
            response.validate("request-1"),
            Err(ProtocolError::InvalidResponse)
        ));

        response.headers[0].value = "+12".into();
        assert!(matches!(
            response.validate("request-1"),
            Err(ProtocolError::InvalidResponse)
        ));

        response.headers.clear();
        response.status = 101;
        assert!(matches!(
            response.validate("request-1"),
            Err(ProtocolError::InvalidResponse)
        ));
    }

    #[tokio::test]
    async fn head_and_chunk_framing_round_trip() {
        let (mut left, mut right) = tokio::io::duplex(2 * MAX_CHUNK_BYTES);
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: "request-1".into(),
            method: HttpMethod::Post,
            path: "/api/v1/commands/host".into(),
            content_type: Some("application/json".into()),
        };
        let body = vec![3u8; MAX_CHUNK_BYTES + 17];
        let expected_body = body.clone();
        let writer = tokio::spawn(async move {
            write_head(&mut left, &head).await.unwrap();
            write_body(&mut left, &body).await.unwrap();
        });
        let decoded: RequestHead = read_head(&mut right).await.unwrap();
        assert_eq!(
            decoded.validate().unwrap().unwrap().request_bytes,
            MAX_COMMAND_BYTES
        );
        assert_eq!(
            read_body(&mut right, MAX_COMMAND_BYTES).await.unwrap(),
            expected_body
        );
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn body_reader_rejects_aggregate_overflow() {
        let (mut left, mut right) = tokio::io::duplex(128);
        let writer = tokio::spawn(async move {
            write_chunk(&mut left, &[1, 2, 3, 4]).await.unwrap();
            finish_body(&mut left).await.unwrap();
        });
        assert!(matches!(
            read_body(&mut right, 3).await,
            Err(ProtocolError::AggregateTooLarge)
        ));
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn framed_message_requires_fin_without_trailing_bytes() {
        let (mut left, mut right) = tokio::io::duplex(128);
        let writer = tokio::spawn(async move {
            write_body(&mut left, b"complete").await.unwrap();
            left.write_all(b"trailing").await.unwrap();
            left.shutdown().await.unwrap();
        });
        assert_eq!(read_body(&mut right, 8).await.unwrap(), b"complete");
        assert!(matches!(
            expect_end(&mut right).await,
            Err(ProtocolError::TrailingData)
        ));
        writer.await.unwrap();
    }

    #[test]
    fn strict_ticket_decoder_rejects_unknown_fields() {
        let payload = format!(
            "{{\"protocol\":1,\"hostId\":\"host-a\",\"hostEndpointId\":\"endpoint-a\",\"relayUrls\":[\"https://relay.example\"],\"directAddresses\":[],\"invitation\":\"{}\",\"expiresAtMs\":1,\"extra\":true}}",
            secret(1).encode().as_str()
        );
        let encoded = format!(
            "ygg://pair/v1/{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes())
        );
        assert!(matches!(
            PairingTicket::decode(&encoded),
            Err(ProtocolError::InvalidJson)
        ));
    }
}
