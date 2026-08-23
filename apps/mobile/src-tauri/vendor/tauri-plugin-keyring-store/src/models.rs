//! Request and response types shared between Rust, IPC, and TypeScript (camelCase where noted).
//!
//! Use [`BytesDto`] for Stronghold-compatible `string | bytes` payloads and [`ProcedureDto`] for
//! optional `crypto` feature procedures.

use std::{fmt, time::Duration};

use serde::{de::Visitor, Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
/// IPC ping request (health / round-trip).
pub struct PingRequest {
    /// Echo payload from the client, if any.
    pub value: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
/// IPC ping response.
pub struct PingResponse {
    /// Echo payload returned to the client.
    pub value: Option<String>,
}

/// Bytes payload compatible with Stronghold's JS API (`string | bytes`).
///
/// # Example
///
/// ```
/// use tauri_plugin_keyring_store::BytesDto;
///
/// let text = BytesDto::Text("client-id".into());
/// let raw = BytesDto::Raw(vec![0x01, 0xff]);
/// assert_eq!(text.as_ref(), b"client-id");
/// assert_eq!(raw.as_ref(), [1, 255]);
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Hash, Eq, PartialEq, Ord, PartialOrd)]
#[serde(untagged)]
pub enum BytesDto {
    /// UTF-8 string (stored/compared as bytes via [`AsRef`]).
    Text(String),
    /// Raw binary (e.g. client id bytes).
    Raw(Vec<u8>),
}

impl AsRef<[u8]> for BytesDto {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Text(t) => t.as_bytes(),
            Self::Raw(b) => b.as_ref(),
        }
    }
}

impl From<BytesDto> for Vec<u8> {
    fn from(v: BytesDto) -> Self {
        match v {
            BytesDto::Text(t) => t.into_bytes(),
            BytesDto::Raw(b) => b,
        }
    }
}

/// Stronghold-compatible duration (ignored for OS keyring persistence).
///
/// # Example
///
/// ```
/// use tauri_plugin_keyring_store::DurationDto;
/// use std::time::Duration;
///
/// let d = DurationDto { secs: 1, nanos: 500 };
/// let std_d: Duration = d.into();
/// assert_eq!(std_d.as_secs(), 1);
/// ```
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DurationDto {
    /// Whole seconds.
    pub secs: u64,
    /// Sub-second nanoseconds (`0..1_000_000_000`).
    pub nanos: u32,
}

