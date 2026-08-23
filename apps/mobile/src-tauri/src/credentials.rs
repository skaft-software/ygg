use std::fmt;
use std::sync::Arc;
#[cfg(target_os = "android")]
use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, OnceLock},
    thread,
    time::{Duration, Instant},
};

use iroh::SecretKey;
#[cfg(target_os = "android")]
use tauri::tao::platform::android::prelude::{main_android_context, GlobalRef};
use tauri_plugin_keyring_store::KeyringStore;
use ygg_companion_protocol::{PairingStatusRequest, MAX_HEAD_BYTES};
use zeroize::Zeroizing;

const ENDPOINT_KEY_ACCOUNT: &str = "companion-endpoint-key-v1";
const PAIRING_PROOF_ACCOUNT: &str = "companion-pairing-proof-v1";

#[cfg(target_os = "android")]
const ANDROID_CONTEXT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "android")]
const ANDROID_CONTEXT_RETRY_INTERVAL: Duration = Duration::from_millis(10);
#[cfg(target_os = "android")]
static ANDROID_APPLICATION_CONTEXT: OnceLock<GlobalRef> = OnceLock::new();
#[cfg(target_os = "android")]
static ANDROID_CONTEXT_INIT: OnceLock<Result<(), &'static str>> = OnceLock::new();

#[cfg(target_os = "android")]
pub(crate) fn initialize_android_keyring_context() -> Result<(), CredentialError> {
    ANDROID_CONTEXT_INIT
        .get_or_init(initialize_android_keyring_context_once)
        .as_ref()
        .map(|_| ())
        .map_err(|_| CredentialError::Unavailable)
}

#[cfg(target_os = "android")]
fn initialize_android_keyring_context_once() -> Result<(), &'static str> {
    let deadline = Instant::now() + ANDROID_CONTEXT_TIMEOUT;
    let (java_vm, application) = loop {
        // WryActivity calls Rust.create() before Rust.wryCreate(). Tao's
        // onActivityCreate hook installs the activity context/proxy first, and
        // Rust.create() returns once the Rust looper is ready rather than once
        // app setup finishes. Setup can therefore enqueue this dispatch before
        // wryCreate() installs MAIN_PIPE's Java-looper callback; the callback
        // drains it after Rust.create() returns. Bound both readiness and that
        // queue handoff, and fail closed if either lifecycle step never occurs.
        if main_android_context().is_none() {
            wait_for_android_context_retry(deadline)?;
            continue;
        }

        let (sender, receiver) = mpsc::sync_channel(1);
        let dispatched = catch_unwind(AssertUnwindSafe(|| {
            tauri::wry::android::dispatch(move |env, activity, _webview| {
                let context = (|| -> Result<(usize, GlobalRef), &'static str> {
                    let application = env
                        .call_method(
                            activity,
                            "getApplicationContext",
                            "()Landroid/content/Context;",
                            &[],
                        )
                        .map_err(|_| "failed to obtain the Android application context")?
                        .l()
                        .map_err(|_| "Android returned an invalid application context")?;
                    if application.is_null() {
                        return Err("Android returned a null application context");
                    }
                    let application = env
                        .new_global_ref(application)
                        .map_err(|_| "failed to retain the Android application context")?;
                    let java_vm = env
                        .get_java_vm()
                        .map_err(|_| "failed to obtain the Android Java VM")?
                        .get_java_vm_pointer();
                    if java_vm.is_null() {
                        return Err("Android returned a null Java VM");
                    }
                    Ok((java_vm as usize, application))
                })();
                let _ = sender.send(context);
            });
        }));
        if dispatched.is_err() {
            wait_for_android_context_retry(deadline)?;
            continue;
        }

        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("timed out initializing the Android keyring context")?;
        break receiver
            .recv_timeout(remaining)
            .map_err(|_| "timed out initializing the Android keyring context")??;
    };
    let application_pointer = application.as_obj().as_raw();
    if application_pointer.is_null() {
        return Err("Android returned a null retained application context");
    }
    ANDROID_APPLICATION_CONTEXT
        .set(application)
        .map_err(|_| "Android keyring context was initialized more than once")?;

    // SAFETY: Tauri/Wry supplied the live process Java VM. The jobject is a
    // GlobalRef to the process application context retained in the OnceLock for
    // the rest of the process. This initializer is serialized and cached by
    // ANDROID_CONTEXT_INIT, so ndk-context is initialized exactly once before
    // any KeyringStore operation.
    unsafe {
        ndk_context::initialize_android_context(java_vm as *mut c_void, application_pointer.cast());
    }
    Ok(())
}

#[cfg(target_os = "android")]
fn wait_for_android_context_retry(deadline: Instant) -> Result<(), &'static str> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or("timed out initializing the Android keyring context")?;
    thread::sleep(ANDROID_CONTEXT_RETRY_INTERVAL.min(remaining));
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CredentialError {
    #[error("protected credential storage is unavailable")]
    Unavailable,
    #[error("protected credential storage contains invalid companion data")]
    Invalid,
}

pub(crate) trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, CredentialError>;
    fn set(&self, account: &str, value: &[u8]) -> Result<(), CredentialError>;
    fn delete(&self, account: &str) -> Result<(), CredentialError>;
}

#[derive(Clone)]
pub(crate) struct KeyringCredentials {
    store: KeyringStore,
}

