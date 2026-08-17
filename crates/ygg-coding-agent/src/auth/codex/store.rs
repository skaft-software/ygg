#![allow(missing_docs)]

//! File-backed credential store at `~/.ygg/credentials/codex.json` (mode 0600).

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_MODEL_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_JWT_PAYLOAD_BYTES: usize = 512 * 1024;
const MIGRATION_MARKER_BYTES: &[u8] = b"legacy-import-v1\n";

fn cache_modified_is_stale(modified: std::time::SystemTime, max_age: std::time::Duration) -> bool {
    modified.elapsed().map_or(true, |age| age >= max_age)
}

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
            .field("account_id", &"[REDACTED]")
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
    legacy_home: Option<PathBuf>,
}

/// Cross-process refresh serialization guard. The lock is held on the private
/// credential directory itself, whose descriptor stays stable while credential
/// files are atomically replaced.
#[must_use = "the refresh lock must be retained until the protected operation completes"]
pub(crate) struct RefreshLock {
    directory: std::fs::File,
    path: PathBuf,
    locked: bool,
}

impl RefreshLock {
    fn release(&mut self) -> Result<()> {
        if !std::mem::replace(&mut self.locked, false) {
            return Ok(());
        }
        fs2::FileExt::unlock(&self.directory)
            .with_context(|| format!("unlocking refresh state {}", self.path.display()))
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        self.release()
    }

    fn finish_with<T>(self, result: Result<T>) -> Result<T> {
        match result {
            Ok(value) => {
                self.finish()?;
                Ok(value)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let legacy_home = if path == default_path() {
            dirs::home_dir().filter(|home| home.is_absolute())
        } else {
            None
        };
        Self { path, legacy_home }
    }

    #[cfg(test)]
    pub(crate) fn with_legacy_home(path: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            legacy_home: Some(home.into()),
        }
    }

    fn model_cache_path(&self) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("codex");
        self.path.with_file_name(format!("{stem}-models.json"))
    }

    fn open_model_cache(&self) -> Result<Option<std::fs::File>> {
        let path = self.model_cache_path();
        match ygg_agent::secure_fs::open_private_file_for_read(&path) {
            Ok(file) => Ok(Some(file)),
            Err(ygg_agent::secure_fs::SecureFileError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(anyhow::anyhow!("refusing {}: {error}", path.display())),
        }
    }

    fn migration_marker_path(&self) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("codex");
        self.path
            .with_file_name(format!(".{stem}-legacy-import-v1"))
    }

