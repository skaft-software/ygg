#![allow(missing_docs)]

use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use sha2::{Digest as _, Sha256};
use ygg_agent::extension_runtime::ExtensionRuntimeManager;
use ygg_agent::secure_fs::{create_regular_file_for_append, open_regular_file_for_append};
use ygg_agent::{
    Agent, AgentCompactionMode, AgentConfig, CoreTools, DelegationConfig, DurableGoalStore,
    EffectBroker, EntryValue, ExtensionHost, GoalDriver, Session, SkillRegistry, TelemetryObserver,
};
use ygg_ai::{
    AgentDelegation, AiClient, Auth, CacheCompatibility, Capabilities, Endpoint, EndpointId,
    EndpointTransport, ModalitySet, Model, ModelCatalog, ModelId, ModelLimits, ModelSpec,
    OpenAiChatReasoningMode, Pricing, PricingTier, Protocol, ReasoningCapability, ReasoningConfig,
    ReasoningControl, ReasoningMode, RequestRuntime, TokenRate, ToolDef,
};

use crate::app::{
    level_from_reasoning, model_supports_ultra, normalize_reasoning_for_model,
    normalize_reasoning_selection_for_model_with_subagents, thinking_to_reasoning, App,
};
use crate::config::{CompactionMode, Config, ResumeSelector};
use crate::extensions::{ExecutableExtensions, ExtensionProviderRuntime, SUBAGENTS_EXTENSION_NAME};
use crate::modes::interactive::run_blocking_lifecycle;
use crate::prompts::PromptRegistry;
use crate::providers::{
    ModelDiscovery, ModelFilter, ProviderAuthentication, ProviderDeclaration, ProviderRoute,
    ProviderRuntimeConfiguration, BUILTIN_PROVIDER_DECLARATIONS,
};
use crate::resources::{format_skills_for_prompt, FileSystemSkillRegistry};
use crate::session_store::SessionStore;
use crate::tui::pickers::{model_picker, session_picker};
use crate::tui::view::InteractiveShell;

/// Inputs needed to resolve a launch without constructing an Agent or a TUI.
pub struct Bootstrap {
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub client: AiClient,
    provider_runtime: ExtensionProviderRuntime,
    /// Activated extension host retained while provider declarations are needed
    /// to resolve the user's first model selection.
    prestarted_extensions: RefCell<Option<(ExtensionHost, ExecutableExtensions)>>,
    /// Session opened while resolving resume provenance. Keeping it here
    /// avoids replaying the same JSONL file a second time in `build_app`.
    prepared_session: RefCell<Option<Session>>,
    /// Interactive startup can remain useful as a read-only session viewer
    /// when no configured model exists.
    modeless: std::cell::Cell<bool>,
}

impl Bootstrap {
    /// Starts enabled extensions once when their API 0.3 declarations are
    /// required to resolve an otherwise unknown initial model.
    ///
    /// The temporary session is deliberately not a user session: provider
    /// discovery must not create or mutate a session before launch validation
    /// succeeds. The started host is retained and rebound by `build_app`.
    fn preflight_extension_providers(&self) -> anyhow::Result<()> {
        if self.prestarted_extensions.borrow().is_some() {
            return Ok(());
        }

        let temporary = tempfile::tempdir()
            .context("could not create temporary extension-provider bootstrap session")?;
        let session = Session::create(temporary.path().join("provider-bootstrap.jsonl"))
            .context("could not create temporary extension-provider bootstrap session")?;
        let model = extension_provider_bootstrap_model(&self.catalog);
        let (host, mut extensions) = configured_extensions_with_runtime_manager(
            &self.config,
            &session,
            &model,
            &self.config.reasoning,
            &self.sessions,
            None,
            self.provider_runtime.clone(),
        )?;
        // Populate a throwaway copy now so callers can validate/select the
        // projected models. `build_app` repeats this against its owned catalog.
        let mut catalog = self.catalog.clone();
        extensions.synchronize_provider_catalog(&mut catalog, &self.client);
        *self.prestarted_extensions.borrow_mut() = Some((host, extensions));
        Ok(())
    }

    /// Returns a selection catalog that includes ready host-owned provider
    /// routes without exposing extension declarations as catalog authority.
    fn catalog_with_extension_providers(&self) -> ModelCatalog {
        let mut catalog = self.catalog.clone();
        if let Some((_, extensions)) = self.prestarted_extensions.borrow_mut().as_mut() {
            extensions.synchronize_provider_catalog(&mut catalog, &self.client);
        }
        catalog
    }

    /// Supply an already-open session for the next launch.
    ///
    /// Hosts use this to keep authorization bound to a caller-opened file
    /// descriptor instead of reopening the session by pathname in `build_app`.
    pub(crate) fn set_prepared_session(&mut self, session: Session) {
        *self.prepared_session.get_mut() = Some(session);
    }

    pub(crate) fn take_prepared_session(&self) -> Option<Session> {
        self.prepared_session.borrow_mut().take()
    }

    fn enter_modeless_mode(&self) {
        self.modeless.set(true);
    }

    pub(crate) fn is_modeless(&self) -> bool {
        self.modeless.get()
    }
}

/// Supplies extension initialization with a safe local model snapshot before a
/// provider declaration has made the user's requested model resolvable.
fn extension_provider_bootstrap_model(catalog: &ModelCatalog) -> Model {
    if let Some(model) = catalog
        .models()
        .next()
        .and_then(|specification| catalog.resolve(&specification.id).ok())
    {
        return model;
    }

    let endpoint = Arc::new(Endpoint {
        id: EndpointId("extension-provider-bootstrap".to_owned()),
        base_url: url::Url::parse("http://127.0.0.1:9/")
            .expect("fixed extension-provider bootstrap URL is valid"),
        auth: Auth::None,
        default_headers: http::HeaderMap::new(),
        transport: EndpointTransport::Http,
        runtime: RequestRuntime::default(),
        timeout: Duration::from_secs(30),
    });
    Model {
        spec: Arc::new(ModelSpec {
            id: ModelId("extension-provider-bootstrap".to_owned()),
            endpoint: endpoint.id.clone(),
            api_name: "extension-provider-bootstrap".to_owned(),
            display_name: None,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none(),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: false,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,
                deferred_tool_loading: false,
            },
            limits: ModelLimits {
                context_window: 128_000,
                max_output_tokens: 16_000,
            },
            pricing: None,
            cache: CacheCompatibility::default(),
        }),
        endpoint,
    }
}

/// Selected persistent session operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionSelection {
    OpenExisting(PathBuf),
    CreateNew(PathBuf),
}

/// Resolved model and session for one launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSelection {
    pub model: ModelId,
    pub session: SessionSelection,
    /// Effective reasoning restored from session state or invocation defaults.
    pub reasoning: ReasoningConfig,
    /// Effective execution mode restored independently from reasoning effort.
    pub reasoning_mode: ReasoningMode,
}

fn validate_compaction_route(
    mode: CompactionMode,
    active: &Model,
    compact_model: Option<&Model>,
) -> anyhow::Result<()> {
    if mode != CompactionMode::NativeResponses {
        return Ok(());
    }
    if active.spec.protocol != Protocol::OpenAiResponses {
        anyhow::bail!(
            "native Responses compaction requires an OpenAI Responses route; model {} uses {:?}",
            active.spec.id.0,
            active.spec.protocol
        );
    }
    if let Some(compact_model) = compact_model {
        if compact_model.endpoint.id != active.endpoint.id
            || compact_model.spec.id != active.spec.id
        {
            anyhow::bail!(
                "native Responses compaction requires exact route affinity; compaction.compact_model must match active endpoint/model {}/{}",
                active.endpoint.id.0,
                active.spec.id.0
            );
        }
    }
    Ok(())
}

fn validate_native_compaction_replay(
    mode: CompactionMode,
    session: &Session,
    model: &Model,
) -> anyhow::Result<()> {
    if mode != CompactionMode::NativeResponses {
        return Ok(());
    }
    match session.responses_replay_items(&model.endpoint.id, &model.spec.id)? {
        Some(_) => Ok(()),
        None => anyhow::bail!(
            "native Responses compaction requires complete route-affine opaque replay on the active branch"
        ),
    }
}

fn agent_compaction_mode(mode: CompactionMode) -> AgentCompactionMode {
    match mode {
        CompactionMode::Disabled => AgentCompactionMode::Disabled,
        CompactionMode::Local => AgentCompactionMode::Local,
        CompactionMode::NativeResponses => AgentCompactionMode::NativeResponses,
    }
}

const DEEPSEEK_MODEL_ID: &str = "deepseek-v4-pro";
const DEEPSEEK_DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
// Only a local capacity reserve; it never becomes an implicit request cap.
const DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 384_000;

#[cfg(test)]
const OPENCODE_ANTHROPIC_ENDPOINT_ID: &str = "opencode-anthropic";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
// A provider may spend minutes queueing or processing a large prompt before
// it emits response headers. Connection establishment remains separately
// bounded in ygg-ai; this phase needs a generous, cancellable allowance.
const PROVIDER_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
// Local servers may need to load a model before they can return response
// headers. Keep the same fifteen-minute default for custom endpoints while
// allowing each provider to override it for its own cold-start behavior.
const CUSTOM_ENDPOINT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const MAX_DISCOVERY_BODY_BYTES: usize = 8 * 1024 * 1024;
// Version 2 invalidated inventories whose llama.cpp context length was guessed
// because older discovery ignored hlid's nested `meta.n_ctx` field. Version 4
// invalidated sparse local inventories that were incorrectly cached as
// tool-incompatible by version 3. Version 5 invalidates v4 entries produced by
// the secondary hlid discovery path before it adopted the same tri-state
// local-tool fallback. Version 6 also scopes a cache entry to the configured
// model metadata, so removing or changing an override immediately re-runs
// discovery. Version 7 invalidates inventories created before the built-in
// Apple Foundation Models metadata was applied to sparse model responses.
// Version 8 gives PCC its distinct 32,768-token context window.
const CUSTOM_MODEL_CACHE_VERSION: u8 = 8;
const PROVIDER_INVENTORY_CACHE_VERSION: u8 = 1;
const MAX_PROVIDER_INVENTORY_CACHE_BYTES: usize = MAX_DISCOVERY_BODY_BYTES + 1024 * 1024;
const PROVIDER_INVENTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NEGATIVE_INVENTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

