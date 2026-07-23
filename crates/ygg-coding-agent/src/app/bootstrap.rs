#![allow(missing_docs)]

use std::cell::RefCell;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::EventStream;
use futures_util::StreamExt;
use sha2::{Digest as _, Sha256};
use ygg_agent::{Agent, AgentConfig, CoreTools, EntryValue, ExtensionHost, Session, SkillRegistry};
use ygg_ai::{
    AiClient, Auth, Capabilities, Endpoint, EndpointId, ModalitySet, Model, ModelCatalog, ModelId,
    ModelLimits, ModelSpec, OpenAiChatReasoningMode, Pricing, PricingTier, Protocol,
    ReasoningCapability, ReasoningConfig, ReasoningControl, TokenRate, ToolDef,
};

use crate::app::{level_from_reasoning, normalize_reasoning_for_model, thinking_to_reasoning, App};
use crate::config::{Config, ResumeSelector};
use crate::extensions::ExecutableExtensions;
use crate::modes::interactive::run_blocking_lifecycle;
use crate::prompts::PromptRegistry;
use crate::providers::{
    ModelDiscovery, ModelFilter, ProviderPreset, StaticModelPreset, BUILTIN_PROVIDERS,
    MINIMAX_MODELS, OPENCODE_MODELS,
};
use crate::resources::{validate_skill_requirements, FileSystemSkillRegistry, SkillToolsExtension};
use crate::session_store::SessionStore;
use crate::tui::pickers::{model_picker, session_picker};
use crate::tui::view::InteractiveShell;

/// Inputs needed to resolve a launch without constructing an Agent or a TUI.
pub struct Bootstrap {
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub client: AiClient,
    /// Session opened while resolving resume provenance. Keeping it here
    /// avoids replaying the same JSONL file a second time in `build_app`.
    prepared_session: RefCell<Option<Session>>,
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
}

const DEEPSEEK_ENDPOINT_ID: &str = "deepseek";
const DEEPSEEK_MODEL_ID: &str = "deepseek-v4-pro";
const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1/";
const DEEPSEEK_DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
// Only a local capacity reserve; it never becomes an implicit request cap.
const DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 384_000;

const OPENCODE_ANTHROPIC_ENDPOINT_ID: &str = "opencode-anthropic";
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
// Streaming providers should return response headers before generation. Keep
// this phase short and separately bounded; the response body has its own idle
// and absolute deadlines in ygg-ai.
const PROVIDER_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(30);
// Local servers may need to load a model before they can return response
// headers. Match Pi's cold-start-safe five-minute default for custom endpoints
// without weakening the fail-fast behavior of hosted providers.
const CUSTOM_ENDPOINT_STARTUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_DISCOVERY_BODY_BYTES: usize = 8 * 1024 * 1024;
// Version 2 invalidated inventories whose llama.cpp context length was guessed
// because older discovery ignored hlid's nested `meta.n_ctx` field. Version 4
// invalidated sparse local inventories that were incorrectly cached as
// tool-incompatible by version 3. Version 5 invalidates v4 entries produced by
// the secondary hlid discovery path before it adopted the same tri-state
// local-tool fallback.
const CUSTOM_MODEL_CACHE_VERSION: u8 = 5;
const PROVIDER_INVENTORY_CACHE_VERSION: u8 = 1;
const MAX_PROVIDER_INVENTORY_CACHE_BYTES: usize = MAX_DISCOVERY_BODY_BYTES + 1024 * 1024;
const PROVIDER_INVENTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
const NEGATIVE_INVENTORY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);

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

fn credential_fingerprint(credential: &str) -> String {
    let digest = Sha256::digest(credential.as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
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
    let safe_id = provider_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ygg")
        .join("cache")
        .join("model-inventories")
        .join(format!("{safe_id}.json"))
}

fn load_provider_inventory_cache(
    path: &std::path::Path,
    provider_id: &str,
    inventory_url: &str,
    credential_fingerprint: &str,
) -> anyhow::Result<Option<CachedProviderInventory>> {
    let Some(bytes) = crate::auth::read_bounded_regular(path, MAX_PROVIDER_INVENTORY_CACHE_BYTES)?
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

fn provider_inventory_cache_is_stale(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_none_or(|age| age >= PROVIDER_INVENTORY_REFRESH_INTERVAL)
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
                eprintln!("warning: could not persist {provider_id} model metadata: {error}");
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
            eprintln!("warning: {provider_id} model cache unavailable: {cache_error}");
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
            eprintln!("warning: {provider_id} model cache unavailable: {error}");
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
    let response = reqwest::blocking::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()?
        .get(url)
        .headers(headers)
        .send()
        .map_err(|error| anyhow::anyhow!("GET {url} failed: {error}"))?
        .error_for_status()
        .map_err(|error| anyhow::anyhow!("GET {url} failed: {error}"))?;
    bounded_discovery_json(response, &format!("models response from {url}"))
}

fn has_api_model(catalog: &ModelCatalog, endpoint: &str, api_name: &str) -> bool {
    catalog
        .models()
        .any(|model| model.endpoint.0 == endpoint && model.api_name == api_name)
}

fn bearer_headers(token: &str) -> anyhow::Result<http::HeaderMap> {
    let mut headers = http::HeaderMap::new();
    let mut value = http::HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    headers.insert(http::header::AUTHORIZATION, value);
    Ok(headers)
}

fn build_headers(entries: &[(&'static str, &'static str)]) -> anyhow::Result<http::HeaderMap> {
    let mut headers = http::HeaderMap::new();
    for (name, value) in entries {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes())?,
            http::HeaderValue::from_str(value)?,
        );
    }
    Ok(headers)
}

fn add_headers(
    target: &mut http::HeaderMap,
    entries: &[(&'static str, &'static str)],
) -> anyhow::Result<()> {
    for (name, value) in entries {
        target.insert(
            http::HeaderName::from_bytes(name.as_bytes())?,
            http::HeaderValue::from_str(value)?,
        );
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

fn openai_responses_model_supports_reasoning(id: &str) -> bool {
    id.starts_with("gpt-5")
        || id.starts_with("codex-")
        || id
            .strip_prefix('o')
            .and_then(|rest| rest.as_bytes().first())
            .is_some_and(u8::is_ascii_digit)
}

fn discovered_preset_binding(
    preset: &ProviderPreset,
    model_id: &str,
) -> Option<(&'static str, Protocol)> {
    if preset.id != crate::providers::OPENCODE.id {
        return Some((
            preset.id,
            crate::providers::discovered_protocol(preset.id, model_id, preset.protocol),
        ));
    }
    if model_id.starts_with("gemini-") {
        return None;
    }
    if model_id.starts_with("claude-")
        || (model_id.starts_with("qwen3.") && model_id.ends_with("-plus"))
    {
        return Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages));
    }
    if model_id.starts_with("gpt-") || model_id.starts_with("codex-") {
        return Some((preset.id, Protocol::OpenAiResponses));
    }
    Some((preset.id, Protocol::OpenAiChat))
}

fn register_openai_compatible_models(
    catalog: &mut ModelCatalog,
    preset: &ProviderPreset,
    filter: ModelFilter,
    api_key: &str,
) -> anyhow::Result<()> {
    let models_url = url::Url::parse(preset.base_url)?.join("models")?;
    let mut headers = bearer_headers(api_key)?;
    add_headers(&mut headers, preset.extra_headers)?;
    let body = if preset.id == crate::providers::OPENCODE.id {
        cached_provider_inventory_or_schedule(preset.id, models_url.to_string(), headers, api_key)
    } else {
        cached_provider_inventory(preset.id, models_url.to_string(), headers, api_key)?
    };
    let Some(body) = body else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        let catalog_id = format!("{}/{}", preset.id, model.id);
        let Some((endpoint_id, protocol)) = discovered_preset_binding(preset, &model.id) else {
            continue;
        };
        if !model_filter_matches(filter, &model.id)
            || has_api_model(catalog, endpoint_id, &model.id)
            || has_model_id(catalog, &catalog_id)
        {
            continue;
        }
        let reasoning = protocol == Protocol::OpenAiResponses
            && openai_responses_model_supports_reasoning(&model.id);
        let context_window = model.context_window.unwrap_or(128_000);
        let max_output_tokens = model
            .max_output_tokens
            .unwrap_or(32_768)
            .min(context_window);
        let mut input_modalities = if model.vision
            || model_id_implies_vision(&model.id)
            || ((preset.id == "openai" || preset.id == crate::providers::OPENCODE.id)
                && (model.id.starts_with("gpt-4o")
                    || model.id.starts_with("gpt-4.1")
                    || model.id.starts_with("gpt-5")))
        {
            ModalitySet::none().with(ygg_ai::Modality::Image)
        } else {
            ModalitySet::none()
        };
        if model.audio {
            input_modalities = input_modalities.with(ygg_ai::Modality::Audio);
        }
        let cache = crate::providers::cache_compatibility(preset.id, &model.id, protocol);
        let pricing = crate::providers::model_pricing(preset.id, &model.id);
        catalog.register_model(ModelSpec {
            id: ModelId(catalog_id),
            endpoint: EndpointId(endpoint_id.into()),
            api_name: model.id,
            display_name: None,
            protocol,
            capabilities: Capabilities {
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
                structured_output: protocol != Protocol::OpenAiChat,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens,
            },
            pricing,
            cache,
        })?;
    }
    Ok(())
}

fn register_anthropic_compatible_models(
    catalog: &mut ModelCatalog,
    preset: &ProviderPreset,
    api_key: &str,
) -> anyhow::Result<()> {
    let mut headers = build_headers(&[("anthropic-version", "2023-06-01")])?;
    let mut key_value = http::HeaderValue::from_str(api_key)?;
    key_value.set_sensitive(true);
    headers.insert(http::HeaderName::from_static("x-api-key"), key_value);
    add_headers(&mut headers, preset.extra_headers)?;
    let models_url = url::Url::parse(preset.base_url)?.join("models?limit=1000")?;
    let Some(body) =
        cached_provider_inventory(preset.id, models_url.to_string(), headers, api_key)?
    else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        let catalog_id = format!("{}/{}", preset.id, model.id);
        if (preset.id == "anthropic" && !model.id.starts_with("claude-"))
            || has_api_model(catalog, preset.id, &model.id)
            || has_model_id(catalog, &catalog_id)
        {
            continue;
        }
        let context_window = model.context_window.unwrap_or(200_000);
        let max_output_tokens = model
            .max_output_tokens
            .unwrap_or(64_000)
            .min(context_window);
        let cache = crate::providers::cache_compatibility(
            preset.id,
            &model.id,
            Protocol::AnthropicMessages,
        );
        let pricing = crate::providers::model_pricing(preset.id, &model.id);
        catalog.register_model(ModelSpec {
            id: ModelId(catalog_id),
            endpoint: EndpointId(preset.id.into()),
            api_name: model.id,
            display_name: None,
            protocol: Protocol::AnthropicMessages,
            capabilities: Capabilities {
                input_modalities: if model.vision || preset.id == "anthropic" {
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
                structured_output: true,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens,
            },
            pricing,
            cache,
        })?;
    }
    Ok(())
}

fn deepseek_base_url() -> anyhow::Result<url::Url> {
    let configured = std::env::var("YGG_DEEPSEEK_BASE_URL")
        .unwrap_or_else(|_| DEEPSEEK_DEFAULT_BASE_URL.to_owned());
    let normalized = if configured.ends_with('/') {
        configured
    } else {
        format!("{configured}/")
    };
    url::Url::parse(&normalized)
        .map_err(|error| anyhow::anyhow!("invalid YGG_DEEPSEEK_BASE_URL: {error}"))
}

fn deepseek_limit(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid {name}={value:?}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("could not read {name}: {error}")),
    }
}

