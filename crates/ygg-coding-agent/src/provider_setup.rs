#![allow(missing_docs)]

//! Frontend-neutral, transactional setup for explicitly selected custom
//! OpenAI-compatible providers.
//!
//! This module deliberately owns no terminal state and constructs no Agent. A
//! frontend gathers a draft, asks this service to probe only that draft's
//! endpoint, presents the receipt, and commits only after an explicit final
//! confirmation. The existing custom registry remains the sole credential
//! store and `ModelCatalog` remains the sole runtime catalog.

use std::fmt;
use std::io::Read;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use ygg_ai::{ModelCatalog, ModelId};

use crate::auth::custom::{
    CredentialStore, CustomAuthConfig, CustomCredential, CustomModel, CustomProvider,
    CustomRegistry, RegistryCommitError, RegistrySnapshot,
};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERY_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_DISCOVERED_MODELS: usize = 512;
const MAX_PROVIDER_ID_BYTES: usize = 64;
const MAX_LABEL_BYTES: usize = 128;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_ENDPOINT_BYTES: usize = 512;

/// A credential source used only while setting up one explicitly selected
/// provider. Its secret-bearing variant never implements a revealing `Debug`.
pub(crate) enum SetupAuthentication {
    None,
    Environment { variable: String },
    ApiKey(SecretInput),
}

impl SetupAuthentication {
    pub(crate) fn no_authentication() -> Self {
        Self::None
    }

    pub(crate) fn environment(variable: impl Into<String>) -> Self {
        Self::Environment {
            variable: variable.into(),
        }
    }

    pub(crate) fn api_key(value: String) -> Self {
        Self::ApiKey(SecretInput::new(value))
    }

    fn validate(&self) -> Result<(), ProviderSetupError> {
        match self {
            Self::Environment { variable } if valid_environment_name(variable) => Ok(()),
            Self::Environment { .. } => Err(ProviderSetupError::InvalidConfiguration),
            Self::ApiKey(value) if value.is_empty() => {
                Err(ProviderSetupError::InvalidConfiguration)
            }
            Self::None | Self::ApiKey(_) => Ok(()),
        }
    }

    fn authorization_value(&self) -> Result<Option<http::HeaderValue>, ProbeFailure> {
        let value = match self {
            Self::None => return Ok(None),
            Self::Environment { variable } => ygg_ai::auth::read_bounded_env(variable)
                .ok()
                .flatten()
                .filter(|value| !value.trim().is_empty())
                .ok_or(ProbeFailure::AuthenticationFailed)?,
            Self::ApiKey(value) => value
                .as_str()
                .ok_or(ProbeFailure::AuthenticationFailed)?
                .to_owned(),
        };
        let mut header = http::HeaderValue::from_str(&format!("Bearer {value}"))
            .map_err(|_| ProbeFailure::AuthenticationFailed)?;
        header.set_sensitive(true);
        Ok(Some(header))
    }

    fn receipt_description(&self) -> String {
        match self {
            Self::None => "no credential is sent or stored".to_owned(),
            Self::Environment { variable } => format!(
                "Bearer credential is read from {variable} at runtime; its value is not stored"
            ),
            Self::ApiKey(_) => {
                "Bearer credential is stored only in the owner-private custom registry".to_owned()
            }
        }
    }

    fn into_storage(self) -> (String, Option<CustomAuthConfig>) {
        match self {
            Self::None => (String::new(), Some(CustomAuthConfig::None)),
            Self::Environment { variable } => (
                String::new(),
                Some(CustomAuthConfig::BearerEnv { var: variable }),
            ),
            Self::ApiKey(value) => (value.into_string(), None),
        }
    }
}

impl fmt::Debug for SetupAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("SetupAuthentication::None"),
            Self::Environment { variable } => formatter
                .debug_struct("SetupAuthentication::Environment")
                .field("variable", variable)
                .finish(),
            Self::ApiKey(_) => formatter.write_str("SetupAuthentication::ApiKey([REDACTED])"),
        }
    }
}

/// Bounded secret material that is zeroed when the setup draft is dropped.
pub(crate) struct SecretInput(Vec<u8>);

impl SecretInput {
    fn new(value: String) -> Self {
        Self(value.into_bytes())
    }

    fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn into_string(mut self) -> String {
        let bytes = std::mem::take(&mut self.0);
        String::from_utf8(bytes).unwrap_or_default()
    }
}

impl fmt::Debug for SecretInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretInput {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// User-provided setup values before any network traffic or persistence.
pub(crate) struct SetupDraft {
    provider_id: String,
    label: String,
    endpoint: String,
    authentication: SetupAuthentication,
    replace_existing: bool,
}

impl SetupDraft {
    pub(crate) fn new(
        provider_id: impl Into<String>,
        label: impl Into<String>,
        endpoint: impl Into<String>,
        authentication: SetupAuthentication,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            label: label.into(),
            endpoint: endpoint.into(),
            authentication,
            replace_existing: false,
        }
    }

    pub(crate) fn lm_studio() -> Self {
        Self::new(
            "local",
            "LM Studio",
            "http://localhost:1234/v1/",
            SetupAuthentication::None,
        )
    }