fn optional_env_value(
    _name: &str,
    value: Result<Option<String>, ygg_ai::ConfigError>,
) -> anyhow::Result<Option<String>> {
    match value {
        Ok(value) => Ok(value),
        // Preserve the existing optional-reader policy for invalid Unicode:
        // std::env::var(...).ok() treated it like an unavailable value.
        Err(ygg_ai::ConfigError::InvalidEnv(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn optional_env(name: &str) -> anyhow::Result<Option<String>> {
    optional_env_value(name, ygg_ai::auth::read_bounded_env(name))
}

#[cfg(test)]
fn required_env_value(
    name: &str,
    value: Result<Option<String>, ygg_ai::ConfigError>,
) -> anyhow::Result<String> {
    match value? {
        Some(value) => Ok(value),
        None => Err(anyhow::anyhow!("environment variable {name} not found")),
    }
}

fn strict_env_value(
    name: &str,
    value: Result<Option<String>, ygg_ai::ConfigError>,
) -> anyhow::Result<Option<String>> {
    match value {
        Ok(value) => Ok(value),
        Err(ygg_ai::ConfigError::InvalidEnv(_)) => Err(anyhow::anyhow!(
            "could not read {name}: invalid environment value"
        )),
        Err(error) => Err(error.into()),
    }
}

fn strict_env(name: &str) -> anyhow::Result<Option<String>> {
    strict_env_value(name, ygg_ai::auth::read_bounded_env(name))
}

fn blocking_discovery_client(timeout: Duration) -> reqwest::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn discovery_client(timeout: Duration) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

fn bounded_discovery_json(
    response: reqwest::blocking::Response,
    label: &str,
) -> anyhow::Result<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DISCOVERY_BODY_BYTES as u64)
    {
        anyhow::bail!(
            "{label} response exceeds the {}-byte limit",
            MAX_DISCOVERY_BODY_BYTES
        );
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_DISCOVERY_BODY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DISCOVERY_BODY_BYTES {
        anyhow::bail!(
            "{label} response exceeds the {}-byte limit",
            MAX_DISCOVERY_BODY_BYTES
        );
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid {label} response: {error}"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ProviderInventoryCache {
    version: u8,
    provider_id: String,
    inventory_url: String,
    credential_fingerprint: String,
    body: Option<serde_json::Value>,
}

enum CachedProviderInventory {
    Available(serde_json::Value),
    Unavailable,
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn credential_fingerprint(credential: &str) -> String {
    fingerprint_bytes(credential.as_bytes())
}

fn custom_model_cache_fingerprint(
    credential_fingerprint: &str,
    configured: &[crate::auth::custom::CustomModel],
) -> String {
    let configured =
        serde_json::to_vec(configured).expect("custom model metadata must always be serializable");
    let mut scoped = Vec::with_capacity(credential_fingerprint.len() + configured.len() + 40);
    scoped.extend_from_slice(b"ygg-custom-model-cache-config-v1");
    scoped.extend_from_slice(&(credential_fingerprint.len() as u64).to_be_bytes());
    scoped.extend_from_slice(credential_fingerprint.as_bytes());
    scoped.extend_from_slice(&(configured.len() as u64).to_be_bytes());
    scoped.extend_from_slice(&configured);
    fingerprint_bytes(&scoped)
}

fn custom_credential_fingerprint(api_key: &str, headers: &http::HeaderMap) -> String {
    fn add_component(hasher: &mut Sha256, bytes: &[u8]) {
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }

    // HeaderMap names are case-normalized, but its iteration order is not a
    // stable cache key. Sort the effective on-wire name/value pairs and frame
    // every component so distinct credentials cannot collide by concatenation.
    let mut header_scope = headers
        .iter()
        .map(|(name, value)| (name.as_str().as_bytes(), value.as_bytes()))
        .collect::<Vec<_>>();
    header_scope.sort_unstable();

    let mut hasher = Sha256::new();
    hasher.update(b"ygg-custom-model-cache-scope-v1");
    add_component(&mut hasher, api_key.as_bytes());
    for (name, value) in header_scope {
        add_component(&mut hasher, name);
        add_component(&mut hasher, value);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn provider_inventory_cache_path(provider_id: &str) -> PathBuf {
    // Keep a short readable prefix for diagnostics, but include the complete
    // digest of the original identifier. Replacing punctuation with `_` alone
    // lets distinct provider IDs map to the same cache file.
    let safe_id = provider_id
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
    let readable = if safe_id.is_empty() {
        "provider"
    } else {
        safe_id.as_str()
    };
    let digest = fingerprint_bytes(provider_id.as_bytes());
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("cache")
        .join("model-inventories")
        .join(format!("{readable}-{digest}.json"))
}

fn load_provider_inventory_cache(
    path: &std::path::Path,
    provider_id: &str,
    inventory_url: &str,
    credential_fingerprint: &str,
) -> anyhow::Result<Option<CachedProviderInventory>> {
    let Some(bytes) = crate::auth::read_bounded_private(path, MAX_PROVIDER_INVENTORY_CACHE_BYTES)?
    else {
        return Ok(None);
    };
    let cache: ProviderInventoryCache =
        serde_json::from_slice(&bytes).context("invalid provider inventory cache")?;
    if cache.version != PROVIDER_INVENTORY_CACHE_VERSION
        || cache.provider_id != provider_id
        || cache.inventory_url != inventory_url
        || cache.credential_fingerprint != credential_fingerprint
    {
        return Ok(None);
    }
    Ok(Some(match cache.body {
        Some(body) => CachedProviderInventory::Available(body),
        None => CachedProviderInventory::Unavailable,
    }))
}

fn save_provider_inventory_cache(
    path: &std::path::Path,
    provider_id: &str,
    inventory_url: &str,
    credential_fingerprint: &str,
    body: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    let cache = ProviderInventoryCache {
        version: PROVIDER_INVENTORY_CACHE_VERSION,
        provider_id: provider_id.to_owned(),
        inventory_url: inventory_url.to_owned(),
        credential_fingerprint: credential_fingerprint.to_owned(),
        body: body.cloned(),
    };
    crate::auth::write_private_atomic(path, &serde_json::to_vec(&cache)?, ".provider-models-")
}

fn cache_modified_is_stale(modified: std::time::SystemTime, refresh_interval: Duration) -> bool {
    // A clock rollback or attacker-controlled future timestamp must never pin a
    // cache entry indefinitely. Treat an unmeasurable age as stale.
    modified
        .elapsed()
        .map_or(true, |age| age >= refresh_interval)
}

fn provider_inventory_cache_is_stale(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map_or(true, |modified| {
            cache_modified_is_stale(modified, PROVIDER_INVENTORY_REFRESH_INTERVAL)
        })
}

fn fetch_provider_inventory(
    inventory_url: String,
    headers: http::HeaderMap,
) -> anyhow::Result<serde_json::Value> {
    get_models_json_blocking(&inventory_url, headers)
}

fn schedule_provider_inventory_refresh(
    path: PathBuf,
    provider_id: &'static str,
    inventory_url: String,
    credential_fingerprint: String,
    headers: http::HeaderMap,
    force: bool,
) {
    if cfg!(test) || (!force && !provider_inventory_cache_is_stale(&path)) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name(format!("ygg-{provider_id}-catalog-refresh"))
        .spawn(move || {
            if let Ok(body) = get_models_json_blocking(&inventory_url, headers) {
                let _ = save_provider_inventory_cache(
                    &path,
                    provider_id,
                    &inventory_url,
                    &credential_fingerprint,
                    Some(&body),
                );
            }
        });
}

fn fetch_and_cache_provider_inventory_with<F>(
    path: &std::path::Path,
    provider_id: &'static str,
    inventory_url: String,
    headers: http::HeaderMap,
    credential_fingerprint: &str,
    fetch: F,
) -> anyhow::Result<serde_json::Value>
where
    F: FnOnce(String, http::HeaderMap) -> anyhow::Result<serde_json::Value>,
{
    match fetch(inventory_url.clone(), headers) {
        Ok(body) => {
            if let Err(error) = save_provider_inventory_cache(
                path,
                provider_id,
                &inventory_url,
                credential_fingerprint,
                Some(&body),
            ) {
                crate::output::stderr!(
                    "warning: could not persist {provider_id} model metadata: {error}"
                );
            }
            Ok(body)
        }
        // Never replace a last-good inventory with failure state. A concurrent
        // refresh may have installed one while this request was in flight, so
        // re-read once and use it before surfacing the transient error. Legacy
        // negative markers remain readable, but new failures stay in-process.
        Err(fetch_error) => match load_provider_inventory_cache(
            path,
            provider_id,
            &inventory_url,
            credential_fingerprint,
        ) {
            Ok(Some(CachedProviderInventory::Available(body))) => Ok(body),
            _ => Err(fetch_error),
        },
    }
}

fn cached_provider_inventory(
    provider_id: &'static str,
    inventory_url: String,
    headers: http::HeaderMap,
    credential: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let path = provider_inventory_cache_path(provider_id);
    cached_provider_inventory_with_fetch(
        path,
        provider_id,
        inventory_url,
        headers,
        credential,
        fetch_provider_inventory,
    )
}

fn cached_provider_inventory_with_fetch<F>(
    path: PathBuf,
    provider_id: &'static str,
    inventory_url: String,
    headers: http::HeaderMap,
    credential: &str,
    fetch: F,
) -> anyhow::Result<Option<serde_json::Value>>
where
    F: FnOnce(String, http::HeaderMap) -> anyhow::Result<serde_json::Value>,
{
    let fingerprint = credential_fingerprint(credential);
    match load_provider_inventory_cache(&path, provider_id, &inventory_url, &fingerprint) {
        Ok(Some(CachedProviderInventory::Available(body))) => {
            schedule_provider_inventory_refresh(
                path,
                provider_id,
                inventory_url,
                fingerprint,
                headers,
                false,
            );
            Ok(Some(body))
        }
        Ok(Some(CachedProviderInventory::Unavailable)) => {
            // Dynamic-only providers cannot usefully continue without models.
            // Retry in the foreground so a recovered endpoint becomes usable
            // in this launch, rather than refreshing a file that only a later
            // process could observe.
            fetch_and_cache_provider_inventory_with(
                &path,
                provider_id,
                inventory_url,
                headers,
                &fingerprint,
                fetch,
            )
            .map(Some)
        }
        Ok(None) => fetch_and_cache_provider_inventory_with(
            &path,
            provider_id,
            inventory_url,
            headers,
            &fingerprint,
            fetch,
        )
        .map(Some),
        Err(cache_error) => {
            crate::output::stderr!("warning: {provider_id} model cache unavailable: {cache_error}");
            fetch_and_cache_provider_inventory_with(
                &path,
                provider_id,
                inventory_url,
                headers,
                &fingerprint,
                fetch,
            )
            .map(Some)
        }
    }
}

/// Use an existing inventory immediately, but never make startup wait for a
/// cold supplemental catalog. This is used by providers such as OpenCode that
/// already have a substantial embedded model set; discovery fills the cache for
/// the next launch in the background.
fn cached_provider_inventory_or_schedule(
    provider_id: &'static str,
    inventory_url: String,
    headers: http::HeaderMap,
    credential: &str,
) -> Option<serde_json::Value> {
    let path = provider_inventory_cache_path(provider_id);
    let fingerprint = credential_fingerprint(credential);
    match load_provider_inventory_cache(&path, provider_id, &inventory_url, &fingerprint) {
        Ok(Some(CachedProviderInventory::Available(body))) => {
            schedule_provider_inventory_refresh(
                path,
                provider_id,
                inventory_url,
                fingerprint,
                headers,
                false,
            );
            Some(body)
        }
        Ok(Some(CachedProviderInventory::Unavailable)) => {
            schedule_provider_inventory_refresh(
                path,
                provider_id,
                inventory_url,
                fingerprint,
                headers,
                true,
            );
            None
        }
        Ok(None) => {
            schedule_provider_inventory_refresh(
                path,
                provider_id,
                inventory_url,
                fingerprint,
                headers,
                true,
            );
            None
        }
        Err(error) => {
            crate::output::stderr!("warning: {provider_id} model cache unavailable: {error}");
            schedule_provider_inventory_refresh(
                path,
                provider_id,
                inventory_url,
                fingerprint,
                headers,
                true,
            );
            None
        }
    }
}

async fn bounded_discovery_json_async(
    response: reqwest::Response,
    label: &str,
) -> anyhow::Result<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DISCOVERY_BODY_BYTES as u64)
    {
        anyhow::bail!(
            "{label} response exceeds the {}-byte limit",
            MAX_DISCOVERY_BODY_BYTES
        );
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_DISCOVERY_BODY_BYTES)
        {
            anyhow::bail!(
                "{label} response exceeds the {}-byte limit",
                MAX_DISCOVERY_BODY_BYTES
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid {label} response: {error}"))
}

/// Conservative fallback for provider `/models` responses that omit
/// architecture metadata. Gemini and Claude families are image-capable by
/// contract, as are the explicitly listed open multimodal families below.
fn model_id_implies_vision(id: &str) -> bool {
    let id = id.to_ascii_lowercase().replace('_', ".");
    id.contains("gemini")
        || id.contains("claude")
        || id.contains("gpt-5.1-codex")
        || id.contains("gpt-5.2-codex")
        || id.contains("gpt-5.3-codex")
        || id.contains("gpt-5.4")
        || id.contains("gpt-5.5")
        || id.contains("gpt-5.6")
        || (id.contains("deepseek") && id.contains("vision"))
        || id.contains("codex-mini")
        || id.contains("qwen3.5")
        || id.contains("qwen3.6")
        || id.contains("qwen2-vl")
        || id.contains("qwen2.5-vl")
        || id.contains("qwen3-vl")
        || id.contains("qwen-vl")
        || id.contains("llava")
        || id.contains("internvl")
        || id.contains("pixtral")
}

#[derive(Clone, Debug)]
struct DiscoveredApiModel {
    id: String,
    context_window: Option<u64>,
    max_output_tokens: Option<u64>,
    tools: bool,
    vision: bool,
    audio: bool,
}

fn is_deepseek_v4_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id == "deepseek-v4" || id.starts_with("deepseek-v4-")
}

/// DeepSeek's sparse V4 inventory omits its documented limits. Preserve any
/// explicit positive metadata, but use the embedded V4 limits instead of the
/// generic placeholder when the provider leaves them out.
fn deepseek_discovered_limits(model: &DiscoveredApiModel) -> (u64, u64) {
    let (default_context_window, default_max_output_tokens) = if is_deepseek_v4_model(&model.id) {
        (
            DEEPSEEK_DEFAULT_CONTEXT_WINDOW,
            DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS,
        )
    } else {
        (128_000, 64_000)
    };
    let context_window = model.context_window.unwrap_or(default_context_window);
    let max_output_tokens = model
        .max_output_tokens
        .unwrap_or(default_max_output_tokens)
        .min(context_window);
    (context_window, max_output_tokens)
}

fn metadata_capability_flag(value: &serde_json::Value) -> Option<bool> {
    value
        .as_bool()
        .or_else(|| value.get("supported").and_then(serde_json::Value::as_bool))
}

/// Inventory schemas are not standardized, but the common gateways expose
/// tool support either as a capability flag or as a list of accepted request
/// parameters. Keep unknown distinct from an explicit false so hosted and
/// user-configured local endpoints can apply different safe defaults.
fn model_metadata_tool_support(entry: &serde_json::Value) -> Option<bool> {
    for metadata in [
        Some(entry),
        entry.get("top_provider"),
        entry.get("provider"),
    ]
    .into_iter()
    .flatten()
    {
        for name in [
            "supports_tools",
            "tools",
            "tool_calling",
            "function_calling",
        ] {
            if let Some(supported) = metadata.get(name).and_then(metadata_capability_flag) {
                return Some(supported);
            }
        }
        if let Some(capabilities) = metadata.get("capabilities") {
            for name in ["tools", "tool_calling", "function_calling"] {
                if let Some(supported) = capabilities.get(name).and_then(metadata_capability_flag) {
                    return Some(supported);
                }
            }
        }
        if let Some(parameters) = metadata
            .get("supported_parameters")
            .and_then(serde_json::Value::as_array)
        {
            return Some(parameters.iter().any(|parameter| {
                matches!(
                    parameter.as_str(),
                    Some("tools" | "tool_choice" | "functions" | "function_call")
                )
            }));
        }
    }
    None
}

/// Hosted inventories must positively advertise tools. Sending schemas to an
/// unknown text-only route can otherwise make an ordinary prompt fail before
/// generation begins.
fn model_metadata_supports_tools(entry: &serde_json::Value) -> bool {
    model_metadata_tool_support(entry).unwrap_or(false)
}

/// A custom endpoint is an explicit user-selected OpenAI-compatible runtime.
/// Preserve Ygg's historical/local default when its sparse `/models` response
/// says nothing about tools, while still honoring every explicit false.
fn custom_model_metadata_supports_tools(entry: &serde_json::Value) -> bool {
    model_metadata_tool_support(entry).unwrap_or(true)
}

/// Read provider model-inventory modality metadata without assuming a single
/// envelope. OpenAI-compatible servers put it under `architecture`, while
/// several gateways expose it at the top level (and some call it
/// `modalities`). Keeping this normalization in one place prevents a model
/// from being incorrectly treated as text-only just because its inventory
/// shape differs.
fn input_modalities_from_entry(entry: &serde_json::Value) -> ModalitySet {
    let values = entry
        .get("architecture")
        .and_then(|value| value.get("input_modalities"))
        .or_else(|| entry.get("input_modalities"))
        .or_else(|| entry.get("modalities"))
        .and_then(serde_json::Value::as_array);
    let mut result = ModalitySet::none();
    for value in values.into_iter().flatten() {
        let Some(value) = value.as_str() else {
            continue;
        };
        let value = value.to_ascii_lowercase();
        if value == "image" || value == "vision" || value.contains("image") {
            result = result.with(ygg_ai::Modality::Image);
        }
        if value == "audio" || value.contains("audio") {
            result = result.with(ygg_ai::Modality::Audio);
        }
    }
    result
}

/// Parse the two inventory envelopes used by supported providers: OpenAI-style
/// `{ "data": [...] }` and Codex-style `{ "models": [...] }`. Some local
/// servers return the array directly, so that shape is accepted as well.
fn api_models_from_response(body: &serde_json::Value) -> anyhow::Result<Vec<DiscoveredApiModel>> {
    let entries = body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| body.as_array())
        .ok_or_else(|| anyhow::anyhow!("models response has no data/models array"))?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && *id != "default")
        else {
            continue;
        };
        let input_modalities = input_modalities_from_entry(entry);
        let vision =
            input_modalities.contains(ygg_ai::Modality::Image) || model_id_implies_vision(id);
        let audio = input_modalities.contains(ygg_ai::Modality::Audio);
        models.push(DiscoveredApiModel {
            id: id.to_owned(),
            context_window: positive_u64(
                entry,
                &[
                    "context_window",
                    "context_length",
                    "max_model_len",
                    "max_context_tokens",
                ],
            ),
            max_output_tokens: positive_u64(entry, &["max_output_tokens", "max_completion_tokens"])
                .or_else(|| {
                    entry
                        .get("top_provider")
                        .and_then(|provider| positive_u64(provider, &["max_completion_tokens"]))
                }),
            tools: custom_model_metadata_supports_tools(entry),
            vision,
            audio,
        });
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

fn get_models_json_blocking(
    url: &str,
    headers: http::HeaderMap,
) -> anyhow::Result<serde_json::Value> {
    let response = blocking_discovery_client(DISCOVERY_TIMEOUT)?
        .get(url)
        .headers(headers)
        .send()
        .map_err(|_| anyhow::anyhow!("model discovery request failed"))?
        .error_for_status()
        .map_err(|_| anyhow::anyhow!("model discovery request was rejected"))?;
    bounded_discovery_json(response, "model discovery")
}

fn has_api_model(catalog: &ModelCatalog, endpoint: &str, api_name: &str) -> bool {
    catalog
        .models()
        .any(|model| model.endpoint.0 == endpoint && model.api_name == api_name)
}

fn declaration_discovery_headers(
    declaration: &ProviderDeclaration,
    credential: &crate::providers::EnvironmentCredential,
) -> anyhow::Result<http::HeaderMap> {
    let route = declaration.inventory_route().ok_or_else(|| {
        anyhow::anyhow!(
            "{} provider declaration has no discovery route",
            declaration.id
        )
    })?;
    let mut headers = crate::providers::environment_discovery_headers(route, credential)?;
    add_declared_headers(&mut headers, declaration)?;
    Ok(headers)
}

fn add_declared_headers(
    target: &mut http::HeaderMap,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    let headers = crate::providers::public_headers(declaration.extra_headers)?;
    for (name, value) in &headers {
        target.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn model_filter_matches(filter: ModelFilter, id: &str) -> bool {
    match filter {
        ModelFilter::All => true,
        ModelFilter::Prefix(prefixes) => prefixes.iter().any(|prefix| id.starts_with(prefix)),
    }
}

fn has_model_id(catalog: &ModelCatalog, id: &str) -> bool {
    catalog.resolve(&ModelId(id.to_owned())).is_ok()
}

fn discovered_model_supports_reasoning(protocol: Protocol, id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    match protocol {
        Protocol::OpenAiResponses => {
            id.starts_with("gpt-5")
                || id.starts_with("codex-")
                || id
                    .strip_prefix('o')
                    .and_then(|rest| rest.as_bytes().first())
                    .is_some_and(u8::is_ascii_digit)
        }
        // OpenAI-compatible providers also expose reasoning models through
        // Chat Completions.  This must not be gated on the Responses codec:
        // Cerebras Gemma 4, for example, accepts reasoning_effort on Chat.
        Protocol::OpenAiChat => {
            id.contains("gemma-4")
                || id.contains("qwen3")
                || id.contains("deepseek")
                || id.contains("reason")
                || id.contains("r1")
        }
        Protocol::AnthropicMessages | Protocol::BedrockConverse | Protocol::GoogleGenerativeAi => {
            false
        }
    }
}

fn discovered_preset_binding<'a>(
    declaration: &'a ProviderDeclaration,
    model_id: &str,
) -> Option<&'a ProviderRoute> {
    declaration.route_for_model(model_id)
}

fn register_openai_compatible_models(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    filter: ModelFilter,
    credential: &crate::providers::EnvironmentCredential,
) -> anyhow::Result<()> {
    let models_url = url::Url::parse(declaration.base_url)?.join("models")?;
    let headers = declaration_discovery_headers(declaration, credential)?;
    let body = match declaration.inventory_cache {
        crate::providers::InventoryCacheMode::Supplemental => {
            cached_provider_inventory_or_schedule(
                declaration.id,
                models_url.to_string(),
                headers,
                credential.value(),
            )
        }
        crate::providers::InventoryCacheMode::Required => cached_provider_inventory(
            declaration.id,
            models_url.to_string(),
            headers,
            credential.value(),
        )?,
    };
    let Some(body) = body else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        let api_name = model.id.as_str();
        let catalog_id = format!("{}/{}", declaration.id, api_name);
        let Some(route) = discovered_preset_binding(declaration, api_name) else {
            continue;
        };
        if !model_filter_matches(filter, api_name)
            || has_api_model(catalog, route.endpoint_id, api_name)
            || has_model_id(catalog, &catalog_id)
        {
            continue;
        }
        let protocol = route.protocol;
        let reasoning = discovered_model_supports_reasoning(protocol, api_name);
        let context_window = model.context_window.unwrap_or(128_000);
        let max_output_tokens = model
            .max_output_tokens
            .unwrap_or(32_768)
            .min(context_window);
        let mut input_modalities = if model.vision
            || model_id_implies_vision(api_name)
            || declaration
                .discovery_capabilities
                .gpt_vision_fallback(api_name)
        {
            ModalitySet::none().with(ygg_ai::Modality::Image)
        } else {
            ModalitySet::none()
        };
        // Audio inventory metadata is only actionable on the Chat codec; the
        // Responses and Anthropic codecs intentionally have no audio mapping.
        if model.audio && protocol == Protocol::OpenAiChat {
            input_modalities = input_modalities.with(ygg_ai::Modality::Audio);
        }
        crate::providers::register_discovered_model(
            catalog,
            declaration,
            api_name,
            None,
            Capabilities {
                input_modalities,
                output_modalities: ModalitySet::none(),
                tools: model.tools,
                parallel_tool_calls: model.tools && protocol != Protocol::OpenAiChat,
                reasoning: reasoning.then_some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: true,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::Standard,
                    min_effort: ygg_ai::ReasoningEffort::Minimal,
                    max_effort: ygg_ai::ReasoningEffort::High,
                }),
                responses_lite: false,
                agent_delegation: None,
                structured_output: protocol != Protocol::OpenAiChat,
                deferred_tool_loading: false,
            },
            ModelLimits {
                context_window,
                max_output_tokens,
            },
            None,
        )?;
    }
    Ok(())
}

fn register_anthropic_compatible_models(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    filter: ModelFilter,
    credential: &crate::providers::EnvironmentCredential,
) -> anyhow::Result<()> {
    let mut headers = declaration_discovery_headers(declaration, credential)?;
    headers.insert(
        http::HeaderName::from_static("anthropic-version"),
        http::HeaderValue::from_static("2023-06-01"),
    );
    let models_url = url::Url::parse(declaration.base_url)?.join("models?limit=1000")?;
    let Some(body) = cached_provider_inventory(
        declaration.id,
        models_url.to_string(),
        headers,
        credential.value(),
    )?
    else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        let api_name = model.id.as_str();
        let catalog_id = format!("{}/{}", declaration.id, api_name);
        let Some(route) = declaration.route_for_model(api_name) else {
            continue;
        };
        if !model_filter_matches(filter, api_name)
            || has_api_model(catalog, route.endpoint_id, api_name)
            || has_model_id(catalog, &catalog_id)
        {
            continue;
        }
        let context_window = model.context_window.unwrap_or(200_000);
        let max_output_tokens = model
            .max_output_tokens
            .unwrap_or(64_000)
            .min(context_window);
        crate::providers::register_discovered_model(
            catalog,
            declaration,
            api_name,
            None,
            Capabilities {
                input_modalities: if model.vision
                    || declaration.discovery_capabilities.assumes_image_input()
                {
                    ModalitySet::none().with(ygg_ai::Modality::Image)
                } else {
                    ModalitySet::none()
                },
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                // Inventing adaptive-thinking support makes older models reject
                // otherwise valid requests, so discovery remains conservative.
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: true,
                deferred_tool_loading: false,
            },
            ModelLimits {
                context_window,
                max_output_tokens,
            },
            None,
        )?;
    }
    Ok(())
}

fn deepseek_base_url(declaration: &ProviderDeclaration) -> anyhow::Result<url::Url> {
    let configured =
        optional_env("YGG_DEEPSEEK_BASE_URL")?.unwrap_or_else(|| declaration.base_url.to_owned());
    let normalized = if configured.ends_with('/') {
        configured
    } else {
        format!("{configured}/")
    };
    url::Url::parse(&normalized)
        .map_err(|error| anyhow::anyhow!("invalid YGG_DEEPSEEK_BASE_URL: {error}"))
}

fn deepseek_limit(name: &str, default: u64) -> anyhow::Result<u64> {
    let value = match strict_env(name)? {
        Some(value) => value,
        None => return Ok(default),
    };
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid {name}: {error}"))
}

fn declared_deepseek_route(declaration: &ProviderDeclaration) -> anyhow::Result<&ProviderRoute> {
    declaration
        .route_for_model(DEEPSEEK_MODEL_ID)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} declaration has no route for the DeepSeek fallback model",
                declaration.id
            )
        })
}