    fn refresh_lock_directory(&self) -> Result<PathBuf> {
        self.path
            .parent()
            .filter(|path| path.is_absolute())
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "credential path has no absolute parent: {}",
                    self.path.display()
                )
            })
    }

    pub(crate) fn lock_refresh(&self) -> Result<RefreshLock> {
        let path = self.refresh_lock_directory()?;
        let directory = ygg_agent::secure_fs::open_private_directory_for_lock(&path)
            .with_context(|| format!("opening refresh lock directory {}", path.display()))?;
        fs2::FileExt::lock_exclusive(&directory)
            .with_context(|| format!("locking refresh state {}", path.display()))?;
        Ok(RefreshLock {
            directory,
            path,
            locked: true,
        })
    }

    /// Load the credential, or `None` if neither Ygg nor a supported legacy
    /// store contains one. A default store imports at most once while holding
    /// the same cross-process lock used for token rotation.
    pub fn load(&self) -> Result<Option<CredentialFile>> {
        if let Some(credential) = self.load_ygg_credential()? {
            return Ok(Some(credential));
        }
        let Some(home) = self.legacy_home.as_deref() else {
            return Ok(None);
        };

        // A marker and the migrated credential are two durable files. Always
        // serialize the decision before inspecting the marker so another
        // process cannot observe the deliberate marker-first crash window as a
        // completed migration.
        let refresh_lock = self.lock_refresh()?;
        let result = (|| {
            // Re-check both files after waiting: a login, logout, refresh, or
            // migration in another process may have completed first.
            if let Some(credential) = self.load_ygg_credential()? {
                return Ok(Some(credential));
            }
            if self.migration_marker_exists()? {
                return Ok(None);
            }
            let Some(credential) = import_legacy_from_home(home)? else {
                return Ok(None);
            };

            // Record the one-time decision before copying the credential. A crash
            // can therefore suppress a retry, but can never cause legacy secrets
            // to be imported again after logout.
            self.record_migration_while_refresh_locked(&refresh_lock)?;
            self.save_while_refresh_locked(&credential, &refresh_lock)?;
            Ok(Some(credential))
        })();
        refresh_lock.finish_with(result)
    }

    fn load_ygg_credential(&self) -> Result<Option<CredentialFile>> {
        let Some(bytes) = crate::auth::read_bounded_private(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?
        else {
            return Ok(None);
        };
        parse_credential(&self.path, &bytes).map(Some)
    }

    /// Read only the Ygg-owned credential while the caller owns the refresh
    /// lock. This deliberately never enters legacy migration, avoiding lock
    /// recursion when logout races token refresh.
    pub(crate) fn load_while_refresh_locked(
        &self,
        _refresh_lock: &RefreshLock,
    ) -> Result<Option<CredentialFile>> {
        self.load_ygg_credential()
    }

    fn migration_marker_exists(&self) -> Result<bool> {
        let path = self.migration_marker_path();
        Ok(crate::auth::read_bounded_private(&path, 1024)
            .with_context(|| format!("reading migration marker {}", path.display()))?
            .is_some())
    }

    fn record_migration_while_refresh_locked(&self, _refresh_lock: &RefreshLock) -> Result<()> {
        let path = self.migration_marker_path();
        prepare_private_parent(&path)?;
        write_private(&path, MIGRATION_MARKER_BYTES)
            .with_context(|| format!("writing migration marker {}", path.display()))
    }

    /// Persist account-scoped model metadata with owner-only permissions.
    pub(crate) fn save_model_cache(&self, bytes: &[u8]) -> Result<()> {
        let path = self.model_cache_path();
        prepare_private_parent(&path)?;
        write_private(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Load model metadata only when the exact descriptor being read is fresh.
    pub(crate) fn load_fresh_model_cache(
        &self,
        max_age: std::time::Duration,
    ) -> Result<Option<Vec<u8>>> {
        let path = self.model_cache_path();
        let Some(mut file) = self.open_model_cache()? else {
            return Ok(None);
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("reading {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("reading modification time for {}", path.display()))?;
        if cache_modified_is_stale(modified, max_age) {
            return Ok(None);
        }
        if metadata.len() > MAX_MODEL_CACHE_BYTES as u64 {
            anyhow::bail!(
                "model cache {} exceeds the {}-byte limit",
                path.display(),
                MAX_MODEL_CACHE_BYTES
            );
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&mut file)
            .take(MAX_MODEL_CACHE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {}", path.display()))?;
        if bytes.len() > MAX_MODEL_CACHE_BYTES {
            anyhow::bail!(
                "model cache {} exceeds the {}-byte limit",
                path.display(),
                MAX_MODEL_CACHE_BYTES
            );
        }
        Ok(Some(bytes))
    }

    /// Persist a credential with owner-only permissions. The file is created
    /// `0600` *before* the secret bytes are written, so there is never a window
    /// where the tokens are world-readable. Credential replacement is
    /// serialized with refresh-token rotation across Ygg processes.
    pub fn save(&self, cred: &CredentialFile) -> Result<()> {
        let refresh_lock = self.lock_refresh()?;
        let result = self.save_while_refresh_locked(cred, &refresh_lock);
        refresh_lock.finish_with(result)
    }

    /// Persist while the caller owns this store's refresh lock.
    pub(crate) fn save_while_refresh_locked(
        &self,
        cred: &CredentialFile,
        _refresh_lock: &RefreshLock,
    ) -> Result<()> {
        prepare_private_parent(&self.path)?;
        let bytes = serde_json::to_vec_pretty(cred)?;
        write_private(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        let refresh_lock = self.lock_refresh()?;
        let result = (|| {
            if self.legacy_home.is_some() {
                // Logout permanently suppresses automatic legacy re-import. The
                // source files remain byte-for-byte untouched.
                self.record_migration_while_refresh_locked(&refresh_lock)?;
            }
            remove_if_present(&self.path)?;
            remove_if_present(&self.model_cache_path())
        })();
        refresh_lock.finish_with(result)
    }

    pub(crate) async fn delete_async(&self) -> Result<()> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.delete())
            .await
            .context("credential-delete worker failed")?
    }
}

fn parse_credential(path: &Path, bytes: &[u8]) -> Result<CredentialFile> {
    serde_json::from_slice(bytes)
        .with_context(|| format!("corrupt credential file {}", path.display()))
}

fn import_legacy_from_home(home: &Path) -> Result<Option<CredentialFile>> {
    let codex_path = home.join(".codex").join("auth.json");
    if let Some(value) = read_legacy_json(&codex_path)? {
        if let Some(credential) = credential_from_codex_json(&value) {
            return Ok(Some(credential));
        }
    }
    let hamr_path = home.join(".hamr").join("agent").join("auth.json");
    if let Some(value) = read_legacy_json(&hamr_path)? {
        if let Some(credential) = credential_from_hamr_json(&value) {
            return Ok(Some(credential));
        }
    }
    Ok(None)
}

fn read_legacy_json(path: &Path) -> Result<Option<serde_json::Value>> {
    let Some(bytes) = crate::auth::read_bounded_regular(path, MAX_CREDENTIAL_BYTES)
        .with_context(|| format!("reading legacy credential {}", path.display()))?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

fn credential_from_codex_json(value: &serde_json::Value) -> Option<CredentialFile> {
    let tokens = value.get("tokens")?;
    credential_from_parts(
        string_field(tokens, &["access_token", "accessToken", "access"]),
        string_field(tokens, &["refresh_token", "refreshToken", "refresh"]),
        string_field(tokens, &["account_id", "accountId"]),
        number_field(value, &["expires_at", "expiresAt", "expires"]),
    )
}

fn credential_from_hamr_json(value: &serde_json::Value) -> Option<CredentialFile> {
    let tokens = value.get("openai-codex")?;
    credential_from_parts(
        string_field(tokens, &["access_token", "accessToken", "access"]),
        string_field(tokens, &["refresh_token", "refreshToken", "refresh"]),
        string_field(tokens, &["account_id", "accountId"]),
        number_field(tokens, &["expires_at", "expiresAt", "expires"]),
    )
}

fn credential_from_parts(
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
    expires_at: Option<u64>,
) -> Option<CredentialFile> {
    let access_token = access_token.filter(|value| !value.trim().is_empty())?;
    let refresh_token = refresh_token.filter(|value| !value.trim().is_empty())?;
    let claims = jwt_claims(&access_token);
    let account_id = account_id
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            claims
                .as_ref()?
                .get("https://api.openai.com/auth")?
                .get("chatgpt_account_id")?
                .as_str()
                .map(str::to_owned)
        })?;
    let expires_at = normalize_unix_seconds(expires_at.unwrap_or_else(|| {
        claims
            .as_ref()
            .and_then(|value| value.get("exp"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    }));
    Some(CredentialFile {
        tokens: Tokens {
            access_token,
            refresh_token,
            account_id,
        },
        expires_at,
    })
}

fn string_field(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })
}

fn number_field(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        let value = value.get(*name)?;
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|raw| raw.parse().ok()))
    })
}