    pub(crate) fn replace_existing(mut self, replace_existing: bool) -> Self {
        self.replace_existing = replace_existing;
        self
    }
}

impl fmt::Debug for SetupDraft {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetupDraft")
            .field("provider_id", &self.provider_id)
            .field("label", &self.label)
            .field("endpoint", &self.endpoint)
            .field("authentication", &self.authentication)
            .field("replace_existing", &self.replace_existing)
            .finish()
    }
}

struct ValidatedDraft {
    provider_id: String,
    label: String,
    endpoint: url::Url,
    authentication: SetupAuthentication,
    replace_existing: bool,
}

impl ValidatedDraft {
    fn endpoint_string(&self) -> String {
        self.endpoint.to_string()
    }
}

/// A model ID returned by explicit discovery, safe to display and select.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetupModel {
    pub(crate) api_name: String,
    pub(crate) display_name: String,
}

/// A no-write setup transaction. Dropping or cancelling it cannot affect disk.
pub(crate) struct SetupTransaction {
    snapshot: RegistrySnapshot,
    registry_path: PathBuf,
    draft: ValidatedDraft,
    discovered: Vec<CustomModel>,
}

impl SetupTransaction {
    pub(crate) fn cancel(self) -> ProviderSetupState {
        ProviderSetupState::Cancelled
    }
}

impl fmt::Debug for SetupTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetupTransaction")
            .field("provider_id", &self.draft.provider_id)
            .field("endpoint", &self.draft.endpoint_string())
            .field("discovered_models", &self.discovered.len())
            .finish()
    }
}

/// A prepared, reviewed setup transaction. It has not written any state yet.
pub(crate) struct SetupPrepared {
    snapshot: RegistrySnapshot,
    draft: ValidatedDraft,
    models: Vec<CustomModel>,
    selected: CustomModel,
    receipt: SetupReceipt,
}

impl SetupPrepared {
    pub(crate) fn receipt(&self) -> &SetupReceipt {
        &self.receipt
    }

    pub(crate) fn cancel(self) -> ProviderSetupState {
        ProviderSetupState::Cancelled
    }
}

impl fmt::Debug for SetupPrepared {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SetupPrepared")
            .field("provider_id", &self.draft.provider_id)
            .field("endpoint", &self.draft.endpoint_string())
            .field("models", &self.models.len())
            .field("selected", &self.selected.api_name)
            .field("receipt", &self.receipt)
            .finish()
    }
}

/// Stable, secret-free result of a successful setup operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetupReceipt {
    provider_id: String,
    provider_label: String,
    endpoint: String,
    endpoint_kind: EndpointKind,
    selected_model: String,
    credential_policy: String,
    registry_path: PathBuf,
}

impl SetupReceipt {
    pub(crate) fn selected_model_id(&self) -> ModelId {
        ModelId(format!(
            "custom/{}/{}",
            self.provider_id, self.selected_model
        ))
    }

    pub(crate) fn render(&self, authority: Option<&SetupAuthority>) -> String {
        let locality = match self.endpoint_kind {
            EndpointKind::Local => "local endpoint",
            EndpointKind::Network => "network endpoint",
        };
        let mut lines = vec![
            "Provider setup ready".to_owned(),
            format!("provider: {} ({})", self.provider_label, self.provider_id),
            format!("model: {}", self.selected_model_id().0),
            format!("endpoint: {} ({locality})", self.endpoint),
            "traffic: direct only from this Ygg process to the selected endpoint; no endpoint scanning or setup telemetry".to_owned(),
            format!("credentials: {}", self.credential_policy),
            format!(
                "registry storage: owner-private atomic file at {}",
                self.registry_path.display()
            ),
        ];
        if let Some(authority) = authority {
            lines.extend(authority.render_lines());
        }
        lines.join("\n")
    }
}

/// The caller-owned authority facts shown alongside an otherwise
/// frontend-neutral receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetupAuthority {
    session_dir: String,
    workspace_trusted: bool,
    enabled_tools: String,
}

impl SetupAuthority {
    pub(crate) fn from_config(config: &crate::config::Config) -> Self {
        let tools = ["read", "edit", "write", "bash"]
            .into_iter()
            .filter(|name| config.tool_available(name))
            .collect::<Vec<_>>();
        Self {
            session_dir: bounded_copy(
                &config.session_dir.display().to_string(),
                MAX_ENDPOINT_BYTES,
            ),
            workspace_trusted: config.workspace_trusted,
            enabled_tools: if tools.is_empty() {
                "none of read, edit, write, or bash".to_owned()
            } else {
                tools.join(", ")
            },
        }
    }

