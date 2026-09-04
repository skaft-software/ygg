#![allow(missing_docs)]

//! File-backed custom endpoint store at `~/.ygg/credentials/custom.json`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ygg_ai::CacheCompatibility;

const MAX_CREDENTIAL_BYTES: usize = 1024 * 1024;
const MAX_MODEL_CACHE_BYTES: usize = 8 * 1024 * 1024;
const REGISTRY_VERSION: u8 = 1;
const LEGACY_PROVIDER_ID: &str = "custom-openai";

fn cache_modified_is_stale(modified: std::time::SystemTime, max_age: std::time::Duration) -> bool {
    modified.elapsed().map_or(true, |age| age >= max_age)
}

fn cache_path_component(provider_id: &str) -> String {
    // File-name sanitization is lossy (`a/b` and `a:b` would both become
    // `a_b`). Bind every non-legacy cache name to the complete source ID.
    let readable = provider_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect::<String>();
    let readable = if readable.is_empty() {
        "provider"
    } else {
        &readable
    };
    let digest = Sha256::digest(provider_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{readable}-{digest}")
}

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

/// Authentication configuration for a registry provider.
///
/// Static API keys remain accepted in [`CustomCredential`] for compatibility,
/// but new configurations should reference an environment variable instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CustomAuthConfig {
    /// Do not send an Authorization header.
    None,
    /// Read a bearer token from an environment variable at runtime.
    BearerEnv {
        /// Environment variable containing the bearer token.
        var: String,
    },
}

/// One named provider in the custom endpoint registry.
#[derive(Clone, Serialize, Deserialize)]
pub struct CustomProvider {
    /// Human-facing provider name used in the model picker and status output.
    #[serde(default)]
    pub label: String,
    /// Core OpenAI-compatible endpoint configuration.
    #[serde(flatten)]
    pub credential: CustomCredential,
    /// Optional authentication strategy. When omitted, the compatibility
    /// `api_key` field is used if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<CustomAuthConfig>,
    /// Optional provider-specific environment variable for a bearer token.
    /// This is a compact alternative to the `auth` object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Optional prompt-cache compatibility controls for this provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheCompatibility>,
    /// Maximum time to wait for initial response headers from a cold endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
    /// Opt into the bounded HTTP/SSE lifecycle feedback extension for this
    /// OpenAI-compatible endpoint. Ordinary endpoint behavior is unchanged
    /// unless this is explicitly enabled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lifecycle_feedback: bool,
}

impl CustomProvider {
    /// Wrap a compatibility credential as a named provider configuration.
    #[cfg(test)]
    pub fn from_credential(credential: CustomCredential) -> Self {
        Self {
            label: String::new(),
            credential,
            auth: None,
            api_key_env: None,
            cache: None,
            startup_timeout_secs: None,
            lifecycle_feedback: false,
        }
    }
}

impl fmt::Debug for CustomProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustomProvider")
            .field("label", &self.label)
            .field("credential", &self.credential)
            .field("auth", &self.auth)
            .field("api_key_env", &self.api_key_env)
            .field("cache", &self.cache)
            .field("startup_timeout_secs", &self.startup_timeout_secs)
            .field("lifecycle_feedback", &self.lifecycle_feedback)
            .finish()
    }
}

/// Unified multi-provider custom endpoint configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomRegistry {
    /// Schema version of this registry.
    #[serde(default = "default_registry_version")]
    pub version: u8,
    /// Providers keyed by stable machine-readable IDs.
    pub providers: BTreeMap<String, CustomProvider>,
    /// True only when this registry was normalized from the original
    /// single-object credential shape. It is never serialized.
    #[serde(skip)]
    pub legacy_single_endpoint: bool,
}

impl CustomRegistry {
    /// Construct a registry containing one provider.
    pub fn single(provider_id: impl Into<String>, provider: CustomProvider) -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(provider_id.into(), provider);
        Self {
            version: REGISTRY_VERSION,
            providers,
            legacy_single_endpoint: false,
        }
    }
}