fn register_deepseek_v4_pro(catalog: &mut ModelCatalog) -> anyhow::Result<()> {
    // Unit tests retain this deterministic fallback without ambient credentials;
    // runtime callers reach it only after the preset resolves DEEPSEEK_API_KEY.
    let endpoint_id = EndpointId(DEEPSEEK_ENDPOINT_ID.into());
    if !catalog.has_endpoint(&endpoint_id) {
        catalog.register_endpoint(Endpoint {
            id: endpoint_id.clone(),
            base_url: deepseek_base_url()?,
            auth: Auth::bearer_env("DEEPSEEK_API_KEY"),
            default_headers: http::HeaderMap::new(),
            transport: ygg_ai::EndpointTransport::Http,
            timeout: PROVIDER_RESPONSE_HEADER_TIMEOUT,
        })?;
    }
    if has_model_id(catalog, DEEPSEEK_MODEL_ID) {
        return Ok(());
    }
    let api_name =
        std::env::var("YGG_DEEPSEEK_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL_ID.to_owned());
    let cache = crate::providers::cache_compatibility(
        crate::providers::DEEPSEEK.id,
        &api_name,
        Protocol::OpenAiChat,
    );
    let pricing = crate::providers::model_pricing(crate::providers::DEEPSEEK.id, &api_name);
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
        endpoint: EndpointId(DEEPSEEK_ENDPOINT_ID.into()),
        api_name,
        display_name: None,
        protocol: Protocol::OpenAiChat,
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
            structured_output: false,
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

fn register_discovered_deepseek_models(catalog: &mut ModelCatalog) -> anyhow::Result<()> {
    let key = std::env::var("DEEPSEEK_API_KEY")?;
    let url = deepseek_base_url()?.join("models")?.to_string();
    let Some(body) = cached_provider_inventory(
        crate::providers::DEEPSEEK.id,
        url,
        bearer_headers(&key)?,
        &key,
    )?
    else {
        return Ok(());
    };
    for model in api_models_from_response(&body)? {
        if has_api_model(catalog, DEEPSEEK_ENDPOINT_ID, &model.id) {
            continue;
        }
        let supports_reasoning =
            model.id.contains("reason") || model.id.contains("r1") || model.id.contains("v4");
        let cache = crate::providers::cache_compatibility(
            crate::providers::DEEPSEEK.id,
            &model.id,
            Protocol::OpenAiChat,
        );
        let pricing = crate::providers::model_pricing(crate::providers::DEEPSEEK.id, &model.id);
        let context_window = model.context_window.unwrap_or(128_000);
        let max_output_tokens = model
            .max_output_tokens
            .unwrap_or(64_000)
            .min(context_window);
        catalog.register_model(ModelSpec {
            id: ModelId(format!("deepseek/{}", model.id)),
            endpoint: EndpointId(DEEPSEEK_ENDPOINT_ID.into()),
            api_name: model.id,
            display_name: None,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
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
                structured_output: false,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens,
            },
            pricing,
            cache,
        })?;
    }
    Ok(())
}

/// Populate OpenRouter from its live inventory while retaining provider-specific
/// capability and pricing metadata.
fn register_openrouter_models_for_preset(
    catalog: &mut ModelCatalog,
    preset: &ProviderPreset,
    api_key: &str,
) -> anyhow::Result<()> {
    let models_url = url::Url::parse(preset.base_url)?.join("models")?;
    let Some(body) = cached_provider_inventory(
        preset.id,
        models_url.to_string(),
        bearer_headers(api_key)?,
        api_key,
    )?
    else {
        return Ok(());
    };
    for model in openrouter_models_from_response(&body)? {
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

fn openrouter_models_from_response(body: &serde_json::Value) -> anyhow::Result<Vec<ModelSpec>> {
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
        let max_output_tokens = entry
            .get("top_provider")
            .and_then(|provider| provider.get("max_completion_tokens"))
            .and_then(serde_json::Value::as_u64)
            .filter(|value| *value > 0)
            .unwrap_or(16_384)
            .min(context_window);
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

        models.push(ModelSpec {
            id: ModelId(format!("{}/{api_name}", crate::providers::OPENROUTER.id)),
            endpoint: EndpointId(crate::providers::OPENROUTER.id.into()),
            api_name: api_name.into(),
            display_name: None,
            protocol: Protocol::OpenAiChat,
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
                structured_output: false,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens,
            },
            pricing: openrouter_pricing(entry),
            cache: crate::providers::cache_compatibility(
                crate::providers::OPENROUTER.id,
                api_name,
                Protocol::OpenAiChat,
            ),
        });
    }
    models.sort_by(|left, right| left.api_name.cmp(&right.api_name));
    Ok(models)
}

fn resolve_first_env(names: &'static [&'static str]) -> Option<(&'static str, String)> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| (*name, value))
    })
}

fn preset_auth(protocol: Protocol, api_key_env: &'static str) -> Auth {
    if protocol == Protocol::AnthropicMessages {
        Auth::header_env(http::HeaderName::from_static("x-api-key"), api_key_env)
    } else {
        Auth::bearer_env(api_key_env)
    }
}

fn register_preset_endpoint(
    catalog: &mut ModelCatalog,
    preset: &ProviderPreset,
    api_key_env: &'static str,
) -> anyhow::Result<()> {
    let endpoint_id = EndpointId(preset.id.into());
    if catalog.has_endpoint(&endpoint_id) {
        return Ok(());
    }
    catalog.register_endpoint(Endpoint {
        id: endpoint_id,
        base_url: url::Url::parse(preset.base_url)?,
        auth: preset_auth(preset.protocol, api_key_env),
        default_headers: build_headers(preset.extra_headers)?,
        transport: ygg_ai::EndpointTransport::Http,
        timeout: PROVIDER_RESPONSE_HEADER_TIMEOUT,
    })?;
    Ok(())
}

fn static_model_reasoning(model: &StaticModelPreset) -> Option<ReasoningCapability> {
    model.reasoning.then_some(ReasoningCapability {
        control: ReasoningControl::Effort,
        exposes_text: true,
        preserves_state: model.protocol != Protocol::OpenAiChat,
        effort_budgets: None,
        openai_chat_mode: if model.protocol == Protocol::OpenAiChat
            && model.id.starts_with("deepseek-")
        {
            OpenAiChatReasoningMode::DeepSeekThinking
        } else {
            OpenAiChatReasoningMode::Standard
        },
        min_effort: ygg_ai::ReasoningEffort::Minimal,
        max_effort: model.max_reasoning_effort,
    })
}

fn register_static_models(
    catalog: &mut ModelCatalog,
    provider_id: &str,
    models: &[StaticModelPreset],
) -> anyhow::Result<()> {
    for model in models {
        let catalog_id = format!("{provider_id}/{}", model.id);
        if has_model_id(catalog, &catalog_id) {
            continue;
        }
        let endpoint = if provider_id == crate::providers::OPENCODE.id
            && model.protocol == Protocol::AnthropicMessages
        {
            OPENCODE_ANTHROPIC_ENDPOINT_ID
        } else {
            provider_id
        };
        catalog.register_model(ModelSpec {
            id: ModelId(catalog_id),
            endpoint: EndpointId(endpoint.into()),
            api_name: model.id.into(),
            display_name: Some(model.name.into()),
            protocol: model.protocol,
            capabilities: Capabilities {
                input_modalities: if model.vision {
                    ModalitySet::none().with(ygg_ai::Modality::Image)
                } else {
                    ModalitySet::none()
                },
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: model.protocol != Protocol::OpenAiChat,
                reasoning: static_model_reasoning(model),
                structured_output: model.protocol != Protocol::OpenAiChat,
            },
            limits: ModelLimits {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
            },
            pricing: crate::providers::model_pricing(provider_id, model.id),
            cache: crate::providers::cache_compatibility(provider_id, model.id, model.protocol),
        })?;
    }
    Ok(())
}

fn register_opencode(
    catalog: &mut ModelCatalog,
    preset: &ProviderPreset,
    api_key_env: &'static str,
) -> anyhow::Result<()> {
    let anthropic_endpoint = EndpointId(OPENCODE_ANTHROPIC_ENDPOINT_ID.into());
    if !catalog.has_endpoint(&anthropic_endpoint) {
        // Pi's Anthropic SDK appends /v1/messages to /zen. Ygg joins only the
        // final method path, so both protocol endpoints use the versioned URL.
        catalog.register_endpoint(Endpoint {
            id: anthropic_endpoint,
            base_url: url::Url::parse(preset.base_url)?,
            auth: preset_auth(Protocol::AnthropicMessages, api_key_env),
            default_headers: build_headers(preset.extra_headers)?,
            transport: ygg_ai::EndpointTransport::Http,
            timeout: PROVIDER_RESPONSE_HEADER_TIMEOUT,
        })?;
    }
    register_static_models(catalog, preset.id, OPENCODE_MODELS)
}

fn try_register_preset(catalog: &mut ModelCatalog, preset: &ProviderPreset) -> anyhow::Result<()> {
    let Some((api_key_env, api_key)) = resolve_first_env(preset.api_key_env) else {
        return Ok(());
    };

    if preset.id == crate::providers::DEEPSEEK.id {
        register_deepseek_v4_pro(catalog)?;
        register_discovered_deepseek_models(catalog)?;
        return Ok(());
    }

    register_preset_endpoint(catalog, preset, api_key_env)?;
    if preset.id == crate::providers::OPENCODE.id {
        register_opencode(catalog, preset, api_key_env)?;
    } else if preset.id == crate::providers::MINIMAX.id {
        register_static_models(catalog, preset.id, MINIMAX_MODELS)?;
    }

    match preset.model_discovery {
        ModelDiscovery::Static | ModelDiscovery::None => {}
        ModelDiscovery::OpenAiModels { filter } => {
            register_openai_compatible_models(catalog, preset, filter, &api_key)?;
        }
        ModelDiscovery::AnthropicModels => {
            register_anthropic_compatible_models(catalog, preset, &api_key)?;
        }
        ModelDiscovery::OpenRouterModels => {
            register_openrouter_models_for_preset(catalog, preset, &api_key)?;
        }
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
        if !has_model_id(target, &spec.id.0) {
            target.register_model(spec)?;
        }
    }
    Ok(())
}