    fn render_lines(&self) -> Vec<String> {
        vec![
            format!("session storage: local files at {}", self.session_dir),
            format!(
                "workspace trust: {}",
                if self.workspace_trusted {
                    "trusted"
                } else {
                    "not trusted"
                }
            ),
            format!("tool authority: {}", self.enabled_tools),
            "OS isolation: none; enabled tools retain their normal host authority".to_owned(),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointKind {
    Local,
    Network,
}

/// Recoverable readiness and setup states. They contain no endpoint response,
/// header, or credential value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProviderSetupState {
    NoProvider,
    EndpointUnreachable,
    AuthenticationFailed,
    EmptyDiscovery,
    ManualModelRequired,
    UnavailableResumedModel { model: String },
    Offline,
    ConcurrentRegistry,
    Cancelled,
    ProviderAlreadyConfigured,
    Ready,
}

impl fmt::Display for ProviderSetupState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoProvider => formatter.write_str(
                "no provider is configured; run `ygg setup --yes` or start interactive Ygg",
            ),
            Self::EndpointUnreachable => formatter.write_str(
                "the selected endpoint could not be reached; retry, edit it, or enter a model manually",
            ),
            Self::AuthenticationFailed => formatter.write_str(
                "the selected endpoint rejected authentication; change the credential source and retry",
            ),
            Self::EmptyDiscovery => formatter.write_str(
                "model discovery returned no usable models; enter an explicit model ID",
            ),
            Self::ManualModelRequired => formatter.write_str(
                "the requested model was not discovered; enter it with --manual-model",
            ),
            Self::UnavailableResumedModel { model } => {
                write!(formatter, "the resumed model {model:?} is unavailable; select a replacement or cancel resume")
            }
            Self::Offline => formatter.write_str(
                "setup discovery is disabled by offline mode; provide --manual-model or retry online",
            ),
            Self::ConcurrentRegistry => formatter.write_str(
                "the custom provider registry changed during setup; reload and merge deliberately",
            ),
            Self::Cancelled => formatter.write_str("setup cancelled; no provider state was written"),
            Self::ProviderAlreadyConfigured => formatter.write_str(
                "that provider ID is already configured; choose another ID or explicitly replace it",
            ),
            Self::Ready => formatter.write_str("provider setup is ready"),
        }
    }
}

/// Safe failure boundary for a setup operation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProviderSetupError {
    #[error("{0}")]
    State(ProviderSetupState),
    #[error("provider setup configuration is invalid")]
    InvalidConfiguration,
    #[error("could not read or save the custom provider registry")]
    Storage,
    #[error("the rebuilt model catalog did not expose the selected model")]
    CatalogUnavailable,
}

impl ProviderSetupError {
    pub(crate) fn state(&self) -> Option<&ProviderSetupState> {
        match self {
            Self::State(state) => Some(state),
            Self::InvalidConfiguration | Self::Storage | Self::CatalogUnavailable => None,
        }
    }
}

/// An intentionally narrow network-probe contract. Test fixtures can replace
/// it without exposing credentials or constructing an Agent.
pub(crate) trait SetupProbe {
    fn discover(
        &self,
        endpoint: &url::Url,
        authentication: &SetupAuthentication,
    ) -> Result<Vec<CustomModel>, ProbeFailure>;
}

/// Secret-free classification of an explicit endpoint probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProbeFailure {
    Unreachable,
    AuthenticationFailed,
    InvalidDiscovery,
}

/// The production probe for one selected OpenAI-compatible `/models` URL.
#[derive(Clone, Debug)]
pub(crate) struct HttpSetupProbe {
    timeout: Duration,
}

impl Default for HttpSetupProbe {
    fn default() -> Self {
        Self {
            timeout: DISCOVERY_TIMEOUT,
        }
    }
}

impl SetupProbe for HttpSetupProbe {
    fn discover(
        &self,
        endpoint: &url::Url,
        authentication: &SetupAuthentication,
    ) -> Result<Vec<CustomModel>, ProbeFailure> {
        let models_url = endpoint
            .join("models")
            .map_err(|_| ProbeFailure::Unreachable)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ProbeFailure::Unreachable)?;
        let mut request = client.get(models_url);
        if let Some(authorization) = authentication.authorization_value()? {
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::AUTHORIZATION, authorization);
            request = request.headers(headers);
        }
        let response = request.send().map_err(|_| ProbeFailure::Unreachable)?;
        let status = response.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(ProbeFailure::AuthenticationFailed);
        }
        if !status.is_success() {
            return Err(ProbeFailure::Unreachable);
        }
        let body = bounded_json(response).map_err(|_| ProbeFailure::InvalidDiscovery)?;
        models_from_json(&body)
    }
}

/// Transactional coordinator shared by CLI and TUI adapters.
pub(crate) struct ProviderSetupService<P = HttpSetupProbe> {
    store: CredentialStore,
    offline: bool,
    probe: P,
}

impl ProviderSetupService<HttpSetupProbe> {
    pub(crate) fn new(store: CredentialStore, offline: bool) -> Self {
        Self {
            store,
            offline,
            probe: HttpSetupProbe::default(),
        }
    }
}