fn register_deepseek_v4_pro(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    let route = declared_deepseek_route(declaration)?;
    let endpoint_id = EndpointId(route.endpoint_id.into());
    if !catalog.has_endpoint(&endpoint_id) {
        anyhow::bail!(
            "{} declaration endpoint must be registered before fallback models",
            declaration.id
        );
    }
    if has_model_id(catalog, DEEPSEEK_MODEL_ID) {
        return Ok(());
    }
    let api_name =
        optional_env("YGG_DEEPSEEK_MODEL")?.unwrap_or_else(|| DEEPSEEK_MODEL_ID.to_owned());
    let cache =
        crate::providers::cache_compatibility(declaration.compatibility, &api_name, route.protocol);
    let pricing = crate::providers::pricing_for(declaration, &api_name);
    let context_window = deepseek_limit(
        "YGG_DEEPSEEK_CONTEXT_WINDOW",
        DEEPSEEK_DEFAULT_CONTEXT_WINDOW,
    )?;
    let max_output_tokens = deepseek_limit(
        "YGG_DEEPSEEK_MAX_OUTPUT_TOKENS",
        DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS,
    )?;
    if max_output_tokens > context_window {
        anyhow::bail!("YGG_DEEPSEEK_MAX_OUTPUT_TOKENS must not exceed YGG_DEEPSEEK_CONTEXT_WINDOW");
    }
    catalog.register_model(ModelSpec {
        id: ModelId(DEEPSEEK_MODEL_ID.into()),
        endpoint: endpoint_id,
        api_name,
        display_name: None,
        protocol: route.protocol,
        capabilities: Capabilities {
            input_modalities: ModalitySet::none(),
            output_modalities: ModalitySet::none(),
            tools: true,
            parallel_tool_calls: false,
            reasoning: Some(ReasoningCapability {
                control: ReasoningControl::Effort,
                exposes_text: true,
                preserves_state: false,
                effort_budgets: None,
                openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
                min_effort: ygg_ai::ReasoningEffort::High,
                max_effort: ygg_ai::ReasoningEffort::Xhigh,
            }),
            responses_lite: false,
            agent_delegation: None,
            structured_output: false,

            deferred_tool_loading: false,
        },
        limits: ModelLimits {
            context_window,
            max_output_tokens,
        },
        pricing,
        cache,
    })?;
    Ok(())
}

fn register_discovered_deepseek_models(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    credential: &crate::providers::EnvironmentCredential,
    base_url: &url::Url,
) -> anyhow::Result<()> {
    let discovery_route = declared_deepseek_route(declaration)?;
    let url = base_url.join("models")?.to_string();
    let mut headers = crate::providers::environment_discovery_headers(discovery_route, credential)?;
    add_declared_headers(&mut headers, declaration)?;
    let Some(body) = cached_provider_inventory(declaration.id, url, headers, credential.value())?
    else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        let api_name = model.id.as_str();
        let Some(route) = declaration.route_for_model(api_name) else {
            continue;
        };
        if has_api_model(catalog, route.endpoint_id, api_name) {
            continue;
        }
        let supports_reasoning =
            api_name.contains("reason") || api_name.contains("r1") || api_name.contains("v4");
        let (context_window, max_output_tokens) = deepseek_discovered_limits(&model);
        crate::providers::register_discovered_model(
            catalog,
            declaration,
            api_name,
            None,
            Capabilities {
                input_modalities: if model.vision {
                    ModalitySet::none().with(ygg_ai::Modality::Image)
                } else {
                    ModalitySet::none()
                },
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: false,
                reasoning: supports_reasoning.then_some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: false,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
                    min_effort: ygg_ai::ReasoningEffort::Minimal,
                    max_effort: ygg_ai::ReasoningEffort::High,
                }),
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,

                deferred_tool_loading: false,
            },
            ModelLimits {
                context_window,
                max_output_tokens,
            },
            None,
        )?;
    }
    Ok(())
}

/// Populate OpenRouter from its live inventory while retaining provider-specific
/// capability and pricing metadata.
fn register_openrouter_models(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
    credential: &crate::providers::EnvironmentCredential,
) -> anyhow::Result<()> {
    let models_url = url::Url::parse(declaration.base_url)?.join("models")?;
    let headers = declaration_discovery_headers(declaration, credential)?;
    let Some(body) = cached_provider_inventory(
        declaration.id,
        models_url.to_string(),
        headers,
        credential.value(),
    )?
    else {
        return Ok(());
    };
    for model in openrouter_models_from_response(declaration, &body)? {
        if !has_model_id(catalog, &model.id.0) {
            catalog.register_model(model)?;
        }
    }
    Ok(())
}

fn openrouter_pricing_value<'a>(
    pricing: &'a serde_json::Value,
    names: &[&str],
) -> Option<&'a serde_json::Value> {
    names.iter().find_map(|name| pricing.get(name))
}

fn openrouter_token_rate(value: Option<&serde_json::Value>) -> Option<TokenRate> {
    let value = value?;
    let raw = match value {
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Object(object) => {
            return ["value", "price", "rate", "per_token"]
                .iter()
                .find_map(|name| openrouter_token_rate(object.get(*name)));
        }
        _ => return None,
    };
    let raw = raw.trim();
    let (whole, fraction) = raw.split_once('.').unwrap_or((raw, ""));
    if whole.starts_with('-') || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?.checked_mul(1_000_000_000_000)?;
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut fractional = fraction
        .bytes()
        .take(12)
        .fold(0u64, |value, digit| value * 10 + u64::from(digit - b'0'));
    let places = fraction.len().min(12);
    fractional = fractional.checked_mul(10u64.pow((12 - places) as u32))?;
    // Round values more precise than one microdollar per million tokens to the
    // nearest representable TokenRate rather than silently charging zero.
    if fraction
        .as_bytes()
        .get(12)
        .is_some_and(|digit| *digit >= b'5')
    {
        fractional = fractional.checked_add(1)?;
    }
    whole.checked_add(fractional).map(TokenRate)
}

fn openrouter_pricing(entry: &serde_json::Value) -> Option<Pricing> {
    let pricing = entry.get("pricing")?;
    let input = openrouter_token_rate(openrouter_pricing_value(pricing, &["prompt", "input"]))?;
    let output =
        openrouter_token_rate(openrouter_pricing_value(pricing, &["completion", "output"]))?;
    let cache_read = openrouter_token_rate(openrouter_pricing_value(
        pricing,
        &["input_cache_read", "cache_read"],
    ))
    .unwrap_or(input);
    let cache_write = openrouter_token_rate(openrouter_pricing_value(
        pricing,
        &["input_cache_write", "cache_write"],
    ))
    .unwrap_or(input);
    let reasoning = openrouter_token_rate(openrouter_pricing_value(
        pricing,
        &["internal_reasoning", "reasoning"],
    ));

    let tiers = pricing
        .get("tiers")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tier| {
            let min_input_tokens = ["min_input_tokens", "min_tokens", "min"]
                .iter()
                .find_map(|name| tier.get(*name).and_then(serde_json::Value::as_u64))?;
            Some(PricingTier {
                min_input_tokens,
                input: openrouter_token_rate(openrouter_pricing_value(tier, &["prompt", "input"])),
                output: openrouter_token_rate(openrouter_pricing_value(
                    tier,
                    &["completion", "output"],
                )),
                cache_read: openrouter_token_rate(openrouter_pricing_value(
                    tier,
                    &["input_cache_read", "cache_read"],
                )),
                cache_write_5m: openrouter_token_rate(openrouter_pricing_value(
                    tier,
                    &["input_cache_write", "cache_write"],
                )),
                cache_write_1h: None,
                reasoning: openrouter_token_rate(openrouter_pricing_value(
                    tier,
                    &["internal_reasoning", "reasoning"],
                )),
            })
        })
        .collect();

    Some(Pricing {
        input,
        output,
        cache_read,
        cache_write_5m: cache_write,
        cache_write_1h: None,
        reasoning,
        tiers,
    })
}

fn openrouter_models_from_response(
    declaration: &ProviderDeclaration,
    body: &serde_json::Value,
) -> anyhow::Result<Vec<ModelSpec>> {
    let entries = body
        .get("data")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("OpenRouter models response is missing a data array"))?;

    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(api_name) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if api_name.trim().is_empty() {
            continue;
        }
        let context_window = entry
            .get("context_length")
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
            .unwrap_or(131_072);
        let Some(max_output_tokens) = entry
            .get("top_provider")
            .and_then(|provider| provider.get("max_completion_tokens"))
            .or_else(|| entry.get("max_completion_tokens"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
            .map(|value| value.min(context_window))
        else {
            // Without a provider-advertised completion ceiling, Ygg cannot
            // distinguish a real model limit from a guessed local cap.
            continue;
        };
        // OpenRouter may expose modality metadata under architecture or at
        // the top level (depending on the inventory proxy). Normalize both so
        // attachments are not rejected before the request reaches the API.
        let mut input_modalities = input_modalities_from_entry(entry);
        if model_id_implies_vision(api_name) {
            input_modalities = input_modalities.with(ygg_ai::Modality::Image);
        }
        let supports_tools = model_metadata_supports_tools(entry);

        let supports_reasoning = entry
            .get("supported_parameters")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parameters| {
                parameters.iter().any(|parameter| {
                    matches!(parameter.as_str(), Some("reasoning" | "reasoning.effort"))
                })
            });

        let Some(route) = declaration.route_for_model(api_name) else {
            continue;
        };
        models.push(ModelSpec {
            id: ModelId(format!("{}/{api_name}", declaration.id)),
            endpoint: EndpointId(route.endpoint_id.into()),
            api_name: api_name.into(),
            display_name: None,
            protocol: route.protocol,
            capabilities: Capabilities {
                input_modalities,
                output_modalities: ModalitySet::none(),
                tools: supports_tools,
                parallel_tool_calls: false,
                reasoning: supports_reasoning.then_some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: false,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::OpenRouter,
                    min_effort: ygg_ai::ReasoningEffort::Minimal,
                    max_effort: ygg_ai::ReasoningEffort::High,
                }),
                responses_lite: false,
                agent_delegation: None,
                structured_output: false,

                deferred_tool_loading: false,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens,
            },
            pricing: openrouter_pricing(entry)
                .or_else(|| crate::providers::pricing_for(declaration, api_name)),
            cache: crate::providers::cache_compatibility(
                declaration.compatibility,
                api_name,
                route.protocol,
            ),
        });
    }
    models.sort_by(|left, right| left.api_name.cmp(&right.api_name));
    Ok(models)
}

fn azure_openai_configuration(
    declaration: &ProviderDeclaration,
) -> anyhow::Result<Option<(url::Url, String)>> {
    let endpoint = optional_env("AZURE_OPENAI_ENDPOINT")?;
    let resource = optional_env("AZURE_OPENAI_RESOURCE")?;
    let version = optional_env("AZURE_OPENAI_API_VERSION")?;
    let deployment = optional_env("AZURE_OPENAI_DEPLOYMENT")?;
    azure_openai_configuration_from_values(
        declaration,
        endpoint.as_deref(),
        resource.as_deref(),
        version.as_deref(),
        deployment.as_deref(),
    )
}

fn azure_openai_configuration_from_values(
    declaration: &ProviderDeclaration,
    endpoint: Option<&str>,
    resource: Option<&str>,
    version: Option<&str>,
    deployment: Option<&str>,
) -> anyhow::Result<Option<(url::Url, String)>> {
    let endpoint = match endpoint {
        Some(endpoint) => endpoint.to_owned(),
        None => {
            let Some(resource) = resource else {
                return Ok(None);
            };
            if resource.is_empty()
                || resource.len() > 128
                || !resource
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                anyhow::bail!("invalid AZURE_OPENAI_RESOURCE");
            }
            format!("https://{resource}.openai.azure.com/")
        }
    };
    let mut endpoint =
        url::Url::parse(&endpoint).map_err(|_| anyhow::anyhow!("invalid AZURE_OPENAI_ENDPOINT"))?;
    if !matches!(endpoint.scheme(), "https" | "http")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        anyhow::bail!("invalid AZURE_OPENAI_ENDPOINT");
    }
    if !endpoint.path().ends_with('/') {
        endpoint.set_path(&format!("{}/", endpoint.path()));
    }
    let mut base_url = endpoint.join("openai/")?;
    let version = version
        .map(str::to_owned)
        .or_else(|| {
            url::Url::parse(declaration.base_url).ok().and_then(|url| {
                url.query_pairs()
                    .find(|(name, _)| name == "api-version")
                    .map(|(_, value)| value.into_owned())
            })
        })
        .ok_or_else(|| anyhow::anyhow!("AZURE_OPENAI_API_VERSION is required"))?;
    if version.is_empty()
        || version.len() > 96
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        anyhow::bail!("invalid AZURE_OPENAI_API_VERSION");
    }
    base_url.set_query(Some(&format!("api-version={version}")));
    let deployment = deployment
        .ok_or_else(|| anyhow::anyhow!("AZURE_OPENAI_DEPLOYMENT is required"))?
        .to_owned();
    if deployment.is_empty() || deployment.len() > 256 || deployment.chars().any(char::is_control) {
        anyhow::bail!("invalid AZURE_OPENAI_DEPLOYMENT");
    }
    Ok(Some((base_url, deployment)))
}

fn register_azure_openai(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    let Some(credential) = crate::providers::resolve_environment(declaration)? else {
        return Ok(());
    };
    let Some((base_url, deployment)) = azure_openai_configuration(declaration)? else {
        return Ok(());
    };
    crate::providers::register_environment_endpoints_at_base_url(
        catalog,
        declaration,
        &credential,
        &base_url,
        PROVIDER_RESPONSE_HEADER_TIMEOUT,
    )?;
    let Some(route) = declaration.route_for_model(&deployment) else {
        anyhow::bail!("Azure OpenAI declaration has no default route");
    };
    crate::providers::register_discovered_model(
        catalog,
        declaration,
        &deployment,
        Some(format!("Azure OpenAI: {deployment}")),
        Capabilities {
            input_modalities: ModalitySet::none().with(ygg_ai::Modality::Image),
            output_modalities: ModalitySet::none(),
            tools: true,
            parallel_tool_calls: true,
            reasoning: discovered_model_supports_reasoning(route.protocol, &deployment).then_some(
                ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: true,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::Standard,
                    min_effort: ygg_ai::ReasoningEffort::Minimal,
                    max_effort: ygg_ai::ReasoningEffort::High,
                },
            ),
            responses_lite: false,
            agent_delegation: None,
            structured_output: true,
            deferred_tool_loading: false,
        },
        ModelLimits {
            context_window: 128_000,
            max_output_tokens: 16_384,
        },
        None,
    )
}

fn register_aws_bedrock(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    let region = crate::providers::aws_bedrock_region()?;
    let Some(auth) = crate::providers::aws_bedrock_auth(&region)? else {
        return Ok(());
    };
    let base_url = crate::providers::aws_bedrock_base_url(&region)?;
    crate::providers::register_private_endpoints_at_base_url(
        catalog,
        declaration,
        auth,
        &base_url,
        PROVIDER_RESPONSE_HEADER_TIMEOUT,
    )?;
    crate::providers::register_static_models(catalog, declaration)
}

fn try_register_declaration(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    declaration.validate().map_err(|error| {
        anyhow::anyhow!("invalid {} provider declaration: {error}", declaration.id)
    })?;
    match declaration.runtime_configuration {
        ProviderRuntimeConfiguration::AwsBedrock => {
            return register_aws_bedrock(catalog, declaration);
        }
        ProviderRuntimeConfiguration::AzureOpenAi => {
            return register_azure_openai(catalog, declaration);
        }
        ProviderRuntimeConfiguration::Default => {}
    }

    match declaration.authentication {
        ProviderAuthentication::Environment { .. } => {
            try_register_environment_declaration(catalog, declaration)
        }
        ProviderAuthentication::ApplicationDefaultCredentials => {
            let Some(configuration) = crate::providers::resolve_application_default_credentials()?
            else {
                return Ok(());
            };
            match declaration.model_discovery {
                ModelDiscovery::Static | ModelDiscovery::None => {}
                _ => anyhow::bail!(
                    "dynamic-credential provider {} must use static model discovery",
                    declaration.id
                ),
            }
            crate::providers::register_dynamic_endpoints_at_base_url(
                catalog,
                declaration,
                configuration.auth,
                &configuration.base_url,
                PROVIDER_RESPONSE_HEADER_TIMEOUT,
            )?;
            crate::providers::register_static_models(catalog, declaration)
        }
        // Built-in AWS declarations are handled by their runtime configuration
        // above; no generic endpoint construction can safely invent a signer.
        ProviderAuthentication::Aws { .. } => {
            unreachable!("AWS provider declarations require a runtime configuration")
        }
        // Host-owned credentials can only enter through an explicit embedding
        // integration; preset bootstrap must never synthesize that authority.
        ProviderAuthentication::Subscription { .. } | ProviderAuthentication::HostOwned { .. } => {
            Ok(())
        }
    }
}

fn try_register_environment_declaration(
    catalog: &mut ModelCatalog,
    declaration: &ProviderDeclaration,
) -> anyhow::Result<()> {
    let Some(credential) = crate::providers::resolve_environment(declaration)? else {
        return Ok(());
    };

    if matches!(declaration.model_discovery, ModelDiscovery::DeepSeekModels) {
        let base_url = deepseek_base_url(declaration)?;
        crate::providers::register_environment_endpoints_at_base_url(
            catalog,
            declaration,
            &credential,
            &base_url,
            PROVIDER_RESPONSE_HEADER_TIMEOUT,
        )?;
        register_deepseek_v4_pro(catalog, declaration)?;
        register_discovered_deepseek_models(catalog, declaration, &credential, &base_url)?;
        return Ok(());
    }

    crate::providers::register_environment_endpoints(
        catalog,
        declaration,
        &credential,
        PROVIDER_RESPONSE_HEADER_TIMEOUT,
    )?;
    crate::providers::register_static_models(catalog, declaration)?;

    match declaration.model_discovery {
        ModelDiscovery::Static | ModelDiscovery::None => {}
        ModelDiscovery::OpenAiModels { filter } => {
            register_openai_compatible_models(catalog, declaration, filter, &credential)?;
        }
        ModelDiscovery::AnthropicModels { filter } => {
            register_anthropic_compatible_models(catalog, declaration, filter, &credential)?;
        }
        ModelDiscovery::OpenRouterModels => {
            register_openrouter_models(catalog, declaration, &credential)?;
        }
        ModelDiscovery::DeepSeekModels => unreachable!("handled before endpoint registration"),
        // Host-owned subscription discovery is registered by its embedding
        // integration, never by the environment-backed preset bootstrap.
        ModelDiscovery::CodexSubscription | ModelDiscovery::HostOwnedSubscription => {}
    }
    Ok(())
}

fn merge_provider_catalog(target: &mut ModelCatalog, source: ModelCatalog) -> anyhow::Result<()> {
    let models = source.models().cloned().collect::<Vec<_>>();
    for spec in models {
        let resolved = source.resolve(&spec.id)?;
        if !target.has_endpoint(&resolved.endpoint.id) {
            target.register_endpoint((*resolved.endpoint).clone())?;
        }
        if let Some(label) = source.endpoint_label(&resolved.endpoint.id) {
            target.set_endpoint_label(resolved.endpoint.id.clone(), label.to_owned())?;
        }
        if !has_model_id(target, &spec.id.0) {
            target.register_model(spec)?;
        }
    }
    Ok(())
}

fn declaration_is_configured(declaration: &ProviderDeclaration) -> anyhow::Result<bool> {
    match declaration.runtime_configuration {
        // The AWS chain includes EC2 instance metadata, which has no local
        // configuration marker. Schedule one bounded private registration job;
        // it decides whether a credential exists and avoids probing the chain
        // once here and again when constructing the endpoint.
        ProviderRuntimeConfiguration::AwsBedrock => Ok(true),
        ProviderRuntimeConfiguration::AzureOpenAi => {
            if crate::providers::resolve_environment(declaration)?.is_none() {
                return Ok(false);
            }
            Ok(azure_openai_configuration(declaration)?.is_some())
        }
        ProviderRuntimeConfiguration::Default => match declaration.authentication {
            ProviderAuthentication::Environment { .. } => {
                Ok(crate::providers::resolve_environment(declaration)?.is_some())
            }
            ProviderAuthentication::ApplicationDefaultCredentials => {
                Ok(crate::providers::resolve_application_default_credentials()?.is_some())
            }
            // A host-owned integration must call CopilotProvider explicitly;
            // environment/configuration bootstrap has no authority to enable it.
            ProviderAuthentication::Subscription { .. }
            | ProviderAuthentication::HostOwned { .. } => Ok(false),
            ProviderAuthentication::Aws { .. } => {
                unreachable!("AWS provider declarations require a runtime configuration")
            }
        },
    }
}

