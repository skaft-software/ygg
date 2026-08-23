//! Encrypted backup envelope (Argon2id + ChaCha20-Poly1305), always available.
//! (The `crypto` Cargo feature only toggles SLIP10/BIP39/Ed25519 procedures, not this module.)
//!
//! We pin `generic-array` 0.14.9+ for docs.rs; `chacha20poly1305` still calls deprecated `GenericArray::from_slice` until the ecosystem moves to generic-array 1.x.

#![allow(deprecated)]

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::error::{Error, Result};
use crate::models::PasswordBackupEncryptedDto;

const BACKUP_CRYPTO_FORMAT: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

/// Argon2id parameters (memory KiB, iterations, lanes, output length).
fn argon_params() -> Params {
    Params::new(19456, 3, 1, Some(KEY_LEN)).expect("valid argon params")
}

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN]> {
    let mut key = [0u8; KEY_LEN];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, argon_params())
        .hash_password_into(passphrase, salt, &mut key)
        .map_err(|e| Error::Crypto(format!("argon2: {e}")))?;
    Ok(key)
}

/// Encrypt UTF-8 plaintext bytes (typically JSON of [`crate::models::PasswordBackupPlainDto`]).
pub fn encrypt_plain_bytes(plain: &[u8], passphrase: &[u8]) -> Result<PasswordBackupEncryptedDto> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).map_err(|e| Error::Crypto(format!("rng salt: {e}")))?;

    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| Error::Crypto(format!("rng nonce: {e}")))?;

    let key = derive_key(passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let n = Nonce::from_slice(&nonce);
    let ciphertext = cipher
        .encrypt(n, plain)
        .map_err(|_| Error::Crypto("chacha encrypt failed".into()))?;

    let engine = base64::engine::general_purpose::STANDARD;
    Ok(PasswordBackupEncryptedDto {
        format_version: BACKUP_CRYPTO_FORMAT,
        salt: engine.encode(salt),
        nonce: engine.encode(nonce),
        ciphertext: engine.encode(ciphertext),
    })
}

/// Decrypt envelope back to plaintext bytes.
pub fn decrypt_to_plain(dto: &PasswordBackupEncryptedDto, passphrase: &[u8]) -> Result<Vec<u8>> {
    if dto.format_version != BACKUP_CRYPTO_FORMAT {
        return Err(Error::Crypto(format!(
            "unsupported backup crypto format {}",
            dto.format_version
        )));
    }

    let engine = base64::engine::general_purpose::STANDARD;
    let salt = engine
        .decode(dto.salt.trim())
        .map_err(|e| Error::Crypto(format!("salt base64: {e}")))?;
    let nonce = engine
        .decode(dto.nonce.trim())
        .map_err(|e| Error::Crypto(format!("nonce base64: {e}")))?;
    let ciphertext = engine
        .decode(dto.ciphertext.trim())
        .map_err(|e| Error::Crypto(format!("ciphertext base64: {e}")))?;

    if salt.len() != SALT_LEN || nonce.len() != NONCE_LEN {
        return Err(Error::Crypto("invalid salt or nonce length".into()));
    }

    let key = derive_key(passphrase, &salt)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let n = Nonce::from_slice(&nonce);
    cipher.decrypt(n, ciphertext.as_ref()).map_err(|_| {
        Error::Crypto("chacha decrypt failed (wrong passphrase or corrupt data)".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let plain = br#"{"formatVersion":1,"entries":[]}"#;
        let enc = encrypt_plain_bytes(plain, b"test-passphrase").unwrap();
        let back = decrypt_to_plain(&enc, b"test-passphrase").unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn wrong_passphrase_fails() {
        let plain = b"secret data";
        let enc = encrypt_plain_bytes(plain, b"a").unwrap();
        assert!(decrypt_to_plain(&enc, b"b").is_err());
    }
}