impl<P> ProviderSetupService<P>
where
    P: SetupProbe,
{
    #[cfg(test)]
    fn with_probe(store: CredentialStore, offline: bool, probe: P) -> Self {
        Self {
            store,
            offline,
            probe,
        }
    }

    /// Classify startup without launching an onboarding surface.
    pub(crate) fn readiness(
        catalog: &ModelCatalog,
        requested_model: Option<&ModelId>,
    ) -> ProviderSetupState {
        if let Some(model) = requested_model {
            if catalog.resolve(model).is_err() {
                return ProviderSetupState::UnavailableResumedModel {
                    model: bounded_copy(&model.0, MAX_MODEL_ID_BYTES),
                };
            }
        }
        if catalog.models().next().is_none() {
            ProviderSetupState::NoProvider
        } else {
            ProviderSetupState::Ready
        }
    }

    /// Capture a private registry snapshot and validate a draft. This does not
    /// probe, write, cache, or otherwise mutate persistent state.
    pub(crate) fn begin(&self, draft: SetupDraft) -> Result<SetupTransaction, ProviderSetupError> {
        let draft = validate_draft(draft)?;
        let snapshot = self
            .store
            .load_registry_snapshot()
            .map_err(|_| ProviderSetupError::Storage)?;
        // Fail before discovery when an existing provider would need explicit
        // replacement. The commit path repeats this check against the captured
        // snapshot, so a provider added after this point remains protected by
        // the CAS rather than being silently overwritten.
        if !draft.replace_existing
            && snapshot
                .registry()
                .is_some_and(|registry| registry.providers.contains_key(&draft.provider_id))
        {
            return Err(ProviderSetupError::State(
                ProviderSetupState::ProviderAlreadyConfigured,
            ));
        }
        Ok(SetupTransaction {
            snapshot,
            registry_path: self.store.path().to_owned(),
            draft,
            discovered: Vec::new(),
        })
    }

    /// Probe only the transaction's explicitly selected endpoint. No cache or
    /// registry is written, and offline mode returns before credential lookup or
    /// network construction.
    pub(crate) fn discover(
        &self,
        transaction: &mut SetupTransaction,
    ) -> Result<Vec<SetupModel>, ProviderSetupError> {
        if self.offline {
            return Err(ProviderSetupError::State(ProviderSetupState::Offline));
        }
        let models = self
            .probe
            .discover(
                &transaction.draft.endpoint,
                &transaction.draft.authentication,
            )
            .map_err(probe_error)?;
        if models.is_empty() {
            transaction.discovered.clear();
            return Err(ProviderSetupError::State(
                ProviderSetupState::EmptyDiscovery,
            ));
        }
        transaction.discovered = models;
        Ok(transaction
            .discovered
            .iter()
            .map(|model| SetupModel {
                api_name: model.api_name.clone(),
                display_name: if model.display_name.trim().is_empty() {
                    model.api_name.clone()
                } else {
                    model.display_name.clone()
                },
            })
            .collect())
    }

    /// Select one previously discovered model without any new network traffic.
    pub(crate) fn prepare_discovered(
        &self,
        transaction: SetupTransaction,
        model_id: &str,
    ) -> Result<SetupPrepared, ProviderSetupError> {
        let model_id = validate_model_id(model_id)?;
        let selected = transaction
            .discovered
            .iter()
            .find(|model| model.api_name == model_id)
            .cloned()
            .ok_or(ProviderSetupError::State(
                ProviderSetupState::ManualModelRequired,
            ))?;
        let models = transaction.discovered.clone();
        Ok(prepared_from(transaction, models, selected))
    }

    /// Prepare an explicit manual model inventory. This path intentionally does
    /// not probe, so it is also the safe recovery path for offline or unsupported
    /// discovery.
    pub(crate) fn prepare_manual(
        &self,
        transaction: SetupTransaction,
        model_id: impl AsRef<str>,
    ) -> Result<SetupPrepared, ProviderSetupError> {
        let api_name = validate_model_id(model_id.as_ref())?;
        let model = CustomModel {
            api_name: api_name.clone(),
            display_name: api_name,
            ..CustomModel::default()
        };
        Ok(prepared_from(transaction, vec![model.clone()], model))
    }

    /// Persist a prepared transaction with owner-private compare-and-swap
    /// semantics, then rebuild and verify the same canonical model catalog.
    /// No Agent is constructed at this boundary.
    pub(crate) fn commit_and_rebuild(
        &self,
        prepared: SetupPrepared,
    ) -> Result<CompletedSetup, ProviderSetupError> {
        let selected_model = prepared.receipt.selected_model_id();
        let receipt = prepared.receipt.clone();
        let registry = registry_for_prepared(prepared)?;
        self.store
            .save_registry_if_unchanged(&registry.0, &registry.1)
            .map_err(registry_error)?;
        let catalog =
            crate::app::bootstrap::model_catalog_with_setup_store(&self.store, self.offline)
                .map_err(|_| ProviderSetupError::CatalogUnavailable)?;
        if catalog.resolve(&selected_model).is_err() {
            return Err(ProviderSetupError::CatalogUnavailable);
        }
        Ok(CompletedSetup {
            catalog,
            model: selected_model,
            receipt,
        })
    }
}

impl<P> Clone for ProviderSetupService<P>
where
    P: Clone,
{
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            offline: self.offline,
            probe: self.probe.clone(),
        }
    }
}

/// The canonical catalog and selected model returned after a confirmed setup.
pub(crate) struct CompletedSetup {
    pub(crate) catalog: ModelCatalog,
    pub(crate) model: ModelId,
    pub(crate) receipt: SetupReceipt,
}