const fn default_registry_version() -> u8 {
    REGISTRY_VERSION
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
    /// Whether ygg can configure reasoning. A model may still reason by
    /// default when this is false.
    #[serde(default = "default_true")]
    pub reasoning_configurable: bool,
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
    /// Optional user-declared pricing. When absent, ygg treats the model as
    /// free (zero rates), which still satisfies guardrails that require
    /// trusted pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<CustomPricing>,
}

/// Explicit per-token pricing for one custom model.
///
/// Rates are microdollars per million tokens. Omitted fields default to zero,
/// so `"pricing": {}` declares the model free. Custom endpoints are
/// user-configured and therefore user-trusted: declaring any pricing
/// (including the zero default ygg applies) enables cost guardrails such as
/// subagent cost ceilings that require trusted model pricing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomPricing {
    /// Rate for prompt input tokens.
    #[serde(default)]
    pub input: u64,
    /// Rate for generated output tokens.
    #[serde(default)]
    pub output: u64,
    /// Rate for cached input tokens that were read.
    #[serde(default)]
    pub cache_read: u64,
    /// Rate for input tokens that caused a prompt-cache write.
    #[serde(default)]
    pub cache_write_5m: u64,
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
            reasoning_configurable: true,
            reasoning_values: Vec::new(),
            reasoning_default: String::new(),
            reasoning_uses_system_message: false,
            pricing: None,
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

/// One registry read bound to the exact private bytes that produced it.
///
/// The raw snapshot never crosses the custom credential boundary: setup uses it
/// only as an optimistic-concurrency token for a final private atomic write.
pub(crate) struct RegistrySnapshot {
    registry: Option<CustomRegistry>,
    bytes: Option<Vec<u8>>,
}

impl fmt::Debug for RegistrySnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistrySnapshot")
            .field("configured", &self.registry.is_some())
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

impl RegistrySnapshot {
    pub(crate) fn registry(&self) -> Option<&CustomRegistry> {
        self.registry.as_ref()
    }

    fn expected_bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }
}

/// Result of a compare-and-swap registry publication.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RegistryCommitError {
    /// Another writer changed the registry after setup read its snapshot.
    #[error("custom provider registry changed while setup was in progress")]
    Changed,
    /// The owner-private registry could not be published.
    #[error("could not save custom provider registry")]
    Storage,
}

