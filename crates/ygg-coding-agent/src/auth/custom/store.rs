#![allow(missing_docs)]

//! File-backed custom endpoint store at `~/.ygg/credentials/custom.json`.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use ygg_ai::CacheCompatibility;

const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_MODEL_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Configuration for one custom OpenAI-compatible endpoint.
#[derive(Clone, Serialize, Deserialize)]
pub struct CustomCredential {
    /// Base URL of the endpoint (must end with `/`).
    pub base_url: String,
    /// Bearer token or API key, if any. Empty string means no auth.
    #[serde(default)]
    pub api_key: String,
    /// The on-wire model name to use (single-model legacy format).
    /// When `models` is present, this is ignored.
    #[serde(default)]
    pub api_name: String,
    /// Extra static headers to send with every request.
    #[serde(default)]
    pub headers: Vec<HeaderEntry>,
    /// Multi-model configuration. When present, supersedes `api_name`.
    #[serde(default)]
    pub models: Vec<CustomModel>,
    /// When true (the default), ygg calls GET /v1/models on the endpoint at
    /// startup to discover models. Set to false only when an endpoint cannot
    /// provide a usable `/v1/models`.
    #[serde(default = "default_auto_discover")]
    pub auto_discover: bool,
}

/// A single model served by the custom endpoint.
#[derive(Clone, Serialize, Deserialize)]
pub struct CustomModel {
    /// The on-wire model name (sent as `model` in API requests).
    pub api_name: String,
    /// Optional display name in ygg's model picker. Defaults to api_name.
    #[serde(default)]
    pub display_name: String,
    /// Context window size in tokens.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Maximum output tokens.
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u64,
    /// Whether the model supports tools/function calling.
    #[serde(default = "default_true")]
    pub tools: bool,
    /// Whether the model supports parallel tool calls.
    #[serde(default)]
    pub parallel_tool_calls: bool,
    /// Whether the model supports vision/image inputs.
    #[serde(default)]
    pub vision: bool,
    /// Whether the model supports structured output (JSON schema/mode).
    #[serde(default)]
    pub structured_output: bool,
    /// Whether the model supports reasoning/thinking.
    #[serde(default)]
    pub reasoning: bool,
    /// Exact reasoning selector values advertised by the endpoint. Empty
    /// preserves the legacy effort-range behavior for manual configurations.
    #[serde(default)]
    pub reasoning_values: Vec<String>,
    /// Endpoint-advertised default reasoning selector value.
    #[serde(default)]
    pub reasoning_default: String,
    /// Whether reasoning-capable requests must keep the system prompt as a
    /// `system` message instead of using OpenAI's `developer` role.
    #[serde(default)]
    pub reasoning_uses_system_message: bool,
}

const fn default_auto_discover() -> bool {
    true
}

const fn default_context_window() -> u64 {
    131_072
}

const fn default_max_output_tokens() -> u64 {
    16_384
}

const fn default_true() -> bool {
    true
}

