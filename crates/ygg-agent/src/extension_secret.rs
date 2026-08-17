//! Host-owned, owner-scoped secret brokerage for executable extensions.
//!
//! Secret values stay in zeroizing byte buffers. The extension transport
//! serializes them from a borrowed UTF-8 view into a writer frame whose bytes
//! are also erased after delivery.

use std::fmt;

use async_trait::async_trait;

use crate::extension_process::{ExtensionIdentity, ExtensionResourceOwner};

/// Maximum UTF-8 secret value accepted from a configured broker (64 KiB).
pub const MAX_EXTENSION_SECRET_BYTES: usize = 64 * 1024;

/// Host-derived context for one extension secret lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionSecretRequest {
    /// The manifest-bound extension principal requesting the value.
    pub extension: ExtensionIdentity,
    /// The exact host session/process owner of the parent operation.
    pub resource_owner: ExtensionResourceOwner,
    /// Exact active host request that owns this lookup.
    pub parent_request_id: u64,
    /// The manifest-allowlisted logical secret name.
    pub name: String,
}

/// A secret value whose backing allocation is erased on drop.
pub struct ExtensionSecretValue(Vec<u8>);

impl ExtensionSecretValue {
    /// Copies a UTF-8 value into a zeroizing buffer after enforcing the wire
    /// bound. Empty values are allowed because some credentials intentionally
    /// distinguish an empty value from an unavailable secret.
    pub fn new(value: impl AsRef<[u8]>) -> Result<Self, ExtensionSecretError> {
        let value = value.as_ref();
        if value.len() > MAX_EXTENSION_SECRET_BYTES {
            return Err(ExtensionSecretError::TooLarge {
                bytes: value.len(),
                limit: MAX_EXTENSION_SECRET_BYTES,
            });
        }
        std::str::from_utf8(value).map_err(|_| ExtensionSecretError::NotUtf8)?;
        Ok(Self(value.to_vec()))
    }

    /// Borrows the validated UTF-8 value without allocating another copy.
    pub fn as_str(&self) -> &str {
        // Construction validates UTF-8 and the bytes are never exposed mutably.
        std::str::from_utf8(&self.0).expect("ExtensionSecretValue is valid UTF-8")
    }

    /// Returns the number of secret bytes held by this value.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether this secret contains zero bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ExtensionSecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtensionSecretValue")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl Drop for ExtensionSecretValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Secret-provider failure. Transport responses deliberately collapse these
/// details to a generic unavailable error so extensions cannot probe host
/// configuration; diagnostics remain host-side.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionSecretError {
    /// The broker returned a value larger than the host wire bound.
    #[error("secret is too large ({bytes} bytes, limit {limit})")]
    TooLarge {
        /// Observed bytes.
        bytes: usize,
        /// Maximum accepted bytes.
        limit: usize,
    },
    /// Secret transport is intentionally UTF-8 in API `0.2`.
    #[error("secret is not valid UTF-8")]
    NotUtf8,
    /// The configured provider could not complete the lookup.
    #[error("secret provider failed: {0}")]
    Provider(String),
}

/// Optional host service that resolves manifest-allowlisted secret names.
///
/// Ygg supplies the extension principal and resource owner; neither value is
/// accepted from child JSON. Implementations should apply any additional
/// user, vault, or environment policy before returning a value. A broker must
/// not strongly retain the [`crate::ExtensionProcess`] that owns it; keep
/// independent provider state or a weak reference so reload/shutdown remains
/// acyclic.
#[async_trait]
pub trait ExtensionSecretBroker: Send + Sync {
    /// Resolve one secret. `Ok(None)` means the name is unavailable for this
    /// principal and owner without revealing why.
    async fn get_secret(
        &self,
        request: ExtensionSecretRequest,
    ) -> Result<Option<ExtensionSecretValue>, ExtensionSecretError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_value_is_bounded_utf8_and_redacted() {
        let value = ExtensionSecretValue::new("top-secret").unwrap();
        assert_eq!(value.as_str(), "top-secret");
        assert_eq!(value.len(), 10);
        let debug = format!("{value:?}");
        assert!(debug.contains("bytes: 10"));
        assert!(!debug.contains("top-secret"));

        assert_eq!(
            ExtensionSecretValue::new([0xff]).unwrap_err(),
            ExtensionSecretError::NotUtf8
        );
        assert!(matches!(
            ExtensionSecretValue::new(vec![b'x'; MAX_EXTENSION_SECRET_BYTES + 1]),
            Err(ExtensionSecretError::TooLarge { .. })
        ));
    }
}