impl CredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn model_cache_path_for(&self, provider_id: &str) -> PathBuf {
        let stem = self
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("custom");
        if provider_id == LEGACY_PROVIDER_ID {
            self.path.with_file_name(format!("{stem}-models.json"))
        } else {
            let component = cache_path_component(provider_id);
            self.path
                .with_file_name(format!("{stem}-models-{component}.json"))
        }
    }

    fn model_cache_path(&self) -> PathBuf {
        self.model_cache_path_for(LEGACY_PROVIDER_ID)
    }

    /// Load the unified provider registry, or `None` if the file does not exist.
    ///
    /// The original single-provider object is normalized in memory as the
    /// `custom-openai` provider. It is never treated as a separate runtime
    /// abstraction.
    pub fn load_registry(&self) -> Result<Option<CustomRegistry>> {
        Ok(self.load_registry_snapshot()?.registry)
    }

    /// Load a registry with the private byte snapshot needed for an atomic
    /// read/validate/merge/publish setup transaction.
    pub(crate) fn load_registry_snapshot(&self) -> Result<RegistrySnapshot> {
        let bytes = crate::auth::read_bounded_private(&self.path, MAX_CREDENTIAL_BYTES)
            .with_context(|| format!("reading {}", self.path.display()))?;
        let registry = bytes
            .as_deref()
            .map(|bytes| parse_registry(bytes, &self.path))
            .transpose()?;
        Ok(RegistrySnapshot { registry, bytes })
    }

    /// Publish a complete registry only if it still matches a previously read
    /// snapshot. Callers use `Changed` to offer an explicit reload/merge path;
    /// they must never silently overwrite another setup operation.
    pub(crate) fn save_registry_if_unchanged(
        &self,
        snapshot: &RegistrySnapshot,
        registry: &CustomRegistry,
    ) -> std::result::Result<(), RegistryCommitError> {
        if registry.version != REGISTRY_VERSION {
            return Err(RegistryCommitError::Storage);
        }
        let bytes =
            serde_json::to_vec_pretty(registry).map_err(|_| RegistryCommitError::Storage)?;
        match ygg_agent::secure_fs::write_private_atomic_if_unchanged(
            &self.path,
            snapshot.expected_bytes(),
            &bytes,
            MAX_CREDENTIAL_BYTES,
        ) {
            Ok(()) => Ok(()),
            Err(ygg_agent::secure_fs::SecureFileError::Changed) => {
                Err(RegistryCommitError::Changed)
            }
            Err(_) => Err(RegistryCommitError::Storage),
        }
    }

    /// Private path used only in receipts and catalog rebuilding. No caller can
    /// use it to bypass the store's owner-only persistence operations.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Compatibility accessor for callers that only support one provider.
    #[cfg(test)]
    pub fn load(&self) -> Result<Option<CustomCredential>> {
        let Some(registry) = self.load_registry()? else {
            return Ok(None);
        };
        if registry.providers.len() > 1 {
            anyhow::bail!(
                "{} contains multiple custom providers; use load_registry",
                self.path.display()
            );
        }
        Ok(registry
            .providers
            .into_values()
            .next()
            .map(|provider| provider.credential))
    }

    /// Load cached model metadata produced by a successful custom-endpoint
    /// discovery for one provider. The cache is deliberately separate from the
    /// credential so startup never rewrites or exposes the secret-bearing file.
    pub(crate) fn load_model_cache_for(&self, provider_id: &str) -> Result<Option<Vec<u8>>> {
        let path = self.model_cache_path_for(provider_id);
        crate::auth::read_bounded_private(&path, MAX_MODEL_CACHE_BYTES)
            .with_context(|| format!("reading {}", path.display()))
    }

    /// Compatibility cache accessor for the original single endpoint.
    #[cfg(test)]
    pub(crate) fn load_model_cache(&self) -> Result<Option<Vec<u8>>> {
        self.load_model_cache_for(LEGACY_PROVIDER_ID)
    }

    /// Persist discovered model metadata with the same owner-only guarantees as
    /// the credential itself.
    pub(crate) fn save_model_cache_for(&self, provider_id: &str, bytes: &[u8]) -> Result<()> {
        let path = self.model_cache_path_for(provider_id);
        write_private(&path, bytes).with_context(|| format!("writing {}", path.display()))
    }

    /// Compatibility cache accessor for the original single endpoint.
    #[cfg(test)]
    pub(crate) fn save_model_cache(&self, bytes: &[u8]) -> Result<()> {
        self.save_model_cache_for(LEGACY_PROVIDER_ID, bytes)
    }

    pub(crate) fn model_cache_is_stale_for(
        &self,
        provider_id: &str,
        max_age: std::time::Duration,
    ) -> Result<bool> {
        let path = self.model_cache_path_for(provider_id);
        let modified = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.modified()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Ok(cache_modified_is_stale(modified, max_age))
    }

    /// Load prompt-cache compatibility controls for the sole configured
    /// provider. This compatibility accessor is retained for existing callers;
    /// registry-aware code reads the provider's `cache` field directly.
    #[cfg(test)]
    pub fn load_cache_compatibility(&self) -> Result<Option<CacheCompatibility>> {
        let Some(registry) = self.load_registry()? else {
            return Ok(None);
        };
        if registry.providers.len() != 1 {
            return Ok(None);
        }
        Ok(registry
            .providers
            .values()
            .next()
            .and_then(|provider| provider.cache.clone()))
    }

    /// Load the response-header allowance for the sole configured provider.
    #[cfg(test)]
    pub fn load_startup_timeout_secs(&self) -> Result<Option<u64>> {
        let Some(registry) = self.load_registry()? else {
            return Ok(None);
        };
        if registry.providers.len() != 1 {
            return Ok(None);
        }
        Ok(registry
            .providers
            .values()
            .next()
            .and_then(|provider| provider.startup_timeout_secs))
    }

    /// Persist a compatibility credential as a one-provider registry.
    #[cfg(test)]
    pub fn save(&self, cred: &CustomCredential) -> Result<()> {
        let mut provider = CustomProvider::from_credential(cred.clone());
        if let Ok(Some(existing)) = self.load_registry() {
            if existing.providers.len() > 1 {
                anyhow::bail!(
                    "{} contains multiple custom providers; use save_registry",
                    self.path.display()
                );
            }
            if let Some(previous) = existing.providers.values().next() {
                provider.label = previous.label.clone();
                provider.auth = previous.auth.clone();
                provider.api_key_env = previous.api_key_env.clone();
                provider.cache = previous.cache.clone();
                provider.startup_timeout_secs = previous.startup_timeout_secs;
                provider.lifecycle_feedback = previous.lifecycle_feedback;
            }
        }
        self.save_registry(&CustomRegistry::single(LEGACY_PROVIDER_ID, provider))
    }

    /// Persist the complete provider registry with owner-only permissions.
    pub fn save_registry(&self, registry: &CustomRegistry) -> Result<()> {
        if registry.version != REGISTRY_VERSION {
            anyhow::bail!(
                "unsupported custom provider registry version {}",
                registry.version
            );
        }
        let bytes = serde_json::to_vec_pretty(registry)?;
        write_private(&self.path, &bytes)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    pub fn delete(&self) -> Result<()> {
        remove_if_present(&self.path)?;
        remove_if_present(&self.model_cache_path())?;
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        let stem = self
            .path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("custom");
        let prefix = format!("{stem}-models-");
        let entries = match std::fs::read_dir(parent) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", parent.display()))
            }
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".json"))
            {
                remove_if_present(&entry.path())?;
            }
        }
        Ok(())
    }
}

