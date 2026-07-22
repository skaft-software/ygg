#![allow(missing_docs)]

//! File-backed credential store at `~/.ygg/credentials/codex.json` (mode 0600).

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_MODEL_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// The OAuth tokens plus the derived account id.
#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Tokens")
            .field("access_token", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .finish()
    }
}

/// On-disk credential record. `expires_at` is Unix seconds.
#[derive(Clone, Serialize, Deserialize)]
pub struct CredentialFile {
    pub tokens: Tokens,
    pub expires_at: u64,
}

impl fmt::Debug for CredentialFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialFile")
            .field("tokens", &self.tokens)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Default store path: `~/.ygg/credentials/codex.json`.
pub fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("credentials")
        .join("codex.json")
}

/// A single JSON credential file.
#[derive(Clone, Debug)]
pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn model_cache_path(&self) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("codex");
        self.path.with_file_name(format!("{stem}-models.json"))
    }

    /// Load the credential, or `None` if the file does not exist.
    pub fn load(&self) -> Result<Option<CredentialFile>> {
        let Some(bytes) = crate::auth::read_bounded_regular(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?
        else {
            return Ok(None);
        };
        let cred = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt credential file {}", self.path.display()))?;
        Ok(Some(cred))
    }

    /// Load the account-scoped model cache, if present.
    pub(crate) fn load_model_cache(&self) -> Result<Option<Vec<u8>>> {
        let path = self.model_cache_path();
        crate::auth::read_bounded_regular(&path, MAX_MODEL_CACHE_BYTES)
            .with_context(|| format!("reading {}", path.display()))
    }

    /// Persist account-scoped model metadata with owner-only permissions.
    pub(crate) fn save_model_cache(&self, bytes: &[u8]) -> Result<()> {
        let path = self.model_cache_path();
        prepare_private_parent(&path)?;
        write_private(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Whether cached model metadata should be refreshed in the background.
    /// A future-dated mtime is treated as fresh; a missing cache is stale.
    pub(crate) fn model_cache_is_stale(&self, max_age: std::time::Duration) -> Result<bool> {
        let path = self.model_cache_path();
        let modified = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(modified.elapsed().is_ok_and(|age| age >= max_age))
    }

    /// Persist a credential with owner-only permissions. The file is created
    /// `0600` *before* the secret bytes are written, so there is never a window
    /// where the tokens are world-readable.
    pub fn save(&self, cred: &CredentialFile) -> Result<()> {
        prepare_private_parent(&self.path)?;
        let bytes = serde_json::to_vec_pretty(cred)?;
        write_private(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        remove_if_present(&self.path)?;
        remove_if_present(&self.model_cache_path())
    }
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // The file itself is 0600, but the directory should not be
        // world-traversable either.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("restricting {}", parent.display()))?;
        }
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("deleting {}", path.display())),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "credential path has no parent",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".codex-credential-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    // Persist the rename itself across a power loss where the platform permits
    // directory handles to be synced.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CredentialFile {
        CredentialFile {
            tokens: Tokens {
                access_token: "acc".into(),
                refresh_token: "ref".into(),
                account_id: "acct_1".into(),
            },
            expires_at: 1_000_000,
        }
    }

    #[test]
    fn debug_output_redacts_both_tokens() {
        let debug = format!("{:?}", sample());
        assert!(!debug.contains("\"acc\""), "{debug}");
        assert!(!debug.contains("\"ref\""), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
    }

    #[test]
    fn round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(&path);
        assert!(!path.exists());
        assert!(store.load().unwrap().is_none());

        store.save(&sample()).unwrap();
        assert!(path.exists());
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token, "acc");
        assert_eq!(loaded.tokens.account_id, "acct_1");
        assert_eq!(loaded.expires_at, 1_000_000);

        let mut rotated = sample();
        rotated.tokens.access_token = "rotated-access".to_string();
        rotated.tokens.refresh_token = "rotated-refresh".to_string();
        store.save(&rotated).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token, "rotated-access");
        assert_eq!(loaded.tokens.refresh_token, "rotated-refresh");

        store.save_model_cache(br#"{"version":1}"#).unwrap();
        assert_eq!(
            store.load_model_cache().unwrap().unwrap(),
            br#"{"version":1}"#
        );
        assert!(store.model_cache_path().exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credential file must be owner-only");
            let cache_mode = std::fs::metadata(store.model_cache_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(cache_mode & 0o777, 0o600, "model cache must be owner-only");
            let dir_mode = std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                dir_mode & 0o777,
                0o700,
                "credentials directory must not be world-traversable"
            );
        }

        store.delete().unwrap();
        assert!(!path.exists());
        assert!(!store.model_cache_path().exists());
        // Deleting missing files is not an error.
        store.delete().unwrap();
    }
}