/// Discover configured provider catalogs concurrently, then merge them on the
/// launch thread. A fleet outage therefore costs at most one bounded discovery
/// interval instead of one interval per configured account.
fn register_configured_presets_parallel(catalog: &mut ModelCatalog) {
    let mut jobs = Vec::new();
    for preset in BUILTIN_PROVIDERS {
        if resolve_first_env(preset.api_key_env).is_none() {
            continue;
        }
        let preset = *preset;
        match std::thread::Builder::new()
            .name(format!("ygg-{}-catalog", preset.id))
            .spawn(move || {
                let mut provider_catalog = ModelCatalog::default();
                try_register_preset(&mut provider_catalog, &preset)?;
                Ok::<_, anyhow::Error>(provider_catalog)
            }) {
            Ok(handle) => jobs.push((preset, handle)),
            Err(error) => eprintln!(
                "warning: could not start {} model discovery: {error}",
                preset.name
            ),
        }
    }

    for (preset, job) in jobs {
        match job.join() {
            Ok(Ok(provider_catalog)) => {
                if let Err(error) = merge_provider_catalog(catalog, provider_catalog) {
                    eprintln!("warning: {} unavailable: {error}", preset.name);
                }
            }
            Ok(Err(error)) => eprintln!("warning: {} unavailable: {error}", preset.name),
            Err(_) => eprintln!(
                "warning: {} unavailable: model discovery thread panicked",
                preset.name
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

fn load_custom_model_cache(
    store: &crate::auth::custom::CredentialStore,
    base_url: &str,
    credential_fingerprint: &str,
) -> anyhow::Result<Option<CachedCustomInventory>> {
    let Some(bytes) = store.load_model_cache()? else {
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

fn save_custom_model_cache(
    store: &crate::auth::custom::CredentialStore,
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
    store.save_model_cache(&serde_json::to_vec_pretty(&cache)?)
}

fn schedule_custom_model_cache_refresh(
    store: crate::auth::custom::CredentialStore,
    cred: crate::auth::custom::CustomCredential,
    credential_fingerprint: String,
    refresh_interval: Duration,
) {
    if cfg!(test) || !store.model_cache_is_stale(refresh_interval).unwrap_or(true) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("ygg-custom-catalog-refresh".to_owned())
        .spawn(move || {
            let discovered = discover_models_blocking(&cred, false);
            if !discovered.is_empty() {
                let _ = save_custom_model_cache(
                    &store,
                    &cred.base_url,
                    &credential_fingerprint,
                    &discovered,
                );
            }
        });
}

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
    if !store.model_cache_is_stale(refresh_interval).unwrap_or(true) {
        return cached;
    }

    let discovered =
        discover_and_cache_custom_models_with(store, cred, credential_fingerprint, false, discover);
    if discovered.is_empty() {
        // A transient discovery failure must not discard a last-good catalog.
        cached
    } else {
        discovered
    }
}

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
    let discovered = discover(cred);
    if persist_empty || !discovered.is_empty() {
        if let Err(error) =
            save_custom_model_cache(store, &cred.base_url, credential_fingerprint, &discovered)
        {
            eprintln!("warning: could not persist custom model metadata: {error}");
        }
    }
    discovered
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

fn resolve_custom_startup_timeout(
    configured_secs: Option<u64>,
    environment: Option<&str>,
) -> anyhow::Result<Duration> {
    let seconds = match environment.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<u64>().map_err(|error| {
            anyhow::anyhow!("invalid YGG_CUSTOM_STARTUP_TIMEOUT_SECS {value:?}: {error}")
        })?,
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
        if model.reasoning_uses_system_message {
            OpenAiChatReasoningMode::SystemMessage
        } else {
            OpenAiChatReasoningMode::Standard
        }
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

fn register_custom_openai_endpoint(
    catalog: &mut ModelCatalog,
    offline: bool,
) -> anyhow::Result<()> {
    use crate::auth::custom::{self, CustomModel};

    let store = custom::CredentialStore::new(custom::default_path());
    let Some(cred) = store.load()? else {
        return Ok(()); // no custom endpoint configured
    };
    // Optional top-level `cache` controls let gateways opt into Anthropic
    // markers and affinity headers without changing the legacy credential
    // struct shape.
    let cache = store
        .load_cache_compatibility()?
        .unwrap_or_else(ygg_ai::CacheCompatibility::default);
    let startup_timeout = resolve_custom_startup_timeout(
        store.load_startup_timeout_secs()?,
        std::env::var("YGG_CUSTOM_STARTUP_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    )?;

    let base_url = if cred.base_url.ends_with('/') {
        url::Url::parse(&cred.base_url)
    } else {
        url::Url::parse(&format!("{}/", cred.base_url))
    }
    .map_err(|e| anyhow::anyhow!("invalid custom endpoint base URL {}: {e}", cred.base_url))?;

    // When the file contains an API key, use it directly via Auth::bearer.
    // When the file leaves api_key empty, try the YGG_CUSTOM_API_KEY env var.
    // If neither is available, the endpoint is unauthenticated.
    let environment_key = std::env::var("YGG_CUSTOM_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());
    let effective_key = if !cred.api_key.is_empty() {
        cred.api_key.as_str()
    } else {
        environment_key.as_deref().unwrap_or_default()
    };
    let auth = if !cred.api_key.is_empty() {
        Auth::bearer(cred.api_key.as_str())
    } else if environment_key.is_some() {
        Auth::bearer_env("YGG_CUSTOM_API_KEY")
    } else {
        Auth::None
    };

    let mut default_headers = http::HeaderMap::new();
    for header in &cred.headers {
        let name = http::HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid header name {}: {e}", header.name))?;
        let value = http::HeaderValue::from_str(&header.value)
            .map_err(|e| anyhow::anyhow!("invalid header value for {}: {e}", header.name))?;
        default_headers.insert(name, value);
    }
    let custom_credential_fingerprint =
        custom_credential_fingerprint(effective_key, &default_headers);

    let endpoint = Endpoint {
        id: EndpointId(custom::ENDPOINT_ID.into()),
        base_url,
        auth,
        default_headers,
        transport: ygg_ai::EndpointTransport::Http,
        timeout: startup_timeout,
    };
    catalog.register_endpoint(endpoint)?;

    // A successful inventory is durable startup metadata, not something every
    // invocation should fetch again. A fresh cache removes a network round trip
    // from resume/model-picker latency and also keeps discovered local models
    // available in explicit offline mode. Once stale, refresh it before building
    // this process's catalog; a background-only refresh leaves a switched local
    // server displaying the previous model until Ygg is restarted again.
    let configured = configured_custom_models(&cred);
    let cached = if cred.auto_discover {
        match load_custom_model_cache(&store, &cred.base_url, &custom_credential_fingerprint) {
            Ok(models) => models,
            Err(error) => {
                eprintln!("warning: custom model cache unavailable: {error}");
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
                refresh_stale_custom_models_with(
                    &store,
                    &cred,
                    &custom_credential_fingerprint,
                    models,
                    PROVIDER_INVENTORY_REFRESH_INTERVAL,
                    discover_models,
                )
            }
        }
        Some(CachedCustomInventory::Unavailable)
            if cred.auto_discover && !offline && configured.is_empty() =>
        {
            // With no configured fallback, a background refresh cannot update
            // this process's catalog. Retry now so recovery is visible in this
            // launch instead of requiring a third restart.
            let discovered = discover_and_cache_custom_models_with(
                &store,
                &cred,
                &custom_credential_fingerprint,
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
                schedule_custom_model_cache_refresh(
                    store.clone(),
                    cred.clone(),
                    custom_credential_fingerprint.clone(),
                    NEGATIVE_INVENTORY_REFRESH_INTERVAL,
                );
            }
            configured
        }
        None if cred.auto_discover && !offline => {
            let discovered = discover_and_cache_custom_models_with(
                &store,
                &cred,
                &custom_credential_fingerprint,
                true,
                discover_models,
            );
            // A cold empty result is a negative marker, not a positive
            // inventory. It uses the short refresh policy above and is never
            // allowed to replace an existing last-good inventory.
            if discovered.is_empty() {
                configured
            } else {
                discovered
            }
        }
        None => configured,
    };
    if models.is_empty() {
        return Ok(());
    }

    for m in &models {
        let configured_display =
            (!m.display_name.trim().is_empty()).then(|| m.display_name.trim().to_owned());
        let canonical_label = configured_display.as_deref().unwrap_or(&m.api_name);
        let model_id = format!("custom/{canonical_label}");
        let input_mods = if m.vision {
            ModalitySet::none().with(ygg_ai::Modality::Image)
        } else {
            ModalitySet::none()
        };

        catalog.register_model(ModelSpec {
            id: ModelId(model_id),
            endpoint: EndpointId(custom::ENDPOINT_ID.into()),
            api_name: m.api_name.clone(),
            display_name: configured_display,
            protocol: Protocol::OpenAiChat,
            capabilities: Capabilities {
                input_modalities: input_mods,
                output_modalities: ModalitySet::none(),
                tools: m.tools,
                parallel_tool_calls: m.tools && m.parallel_tool_calls,
                reasoning: custom_reasoning_capability(m),
                structured_output: m.structured_output,
            },
            limits: ModelLimits {
                context_window: m.context_window,
                max_output_tokens: m.max_output_tokens,
            },
            pricing: None,
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
                eprintln!("warning: auto-discover URL parse failed: {e}");
            }
            return Vec::new();
        }
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            if report_errors {
                eprintln!("warning: auto-discover client build failed: {e}");
            }
            return Vec::new();
        }
    };

    let mut req = client.get(&models_url);
    let discovery_key = if !cred.api_key.trim().is_empty() {
        Some(cred.api_key.clone())
    } else {
        std::env::var("YGG_CUSTOM_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
    };
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
                eprintln!("warning: auto-discover GET {} failed: {e}", models_url);
            }
            return Vec::new();
        }
    };

    let status = resp.status();
    let body = match bounded_discovery_json(resp, "custom models") {
        Ok(value) => value,
        Err(error) => {
            if report_errors {
                eprintln!(
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
                eprintln!(
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
            reasoning_values,
            reasoning_default,
            // Auto-discovered local models are not guaranteed to implement
            // OpenAI's newer `developer` role. vLLM Qwen chat templates, in
            // particular, reject it while still accepting `system`.
            reasoning_uses_system_message: true,
        });
    }
    models
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
// `/models` response can downshift a Plus account or expose a larger Pro
// window, and is authoritative whenever available.
const CODEX_LEGACY_CONTEXT_WINDOW: u64 = 272_000;
const CODEX_5_6_CONTEXT_WINDOW: u64 = 372_000;
const CODEX_PRO_CONTEXT_WINDOW: u64 = 1_000_000;
const CODEX_MAX_OUTPUT_TOKENS: u64 = 128_000;
const CODEX_MODEL_CACHE_VERSION: u8 = 1;
const CODEX_MODEL_CACHE_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);
// This is the Codex `/models` schema compatibility version Ygg implements,
// not Ygg's package version. Sending `0.1.0` causes the backend to filter out
// models that require a contemporary Codex client.
const CODEX_MODELS_CLIENT_VERSION: &str = "0.145.0";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct DiscoveredCodexModel {
    id: String,
    context_window: u64,
    max_context_window: u64,
    max_output_tokens: u64,
    min_effort: ygg_ai::ReasoningEffort,
    max_effort: ygg_ai::ReasoningEffort,
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
        _ => None,
    }
}

fn codex_reasoning_range(
    entry: &serde_json::Value,
    model_id: &str,
) -> (ygg_ai::ReasoningEffort, ygg_ai::ReasoningEffort) {
    let efforts = entry
        .get("supported_reasoning_levels")
        .or_else(|| entry.get("supported_reasoning_efforts"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|level| {
            level.as_str().or_else(|| {
                level
                    .get("effort")
                    .or_else(|| level.get("value"))
                    .and_then(serde_json::Value::as_str)
            })
        })
        .filter_map(reasoning_effort)
        .collect::<Vec<_>>();
    let min = efforts
        .iter()
        .copied()
        .min()
        .unwrap_or(ygg_ai::ReasoningEffort::Minimal);
    let max = efforts
        .iter()
        .copied()
        .max()
        .unwrap_or_else(|| codex_max_effort(model_id));
    (min, max)
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
        let (min_effort, max_effort) = codex_reasoning_range(entry, id);
        models.push(DiscoveredCodexModel {
            id: id.to_owned(),
            context_window,
            max_context_window,
            max_output_tokens,
            min_effort,
            max_effort,
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
    if plan.is_some_and(crate::auth::codex::ChatGptPlan::uses_max_context_window) {
        max_context_window
    } else {
        default_context_window
    }
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

fn codex_pricing(model_id: &str) -> Option<Pricing> {
    let (input, output, cache_read, cache_write, tier) = match model_id {
        "gpt-5.3-codex-spark" => (1_750_000, 14_000_000, 175_000, 0, None),
        "gpt-5.4" => (
            2_500_000,
            15_000_000,
            250_000,
            0,
            Some((5_000_000, 22_500_000, 500_000, 0)),
        ),
        "gpt-5.4-mini" => (750_000, 4_500_000, 75_000, 0, None),
        "gpt-5.5" => (
            5_000_000,
            30_000_000,
            500_000,
            0,
            Some((10_000_000, 45_000_000, 1_000_000, 0)),
        ),
        "gpt-5.6-luna" => (
            1_000_000,
            6_000_000,
            100_000,
            1_250_000,
            Some((2_000_000, 9_000_000, 200_000, 2_500_000)),
        ),
        "gpt-5.6-sol" => (
            5_000_000,
            30_000_000,
            500_000,
            6_250_000,
            Some((10_000_000, 45_000_000, 1_000_000, 12_500_000)),
        ),
        "gpt-5.6-terra" => (
            2_500_000,
            15_000_000,
            250_000,
            3_125_000,
            Some((5_000_000, 22_500_000, 500_000, 6_250_000)),
        ),
        _ => return None,
    };
    let tiers = tier
        .map(|(input, output, cache_read, cache_write)| PricingTier {
            // Pi's source catalog expresses this as "above 272000".
            min_input_tokens: 272_001,
            input: Some(TokenRate(input)),
            output: Some(TokenRate(output)),
            cache_read: Some(TokenRate(cache_read)),
            cache_write_5m: Some(TokenRate(cache_write)),
            cache_write_1h: None,
            reasoning: None,
        })
        .into_iter()
        .collect();
    Some(Pricing {
        input: TokenRate(input),
        output: TokenRate(output),
        cache_read: TokenRate(cache_read),
        cache_write_5m: TokenRate(cache_write),
        cache_write_1h: None,
        reasoning: None,
        tiers,
    })
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
    let Some(bytes) = store.load_model_cache()? else {
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
    Ok(Some(cache.models))
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
            }
        })
        .collect()
}

fn codex_models_url() -> anyhow::Result<url::Url> {
    let mut url = url::Url::parse(crate::auth::codex::BACKEND_BASE_URL)?.join("models")?;
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
            headers.insert(
                http::HeaderName::from_static("openai-beta"),
                http::HeaderValue::from_static("responses=experimental"),
            );
            headers.insert(
                http::HeaderName::from_static("originator"),
                http::HeaderValue::from_static(crate::auth::codex::ORIGINATOR),
            );
            headers.insert(
                http::header::USER_AGENT,
                http::HeaderValue::from_str(&codex_user_agent())?,
            );

            let url = codex_models_url()?;
            let response = reqwest::Client::builder()
                .timeout(DISCOVERY_TIMEOUT)
                .build()?
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

/// Refresh stale Codex inventory after startup has already selected the cached
/// catalog. Only a comfortably unexpired token is used here, so the background
/// refresh cannot race the request resolver's refresh-token rotation.
fn schedule_codex_model_cache_refresh(store: crate::auth::codex::CredentialStore) {
    if cfg!(test)
        || !store
            .model_cache_is_stale(CODEX_MODEL_CACHE_REFRESH_INTERVAL)
            .unwrap_or(true)
    {
        return;
    }
    let token_is_fresh = store.load().ok().flatten().is_some_and(|credential| {
        crate::auth::codex::now_unix().saturating_add(crate::auth::codex::REFRESH_SKEW_SECS)
            < credential.expires_at
    });
    if !token_is_fresh {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("ygg-codex-catalog-refresh".to_owned())
        .spawn(move || {
            if let Ok(discovery) = discover_codex_models(store.clone()) {
                let _ = save_codex_model_cache(&store, &discovery);
            }
        });
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

    let Some(initial_claims) = codex::usable_subscription_claims(&store)? else {
        return Ok(());
    };

    // Tests use synthetic JWTs and must not contact the production catalog.
    // At runtime an account-and-plan-matched cache is authoritative for this
    // launch and stale metadata refreshes after startup. This keeps a 10-second
    // network timeout off the launch/resume critical path without widening the
    // cache across accounts. A first launch still performs one bounded discovery
    // to seed the cache, with the conservative built-in catalog as its fallback.
    let models = if offline {
        match load_codex_model_cache(&store, &initial_claims) {
            Ok(Some(models)) => models,
            Ok(None) => fallback_codex_models(initial_claims.plan.as_ref()),
            Err(error) => {
                eprintln!(
                    "warning: Codex model cache was unusable ({error}); using conservative offline fallback catalog"
                );
                fallback_codex_models(initial_claims.plan.as_ref())
            }
        }
    } else if cfg!(test) {
        fallback_codex_models(initial_claims.plan.as_ref())
    } else {
        match load_codex_model_cache(&store, &initial_claims) {
            Ok(Some(models)) => {
                schedule_codex_model_cache_refresh(store.clone());
                models
            }
            cache_result => match discover_codex_models(store.clone()) {
                Ok(discovery) => {
                    if let Err(error) = save_codex_model_cache(&store, &discovery) {
                        eprintln!("warning: could not persist Codex model metadata: {error}");
                    }
                    discovery.models
                }
                Err(discovery_error) => {
                    if let Err(cache_error) = cache_result {
                        eprintln!(
                            "warning: Codex model cache was unusable ({cache_error}); live discovery also failed ({discovery_error}); using conservative fallback catalog"
                        );
                    } else {
                        eprintln!(
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

    let mut default_headers = http::HeaderMap::new();
    default_headers.insert(
        http::HeaderName::from_static("openai-beta"),
        http::HeaderValue::from_static("responses=experimental"),
    );
    default_headers.insert(
        http::HeaderName::from_static("originator"),
        http::HeaderValue::from_static(codex::ORIGINATOR),
    );
    default_headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_str(&codex_user_agent())?,
    );

    catalog.register_endpoint(Endpoint {
        id: EndpointId(codex::ENDPOINT_ID.into()),
        base_url: url::Url::parse(codex::BACKEND_BASE_URL)?,
        auth: Auth::dynamic(resolver),
        default_headers,
        // AiClient currently implements HTTP/SSE only. Do not advertise a
        // WebSocket preference until the transport actually honors it.
        transport: ygg_ai::EndpointTransport::Http,
        timeout: PROVIDER_RESPONSE_HEADER_TIMEOUT,
    })?;

    for model in models {
        // Preserve familiar bare Codex ids when possible. If another API
        // already owns one, namespace only the colliding entry instead of
        // rejecting the account's entire live catalog.
        let catalog_id = if catalog.resolve(&ModelId(model.id.clone())).is_ok() {
            ModelId(format!("codex/{}", model.id))
        } else {
            ModelId(model.id.clone())
        };
        let pricing = codex_pricing(&model.id);
        let supports_image_input = codex_supports_image_input(&model.id);
        catalog.register_model(ModelSpec {
            id: catalog_id,
            endpoint: EndpointId(codex::ENDPOINT_ID.into()),
            api_name: model.id,
            display_name: None,
            protocol: Protocol::OpenAiResponses,
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
                structured_output: false,
            },
            limits: ModelLimits {
                context_window: model.context_window,
                max_output_tokens: model.max_output_tokens,
            },
            pricing,
            // Keep the application session ID consistent across the Responses
            // cache key and Codex's hyphenated affinity headers. The resolver
            // only owns credentials/account routing, never session identity.
            cache: ygg_ai::CacheCompatibility {
                supports_long_retention: false,
                send_session_id_header: false,
                session_affinity_format: Some(ygg_ai::SessionAffinityFormat::Codex),
                ..ygg_ai::CacheCompatibility::default()
            },
        })?;
    }
    Ok(())
}

fn base_model_catalog(offline: bool) -> anyhow::Result<ModelCatalog> {
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
        register_deepseek_v4_pro(&mut catalog)?;
    } else if !offline {
        register_configured_presets_parallel(&mut catalog);
    }

    // Explicit custom models remain usable offline; only auto-discovery is skipped.
    // Tests never inspect ambient HOME credentials.
    if !cfg!(test) {
        if let Err(error) = register_custom_openai_endpoint(&mut catalog, offline) {
            eprintln!("warning: custom endpoint unavailable: {error}");
        }
    }
    Ok(catalog)
}

/// Build the runtime model catalog, exposing ChatGPT subscription models only
/// when Ygg owns a usable OAuth credential.
pub fn model_catalog() -> anyhow::Result<ModelCatalog> {
    model_catalog_with_offline(false)
}

fn model_catalog_with_offline(offline: bool) -> anyhow::Result<ModelCatalog> {
    let mut catalog = base_model_catalog(offline)?;
    // Unit tests use explicit temporary credential stores and must never inspect
    // the developer's ambient HOME. Runtime offline mode still registers a
    // locally authenticated Codex endpoint, but never discovers or refreshes
    // its inventory over the network.
    if !cfg!(test) {
        let store = crate::auth::codex::CredentialStore::new(crate::auth::codex::default_path());
        // Non-fatal: a stale or malformed OAuth file must never block Ygg startup.
        if let Err(error) = register_openai_codex(&mut catalog, store, offline) {
            eprintln!("warning: OpenAI Codex models unavailable: {error}");
        }
    }
    Ok(catalog)
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
    let client = AiClient::try_new()?;
    Ok(Bootstrap {
        config,
        catalog,
        sessions,
        client,
        prepared_session: RefCell::new(None),
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
}

fn persisted_session_config(session: &Session) -> anyhow::Result<PersistedSessionConfig> {
    let path = session.path();
    let mut persisted = PersistedSessionConfig::default();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session
            .entry(id)
            .ok_or_else(|| anyhow::anyhow!("session head references missing entry {}", id.0))?;
        if let EntryValue::Config { model, reasoning } = &entry.value {
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
            if persisted.model.is_some() && persisted.reasoning.is_some() {
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
) -> anyhow::Result<()> {
    let persisted = persisted_session_config(session)?;
    if persisted.model.as_ref() == Some(model) && persisted.reasoning.as_ref() == Some(reasoning) {
        return Ok(());
    }
    session.append(EntryValue::Config {
        model: Some(model.0.clone()),
        reasoning: Some(crate::app::reasoning_label(reasoning)),
    })?;
    Ok(())
}

fn launch_configuration_parts(
    config: &Config,
    session: &SessionSelection,
) -> anyhow::Result<(Option<Session>, Option<ModelId>, ReasoningConfig)> {
    let prepared = match session {
        SessionSelection::OpenExisting(path) => Some(Session::open(path)?),
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
    Ok((prepared, model, reasoning))
}

fn launch_configuration(
    boot: &Bootstrap,
    session: &SessionSelection,
) -> anyhow::Result<(Option<ModelId>, ReasoningConfig)> {
    let (prepared, model, reasoning) = launch_configuration_parts(&boot.config, session)?;
    *boot.prepared_session.borrow_mut() = prepared;
    Ok((model, reasoning))
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
        ResumeSelector::Resume(None) => {
            let sessions = boot.sessions.clone();
            let session_dir = sessions.dir().to_owned();
            let available =
                run_blocking_lifecycle(shell, input, "discovering sessions…", move || {
                    Ok(sessions.list())
                })
                .await?;
            session_picker(shell, input, &available, &session_dir)
                .await?
                .map(SessionSelection::OpenExisting)
                .ok_or_else(|| anyhow::anyhow!("session selection cancelled"))?
        }
    };
    let config = boot.config.clone();
    let selected_session = session.clone();
    let (prepared, model, reasoning) =
        run_blocking_lifecycle(shell, input, "replaying session…", move || {
            launch_configuration_parts(&config, &selected_session)
        })
        .await?;
    *boot.prepared_session.borrow_mut() = prepared;
    let model = match model {
        Some(model) => model,
        None => model_picker(shell, input, &boot.catalog).await?,
    };
    Ok(LaunchSelection {
        model,
        session,
        reasoning,
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
        ResumeSelector::Resume(None) => {
            anyhow::bail!("--resume needs a session id in print mode")
        }
    };
    let (model, reasoning) = launch_configuration(boot, &session)?;
    let model = model.ok_or_else(|| {
        let mut models = boot
            .catalog
            .models()
            .map(|model| model.id.0.clone())
            .collect::<Vec<_>>();
        models.sort();
        anyhow::anyhow!(
            "no model configured: pass --model <id>, resume a session with model provenance, or set model in .ygg/config.toml (available: {})",
            models.join(", ")
        )
    })?;

    Ok(LaunchSelection {
        model,
        session,
        reasoning,
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
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn validate_explicit_tool_policy(
    config: &Config,
    extensions: &ExtensionHost,
    model: &Model,
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
        .filter(|name| !registered.contains(*name))
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

fn validate_active_skill_policy(
    session: &Session,
    extensions: &ExtensionHost,
) -> anyhow::Result<()> {
    let Some(head) = session.head() else {
        return Ok(());
    };
    let active = session
        .resolve_active_skills(&head)
        .context("failed to resolve active skills for the selected session")?;
    if active.active_skills.is_empty() {
        return Ok(());
    }

    let mut registered = extensions
        .tool_definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    registered.sort();
    registered.dedup();
    let available = if registered.is_empty() {
        "(none)".to_owned()
    } else {
        registered.join(", ")
    };

    for skill in active.active_skills {
        if let Err(error) = validate_skill_requirements(&skill.descriptor, &registered) {
            anyhow::bail!(
                "active skill {:?} cannot resume under the final tool policy: {error}; available tools: {available}. Re-enable its required tools, sandbox capabilities, or executable extensions before resuming this session",
                skill.descriptor.id,
            );
        }
    }
    Ok(())
}

fn configured_extensions(
    skills: Arc<dyn SkillRegistry>,
    config: &Config,
    session: &Session,
    model: &Model,
    reasoning: &ReasoningConfig,
    sessions: &SessionStore,
) -> (ExtensionHost, ExecutableExtensions) {
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    if !skills.descriptors().is_empty() {
        extensions.load(&SkillToolsExtension::new(skills));
    }
    let executable_extensions = ExecutableExtensions::discover_and_start(
        config,
        session,
        model,
        reasoning,
        sessions,
        &mut extensions,
    );
    extensions.retain_tools(|name| model.spec.capabilities.tools && config.tool_available(name));
    (extensions, executable_extensions)
}

pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App> {
    let Bootstrap {
        mut config,
        catalog,
        sessions,
        client,
        prepared_session,
    } = boot;
    let model = catalog.resolve(&launch.model)?;
    let compact_model = config
        .compaction
        .compact_model
        .as_ref()
        .map(|id| catalog.resolve(id))
        .transpose()
        .with_context(|| "configured compaction model could not be resolved")?;
    let mut prepared_session = prepared_session.into_inner();
    let mut session = match launch.session {
        SessionSelection::CreateNew(path) => {
            if let Some(parent) = path.parent() {
                create_private_session_dir(parent)?;
            }
            Session::create(path)?
        }
        SessionSelection::OpenExisting(path) => match prepared_session.take() {
            Some(session) if session.path() == path => session,
            _ => Session::open(path)?,
        },
    };

    let reasoning = normalize_reasoning_for_model(&launch.reasoning, &model)?;
    append_config_if_changed(&mut session, &model.spec.id, &reasoning)?;
    config.model = Some(model.spec.id.clone());
    config.reasoning = reasoning.clone();

    let skills: Arc<dyn SkillRegistry> = Arc::new(FileSystemSkillRegistry::new(
        config.workspace.clone(),
        config.skill_paths.clone(),
        config.workspace_trusted,
    )?);
    let prompts = Arc::new(PromptRegistry::discover(
        &config.workspace,
        &config.prompt_paths,
        config.workspace_trusted,
    ));
    let (extensions, executable_extensions) = configured_extensions(
        skills.clone(),
        &config,
        &session,
        &model,
        &reasoning,
        &sessions,
    );
    validate_explicit_tool_policy(&config, &extensions, &model)?;
    validate_active_skill_policy(&session, &extensions)?;
    let system_tokens = estimate_text_tokens(&system);
    let tool_schema_tokens = tool_schema_reserve(&extensions.tool_definitions());
    let mut agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
        cache_retention: config.cache_retention,
        session_id: None,
    })?;
    agent.set_prompt_model_source(Some(crate::tui::theme::model_lab(&model).key().to_owned()));
    agent.set_prompt_color(Some(crate::tui::theme::prompt_color_for_model(&model)));
    agent.set_compaction_model(compact_model);
    agent.set_compaction_policy(
        config.compaction.enabled,
        config.compaction.threshold_fraction,
        config.compaction.keep_recent_turns,
    )?;
    agent.set_max_session_cost_microdollars(config.max_cost_microdollars);

    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens,
        skills,
        prompts,
        executable_extensions,
    })
}

/// Recreate the Agent at an idle boundary. Taking `App` by value guarantees the
/// old Agent and its session file are dropped before a session is reopened.
pub fn rebuild_app(
    app: App,
    new_model: Option<Model>,
    new_reasoning: Option<ReasoningConfig>,
    selection: Option<SessionSelection>,
) -> anyhow::Result<App> {
    let App {
        agent,
        model,
        client,
        mut config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens: _,
        skills: _,
        prompts: _,
        mut executable_extensions,
    } = app;
    executable_extensions.shutdown_blocking();
    let compact_model = config
        .compaction
        .compact_model
        .as_ref()
        .map(|id| catalog.resolve(id))
        .transpose()
        .with_context(|| "configured compaction model could not be resolved")?;
    let current_path = agent.session().path().to_owned();
    drop(agent);

    let (persisted, mut prepared_session) = match selection.as_ref() {
        Some(SessionSelection::OpenExisting(path)) => {
            let session = Session::open(path)?;
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
    let old_model = model;
    let model = new_model
        .or(restored_model)
        .unwrap_or_else(|| old_model.clone());
    let reasoning = match (new_reasoning, persisted.reasoning) {
        (Some(reasoning), _) => normalize_reasoning_for_model(&reasoning, &model)?,
        (None, Some(reasoning)) => normalize_reasoning_for_model(&reasoning, &model)?,
        (None, None) if changing_model => {
            let level = level_from_reasoning(&reasoning, &old_model)?;
            thinking_to_reasoning(level, &model)?
        }
        (None, None) => normalize_reasoning_for_model(&reasoning, &model)?,
    };
    let mut session = match selection {
        Some(SessionSelection::CreateNew(path)) => {
            if let Some(parent) = path.parent() {
                create_private_session_dir(parent)?;
            }
            Session::create(path)?
        }
        Some(SessionSelection::OpenExisting(path)) => match prepared_session.take() {
            Some(session) if session.path() == path => session,
            _ => Session::open(path)?,
        },
        None => Session::open(current_path)?,
    };
    append_config_if_changed(&mut session, &model.spec.id, &reasoning)?;

    config.model = Some(model.spec.id.clone());
    config.reasoning = reasoning.clone();
    let skills: Arc<dyn SkillRegistry> = Arc::new(FileSystemSkillRegistry::new(
        config.workspace.clone(),
        config.skill_paths.clone(),
        config.workspace_trusted,
    )?);
    let prompts = Arc::new(PromptRegistry::discover(
        &config.workspace,
        &config.prompt_paths,
        config.workspace_trusted,
    ));
    let (extensions, executable_extensions) = configured_extensions(
        skills.clone(),
        &config,
        &session,
        &model,
        &reasoning,
        &sessions,
    );
    validate_explicit_tool_policy(&config, &extensions, &model)?;
    validate_active_skill_policy(&session, &extensions)?;
    let tool_schema_tokens = tool_schema_reserve(&extensions.tool_definitions());
    let mut agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
        cache_retention: config.cache_retention,
        session_id: None,
    })?;
    agent.set_prompt_model_source(Some(crate::tui::theme::model_lab(&model).key().to_owned()));
    agent.set_prompt_color(Some(crate::tui::theme::prompt_color_for_model(&model)));
    agent.set_compaction_model(compact_model);
    agent.set_compaction_policy(
        config.compaction.enabled,
        config.compaction.threshold_fraction,
        config.compaction.keep_recent_turns,
    )?;
    agent.set_max_session_cost_microdollars(config.max_cost_microdollars);

    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens,
        skills,
        prompts,
        executable_extensions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_endpoint_startup_timeout_is_cold_start_safe_and_configurable() {
        assert_eq!(
            resolve_custom_startup_timeout(None, None).unwrap(),
            Duration::from_secs(300)
        );
        assert_eq!(
            resolve_custom_startup_timeout(Some(420), None).unwrap(),
            Duration::from_secs(420)
        );
        assert_eq!(
            resolve_custom_startup_timeout(Some(420), Some(" 600 ")).unwrap(),
            Duration::from_secs(600)
        );
        assert!(resolve_custom_startup_timeout(None, Some("0")).is_err());
        assert!(resolve_custom_startup_timeout(None, Some("not-a-number")).is_err());
    }
    use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

    fn config(directory: &std::path::Path, model: Option<&str>) -> Config {
        Config {
            workspace: directory.to_path_buf(),
            invocation_cwd: directory.to_path_buf(),
            model: model.map(|model| ModelId(model.to_owned())),
            model_explicit: model.is_some(),
            reasoning: ReasoningConfig::Off,
            reasoning_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            plain: false,
            session_dir: directory.join("sessions"),
            compaction: CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: Mode::Print {
                prompt: "hi".to_owned(),
            },
            resume: ResumeSelector::New,
            mouse: crate::config::MouseMode::Auto,
            skill_paths: vec![],
            extension_paths: vec![],
            enabled_extensions: vec![],
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![],
            tools: crate::config::ToolPolicy::default(),
            context_files: true,
            offline: true,
            workspace_trusted: true,
        }
    }

    fn configured_test_extensions(
        skills: Arc<dyn SkillRegistry>,
        config: &Config,
    ) -> ExtensionHost {
        let boot = bootstrap(config.clone()).unwrap();
        let model_id = config.model.as_ref().expect("test model");
        let model = boot.catalog.resolve(model_id).unwrap();
        let session = Session::create(config.workspace.join("tool-policy-test.jsonl")).unwrap();
        configured_extensions(
            skills,
            config,
            &session,
            &model,
            &config.reasoning,
            &boot.sessions,
        )
        .0
    }

    fn append_active_skill(session: &mut Session, id: &str, required_tools: &[&str]) {
        session
            .append(EntryValue::SkillActivated {
                descriptor: ygg_agent::SkillDescriptor {
                    id: id.into(),
                    name: id.into(),
                    description: "test active skill".into(),
                    version: None,
                    source: ygg_agent::SkillSource::BuiltIn,
                    trust: ygg_agent::SkillTrust::BuiltIn,
                    required_tools: required_tools
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect(),
                    tags: vec![],
                },
                instructions_hash: "test-hash".into(),
                instructions: "test instructions".into(),
            })
            .unwrap();
    }

    #[test]
    fn configured_compaction_model_is_resolved_into_the_agent() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(directory.path(), Some("gpt-4o-mini"));
        config.compaction.compact_model = Some(ModelId("gpt-4o-mini".into()));
        let boot = bootstrap(config).unwrap();
        let app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
                reasoning: ReasoningConfig::Off,
            },
            "system".into(),
        )
        .unwrap();
        assert_eq!(
            app.agent
                .compaction_model()
                .map(|model| model.spec.id.0.as_str()),
            Some("gpt-4o-mini")
        );
    }

    #[test]
    fn model_resolution_has_cli_project_global_precedence() {
        let id = |value: &str| Some(ModelId(value.into()));
        assert_eq!(
            resolve_model_id(id("cli"), id("project"), id("global")),
            id("cli")
        );
        assert_eq!(
            resolve_model_id(None, id("project"), id("global")),
            id("project")
        );
        assert_eq!(resolve_model_id(None, None, id("global")), id("global"));
        assert_eq!(resolve_model_id(None, None, None), None);
    }

    #[test]
    fn opencode_discovery_infers_supported_protocols_and_skips_gemini() {
        let preset = &crate::providers::OPENCODE;
        assert_eq!(
            discovered_preset_binding(preset, "gpt-future"),
            Some(("opencode", Protocol::OpenAiResponses))
        );
        assert_eq!(
            discovered_preset_binding(preset, "claude-future"),
            Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
        );
        assert_eq!(
            discovered_preset_binding(preset, "qwen3.7-plus"),
            Some((OPENCODE_ANTHROPIC_ENDPOINT_ID, Protocol::AnthropicMessages))
        );
        assert_eq!(discovered_preset_binding(preset, "gemini-future"), None);
        assert_eq!(
            discovered_preset_binding(preset, "kimi-future"),
            Some(("opencode", Protocol::OpenAiChat))
        );
    }

    #[test]
    fn opencode_static_models_use_protocol_specific_endpoints() {
        let mut catalog = ModelCatalog::default();
        let preset = &crate::providers::OPENCODE;
        register_preset_endpoint(&mut catalog, preset, "YGG_TEST_OPENCODE_KEY").unwrap();
        register_opencode(&mut catalog, preset, "YGG_TEST_OPENCODE_KEY").unwrap();

        let responses = catalog
            .resolve(&ModelId("opencode/gpt-5.6-sol".into()))
            .unwrap();
        assert_eq!(responses.spec.protocol, Protocol::OpenAiResponses);
        assert_eq!(responses.endpoint.id.0, "opencode");
        assert_eq!(
            responses.endpoint.base_url.as_str(),
            "https://opencode.ai/zen/v1/"
        );

        let anthropic = catalog
            .resolve(&ModelId("opencode/claude-sonnet-4-6".into()))
            .unwrap();
        assert_eq!(anthropic.spec.protocol, Protocol::AnthropicMessages);
        assert_eq!(anthropic.endpoint.id.0, OPENCODE_ANTHROPIC_ENDPOINT_ID);
        assert_eq!(
            anthropic.endpoint.base_url.as_str(),
            "https://opencode.ai/zen/v1/"
        );

        let chat = catalog
            .resolve(&ModelId("opencode/deepseek-v4-pro".into()))
            .unwrap();
        assert_eq!(chat.spec.protocol, Protocol::OpenAiChat);
        assert_eq!(chat.endpoint.id.0, "opencode");
        assert!(catalog
            .resolve(&ModelId("opencode/gemini-3.1-pro".into()))
            .is_err());
    }

    #[test]
    fn minimax_static_models_use_the_anthropic_protocol() {
        let mut catalog = ModelCatalog::default();
        let preset = &crate::providers::MINIMAX;
        register_preset_endpoint(&mut catalog, preset, "YGG_TEST_MINIMAX_KEY").unwrap();
        register_static_models(&mut catalog, preset.id, MINIMAX_MODELS).unwrap();

        let model = catalog
            .resolve(&ModelId("minimax/MiniMax-M3".into()))
            .unwrap();
        assert_eq!(model.spec.protocol, Protocol::AnthropicMessages);
        assert_eq!(model.endpoint.id.0, "minimax");
        assert_eq!(
            model.endpoint.base_url.as_str(),
            "https://api.minimax.io/anthropic/v1/"
        );
        assert!(model
            .spec
            .capabilities
            .input_modalities
            .contains(ygg_ai::Modality::Image));
    }

    #[test]
    fn metadata_sparse_multimodal_model_ids_get_a_vision_fallback() {
        let response = serde_json::json!({
            "data": [{
                "id": "Intel/Qwen3.6-27B-int4-AutoRound",
                "max_model_len": 131_072
            }]
        });
        let models = api_models_from_response(&response).unwrap();
        assert_eq!(models.len(), 1);
        assert!(models[0].vision);
        assert!(model_id_implies_vision("gemini-2.5-pro"));
        assert!(model_id_implies_vision("anthropic/claude-sonnet-4-6"));
        assert!(model_id_implies_vision("Qwen/Qwen2.5-VL-7B"));
        assert!(!model_id_implies_vision("Qwen/Qwen3-Coder-30B"));
    }

    #[test]
    fn model_inventory_normalizes_flattened_audio_modalities() {
        let response = serde_json::json!({
            "data": [{
                "id": "audio-model",
                "input_modalities": ["text", "audio"]
            }]
        });
        let models = api_models_from_response(&response).unwrap();
        assert_eq!(models.len(), 1);
        assert!(!models[0].vision);
        assert!(models[0].audio);
    }

    #[test]
    fn custom_model_inventory_defaults_sparse_metadata_to_tool_capable() {
        let response = serde_json::json!({
            "data": [
                {"id": "unknown"},
                {"id": "parameters", "supported_parameters": ["tools"]},
                {"id": "empty-parameters", "supported_parameters": []},
                {
                    "id": "capability-object",
                    "capabilities": {"tool_calling": {"supported": true}}
                },
                {
                    "id": "provider-metadata",
                    "provider": {"capabilities": {"function_calling": true}}
                },
                {
                    "id": "explicitly-disabled",
                    "supports_tools": false,
                    "supported_parameters": ["tools"]
                }
            ]
        });
        let models = api_models_from_response(&response).unwrap();
        let tools = models
            .iter()
            .map(|model| (model.id.as_str(), model.tools))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert!(tools["unknown"]);
        assert!(tools["parameters"]);
        assert!(!tools["empty-parameters"]);
        assert!(tools["capability-object"]);
        assert!(tools["provider-metadata"]);
        assert!(!tools["explicitly-disabled"]);
    }

    #[test]
    fn openrouter_discovery_uses_live_ids_limits_and_capabilities() {
        let response = serde_json::json!({
            "data": [
                {
                    "id": "zeta/model",
                    "context_length": 64_000,
                    "top_provider": { "max_completion_tokens": 8_000 },
                    "architecture": { "input_modalities": ["text", "image", "audio"] },
                    "supported_parameters": ["tools", "tool_choice", "reasoning", "reasoning.effort"],
                    "pricing": {
                        "prompt": "0.00000015",
                        "completion": "0.00000060",
                        "input_cache_read": "0.000000075"
                    }
                },
                {
                    "id": "alpha/model",
                    "context_length": 8_000,
                    "top_provider": { "max_completion_tokens": 16_000 },
                    "supported_parameters": []
                }
            ]
        });

        let models = openrouter_models_from_response(&response).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id.0, "openrouter/alpha/model");
        assert_eq!(models[1].id.0, "openrouter/zeta/model");
        assert_eq!(models[1].api_name, "zeta/model");
        assert_eq!(models[1].limits.context_window, 64_000);
        assert_eq!(models[1].limits.max_output_tokens, 8_000);
        assert!(models[1].capabilities.tools);
        assert!(models[1].capabilities.reasoning.is_some());
        assert_eq!(
            models[1]
                .capabilities
                .reasoning
                .as_ref()
                .unwrap()
                .openai_chat_mode,
            OpenAiChatReasoningMode::OpenRouter
        );
        let pricing = models[1].pricing.as_ref().expect("OpenRouter price");
        assert_eq!(pricing.input, TokenRate(150_000));
        assert_eq!(pricing.output, TokenRate(600_000));
        assert_eq!(pricing.cache_read, TokenRate(75_000));
        assert!(models[1]
            .capabilities
            .input_modalities
            .contains(ygg_ai::Modality::Image));
        assert!(models[1]
            .capabilities
            .input_modalities
            .contains(ygg_ai::Modality::Audio));
        // An advertised output limit cannot exceed the model context window.
        assert_eq!(models[0].limits.max_output_tokens, 8_000);
        assert!(!models[0].capabilities.tools);
    }

    #[test]
    fn openrouter_anthropic_routes_enable_anthropic_cache_markers() {
        let response = serde_json::json!({
            "data": [{
                "id": "anthropic/claude-sonnet-4-5",
                "context_length": 200_000,
                "top_provider": { "max_completion_tokens": 8_192 }
            }]
        });
        let models = openrouter_models_from_response(&response).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].cache.cache_control_format,
            Some(ygg_ai::CacheControlFormat::Anthropic)
        );
    }

    fn write_codex_credential(path: &std::path::Path, localhost: bool, plan: &str) {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_test",
                "chatgpt_plan_type": plan,
                "localhost": localhost
            }
        });
        let access = format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "access_token": access,
                    "refresh_token": "refresh",
                    "account_id": "acct_test"
                },
                "expires_at": u64::MAX
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn codex_models_require_a_usable_credential_and_include_luna_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.json");
        let store = crate::auth::codex::CredentialStore::new(&path);

        let mut catalog = base_model_catalog(true).unwrap();
        register_openai_codex(&mut catalog, store.clone(), false).unwrap();
        assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

        write_codex_credential(&path, true, "plus");
        let mut catalog = base_model_catalog(true).unwrap();
        let error = register_openai_codex(&mut catalog, store.clone(), false).unwrap_err();
        assert!(error.to_string().contains("localhost-only"));
        assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

        write_codex_credential(&path, false, "plus");
        let mut catalog = base_model_catalog(true).unwrap();
        register_openai_codex(&mut catalog, store, false).unwrap();
        for model_id in crate::auth::codex::MODELS {
            let model = catalog.resolve(&ModelId((*model_id).into())).unwrap();
            assert_eq!(model.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
            assert_eq!(model.spec.protocol, Protocol::OpenAiResponses);
            assert_eq!(
                model.spec.limits.context_window,
                if model_id.starts_with("gpt-5.6-") {
                    372_000
                } else {
                    272_000
                }
            );
            assert_eq!(model.spec.limits.max_output_tokens, 128_000);
            assert!(model.spec.pricing.is_some());
            assert!(!model.spec.cache.supports_long_retention);
            assert!(!model.spec.cache.send_session_id_header);
            assert_eq!(
                model.spec.cache.session_affinity_format,
                Some(ygg_ai::SessionAffinityFormat::Codex)
            );
            assert_eq!(model.endpoint.transport, ygg_ai::EndpointTransport::Http);
        }
        let sol = catalog.resolve(&ModelId("gpt-5.6-sol".into())).unwrap();
        assert_eq!(crate::compaction::context_window(&sol), 372_000);

        // Pro is not in the fallback subscription catalog. Luna is included and
        // live account discovery can add or remove models independently of it.
        assert!(catalog.resolve(&ModelId("gpt-5.5-pro".into())).is_err());
        assert!(catalog.resolve(&ModelId("gpt-5.6-luna".into())).is_ok());
    }

    #[test]
    fn offline_codex_registration_uses_account_cache_or_fallback_without_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cached-codex.json");
        write_codex_credential(&path, false, "plus");
        let store = crate::auth::codex::CredentialStore::new(&path);
        let claims = crate::auth::codex::usable_subscription_claims(&store)
            .unwrap()
            .unwrap();
        let cached = CodexDiscovery {
            claims,
            models: codex_models_from_response(
                &serde_json::json!({
                    "models": [{
                        "slug": "cached-account-model",
                        "context_window": 196_000,
                        "max_output_tokens": 24_000
                    }]
                }),
                Some(&crate::auth::codex::ChatGptPlan::Plus),
            )
            .unwrap(),
        };
        save_codex_model_cache(&store, &cached).unwrap();

        let mut catalog = base_model_catalog(true).unwrap();
        register_openai_codex(&mut catalog, store, true).unwrap();
        let model = catalog
            .resolve(&ModelId("cached-account-model".into()))
            .unwrap();
        assert_eq!(model.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
        assert_eq!(model.spec.limits.context_window, 196_000);

        let fallback_path = directory.path().join("fallback-codex.json");
        write_codex_credential(&fallback_path, false, "plus");
        let mut fallback_catalog = base_model_catalog(true).unwrap();
        register_openai_codex(
            &mut fallback_catalog,
            crate::auth::codex::CredentialStore::new(fallback_path),
            true,
        )
        .unwrap();
        let fallback = fallback_catalog
            .resolve(&ModelId("gpt-5.6-sol".into()))
            .unwrap();
        assert_eq!(fallback.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
    }

    #[test]
    fn codex_spark_is_registered_as_image_capable() {
        assert!(codex_supports_image_input("gpt-5.3-codex-spark"));
        assert!(codex_supports_image_input("gpt-5.3-codex"));
        assert!(codex_supports_image_input("gpt-5.4-mini"));
        assert!(codex_supports_image_input("gpt-5.4-pro"));
        assert!(codex_supports_image_input("gpt-5.5"));
        assert!(codex_supports_image_input("gpt-5.5-pro"));
        assert!(codex_supports_image_input("gpt-5.6-sol"));
        assert!(codex_supports_image_input("gpt-5.6-luna"));
        assert!(codex_supports_image_input("gpt-5.1-codex"));
        assert!(codex_supports_image_input("gpt-5.1-codex-mini"));
        assert!(codex_supports_image_input("gpt-5.1-codex-max"));
        assert!(codex_supports_image_input("codex-mini-latest"));
        assert!(!codex_supports_image_input("gpt-5-codex"));
    }

    #[test]
    fn codex_catalog_query_uses_the_implemented_schema_version() {
        let url = codex_models_url().unwrap();
        assert_eq!(url.path(), "/backend-api/codex/models");
        assert_eq!(
            url.query_pairs()
                .find(|(name, _)| name == "client_version")
                .map(|(_, value)| value.into_owned()),
            Some(CODEX_MODELS_CLIENT_VERSION.to_string())
        );
    }

    #[test]
    fn codex_discovery_accepts_account_catalog_and_uses_live_metadata() {
        let body = serde_json::json!({
            "models": [
                {
                    "slug": "gpt-5.6-luna",
                    "context_window": 400_000,
                    "max_output_tokens": 150_000,
                    "supported_reasoning_levels": [
                        {"effort": "low"},
                        {"effort": "max"}
                    ]
                },
                {"slug": "gpt-account-preview"},
                "gpt-string-preview",
                {"slug": "gpt-5.6-luna"}
            ]
        });
        let models = codex_models_from_response(&body, None).unwrap();
        assert_eq!(models.len(), 3, "duplicate slugs must be collapsed");
        let luna = models
            .iter()
            .find(|model| model.id == "gpt-5.6-luna")
            .unwrap();
        assert_eq!(luna.context_window, 400_000);
        assert_eq!(luna.max_context_window, 400_000);
        assert_eq!(luna.max_output_tokens, 150_000);
        assert_eq!(luna.min_effort, ygg_ai::ReasoningEffort::Low);
        assert_eq!(luna.max_effort, ygg_ai::ReasoningEffort::Max);
        assert_eq!(
            models
                .iter()
                .find(|model| model.id == "gpt-string-preview")
                .unwrap()
                .context_window,
            CODEX_LEGACY_CONTEXT_WINDOW
        );
    }

    #[test]
    fn codex_discovery_selects_default_or_max_window_from_oauth_plan() {
        let body = serde_json::json!({
            "models": [{
                "slug": "gpt-5.4",
                "context_window": 272_000,
                "max_context_window": 1_000_000
            }]
        });
        let plus = crate::auth::codex::ChatGptPlan::Plus;
        let pro = crate::auth::codex::ChatGptPlan::Pro;
        let pro_lite = crate::auth::codex::ChatGptPlan::ProLite;

        assert_eq!(
            codex_models_from_response(&body, Some(&plus)).unwrap()[0].context_window,
            272_000
        );
        assert_eq!(
            codex_models_from_response(&body, Some(&pro)).unwrap()[0].context_window,
            1_000_000
        );
        assert_eq!(
            codex_models_from_response(&body, Some(&pro_lite)).unwrap()[0].context_window,
            1_000_000
        );
    }

    #[test]
    fn codex_model_cache_is_scoped_to_account_and_plan() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::codex::CredentialStore::new(directory.path().join("codex.json"));
        let plus = crate::auth::codex::ChatGptPlan::Plus;
        let claims = crate::auth::codex::SubscriptionClaims {
            account_id: "acct-a".into(),
            plan: Some(plus.clone()),
        };
        let body = serde_json::json!({
            "models": [{"slug": "gpt-5.6-sol", "context_window": 272_000}]
        });
        let discovery = CodexDiscovery {
            models: codex_models_from_response(&body, Some(&plus)).unwrap(),
            claims: claims.clone(),
        };
        save_codex_model_cache(&store, &discovery).unwrap();
        assert_eq!(
            load_codex_model_cache(&store, &claims).unwrap(),
            Some(discovery.models)
        );

        let upgraded = crate::auth::codex::SubscriptionClaims {
            account_id: "acct-a".into(),
            plan: Some(crate::auth::codex::ChatGptPlan::Pro),
        };
        assert!(load_codex_model_cache(&store, &upgraded).unwrap().is_none());
        let other_account = crate::auth::codex::SubscriptionClaims {
            account_id: "acct-b".into(),
            plan: Some(plus),
        };
        assert!(load_codex_model_cache(&store, &other_account)
            .unwrap()
            .is_none());
    }

    #[test]
    fn generic_discovery_accepts_openai_codex_and_bare_array_shapes() {
        for body in [
            serde_json::json!({"data": [{"id": "a", "context_length": 10}]}),
            serde_json::json!({"models": [{"slug": "b", "max_model_len": 20}]}),
            serde_json::json!([{"id": "c", "max_context_tokens": 30}]),
        ] {
            let models = api_models_from_response(&body).unwrap();
            assert_eq!(models.len(), 1);
            assert!(models[0].context_window.is_some());
        }
    }

    #[test]
    fn discovery_rejects_error_objects_instead_of_hiding_them_as_empty() {
        assert!(api_models_from_response(&serde_json::json!({
            "error": {"message": "unauthorized"}
        }))
        .is_err());
        assert!(codex_models_from_response(&serde_json::json!({"models": []}), None).is_err());
    }

    #[test]
    fn provider_inventory_cache_is_private_and_scoped_to_provider_url_and_account() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache/openrouter.json");
        let body = serde_json::json!({"data": [{"id": "model-a"}]});
        let first_key = credential_fingerprint("key-one");
        save_provider_inventory_cache(
            &path,
            "openrouter",
            "https://one.test/v1/models",
            &first_key,
            Some(&body),
        )
        .unwrap();
        match load_provider_inventory_cache(
            &path,
            "openrouter",
            "https://one.test/v1/models",
            &first_key,
        )
        .unwrap()
        {
            Some(CachedProviderInventory::Available(cached)) => assert_eq!(cached, body),
            _ => panic!("expected cached provider inventory"),
        }
        assert!(load_provider_inventory_cache(
            &path,
            "opencode",
            "https://one.test/v1/models",
            &first_key,
        )
        .unwrap()
        .is_none());
        assert!(load_provider_inventory_cache(
            &path,
            "openrouter",
            "https://two.test/v1/models",
            &first_key,
        )
        .unwrap()
        .is_none());
        assert!(
            load_provider_inventory_cache(
                &path,
                "openrouter",
                "https://one.test/v1/models",
                &credential_fingerprint("key-two"),
            )
            .unwrap()
            .is_none(),
            "changing accounts must invalidate the cached inventory"
        );
        save_provider_inventory_cache(
            &path,
            "openrouter",
            "https://one.test/v1/models",
            &first_key,
            None,
        )
        .unwrap();
        assert!(
            matches!(
                load_provider_inventory_cache(
                    &path,
                    "openrouter",
                    "https://one.test/v1/models",
                    &first_key,
                )
                .unwrap(),
                Some(CachedProviderInventory::Unavailable)
            ),
            "failed discovery must leave a reusable negative cache marker"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn negative_provider_cache_recovers_in_the_current_launch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache/openrouter.json");
        let url = "https://openrouter.test/v1/models";
        let credential = "key-one";
        let fingerprint = credential_fingerprint(credential);
        save_provider_inventory_cache(&path, "openrouter", url, &fingerprint, None).unwrap();
        let recovered = serde_json::json!({"data": [{"id": "recovered-model"}]});

        let body = cached_provider_inventory_with_fetch(
            path.clone(),
            "openrouter",
            url.to_string(),
            http::HeaderMap::new(),
            credential,
            |_, _| Ok(recovered.clone()),
        )
        .unwrap()
        .expect("a foreground retry should recover the inventory");
        assert_eq!(body, recovered);
        assert!(matches!(
            load_provider_inventory_cache(&path, "openrouter", url, &fingerprint).unwrap(),
            Some(CachedProviderInventory::Available(body)) if body == recovered
        ));
    }

    #[test]
    fn failed_provider_refresh_never_overwrites_last_good_inventory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cache/openrouter.json");
        let url = "https://openrouter.test/v1/models";
        let fingerprint = credential_fingerprint("key-one");
        let last_good = serde_json::json!({"data": [{"id": "last-good"}]});
        save_provider_inventory_cache(&path, "openrouter", url, &fingerprint, Some(&last_good))
            .unwrap();

        let recovered = fetch_and_cache_provider_inventory_with(
            &path,
            "openrouter",
            url.to_string(),
            http::HeaderMap::new(),
            &fingerprint,
            |_, _| anyhow::bail!("transient failure"),
        )
        .expect("a transient refresh failure should retain last-good metadata");
        assert_eq!(recovered, last_good);
        assert!(matches!(
            load_provider_inventory_cache(&path, "openrouter", url, &fingerprint).unwrap(),
            Some(CachedProviderInventory::Available(body)) if body == last_good
        ));
    }

    #[test]
    fn custom_model_cache_is_scoped_to_endpoint_and_reuses_discovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::custom::CredentialStore::new(
            directory.path().join("credentials/custom.json"),
        );
        let models = vec![crate::auth::custom::CustomModel {
            api_name: "local-model".into(),
            display_name: "Local Model".into(),
            context_window: 262_144,
            max_output_tokens: 16_384,
            tools: true,
            parallel_tool_calls: true,
            vision: false,
            structured_output: false,
            reasoning: true,
            reasoning_values: Vec::new(),
            reasoning_default: String::new(),
            reasoning_uses_system_message: true,
        }];
        let mut first_headers = http::HeaderMap::new();
        first_headers.insert("x-organization", "tenant-one".parse().unwrap());
        first_headers.insert("x-region", "north".parse().unwrap());
        let first_key = custom_credential_fingerprint("custom-key-one", &first_headers);
        save_custom_model_cache(&store, "http://one.test/v1/", &first_key, &models).unwrap();
        let Some(CachedCustomInventory::Available(loaded)) =
            load_custom_model_cache(&store, "http://one.test/v1/", &first_key).unwrap()
        else {
            panic!("expected positive custom inventory")
        };
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].api_name, "local-model");
        assert!(
            load_custom_model_cache(&store, "http://two.test/v1/", &first_key)
                .unwrap()
                .is_none(),
            "a cache from another endpoint must never populate this catalog"
        );
        assert!(
            load_custom_model_cache(
                &store,
                "http://one.test/v1/",
                &custom_credential_fingerprint("custom-key-two", &first_headers),
            )
            .unwrap()
            .is_none(),
            "a cache from another custom account must never populate this catalog"
        );
        let mut changed_headers = first_headers.clone();
        changed_headers.insert("x-organization", "tenant-two".parse().unwrap());
        assert!(
            load_custom_model_cache(
                &store,
                "http://one.test/v1/",
                &custom_credential_fingerprint("custom-key-one", &changed_headers),
            )
            .unwrap()
            .is_none(),
            "changing a tenant or authorization header must invalidate the inventory"
        );

        let mut reordered_headers = http::HeaderMap::new();
        reordered_headers.insert("x-region", "north".parse().unwrap());
        reordered_headers.insert("x-organization", "tenant-one".parse().unwrap());
        assert_eq!(
            first_key,
            custom_credential_fingerprint("custom-key-one", &reordered_headers),
            "header insertion order is not part of the credential scope"
        );

        save_custom_model_cache(&store, "http://one.test/v1/", &first_key, &[]).unwrap();
        assert!(
            matches!(
                load_custom_model_cache(&store, "http://one.test/v1/", &first_key).unwrap(),
                Some(CachedCustomInventory::Unavailable)
            ),
            "an empty inventory is a valid negative cache marker"
        );
    }

    #[test]
    fn version_four_custom_cache_is_invalid_after_hlid_tool_fallback_change() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::custom::CredentialStore::new(
            directory.path().join("credentials/custom.json"),
        );
        let base_url = "https://ai.watchyourtemper.com/v1/";
        let fingerprint = custom_credential_fingerprint("", &http::HeaderMap::new());
        let stale = CustomModelCache {
            version: 4,
            base_url: base_url.into(),
            credential_fingerprint: fingerprint.clone(),
            models: vec![crate::auth::custom::CustomModel {
                api_name: "qwen3.6-27b".into(),
                tools: false,
                ..Default::default()
            }],
        };
        store
            .save_model_cache(&serde_json::to_vec(&stale).unwrap())
            .unwrap();

        assert!(
            load_custom_model_cache(&store, base_url, &fingerprint)
                .unwrap()
                .is_none(),
            "v4 may contain tools=false from the pre-tri-state hlid path"
        );
    }

    #[test]
    fn stale_positive_custom_cache_refreshes_the_current_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::custom::CredentialStore::new(
            directory.path().join("credentials/custom.json"),
        );
        let cred = crate::auth::custom::CustomCredential {
            base_url: "http://custom.test/v1/".to_string(),
            api_key: "key".to_string(),
            api_name: String::new(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        };
        let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
        let previous = crate::auth::custom::CustomModel {
            api_name: "previous-model".to_string(),
            ..Default::default()
        };
        save_custom_model_cache(
            &store,
            &cred.base_url,
            &fingerprint,
            std::slice::from_ref(&previous),
        )
        .unwrap();

        let fresh = refresh_stale_custom_models_with(
            &store,
            &cred,
            &fingerprint,
            vec![previous],
            Duration::ZERO,
            |_| {
                vec![crate::auth::custom::CustomModel {
                    api_name: "currently-served-model".to_string(),
                    ..Default::default()
                }]
            },
        );

        assert_eq!(fresh[0].api_name, "currently-served-model");
        assert!(matches!(
            load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
            Some(CachedCustomInventory::Available(models))
                if models[0].api_name == "currently-served-model"
        ));
    }

    #[test]
    fn failed_stale_custom_refresh_retains_the_last_good_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::custom::CredentialStore::new(
            directory.path().join("credentials/custom.json"),
        );
        let cred = crate::auth::custom::CustomCredential {
            base_url: "http://custom.test/v1/".to_string(),
            api_key: "key".to_string(),
            api_name: String::new(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        };
        let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
        let previous = crate::auth::custom::CustomModel {
            api_name: "last-good-model".to_string(),
            ..Default::default()
        };
        save_custom_model_cache(
            &store,
            &cred.base_url,
            &fingerprint,
            std::slice::from_ref(&previous),
        )
        .unwrap();

        let retained = refresh_stale_custom_models_with(
            &store,
            &cred,
            &fingerprint,
            vec![previous],
            Duration::ZERO,
            |_| Vec::new(),
        );

        assert_eq!(retained[0].api_name, "last-good-model");
        assert!(matches!(
            load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
            Some(CachedCustomInventory::Available(models))
                if models[0].api_name == "last-good-model"
        ));
    }

    #[test]
    fn hlid_llama_cpp_metadata_reports_the_served_context_window() {
        let entry = serde_json::json!({
            "id": "ornith-35b-q4km",
            "meta": {
                "n_ctx": 131_072,
                "n_ctx_train": 262_144
            }
        });

        assert_eq!(extract_ctx_from_model_entry(&entry), 131_072);
    }

    #[test]
    fn sparse_custom_inventory_preserves_local_tools_but_honors_explicit_false() {
        // This is the live hlid shape: it advertises reasoning details but no
        // standardized tool capability field. A user-configured local OpenAI
        // endpoint keeps the historical tool-capable default.
        let sparse_hlid = serde_json::json!({
            "id": "qwen3.6-27b",
            "capabilities": {"reasoning": {
                "supported": true,
                "control": "binary",
                "values": ["none", "default"],
                "default": "default"
            }}
        });
        assert_eq!(model_metadata_tool_support(&sparse_hlid), None);
        assert!(custom_model_metadata_supports_tools(&sparse_hlid));
        assert!(!model_metadata_supports_tools(&sparse_hlid));

        let explicitly_disabled = serde_json::json!({
            "id": "text-only",
            "capabilities": {"tools": {"supported": false}}
        });
        assert_eq!(
            model_metadata_tool_support(&explicitly_disabled),
            Some(false)
        );
        assert!(!custom_model_metadata_supports_tools(&explicitly_disabled));

        let explicit_parameter_list = serde_json::json!({
            "id": "reasoning-only",
            "supported_parameters": ["reasoning_effort"]
        });
        assert_eq!(
            model_metadata_tool_support(&explicit_parameter_list),
            Some(false)
        );
        assert!(!custom_model_metadata_supports_tools(
            &explicit_parameter_list
        ));
    }

    #[test]
    fn hlid_reasoning_metadata_controls_custom_capabilities_exactly() {
        let off_only = serde_json::json!({
            "capabilities": {"reasoning": {
                "supported": true,
                "control": "binary",
                "values": ["none"],
                "default": "none"
            }}
        });
        let (reasoning, values, default) = discovered_custom_reasoning(&off_only);
        assert!(!reasoning);
        assert_eq!(values, ["none"]);
        assert_eq!(default, "none");
        let off_model = crate::auth::custom::CustomModel {
            reasoning,
            reasoning_values: values,
            reasoning_default: default,
            ..Default::default()
        };
        assert!(custom_reasoning_capability(&off_model).is_none());

        let binary = serde_json::json!({
            "capabilities": {"reasoning": {
                "supported": true,
                "control": "binary",
                "values": ["none", "default"],
                "default": "default"
            }}
        });
        let (reasoning, values, default) = discovered_custom_reasoning(&binary);
        let binary_model = crate::auth::custom::CustomModel {
            reasoning,
            reasoning_values: values,
            reasoning_default: default,
            reasoning_uses_system_message: true,
            ..Default::default()
        };
        let binary_capability = custom_reasoning_capability(&binary_model).unwrap();
        assert_eq!(binary_capability.control, ReasoningControl::Toggle);
        assert!(matches!(
            binary_capability.openai_chat_mode,
            OpenAiChatReasoningMode::ProviderValues {
                values,
                default: Some(default),
                system_message: true,
            } if values == ["none", "default"] && default == "default"
        ));

        let levels = serde_json::json!({
            "capabilities": {"reasoning": {
                "supported": true,
                "control": "levels",
                "values": ["none", "low", "medium", "high"],
                "default": "medium"
            }}
        });
        let (reasoning, values, default) = discovered_custom_reasoning(&levels);
        let level_model = crate::auth::custom::CustomModel {
            reasoning,
            reasoning_values: values,
            reasoning_default: default,
            ..Default::default()
        };
        let level_capability = custom_reasoning_capability(&level_model).unwrap();
        assert_eq!(level_capability.control, ReasoningControl::Effort);
        assert_eq!(level_capability.min_effort, ygg_ai::ReasoningEffort::Low);
        assert_eq!(level_capability.max_effort, ygg_ai::ReasoningEffort::High);
    }

    #[test]
    fn negative_custom_cache_recovers_without_another_restart() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::auth::custom::CredentialStore::new(
            directory.path().join("credentials/custom.json"),
        );
        let cred = crate::auth::custom::CustomCredential {
            base_url: "http://custom.test/v1/".to_string(),
            api_key: "key".to_string(),
            api_name: String::new(),
            headers: Vec::new(),
            models: Vec::new(),
            auto_discover: true,
        };
        let fingerprint = custom_credential_fingerprint(&cred.api_key, &http::HeaderMap::new());
        save_custom_model_cache(&store, &cred.base_url, &fingerprint, &[]).unwrap();
        assert!(matches!(
            load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
            Some(CachedCustomInventory::Unavailable)
        ));
        let recovered = crate::auth::custom::CustomModel {
            api_name: "recovered-local".to_string(),
            ..Default::default()
        };

        let models =
            discover_and_cache_custom_models_with(&store, &cred, &fingerprint, false, |_| {
                vec![recovered.clone()]
            });
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].api_name, "recovered-local");
        assert!(matches!(
            load_custom_model_cache(&store, &cred.base_url, &fingerprint).unwrap(),
            Some(CachedCustomInventory::Available(models))
                if models.len() == 1 && models[0].api_name == "recovered-local"
        ));
    }

    #[test]
    fn deepseek_v4_pro_is_registered_as_openai_chat_with_env_auth() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), Some(DEEPSEEK_MODEL_ID))).unwrap();
        let model = boot
            .catalog
            .resolve(&ModelId(DEEPSEEK_MODEL_ID.into()))
            .unwrap();
        assert_eq!(model.spec.protocol, Protocol::OpenAiChat);
        assert_eq!(model.endpoint.id.0, DEEPSEEK_ENDPOINT_ID);
        assert_eq!(
            model.spec.api_name,
            std::env::var("YGG_DEEPSEEK_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL_ID.into())
        );
        assert!(model.spec.capabilities.tools);
        assert!(matches!(
            model.spec.capabilities.reasoning.as_ref(),
            Some(ReasoningCapability {
                control: ReasoningControl::Effort,
                exposes_text: true,
                openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
                ..
            })
        ));
        assert_eq!(
            model.spec.limits.context_window,
            std::env::var("YGG_DEEPSEEK_CONTEXT_WINDOW")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEEPSEEK_DEFAULT_CONTEXT_WINDOW)
        );
        assert_eq!(
            model.spec.limits.max_output_tokens,
            std::env::var("YGG_DEEPSEEK_MAX_OUTPUT_TOKENS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS)
        );
    }

    #[test]
    fn deepseek_v4_pro_accepts_high_reasoning_at_startup() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(directory.path(), Some(DEEPSEEK_MODEL_ID));
        config.reasoning = ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High);
        let boot = bootstrap(config).unwrap();
        let launch = resolve_launch_print(&boot, "test-session").unwrap();
        let app = build_app(boot, launch, "system".into()).unwrap();
        assert_eq!(
            app.reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
        );
    }

    #[test]
    fn print_launch_errors_without_model() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), None)).unwrap();
        let error = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap_err();
        assert!(error.to_string().contains("no model configured"));
    }

    #[test]
    fn print_launch_creates_new_session_path_with_model() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), Some("gpt-4o-mini"))).unwrap();
        let launch = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap();
        assert_eq!(launch.model.0, "gpt-4o-mini");
        assert!(matches!(launch.session, SessionSelection::CreateNew(_)));
    }

    #[test]
    fn print_resume_restores_session_model_and_reasoning_unless_cli_overrides() {
        let directory = tempfile::tempdir().unwrap();
        let mut process_config = config(directory.path(), None);
        process_config.resume = ResumeSelector::Continue;
        process_config.model_explicit = false;
        process_config.reasoning_explicit = false;
        let boot = bootstrap(process_config).unwrap();
        let path = boot.sessions.new_path("2026-07-12T00-00-00Z");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Config {
                model: Some("gpt-5.4-mini-responses".to_string()),
                reasoning: Some("high".to_string()),
            })
            .unwrap();
        session
            .append(EntryValue::Message(ygg_ai::Message::User(
                ygg_ai::UserMessage {
                    content: vec![ygg_ai::UserPart::Text("resumable prompt".into())],
                },
            )))
            .unwrap();
        drop(session);

        let launch = resolve_launch_print(&boot, "unused").unwrap();
        assert_eq!(launch.model.0, "gpt-5.4-mini-responses");
        assert_eq!(
            launch.reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
        );

        let mut overridden = config(directory.path(), Some("gpt-4o-mini"));
        overridden.resume = ResumeSelector::Continue;
        overridden.model_explicit = true;
        overridden.reasoning = ReasoningConfig::Off;
        overridden.reasoning_explicit = true;
        let launch = resolve_launch_print(&bootstrap(overridden).unwrap(), "unused").unwrap();
        assert_eq!(launch.model.0, "gpt-4o-mini");
        assert_eq!(launch.reasoning, ReasoningConfig::Off);
    }

    #[test]
    fn launch_configuration_parts_returns_the_preopened_resume_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(directory.path(), None);
        config.model_explicit = false;
        config.reasoning_explicit = false;
        let path = directory.path().join("preopened.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Config {
                model: Some("gpt-5.4-mini-responses".to_owned()),
                reasoning: Some("high".to_owned()),
            })
            .unwrap();
        drop(session);

        let (prepared, model, reasoning) =
            launch_configuration_parts(&config, &SessionSelection::OpenExisting(path.clone()))
                .unwrap();

        assert_eq!(prepared.as_ref().map(Session::path), Some(path.as_path()));
        assert_eq!(model, Some(ModelId("gpt-5.4-mini-responses".into())));
        assert_eq!(
            reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
        );
    }

    #[test]
    fn disabled_tools_are_absent_from_both_schema_and_execution_registry() {
        let directory = tempfile::tempdir().unwrap();
        let skills: Arc<dyn SkillRegistry> = Arc::new(
            FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap(),
        );
        let mut config = config(directory.path(), Some("gpt-4o-mini"));
        config.sandbox.allow_edit = false;
        config.sandbox.allow_write = false;
        config.sandbox.allow_process = false;
        config.sandbox.allow_shell = false;
        let extensions = configured_test_extensions(skills, &config);
        let names = extensions
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read"]);
    }

    #[test]
    fn skill_requirements_use_the_filtered_core_and_extension_registry() {
        let directory = tempfile::tempdir().unwrap();
        let skill_root = directory.path().join(".ygg/skills/reviewer");
        std::fs::create_dir_all(&skill_root).unwrap();
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nid: reviewer\nname: Reviewer\ndescription: Review code\n---\nReview carefully.",
        )
        .unwrap();
        let skills: Arc<dyn SkillRegistry> = Arc::new(
            FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], true).unwrap(),
        );
        let mut config = config(directory.path(), Some("gpt-4o-mini"));
        config.tools =
            crate::config::ToolPolicy::only(["read".to_owned(), "load_skill".to_owned()]).unwrap();
        config.sandbox.allow_edit = false;
        let extensions = configured_test_extensions(skills, &config);
        let registered = extensions
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(registered, vec!["read", "load_skill"]);

        let available = ygg_agent::SkillDescriptor {
            id: "available".into(),
            name: "Available".into(),
            description: "test".into(),
            version: None,
            source: ygg_agent::SkillSource::BuiltIn,
            trust: ygg_agent::SkillTrust::BuiltIn,
            required_tools: vec!["read".into(), "load_skill".into()],
            tags: vec![],
        };
        assert!(crate::resources::validate_skill_requirements(&available, &registered).is_ok());

        let unavailable = ygg_agent::SkillDescriptor {
            required_tools: vec!["edit".into()],
            ..available
        };
        assert!(matches!(
            crate::resources::validate_skill_requirements(&unavailable, &registered),
            Err(ygg_agent::SkillLoadError::MissingRequiredTools(missing)) if missing == vec!["edit"]
        ));
    }

    #[test]
    fn initial_build_rejects_an_active_skill_missing_from_the_final_tool_registry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active-skill.jsonl");
        let mut session = Session::create(&path).unwrap();
        append_active_skill(&mut session, "editor", &["edit"]);
        drop(session);

        let mut config = config(directory.path(), Some("gpt-4o-mini"));
        config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
        let boot = bootstrap(config).unwrap();
        let error = match build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::OpenExisting(path),
                reasoning: ReasoningConfig::Off,
            },
            "system".into(),
        ) {
            Ok(_) => panic!("an incompatible active skill must fail before the app starts"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("active skill \"editor\""), "{message}");
        assert!(
            message.contains("Missing required tools: [\"edit\"]"),
            "{message}"
        );
        assert!(message.contains("available tools: read"), "{message}");
        assert!(message.contains("sandbox capabilities"), "{message}");
    }

    #[test]
    fn rebuild_revalidates_active_skills_after_the_tool_policy_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("active-skill-rebuild.jsonl");
        let mut session = Session::create(&path).unwrap();
        append_active_skill(&mut session, "editor", &["edit"]);
        drop(session);

        let config = config(directory.path(), Some("gpt-4o-mini"));
        let boot = bootstrap(config).unwrap();
        let mut app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::OpenExisting(path),
                reasoning: ReasoningConfig::Off,
            },
            "system".into(),
        )
        .unwrap();
        app.config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();

        let error = match rebuild_app(app, None, None, None) {
            Ok(_) => panic!("rebuild must revalidate persisted active skills"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("active skill \"editor\""), "{message}");
        assert!(
            message.contains("Missing required tools: [\"edit\"]"),
            "{message}"
        );
        assert!(message.contains("available tools: read"), "{message}");
    }

    #[test]
    fn explicit_unavailable_tools_report_final_available_names_and_policy_gates() {
        let directory = tempfile::tempdir().unwrap();
        let skills: Arc<dyn SkillRegistry> = Arc::new(
            FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap(),
        );
        let mut config = config(directory.path(), Some("gpt-4o-mini"));
        config.tools = crate::config::ToolPolicy::only([
            "read".to_owned(),
            "edit".to_owned(),
            "missing-extension".to_owned(),
        ])
        .unwrap();
        config.tools.exclude("edit").unwrap();
        config.sandbox.allow_edit = false;
        let extensions = configured_test_extensions(skills, &config);
        let boot = bootstrap(config.clone()).unwrap();
        let model = boot
            .catalog
            .resolve(config.model.as_ref().unwrap())
            .unwrap();

        let error = validate_explicit_tool_policy(&config, &extensions, &model).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("edit, missing-extension"), "{message}");
        assert!(
            message.contains("allowlists, sandbox gates, and extension registration"),
            "{message}"
        );
        assert!(message.contains("available tools: read"), "{message}");
    }

    #[test]
    fn model_without_tool_capability_gets_no_default_surface_and_rejects_explicit_tools() {
        let directory = tempfile::tempdir().unwrap();
        let mut default_config = config(directory.path(), Some("gpt-4o-mini"));
        let boot = bootstrap(default_config.clone()).unwrap();
        let resolved = boot
            .catalog
            .resolve(default_config.model.as_ref().unwrap())
            .unwrap();
        let mut spec = (*resolved.spec).clone();
        spec.capabilities.tools = false;
        spec.capabilities.parallel_tool_calls = false;
        let model = Model {
            spec: Arc::new(spec),
            endpoint: resolved.endpoint,
        };
        let session = Session::create(directory.path().join("no-tools-default.jsonl")).unwrap();
        let skills: Arc<dyn SkillRegistry> = Arc::new(
            FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], false).unwrap(),
        );
        let (extensions, _) = configured_extensions(
            skills.clone(),
            &default_config,
            &session,
            &model,
            &default_config.reasoning,
            &boot.sessions,
        );
        assert!(extensions.tool_definitions().is_empty());
        validate_explicit_tool_policy(&default_config, &extensions, &model).unwrap();

        default_config.tools = crate::config::ToolPolicy::only(["read".to_owned()]).unwrap();
        let explicit_session =
            Session::create(directory.path().join("no-tools-explicit.jsonl")).unwrap();
        let (extensions, _) = configured_extensions(
            skills,
            &default_config,
            &explicit_session,
            &model,
            &default_config.reasoning,
            &boot.sessions,
        );
        let error =
            validate_explicit_tool_policy(&default_config, &extensions, &model).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("gpt-4o-mini does not support tools"),
            "{message}"
        );
        assert!(
            message.contains("explicit tool policy requested: read"),
            "{message}"
        );
    }

    #[test]
    fn initial_build_records_configuration_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), Some("gpt-4o-mini"))).unwrap();
        let launch = resolve_launch_print(&boot, "initial-config").unwrap();
        let app = build_app(boot, launch, "system".to_string()).unwrap();
        assert_eq!(
            app.agent.completion_policy(),
            ygg_agent::CompletionPolicy::Natural,
            "ordinary coding turns must not pay for a hidden second inference"
        );
        assert!(matches!(
            app.agent.session().entries().first().map(|entry| &entry.value),
            Some(EntryValue::Config {
                model: Some(model),
                reasoning: Some(reasoning),
            }) if model == "gpt-4o-mini" && reasoning == "off"
        ));
    }

    #[test]
    fn tool_schema_reserve_is_positive_and_deterministic() {
        let directory = tempfile::tempdir().unwrap();
        let skills: Arc<dyn SkillRegistry> = Arc::new(
            FileSystemSkillRegistry::new(directory.path().to_owned(), vec![], true).unwrap(),
        );
        let config = config(directory.path(), Some("gpt-4o-mini"));
        let extensions = configured_test_extensions(skills, &config);
        let definitions = extensions.tool_definitions();
        let names = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read", "edit", "write", "exec"]);
        let default_reserve = tool_schema_reserve(&definitions);
        assert!(default_reserve > 0);
        assert_eq!(default_reserve, tool_schema_reserve(&definitions));

        let mut all_core = ExtensionHost::new();
        all_core.load(&CoreTools);
        let all_core_definitions = all_core.tool_definitions();
        assert_eq!(
            all_core_definitions
                .iter()
                .map(|definition| definition.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "edit", "write", "exec", "search"]
        );
        assert!(tool_schema_reserve(&all_core_definitions) > default_reserve);
    }

    fn fresh_app(directory: &std::path::Path) -> App {
        let boot = bootstrap(config(directory, Some("gpt-4o-mini"))).unwrap();
        let launch = resolve_launch_print(&boot, "test-session").unwrap();
        build_app(boot, launch, "system".into()).unwrap()
    }

    #[test]
    fn rebuild_same_session_preserves_history_without_redundant_config_write() {
        use ygg_ai::{Message, UserMessage, UserPart};

        let directory = tempfile::tempdir().unwrap();
        let mut app = fresh_app(directory.path());
        let entry = app
            .agent
            .session_mut()
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("keep me".into())],
            })))
            .unwrap();
        let path = app.agent.session().path().to_owned();
        let entries_before = app.agent.session().entries().len();
        let bytes_before = std::fs::metadata(&path).unwrap().len();
        let app = rebuild_app(app, None, None, None).unwrap();
        assert!(app.agent.session().entry(&entry).is_some());
        assert_eq!(app.agent.session().entries().len(), entries_before);
        assert_eq!(std::fs::metadata(path).unwrap().len(), bytes_before);
    }

    #[test]
    fn rebuild_restores_the_target_sessions_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let app = fresh_app(directory.path());
        let target = directory.path().join("target.jsonl");
        let mut session = Session::create(&target).unwrap();
        session
            .append(EntryValue::Config {
                model: Some("gpt-5.4-mini-responses".to_string()),
                reasoning: Some("medium".to_string()),
            })
            .unwrap();
        drop(session);

        let app = rebuild_app(
            app,
            None,
            None,
            Some(SessionSelection::OpenExisting(target)),
        )
        .unwrap();
        assert_eq!(app.model.spec.id.0, "gpt-5.4-mini-responses");
        assert_eq!(
            app.reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Medium)
        );
    }

    #[test]
    fn rebuild_new_session_has_empty_context_and_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let app = fresh_app(directory.path());
        let new_path = directory.path().join("new.jsonl");
        let app =
            rebuild_app(app, None, None, Some(SessionSelection::CreateNew(new_path))).unwrap();
        assert!(app.agent.session().context().unwrap().is_empty());
        assert_eq!(app.agent.session().entries().len(), 1);
        assert!(matches!(
            app.agent.session().entries()[0].value,
            EntryValue::Config { .. }
        ));
    }
}
