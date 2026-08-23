use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;
use ygg_companion_protocol::{
    DevicePlatform, PairingDeviceClaim, PairingOperation, PairingReply, PairingRequest,
    PairingStatusRequest, PairingTicket,
};
use zeroize::Zeroizing;

use crate::client::{ClientError, RemoteClient};
use crate::credentials::{
    delete_pairing_proof, load_pairing_proof, store_pairing_proof, CredentialError, EndpointKey,
    SharedCredentials,
};
use crate::profile::{HostProfile, HostTarget, PendingPairing, ProfileError, ProfileStore};

const MAX_DEVICE_NAME_BYTES: usize = 128;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CoreError {
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Profile(#[from] ProfileError),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the companion pairing ticket is invalid")]
    InvalidTicket,
    #[error("the requested companion transition is unavailable")]
    Conflict,
    #[error("paired access can only be removed from native app settings")]
    PairedRemovalRequiresSettings,
    #[error("the host returned inconsistent pairing state")]
    InvalidPairing,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicNativeState {
    phase: &'static str,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    phrase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expires_at_ms: Option<u64>,
}

impl PublicNativeState {
    fn unpaired() -> Self {
        Self {
            phase: "unpaired",
            message: "Import the short-lived invitation shown on your Ygg host.",
            phrase: None,
            expires_at_ms: None,
        }
    }
}

enum RuntimeState {
    Unpaired,
    Pending {
        pending: PendingPairing,
        proof: PairingStatusRequest,
    },
    Paired(HostProfile),
    Denied,
    Expired,
    Revoked,
    RestartRequired,
}

pub(crate) struct NativeCore {
    credentials: SharedCredentials,
    key: EndpointKey,
    profiles: ProfileStore,
    remote: RemoteClient,
    state: Mutex<RuntimeState>,
    operation: Mutex<()>,
}

impl NativeCore {
    pub(crate) async fn load(
        credentials: SharedCredentials,
        profiles: ProfileStore,
    ) -> Result<Arc<Self>, CoreError> {
        recover_access_removal(credentials.as_ref(), &profiles)?;
        let profile = profiles.load_profile()?;
        let pending = profiles.load_pending()?;
        let proof = load_pairing_proof(credentials.as_ref())?;
        let key = match EndpointKey::load(credentials.as_ref())? {
            Some(key) => key,
            None if profile.is_some() || pending.is_some() || proof.is_some() => {
                return Err(CredentialError::Invalid.into());
            }
            None => EndpointKey::create(credentials.as_ref())?,
        };

        let state = match (pending, proof, profile) {
            (Some(pending), Some(proof), staged_profile) => {
                if pending.request_id != proof.request_id {
                    return Err(CoreError::InvalidPairing);
                }
                if let Some(profile) = &staged_profile {
                    validate_key_profile(&key, profile)?;
                    if profile.target.host_endpoint_id != pending.target.host_endpoint_id {
                        return Err(CoreError::InvalidPairing);
                    }
                }
                RuntimeState::Pending { pending, proof }
            }
            (Some(_), None, Some(_)) => {
                // A missing proof cannot activate a device. Discard staged public
                // metadata rather than treating an unacknowledged profile as paired.
                profiles.remove_profile()?;
                profiles.remove_pending()?;
                RuntimeState::Unpaired
            }
            (Some(_), None, None) => {
                profiles.remove_pending()?;
                RuntimeState::Unpaired
            }
            (None, Some(_), Some(profile)) => {
                // Activation is acknowledged before pending cleanup starts, so this is
                // an interrupted cleanup rather than a new source of authority.
                validate_key_profile(&key, &profile)?;
                delete_pairing_proof(credentials.as_ref())?;
                RuntimeState::Paired(profile)
            }
            (None, Some(_), None) => {
                delete_pairing_proof(credentials.as_ref())?;
                RuntimeState::Unpaired
            }
            (None, None, Some(profile)) => {
                validate_key_profile(&key, &profile)?;
                RuntimeState::Paired(profile)
            }
            (None, None, None) => RuntimeState::Unpaired,
        };
        let remote = RemoteClient::start(&key).await?;
        Ok(Arc::new(Self {
            credentials,
            key,
            profiles,
            remote,
            state: Mutex::new(state),
            operation: Mutex::new(()),
        }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        credentials: SharedCredentials,
        key: EndpointKey,
        profiles: ProfileStore,
        remote: RemoteClient,
        profile: Option<HostProfile>,
    ) -> Result<Arc<Self>, CoreError> {
        let state = match profile {
            Some(profile) => {
                validate_key_profile(&key, &profile)?;
                RuntimeState::Paired(profile)
            }
            None => RuntimeState::Unpaired,
        };
        Ok(Arc::new(Self {
            credentials,
            key,
            profiles,
            remote,
            state: Mutex::new(state),
            operation: Mutex::new(()),
        }))
    }

    pub(crate) async fn public_state(&self) -> PublicNativeState {
        match &*self.state.lock().await {
            RuntimeState::Unpaired => PublicNativeState::unpaired(),
            RuntimeState::Pending { pending, .. } => PublicNativeState {
                phase: "pending",
                message: "Compare this phrase with the host, then approve this device there.",
                phrase: Some(pending.phrase.clone()),
                expires_at_ms: Some(pending.expires_at_ms),
            },
            RuntimeState::Paired(_) => PublicNativeState {
                phase: "paired",
                message: "This device is paired.",
                phrase: None,
                expires_at_ms: None,
            },
            RuntimeState::Denied => PublicNativeState {
                phase: "denied",
                message: "The host owner denied this pairing request.",
                phrase: None,
                expires_at_ms: None,
            },
            RuntimeState::Expired => PublicNativeState {
                phase: "expired",
                message: "The pairing invitation expired. Open a new invitation on the host.",
                phrase: None,
                expires_at_ms: None,
            },
            RuntimeState::Revoked => PublicNativeState {
                phase: "revoked",
                message: "The host owner revoked this device. Restart the app, then re-pair with a new invitation.",
                phrase: None,
                expires_at_ms: None,
            },
            RuntimeState::RestartRequired => PublicNativeState {
                phase: "restartRequired",
                message: "Local companion access was removed. Restart the app before pairing again.",
                phrase: None,
                expires_at_ms: None,
            },
        }
    }

    pub(crate) async fn paired_profile(&self) -> Option<HostProfile> {
        match &*self.state.lock().await {
            RuntimeState::Paired(profile) => Some(profile.clone()),
            _ => None,
        }
    }

    pub(crate) fn remote(&self) -> &RemoteClient {
        &self.remote
    }

    pub(crate) async fn begin_pairing(
        &self,
        encoded_ticket: &str,
        device_name: &str,
    ) -> Result<PublicNativeState, CoreError> {
        let _operation = self.operation.lock().await;
        if !matches!(
            &*self.state.lock().await,
            RuntimeState::Unpaired | RuntimeState::Denied | RuntimeState::Expired
        ) {
            return Err(CoreError::Conflict);
        }
        if device_name.trim().is_empty()
            || device_name.len() > MAX_DEVICE_NAME_BYTES
            || device_name.chars().any(char::is_control)
        {
            return Err(CoreError::InvalidTicket);
        }
        let ticket = PairingTicket::decode(encoded_ticket).map_err(|_| CoreError::InvalidTicket)?;
        if ticket.expires_at_ms <= now_ms() {
            return Err(CoreError::InvalidTicket);
        }
        let client_nonce = random_nonce()?;
        let expected_phrase = ticket.verification_phrase(&self.key.public_id(), &client_nonce);
        let target = HostTarget::from_ticket(&ticket);
        target.validate()?;
        let device = PairingDeviceClaim {
            name: device_name.trim().to_owned(),
            platform: native_platform(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        device.validate().map_err(|_| CoreError::InvalidTicket)?;
        let request = PairingRequest {
            invitation: ticket.invitation,
            client_nonce,
            observed_host_id: target.host_id.clone(),
            observed_host_endpoint_id: target.host_endpoint_id.clone(),
            device: device.clone(),
        };
        let reply = self
            .remote
            .pair(&target, PairingOperation::Request(request))
            .await?;
        let (request_id, poll_token, phrase, expires_at_ms) = match reply {
            PairingReply::PendingRequest {
                request_id,
                poll_token,
                phrase,
                expires_at_ms,
            } => (request_id, poll_token, phrase, expires_at_ms),
            _ => return Err(CoreError::InvalidPairing),
        };
        if phrase != expected_phrase || expires_at_ms != ticket.expires_at_ms {
            return Err(CoreError::InvalidPairing);
        }
        let proof = PairingStatusRequest {
            request_id: request_id.clone(),
            poll_token,
        };
        let pending =
            PendingPairing::new(request_id, target.clone(), device, phrase, expires_at_ms)?;

        store_pairing_proof(self.credentials.as_ref(), &proof)?;
        if let Err(error) = self.profiles.store_pending(&pending) {
            let _ = delete_pairing_proof(self.credentials.as_ref());
            let _ = self
                .remote
                .pair(&target, PairingOperation::Cancel(proof))
                .await;
            return Err(error.into());
        }
        *self.state.lock().await = RuntimeState::Pending { pending, proof };
        Ok(self.public_state().await)
    }

    pub(crate) async fn poll_pairing(&self) -> Result<PublicNativeState, CoreError> {
        let _operation = self.operation.lock().await;
        let pair = {
            let state = self.state.lock().await;
            match &*state {
                RuntimeState::Pending { pending, proof } => Some((pending.clone(), proof.clone())),
                _ => None,
            }
        };
        let Some((pending, proof)) = pair else {
            return Ok(self.public_state().await);
        };
        // Even after the invitation deadline, ask the authenticated host before
        // discarding recovery metadata. The host may have durably committed an
        // acknowledgement whose responses were lost; registered endpoints can
        // recover that authority independently of the expired in-memory request.
        let reply = self
            .remote
            .pair(&pending.target, PairingOperation::Status(proof.clone()))
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(ClientError::Rejected) if pending.expires_at_ms <= now_ms() => {
                self.finish_terminal_pairing(RuntimeState::Expired).await?;
                return Ok(self.public_state().await);
            }
            Err(error) => return Err(error.into()),
        };
        match reply {
            PairingReply::Pending {
                phrase,
                expires_at_ms,
            } => {
                if phrase != pending.phrase || expires_at_ms != pending.expires_at_ms {
                    return Err(CoreError::InvalidPairing);
                }
            }
            PairingReply::Approved { device } => {
                self.activate(pending, proof, device, true).await?;
            }
            PairingReply::Acknowledged { device } => {
                self.activate(pending, proof, device, false).await?;
            }
            PairingReply::Denied => {
                self.finish_terminal_pairing(RuntimeState::Denied).await?;
            }
            PairingReply::Expired | PairingReply::Cancelled => {
                self.finish_terminal_pairing(RuntimeState::Expired).await?;
            }
            PairingReply::PendingRequest { .. } => return Err(CoreError::InvalidPairing),
        }
        Ok(self.public_state().await)
    }

    pub(crate) async fn cancel_pairing(&self) -> Result<PublicNativeState, CoreError> {
        let _operation = self.operation.lock().await;
        let pair = match &*self.state.lock().await {
            RuntimeState::Pending { pending, proof } => (pending.clone(), proof.clone()),
            _ => return Err(CoreError::Conflict),
        };
        let (pending, proof) = pair;
        let reply = self
            .remote
            .pair(&pending.target, PairingOperation::Cancel(proof.clone()))
            .await;
        match reply {
            Ok(PairingReply::Cancelled | PairingReply::Denied | PairingReply::Expired)
            | Err(ClientError::Rejected) => {
                self.profiles.remove_profile()?;
                self.clear_pending_storage()?;
                *self.state.lock().await = RuntimeState::Unpaired;
            }
            Ok(PairingReply::Acknowledged { device }) => {
                // The acknowledgement may have committed even when its response was
                // lost. Preserve that durable authority instead of silently
                // orphaning a host-side device that this endpoint can no longer use.
                self.activate(pending, proof, device, false).await?;
            }
            Ok(_) => return Err(CoreError::InvalidPairing),
            Err(error) => return Err(error.into()),
        }
        Ok(self.public_state().await)
    }

    pub(crate) async fn remove_access_from_settings(&self) -> Result<PublicNativeState, CoreError> {
        let _operation = self.operation.lock().await;
        self.remove_access_locked().await
    }

    pub(crate) async fn remove_unpaired_access(&self) -> Result<PublicNativeState, CoreError> {
        let _operation = self.operation.lock().await;
        let paired = matches!(&*self.state.lock().await, RuntimeState::Paired(_));
        if paired || self.profiles.load_profile()?.is_some() {
            return Err(CoreError::PairedRemovalRequiresSettings);
        }
        self.remove_access_locked().await
    }

    async fn remove_access_locked(&self) -> Result<PublicNativeState, CoreError> {
        self.profiles.begin_access_removal()?;
        *self.state.lock().await = RuntimeState::RestartRequired;
        self.remote.close().await;
        complete_access_removal(self.credentials.as_ref(), &self.profiles)?;
        Ok(self.public_state().await)
    }

    pub(crate) async fn mark_revoked(&self) {
        let _operation = self.operation.lock().await;
        *self.state.lock().await = RuntimeState::Revoked;
        self.remote.close().await;
        if self.profiles.begin_access_removal().is_ok() {
            let _ = complete_access_removal(self.credentials.as_ref(), &self.profiles);
        } else {
            let _ = self.profiles.remove_profile();
            let _ = self.profiles.remove_pending();
            let _ = delete_pairing_proof(self.credentials.as_ref());
            let _ = EndpointKey::delete(self.credentials.as_ref());
        }
    }

    async fn activate(
        &self,
        pending: PendingPairing,
        proof: PairingStatusRequest,
        device: ygg_companion_protocol::DeviceSummary,
        send_ack: bool,
    ) -> Result<(), CoreError> {
        device.validate().map_err(|_| CoreError::InvalidPairing)?;
        if device.name != pending.device.name
            || device.platform != pending.device.platform
            || device.revoked_at_ms.is_some()
        {
            return Err(CoreError::InvalidPairing);
        }
        let profile =
            HostProfile::from_approval(pending.target.clone(), self.key.public_id(), &device)?;
        self.profiles.store_profile(&profile)?;
        if send_ack {
            let acknowledged = self
                .remote
                .pair(&pending.target, PairingOperation::Ack(proof))
                .await?;
            let PairingReply::Acknowledged {
                device: acknowledged_device,
            } = acknowledged
            else {
                return Err(CoreError::InvalidPairing);
            };
            acknowledged_device
                .validate()
                .map_err(|_| CoreError::InvalidPairing)?;
            if acknowledged_device.id != device.id
                || acknowledged_device.name != device.name
                || acknowledged_device.platform != device.platform
                || acknowledged_device.paired_at_ms != device.paired_at_ms
                || acknowledged_device.revoked_at_ms.is_some()
            {
                return Err(CoreError::InvalidPairing);
            }
        }
        self.remote.invalidate_connection().await;
        self.clear_pending_storage()?;
        *self.state.lock().await = RuntimeState::Paired(profile);
        Ok(())
    }

    async fn finish_terminal_pairing(&self, state: RuntimeState) -> Result<(), CoreError> {
        self.profiles.remove_profile()?;
        self.clear_pending_storage()?;
        *self.state.lock().await = state;
        Ok(())
    }

    fn clear_pending_storage(&self) -> Result<(), CoreError> {
        self.profiles.remove_pending()?;
        delete_pairing_proof(self.credentials.as_ref())?;
        Ok(())
    }
}

fn recover_access_removal(
    credentials: &dyn crate::credentials::CredentialStore,
    profiles: &ProfileStore,
) -> Result<(), CoreError> {
    if profiles.access_removal_pending()? {
        complete_access_removal(credentials, profiles)?;
    }
    Ok(())
}

fn complete_access_removal(
    credentials: &dyn crate::credentials::CredentialStore,
    profiles: &ProfileStore,
) -> Result<(), CoreError> {
    profiles.remove_profile()?;
    profiles.remove_pending()?;
    delete_pairing_proof(credentials)?;
    EndpointKey::delete(credentials)?;
    profiles.finish_access_removal()?;
    Ok(())
}

fn validate_key_profile(key: &EndpointKey, profile: &HostProfile) -> Result<(), CoreError> {
    profile.validate()?;
    if profile.device_endpoint_id != key.public_id() {
        return Err(CoreError::InvalidPairing);
    }
    Ok(())
}

fn native_platform() -> DevicePlatform {
    #[cfg(target_os = "ios")]
    return DevicePlatform::Ios;
    #[cfg(target_os = "android")]
    return DevicePlatform::Android;
    #[cfg(target_os = "macos")]
    return DevicePlatform::Macos;
    #[allow(unreachable_code)]
    DevicePlatform::Other
}

fn random_nonce() -> Result<String, CoreError> {
    let mut bytes = Zeroizing::new([0u8; 16]);
    getrandom::fill(bytes.as_mut()).map_err(|_| CredentialError::Unavailable)?;
    let mut nonce = String::with_capacity(38);
    nonce.push_str("pair-native-");
    for byte in bytes.iter() {
        use std::fmt::Write as _;
        write!(nonce, "{byte:02x}").map_err(|_| CredentialError::Unavailable)?;
    }
    Ok(nonce)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::tests::MemoryCredentials;
    use tempfile::TempDir;
    use ygg_companion_protocol::DeviceSummary;

    fn target() -> HostTarget {
        HostTarget {
            host_id: "host-test".into(),
            host_endpoint_id: "host-endpoint-test".into(),
            relay_urls: vec!["https://relay.example".into()],
            direct_addresses: vec!["127.0.0.1:1234".into()],
        }
    }

    async fn unpaired_core() -> (TempDir, ProfileStore, SharedCredentials, Arc<NativeCore>) {
        let temp = TempDir::new().unwrap();
        let profiles = ProfileStore::open(temp.path().join("state")).unwrap();
        let credentials: SharedCredentials = Arc::new(MemoryCredentials::default());
        let endpoint_key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
        let endpoint = iroh::Endpoint::empty_builder(iroh::RelayMode::Disabled)
            .secret_key(endpoint_key.clone_for_endpoint())
            .bind()
            .await
            .unwrap();
        let core = NativeCore::for_test(
            credentials.clone(),
            endpoint_key,
            profiles.clone(),
            RemoteClient::for_test(endpoint),
            None,
        )
        .unwrap();
        (temp, profiles, credentials, core)
    }

    fn pending_pairing(request_id: &str, token_byte: u8) -> (PendingPairing, PairingStatusRequest) {
        let proof = PairingStatusRequest {
            request_id: request_id.to_owned(),
            poll_token: ygg_companion_protocol::Secret32::from_bytes([token_byte; 32]),
        };
        let pending = PendingPairing::new(
            proof.request_id.clone(),
            target(),
            PairingDeviceClaim {
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                app_version: "test".to_owned(),
            },
            "amber · birch · cairn · delta · ember · fern".to_owned(),
            10_000,
        )
        .unwrap();
        (pending, proof)
    }

    #[tokio::test]
    async fn startup_never_replaces_a_missing_authoritative_endpoint_key() {
        let temp = TempDir::new().unwrap();
        let profiles = ProfileStore::open(temp.path().join("state")).unwrap();
        let credentials = Arc::new(MemoryCredentials::default());
        let old_key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
        let profile = HostProfile::from_approval(
            target(),
            old_key.public_id(),
            &DeviceSummary {
                id: "device-one".into(),
                name: "Phone".into(),
                platform: DevicePlatform::Ios,
                paired_at_ms: 1,
                last_seen_at_ms: None,
                revoked_at_ms: None,
                connected: false,
            },
        )
        .unwrap();
        profiles.store_profile(&profile).unwrap();
        EndpointKey::delete(credentials.as_ref()).unwrap();

        let shared: SharedCredentials = credentials.clone();
        assert!(matches!(
            NativeCore::load(shared, profiles).await,
            Err(CoreError::Credential(CredentialError::Invalid))
        ));
        assert!(EndpointKey::load(credentials.as_ref()).unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_after_host_commit_activates_acknowledged_authority() {
        let server = iroh::Endpoint::empty_builder(iroh::RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let target = HostTarget {
            host_id: "host-1".to_owned(),
            host_endpoint_id: server.id().to_string(),
            relay_urls: vec![iroh::defaults::prod::default_na_east_relay()
                .url
                .to_string()],
            direct_addresses: server.addr().ip_addrs().map(ToString::to_string).collect(),
        };
        assert!(!target.direct_addresses.is_empty());
        let client_endpoint = iroh::Endpoint::empty_builder(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: ygg_companion_protocol::RequestHead =
                    ygg_companion_protocol::read_head(&mut recv).await.unwrap();
                assert!(matches!(
                    &head,
                    ygg_companion_protocol::RequestHead::Pairing {
                        operation: ygg_companion_protocol::PairingOperation::Cancel(proof),
                        ..
                    } if proof.request_id == "pair-one"
                ));
                ygg_companion_protocol::expect_end(&mut recv).await.unwrap();
                let body = serde_json::to_vec(&PairingReply::Acknowledged {
                    device: DeviceSummary {
                        id: "device-one".to_owned(),
                        name: "Phone".to_owned(),
                        platform: DevicePlatform::Ios,
                        paired_at_ms: 1,
                        last_seen_at_ms: None,
                        revoked_at_ms: None,
                        connected: false,
                    },
                })
                .unwrap();
                ygg_companion_protocol::write_head(
                    &mut send,
                    &ygg_companion_protocol::ResponseHead {
                        protocol: ygg_companion_protocol::PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ygg_companion_protocol::ResponseHeader {
                            name: "content-length".to_owned(),
                            value: body.len().to_string(),
                        }],
                    },
                )
                .await
                .unwrap();
                ygg_companion_protocol::write_body(&mut send, &body)
                    .await
                    .unwrap();
                send.finish().unwrap();
                connection
            }
        });

        let credentials: SharedCredentials = Arc::new(MemoryCredentials::default());
        let endpoint_key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
        let temp = TempDir::new().unwrap();
        let profiles = ProfileStore::open(temp.path().to_path_buf()).unwrap();
        let proof = PairingStatusRequest {
            request_id: "pair-one".to_owned(),
            poll_token: ygg_companion_protocol::Secret32::from_bytes([7; 32]),
        };
        store_pairing_proof(credentials.as_ref(), &proof).unwrap();
        let pending = PendingPairing::new(
            proof.request_id.clone(),
            target,
            PairingDeviceClaim {
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                app_version: "test".to_owned(),
            },
            "amber · birch · cairn · delta · ember · fern".to_owned(),
            10_000,
        )
        .unwrap();
        profiles.store_pending(&pending).unwrap();
        let core = NativeCore::for_test(
            credentials.clone(),
            endpoint_key,
            profiles.clone(),
            RemoteClient::for_test(client_endpoint),
            None,
        )
        .unwrap();
        *core.state.lock().await = RuntimeState::Pending { pending, proof };

        let state = core.cancel_pairing().await.unwrap();
        assert_eq!(state.phase, "paired");
        assert!(profiles.load_pending().unwrap().is_none());
        assert!(load_pairing_proof(credentials.as_ref()).unwrap().is_none());
        assert_eq!(
            profiles.load_profile().unwrap().unwrap().device_id,
            "device-one"
        );

        server_task.await.unwrap();
        core.remote().close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn expired_pending_recovers_a_durable_host_acknowledgement() {
        let server = iroh::Endpoint::empty_builder(iroh::RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let target = HostTarget {
            host_id: "host-1".to_owned(),
            host_endpoint_id: server.id().to_string(),
            relay_urls: vec![iroh::defaults::prod::default_na_east_relay()
                .url
                .to_string()],
            direct_addresses: server.addr().ip_addrs().map(ToString::to_string).collect(),
        };
        assert!(!target.direct_addresses.is_empty());
        let client_endpoint = iroh::Endpoint::empty_builder(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: ygg_companion_protocol::RequestHead =
                    ygg_companion_protocol::read_head(&mut recv).await.unwrap();
                assert!(matches!(
                    &head,
                    ygg_companion_protocol::RequestHead::Pairing {
                        operation: ygg_companion_protocol::PairingOperation::Status(proof),
                        ..
                    } if proof.request_id == "pair-one"
                ));
                ygg_companion_protocol::expect_end(&mut recv).await.unwrap();
                let body = serde_json::to_vec(&PairingReply::Acknowledged {
                    device: DeviceSummary {
                        id: "device-one".to_owned(),
                        name: "Phone".to_owned(),
                        platform: DevicePlatform::Ios,
                        paired_at_ms: 1,
                        last_seen_at_ms: None,
                        revoked_at_ms: None,
                        connected: false,
                    },
                })
                .unwrap();
                ygg_companion_protocol::write_head(
                    &mut send,
                    &ygg_companion_protocol::ResponseHead {
                        protocol: ygg_companion_protocol::PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ygg_companion_protocol::ResponseHeader {
                            name: "content-length".to_owned(),
                            value: body.len().to_string(),
                        }],
                    },
                )
                .await
                .unwrap();
                ygg_companion_protocol::write_body(&mut send, &body)
                    .await
                    .unwrap();
                send.finish().unwrap();
                connection
            }
        });

        let credentials: SharedCredentials = Arc::new(MemoryCredentials::default());
        let endpoint_key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
        let temp = TempDir::new().unwrap();
        let profiles = ProfileStore::open(temp.path().to_path_buf()).unwrap();
        let proof = PairingStatusRequest {
            request_id: "pair-one".to_owned(),
            poll_token: ygg_companion_protocol::Secret32::from_bytes([7; 32]),
        };
        store_pairing_proof(credentials.as_ref(), &proof).unwrap();
        let pending = PendingPairing::new(
            proof.request_id.clone(),
            target.clone(),
            PairingDeviceClaim {
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                app_version: "test".to_owned(),
            },
            "amber · birch · cairn · delta · ember · fern".to_owned(),
            now_ms().saturating_sub(1).max(1),
        )
        .unwrap();
        profiles.store_pending(&pending).unwrap();
        profiles
            .store_profile(
                &HostProfile::from_approval(
                    target,
                    endpoint_key.public_id(),
                    &DeviceSummary {
                        id: "device-one".to_owned(),
                        name: "Phone".to_owned(),
                        platform: DevicePlatform::Ios,
                        paired_at_ms: 1,
                        last_seen_at_ms: None,
                        revoked_at_ms: None,
                        connected: false,
                    },
                )
                .unwrap(),
            )
            .unwrap();
        let core = NativeCore::for_test(
            credentials.clone(),
            endpoint_key,
            profiles.clone(),
            RemoteClient::for_test(client_endpoint),
            None,
        )
        .unwrap();
        *core.state.lock().await = RuntimeState::Pending { pending, proof };

        let state = core.poll_pairing().await.unwrap();
        assert_eq!(state.phase, "paired");
        assert!(profiles.load_pending().unwrap().is_none());
        assert!(load_pairing_proof(credentials.as_ref()).unwrap().is_none());
        assert_eq!(
            profiles.load_profile().unwrap().unwrap().device_id,
            "device-one"
        );

        server_task.await.unwrap();
        core.remote().close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn onboarding_removal_rechecks_pairing_after_waiting_for_activation() {
        let (_temp, profiles, credentials, core) = unpaired_core().await;
        let (pending, proof) = pending_pairing("pair-race", 9);
        store_pairing_proof(credentials.as_ref(), &proof).unwrap();
        profiles.store_pending(&pending).unwrap();
        *core.state.lock().await = RuntimeState::Pending {
            pending: pending.clone(),
            proof: proof.clone(),
        };

        // Model poll_pairing holding the operation lock while the host's durable
        // acknowledgement activates local authority. An onboarding-origin removal
        // that already started must check the resulting state only after this lock.
        let pairing_operation = core.operation.lock().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let removal = tokio::spawn({
            let core = Arc::clone(&core);
            async move {
                started_tx.send(()).unwrap();
                core.remove_unpaired_access().await
            }
        });
        started_rx.await.unwrap();
        core.activate(
            pending,
            proof,
            DeviceSummary {
                id: "device-one".to_owned(),
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                paired_at_ms: 1,
                last_seen_at_ms: None,
                revoked_at_ms: None,
                connected: false,
            },
            false,
        )
        .await
        .unwrap();
        drop(pairing_operation);

        assert!(matches!(
            removal.await.unwrap(),
            Err(CoreError::PairedRemovalRequiresSettings)
        ));
        assert_eq!(core.public_state().await.phase, "paired");
        assert_eq!(
            profiles.load_profile().unwrap().unwrap().device_id,
            "device-one"
        );
        assert!(EndpointKey::load(credentials.as_ref()).unwrap().is_some());
        assert!(!profiles.access_removal_pending().unwrap());
        core.remote().close().await;
    }

    #[tokio::test]
    async fn onboarding_removal_preserves_staged_acknowledgement_recovery() {
        let (_temp, profiles, credentials, core) = unpaired_core().await;
        let (pending, proof) = pending_pairing("pair-staged", 10);
        store_pairing_proof(credentials.as_ref(), &proof).unwrap();
        profiles.store_pending(&pending).unwrap();
        *core.state.lock().await = RuntimeState::Pending {
            pending: pending.clone(),
            proof,
        };

        let pairing_operation = core.operation.lock().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let removal = tokio::spawn({
            let core = Arc::clone(&core);
            async move {
                started_tx.send(()).unwrap();
                core.remove_unpaired_access().await
            }
        });
        started_rx.await.unwrap();

        // activate() durably stages this profile before asking the host to
        // commit its acknowledgement. If that response or local cleanup fails,
        // onboarding must preserve the credentials needed to recover the commit.
        let profile = HostProfile::from_approval(
            pending.target,
            core.key.public_id(),
            &DeviceSummary {
                id: "device-staged".to_owned(),
                name: "Phone".to_owned(),
                platform: DevicePlatform::Ios,
                paired_at_ms: 1,
                last_seen_at_ms: None,
                revoked_at_ms: None,
                connected: false,
            },
        )
        .unwrap();
        profiles.store_profile(&profile).unwrap();
        drop(pairing_operation);

        assert!(matches!(
            removal.await.unwrap(),
            Err(CoreError::PairedRemovalRequiresSettings)
        ));
        assert_eq!(core.public_state().await.phase, "pending");
        assert_eq!(
            profiles.load_profile().unwrap().unwrap().device_id,
            "device-staged"
        );
        assert!(EndpointKey::load(credentials.as_ref()).unwrap().is_some());
        assert!(!profiles.access_removal_pending().unwrap());
        core.remote().close().await;
    }

    #[test]
    fn startup_finishes_interrupted_access_removal_before_loading_authority() {
        let temp = TempDir::new().unwrap();
        let profiles = ProfileStore::open(temp.path().join("state")).unwrap();
        let credentials = MemoryCredentials::default();
        let old_key = EndpointKey::load_or_create(&credentials).unwrap();
        let old_id = old_key.public_id();
        let profile = HostProfile::from_approval(
            target(),
            old_id.clone(),
            &DeviceSummary {
                id: "device-one".into(),
                name: "Phone".into(),
                platform: DevicePlatform::Ios,
                paired_at_ms: 1,
                last_seen_at_ms: None,
                revoked_at_ms: None,
                connected: false,
            },
        )
        .unwrap();
        profiles.store_profile(&profile).unwrap();
        profiles.begin_access_removal().unwrap();
        drop(old_key);

        recover_access_removal(&credentials, &profiles).unwrap();

        assert!(profiles.load_profile().unwrap().is_none());
        assert!(!profiles.access_removal_pending().unwrap());
        let replacement = EndpointKey::load_or_create(&credentials).unwrap();
        assert_ne!(replacement.public_id(), old_id);
    }
}
