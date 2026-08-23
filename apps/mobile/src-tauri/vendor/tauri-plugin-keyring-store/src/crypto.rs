//! Stronghold-compatible crypto procedures using [`iota-crypto`] primitives.
//!
//! Secrets at SLIP10/BIP39 **output locations** are persisted in the OS keyring as binary blobs (Base64-encoded).

use std::borrow::Borrow;

use crypto::keys::bip39::{self, wordlist};
use crypto::keys::slip10::{self, Seed, Segment};
use crypto::signatures::ed25519::SecretKey;
use zeroize::Zeroize;

use crate::error::{Error, Result};
use crate::models::{BytesDto, KeyType, LocationDto, ProcedureDto, Slip10DeriveInputDto};
use crate::store::KeyringStore;

fn account_for_location(
    store: &KeyringStore,
    snapshot_path: &str,
    client: &BytesDto,
    loc: &LocationDto,
) -> String {
    match loc {
        LocationDto::Generic { vault, record } => {
            store.account_vault_record(snapshot_path, client, vault, record)
        }
        LocationDto::Counter { vault, counter } => {
            let record = BytesDto::Text(counter.to_string());
            store.account_vault_record(snapshot_path, client, vault, &record)
        }
    }
}

fn read_location(
    store: &KeyringStore,
    snapshot_path: &str,
    client: &BytesDto,
    loc: &LocationDto,
) -> Result<Option<Vec<u8>>> {
    let account = account_for_location(store, snapshot_path, client, loc);
    store.get_bytes(&account)
}

fn write_location(
    store: &KeyringStore,
    snapshot_path: &str,
    client: &BytesDto,
    loc: &LocationDto,
    bytes: &[u8],
) -> Result<()> {
    let account = account_for_location(store, snapshot_path, client, loc);
    store.set_bytes(&account, bytes)
}