/// Discover configured provider catalogs concurrently, then merge them on the
/// launch thread. A fleet outage therefore costs at most one bounded discovery
/// interval instead of one interval per configured account.
fn register_configured_presets_parallel(catalog: &mut ModelCatalog) {
    let mut jobs = Vec::new();
    for declaration in BUILTIN_PROVIDER_DECLARATIONS {
        match declaration_is_configured(declaration) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                // An invalid/oversized optional credential must not become a
                // request, but one unusable provider must not block other
                // configured providers from starting.
                crate::output::stderr!("warning: {} unavailable: {error}", declaration.name);
                continue;
            }
        }
        let declaration = *declaration;
        match std::thread::Builder::new()
            .name(format!("ygg-{}-catalog", declaration.id))
            .spawn(move || {
                let mut provider_catalog = ModelCatalog::default();
                try_register_declaration(&mut provider_catalog, &declaration)?;
                Ok::<_, anyhow::Error>(provider_catalog)
            }) {
            Ok(handle) => jobs.push((declaration, handle)),
            Err(error) => crate::output::stderr!(
                "warning: could not start {} model discovery: {error}",
                declaration.name
            ),
        }
    }

    for (declaration, job) in jobs {
        match job.join() {
            Ok(Ok(provider_catalog)) => {
                if let Err(error) = merge_provider_catalog(catalog, provider_catalog) {
                    crate::output::stderr!("warning: {} unavailable: {error}", declaration.name);
                }
            }
            Ok(Err(error)) => {
                crate::output::stderr!("warning: {} unavailable: {error}", declaration.name)
            }
            Err(_) => crate::output::stderr!(
                "warning: {} unavailable: model discovery thread panicked",
                declaration.name
            ),
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CustomModelCache {
    version: u8,
    base_url: String,
    credential_fingerprint: String,
    models: Vec<crate::auth::custom::CustomModel>,
}

enum CachedCustomInventory {
    Available(Vec<crate::auth::custom::CustomModel>),
    Unavailable,
}

fn load_custom_model_cache_for(
    store: &crate::auth::custom::CredentialStore,
    provider_id: &str,
    base_url: &str,
    credential_fingerprint: &str,
) -> anyhow::Result<Option<CachedCustomInventory>> {
    let Some(bytes) = store.load_model_cache_for(provider_id)? else {
        return Ok(None);
    };
    let cache: CustomModelCache =
        serde_json::from_slice(&bytes).context("invalid custom model cache")?;
    if cache.version != CUSTOM_MODEL_CACHE_VERSION
        || cache.base_url != base_url
        || cache.credential_fingerprint != credential_fingerprint
    {
        return Ok(None);
    }
    Ok(Some(if cache.models.is_empty() {
        CachedCustomInventory::Unavailable
    } else {
        CachedCustomInventory::Available(cache.models)
    }))
}

#[cfg(test)]
fn load_custom_model_cache(
    store: &crate::auth::custom::CredentialStore,
    base_url: &str,
    credential_fingerprint: &str,
) -> anyhow::Result<Option<CachedCustomInventory>> {
    load_custom_model_cache_for(
        store,
        crate::auth::custom::ENDPOINT_ID,
        base_url,
        credential_fingerprint,
    )
}

fn save_custom_model_cache_for(
    store: &crate::auth::custom::CredentialStore,
    provider_id: &str,
    base_url: &str,
    credential_fingerprint: &str,
    models: &[crate::auth::custom::CustomModel],
) -> anyhow::Result<()> {
    let cache = CustomModelCache {
        version: CUSTOM_MODEL_CACHE_VERSION,
        base_url: base_url.to_owned(),
        credential_fingerprint: credential_fingerprint.to_owned(),
        models: models.to_vec(),
    };
    store.save_model_cache_for(provider_id, &serde_json::to_vec_pretty(&cache)?)
}

#[cfg(test)]
fn save_custom_model_cache(
    store: &crate::auth::custom::CredentialStore,
    base_url: &str,
    credential_fingerprint: &str,
    models: &[crate::auth::custom::CustomModel],
) -> anyhow::Result<()> {
    save_custom_model_cache_for(
        store,
        crate::auth::custom::ENDPOINT_ID,
        base_url,
        credential_fingerprint,
        models,
    )
}

fn schedule_custom_model_cache_refresh_for(
    store: crate::auth::custom::CredentialStore,
    provider_id: String,
    cred: crate::auth::custom::CustomCredential,
    credential_fingerprint: String,
    refresh_interval: Duration,
) {
    if cfg!(test)
        || !store
            .model_cache_is_stale_for(&provider_id, refresh_interval)
            .unwrap_or(true)
    {
        return;
    }
    let configured = configured_custom_models(&cred);
    let cache_fingerprint = custom_model_cache_fingerprint(&credential_fingerprint, &configured);
    let _ = std::thread::Builder::new()
        .name(format!("ygg-custom-{provider_id}-catalog-refresh"))
        .spawn(move || {
            let discovered = apply_configured_custom_model_overrides(
                apply_known_custom_model_defaults(&cred, discover_models_blocking(&cred, false)),
                &configured,
            );
            if !discovered.is_empty() {
                let _ = save_custom_model_cache_for(
                    &store,
                    &provider_id,
                    &cred.base_url,
                    &cache_fingerprint,
                    &discovered,
                );
            }
        });
}

fn refresh_stale_custom_models_with_for<F>(
    store: &crate::auth::custom::CredentialStore,
    provider_id: &str,
    cred: &crate::auth::custom::CustomCredential,
    credential_fingerprint: &str,
    cached: Vec<crate::auth::custom::CustomModel>,
    refresh_interval: Duration,
    discover: F,
) -> Vec<crate::auth::custom::CustomModel>
where
    F: FnOnce(&crate::auth::custom::CustomCredential) -> Vec<crate::auth::custom::CustomModel>,
{
    if !store
        .model_cache_is_stale_for(provider_id, refresh_interval)
        .unwrap_or(true)
    {
        return cached;
    }

    let discovered = discover_and_cache_custom_models_with_for(
        store,
        provider_id,
        cred,
        credential_fingerprint,
        false,
        discover,
    );
    if discovered.is_empty() {
        // A transient discovery failure must not discard a last-good catalog.
        cached
    } else {
        discovered
    }
}

#[cfg(test)]
fn refresh_stale_custom_models_with<F>(
    store: &crate::auth::custom::CredentialStore,
    cred: &crate::auth::custom::CustomCredential,
    credential_fingerprint: &str,
    cached: Vec<crate::auth::custom::CustomModel>,
    refresh_interval: Duration,
    discover: F,
) -> Vec<crate::auth::custom::CustomModel>
where
    F: FnOnce(&crate::auth::custom::CustomCredential) -> Vec<crate::auth::custom::CustomModel>,
{
    refresh_stale_custom_models_with_for(
        store,
        crate::auth::custom::ENDPOINT_ID,
        cred,
        credential_fingerprint,
        cached,
        refresh_interval,
        discover,
    )
}

fn discover_and_cache_custom_models_with_for<F>(
    store: &crate::auth::custom::CredentialStore,
    provider_id: &str,
    cred: &crate::auth::custom::CustomCredential,
    credential_fingerprint: &str,
    persist_empty: bool,
    discover: F,
) -> Vec<crate::auth::custom::CustomModel>
where
    F: FnOnce(&crate::auth::custom::CustomCredential) -> Vec<crate::auth::custom::CustomModel>,
{
    let discovered = apply_configured_custom_model_overrides(
        apply_known_custom_model_defaults(cred, discover(cred)),
        &configured_custom_models(cred),
    );
    if persist_empty || !discovered.is_empty() {
        if let Err(error) = save_custom_model_cache_for(
            store,
            provider_id,
            &cred.base_url,
            credential_fingerprint,
            &discovered,
        ) {
            crate::output::stderr!("warning: could not persist custom model metadata: {error}");
        }
    }
    discovered
}

#[cfg(test)]
fn discover_and_cache_custom_models_with<F>(
    store: &crate::auth::custom::CredentialStore,
    cred: &crate::auth::custom::CustomCredential,
    credential_fingerprint: &str,
    persist_empty: bool,
    discover: F,
) -> Vec<crate::auth::custom::CustomModel>
where
    F: FnOnce(&crate::auth::custom::CustomCredential) -> Vec<crate::auth::custom::CustomModel>,
{
    discover_and_cache_custom_models_with_for(
        store,
        crate::auth::custom::ENDPOINT_ID,
        cred,
        credential_fingerprint,
        persist_empty,
        discover,
    )
}

fn configured_custom_models(
    cred: &crate::auth::custom::CustomCredential,
) -> Vec<crate::auth::custom::CustomModel> {
    if !cred.models.is_empty() {
        cred.models.clone()
    } else if !cred.api_name.is_empty() {
        vec![crate::auth::custom::CustomModel {
            api_name: cred.api_name.clone(),
            display_name: String::new(),
            ..Default::default()
        }]
    } else {
        Vec::new()
    }
}

fn apply_configured_custom_model_overrides(
    discovered: Vec<crate::auth::custom::CustomModel>,
    configured: &[crate::auth::custom::CustomModel],
) -> Vec<crate::auth::custom::CustomModel> {
    if configured.is_empty() {
        return discovered;
    }

    let mut merged = Vec::with_capacity(discovered.len() + configured.len());
    for model in discovered {
        merged.push(
            configured
                .iter()
                .find(|override_model| override_model.api_name == model.api_name)
                .cloned()
                .unwrap_or(model),
        );
    }
    for model in configured {
        if !merged
            .iter()
            .any(|existing| existing.api_name == model.api_name)
        {
            merged.push(model.clone());
        }
    }
    merged
}

fn resolve_custom_startup_timeout(
    configured_secs: Option<u64>,
    environment: Option<&str>,
) -> anyhow::Result<Duration> {
    let seconds = match environment.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value
            .parse::<u64>()
            .map_err(|error| anyhow::anyhow!("invalid YGG_CUSTOM_STARTUP_TIMEOUT_SECS: {error}"))?,
        None => configured_secs.unwrap_or(CUSTOM_ENDPOINT_STARTUP_TIMEOUT.as_secs()),
    };
    anyhow::ensure!(
        seconds > 0,
        "custom endpoint startup timeout must be greater than zero"
    );
    Ok(Duration::from_secs(seconds))
}

fn custom_reasoning_effort(value: &str) -> Option<ygg_ai::ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" | "min" => Some(ygg_ai::ReasoningEffort::Minimal),
        "low" => Some(ygg_ai::ReasoningEffort::Low),
        "medium" | "med" => Some(ygg_ai::ReasoningEffort::Medium),
        "high" => Some(ygg_ai::ReasoningEffort::High),
        "xhigh" | "x-high" | "extra_high" => Some(ygg_ai::ReasoningEffort::Xhigh),
        "max" => Some(ygg_ai::ReasoningEffort::Max),
        _ => None,
    }
}

fn custom_reasoning_is_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "none" | "off" | "disabled" | "false"
    )
}

fn custom_reasoning_is_on(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "default" | "on" | "enabled" | "true"
    )
}

fn discovered_custom_reasoning(entry: &serde_json::Value) -> (bool, Vec<String>, String) {
    let reported = entry
        .get("capabilities")
        .and_then(|capabilities| capabilities.get("reasoning"));
    let values = reported
        .and_then(|reasoning| reasoning.get("values"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let default = reported
        .and_then(|reasoning| reasoning.get("default"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let enabled = match reported {
        Some(metadata) => {
            metadata
                .get("supported")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                && (values.is_empty() || values.iter().any(|value| !custom_reasoning_is_off(value)))
        }
        None => entry
            .get("supported_parameters")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parameters| {
                parameters.iter().any(|parameter| {
                    matches!(parameter.as_str(), Some("reasoning" | "reasoning_effort"))
                })
            }),
    };
    (enabled, values, default)
}

fn custom_reasoning_capability(
    model: &crate::auth::custom::CustomModel,
) -> Option<ReasoningCapability> {
    if !model.reasoning {
        return None;
    }
    let fixed_mode = if model.reasoning_uses_system_message {
        OpenAiChatReasoningMode::SystemMessage
    } else {
        OpenAiChatReasoningMode::Standard
    };
    if !model.reasoning_configurable {
        // Some providers think by default but reject every reasoning control
        // parameter. Keep that fact visible to ygg as a single `on` option while
        // retaining a parameter-free request path.
        return Some(ReasoningCapability {
            control: ReasoningControl::AlwaysOn,
            exposes_text: true,
            preserves_state: false,
            effort_budgets: None,
            openai_chat_mode: fixed_mode,
            min_effort: ygg_ai::ReasoningEffort::Minimal,
            max_effort: ygg_ai::ReasoningEffort::High,
        });
    }
    let efforts = model
        .reasoning_values
        .iter()
        .filter_map(|value| custom_reasoning_effort(value))
        .collect::<Vec<_>>();
    let control = if !efforts.is_empty() {
        ReasoningControl::Effort
    } else if model
        .reasoning_values
        .iter()
        .any(|value| custom_reasoning_is_on(value))
    {
        ReasoningControl::Toggle
    } else if model.reasoning_values.is_empty() {
        // Legacy/manual `reasoning = true` configurations predate provider
        // value discovery and retain the portable effort range.
        ReasoningControl::Effort
    } else {
        return None;
    };
    let min_effort = efforts
        .iter()
        .copied()
        .min()
        .unwrap_or(ygg_ai::ReasoningEffort::Minimal);
    let max_effort = efforts
        .iter()
        .copied()
        .max()
        .unwrap_or(ygg_ai::ReasoningEffort::High);
    let openai_chat_mode = if model.reasoning_values.is_empty() {
        fixed_mode
    } else {
        OpenAiChatReasoningMode::ProviderValues {
            values: model.reasoning_values.clone(),
            default: (!model.reasoning_default.is_empty()).then(|| model.reasoning_default.clone()),
            system_message: model.reasoning_uses_system_message,
        }
    };
    Some(ReasoningCapability {
        control,
        exposes_text: true,
        preserves_state: false,
        effort_budgets: None,
        openai_chat_mode,
        min_effort,
        max_effort,
    })
}

fn validate_custom_provider_id(provider_id: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !provider_id.is_empty()
            && provider_id.len() <= 64
            && provider_id.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            ),
        "custom provider id must be 1-64 ASCII letters, digits, '-' or '_': {provider_id:?}"
    );
    Ok(())
}

fn resolve_custom_provider_auth(
    provider: &crate::auth::custom::CustomProvider,
    legacy_single_endpoint: bool,
) -> anyhow::Result<(Auth, String)> {
    anyhow::ensure!(
        !(provider.auth.is_some() && provider.api_key_env.is_some()),
        "custom provider cannot set both auth and api_key_env"
    );

    let environment_auth = |var: &str| -> anyhow::Result<(Auth, String)> {
        let var = var.trim();
        anyhow::ensure!(
            !var.is_empty()
                && var
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_'),
            "custom provider auth environment variable must be a valid name: {var:?}"
        );
        let key = optional_env(var)?
            .filter(|key| !key.trim().is_empty())
            .unwrap_or_default();
        Ok((Auth::bearer_env(var), key))
    };

    if let Some(auth) = &provider.auth {
        return match auth {
            crate::auth::custom::CustomAuthConfig::None => Ok((Auth::None, String::new())),
            crate::auth::custom::CustomAuthConfig::BearerEnv { var } => environment_auth(var),
        };
    }
    if let Some(var) = provider.api_key_env.as_deref() {
        return environment_auth(var);
    }
    if !provider.credential.api_key.is_empty() {
        return Ok((
            Auth::bearer(provider.credential.api_key.as_str()),
            provider.credential.api_key.clone(),
        ));
    }
    if legacy_single_endpoint {
        let Some(key) = optional_env("YGG_CUSTOM_API_KEY")?.filter(|key| !key.trim().is_empty())
        else {
            return Ok((Auth::None, String::new()));
        };
        return Ok((Auth::bearer_env("YGG_CUSTOM_API_KEY"), key));
    }
    Ok((Auth::None, String::new()))
}

const APPLE_FM_PROVIDER_ID: &str = "apple-fm";
const APPLE_FM_BASE_URL: &str = "http://127.0.0.1:1976/v1/";
const APPLE_FM_LABEL: &str = "Apple Foundation Models";
const APPLE_FM_SYSTEM_CONTEXT_WINDOW: u64 = 8_192;
const APPLE_FM_PCC_CONTEXT_WINDOW: u64 = 32_768;
const APPLE_FM_MAX_OUTPUT_TOKENS: u64 = 1_024;
const APPLE_FM_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

fn is_apple_foundation_models_endpoint(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    matches!(url.scheme(), "http")
        && url.port() == Some(1976)
        && url
            .host_str()
            .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "::1"))
        && matches!(url.path().trim_end_matches('/'), "/v1")
}

fn custom_model_discovery_is_available(
    base_url: &str,
    apple_server_is_running: impl FnOnce() -> bool,
) -> bool {
    !is_apple_foundation_models_endpoint(base_url) || apple_server_is_running()
}

fn apple_foundation_model_defaults(api_name: &str) -> Option<crate::auth::custom::CustomModel> {
    let (context_window, reasoning_configurable, reasoning_values, reasoning_default) =
        match api_name {
            // The on-device model always thinks and rejects reasoning_effort. Keep
            // it visible as a fixed `on` capability while emitting no control field.
            "system" => (
                APPLE_FM_SYSTEM_CONTEXT_WINDOW,
                false,
                Vec::new(),
                String::new(),
            ),
            // fm serve accepts low/medium/high for the PCC route. PCC may be
            // unavailable on this device, but its wire contract is still stable.
            "pcc" => (
                APPLE_FM_PCC_CONTEXT_WINDOW,
                true,
                vec!["low".to_owned(), "medium".to_owned(), "high".to_owned()],
                "medium".to_owned(),
            ),
            _ => return None,
        };
    Some(crate::auth::custom::CustomModel {
        api_name: api_name.to_owned(),
        display_name: api_name.to_owned(),
        context_window,
        max_output_tokens: APPLE_FM_MAX_OUTPUT_TOKENS,
        tools: true,
        parallel_tool_calls: false,
        vision: false,
        structured_output: false,
        reasoning: true,
        reasoning_configurable,
        reasoning_values,
        reasoning_default,
        reasoning_uses_system_message: true,
        pricing: None,
    })
}

fn apply_known_custom_model_defaults(
    cred: &crate::auth::custom::CustomCredential,
    models: Vec<crate::auth::custom::CustomModel>,
) -> Vec<crate::auth::custom::CustomModel> {
    if !is_apple_foundation_models_endpoint(&cred.base_url) {
        return models;
    }
    models
        .into_iter()
        .map(|model| apple_foundation_model_defaults(&model.api_name).unwrap_or(model))
        .collect()
}

fn apple_foundation_models_health_is_valid(body: &serde_json::Value) -> bool {
    body.get("status").and_then(serde_json::Value::as_str) == Some("fm serve is running")
        && body
            .get("models")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|models| {
                models.iter().any(|model| {
                    model.get("name").and_then(serde_json::Value::as_str) == Some("system")
                        && model.get("available").and_then(serde_json::Value::as_bool) == Some(true)
                })
            })
}