/// Thin deterministic CLI adapter over the same transaction used by guided
/// onboarding. It never reads stdin, opens a picker, or emits a secret value.
pub(crate) fn run_cli(
    command: &crate::cli::SetupCommand,
    config: &crate::config::Config,
) -> anyhow::Result<()> {
    if command.cancel {
        crate::output::stdout_line(ProviderSetupState::Cancelled);
        return Ok(());
    }

    let (preset, endpoint) = cli_preset_and_endpoint(command)?;
    let provider_id = command.provider.clone().unwrap_or_else(|| match preset {
        crate::cli::SetupPreset::LmStudio => "local".to_owned(),
        crate::cli::SetupPreset::OpenAiCompatible => "custom".to_owned(),
    });
    let label = command.label.clone().unwrap_or_else(|| match preset {
        crate::cli::SetupPreset::LmStudio => "LM Studio".to_owned(),
        crate::cli::SetupPreset::OpenAiCompatible => "OpenAI-compatible endpoint".to_owned(),
    });
    let authentication = match &command.api_key_env {
        Some(variable) => SetupAuthentication::environment(variable.clone()),
        None => SetupAuthentication::no_authentication(),
    };
    let service = ProviderSetupService::new(
        CredentialStore::new(crate::auth::custom::default_path()),
        command.offline || config.offline,
    );
    let transaction = service.begin(
        SetupDraft::new(provider_id, label, endpoint, authentication)
            .replace_existing(command.replace),
    )?;
    let prepared = if let Some(model) = command.manual_model.as_deref() {
        service.prepare_manual(transaction, model)?
    } else {
        let mut transaction = transaction;
        let discovered = service.discover(&mut transaction)?;
        let selected = command
            .model
            .as_deref()
            .or_else(|| discovered.first().map(|model| model.api_name.as_str()))
            .ok_or(ProviderSetupError::State(
                ProviderSetupState::EmptyDiscovery,
            ))?;
        service.prepare_discovered(transaction, selected)?
    };
    let authority = SetupAuthority::from_config(config);
    let review = prepared.receipt().render(Some(&authority));
    if !command.yes {
        let _ = prepared.cancel();
        crate::output::stdout_multiline(format!(
            "{review}\nreview: not saved; pass --yes to confirm\nstatus: {}",
            ProviderSetupState::Cancelled
        ));
        return Ok(());
    }

    let completed = service.commit_and_rebuild(prepared)?;
    // This is the same user-level selection persistence performed by the model
    // picker. It happens only after the registry CAS and catalog verification.
    crate::cli::persist_model(&completed.model.0).map_err(|_| {
        anyhow::anyhow!(
            "provider setup committed, but the selected model preference could not be saved; rerun with --model {}",
            completed.model.0
        )
    })?;
    crate::output::stdout_multiline(completed.receipt.render(Some(&authority)));
    Ok(())
}

/// Resolve the command's endpoint before any credential lookup, registry read,
/// or network traffic. An LM Studio probe is therefore possible only after its
/// preset was explicitly selected.
fn cli_preset_and_endpoint(
    command: &crate::cli::SetupCommand,
) -> anyhow::Result<(crate::cli::SetupPreset, String)> {
    let preset = match (command.preset, command.endpoint.as_ref()) {
        // An entered URL is always a generic compatible endpoint. In
        // particular, it cannot inherit the LM Studio profile merely because
        // that profile was also named on the command line.
        (_, Some(_)) => crate::cli::SetupPreset::OpenAiCompatible,
        (Some(preset), None) => preset,
        (None, None) => anyhow::bail!(
            "select an explicit endpoint with --endpoint <URL> or --preset lm-studio; setup never probes an implicit endpoint"
        ),
    };
    let endpoint = match (preset, command.endpoint.as_deref()) {
        (_, Some(endpoint)) => endpoint.to_owned(),
        (crate::cli::SetupPreset::LmStudio, None) => "http://localhost:1234/v1/".to_owned(),
        (crate::cli::SetupPreset::OpenAiCompatible, None) => {
            anyhow::bail!("--endpoint is required with --preset openai-compatible")
        }
    };
    Ok((preset, endpoint))
}

fn validate_draft(draft: SetupDraft) -> Result<ValidatedDraft, ProviderSetupError> {
    let provider_id = draft.provider_id.trim();
    let label = draft.label.trim();
    if !valid_provider_id(provider_id)
        || !valid_display_text(label, MAX_LABEL_BYTES)
        || draft.endpoint.len() > MAX_ENDPOINT_BYTES
    {
        return Err(ProviderSetupError::InvalidConfiguration);
    }
    draft.authentication.validate()?;
    let mut endpoint = url::Url::parse(draft.endpoint.trim())
        .map_err(|_| ProviderSetupError::InvalidConfiguration)?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderSetupError::InvalidConfiguration);
    }
    if !endpoint.path().ends_with('/') {
        let path = format!("{}/", endpoint.path());
        endpoint.set_path(&path);
    }
    if endpoint.to_string().len() > MAX_ENDPOINT_BYTES {
        return Err(ProviderSetupError::InvalidConfiguration);
    }
    Ok(ValidatedDraft {
        provider_id: provider_id.to_owned(),
        label: label.to_owned(),
        endpoint,
        authentication: draft.authentication,
        replace_existing: draft.replace_existing,
    })
}

