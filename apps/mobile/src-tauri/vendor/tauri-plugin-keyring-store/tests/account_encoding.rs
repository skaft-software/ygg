//! Deterministic account key helpers (no OS keyring access).

use tauri_plugin_keyring_store::{BytesDto, KeyringStore};

#[test]
fn store_and_vault_keys_stable_for_same_inputs() {
    let s = KeyringStore::new("com.example.app");
    let client = BytesDto::Text("client-a".into());
    let vault = BytesDto::Text("vault-x".into());
    let record = BytesDto::Raw(vec![1, 2, 3]);

    assert_eq!(
        s.account_store_key("/snapshot/path", &client, "store-key"),
        s.account_store_key("/snapshot/path", &client, "store-key")
    );

    assert_eq!(
        s.account_vault_record("/snapshot/path", &client, &vault, &record),
        s.account_vault_record("/snapshot/path", &client, &vault, &record)
    );

    let counter_record = BytesDto::Text("7".into());
    let g = s.account_vault_record("/snap", &client, &vault, &record);
    let c = s.account_vault_record("/snap", &client, &vault, &counter_record);
    assert_ne!(g, c, "generic record vs counter-as-text must differ");

    let raw = BytesDto::Raw(vec![0xff; 400]);
    let long_key = s.account_store_key("/long-snapshot-path/", &client, "k");
    let long_vault = s.account_vault_record("/long-snapshot-path/", &client, &vault, &raw);
    assert!(!long_key.is_empty());
    assert!(!long_vault.is_empty());
}