fn apple_foundation_models_server_is_running() -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    std::thread::spawn(apple_foundation_models_server_is_running_blocking)
        .join()
        .unwrap_or(false)
}

fn apple_foundation_models_server_is_running_blocking() -> bool {
    let Ok(health_url) = url::Url::parse(APPLE_FM_BASE_URL).and_then(|url| url.join("../health"))
    else {
        return false;
    };
    let Ok(client) = blocking_discovery_client(APPLE_FM_PROBE_TIMEOUT) else {
        return false;
    };
    let Ok(response) = client.get(health_url).send() else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    bounded_discovery_json(response, "Apple Foundation Models health")
        .is_ok_and(|body| apple_foundation_models_health_is_valid(&body))
}

fn default_apple_foundation_models_provider() -> crate::auth::custom::CustomProvider {
    crate::auth::custom::CustomProvider {
        label: APPLE_FM_LABEL.to_owned(),
        credential: crate::auth::custom::CustomCredential {
            base_url: APPLE_FM_BASE_URL.to_owned(),
            api_key: String::new(),
            api_name: String::new(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        },
        auth: Some(crate::auth::custom::CustomAuthConfig::None),
        api_key_env: None,
        cache: None,
        startup_timeout_secs: Some(CUSTOM_ENDPOINT_STARTUP_TIMEOUT.as_secs()),
        lifecycle_feedback: false,
    }
}

/// Trusted pricing for a custom-provider model.
///
/// User-configured endpoints are treated as user-trusted: a declared
/// `pricing` block is honored, and an undeclared one defaults to zero rates
/// so local/self-hosted models count as free while still satisfying
/// guardrails that require trusted model pricing (such as subagent cost
/// ceilings).
fn custom_model_pricing(model: &crate::auth::custom::CustomModel) -> Pricing {
    let rates: crate::auth::custom::CustomPricing = model.pricing.unwrap_or_default();
    Pricing {
        input: TokenRate(rates.input),
        output: TokenRate(rates.output),
        cache_read: TokenRate(rates.cache_read),
        cache_write_5m: TokenRate(rates.cache_write_5m),
        cache_write_1h: None,
        reasoning: None,
        tiers: Vec::new(),
    }
}

fn custom_model_id(
    provider_id: &str,
    legacy_single_endpoint: bool,
    model: &crate::auth::custom::CustomModel,
) -> String {
    if legacy_single_endpoint {
        let configured_display =
            (!model.display_name.trim().is_empty()).then(|| model.display_name.trim().to_owned());
        let canonical_label = configured_display.as_deref().unwrap_or(&model.api_name);
        format!("custom/{canonical_label}")
    } else {
        format!("custom/{provider_id}/{}", model.api_name)
    }
}

fn register_custom_openai_endpoints_from_store(
    catalog: &mut ModelCatalog,
    store: &crate::auth::custom::CredentialStore,
    offline: bool,
) -> anyhow::Result<()> {
    let Some(registry) = store.load_registry()? else {
        return Ok(());
    };
    for (provider_id, provider) in registry.providers {
        let legacy_single_endpoint =
            registry.legacy_single_endpoint && provider_id == crate::auth::custom::ENDPOINT_ID;
        let mut provider_catalog = ModelCatalog::default();
        if let Err(error) = register_custom_openai_provider(
            &mut provider_catalog,
            store,
            &provider_id,
            &provider,
            legacy_single_endpoint,
            offline,
        ) {
            let label = provider.label.trim();
            let label = if label.is_empty() {
                &provider_id
            } else {
                label
            };
            crate::output::stderr!("warning: custom provider {label:?} unavailable: {error}");
            continue;
        }
        if let Err(error) = merge_provider_catalog(catalog, provider_catalog) {
            crate::output::stderr!("warning: custom provider {provider_id:?} unavailable: {error}");
        }
    }
    Ok(())
}

fn register_default_apple_foundation_models(
    catalog: &mut ModelCatalog,
    store: &crate::auth::custom::CredentialStore,
    offline: bool,
) -> anyhow::Result<()> {
    if offline
        || !cfg!(target_os = "macos")
        || catalog.has_endpoint(&EndpointId(crate::auth::custom::endpoint_id(
            APPLE_FM_PROVIDER_ID,
        )))
        || !apple_foundation_models_server_is_running()
    {
        return Ok(());
    }

    let provider = default_apple_foundation_models_provider();
    let mut provider_catalog = ModelCatalog::default();
    register_custom_openai_provider(
        &mut provider_catalog,
        store,
        APPLE_FM_PROVIDER_ID,
        &provider,
        false,
        false,
    )?;
    merge_provider_catalog(catalog, provider_catalog)
}

fn register_custom_openai_provider(
    catalog: &mut ModelCatalog,
    store: &crate::auth::custom::CredentialStore,
    provider_id: &str,
    provider: &crate::auth::custom::CustomProvider,
    legacy_single_endpoint: bool,
    offline: bool,
) -> anyhow::Result<()> {
    use crate::auth::custom::CustomModel;

    validate_custom_provider_id(provider_id)?;
    let (auth, effective_key) = resolve_custom_provider_auth(provider, legacy_single_endpoint)?;
    let mut cred = provider.credential.clone();
    // Discovery uses the resolved value in memory; it is never written back to
    // the provider registry. Requests use the redacted Auth strategy below.
    cred.api_key = effective_key.clone();

    let startup_timeout_env = if legacy_single_endpoint {
        optional_env("YGG_CUSTOM_STARTUP_TIMEOUT_SECS")?
    } else {
        None
    };
    let startup_timeout = resolve_custom_startup_timeout(
        provider.startup_timeout_secs,
        startup_timeout_env.as_deref(),
    )?;

    let base_url = if cred.base_url.ends_with('/') {
        url::Url::parse(&cred.base_url)
    } else {
        url::Url::parse(&format!("{}/", cred.base_url))
    }
    .map_err(|_| anyhow::anyhow!("invalid custom provider {provider_id:?} base URL"))?;

    let mut default_headers = http::HeaderMap::new();
    for header in &cred.headers {
        let name = http::HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid header name {}: {e}", header.name))?;
        let value = http::HeaderValue::from_str(&header.value)
            .map_err(|e| anyhow::anyhow!("invalid header value for {}: {e}", header.name))?;
        default_headers.insert(name, value);
    }
    let custom_credential_fingerprint =
        custom_credential_fingerprint(&effective_key, &default_headers);
    let endpoint_id = EndpointId(crate::auth::custom::endpoint_id(provider_id));

    catalog.register_endpoint(Endpoint {
        id: endpoint_id.clone(),
        base_url,
        auth,
        default_headers,
        transport: ygg_ai::EndpointTransport::Http,
        runtime: ygg_ai::RequestRuntime {
            lifecycle_feedback: provider.lifecycle_feedback,
            ..ygg_ai::RequestRuntime::default()
        },
        timeout: startup_timeout,
    })?;
    let label = provider.label.trim();
    let label = if label.is_empty() {
        if legacy_single_endpoint {
            "local endpoint"
        } else {
            provider_id
        }
    } else {
        label
    };
    catalog.set_endpoint_label(endpoint_id.clone(), label.to_owned())?;

    // A successful inventory is durable startup metadata, not something every
    // invocation should fetch again. The provider-specific cache path prevents
    // one endpoint's inventory from being used by another endpoint.
    let configured = configured_custom_models(&cred);
    let configured_overrides = configured.clone();
    let cache_fingerprint =
        custom_model_cache_fingerprint(&custom_credential_fingerprint, &configured);
    let cached = if cred.auto_discover {
        match load_custom_model_cache_for(store, provider_id, &cred.base_url, &cache_fingerprint) {
            Ok(models) => models,
            Err(error) => {
                crate::output::stderr!("warning: custom provider model cache unavailable: {error}");
                None
            }
        }
    } else {
        None
    };
    let models: Vec<CustomModel> = match cached {
        Some(CachedCustomInventory::Available(models)) => {
            if offline {
                models
            } else {
                refresh_stale_custom_models_with_for(
                    store,
                    provider_id,
                    &cred,
                    &cache_fingerprint,
                    models,
                    PROVIDER_INVENTORY_REFRESH_INTERVAL,
                    discover_models,
                )
            }
        }
        Some(CachedCustomInventory::Unavailable)
            if cred.auto_discover && !offline && configured.is_empty() =>
        {
            let discovered = discover_and_cache_custom_models_with_for(
                store,
                provider_id,
                &cred,
                &cache_fingerprint,
                false,
                discover_models,
            );
            if !discovered.is_empty() {
                discovered
            } else {
                configured
            }
        }
        Some(CachedCustomInventory::Unavailable) => {
            if !offline {
                schedule_custom_model_cache_refresh_for(
                    store.clone(),
                    provider_id.to_owned(),
                    cred.clone(),
                    custom_credential_fingerprint.clone(),
                    NEGATIVE_INVENTORY_REFRESH_INTERVAL,
                );
            }
            configured
        }
        None if cred.auto_discover && !offline => {
            let discovered = discover_and_cache_custom_models_with_for(
                store,
                provider_id,
                &cred,
                &cache_fingerprint,
                true,
                discover_models,
            );
            if discovered.is_empty() {
                configured
            } else {
                discovered
            }
        }
        None => configured,
    };
    let models = apply_configured_custom_model_overrides(
        apply_known_custom_model_defaults(&cred, models),
        &configured_overrides,
    );
    if models.is_empty() {
        return Ok(());
    }

    let cache = provider.cache.clone().unwrap_or_default();
    for model in &models {
        let configured_display =
            (!model.display_name.trim().is_empty()).then(|| model.display_name.trim().to_owned());
        let input_mods = if model.vision {
            ModalitySet::none().with(ygg_ai::Modality::Image)
        } else {
            ModalitySet::none()
        };

        catalog.register_model(ModelSpec {
            id: ModelId(custom_model_id(provider_id, legacy_single_endpoint, model)),
            endpoint: endpoint_id.clone(),
            api_name: model.api_name.clone(),
            display_name: configured_display,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: input_mods,
                output_modalities: ModalitySet::none(),
                tools: model.tools,
                parallel_tool_calls: model.tools && model.parallel_tool_calls,
                reasoning: custom_reasoning_capability(model),
                responses_lite: false,
                agent_delegation: None,
                structured_output: model.structured_output,

                deferred_tool_loading: false,
            },
            limits: ModelLimits {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
            },
            pricing: Some(custom_model_pricing(model)),
            cache: cache.clone(),
        })?;
    }
    Ok(())
}

/// Call GET /v1/models on the custom endpoint and convert the response into
/// `CustomModel` entries. Returns an empty Vec on any error (non-fatal).
fn discover_models(
    cred: &crate::auth::custom::CustomCredential,
) -> Vec<crate::auth::custom::CustomModel> {
    // Apple Foundation Models is an optional local integration. Its health
    // endpoint gives us a cheap, exact readiness signal, so do not issue the
    // noisier /v1/models request when `fm serve` is absent.
    if !custom_model_discovery_is_available(
        &cred.base_url,
        apple_foundation_models_server_is_running,
    ) {
        return Vec::new();
    }

    // Run blocking HTTP work on a separate thread so the reqwest::blocking
    // Client's internal tokio runtime is created and dropped outside the
    // outer #[tokio::main] async context, avoiding:
    //   "Cannot drop a runtime in a context where blocking is not allowed."
    let cred = cred.clone();
    std::thread::spawn(move || discover_models_blocking(&cred, true))
        .join()
        .unwrap_or_default()
}

fn discover_models_blocking(
    cred: &crate::auth::custom::CustomCredential,
    report_errors: bool,
) -> Vec<crate::auth::custom::CustomModel> {
    use crate::auth::custom::CustomModel;

    // Build the models URL following ygg's convention: base_url is versioned
    // (e.g. http://host/v1/) and we join the path segment.
    let base = if cred.base_url.ends_with('/') {
        cred.base_url.clone()
    } else {
        format!("{}/", cred.base_url)
    };
    let models_url = match url::Url::parse(&base).and_then(|u| u.join("models")) {
        Ok(u) => u.to_string(),
        Err(e) => {
            if report_errors {
                crate::output::stderr!("warning: auto-discover URL parse failed: {e}");
            }
            return Vec::new();
        }
    };

    let client = match blocking_discovery_client(std::time::Duration::from_secs(10)) {
        Ok(c) => c,
        Err(e) => {
            if report_errors {
                crate::output::stderr!("warning: auto-discover client build failed: {e}");
            }
            return Vec::new();
        }
    };

    let mut req = client.get(&models_url);
    let discovery_key = (!cred.api_key.trim().is_empty()).then(|| cred.api_key.clone());
    if let Some(key) = discovery_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    for h in &cred.headers {
        req = req.header(&h.name, &h.value);
    }

    let resp = match req
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
    {
        Ok(r) => r,
        Err(e) => {
            if report_errors {
                crate::output::stderr!("warning: auto-discover GET {} failed: {e}", models_url);
            }
            return Vec::new();
        }
    };

    let status = resp.status();
    let body = match bounded_discovery_json(resp, "custom models") {
        Ok(value) => value,
        Err(error) => {
            if report_errors {
                crate::output::stderr!(
                    "warning: auto-discover {} returned HTTP {} with an invalid or oversized body: {error}",
                    models_url,
                    status.as_u16()
                );
            }
            return Vec::new();
        }
    };

    let data = match body
        .get("data")
        .or_else(|| body.get("models"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| body.as_array())
    {
        Some(arr) => arr,
        None => {
            if report_errors {
                crate::output::stderr!(
                    "warning: auto-discover {} missing 'data'/'models' array",
                    models_url
                );
            }
            return Vec::new();
        }
    };

    let mut models = Vec::new();
    for entry in data {
        let id = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if id.is_empty() || id == "default" {
            continue;
        }

        let ctx = extract_ctx_from_model_entry(entry);
        let vision = entry
            .get("architecture")
            .and_then(|a| a.get("input_modalities"))
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().any(|v| v.as_str() == Some("image")))
            .unwrap_or(false)
            || model_id_implies_vision(id);

        let supported_parameters = entry
            .get("supported_parameters")
            .and_then(serde_json::Value::as_array);
        let supports = |name: &str| {
            supported_parameters.is_some_and(|parameters| {
                parameters
                    .iter()
                    .any(|parameter| parameter.as_str() == Some(name))
            })
        };
        let max_output_tokens =
            positive_u64(entry, &["max_output_tokens", "max_completion_tokens"])
                .unwrap_or(16_384)
                .min(ctx);
        let (reasoning, reasoning_values, reasoning_default) = discovered_custom_reasoning(entry);

        models.push(CustomModel {
            api_name: id.to_string(),
            display_name: id.to_string(),
            context_window: ctx,
            max_output_tokens,
            tools: custom_model_metadata_supports_tools(entry),
            parallel_tool_calls: supports("parallel_tool_calls"),
            vision,
            structured_output: supports("response_format"),
            reasoning,
            reasoning_configurable: reasoning,
            reasoning_values,
            reasoning_default,
            // Auto-discovered local models are not guaranteed to implement
            // OpenAI's newer `developer` role. vLLM Qwen chat templates, in
            // particular, reject it while still accepting `system`.
            reasoning_uses_system_message: true,
            pricing: None,
        });
    }
    apply_known_custom_model_defaults(cred, models)
}

/// Walk the model metadata looking for a context length. vLLM emits
/// `--max-model-len`, while llama.cpp-style servers expose `--ctx-size` or
/// `meta.n_ctx` through OpenAI-compatible gateways such as hlid.
fn extract_ctx_from_model_entry(entry: &serde_json::Value) -> u64 {
    let args = match entry
        .get("status")
        .and_then(|s| s.get("args"))
        .and_then(|a| a.as_array())
    {
        Some(a) => a,
        None => {
            // vLLM and hosted OpenAI-compatible APIs expose one of these
            // top-level names in their model object.
            return positive_u64(
                entry,
                &[
                    "max_model_len",
                    "context_window",
                    "context_length",
                    "max_context_tokens",
                ],
            )
            .or_else(|| {
                entry
                    .get("meta")
                    .and_then(|meta| positive_u64(meta, &["n_ctx", "n_ctx_train"]))
            })
            .unwrap_or(262_144);
        }
    };

    let mut next_is_ctx = false;
    for arg in args {
        let s = arg.as_str().unwrap_or("");
        if next_is_ctx {
            if let Ok(v) = s.parse::<u64>() {
                return v;
            }
            next_is_ctx = false;
        }
        if matches!(s, "--ctx-size" | "--max-model-len") {
            next_is_ctx = true;
        }
    }
    positive_u64(
        entry,
        &[
            "max_model_len",
            "context_window",
            "context_length",
            "max_context_tokens",
        ],
    )
    .or_else(|| {
        entry
            .get("meta")
            .and_then(|meta| positive_u64(meta, &["n_ctx", "n_ctx_train"]))
    })
    .unwrap_or(262_144) // sensible default for modern local models
}

// Codex's checked-in defaults are only a discovery fallback. The authenticated
// `/models` response is authoritative for plan-specific advertised limits; Ygg
// then applies the bounded working-window policy below.
const CODEX_LEGACY_CONTEXT_WINDOW: u64 = 272_000;
const CODEX_5_6_CONTEXT_WINDOW: u64 = 372_000;
const CODEX_PRO_CONTEXT_WINDOW: u64 = 1_000_000;
const CODEX_CONTEXT_WINDOW_CAP: u64 = 272_000;
const CODEX_MAX_OUTPUT_TOKENS: u64 = 128_000;
/// Codex retains the provider-advertised maximum as discovery metadata, while
/// Ygg deliberately budgets requests against Pi's 272K working window. Smaller
/// advertised windows remain authoritative.
const CODEX_MODEL_CACHE_VERSION: u8 = 3;
const CODEX_MODEL_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
// This is the Codex `/models` schema compatibility version Ygg implements,
// not Ygg's package version. Sending `0.1.0` causes the backend to filter out
// models that require a contemporary Codex client.
const CODEX_MODELS_CLIENT_VERSION: &str = "0.147.0";

pub(crate) fn effective_compaction_threshold_fraction(config: &Config, model: &Model) -> f64 {
    let Some(max_active_tokens) = config
        .compaction
        .max_active_tokens
        .filter(|tokens| *tokens > 0)
    else {
        return config.compaction.threshold_fraction;
    };
    let context_window = model.spec.limits.context_window.max(1);
    let absolute_fraction =
        (max_active_tokens.min(context_window) as f64) / (context_window as f64);
    config.compaction.threshold_fraction.min(absolute_fraction)
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiscoveredCodexModel {
    id: String,
    context_window: u64,
    max_context_window: u64,
    max_output_tokens: u64,
    min_effort: ygg_ai::ReasoningEffort,
    max_effort: ygg_ai::ReasoningEffort,
    responses_lite: bool,
    // `Option<T>` normally treats a missing key as `None`; the custom decoder
    // keeps explicit null valid while making incomplete dynamic metadata fail.
    #[serde(deserialize_with = "deserialize_required_agent_delegation")]
    agent_delegation: Option<AgentDelegation>,
}

fn deserialize_required_agent_delegation<'de, D>(
    deserializer: D,
) -> Result<Option<AgentDelegation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<AgentDelegation> as serde::Deserialize>::deserialize(deserializer)
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CodexModelCache {
    version: u8,
    account_id: String,
    plan: Option<String>,
    models: Vec<DiscoveredCodexModel>,
}