fn parse_registry(bytes: &[u8], path: &Path) -> Result<CustomRegistry> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .with_context(|| format!("corrupt credential file {}", path.display()))?;

    if value.get("providers").is_some() {
        let registry: CustomRegistry = serde_json::from_value(value)
            .with_context(|| format!("invalid custom provider registry {}", path.display()))?;
        if registry.version != REGISTRY_VERSION {
            anyhow::bail!(
                "unsupported custom provider registry version {} in {}",
                registry.version,
                path.display()
            );
        }
        Ok(registry)
    } else {
        let credential: CustomCredential = serde_json::from_value(value.clone())
            .with_context(|| format!("invalid legacy custom credential {}", path.display()))?;
        let cache = value
            .get("cache")
            .map(|cache| {
                serde_json::from_value(cache.clone())
                    .with_context(|| format!("invalid cache compatibility in {}", path.display()))
            })
            .transpose()?;
        let startup_timeout_secs = value
            .get("startup_timeout_secs")
            .map(|timeout| {
                serde_json::from_value(timeout.clone())
                    .with_context(|| format!("invalid startup timeout in {}", path.display()))
            })
            .transpose()?;
        let lifecycle_feedback = value
            .get("lifecycle_feedback")
            .map(|enabled| {
                serde_json::from_value(enabled.clone()).with_context(|| {
                    format!("invalid lifecycle feedback setting in {}", path.display())
                })
            })
            .transpose()?
            .unwrap_or(false);
        let mut registry = CustomRegistry::single(
            LEGACY_PROVIDER_ID,
            CustomProvider {
                label: String::new(),
                credential,
                auth: None,
                api_key_env: None,
                cache,
                startup_timeout_secs,
                lifecycle_feedback,
            },
        );
        registry.legacy_single_endpoint = true;
        Ok(registry)
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
    ygg_agent::secure_fs::write_private_atomic(path, bytes, MAX_MODEL_CACHE_BYTES)
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_private_fixture(path: &Path, bytes: impl AsRef<[u8]>) {
        ygg_agent::secure_fs::write_private_atomic(path, bytes.as_ref(), 64 * 1024).unwrap();
    }

    #[test]
    fn provider_cache_components_do_not_collide_and_future_entries_are_stale() {
        assert_ne!(
            cache_path_component("provider/a"),
            cache_path_component("provider:a")
        );
        assert!(cache_modified_is_stale(
            std::time::SystemTime::now() + std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(1),
        ));
    }

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
    fn model_pricing_round_trips_and_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        let store = CredentialStore::new(&path);
        let mut provider = CustomProvider::from_credential(CustomCredential {
            base_url: "http://localhost:1234/v1/".into(),
            api_key: String::new(),
            api_name: String::new(),
            headers: Vec::new(),
            models: vec![CustomModel {
                api_name: "free-model".into(),
                ..Default::default()
            }],
            auto_discover: false,
        });
        // Declared pricing serializes and deserializes verbatim.
        provider.credential.models[0].pricing = Some(CustomPricing {
            input: 10,
            output: 20,
            cache_read: 1,
            cache_write_5m: 2,
        });
        store
            .save_registry(&CustomRegistry::single("local", provider.clone()))
            .unwrap();
        let loaded = store.load_registry().unwrap().unwrap();
        assert_eq!(
            loaded.providers["local"].credential.models[0].pricing,
            Some(CustomPricing {
                input: 10,
                output: 20,
                cache_read: 1,
                cache_write_5m: 2,
            })
        );

        // Undeclared pricing stays absent in the file and after a reload.
        provider.credential.models[0].pricing = None;
        store
            .save_registry(&CustomRegistry::single("local", provider))
            .unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("pricing"),
            "undeclared pricing must not be serialized: {raw}"
        );

        // A pre-existing file without the pricing field loads as `None`.
        let legacy = r#"{
            "version": 1,
            "providers": {
                "legacy": {
                    "label": "Legacy",
                    "base_url": "http://localhost:1234/v1/",
                    "auth": { "kind": "none" },
                    "auto_discover": false,
                    "models": [
                        { "api_name": "old-model", "context_window": 8192 }
                    ]
                }
            }
        }"#;
        write_private_fixture(&path, legacy);
        let loaded = store.load_registry().unwrap().unwrap();
        assert_eq!(
            loaded.providers["legacy"].credential.models[0].pricing,
            None
        );
    }

    #[test]
    fn unified_registry_round_trips_labels_and_auth_without_secret_debug_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        let store = CredentialStore::new(&path);
        let provider =
            |label: &str, base_url: &str, auth: Option<CustomAuthConfig>| CustomProvider {
                label: label.into(),
                credential: CustomCredential {
                    base_url: base_url.into(),
                    api_key: String::new(),
                    api_name: String::new(),
                    headers: Vec::new(),
                    models: vec![CustomModel {
                        api_name: "shared-model".into(),
                        ..Default::default()
                    }],
                    auto_discover: false,
                },
                auth,
                api_key_env: None,
                cache: None,
                startup_timeout_secs: Some(420),
                lifecycle_feedback: false,
            };
        let mut registry = CustomRegistry::single(
            "apple-fm",
            provider(
                "Apple Foundation Models",
                "http://127.0.0.1:1976/v1/",
                Some(CustomAuthConfig::None),
            ),
        );
        registry.providers.insert(
            "home-server".into(),
            provider(
                "Home Server",
                "http://127.0.0.1:8000/v1/",
                Some(CustomAuthConfig::BearerEnv {
                    var: "HOME_SERVER_API_KEY".into(),
                }),
            ),
        );
        registry
            .providers
            .get_mut("apple-fm")
            .unwrap()
            .credential
            .api_key = "sk-apple-secret".into();
        store.save_registry(&registry).unwrap();

        let loaded = store.load_registry().unwrap().unwrap();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(
            loaded.providers["apple-fm"].label,
            "Apple Foundation Models"
        );
        assert!(matches!(
            loaded.providers["home-server"].auth,
            Some(CustomAuthConfig::BearerEnv { ref var }) if var == "HOME_SERVER_API_KEY"
        ));
        assert!(!format!("{loaded:?}").contains("sk-apple-secret"));
        assert!(
            store.load().is_err(),
            "single-credential access must reject a registry"
        );
    }

    #[test]
    fn legacy_file_normalizes_to_one_provider_and_preserves_legacy_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        write_private_fixture(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "api_name": "legacy-model",
                "cache": { "supports_long_retention": false }
            }"#,
        );

        let registry = CredentialStore::new(&path)
            .load_registry()
            .unwrap()
            .unwrap();
        assert!(registry.legacy_single_endpoint);
        assert_eq!(registry.providers.len(), 1);
        assert!(registry.providers.contains_key(LEGACY_PROVIDER_ID));
        assert_eq!(
            registry.providers[LEGACY_PROVIDER_ID].credential.api_name,
            "legacy-model"
        );
        assert!(
            !registry.providers[LEGACY_PROVIDER_ID]
                .cache
                .as_ref()
                .unwrap()
                .supports_long_retention
        );
    }

    #[test]
    fn provider_model_caches_are_isolated_and_deleted_together() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(dir.path().join("credentials/custom.json"));
        store
            .save_model_cache_for("apple-fm", br#"{"provider":"apple"}"#)
            .unwrap();
        store
            .save_model_cache_for("home-server", br#"{"provider":"home"}"#)
            .unwrap();
        assert_eq!(
            store.load_model_cache_for("apple-fm").unwrap().unwrap(),
            br#"{"provider":"apple"}"#
        );
        assert_eq!(
            store.load_model_cache_for("home-server").unwrap().unwrap(),
            br#"{"provider":"home"}"#
        );
        assert_ne!(
            store.model_cache_path_for("apple-fm"),
            store.model_cache_path_for("home-server")
        );
        store.delete().unwrap();
        assert!(store.load_model_cache_for("apple-fm").unwrap().is_none());
        assert!(store.load_model_cache_for("home-server").unwrap().is_none());
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
        write_private_fixture(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "cache": {
                    "cache_control_format": "anthropic",
                    "send_session_affinity_headers": true,
                    "supports_long_retention": false
                }
            }"#,
        );

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
        write_private_fixture(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "startup_timeout_secs": 420
            }"#,
        );

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
    fn lifecycle_feedback_defaults_false_and_is_preserved_for_legacy_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials/custom.json");
        write_private_fixture(
            &path,
            r#"{
                "base_url": "http://localhost:1234/v1/",
                "lifecycle_feedback": true
            }"#,
        );

        let store = CredentialStore::new(&path);
        let registry = store.load_registry().unwrap().unwrap();
        assert!(registry.providers[LEGACY_PROVIDER_ID].lifecycle_feedback);

        let credential = CustomCredential {
            base_url: "http://localhost:5678/v1/".to_string(),
            api_key: String::new(),
            api_name: "local".to_string(),
            headers: vec![],
            models: vec![],
            auto_discover: false,
        };
        store.save(&credential).unwrap();
        let persisted = std::fs::read_to_string(&path).unwrap();
        assert!(persisted.contains("\"lifecycle_feedback\": true"));

        let defaulted: CustomProvider =
            serde_json::from_str(r#"{"base_url":"http://localhost:1234/v1/"}"#).unwrap();
        assert!(!defaulted.lifecycle_feedback);
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
