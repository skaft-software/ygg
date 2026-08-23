//! Error types returned by the plugin (IPC-safe).

use serde::{ser::Serializer, Serialize};

/// Result alias using this crate’s [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced to the frontend and backend helpers (serializes as a plain string over IPC).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Stronghold-style session path was used before `initialize` completed for that path.
    #[error("session not initialized: {0}")]
    SessionNotInitialized(String),
    /// OS keyring reported “no such credential” (or empty semantics aligned with Stronghold).
    #[error("credential entry not found")]
    NoEntry,
    /// Platform backend could not be registered (e.g. missing OS APIs in doc builds).
    #[error("plugin initialization failed: {0}")]
    Init(String),
    /// Keychain / protected store unavailable while the device is locked (iOS Data Protection).
    #[error("keychain locked or protected data unavailable")]
    KeychainLocked,
    /// Wrapped error from [`keyring_core`] / native store.
    #[error("keyring error: {0}")]
    Keyring(String),
    /// Base64 or other encoding used by the store layer failed.
    #[error("encoding error: {0}")]
    Encoding(String),
    /// Optional `crypto` feature: SLIP10 / BIP39 / signing procedure failed.
    #[error("crypto procedure error: {0}")]
    Crypto(String),
    /// [`crate::join_prefix`] / [`crate::split_prefixed`] validation failed.
    #[error("naming / validation error: {0}")]
    Naming(String),
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.to_string().as_ref())
    }
}