impl Default for CustomModel {
    fn default() -> Self {
        Self {
            api_name: String::new(),
            display_name: String::new(),
            context_window: default_context_window(),
            max_output_tokens: default_max_output_tokens(),
            tools: true,
            parallel_tool_calls: false,
            vision: false,
            structured_output: false,
            reasoning: false,
            reasoning_values: Vec::new(),
            reasoning_default: String::new(),
            reasoning_uses_system_message: false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HeaderEntry {
    pub name: String,
    pub value: String,
}

impl fmt::Debug for CustomCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomCredential")
            .field("base_url", &self.base_url)
            .field("api_key", &"[REDACTED]")
            .field("api_name", &self.api_name)
            .field(
                "headers",
                &self.headers.iter().map(|h| &h.name).collect::<Vec<_>>(),
            )
            .field("models", &self.models.len())
            .field("auto_discover", &self.auto_discover)
            .finish()
    }
}

/// Default store path: `~/.ygg/credentials/custom.json`.
pub fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("credentials")
        .join("custom.json")
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
            .unwrap_or("custom");
        self.path.with_file_name(format!("{stem}-models.json"))
    }

    /// Load the credential, or `None` if the file does not exist.
    pub fn load(&self) -> Result<Option<CustomCredential>> {
        let Some(bytes) = crate::auth::read_bounded_regular(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?
        else {
            return Ok(None);
        };
        let cred = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt credential file {}", self.path.display()))?;
        Ok(Some(cred))
    }

    /// Load cached model metadata produced by a successful custom-endpoint
    /// discovery. The cache is deliberately separate from the credential so a
    /// routine startup never has to rewrite or expose the secret-bearing file.
    pub(crate) fn load_model_cache(&self) -> Result<Option<Vec<u8>>> {
        let path = self.model_cache_path();
        crate::auth::read_bounded_regular(&path, MAX_MODEL_CACHE_BYTES)
            .with_context(|| format!("reading {}", path.display()))
    }

    /// Persist discovered model metadata with the same owner-only guarantees as
    /// the credential itself.
    pub(crate) fn save_model_cache(&self, bytes: &[u8]) -> Result<()> {
        let path = self.model_cache_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restricting {}", parent.display()))?;
            }
        }
        write_private(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    pub(crate) fn model_cache_is_stale(&self, max_age: std::time::Duration) -> Result<bool> {
        let path = self.model_cache_path();
        let modified = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(modified.elapsed().is_ok_and(|age| age >= max_age))
    }

    /// Load optional prompt-cache compatibility controls from the credential
    /// JSON's top-level `cache` object.
    ///
    /// This is intentionally separate from [`CustomCredential`] so existing
    /// callers using struct literals remain source-compatible. The object uses
    /// ygg-ai's `CacheCompatibility` shape, for example:
    /// `{ "cache": { "cache_control_format": "anthropic", "send_session_affinity_headers": true } }`.
    pub fn load_cache_compatibility(&self) -> Result<Option<CacheCompatibility>> {
        let Some(bytes) = crate::auth::read_bounded_regular(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?
        else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt credential file {}", self.path.display()))?;
        let Some(cache) = value.get("cache") else {
            return Ok(None);
        };
        serde_json::from_value(cache.clone())
            .map(Some)
            .with_context(|| format!("invalid cache compatibility in {}", self.path.display()))
    }

    /// Load the optional response-header allowance for a cold-starting custom
    /// endpoint. This expert setting stays outside [`CustomCredential`] so the
    /// public credential shape remains source-compatible.
    pub fn load_startup_timeout_secs(&self) -> Result<Option<u64>> {
        let Some(bytes) = crate::auth::read_bounded_regular(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?
        else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("corrupt credential file {}", self.path.display()))?;
        let Some(timeout) = value.get("startup_timeout_secs") else {
            return Ok(None);
        };
        serde_json::from_value(timeout.clone())
            .map(Some)
            .with_context(|| format!("invalid startup timeout in {}", self.path.display()))
    }

    /// Persist a credential with owner-only permissions.
    pub fn save(&self, cred: &CustomCredential) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("restricting {}", parent.display()))?;
            }
        }
        let mut value = serde_json::to_value(cred)?;
        // Expert compatibility extensions stay outside `CustomCredential` for
        // source compatibility. Preserve them when the endpoint is re-saved
        // through the normal login/configuration flow.
        if let Ok(Some(existing)) =
            crate::auth::read_bounded_regular(&self.path, MAX_CREDENTIAL_BYTES)
        {
            if let Ok(existing) = serde_json::from_slice::<serde_json::Value>(&existing) {
                for extension in ["cache", "startup_timeout_secs"] {
                    if let Some(extension_value) = existing.get(extension) {
                        if let Some(object) = value.as_object_mut() {
                            object.insert(extension.to_string(), extension_value.clone());
                        }
                    }
                }
            }
        }
        let bytes = serde_json::to_vec_pretty(&value)?;
        write_private(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        remove_if_present(&self.path)?;
        remove_if_present(&self.model_cache_path())
    }
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
        .prefix(".custom-credential-")
        .tempfile_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut temporary, bytes)?;
    std::io::Write::flush(&mut temporary)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;

    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        let store = CredentialStore::new(&path);
        assert!(store.load().unwrap().is_none());

        let cred = CustomCredential {
            base_url: "http://localhost:1234/v1/".into(),
            api_key: "sk-test".into(),
            api_name: "llama-3.1-8b".into(),
            headers: vec![HeaderEntry {
                name: "CF-Access-Client-Id".into(),
                value: "xxx".into(),
            }],
            models: Vec::new(),
            auto_discover: true,
        };
        store.save(&cred).unwrap();
        assert!(path.exists());

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.base_url, "http://localhost:1234/v1/");
        assert_eq!(loaded.api_key, "sk-test");
        assert_eq!(loaded.api_name, "llama-3.1-8b");
        assert_eq!(loaded.headers.len(), 1);
        assert_eq!(loaded.headers[0].name, "CF-Access-Client-Id");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        store.delete().unwrap();
        assert!(!path.exists());
        store.delete().unwrap(); // idempotent
    }

    #[test]
    fn model_cache_is_private_bounded_and_deleted_with_credential() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        let store = CredentialStore::new(&path);
        let cred = CustomCredential {
            base_url: "http://localhost:1234/v1/".into(),
            api_key: String::new(),
            api_name: "model".into(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        };
        store.save(&cred).unwrap();
        store.save_model_cache(br#"{"version":1}"#).unwrap();
        assert_eq!(
            store.load_model_cache().unwrap().unwrap(),
            br#"{"version":1}"#
        );
        let cache_path = store.model_cache_path();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&cache_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.delete().unwrap();
        assert!(!path.exists());
        assert!(!cache_path.exists());
    }

    #[test]
    fn legacy_single_model_format_still_parses() {
        let json = r#"{
            "base_url": "http://localhost:1234/v1/",
            "api_key": "sk-legacy",
            "api_name": "llama-3.1-8b",
            "headers": [{"name": "X-Test", "value": "1"}]
        }"#;
        let cred: CustomCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.api_name, "llama-3.1-8b");
        assert!(cred.models.is_empty());
    }

    #[test]
    fn multi_model_format_parses() {
        let json = r#"{
            "base_url": "http://localhost:1234/v1/",
            "api_key": "",
            "api_name": "",
            "headers": [],
            "models": [
                {
                    "api_name": "model-a",
                    "display_name": "Model A",
                    "context_window": 262144,
                    "max_output_tokens": 16384,
                    "tools": true,
                    "parallel_tool_calls": false,
                    "vision": true,
                    "structured_output": false,
                    "reasoning": true
                },
                {
                    "api_name": "model-b",
                    "display_name": "",
                    "context_window": 131072,
                    "max_output_tokens": 8192,
                    "tools": false,
                    "parallel_tool_calls": false,
                    "vision": false,
                    "structured_output": false,
                    "reasoning": false
                }
            ]
        }"#;
        let cred: CustomCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.models.len(), 2);
        assert_eq!(cred.models[0].api_name, "model-a");
        assert_eq!(cred.models[0].display_name, "Model A");
        assert!(cred.models[0].vision);
        assert!(cred.models[0].reasoning);
        assert_eq!(cred.models[1].api_name, "model-b");
        assert_eq!(cred.models[1].display_name, ""); // defaults to api_name in registration
        assert!(!cred.models[1].tools);
    }

    #[test]
    fn custom_cache_compatibility_is_loaded_without_changing_legacy_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "cache": {
                    "cache_control_format": "anthropic",
                    "send_session_affinity_headers": true,
                    "supports_long_retention": false
                }
            }"#,
        )
        .unwrap();

        let cache = CredentialStore::new(&path)
            .load_cache_compatibility()
            .unwrap()
            .unwrap();
        assert_eq!(
            cache.cache_control_format,
            Some(ygg_ai::CacheControlFormat::Anthropic)
        );
        assert!(cache.send_session_affinity_headers);
        assert!(!cache.supports_long_retention);

        let credential = CustomCredential {
            base_url: "http://localhost:5678/v1/".to_string(),
            api_key: String::new(),
            api_name: "local".to_string(),
            headers: vec![],
            models: vec![],
            auto_discover: false,
        };
        let store = CredentialStore::new(path);
        store.save(&credential).unwrap();
        assert_eq!(
            store
                .load_cache_compatibility()
                .unwrap()
                .unwrap()
                .cache_control_format,
            Some(ygg_ai::CacheControlFormat::Anthropic)
        );
    }

    #[test]
    fn custom_startup_timeout_is_loaded_and_preserved_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "startup_timeout_secs": 420
            }"#,
        )
        .unwrap();

        let store = CredentialStore::new(&path);
        assert_eq!(store.load_startup_timeout_secs().unwrap(), Some(420));

        let credential = CustomCredential {
            base_url: "http://localhost:5678/v1/".to_string(),
            api_key: String::new(),
            api_name: "local".to_string(),
            headers: vec![],
            models: vec![],
            auto_discover: false,
        };
        store.save(&credential).unwrap();
        assert_eq!(store.load_startup_timeout_secs().unwrap(), Some(420));
    }

    #[test]
    fn multi_model_backward_compat_empty_models_uses_api_name() {
        let json = r#"{
            "base_url": "http://localhost:1234/v1/",
            "api_key": "",
            "api_name": "single-model",
            "headers": []
        }"#;
        let cred: CustomCredential = serde_json::from_str(json).unwrap();
        assert_eq!(cred.api_name, "single-model");
        assert!(cred.models.is_empty());
        // The registration code will see empty models + non-empty api_name
        // and wrap it in a single-element CustomModel vec (legacy path).
    }
}