fn prepared_from(
    transaction: SetupTransaction,
    mut models: Vec<CustomModel>,
    selected: CustomModel,
) -> SetupPrepared {
    models.sort_by(|left, right| left.api_name.cmp(&right.api_name));
    models.dedup_by(|left, right| left.api_name == right.api_name);
    let receipt = SetupReceipt {
        provider_id: transaction.draft.provider_id.clone(),
        provider_label: transaction.draft.label.clone(),
        endpoint: transaction.draft.endpoint_string(),
        endpoint_kind: endpoint_kind(&transaction.draft.endpoint),
        selected_model: selected.api_name.clone(),
        credential_policy: transaction.draft.authentication.receipt_description(),
        registry_path: transaction.registry_path.clone(),
    };
    SetupPrepared {
        snapshot: transaction.snapshot,
        draft: transaction.draft,
        models,
        selected,
        receipt,
    }
}

fn registry_for_prepared(
    prepared: SetupPrepared,
) -> Result<(RegistrySnapshot, CustomRegistry), ProviderSetupError> {
    let SetupPrepared {
        snapshot,
        draft,
        models,
        selected,
        ..
    } = prepared;
    let ValidatedDraft {
        provider_id,
        label,
        endpoint,
        authentication,
        replace_existing,
    } = draft;
    let base_url = endpoint.to_string();
    let (api_key, auth) = authentication.into_storage();
    let provider = CustomProvider {
        label,
        credential: CustomCredential {
            base_url,
            api_key,
            api_name: selected.api_name,
            headers: Vec::new(),
            models,
            // The reviewed inventory is explicit. Startup therefore rebuilds
            // immediately and offline without a second surprise probe.
            auto_discover: false,
        },
        auth,
        api_key_env: None,
        cache: None,
        startup_timeout_secs: None,
    };
    let mut registry = snapshot.registry().cloned().unwrap_or(CustomRegistry {
        version: 1,
        providers: Default::default(),
        legacy_single_endpoint: false,
    });
    if registry.providers.contains_key(&provider_id) && !replace_existing {
        return Err(ProviderSetupError::State(
            ProviderSetupState::ProviderAlreadyConfigured,
        ));
    }
    registry.legacy_single_endpoint = false;
    registry.providers.insert(provider_id, provider);
    Ok((snapshot, registry))
}

fn registry_error(error: RegistryCommitError) -> ProviderSetupError {
    match error {
        RegistryCommitError::Changed => {
            ProviderSetupError::State(ProviderSetupState::ConcurrentRegistry)
        }
        RegistryCommitError::Storage => ProviderSetupError::Storage,
    }
}

fn probe_error(error: ProbeFailure) -> ProviderSetupError {
    ProviderSetupError::State(match error {
        ProbeFailure::Unreachable | ProbeFailure::InvalidDiscovery => {
            ProviderSetupState::EndpointUnreachable
        }
        ProbeFailure::AuthenticationFailed => ProviderSetupState::AuthenticationFailed,
    })
}

fn bounded_json(response: reqwest::blocking::Response) -> Result<serde_json::Value, ()> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_DISCOVERY_BODY_BYTES as u64)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_DISCOVERY_BODY_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_DISCOVERY_BODY_BYTES {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn models_from_json(value: &serde_json::Value) -> Result<Vec<CustomModel>, ProbeFailure> {
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())
        .ok_or(ProbeFailure::InvalidDiscovery)?;
    let mut models = Vec::new();
    for entry in entries.iter().take(MAX_DISCOVERED_MODELS) {
        let Some(id) = entry
            .get("id")
            .or_else(|| entry.get("slug"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| valid_model_id(id))
        else {
            continue;
        };
        let mut model = CustomModel {
            api_name: id.to_owned(),
            display_name: id.to_owned(),
            ..CustomModel::default()
        };
        if let Some(context_window) = positive_u64(
            entry,
            &[
                "context_window",
                "context_length",
                "max_model_len",
                "max_context_tokens",
            ],
        ) {
            model.context_window = context_window;
        }
        if let Some(max_output_tokens) =
            positive_u64(entry, &["max_output_tokens", "max_completion_tokens"])
        {
            model.max_output_tokens = max_output_tokens.min(model.context_window);
        }
        models.push(model);
    }
    models.sort_by(|left, right| left.api_name.cmp(&right.api_name));
    models.dedup_by(|left, right| left.api_name == right.api_name);
    Ok(models)
}

fn positive_u64(entry: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| entry.get(*name).and_then(serde_json::Value::as_u64))
        .filter(|value| *value > 0)
}

fn endpoint_kind(endpoint: &url::Url) -> EndpointKind {
    let local = endpoint.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.ends_with(".local")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if local {
        EndpointKind::Local
    } else {
        EndpointKind::Network
    }
}

fn valid_provider_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_PROVIDER_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_environment_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(byte) if byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_display_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn valid_model_id(value: &str) -> bool {
    valid_display_text(value, MAX_MODEL_ID_BYTES)
}