fn normalize_unix_seconds(value: u64) -> u64 {
    if value > 10_000_000_000 {
        value / 1_000
    } else {
        value
    }
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if header.is_empty()
        || payload.is_empty()
        || signature.is_empty()
        || segments.next().is_some()
        || payload.len() > MAX_JWT_PAYLOAD_BYTES.saturating_mul(2)
    {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    if bytes.len() > MAX_JWT_PAYLOAD_BYTES {
        return None;
    }
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.is_object().then_some(claims)
}

fn prepare_private_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("credential path has no parent"))?;
    ygg_agent::secure_fs::create_private_directory_all(parent)
        .with_context(|| format!("preparing {}", parent.display()))
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("deleting {}", path.display())),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    ygg_agent::secure_fs::write_private_atomic(path, bytes, MAX_MODEL_CACHE_BYTES)
        .map_err(std::io::Error::other)
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

    fn write_legacy_codex(home: &Path) -> (PathBuf, Vec<u8>) {
        let path = home.join(".codex/auth.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let source = br#"{"tokens":{"access_token":"access","refresh_token":"refresh","account_id":"acct"},"expires_at":4102444800000}"#.to_vec();
        std::fs::write(&path, &source).unwrap();
        (path, source)
    }

    #[test]
    fn future_dated_model_cache_is_stale() {
        assert!(cache_modified_is_stale(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(1),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn model_cache_freshness_refuses_symlinked_metadata() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path().join("credentials/codex.json"));
        let cache = store.model_cache_path();
        prepare_private_parent(&cache).unwrap();
        let target = directory.path().join("unrelated-model-cache");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, &cache).unwrap();

        let error = store
            .load_fresh_model_cache(std::time::Duration::from_secs(60))
            .unwrap_err();
        assert!(error.to_string().contains("refusing"), "{error:#}");
        assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn debug_output_redacts_tokens_and_account_identity() {
        let debug = format!("{:?}", sample());
        assert!(!debug.contains("\"acc\""), "{debug}");
        assert!(!debug.contains("\"ref\""), "{debug}");
        assert!(!debug.contains("acct_1"), "{debug}");
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
            store
                .load_fresh_model_cache(std::time::Duration::from_secs(60))
                .unwrap()
                .unwrap(),
            br#"{"version":1}"#
        );
        assert!(store.model_cache_path().exists());
        assert_eq!(
            store.refresh_lock_directory().unwrap(),
            path.parent().unwrap()
        );

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

    #[cfg(unix)]
    #[test]
    fn refresh_lock_waits_on_the_private_credential_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(path.clone());
        prepare_private_parent(&path).unwrap();
        let lock_directory = store.refresh_lock_directory().unwrap();
        let blocker =
            ygg_agent::secure_fs::open_private_directory_for_lock(&lock_directory).unwrap();
        fs2::FileExt::lock_exclusive(&blocker).unwrap();

        let contender = store.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(contender.lock_refresh()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        fs2::FileExt::unlock(&blocker).unwrap();
        let guard = done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("refresh-lock waiter remained blocked")
            .unwrap();
        guard.finish().unwrap();
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refresh_lock_does_not_replace_an_already_private_credential_directory() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials/codex.json"));
        store.lock_refresh().unwrap().finish().unwrap();
        let before = std::fs::metadata(store.refresh_lock_directory().unwrap()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));

        store.lock_refresh().unwrap().finish().unwrap();
        let after = std::fs::metadata(store.refresh_lock_directory().unwrap()).unwrap();
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec()),
            "opening an already-private credential directory must preserve its identity"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn refresh_lock_remains_serialized_when_credential_file_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(&path);
        store.save(&sample()).unwrap();
        let guard = store.lock_refresh().unwrap();

        ygg_agent::secure_fs::write_private_atomic(&path, br#"{"rotated":true}"#, 1024).unwrap();

        let contender = store.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(contender.lock_refresh()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        guard.finish().unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("refresh-lock waiter remained blocked after credential replacement")
            .unwrap()
            .finish()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn credential_save_waits_for_inflight_refresh_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(&path);
        store.save(&sample()).unwrap();
        let refresh_lock = store.lock_refresh().unwrap();

        let mut replacement = sample();
        replacement.tokens.access_token = "new-login".into();
        let contender = store.clone();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(contender.save(&replacement)).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            done_rx.recv_timeout(std::time::Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));

        drop(refresh_lock);
        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("credential save remained blocked after refresh completed")
            .unwrap();
        worker.join().unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().tokens.access_token,
            "new-login"
        );
    }

    #[test]
    fn imports_codex_cli_credentials_once_without_modifying_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let (auth_path, source) = write_legacy_codex(&home);
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = CredentialStore::with_legacy_home(&path, &home);

        let imported = store.load().unwrap().unwrap();
        assert_eq!(imported.tokens.account_id, "acct");
        assert_eq!(imported.expires_at, 4_102_444_800);
        assert!(path.exists());
        assert_eq!(
            std::fs::read(store.migration_marker_path()).unwrap(),
            MIGRATION_MARKER_BYTES
        );
        let debug = format!("{imported:?}");
        assert!(!debug.contains("\"access\""), "{debug}");
        assert!(!debug.contains("\"refresh\""), "{debug}");
        assert!(!debug.contains("\"acct\""), "{debug}");
        assert_eq!(std::fs::read(auth_path).unwrap(), source);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(store.migration_marker_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn logout_durably_suppresses_legacy_reimport() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let (auth_path, source) = write_legacy_codex(&home);
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = CredentialStore::with_legacy_home(&path, &home);

        assert!(store.load().unwrap().is_some());
        store.delete().unwrap();
        assert!(!path.exists());
        assert!(store.migration_marker_path().exists());
        assert!(store.load().unwrap().is_none());
        assert_eq!(std::fs::read(auth_path).unwrap(), source);
    }

    #[test]
    fn falls_back_to_hamr_codex_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let codex_path = dir.path().join(".codex/auth.json");
        std::fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
        std::fs::write(codex_path, b"not JSON").unwrap();
        let auth_path = dir.path().join(".hamr/agent/auth.json");
        std::fs::create_dir_all(auth_path.parent().unwrap()).unwrap();
        std::fs::write(
            auth_path,
            r#"{"openai-codex":{"access":"access","refresh":"refresh","expires":4102444800,"accountId":"acct"}}"#,
        )
        .unwrap();

        let imported = import_legacy_from_home(dir.path()).unwrap().unwrap();
        assert_eq!(imported.tokens.account_id, "acct");
        assert_eq!(imported.expires_at, 4_102_444_800);
    }

    #[test]
    fn reload_while_refresh_locked_never_reenters_legacy_migration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write_legacy_codex(&home);
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = CredentialStore::with_legacy_home(&path, &home);
        let refresh_lock = store.lock_refresh().unwrap();

        assert!(store
            .load_while_refresh_locked(&refresh_lock)
            .unwrap()
            .is_none());
        assert!(!path.exists());
        assert!(!store.migration_marker_path().exists());
    }

    #[test]
    fn concurrent_first_loads_share_one_migrated_copy() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let (auth_path, source) = write_legacy_codex(&home);
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = std::sync::Arc::new(CredentialStore::with_legacy_home(&path, &home));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                store.load().unwrap().unwrap()
            }));
        }
        barrier.wait();
        for worker in workers {
            let credential = worker.join().unwrap();
            assert_eq!(credential.tokens.account_id, "acct");
        }
        assert_eq!(std::fs::read(auth_path).unwrap(), source);
        assert!(path.exists());
        assert!(store.migration_marker_path().exists());
    }

    #[test]
    fn legacy_jwt_decoder_accepts_padded_base64url_and_rejects_malformed_input() {
        use base64::engine::general_purpose::URL_SAFE;

        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct_from_jwt" },
            "exp": 4_102_444_800_u64,
        });
        let mut claim_bytes = serde_json::to_vec(&claims).unwrap();
        if claim_bytes.len() % 3 == 0 {
            claim_bytes.push(b' ');
        }
        let payload = URL_SAFE.encode(claim_bytes);
        assert!(payload.ends_with('='), "test payload must exercise padding");
        let credential = credential_from_parts(
            Some(format!("header.{payload}.signature")),
            Some("refresh".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(credential.tokens.account_id, "acct_from_jwt");
        assert_eq!(credential.expires_at, 4_102_444_800);

        for malformed in [
            "not-a-jwt".to_owned(),
            "header.%%%.signature".to_owned(),
            "header.e30.signature.extra".to_owned(),
            format!(
                "header.{}.signature",
                "A".repeat(MAX_JWT_PAYLOAD_BYTES.saturating_mul(2) + 1)
            ),
        ] {
            assert!(
                credential_from_parts(Some(malformed), Some("refresh".into()), None, None,)
                    .is_none()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_symlinked_sources_and_markers_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let source_target = dir.path().join("legacy-target");
        std::fs::write(&source_target, b"legacy target unchanged").unwrap();
        let source_path = home.join(".codex/auth.json");
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        symlink(&source_target, &source_path).unwrap();
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = CredentialStore::with_legacy_home(&path, &home);

        assert!(store.load().is_err());
        assert_eq!(
            std::fs::read(&source_target).unwrap(),
            b"legacy target unchanged"
        );
        assert!(!path.exists());

        std::fs::remove_file(source_path).unwrap();
        write_legacy_codex(&home);
        prepare_private_parent(&store.migration_marker_path()).unwrap();
        let marker_target = dir.path().join("marker-target");
        std::fs::write(&marker_target, b"marker target unchanged").unwrap();
        symlink(&marker_target, store.migration_marker_path()).unwrap();

        assert!(store.load().is_err());
        assert_eq!(
            std::fs::read(marker_target).unwrap(),
            b"marker target unchanged"
        );
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn logout_rejects_a_symlinked_marker_without_touching_private_state() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let path = dir.path().join("ygg/credentials/codex.json");
        let store = CredentialStore::with_legacy_home(&path, &home);
        store.save(&sample()).unwrap();
        let marker_target = dir.path().join("marker-target");
        std::fs::write(&marker_target, b"unchanged").unwrap();
        symlink(&marker_target, store.migration_marker_path()).unwrap();

        assert!(store.delete().is_err());
        assert_eq!(std::fs::read(&marker_target).unwrap(), b"unchanged");
        assert!(std::fs::symlink_metadata(store.migration_marker_path())
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(store.load_ygg_credential().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_lock_rejects_a_symlinked_credential_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/codex.json");
        let store = CredentialStore::new(path);
        let target = dir.path().join("unrelated");
        std::fs::create_dir(&target).unwrap();
        let sentinel = target.join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        symlink(&target, store.refresh_lock_directory().unwrap()).unwrap();

        assert!(store.lock_refresh().is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
    }
}
