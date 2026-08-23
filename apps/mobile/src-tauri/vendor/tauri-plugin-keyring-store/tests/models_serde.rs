//! Serde round-trips for IPC DTOs (no keyring).

use serde_json::json;
use tauri_plugin_keyring_store::{
    BytesDto, DurationDto, Error, KeyType, LocationDto, PasswordBackupEncryptedDto,
    PasswordBackupEntryDto, PasswordBackupPlainDto, PasswordEntryDto, PingRequest, ProcedureDto,
    Slip10DeriveInputDto,
};

#[test]
fn error_keychain_locked_serializes_as_string() {
    let e = Error::KeychainLocked;
    let s = serde_json::to_string(&e).unwrap();
    assert_eq!(
        s,
        "\"keychain locked or protected data unavailable\""
    );
}

#[test]
fn bytes_dto_text_roundtrip() {
    let v = BytesDto::Text("hello".into());
    let s = serde_json::to_string(&v).unwrap();
    let back: BytesDto = serde_json::from_str(&s).unwrap();
    assert_eq!(back, v);
}

#[test]
fn bytes_dto_raw_roundtrip() {
    let v = BytesDto::Raw(vec![0, 255, 1]);
    let s = serde_json::to_string(&v).unwrap();
    let back: BytesDto = serde_json::from_str(&s).unwrap();
    assert_eq!(back, v);
}

#[test]
fn ping_request_camel_case() {
    let j = json!({"value": "x"});
    let p: PingRequest = serde_json::from_value(j).unwrap();
    assert_eq!(p.value.as_deref(), Some("x"));
}

#[test]
fn duration_dto_roundtrip() {
    let d = DurationDto { secs: 1, nanos: 2 };
    let s = serde_json::to_string(&d).unwrap();
    let back: DurationDto = serde_json::from_str(&s).unwrap();
    assert_eq!(back.secs, 1);
    assert_eq!(back.nanos, 2);
}

#[test]
fn procedure_slip10_generate_json_shape() {
    let p = ProcedureDto::SLIP10Generate {
        output: LocationDto::Generic {
            vault: BytesDto::Text("v".into()),
            record: BytesDto::Text("r".into()),
        },
        size_bytes: Some(32),
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["type"], "SLIP10Generate");
    assert!(v["payload"]["sizeBytes"].as_u64().is_some());
}

#[test]
fn slip10_derive_input_seed_variant() {
    let loc = LocationDto::Counter {
        vault: BytesDto::Raw(vec![1]),
        counter: 42,
    };
    let p = ProcedureDto::SLIP10Derive {
        chain: vec![44, 0],
        input: Slip10DeriveInputDto::Seed(loc.clone()),
        output: LocationDto::Generic {
            vault: BytesDto::Text("outv".into()),
            record: BytesDto::Text("outr".into()),
        },
    };
    let json = serde_json::to_string(&p).unwrap();
    let back: ProcedureDto = serde_json::from_str(&json).unwrap();
    match back {
        ProcedureDto::SLIP10Derive { chain, .. } => assert_eq!(chain, vec![44, 0]),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn public_key_procedure_ed25519() {
    let p = ProcedureDto::PublicKey {
        ty: KeyType::Ed25519,
        private_key: LocationDto::Generic {
            vault: BytesDto::Text("v".into()),
            record: BytesDto::Text("r".into()),
        },
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["payload"]["type"], "ed25519");
}

#[test]
fn password_entry_dto_camel_case() {
    let j = json!([{"account": "a", "secret": "s"}]);
    let v: Vec<PasswordEntryDto> = serde_json::from_value(j).unwrap();
    assert_eq!(v[0].account, "a");
    assert_eq!(v[0].secret, "s");
}

#[test]
fn password_backup_plain_roundtrip() {
    let b = PasswordBackupPlainDto {
        format_version: 1,
        entries: vec![PasswordBackupEntryDto {
            account: "k".into(),
            secret: Some("v".into()),
        }],
    };
    let s = serde_json::to_string(&b).unwrap();
    let back: PasswordBackupPlainDto = serde_json::from_str(&s).unwrap();
    assert_eq!(back.format_version, 1);
    assert_eq!(back.entries[0].account, "k");
    assert_eq!(back.entries[0].secret.as_deref(), Some("v"));
}

#[test]
fn password_backup_encrypted_dto_shape() {
    let d = PasswordBackupEncryptedDto {
        format_version: 1,
        salt: "YQ==".into(),
        nonce: "Yg==".into(),
        ciphertext: "Yw==".into(),
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["formatVersion"], 1);
    assert!(v["salt"].as_str().is_some());
}
