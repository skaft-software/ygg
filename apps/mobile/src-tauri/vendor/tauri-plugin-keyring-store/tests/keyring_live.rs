//! Live OS keyring integration. Run locally only:
//!
//! ```text
//! cargo test keyring_roundtrip_live -- --ignored --nocapture
//! ```

use tauri_plugin_keyring_store::KeyringStore;

#[test]
#[ignore = "requires desktop session keyring (Secret Service / Keychain / Credential Manager)"]
fn keyring_roundtrip_live() {
    let store = KeyringStore::new("com.tauri.plugin.keyring.integration-test");
    let account = "integration.test.ephemeral";
    store.delete(account).ok();
    store.set_password(account, "secret-value").expect("set");
    let v = store.get_password(account).expect("get").expect("some");
    assert_eq!(v, "secret-value");
    store.delete(account).expect("delete");
}