struct CodexDiscovery {
    claims: crate::auth::codex::SubscriptionClaims,
    models: Vec<DiscoveredCodexModel>,
}

fn positive_u64(entry: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        entry
            .get(*name)
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
    })
}

fn reasoning_effort(value: &str) -> Option<ygg_ai::ReasoningEffort> {
    match value.to_ascii_lowercase().as_str() {
        "minimal" | "none" => Some(ygg_ai::ReasoningEffort::Minimal),
        "low" => Some(ygg_ai::ReasoningEffort::Low),
        "medium" => Some(ygg_ai::ReasoningEffort::Medium),
        "high" => Some(ygg_ai::ReasoningEffort::High),
        "xhigh" | "extra_high" => Some(ygg_ai::ReasoningEffort::Xhigh),
        "max" => Some(ygg_ai::ReasoningEffort::Max),
        "ultra" => Some(ygg_ai::ReasoningEffort::Ultra),
        _ => None,
    }
}

fn codex_reasoning_range(
    entry: &serde_json::Value,
    model_id: &str,
) -> (ygg_ai::ReasoningEffort, ygg_ai::ReasoningEffort) {
    let fallback = (ygg_ai::ReasoningEffort::Minimal, codex_max_effort(model_id));
    let Some(levels) = entry
        .get("supported_reasoning_levels")
        .or_else(|| entry.get("supported_reasoning_efforts"))
    else {
        return fallback;
    };
    let Some(levels) = levels.as_array().filter(|levels| !levels.is_empty()) else {
        return fallback;
    };
    let mut efforts = Vec::with_capacity(levels.len());
    for level in levels {
        let Some(value) = level.as_str().or_else(|| {
            level
                .get("effort")
                .or_else(|| level.get("value"))
                .and_then(serde_json::Value::as_str)
        }) else {
            return fallback;
        };
        let Some(effort) = reasoning_effort(value) else {
            return fallback;
        };
        efforts.push(effort);
    }
    (
        efforts
            .iter()
            .copied()
            .min()
            .expect("non-empty validated reasoning levels"),
        efforts
            .iter()
            .copied()
            .max()
            .expect("non-empty validated reasoning levels"),
    )
}

fn codex_models_from_response(
    body: &serde_json::Value,
    plan: Option<&crate::auth::codex::ChatGptPlan>,
) -> anyhow::Result<Vec<DiscoveredCodexModel>> {
    // The subscription backend uses `models`, while OpenAI-compatible proxies
    // commonly expose the same inventory under `data`. Accepting both keeps
    // OAuth discovery working through enterprise gateways as well.
    let entries = body
        .get("models")
        .or_else(|| body.get("data"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Codex models response has no models array"))?;
    let mut models = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry
            .as_str()
            .or_else(|| {
                entry
                    .get("slug")
                    .or_else(|| entry.get("id"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let fallback = codex_model_context_limits(id);
        let advertised_context = positive_u64(
            entry,
            &["context_window", "context_length", "max_context_tokens"],
        );
        let advertised_max = positive_u64(entry, &["max_context_window"]);
        let (default_context_window, max_context_window) =
            match (advertised_context, advertised_max) {
                (Some(context), Some(maximum)) => (context.min(maximum), maximum),
                (Some(context), None) => (context, context),
                (None, Some(maximum)) => (maximum, maximum),
                (None, None) => fallback,
            };
        let context_window =
            codex_context_window_for_plan(default_context_window, max_context_window, plan);
        let max_output_tokens =
            positive_u64(entry, &["max_output_tokens", "max_completion_tokens"])
                .unwrap_or(CODEX_MAX_OUTPUT_TOKENS)
                .min(context_window);
        let agent_delegation = entry
            .get("multi_agent_version")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|version| version.eq_ignore_ascii_case("v2"))
            .then_some(AgentDelegation::V2);
        let (mut min_effort, mut max_effort) = codex_reasoning_range(entry, id);
        if agent_delegation != Some(AgentDelegation::V2) {
            max_effort = max_effort.min(ygg_ai::ReasoningEffort::Max);
            min_effort = min_effort.min(max_effort);
        }
        let responses_lite = entry
            .get("use_responses_lite")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        models.push(DiscoveredCodexModel {
            id: id.to_owned(),
            context_window,
            max_context_window,
            max_output_tokens,
            min_effort,
            max_effort,
            responses_lite,
            agent_delegation,
        });
    }
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    if models.is_empty() {
        anyhow::bail!("Codex models response contained no usable models");
    }
    Ok(models)
}

fn codex_model_context_limits(model_id: &str) -> (u64, u64) {
    if model_id == "gpt-5.4" || model_id == "codex-auto-review" {
        (CODEX_LEGACY_CONTEXT_WINDOW, CODEX_PRO_CONTEXT_WINDOW)
    } else if model_id.starts_with("gpt-5.6-") {
        (CODEX_5_6_CONTEXT_WINDOW, CODEX_5_6_CONTEXT_WINDOW)
    } else {
        (CODEX_LEGACY_CONTEXT_WINDOW, CODEX_LEGACY_CONTEXT_WINDOW)
    }
}

fn codex_context_window_for_plan(
    default_context_window: u64,
    max_context_window: u64,
    plan: Option<&crate::auth::codex::ChatGptPlan>,
) -> u64 {
    let selected = if plan.is_some_and(crate::auth::codex::ChatGptPlan::uses_max_context_window) {
        max_context_window
    } else {
        default_context_window
    };
    selected.min(CODEX_CONTEXT_WINDOW_CAP)
}

fn codex_model_limits(
    model_id: &str,
    plan: Option<&crate::auth::codex::ChatGptPlan>,
) -> (ModelLimits, u64) {
    let (default_context_window, max_context_window) = codex_model_context_limits(model_id);
    (
        ModelLimits {
            context_window: codex_context_window_for_plan(
                default_context_window,
                max_context_window,
                plan,
            ),
            max_output_tokens: CODEX_MAX_OUTPUT_TOKENS,
        },
        max_context_window,
    )
}

// New Codex families accept the top `max` effort tier. Live discovery narrows
// this range when the backend publishes explicit supported reasoning levels.
fn codex_max_effort(model_id: &str) -> ygg_ai::ReasoningEffort {
    if model_id.starts_with("gpt-5.6-") {
        ygg_ai::ReasoningEffort::Max
    } else {
        ygg_ai::ReasoningEffort::High
    }
}

/// Current Codex vision-capable families. The Codex backend's inventory does
/// not reliably include modality metadata, so keep this capability aligned
/// with the provider's published model contract instead of defaulting every
/// OAuth model to text-only.
fn codex_supports_image_input(model_id: &str) -> bool {
    model_id == "codex-mini-latest"
        || model_id.starts_with("gpt-5.4")
        || model_id.starts_with("gpt-5.5")
        || model_id.starts_with("gpt-5.6")
        || model_id.starts_with("gpt-5.3-codex")
        || model_id.starts_with("gpt-5.2-codex")
        || model_id.starts_with("gpt-5.1-codex")
}

fn codex_plan_cache_key(claims: &crate::auth::codex::SubscriptionClaims) -> Option<&str> {
    claims.plan.as_ref().map(|plan| plan.raw_value())
}

fn save_codex_model_cache(
    store: &crate::auth::codex::CredentialStore,
    discovery: &CodexDiscovery,
) -> anyhow::Result<()> {
    let cache = CodexModelCache {
        version: CODEX_MODEL_CACHE_VERSION,
        account_id: discovery.claims.account_id.clone(),
        plan: codex_plan_cache_key(&discovery.claims).map(str::to_owned),
        models: discovery.models.clone(),
    };
    store.save_model_cache(&serde_json::to_vec_pretty(&cache)?)
}

fn load_codex_model_cache(
    store: &crate::auth::codex::CredentialStore,
    claims: &crate::auth::codex::SubscriptionClaims,
) -> anyhow::Result<Option<Vec<DiscoveredCodexModel>>> {
    let Some(bytes) = store.load_fresh_model_cache(CODEX_MODEL_CACHE_REFRESH_INTERVAL)? else {
        return Ok(None);
    };
    let cache: CodexModelCache =
        serde_json::from_slice(&bytes).context("invalid Codex model cache")?;
    if cache.version != CODEX_MODEL_CACHE_VERSION
        || cache.account_id != claims.account_id
        || cache.plan.as_deref() != codex_plan_cache_key(claims)
        || cache.models.is_empty()
    {
        return Ok(None);
    }
    let mut ids = std::collections::BTreeSet::new();
    for model in &cache.models {
        if model.id.trim() != model.id
            || model.id.is_empty()
            || !ids.insert(model.id.as_str())
            || model.context_window == 0
            || model.max_context_window < model.context_window
            || model.max_output_tokens == 0
            || model.max_output_tokens > model.context_window
            || model.min_effort > model.max_effort
            || (model.max_effort == ygg_ai::ReasoningEffort::Ultra
                && model.agent_delegation != Some(AgentDelegation::V2))
        {
            anyhow::bail!("invalid Codex model cache: incomplete or inconsistent model metadata");
        }
    }
    Ok(Some(cache.models))
}

fn conservative_offline_codex_models(
    mut models: Vec<DiscoveredCodexModel>,
) -> Vec<DiscoveredCodexModel> {
    for model in &mut models {
        model.responses_lite = false;
        model.agent_delegation = None;
        model.max_effort = model.max_effort.min(ygg_ai::ReasoningEffort::Max);
        model.min_effort = model.min_effort.min(model.max_effort);
    }
    models
}

fn fallback_codex_models(
    plan: Option<&crate::auth::codex::ChatGptPlan>,
) -> Vec<DiscoveredCodexModel> {
    crate::auth::codex::MODELS
        .iter()
        .map(|model_id| {
            let (limits, max_context_window) = codex_model_limits(model_id, plan);
            DiscoveredCodexModel {
                id: (*model_id).to_owned(),
                context_window: limits.context_window,
                max_context_window,
                max_output_tokens: limits.max_output_tokens,
                min_effort: ygg_ai::ReasoningEffort::Minimal,
                max_effort: codex_max_effort(model_id),
                responses_lite: false,
                agent_delegation: None,
            }
        })
        .collect()
}

fn codex_models_url() -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse(crate::providers::CODEX.base_url)?.join("models")?;
    url.query_pairs_mut()
        .append_pair("client_version", CODEX_MODELS_CLIENT_VERSION);
    Ok(url)
}

fn discover_codex_models(
    store: crate::auth::codex::CredentialStore,
) -> anyhow::Result<CodexDiscovery> {
    std::thread::spawn(move || -> anyhow::Result<CodexDiscovery> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async move {
            let resolver = crate::auth::codex::CodexResolver::new(store);
            let (mut headers, claims) = resolver.discovery_headers().await?;
            let static_headers =
                crate::providers::public_headers(crate::providers::CODEX.extra_headers)?;
            for (name, value) in &static_headers {
                headers.insert(name.clone(), value.clone());
            }
            headers.insert(
                http::header::USER_AGENT,
                http::HeaderValue::from_str(&codex_user_agent())?,
            );

            let url = codex_models_url()?;
            let response = discovery_client(DISCOVERY_TIMEOUT)?
                .get(url)
                .headers(headers)
                .send()
                .await
                .map_err(|error| anyhow::anyhow!("GET Codex models failed: {error}"))?
                .error_for_status()
                .map_err(|error| anyhow::anyhow!("GET Codex models failed: {error}"))?;
            let body = bounded_discovery_json_async(response, "Codex models").await?;
            let models = codex_models_from_response(&body, claims.plan.as_ref())?;
            Ok(CodexDiscovery { claims, models })
        })
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Codex model discovery thread panicked"))?
}

fn codex_user_agent() -> String {
    format!(
        "ygg/{} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

/// Register the OpenAI Codex (Sign in with ChatGPT) endpoint and discover the
/// account's current model inventory, but only for a validated subscription
/// credential. Codex-specific headers are composed from static endpoint
/// headers, request-scoped session affinity, and resolver account routing.
fn register_openai_codex(
    catalog: &mut ModelCatalog,
    store: crate::auth::codex::CredentialStore,
    offline: bool,
) -> anyhow::Result<()> {
    use crate::auth::codex;

    let declaration = &crate::providers::CODEX;
    declaration.validate().map_err(|error| {
        anyhow::anyhow!("invalid {} provider declaration: {error}", declaration.id)
    })?;
    let route = declaration.inventory_route().ok_or_else(|| {
        anyhow::anyhow!(
            "{} provider declaration has no subscription route",
            declaration.id
        )
    })?;

    let Some(initial_claims) = codex::usable_subscription_claims(&store)? else {
        return Ok(());
    };

    // Tests use synthetic JWTs and must not contact the production catalog.
    // At runtime only a fresh, account-and-plan-matched cache is authoritative
    // for a launch. Stale or future-dated metadata is synchronously refreshed
    // online and reduced to the conservative fallback offline, so dynamic
    // capabilities can never survive past the freshness boundary. A first
    // launch performs one bounded discovery to seed the cache.
    let models = if offline {
        match load_codex_model_cache(&store, &initial_claims) {
            Ok(Some(models)) => conservative_offline_codex_models(models),
            Ok(None) => fallback_codex_models(initial_claims.plan.as_ref()),
            Err(error) => {
                crate::output::stderr!(
                    "warning: Codex model cache was unusable ({error}); using conservative offline fallback catalog"
                );
                fallback_codex_models(initial_claims.plan.as_ref())
            }
        }
    } else if cfg!(test) {
        fallback_codex_models(initial_claims.plan.as_ref())
    } else {
        match load_codex_model_cache(&store, &initial_claims) {
            Ok(Some(models)) => models,
            cache_result => match discover_codex_models(store.clone()) {
                Ok(discovery) => {
                    if let Err(error) = save_codex_model_cache(&store, &discovery) {
                        crate::output::stderr!(
                            "warning: could not persist Codex model metadata: {error}"
                        );
                    }
                    discovery.models
                }
                Err(discovery_error) => {
                    if let Err(cache_error) = cache_result {
                        crate::output::stderr!(
                            "warning: Codex model cache was unusable ({cache_error}); live discovery also failed ({discovery_error}); using conservative fallback catalog"
                        );
                    } else {
                        crate::output::stderr!(
                            "warning: Codex model auto-discovery failed; using conservative fallback catalog: {discovery_error}"
                        );
                    }
                    // Discovery may have refreshed a token before the inventory
                    // request failed, so re-read claims for the fallback limits.
                    let current_claims = codex::usable_subscription_claims(&store)
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| initial_claims.clone());
                    fallback_codex_models(current_claims.plan.as_ref())
                }
            },
        }
    };
    let resolver = std::sync::Arc::new(codex::CodexResolver::new(store));

    let mut default_headers = crate::providers::public_headers(declaration.extra_headers)?;
    default_headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_str(&codex_user_agent())?,
    );

    catalog.register_endpoint(Endpoint {
        id: EndpointId(route.endpoint_id.into()),
        base_url: url::Url::parse(declaration.base_url)?,
        auth: Auth::dynamic(resolver),
        default_headers,
        // The declaration prefers the cached Responses WebSocket. AiClient
        // retains HTTP/SSE as a conservative fallback for unavailable or
        // provider-rejected sockets.
        transport: route.transport,
        runtime: route.runtime,
        timeout: PROVIDER_RESPONSE_HEADER_TIMEOUT,
    })?;

    for model in models {
        // Preserve familiar bare Codex ids when possible. If another API
        // already owns one, namespace only the colliding entry instead of
        // rejecting the account's entire live catalog.
        let catalog_id = if catalog.resolve(&ModelId(model.id.clone())).is_ok() {
            ModelId(format!("{}/{}", declaration.id, model.id))
        } else {
            ModelId(model.id.clone())
        };
        let pricing = crate::providers::pricing_for(declaration, &model.id);
        let supports_image_input = codex_supports_image_input(&model.id);
        // The declaration keeps application session identity separate from the
        // resolver's credential/account routing.
        let cache = crate::providers::cache_compatibility(
            declaration.compatibility,
            &model.id,
            route.protocol,
        );
        catalog.register_model(ModelSpec {
            id: catalog_id,
            endpoint: EndpointId(route.endpoint_id.into()),
            api_name: model.id,
            display_name: None,
            protocol: route.protocol,
            capabilities: Capabilities {
                input_modalities: if supports_image_input {
                    ModalitySet::none().with(ygg_ai::Modality::Image)
                } else {
                    ModalitySet::none()
                },
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: Some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: true,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::Standard,
                    min_effort: model.min_effort,
                    max_effort: model.max_effort,
                }),
                responses_lite: model.responses_lite,
                agent_delegation: model.agent_delegation,
                structured_output: false,

                deferred_tool_loading: false,
            },
            limits: ModelLimits {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
            },
            pricing,
            cache,
        })?;
    }
    Ok(())
}

fn base_model_catalog_with_custom_store(
    offline: bool,
    explicit_custom_store: Option<&crate::auth::custom::CredentialStore>,
) -> anyhow::Result<ModelCatalog> {
    let mut catalog = ModelCatalog::builtin()?;
    // The embedded catalog describes supported integrations, not enabled
    // accounts. Do not offer a cloud model until its endpoint can resolve a
    // credential from this process's environment. Unit tests intentionally
    // retain the complete fixture catalog so they can exercise protocol and
    // session behavior without ambient secrets.
    #[cfg(not(test))]
    catalog.retain_configured_models();
    if cfg!(test) {
        // Tests keep the historical deterministic DeepSeek fixture and never
        // use ambient credentials or contact provider discovery endpoints.
        let declaration = &crate::providers::DEEPSEEK;
        let credential = crate::providers::EnvironmentCredential::for_test(
            "DEEPSEEK_API_KEY",
            "test-deepseek-key",
        );
        let base_url = deepseek_base_url(declaration)?;
        crate::providers::register_environment_endpoints_at_base_url(
            &mut catalog,
            declaration,
            &credential,
            &base_url,
            PROVIDER_RESPONSE_HEADER_TIMEOUT,
        )?;
        register_deepseek_v4_pro(&mut catalog, declaration)?;
    } else if !offline {
        register_configured_presets_parallel(&mut catalog);
    }

    // Explicit custom models remain usable offline; only auto-discovery is skipped.
    // Normal tests never inspect ambient HOME credentials, while provider setup
    // passes its explicit existing store so it can rebuild the same catalog
    // immediately without introducing a second registry or catalog type.
    if let Some(store) = explicit_custom_store {
        if let Err(error) =
            register_custom_openai_endpoints_from_store(&mut catalog, store, offline)
        {
            crate::output::stderr!("warning: custom provider registry unavailable: {error}");
        }
        if !cfg!(test) {
            if let Err(error) =
                register_default_apple_foundation_models(&mut catalog, store, offline)
            {
                crate::output::stderr!("warning: Apple Foundation Models unavailable: {error}");
            }
            catalog.retain_configured_models();
        }
    } else if !cfg!(test) {
        let store = crate::auth::custom::CredentialStore::new(crate::auth::custom::default_path());
        if let Err(error) =
            register_custom_openai_endpoints_from_store(&mut catalog, &store, offline)
        {
            crate::output::stderr!("warning: custom provider registry unavailable: {error}");
        }
        if let Err(error) = register_default_apple_foundation_models(&mut catalog, &store, offline)
        {
            crate::output::stderr!("warning: Apple Foundation Models unavailable: {error}");
        }
        // Custom providers may use provider-scoped environment credentials;
        // hide models whose referenced variable is not configured, just as the
        // built-in provider catalog does above.
        catalog.retain_configured_models();
    }
    Ok(catalog)
}

fn base_model_catalog(offline: bool) -> anyhow::Result<ModelCatalog> {
    base_model_catalog_with_custom_store(offline, None)
}

/// Build the runtime model catalog, exposing ChatGPT subscription models only
/// when Ygg owns a usable OAuth credential.
pub fn model_catalog() -> anyhow::Result<ModelCatalog> {
    model_catalog_with_offline(false)
}

pub fn model_catalog_with_offline(offline: bool) -> anyhow::Result<ModelCatalog> {
    let mut catalog = base_model_catalog(offline)?;
    register_codex_catalog(&mut catalog, offline);
    Ok(catalog)
}

/// Rebuild the canonical catalog against one explicit existing custom store.
/// Provider setup uses this after its final atomic write so a selected model is
/// immediately available without restarting or constructing a partial Agent.
pub(crate) fn model_catalog_with_setup_store(
    custom_store: &crate::auth::custom::CredentialStore,
    offline: bool,
) -> anyhow::Result<ModelCatalog> {
    let mut catalog = base_model_catalog_with_custom_store(offline, Some(custom_store))?;
    register_codex_catalog(&mut catalog, offline);
    Ok(catalog)
}

fn register_codex_catalog(catalog: &mut ModelCatalog, offline: bool) {
    // Unit tests use explicit temporary credential stores and must never inspect
    // the developer's ambient HOME. Runtime offline mode still registers a
    // locally authenticated Codex endpoint, but never discovers or refreshes
    // its inventory over the network.
    if !cfg!(test) {
        let store = crate::auth::codex::CredentialStore::new(crate::auth::codex::default_path());
        // Non-fatal: a stale or malformed OAuth file must never block Ygg startup.
        if let Err(error) = register_openai_codex(catalog, store, offline) {
            crate::output::stderr!("warning: OpenAI Codex models unavailable: {error}");
        }
    }
}

/// Build the catalog without subscription models, used to make `/logout`
/// atomic when the active model itself belongs to ChatGPT.
pub fn model_catalog_without_codex() -> anyhow::Result<ModelCatalog> {
    base_model_catalog(false)
}

/// Build bootstrap state from resolved configuration.
pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap> {
    let catalog = model_catalog_with_offline(config.offline)?;
    let sessions = SessionStore::new(&config.session_dir, &config.workspace);
    // Record the workspace path so cross-workspace browsing can name each
    // session's home. Non-fatal: pickers fall back to directory names.
    if let Err(error) = sessions.write_workspace_marker() {
        crate::output::stderr!("warning: could not write session workspace marker: {error}");
    }
    let client = AiClient::try_new()?;
    Ok(Bootstrap {
        config,
        catalog,
        sessions,
        client,
        provider_runtime: ExtensionProviderRuntime::default(),
        prestarted_extensions: RefCell::new(None),
        prepared_session: RefCell::new(None),
        modeless: std::cell::Cell::new(false),
    })
}

/// Resolve model configuration precedence. The caller supplies values from
/// distinct configuration layers; explicit CLI selection always wins.
pub fn resolve_model_id(
    cli: Option<ModelId>,
    project: Option<ModelId>,
    global: Option<ModelId>,
) -> Option<ModelId> {
    cli.or(project).or(global)
}

#[derive(Default)]
struct PersistedSessionConfig {
    model: Option<ModelId>,
    reasoning: Option<ReasoningConfig>,
    reasoning_mode: Option<ReasoningMode>,
}

fn persisted_session_config(session: &Session) -> anyhow::Result<PersistedSessionConfig> {
    let path = session.path();
    let mut persisted = PersistedSessionConfig::default();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session
            .entry(id)
            .ok_or_else(|| anyhow::anyhow!("session head references missing entry {}", id.0))?;
        if let EntryValue::Config {
            model,
            reasoning,
            reasoning_mode,
        } = &entry.value
        {
            if persisted.model.is_none() {
                persisted.model = model.clone().map(ModelId);
            }
            if persisted.reasoning.is_none() {
                persisted.reasoning = reasoning
                    .as_deref()
                    .map(crate::config::parse_reasoning)
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "invalid reasoning state in session {} at entry {}: {error}",
                            path.display(),
                            id.0
                        )
                    })?;
            }
            if persisted.reasoning_mode.is_none() {
                persisted.reasoning_mode = reasoning_mode
                    .as_deref()
                    .map(crate::config::parse_reasoning_mode)
                    .transpose()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "invalid reasoning mode in session {} at entry {}: {error}",
                            path.display(),
                            id.0
                        )
                    })?;
            }
            if persisted.model.is_some()
                && persisted.reasoning.is_some()
                && persisted.reasoning_mode.is_some()
            {
                break;
            }
        }
        cursor = entry.parent.as_ref();
    }
    Ok(persisted)
}