fn validate_model_id(value: &str) -> Result<String, ProviderSetupError> {
    let value = value.trim();
    if valid_model_id(value) {
        Ok(value.to_owned())
    } else {
        Err(ProviderSetupError::InvalidConfiguration)
    }
}

fn bounded_copy(value: &str, maximum: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if output.len().saturating_add(character.len_utf8()) > maximum {
            break;
        }
        output.push(character);
    }
    output.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct FixtureProbe {
        result: Arc<Mutex<Result<Vec<CustomModel>, ProbeFailure>>>,
        requests: Arc<Mutex<usize>>,
    }

    impl FixtureProbe {
        fn models(names: &[&str]) -> Self {
            Self {
                result: Arc::new(Mutex::new(Ok(names
                    .iter()
                    .map(|name| CustomModel {
                        api_name: (*name).to_owned(),
                        display_name: (*name).to_owned(),
                        ..CustomModel::default()
                    })
                    .collect()))),
                requests: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl SetupProbe for FixtureProbe {
        fn discover(
            &self,
            _endpoint: &url::Url,
            _authentication: &SetupAuthentication,
        ) -> Result<Vec<CustomModel>, ProbeFailure> {
            *self.requests.lock().unwrap() += 1;
            self.result.lock().unwrap().clone()
        }
    }

    fn service(probe: FixtureProbe) -> (tempfile::TempDir, ProviderSetupService<FixtureProbe>) {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path().join("credentials/custom.json"));
        let service = ProviderSetupService::with_probe(store, false, probe);
        (directory, service)
    }

    fn draft() -> SetupDraft {
        SetupDraft::new(
            "local",
            "Fixture Local",
            "http://127.0.0.1:1234/v1/",
            SetupAuthentication::no_authentication(),
        )
    }

    fn cli_command() -> crate::cli::SetupCommand {
        crate::cli::SetupCommand {
            preset: None,
            endpoint: None,
            provider: None,
            label: None,
            model: None,
            manual_model: None,
            api_key_env: None,
            no_auth: false,
            replace: false,
            yes: false,
            cancel: false,
            offline: false,
        }
    }

    #[test]
    fn cli_requires_an_explicit_endpoint_selection_before_any_probe() {
        let command = cli_command();
        assert!(cli_preset_and_endpoint(&command)
            .unwrap_err()
            .to_string()
            .contains("never probes an implicit endpoint"));

        let mut lm_studio = cli_command();
        lm_studio.preset = Some(crate::cli::SetupPreset::LmStudio);
        assert_eq!(
            cli_preset_and_endpoint(&lm_studio).unwrap(),
            (
                crate::cli::SetupPreset::LmStudio,
                "http://localhost:1234/v1/".to_owned()
            )
        );

        let mut endpoint = cli_command();
        endpoint.endpoint = Some("https://models.example.test/v1/".to_owned());
        assert_eq!(
            cli_preset_and_endpoint(&endpoint).unwrap(),
            (
                crate::cli::SetupPreset::OpenAiCompatible,
                "https://models.example.test/v1/".to_owned()
            )
        );

        endpoint.preset = Some(crate::cli::SetupPreset::LmStudio);
        assert_eq!(
            cli_preset_and_endpoint(&endpoint).unwrap(),
            (
                crate::cli::SetupPreset::OpenAiCompatible,
                "https://models.example.test/v1/".to_owned()
            )
        );
    }

    #[test]
    fn discovered_provider_is_prepared_then_committed_without_secret_receipt() {
        let (_directory, service) = service(FixtureProbe::models(&["alpha", "beta"]));
        let mut transaction = service.begin(draft()).unwrap();
        let models = service.discover(&mut transaction).unwrap();
        assert_eq!(
            models
                .iter()
                .map(|model| model.api_name.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let prepared = service.prepare_discovered(transaction, "beta").unwrap();
        let preview = prepared.receipt().render(None);
        assert!(preview.contains("direct only"));
        assert!(!preview.contains("secret-value"));
        let completed = service.commit_and_rebuild(prepared).unwrap();
        assert_eq!(completed.model.0, "custom/local/beta");
        assert!(completed.catalog.resolve(&completed.model).is_ok());
        assert!(completed.receipt.render(None).contains("Fixture Local"));
    }

    #[test]
    fn cancellation_and_offline_leave_the_registry_absent_without_a_probe() {
        let probe = FixtureProbe::models(&["alpha"]);
        let requests = Arc::clone(&probe.requests);
        let (directory, service) = service(probe);
        let transaction = service.begin(draft()).unwrap();
        assert_eq!(transaction.cancel(), ProviderSetupState::Cancelled);
        assert!(!directory.path().join("credentials/custom.json").exists());
        assert_eq!(*requests.lock().unwrap(), 0);

        let offline = ProviderSetupService::with_probe(
            CredentialStore::new(directory.path().join("offline/custom.json")),
            true,
            FixtureProbe::models(&["alpha"]),
        );
        let mut transaction = offline.begin(draft()).unwrap();
        assert!(matches!(
            offline.discover(&mut transaction),
            Err(ProviderSetupError::State(ProviderSetupState::Offline))
        ));
        assert!(!directory.path().join("offline/custom.json").exists());
    }

    #[test]
    fn manual_model_recovers_without_discovery_and_rebuilds_offline() {
        let directory = tempfile::tempdir().unwrap();
        let store = CredentialStore::new(directory.path().join("credentials/custom.json"));
        let probe = FixtureProbe::models(&["ignored"]);
        let service = ProviderSetupService::with_probe(store, true, probe.clone());
        let transaction = service.begin(draft()).unwrap();
        let prepared = service.prepare_manual(transaction, "manual-model").unwrap();
        let completed = service.commit_and_rebuild(prepared).unwrap();
        assert_eq!(completed.model.0, "custom/local/manual-model");
        assert_eq!(*probe.requests.lock().unwrap(), 0);
    }

    #[test]
    fn existing_provider_requires_explicit_replacement_before_discovery() {
        let probe = FixtureProbe::models(&["alpha"]);
        let requests = Arc::clone(&probe.requests);
        let (_directory, service) = service(probe);
        let transaction = service.begin(draft()).unwrap();
        let prepared = service.prepare_manual(transaction, "manual-model").unwrap();
        service.commit_and_rebuild(prepared).unwrap();

        assert!(matches!(
            service.begin(draft()),
            Err(ProviderSetupError::State(
                ProviderSetupState::ProviderAlreadyConfigured
            ))
        ));
        assert_eq!(*requests.lock().unwrap(), 0);

        assert!(service.begin(draft().replace_existing(true)).is_ok());
    }

    #[test]
    fn concurrent_registry_change_is_not_overwritten() {
        let (directory, service) = service(FixtureProbe::models(&["alpha"]));
        let mut transaction = service.begin(draft()).unwrap();
        service.discover(&mut transaction).unwrap();
        let prepared = service.prepare_discovered(transaction, "alpha").unwrap();

        let store = CredentialStore::new(directory.path().join("credentials/custom.json"));
        store
            .save_registry(&CustomRegistry::single(
                "other",
                CustomProvider {
                    label: "Other".to_owned(),
                    credential: CustomCredential {
                        base_url: "http://127.0.0.1:9999/v1/".to_owned(),
                        api_key: String::new(),
                        api_name: "other".to_owned(),
                        headers: Vec::new(),
                        models: vec![CustomModel {
                            api_name: "other".to_owned(),
                            display_name: "other".to_owned(),
                            ..CustomModel::default()
                        }],
                        auto_discover: false,
                    },
                    auth: Some(CustomAuthConfig::None),
                    api_key_env: None,
                    cache: None,
                    startup_timeout_secs: None,
                },
            ))
            .unwrap();

        assert!(matches!(
            service.commit_and_rebuild(prepared),
            Err(ProviderSetupError::State(
                ProviderSetupState::ConcurrentRegistry
            ))
        ));
        let registry = store.load_registry().unwrap().unwrap();
        assert!(registry.providers.contains_key("other"));
        assert!(!registry.providers.contains_key("local"));
    }

    #[test]
    fn receipt_and_errors_do_not_format_api_keys() {
        let secret = "secret-value-must-never-appear";
        let probe = FixtureProbe::models(&["alpha"]);
        let (_directory, service) = service(probe);
        let mut transaction = service
            .begin(SetupDraft::new(
                "local",
                "Fixture Local",
                "http://127.0.0.1:1234/v1/",
                SetupAuthentication::api_key(secret.to_owned()),
            ))
            .unwrap();
        service.discover(&mut transaction).unwrap();
        let prepared = service.prepare_discovered(transaction, "alpha").unwrap();
        assert!(!format!("{prepared:?}").contains(secret));
        assert!(!prepared.receipt().render(None).contains(secret));
    }

    #[test]
    fn readiness_covers_no_provider_unavailable_resume_and_ready() {
        let catalog = ModelCatalog::default();
        assert_eq!(
            ProviderSetupService::<HttpSetupProbe>::readiness(&catalog, None),
            ProviderSetupState::NoProvider
        );
        assert!(matches!(
            ProviderSetupService::<HttpSetupProbe>::readiness(
                &catalog,
                Some(&ModelId("gone".to_owned())),
            ),
            ProviderSetupState::UnavailableResumedModel { .. }
        ));
    }

    #[test]
    fn endpoint_and_model_validation_reject_credential_bearing_or_control_values() {
        let probe = FixtureProbe::models(&["alpha"]);
        let (_directory, service) = service(probe);
        assert!(matches!(
            service.begin(SetupDraft::new(
                "local",
                "Local",
                "https://user:password@example.test/v1/",
                SetupAuthentication::no_authentication(),
            )),
            Err(ProviderSetupError::InvalidConfiguration)
        ));
        assert!(matches!(
            service.begin(SetupDraft::new(
                "local",
                "Local",
                "http://127.0.0.1:1234/v1/?token=secret",
                SetupAuthentication::no_authentication(),
            )),
            Err(ProviderSetupError::InvalidConfiguration)
        ));
        assert!(validate_model_id("bad\u{1b}model").is_err());
    }
}