impl KeyringCredentials {
    pub(crate) fn new() -> Self {
        #[cfg(target_os = "ios")]
        let store = KeyringStore::new("org.skaft.ygg.companion.credentials")
            .with_write_accessibility(
                tauri_plugin_keyring_store::WriteAccessibility::WhenUnlockedThisDeviceOnly,
            );
        #[cfg(not(target_os = "ios"))]
        let store = KeyringStore::new("org.skaft.ygg.companion.credentials");
        Self { store }
    }
}

impl CredentialStore for KeyringCredentials {
    fn get(&self, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        self.store
            .get_bytes(account)
            .map_err(|_| CredentialError::Unavailable)
    }

    fn set(&self, account: &str, value: &[u8]) -> Result<(), CredentialError> {
        self.store
            .set_bytes(account, value)
            .map_err(|_| CredentialError::Unavailable)
    }

    fn delete(&self, account: &str) -> Result<(), CredentialError> {
        self.store
            .delete(account)
            .map_err(|_| CredentialError::Unavailable)
    }
}

pub(crate) struct EndpointKey(SecretKey);

impl EndpointKey {
    pub(crate) fn load(credentials: &dyn CredentialStore) -> Result<Option<Self>, CredentialError> {
        let Some(bytes) = credentials.get(ENDPOINT_KEY_ACCOUNT)? else {
            return Ok(None);
        };
        let bytes = Zeroizing::new(bytes);
        if bytes.len() != 32 {
            return Err(CredentialError::Invalid);
        }
        let mut key_bytes = Zeroizing::new([0u8; 32]);
        key_bytes.copy_from_slice(&bytes);
        Ok(Some(Self(SecretKey::from_bytes(&key_bytes))))
    }

    pub(crate) fn create(credentials: &dyn CredentialStore) -> Result<Self, CredentialError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut()).map_err(|_| CredentialError::Unavailable)?;
        let key = Self(SecretKey::from_bytes(&bytes));
        credentials.set(ENDPOINT_KEY_ACCOUNT, bytes.as_ref())?;
        Ok(key)
    }

    #[cfg(test)]
    pub(crate) fn load_or_create(
        credentials: &dyn CredentialStore,
    ) -> Result<Self, CredentialError> {
        match Self::load(credentials)? {
            Some(key) => Ok(key),
            None => Self::create(credentials),
        }
    }

    pub(crate) fn public_id(&self) -> String {
        self.0.public().to_string()
    }

    pub(crate) fn clone_for_endpoint(&self) -> SecretKey {
        self.0.clone()
    }

    pub(crate) fn delete(credentials: &dyn CredentialStore) -> Result<(), CredentialError> {
        credentials.delete(ENDPOINT_KEY_ACCOUNT)
    }
}

impl fmt::Debug for EndpointKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EndpointKey([REDACTED])")
    }
}

pub(crate) fn load_pairing_proof(
    credentials: &dyn CredentialStore,
) -> Result<Option<PairingStatusRequest>, CredentialError> {
    let Some(bytes) = credentials.get(PAIRING_PROOF_ACCOUNT)? else {
        return Ok(None);
    };
    let bytes = Zeroizing::new(bytes);
    if bytes.is_empty() || bytes.len() > MAX_HEAD_BYTES {
        return Err(CredentialError::Invalid);
    }
    let proof: PairingStatusRequest =
        serde_json::from_slice(&bytes).map_err(|_| CredentialError::Invalid)?;
    proof.validate().map_err(|_| CredentialError::Invalid)?;
    Ok(Some(proof))
}

pub(crate) fn store_pairing_proof(
    credentials: &dyn CredentialStore,
    proof: &PairingStatusRequest,
) -> Result<(), CredentialError> {
    proof.validate().map_err(|_| CredentialError::Invalid)?;
    let bytes = Zeroizing::new(serde_json::to_vec(proof).map_err(|_| CredentialError::Invalid)?);
    if bytes.is_empty() || bytes.len() > MAX_HEAD_BYTES {
        return Err(CredentialError::Invalid);
    }
    credentials.set(PAIRING_PROOF_ACCOUNT, &bytes)
}

pub(crate) fn delete_pairing_proof(
    credentials: &dyn CredentialStore,
) -> Result<(), CredentialError> {
    credentials.delete(PAIRING_PROOF_ACCOUNT)
}

pub(crate) type SharedCredentials = Arc<dyn CredentialStore>;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct MemoryCredentials {
        values: Mutex<HashMap<String, Vec<u8>>>,
        fail_writes: Mutex<bool>,
    }

    impl MemoryCredentials {
        pub(crate) fn fail_writes(&self) {
            *self.fail_writes.lock().unwrap() = true;
        }
    }

    impl CredentialStore for MemoryCredentials {
        fn get(&self, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(self.values.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, value: &[u8]) -> Result<(), CredentialError> {
            if *self.fail_writes.lock().unwrap() {
                return Err(CredentialError::Unavailable);
            }
            self.values
                .lock()
                .unwrap()
                .insert(account.to_owned(), value.to_vec());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), CredentialError> {
            self.values.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn endpoint_key_persists_and_debug_is_redacted() {
        let store = MemoryCredentials::default();
        let first = EndpointKey::load_or_create(&store).unwrap();
        let id = first.public_id();
        assert!(!format!("{first:?}").contains(&id));
        drop(first);
        assert_eq!(EndpointKey::load_or_create(&store).unwrap().public_id(), id);
    }

    #[test]
    fn endpoint_key_creation_fails_closed_when_storage_fails() {
        let store = MemoryCredentials::default();
        store.fail_writes();
        assert!(matches!(
            EndpointKey::load_or_create(&store),
            Err(CredentialError::Unavailable)
        ));
    }
}