/// Executes a Stronghold-shaped procedure; reads/writes key material via [`KeyringStore`] locations.
///
/// This module is crate-private; embedders call the **`execute_procedure`** IPC command (guest-js
/// `Vault.*` helpers) when the **`crypto`** feature is enabled for the Rust crate.
pub fn execute_procedure(
    store: &KeyringStore,
    snapshot_path: &str,
    client: &BytesDto,
    procedure: ProcedureDto,
) -> Result<Vec<u8>> {
    match procedure {
        ProcedureDto::SLIP10Generate { output, size_bytes } => {
            let size = size_bytes.unwrap_or(32).max(16);
            let mut buf = vec![0_u8; size];
            crypto::utils::rand::fill(&mut buf).map_err(|e| Error::Crypto(format!("{e:?}")))?;
            let seed = Seed::from_bytes(&buf);
            buf.zeroize();
            let master = slip10::Slip10::<SecretKey>::from(&seed);
            let ext = *master.extended_bytes();
            write_location(store, snapshot_path, client, &output, &ext)?;
            Ok(ext.to_vec())
        }
        ProcedureDto::SLIP10Derive {
            chain,
            input,
            output,
        } => {
            let slip = match input {
                Slip10DeriveInputDto::Seed(loc) => {
                    let bytes =
                        read_location(store, snapshot_path, client, &loc)?.ok_or_else(|| {
                            Error::Crypto("SLIP10 derive: seed location empty".into())
                        })?;
                    let seed = Seed::from_bytes(&bytes);
                    slip10::Slip10::<SecretKey>::from(&seed)
                }
                Slip10DeriveInputDto::Key(loc) => {
                    let bytes = read_location(store, snapshot_path, client, &loc)?
                        .ok_or_else(|| Error::Crypto("SLIP10 derive: key location empty".into()))?;
                    if bytes.len() != 65 {
                        return Err(Error::Crypto(format!(
                            "SLIP10 derive: expected 65-byte extended key, got {}",
                            bytes.len()
                        )));
                    }
                    let mut arr = [0_u8; 65];
                    arr.copy_from_slice(&bytes);
                    slip10::Slip10::<SecretKey>::try_from_extended_bytes(&arr)
                        .map_err(|e| Error::Crypto(format!("{e:?}")))?
                }
            };

            let derived = chain
                .into_iter()
                .fold(slip, |acc, seg| acc.child_key(seg.harden()));
            let ext = *derived.extended_bytes();
            write_location(store, snapshot_path, client, &output, &ext)?;
            Ok(ext.to_vec())
        }
        ProcedureDto::BIP39Recover {
            mnemonic,
            passphrase,
            output,
        } => {
            let m = bip39::Mnemonic::from(mnemonic.as_str());
            wordlist::verify(m.borrow(), &wordlist::ENGLISH)
                .map_err(|e| Error::Crypto(format!("{e:?}")))?;
            let pwd = passphrase.map(bip39::Passphrase::from).unwrap_or_default();
            let bip_seed = bip39::mnemonic_to_seed(m.borrow(), pwd.borrow());
            let slip_seed: slip10::Seed = bip_seed.into();
            let master = slip10::Slip10::<SecretKey>::from(&slip_seed);
            let ext = *master.extended_bytes();
            write_location(store, snapshot_path, client, &output, &ext)?;
            Ok(ext.to_vec())
        }
        ProcedureDto::BIP39Generate { passphrase, output } => {
            let mut entropy = [0_u8; 16];
            crypto::utils::rand::fill(&mut entropy).map_err(|e| Error::Crypto(format!("{e:?}")))?;
            let mnemonic = wordlist::encode(&entropy, &wordlist::ENGLISH)
                .map_err(|e| Error::Crypto(format!("{e:?}")))?;
            let pwd = passphrase.map(bip39::Passphrase::from).unwrap_or_default();
            let bip_seed = bip39::mnemonic_to_seed(mnemonic.borrow(), pwd.borrow());
            let slip_seed: slip10::Seed = bip_seed.into();
            let master = slip10::Slip10::<SecretKey>::from(&slip_seed);
            let ext = *master.extended_bytes();
            write_location(store, snapshot_path, client, &output, &ext)?;
            Ok(ext.to_vec())
        }
        ProcedureDto::PublicKey { ty, private_key } => match ty {
            KeyType::Ed25519 => {
                let bytes = read_location(store, snapshot_path, client, &private_key)?.ok_or_else(
                    || Error::Crypto("public key: private key location empty".into()),
                )?;
                let pk = if bytes.len() == 65 {
                    let mut arr = [0_u8; 65];
                    arr.copy_from_slice(&bytes);
                    let slip = slip10::Slip10::<SecretKey>::try_from_extended_bytes(&arr)
                        .map_err(|e| Error::Crypto(format!("{e:?}")))?;
                    slip.secret_key().public_key()
                } else if bytes.len() == 32 {
                    let mut arr = [0_u8; 32];
                    arr.copy_from_slice(&bytes);
                    let sk = SecretKey::from_bytes(&arr);
                    sk.public_key()
                } else {
                    return Err(Error::Crypto(format!(
                        "public key: expected 32-byte sk or 65-byte extended key, got {}",
                        bytes.len()
                    )));
                };
                Ok(pk.to_bytes().to_vec())
            }
            KeyType::X25519 => Err(Error::Crypto(
                "X25519 public key extraction is not implemented".into(),
            )),
        },
        ProcedureDto::Ed25519Sign { private_key, msg } => {
            let bytes = read_location(store, snapshot_path, client, &private_key)?
                .ok_or_else(|| Error::Crypto("sign: private key location empty".into()))?;
            let sig = if bytes.len() == 65 {
                let mut arr = [0_u8; 65];
                arr.copy_from_slice(&bytes);
                let slip = slip10::Slip10::<SecretKey>::try_from_extended_bytes(&arr)
                    .map_err(|e| Error::Crypto(format!("{e:?}")))?;
                slip.secret_key().sign(msg.as_bytes())
            } else if bytes.len() == 32 {
                let mut arr = [0_u8; 32];
                arr.copy_from_slice(&bytes);
                let sk = SecretKey::from_bytes(&arr);
                sk.sign(msg.as_bytes())
            } else {
                return Err(Error::Crypto(format!(
                    "sign: expected 32-byte sk or 65-byte extended key, got {}",
                    bytes.len()
                )));
            };
            Ok(sig.to_bytes().to_vec())
        }
    }
}