fn append_config_if_changed(
    session: &mut Session,
    model: &ModelId,
    reasoning: &ReasoningConfig,
    reasoning_mode: ReasoningMode,
) -> anyhow::Result<()> {
    let persisted = persisted_session_config(session)?;
    if persisted.model.as_ref() == Some(model)
        && persisted.reasoning.as_ref() == Some(reasoning)
        && persisted.reasoning_mode == Some(reasoning_mode)
    {
        return Ok(());
    }
    session.append(EntryValue::Config {
        model: Some(model.0.clone()),
        reasoning: Some(crate::app::reasoning_label(reasoning)),
        reasoning_mode: Some(
            match reasoning_mode {
                ReasoningMode::Standard => "standard",
                ReasoningMode::Pro => "pro",
            }
            .to_owned(),
        ),
    })?;
    Ok(())
}

fn launch_configuration_parts(
    config: &Config,
    session: &SessionSelection,
) -> anyhow::Result<(
    Option<Session>,
    Option<ModelId>,
    ReasoningConfig,
    ReasoningMode,
)> {
    let prepared = match session {
        SessionSelection::OpenExisting(path) => {
            let descriptor_path = descriptor_session_path(path)?;
            let file = open_regular_file_for_append(&descriptor_path)?;
            Some(Session::open_with_file(path, file)?)
        }
        SessionSelection::CreateNew(_) => None,
    };
    let persisted = prepared
        .as_ref()
        .map(persisted_session_config)
        .transpose()?
        .unwrap_or_default();
    let model = if config.model_explicit {
        config.model.clone()
    } else {
        persisted.model.or_else(|| config.model.clone())
    };
    let reasoning = if config.reasoning_explicit {
        config.reasoning.clone()
    } else {
        persisted
            .reasoning
            .unwrap_or_else(|| config.reasoning.clone())
    };
    let reasoning_mode = if config.reasoning_mode_explicit {
        config.reasoning_mode
    } else if config.reasoning_explicit {
        // A current explicit effort selection supersedes the obsolete persisted
        // Pro bit; otherwise migration would silently replace the user's
        // requested effort with Ultra.
        ReasoningMode::Standard
    } else {
        persisted.reasoning_mode.unwrap_or(config.reasoning_mode)
    };
    Ok((prepared, model, reasoning, reasoning_mode))
}

fn should_pick_interactive_model(
    config: &Config,
    catalog: &ModelCatalog,
    model: Option<&ModelId>,
) -> bool {
    match model {
        None => true,
        // Keep an explicit CLI selection authoritative so invalid values still
        // produce the usual configuration error rather than silently changing
        // the requested model.
        Some(_) if config.model_explicit => false,
        // A session may outlive the credential that made its model available.
        // Do not carry that stale model into build_app; let the user choose a
        // currently configured route instead.
        Some(model) => catalog.resolve(model).is_err(),
    }
}

fn launch_configuration(
    boot: &Bootstrap,
    session: &SessionSelection,
) -> anyhow::Result<(Option<ModelId>, ReasoningConfig, ReasoningMode)> {
    let (prepared, model, reasoning, reasoning_mode) =
        launch_configuration_parts(&boot.config, session)?;
    *boot.prepared_session.borrow_mut() = prepared;
    Ok((model, reasoning, reasoning_mode))
}

fn resolve_fork_source_path(
    config: &Config,
    store: &SessionStore,
    source: &str,
) -> anyhow::Result<PathBuf> {
    let candidate = Path::new(source);
    if candidate.is_absolute()
        || candidate.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
    {
        let path = if candidate.is_absolute() {
            candidate.to_owned()
        } else {
            config.invocation_cwd.join(candidate)
        };
        return path
            .canonicalize()
            .with_context(|| format!("could not resolve fork source {}", path.display()));
    }
    store.path_by_id(source)
}

fn fork_session_into(
    store: &SessionStore,
    source_path: &std::path::Path,
    destination_path: PathBuf,
) -> anyhow::Result<PathBuf> {
    let source = Session::open_read_only(source_path).with_context(|| {
        format!(
            "could not open source session for forking: {}",
            source_path.display()
        )
    })?;
    let source_id = source
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| anyhow::anyhow!("source session has no valid id"))?;
    let checkpoint = source.head();
    let forked = source
        .fork_to(destination_path.clone(), checkpoint.as_ref())
        .with_context(|| {
            format!(
                "could not fork session {} into {}",
                source_path.display(),
                destination_path.display()
            )
        })?;
    drop(forked);
    if let Some(checkpoint) = checkpoint {
        let destination_id = destination_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow::anyhow!("forked session has no valid id"))?;
        if let Err(error) =
            store.set_fork_provenance(destination_id, source_id, checkpoint.0.as_str())
        {
            let _ = std::fs::remove_file(&destination_path);
            return Err(error);
        }
    }
    Ok(destination_path)
}

/// Resolve an interactive launch and open pickers only while no Agent exists.
pub async fn resolve_launch_interactive(
    boot: &Bootstrap,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<LaunchSelection> {
    let session = match boot.config.resume.clone() {
        ResumeSelector::New => {
            SessionSelection::CreateNew(boot.sessions.new_path(&crate::modes::timestamp()))
        }
        ResumeSelector::Continue => {
            let sessions = boot.sessions.clone();
            let path =
                run_blocking_lifecycle(shell, input, "finding latest session…", move || {
                    Ok(sessions.latest()?.path)
                })
                .await?;
            SessionSelection::OpenExisting(path)
        }
        ResumeSelector::Resume(Some(id)) => {
            let sessions = boot.sessions.clone();
            let path = run_blocking_lifecycle(shell, input, "opening session…", move || {
                sessions.path_by_id(&id)
            })
            .await?;
            SessionSelection::OpenExisting(path)
        }
        ResumeSelector::Fork(source_id) => {
            let source_path = if let Some(id) = source_id {
                let sessions = boot.sessions.clone();
                let config = boot.config.clone();
                run_blocking_lifecycle(shell, input, "opening source session…", move || {
                    resolve_fork_source_path(&config, &sessions, &id)
                })
                .await?
            } else {
                let sessions = boot.sessions.clone();
                let available =
                    run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
                        Ok(sessions.list())
                    })
                    .await?;
                session_picker(shell, input, &available, &boot.sessions, None)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("session selection cancelled"))?
            };
            let store = boot.sessions.clone();
            let destination = boot.sessions.new_path(&crate::modes::timestamp());
            let path = run_blocking_lifecycle(shell, input, "forking session…", move || {
                fork_session_into(&store, &source_path, destination)
            })
            .await?;
            SessionSelection::OpenExisting(path)
        }
        ResumeSelector::Resume(None) => {
            let sessions = boot.sessions.clone();
            let available =
                run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
                    Ok(sessions.list())
                })
                .await?;
            session_picker(shell, input, &available, &boot.sessions, None)
                .await?
                .map(SessionSelection::OpenExisting)
                .ok_or_else(|| anyhow::anyhow!("session selection cancelled"))?
        }
    };
    let config = boot.config.clone();
    let selected_session = session.clone();
    let (prepared, model, reasoning, reasoning_mode) =
        run_blocking_lifecycle(shell, input, "replaying session…", move || {
            launch_configuration_parts(&config, &selected_session)
        })
        .await?;
    *boot.prepared_session.borrow_mut() = prepared;
    // Provider declarations are only available after API 0.3 extension
    // activation; perform that boundary before presenting a model picker.
    if !boot.config.enabled_extensions.is_empty() {
        boot.preflight_extension_providers()?;
    }
    let catalog = boot.catalog_with_extension_providers();
    let no_configured_model = !boot.config.model_explicit && catalog.models().next().is_none();
    let pick_model = should_pick_interactive_model(&boot.config, &catalog, model.as_ref());
    let model = if no_configured_model {
        boot.enter_modeless_mode();
        shell.notice("no configured model; opening session read-only");
        shell.render();
        model.unwrap_or_else(|| ModelId(String::new()))
    } else {
        match model {
            Some(model) if !pick_model => model,
            Some(_) => {
                shell.notice("selected model is unavailable; select a configured model");
                shell.render();
                model_picker(shell, input, &catalog).await?
            }
            None => model_picker(shell, input, &catalog).await?,
        }
    };
    Ok(LaunchSelection {
        model,
        session,
        reasoning,
        reasoning_mode,
    })
}

/// Resolve a print launch without opening an interactive picker.
pub fn resolve_launch_print(boot: &Bootstrap, stamp: &str) -> anyhow::Result<LaunchSelection> {
    let session = match &boot.config.resume {
        ResumeSelector::New => SessionSelection::CreateNew(boot.sessions.new_path(stamp)),
        ResumeSelector::Continue => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => {
            SessionSelection::OpenExisting(boot.sessions.path_by_id(id)?)
        }
        ResumeSelector::Fork(Some(id)) => {
            let source = resolve_fork_source_path(&boot.config, &boot.sessions, id)?;
            let destination = boot.sessions.new_path(stamp);
            SessionSelection::OpenExisting(fork_session_into(&boot.sessions, &source, destination)?)
        }
        ResumeSelector::Fork(None) => {
            anyhow::bail!("--fork needs a session id in print mode")
        }
        ResumeSelector::Resume(None) => {
            anyhow::bail!("--resume needs a session id in print mode")
        }
    };
    let (model, reasoning, reasoning_mode) = launch_configuration(boot, &session)?;
    let catalog = if model
        .as_ref()
        .is_some_and(|model| boot.catalog.resolve(model).is_err())
    {
        boot.preflight_extension_providers()?;
        boot.catalog_with_extension_providers()
    } else {
        boot.catalog.clone()
    };
    let model = model.ok_or_else(|| {
        let mut models = catalog
            .models()
            .map(|model| model.id.0.clone())
            .collect::<Vec<_>>();
        models.sort();
        let diagnosis = match crate::provider_setup::ProviderSetupService::<
            crate::provider_setup::HttpSetupProbe,
        >::readiness(&catalog, None)
        {
            crate::provider_setup::ProviderSetupState::NoProvider => {
                "no provider is configured; run `ygg setup --yes` for an explicitly selected OpenAI-compatible endpoint".to_owned()
            }
            crate::provider_setup::ProviderSetupState::Ready => {
                "no model configured; run `ygg setup --yes` for an explicitly selected OpenAI-compatible endpoint".to_owned()
            }
            state => state.to_string(),
        };
        anyhow::anyhow!(
            "{diagnosis}, pass --model <id>, resume a session with model provenance, or set model in .ygg/config.toml (available: {})",
            models.join(", ")
        )
    })?;
    if catalog.resolve(&model).is_err() {
        let mut available = catalog
            .models()
            .map(|model| model.id.0.clone())
            .collect::<Vec<_>>();
        available.sort();
        let diagnosis = crate::provider_setup::ProviderSetupService::<
            crate::provider_setup::HttpSetupProbe,
        >::readiness(&catalog, Some(&model));
        anyhow::bail!(
            "{diagnosis}; run `ygg setup --yes` or choose an available model (available: {})",
            available.join(", ")
        );
    }

    Ok(LaunchSelection {
        model,
        session,
        reasoning,
        reasoning_mode,
    })
}

/// Conservative character-based token estimate used for capacity reserves.
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Estimate the reserved serialized size of the exact tool schemas registered
/// for the agent, including optional product extensions such as skills.
pub fn tool_schema_reserve(definitions: &[ToolDef]) -> u64 {
    estimate_text_tokens(&serde_json::to_string(definitions).unwrap_or_default())
}

fn create_private_session_dir(path: &std::path::Path) -> std::io::Result<()> {
    ygg_agent::secure_fs::create_private_directory_all(path).map_err(std::io::Error::other)
}

fn descriptor_session_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path must be absolute and identify a file",
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path must not contain traversal components",
        ));
    }
    Ok(path.to_owned())
}

fn validate_explicit_tool_policy(
    config: &Config,
    extensions: &ExtensionHost,
    model: &Model,
    has_dynamic_tool_provider: bool,
) -> anyhow::Result<()> {
    let Some(requested) = config.tools.explicit_names() else {
        return Ok(());
    };
    let requested = requested.collect::<Vec<_>>();
    if !model.spec.capabilities.tools && !requested.is_empty() {
        anyhow::bail!(
            "model {} does not support tools, but the explicit tool policy requested: {}",
            model.spec.id.0,
            requested.join(", "),
        );
    }
    let registered = extensions
        .tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<std::collections::BTreeSet<_>>();
    let missing = requested
        .into_iter()
        .filter(|name| {
            !registered.contains(*name)
                && (!has_dynamic_tool_provider || !config.tool_available(name))
        })
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        let available = if registered.is_empty() {
            "(none)".to_owned()
        } else {
            registered.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        anyhow::bail!(
            "requested tool(s) are unavailable after allowlists, sandbox gates, and extension registration: {}; available tools: {available}",
            missing.join(", "),
        )
    }
}

fn apply_extension_tool_policy(host: &mut ExtensionHost, config: &Config, model: &Model) {
    let tool_config = config.clone();
    let model_supports_tools = model.spec.capabilities.tools;
    host.set_tool_policy(move |name| model_supports_tools && tool_config.tool_available(name));
    host.finalize_tool_surface();
}

fn configured_extension_host(config: &Config, model: &Model) -> anyhow::Result<ExtensionHost> {
    let mut host = ExtensionHost::new();
    host.load(&CoreTools);
    apply_extension_tool_policy(&mut host, config, model);
    if let Some(path) = config.telemetry.as_deref() {
        host.observe(TelemetryObserver::new(path, env!("CARGO_PKG_VERSION"))?);
    }
    Ok(host)
}

#[cfg(test)]
fn configured_extensions(
    config: &Config,
    session: &Session,
    model: &Model,
    reasoning: &ReasoningConfig,
    sessions: &SessionStore,
) -> anyhow::Result<(ExtensionHost, ExecutableExtensions)> {
    configured_extensions_with_runtime_manager(
        config,
        session,
        model,
        reasoning,
        sessions,
        None,
        ExtensionProviderRuntime::default(),
    )
}

fn configured_extensions_with_runtime_manager(
    config: &Config,
    session: &Session,
    model: &Model,
    reasoning: &ReasoningConfig,
    sessions: &SessionStore,
    runtime_manager: Option<ExtensionRuntimeManager>,
    provider_runtime: ExtensionProviderRuntime,
) -> anyhow::Result<(ExtensionHost, ExecutableExtensions)> {
    let mut extensions = configured_extension_host(config, model)?;
    let executable_extensions = ExecutableExtensions::discover_and_start_with_provider_runtime(
        config,
        session,
        model,
        reasoning,
        sessions,
        &mut extensions,
        runtime_manager,
        provider_runtime,
    );
    Ok((extensions, executable_extensions))
}

