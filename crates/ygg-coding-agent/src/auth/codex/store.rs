#![allow(missing_docs)]

//! File-backed credential store at `~/.ygg/credentials/codex.json` (mode 0600).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The OAuth tokens plus the derived account id.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub account_id: String,
}

/// On-disk credential record. `expires_at` is Unix seconds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CredentialFile {
    pub tokens: Tokens,
    pub expires_at: u64,
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

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load the credential, or `None` if the file does not exist.
    pub fn load(&self) -> Result<Option<CredentialFile>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let cred = serde_json::from_slice(&bytes)
                    .with_context(|| format!("corrupt credential file {}", self.path.display()))?;
                Ok(Some(cred))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    /// Persist a credential with owner-only permissions. The file is created
    /// `0600` *before* the secret bytes are written, so there is never a window
    /// where the tokens are world-readable.
    pub fn save(&self, cred: &CredentialFile) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec_pretty(cred)?;
        write_private(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("deleting {}", self.path.display())),
        }
    }
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // Re-assert the mode in case the file already existed with looser bits.
    let perms = std::fs::Permissions::from_mode(0o600);
    file.set_permissions(perms)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
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
    fn round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(&path);
        assert!(!store.exists());
        assert!(store.load().unwrap().is_none());

        store.save(&sample()).unwrap();
        assert!(store.exists());
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.tokens.access_token, "acc");
        assert_eq!(loaded.tokens.account_id, "acct_1");
        assert_eq!(loaded.expires_at, 1_000_000);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credential file must be owner-only");
        }

        store.delete().unwrap();
        assert!(!store.exists());
        // Deleting a missing file is not an error.
        store.delete().unwrap();
    }
}