impl From<DurationDto> for Duration {
    fn from(d: DurationDto) -> Self {
        Duration::new(d.secs, d.nanos)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload")]
/// Logical storage location inside a snapshot (vault + record or counter row).
pub enum LocationDto {
    /// Arbitrary record under a named vault.
    Generic {
        /// Vault id (often a string name).
        vault: BytesDto,
        /// Record path inside the vault.
        record: BytesDto,
    },
    /// Counter-style record (`counter` serialized as text for hashing).
    Counter {
        /// Vault id.
        vault: BytesDto,
        /// Monotonic row index.
        counter: usize,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload")]
/// SLIP10 derive input: either a seed blob or an extended parent key.
pub enum Slip10DeriveInputDto {
    /// Read SLIP10 seed bytes from this location.
    Seed(LocationDto),
    /// Read 65-byte extended SLIP10 key from this location.
    Key(LocationDto),
}

#[derive(Debug, Clone, Copy)]
/// Asymmetric key kind for `PublicKey` procedure (string JSON: `ed25519` / `x25519`).
pub enum KeyType {
    /// Ed25519 curve (supported for derive/sign/public key).
    Ed25519,
    /// X25519 (public key extraction not implemented in this plugin).
    X25519,
}

impl<'de> Deserialize<'de> for KeyType {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct KeyTypeVisitor;

        impl<'de> Visitor<'de> for KeyTypeVisitor {
            type Value = KeyType;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("ed25519 or x25519")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match value.to_lowercase().as_str() {
                    "ed25519" => Ok(KeyType::Ed25519),
                    "x25519" => Ok(KeyType::X25519),
                    _ => Err(serde::de::Error::custom("unknown key type")),
                }
            }
        }

        deserializer.deserialize_str(KeyTypeVisitor)
    }
}

impl Serialize for KeyType {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = match self {
            KeyType::Ed25519 => "ed25519",
            KeyType::X25519 => "x25519",
        };
        serializer.serialize_str(s)
    }
}

/// Stronghold-shaped crypto or signing operation (requires the **`crypto`** crate feature).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum ProcedureDto {
    /// Generate a new SLIP10 master and write the extended key to `output`.
    SLIP10Generate {
        /// Target vault location for the new master key material.
        output: LocationDto,
        /// Seed length in bytes (default 32, clamped to at least 16).
        #[serde(rename = "sizeBytes")]
        size_bytes: Option<usize>,
    },
    /// Derive a child SLIP10 key along `chain` and write to `output`.
    SLIP10Derive {
        /// Hardened segment indices (same semantics as Stronghold).
        chain: Vec<u32>,
        /// Parent seed or extended key read from `input`.
        input: Slip10DeriveInputDto,
        /// Where to write derived extended key bytes.
        output: LocationDto,
    },
    /// Restore BIP39 mnemonic to SLIP10 master at `output`.
    BIP39Recover {
        /// Space-separated mnemonic (English wordlist).
        mnemonic: String,
        /// Optional BIP39 passphrase.
        passphrase: Option<String>,
        /// Where to write extended master key bytes.
        output: LocationDto,
    },
    /// Generate entropy, build an English mnemonic, derive the BIP39 seed → SLIP10 master, and write extended key bytes to `output` (return value matches written bytes).
    BIP39Generate {
        /// Optional BIP39 passphrase for seed derivation.
        passphrase: Option<String>,
        /// Where to write extended master key bytes.
        output: LocationDto,
    },
    /// Extract public key bytes (32-byte Ed25519 pk) from a private or extended key at `private_key`.
    PublicKey {
        /// Curve / algorithm selector.
        #[serde(rename = "type")]
        ty: KeyType,
        /// Location of 32-byte secret key or 65-byte extended SLIP10 key.
        #[serde(rename = "privateKey")]
        private_key: LocationDto,
    },
    /// Sign `msg` (UTF-8 interpreted as bytes) with Ed25519 private key at `private_key`.
    Ed25519Sign {
        /// Location of signing key material.
        #[serde(rename = "privateKey")]
        private_key: LocationDto,
        /// Message to sign (UTF-8 string, not hex).
        msg: String,
    },
}

/// One row for the bulk **set passwords** IPC command (`account` + plaintext `secret`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordEntryDto {
    /// Logical account id (often `prefix.name` from [`crate::join_prefix`]).
    pub account: String,
    /// Secret value to store (plaintext over IPC — see README security notes).
    pub secret: String,
}

/// Single entry in a plaintext password backup (export/import).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordBackupEntryDto {
    /// Account id included in the backup.
    pub account: String,
    /// Stored secret, or [`None`] if the credential was missing at export time.
    pub secret: Option<String>,
}

/// Plaintext backup blob shared by export/import JSON over IPC.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordBackupPlainDto {
    /// Schema version for forward compatibility.
    pub format_version: u32,
    /// All exported rows.
    pub entries: Vec<PasswordBackupEntryDto>,
}

/// Argon2id + ChaCha20-Poly1305 backup envelope (always built).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordBackupEncryptedDto {
    /// Schema version for forward compatibility.
    pub format_version: u32,
    /// Salt for Argon2 (Base64).
    pub salt: String,
    /// Nonce for ChaCha20-Poly1305 (Base64).
    pub nonce: String,
    /// Ciphertext including Poly1305 tag (Base64).
    pub ciphertext: String,
}