fn terminal_goal_store(config: &Config) -> anyhow::Result<Arc<DurableGoalStore>> {
    // Serve and the terminal intentionally use the same private directory and
    // file schema. The terminal does not depend on Serve being enabled; this
    // is only a shared on-disk location for first-party frontends.
    let session_dir = if config.session_dir.is_absolute() {
        config.session_dir.clone()
    } else {
        std::env::current_dir()?.join(&config.session_dir)
    };
    let root = session_dir.join(".serve").join("goals");
    DurableGoalStore::open(&root)
        .map(Arc::new)
        .map_err(|error| anyhow::anyhow!("unable to open durable goal store: {error}"))
}

pub(crate) fn terminal_goal_session_id(session: &Session) -> anyhow::Result<String> {
    session
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("current session has no valid goal identity"))
}

fn subagents_surface_available(
    executable_extensions: &ExecutableExtensions,
    extensions: &ExtensionHost,
    model: &Model,
) -> bool {
    executable_extensions.has_agent_session_service()
        && model.spec.capabilities.tools
        && extensions
            .tool_definitions()
            .iter()
            .any(|definition| definition.name == "subagent_spawn")
}

fn configure_v2_delegation(
    agent: &mut Agent,
    model: &Model,
    reasoning: &ReasoningConfig,
    service_available: bool,
) -> anyhow::Result<()> {
    if !service_available {
        if matches!(
            reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Ultra)
        ) {
            anyhow::bail!(
                "Ultra requires the trusted {SUBAGENTS_EXTENSION_NAME} extension to observe delegated work"
            );
        }
        return Ok(());
    }
    if matches!(
        reasoning,
        ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Ultra)
    ) && !model_supports_ultra(model)
    {
        anyhow::bail!(
            "Ultra requires an available V2 delegation runtime for model {}",
            model.spec.id.0
        );
    }
    let session_parent = agent
        .session()
        .path()
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session path has no parent directory"))?;
    agent
        .enable_v2_delegation_extension_only(DelegationConfig::new(
            session_parent.join(".delegation"),
        ))
        .with_context(|| "could not initialize the extension-owned delegation runtime")?;
    Ok(())
}

pub(crate) fn open_launch_session(
    prepared_session: &mut Option<Session>,
    selection: SessionSelection,
) -> anyhow::Result<Session> {
    match selection {
        SessionSelection::CreateNew(path) => {
            if let Some(parent) = path.parent() {
                create_private_session_dir(parent)?;
            }
            let descriptor_path = descriptor_session_path(&path)?;
            let file = create_regular_file_for_append(&descriptor_path)?;
            Ok(Session::create_with_file(path, file)?)
        }
        SessionSelection::OpenExisting(path) => match prepared_session.take() {
            Some(session) if session.path() == path => Ok(session),
            _ => {
                let descriptor_path = descriptor_session_path(&path)?;
                let file = open_regular_file_for_append(&descriptor_path)?;
                Ok(Session::open_with_file(path, file)?)
            }
        },
    }
}

/// Builds an App with a fresh ordinary-host extension runtime manager.
pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App> {
    build_app_with_runtime_manager(boot, launch, system, None)
}

/// Builds an App using an explicit host-owned extension runtime manager.
///
/// Serve supplies a separately trust-partitioned manager through this seam;
/// ordinary callers retain the historical local-host domain via [`build_app`].
pub(crate) fn build_app_with_runtime_manager(
    boot: Bootstrap,
    launch: LaunchSelection,
    system: String,
    runtime_manager: Option<ExtensionRuntimeManager>,
) -> anyhow::Result<App> {
    let Bootstrap {
        mut config,
        mut catalog,
        sessions,
        client,
        provider_runtime,
        prestarted_extensions,
        prepared_session,
        modeless: _,
    } = boot;
    let mut system = system;
    let mut prestarted_extensions = prestarted_extensions.into_inner();
    if let Some((_, extensions)) = prestarted_extensions.as_mut() {
        extensions.synchronize_provider_catalog(&mut catalog, &client);
    }
    let model = catalog.resolve(&launch.model)?;
    let compact_model = config
        .compaction
        .compact_model
        .as_ref()
        .map(|id| catalog.resolve(id))
        .transpose()
        .with_context(|| "configured compaction model could not be resolved")?;
    validate_compaction_route(config.compaction.mode, &model, compact_model.as_ref())?;
    let mut prepared_session = prepared_session.into_inner();
    let mut session = open_launch_session(&mut prepared_session, launch.session)?;

    let requested_reasoning = normalize_reasoning_for_model(&launch.reasoning, &model)?;
    let requested_reasoning_mode = launch.reasoning_mode;
    validate_native_compaction_replay(config.compaction.mode, &session, &model)?;

    let skills: Arc<dyn SkillRegistry> = Arc::new(FileSystemSkillRegistry::new_with_invocation(
        config.workspace.clone(),
        config.invocation_cwd.clone(),
        config.skill_paths.clone(),
        config.workspace_trusted,
    )?);
    system.push_str(&format_skills_for_prompt(&skills.descriptors()));
    let prompts = Arc::new(PromptRegistry::discover(
        &config.workspace,
        &config.prompt_paths,
        config.workspace_trusted,
    ));
    let (extensions, mut executable_extensions) =
        if let Some((mut extensions, mut executable_extensions)) = prestarted_extensions.take() {
            apply_extension_tool_policy(&mut extensions, &config, &model);
            executable_extensions.transition_active_session(
                &session,
                &model,
                &requested_reasoning,
                &sessions,
            );
            // Preflight must not activate interactive lifecycle work before the
            // final App owns the session driver.
            executable_extensions.deactivate_session_lifecycle_driver();
            executable_extensions.synchronize_provider_catalog(&mut catalog, &client);
            (extensions, executable_extensions)
        } else {
            configured_extensions_with_runtime_manager(
                &config,
                &session,
                &model,
                &requested_reasoning,
                &sessions,
                runtime_manager,
                provider_runtime,
            )?
        };
    executable_extensions.synchronize_provider_catalog(&mut catalog, &client);
    let service_available = executable_extensions.has_agent_session_service();
    let subagents_available = service_available
        && subagents_surface_available(&executable_extensions, &extensions, &model);
    let (reasoning, reasoning_mode, migration_diagnostic) =
        normalize_reasoning_selection_for_model_with_subagents(
            &requested_reasoning,
            requested_reasoning_mode,
            &model,
            subagents_available,
        )?;
    if let Some(diagnostic) = migration_diagnostic {
        crate::output::stderr!("warning: {diagnostic}");
    }
    config.model = Some(model.spec.id.clone());
    config.reasoning = reasoning.clone();
    config.reasoning_mode = reasoning_mode;
    append_config_if_changed(&mut session, &model.spec.id, &reasoning, reasoning_mode)?;
    validate_explicit_tool_policy(
        &config,
        &extensions,
        &model,
        executable_extensions.has_dynamic_tool_provider(),
    )?;
    let goal_store = terminal_goal_store(&config)?;
    let goal_session_id = terminal_goal_session_id(&session)?;
    let goal_driver = GoalDriver::new(goal_store.clone(), goal_session_id.clone());
    let mut agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        effect_broker: EffectBroker::new(config.effect_policy),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
        reasoning_mode,
        cache_retention: config.cache_retention,
        session_id: None,
    })?;
    agent.set_prompt_model_source(Some(crate::tui::theme::model_lab(&model).key().to_owned()));
    agent.set_prompt_color(Some(crate::tui::theme::prompt_color_for_model(&model)));
    agent.set_compaction_model(compact_model);
    agent.set_compaction_token_mode(
        agent_compaction_mode(config.compaction.mode),
        effective_compaction_threshold_fraction(&config, &model),
        config.compaction.keep_recent_tokens,
    )?;
    agent.set_max_session_cost_microdollars(config.max_cost_microdollars);
    configure_v2_delegation(&mut agent, &model, &reasoning, service_available)?;
    executable_extensions.bind_agent_sessions(&agent)?;
    agent.finalize_tool_surface();
    let system_tokens = estimate_text_tokens(agent.system_prompt());

    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        reasoning_mode,
        system,
        system_tokens,
        skills,
        prompts,
        executable_extensions,
        goal_store,
        goal_driver,
        goal_session_id,
    })
}

struct ReleasedExtensionBindingCleanup {
    extensions: Option<ExecutableExtensions>,
}

impl ReleasedExtensionBindingCleanup {
    fn new(extensions: ExecutableExtensions) -> Self {
        Self {
            extensions: Some(extensions),
        }
    }

    fn disarm(mut self) {
        // The replacement App now owns a clone of the same manager. Dropping
        // the released wrapper is intentional: its process list is empty, so
        // its normal Drop path cannot terminate the shared fleet.
        self.extensions.take();
    }
}

impl Drop for ReleasedExtensionBindingCleanup {
    fn drop(&mut self) {
        if let Some(mut extensions) = self.extensions.take() {
            // Once the original binding has been released, any later rebuild
            // error has no App left to own terminal fleet shutdown. Do not
            // leave a workspace service behind on this failure path.
            extensions.shutdown_blocking();
        }
    }
}

/// Recreate the Agent at an idle boundary. Taking `App` by value guarantees the
/// old Agent and its session file are dropped before a session is reopened.
pub fn rebuild_app(
    mut app: App,
    new_model: Option<Model>,
    new_reasoning: Option<ReasoningConfig>,
    new_reasoning_mode: Option<ReasoningMode>,
    selection: Option<SessionSelection>,
) -> anyhow::Result<App> {
    app.synchronize_extension_provider_catalog();
    let mut config = app.config.clone();
    let mut catalog = app.catalog.clone();
    let sessions = app.sessions.clone();
    let client = app.client.clone();
    let model = app.model.clone();
    let reasoning = app.reasoning.clone();
    let reasoning_mode = app.reasoning_mode;
    let system = app.system.clone();
    let old_skills = Arc::clone(&app.skills);
    let goal_store = Arc::clone(&app.goal_store);
    let compact_model = config
        .compaction
        .compact_model
        .as_ref()
        .map(|id| catalog.resolve(id))
        .transpose()
        .with_context(|| "configured compaction model could not be resolved")?;
    let current_path = app.agent.session().path().to_owned();
    let old_skill_metadata = format_skills_for_prompt(&old_skills.descriptors());
    let mut system = system;
    if !old_skill_metadata.is_empty() && system.ends_with(&old_skill_metadata) {
        system.truncate(system.len() - old_skill_metadata.len());
    }

    let (persisted, mut prepared_session) = match selection.as_ref() {
        Some(SessionSelection::OpenExisting(path)) => {
            let descriptor_path = descriptor_session_path(path)?;
            let file = open_regular_file_for_append(&descriptor_path)?;
            let session = Session::open_with_file(path, file)?;
            let persisted = persisted_session_config(&session)?;
            (persisted, Some(session))
        }
        Some(SessionSelection::CreateNew(_)) | None => (PersistedSessionConfig::default(), None),
    };
    let restored_model = persisted
        .model
        .as_ref()
        .map(|id| catalog.resolve(id))
        .transpose()?;
    let changing_model = new_model.is_some() || restored_model.is_some();
    let explicit_reasoning = new_reasoning.is_some();
    let old_model = model;
    let model = new_model
        .or(restored_model)
        .unwrap_or_else(|| old_model.clone());
    validate_compaction_route(config.compaction.mode, &model, compact_model.as_ref())?;
    let requested_reasoning = match (new_reasoning, persisted.reasoning) {
        (Some(reasoning), _) => normalize_reasoning_for_model(&reasoning, &model)?,
        (None, Some(reasoning)) => normalize_reasoning_for_model(&reasoning, &model)?,
        (None, None) if changing_model => {
            let level = level_from_reasoning(&reasoning, &old_model)?;
            thinking_to_reasoning(level, &model)?
        }
        (None, None) => normalize_reasoning_for_model(&reasoning, &model)?,
    };
    let requested_reasoning_mode = if let Some(mode) = new_reasoning_mode {
        mode
    } else if explicit_reasoning {
        // Rebuilds use the same precedence as startup: an explicit current
        // effort supersedes the obsolete Pro bit persisted in a session.
        ReasoningMode::Standard
    } else {
        persisted.reasoning_mode.unwrap_or(reasoning_mode)
    };
    let candidate_session = match selection.as_ref() {
        Some(SessionSelection::OpenExisting(_)) => prepared_session.as_ref(),
        Some(SessionSelection::CreateNew(_)) => None,
        None => Some(app.agent.session()),
    };
    if let Some(candidate_session) = candidate_session {
        validate_native_compaction_replay(config.compaction.mode, candidate_session, &model)?;
    }
    // Do not tear down the working agent or its executable extensions until
    // the complete candidate route and reasoning configuration is known valid.
    // Keep the host-level runtime manager, but release the old App binding so
    // only an explicitly workspace-shared, content-identical process can
    // survive the compatible rebuild.
    let mut released_extensions = std::mem::take(&mut app.executable_extensions);
    let runtime_manager = released_extensions.runtime_manager();
    let provider_runtime = released_extensions.provider_runtime();
    released_extensions.clear_provider_catalog(&mut catalog, &client);
    released_extensions.release_binding_blocking();
    let released_extensions = ReleasedExtensionBindingCleanup::new(released_extensions);
    drop(app);
    let mut session = match selection {
        Some(SessionSelection::CreateNew(path)) => {
            if let Some(parent) = path.parent() {
                create_private_session_dir(parent)?;
            }
            let descriptor_path = descriptor_session_path(&path)?;
            let file = create_regular_file_for_append(&descriptor_path)?;
            Session::create_with_file(path, file)?
        }
        Some(SessionSelection::OpenExisting(path)) => match prepared_session.take() {
            Some(session) if session.path() == path => session,
            _ => {
                let descriptor_path = descriptor_session_path(&path)?;
                let file = open_regular_file_for_append(&descriptor_path)?;
                Session::open_with_file(path, file)?
            }
        },
        None => {
            let descriptor_path = descriptor_session_path(&current_path)?;
            let file = open_regular_file_for_append(&descriptor_path)?;
            Session::open_with_file(current_path, file)?
        }
    };
    let goal_session_id = terminal_goal_session_id(&session)?;
    let goal_driver = GoalDriver::new(goal_store.clone(), goal_session_id.clone());

    let skills: Arc<dyn SkillRegistry> = Arc::new(FileSystemSkillRegistry::new_with_invocation(
        config.workspace.clone(),
        config.invocation_cwd.clone(),
        config.skill_paths.clone(),
        config.workspace_trusted,
    )?);
    system.push_str(&format_skills_for_prompt(&skills.descriptors()));
    let prompts = Arc::new(PromptRegistry::discover(
        &config.workspace,
        &config.prompt_paths,
        config.workspace_trusted,
    ));
    let (extensions, mut executable_extensions) = configured_extensions_with_runtime_manager(
        &config,
        &session,
        &model,
        &requested_reasoning,
        &sessions,
        runtime_manager,
        provider_runtime,
    )?;
    executable_extensions.synchronize_provider_catalog(&mut catalog, &client);
    let service_available = executable_extensions.has_agent_session_service();
    let subagents_available = service_available
        && subagents_surface_available(&executable_extensions, &extensions, &model);
    let (reasoning, reasoning_mode, migration_diagnostic) =
        normalize_reasoning_selection_for_model_with_subagents(
            &requested_reasoning,
            requested_reasoning_mode,
            &model,
            subagents_available,
        )?;
    if let Some(diagnostic) = migration_diagnostic {
        crate::output::stderr!("warning: {diagnostic}");
    }
    config.model = Some(model.spec.id.clone());
    config.reasoning = reasoning.clone();
    config.reasoning_mode = reasoning_mode;
    append_config_if_changed(&mut session, &model.spec.id, &reasoning, reasoning_mode)?;
    validate_explicit_tool_policy(
        &config,
        &extensions,
        &model,
        executable_extensions.has_dynamic_tool_provider(),
    )?;
    let mut agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        effect_broker: EffectBroker::new(config.effect_policy),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
        reasoning_mode,
        cache_retention: config.cache_retention,
        session_id: None,
    })?;
    agent.set_prompt_model_source(Some(crate::tui::theme::model_lab(&model).key().to_owned()));
    agent.set_prompt_color(Some(crate::tui::theme::prompt_color_for_model(&model)));
    agent.set_compaction_model(compact_model);
    agent.set_compaction_token_mode(
        agent_compaction_mode(config.compaction.mode),
        effective_compaction_threshold_fraction(&config, &model),
        config.compaction.keep_recent_tokens,
    )?;
    agent.set_max_session_cost_microdollars(config.max_cost_microdollars);
    configure_v2_delegation(&mut agent, &model, &reasoning, service_available)?;
    executable_extensions.bind_agent_sessions(&agent)?;
    agent.finalize_tool_surface();
    let system_tokens = estimate_text_tokens(agent.system_prompt());

    released_extensions.disarm();
    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        reasoning_mode,
        system,
        system_tokens,
        skills,
        prompts,
        executable_extensions,
        goal_store,
        goal_driver,
        goal_session_id,
    })
}

#[cfg(test)]
mod bounded_env_tests {
    use super::*;

    fn oversized_result(name: &str) -> Result<Option<String>, ygg_ai::ConfigError> {
        Err(ygg_ai::ConfigError::EnvironmentValueTooLarge {
            var: name.to_owned(),
            max_bytes: ygg_ai::auth::MAX_ENV_VALUE_BYTES,
        })
    }

    #[test]
    fn bootstrap_env_adapters_accept_the_limit_and_propagate_oversized_values() {
        let at_limit = "x".repeat(ygg_ai::auth::MAX_ENV_VALUE_BYTES);
        assert_eq!(
            optional_env_value("OPTIONAL", Ok(Some(at_limit.clone()))).unwrap(),
            Some(at_limit.clone())
        );
        assert_eq!(
            required_env_value("REQUIRED", Ok(Some(at_limit.clone()))).unwrap(),
            at_limit.clone()
        );
        assert_eq!(
            strict_env_value("STRICT", Ok(Some(at_limit))).unwrap(),
            Some("x".repeat(ygg_ai::auth::MAX_ENV_VALUE_BYTES))
        );

        let errors = [
            optional_env_value("OPTIONAL", oversized_result("OPTIONAL")),
            required_env_value("REQUIRED", oversized_result("REQUIRED")).map(|_| None),
            strict_env_value("STRICT", oversized_result("STRICT")),
        ];
        for result in errors {
            let error = result.expect_err("oversized environment values must fail");
            assert!(error.to_string().contains("4096-byte limit"));
        }
    }

    #[test]
    fn bootstrap_env_adapters_preserve_unset_and_invalid_unicode_policies() {
        assert_eq!(optional_env_value("OPTIONAL", Ok(None)).unwrap(), None);
        assert_eq!(
            optional_env_value(
                "OPTIONAL",
                Err(ygg_ai::ConfigError::InvalidEnv("OPTIONAL".to_owned()))
            )
            .unwrap(),
            None
        );
        assert_eq!(strict_env_value("STRICT", Ok(None)).unwrap(), None);
        assert!(strict_env_value(
            "STRICT",
            Err(ygg_ai::ConfigError::InvalidEnv("STRICT".to_owned()))
        )
        .is_err());
        assert!(required_env_value("REQUIRED", Ok(None)).is_err());
        assert!(required_env_value(
            "REQUIRED",
            Err(ygg_ai::ConfigError::InvalidEnv("REQUIRED".to_owned()))
        )
        .is_err());
    }
}

#[cfg(test)]
mod tests;
