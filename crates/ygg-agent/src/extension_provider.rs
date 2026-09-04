//! Host-owned lifecycle registry for executable extension provider catalogs.
//!
//! The registry deliberately stores declarations and authorization *state*, not
//! endpoint URLs, headers, credentials, OAuth callbacks, or leases. A process
//! generation owns each entry; teardown and replacement remove that ownership
//! atomically so a stale extension cannot keep a model callable.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::extension_api_v03 as api_v03;

/// Identity of the extension generation which owns a provider catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtensionProviderOwner {
    /// Host-assigned instance identity, stable across a supervised replacement.
    pub extension_instance_id: String,
    /// Monotonic process generation within the extension instance.
    pub generation: u64,
}

/// Non-secret availability state retained for an extension provider.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtensionProviderAuthorizationStatus {
    /// The host has approved the required authorization boundary.
    Ready,
    /// The provider is parked pending an explicit host-policy/UI action.
    #[default]
    Pending,
    /// The host policy refused the request.
    Denied,
    /// No compatible host policy or credential service is currently available.
    Unavailable,
    /// An earlier authorization was revoked and must not be retried implicitly.
    Revoked,
}

impl ExtensionProviderAuthorizationStatus {
    fn from_wire(status: &str) -> Option<Self> {
        match status {
            "ready" => Some(Self::Ready),
            "pending" => Some(Self::Pending),
            "denied" => Some(Self::Denied),
            "unavailable" => Some(Self::Unavailable),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }

    /// Returns the stable API 0.3 wire name.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Pending => "pending",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
            Self::Revoked => "revoked",
        }
    }
}

/// Snapshot of one live, secret-free extension provider declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionProviderCatalogEntry {
    /// Generation that owns the declaration.
    pub owner: ExtensionProviderOwner,
    /// Secret-free provider metadata.
    pub provider: api_v03::ProviderDefinition,
    /// Current complete model catalog for the provider.
    pub models: Vec<api_v03::ProviderModelDefinition>,
    /// Host-owned authorization availability, with no lease retained.
    pub authorization: ExtensionProviderAuthorizationStatus,
}

/// Route selected for a currently callable extension-provider model.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionProviderRoute {
    /// Generation that owns the provider stream endpoint.
    pub owner: ExtensionProviderOwner,
    /// Secret-free provider declaration.
    pub provider: api_v03::ProviderDefinition,
    /// Selected model declaration.
    pub model: api_v03::ProviderModelDefinition,
}

/// Host policy for explicit provider OAuth or credential authorization requests.
///
/// Implementations own browser/UI activity and credential storage. They receive
/// no credential value from the extension and must return only a status plus an
/// optional opaque, request-scoped lease. The registry never stores that lease.
pub trait ExtensionProviderAuthorizationPolicy: Send + Sync {
    /// Resolves one explicit authorization request for an owned provider.
    fn authorize(
        &self,
        owner: &ExtensionProviderOwner,
        provider: &api_v03::ProviderDefinition,
        request: &api_v03::ProviderAuthorizationRequest,
    ) -> api_v03::ProviderAuthorizationResult;
}

/// Errors from a lifecycle-owned provider registry operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ExtensionProviderRegistryError {
    /// The extension sent malformed secret-free provider metadata.
    #[error("invalid extension provider declaration: {0}")]
    Invalid(String),
    /// A catalog mutation came from a stale or foreign generation.
    #[error("extension provider declaration is stale or foreign")]
    StaleOwner,
    /// A globally unique provider identifier belongs to another extension.
    #[error("extension provider identifier is already owned by another extension")]
    ProviderConflict,
    /// A configured registry bound was exceeded.
    #[error("extension provider registry limit exceeded: {0}")]
    ResourceExhausted(&'static str),
}

#[derive(Clone)]
struct ProviderRecord {
    owner: ExtensionProviderOwner,
    provider: api_v03::ProviderDefinition,
    models: BTreeMap<String, api_v03::ProviderModelDefinition>,
    authorization: ExtensionProviderAuthorizationStatus,
}

#[derive(Default)]
struct RegistryState {
    revision: usize,
    providers: BTreeMap<String, ProviderRecord>,
}

/// Shared registry for all executable extension provider declarations.
///
/// A product creates one registry and supplies its clone to each
/// [`crate::ExtensionRuntimeConfig`]. Catalog updates are all-or-nothing and
/// only the current owner generation can mutate or remove an entry.
#[derive(Clone)]
pub struct ExtensionProviderRegistry {
    state: Arc<Mutex<RegistryState>>,
    authorization_policy: Option<Arc<dyn ExtensionProviderAuthorizationPolicy>>,
}

impl std::fmt::Debug for ExtensionProviderRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = lock_registry(&self.state);
        formatter
            .debug_struct("ExtensionProviderRegistry")
            .field("providers", &state.providers.len())
            .field("revision", &state.revision)
            .field(
                "authorization_policy_configured",
                &self.authorization_policy.is_some(),
            )
            .finish()
    }
}

impl Default for ExtensionProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionProviderRegistry {
    /// Creates a registry with no authorization policy. Authenticated providers
    /// remain parked until a product installs a policy or explicitly updates
    /// their status; they never fall back to extension-owned credentials.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
            authorization_policy: None,
        }
    }

    /// Creates a registry using a host-owned authorization policy.
    pub fn with_authorization_policy(
        authorization_policy: Arc<dyn ExtensionProviderAuthorizationPolicy>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
            authorization_policy: Some(authorization_policy),
        }
    }

    /// Atomically registers a previously absent provider declaration.
    pub fn register(
        &self,
        owner: ExtensionProviderOwner,
        request: api_v03::ProviderRegisterParams,
    ) -> Result<api_v03::ProviderCatalogResult, ExtensionProviderRegistryError> {
        self.replace(owner, request.provider, request.models, false)
    }

    /// Atomically replaces the complete model set for an owned provider.
    pub fn update(
        &self,
        owner: ExtensionProviderOwner,
        request: api_v03::ProviderUpdateParams,
    ) -> Result<api_v03::ProviderCatalogResult, ExtensionProviderRegistryError> {
        self.replace(owner, request.provider, request.models, true)
    }

    /// Removes a provider only when it belongs to the calling generation.
    pub fn unregister(
        &self,
        owner: &ExtensionProviderOwner,
        provider_id: &str,
    ) -> Result<api_v03::ProviderCatalogResult, ExtensionProviderRegistryError> {
        validate_identifier(provider_id, "provider_id")?;
        let mut state = lock_registry(&self.state);
        let Some(record) = state.providers.get(provider_id) else {
            return Err(ExtensionProviderRegistryError::StaleOwner);
        };
        if &record.owner != owner {
            return Err(ExtensionProviderRegistryError::StaleOwner);
        }
        state.providers.remove(provider_id);
        state.revision = state.revision.saturating_add(1);
        Ok(catalog_result(state.revision, Vec::new(), Vec::new()))
    }

    /// Removes every declaration still owned by one exact process generation.
    ///
    /// This is idempotent and intentionally ignores a newer replacement with
    /// the same instance identity.
    pub fn remove_owner(&self, owner: &ExtensionProviderOwner) {
        let mut state = lock_registry(&self.state);
        let removed = state
            .providers
            .iter()
            .filter_map(|(id, record)| (&record.owner == owner).then_some(id.clone()))
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            for id in removed {
                state.providers.remove(&id);
            }
            state.revision = state.revision.saturating_add(1);
        }
    }

    /// Returns the current non-secret registry revision and declarations.
    pub fn snapshot(&self) -> (usize, Vec<ExtensionProviderCatalogEntry>) {
        let state = lock_registry(&self.state);
        let entries = state
            .providers
            .values()
            .map(|record| ExtensionProviderCatalogEntry {
                owner: record.owner.clone(),
                provider: record.provider.clone(),
                models: record.models.values().cloned().collect(),
                authorization: record.authorization,
            })
            .collect();
        (state.revision, entries)
    }

    /// Resolves a callable route by provider and local model identifier.
    pub fn resolve(&self, provider_id: &str, model_id: &str) -> Option<ExtensionProviderRoute> {
        let state = lock_registry(&self.state);
        let record = state.providers.get(provider_id)?;
        (record.authorization == ExtensionProviderAuthorizationStatus::Ready).then(|| {
            record
                .models
                .get(model_id)
                .map(|model| ExtensionProviderRoute {
                    owner: record.owner.clone(),
                    provider: record.provider.clone(),
                    model: model.clone(),
                })
        })?
    }

    /// Returns whether this exact route is still current and callable.
    ///
    /// Stream transports use this immediately before accepting a request and
    /// while consuming events. A catalog replacement, owner cutover, or host
    /// authorization revocation therefore cannot leave a previously resolved
    /// route callable.
    pub fn route_is_active(&self, route: &ExtensionProviderRoute) -> bool {
        let state = lock_registry(&self.state);
        let Some(record) = state.providers.get(&route.provider.id) else {
            return false;
        };
        record.owner == route.owner
            && record.authorization == ExtensionProviderAuthorizationStatus::Ready
            && record.provider == route.provider
            && record.models.get(&route.model.id) == Some(&route.model)
    }

    /// Changes non-secret availability after a host UI/configuration refresh.
    ///
    /// The caller cannot install a lease here; leases remain request-scoped and
    /// are only returned from [`Self::request_authorization`].
    pub fn set_authorization_status(
        &self,
        owner: &ExtensionProviderOwner,
        provider_id: &str,
        status: ExtensionProviderAuthorizationStatus,
    ) -> Result<(), ExtensionProviderRegistryError> {
        let mut state = lock_registry(&self.state);
        let record = state
            .providers
            .get_mut(provider_id)
            .ok_or(ExtensionProviderRegistryError::StaleOwner)?;
        if &record.owner != owner {
            return Err(ExtensionProviderRegistryError::StaleOwner);
        }
        if record.authorization != status {
            record.authorization = status;
            state.revision = state.revision.saturating_add(1);
        }
        Ok(())
    }

    /// Applies one explicit host-policy authorization request.
    ///
    /// No response value is persisted except its status. A policy that is not
    /// configured returns `unavailable`; callers should park rather than retry
    /// it until an explicit configuration/UI refresh occurs.
    pub fn request_authorization(
        &self,
        owner: &ExtensionProviderOwner,
        request: api_v03::ProviderAuthorizationRequest,
    ) -> Result<api_v03::ProviderAuthorizationResult, ExtensionProviderRegistryError> {
        validate_authorization_request(&request)?;
        let provider = {
            let state = lock_registry(&self.state);
            let record = state
                .providers
                .get(&request.provider_id)
                .ok_or(ExtensionProviderRegistryError::StaleOwner)?;
            if &record.owner != owner {
                return Err(ExtensionProviderRegistryError::StaleOwner);
            }
            record.provider.clone()
        };
        validate_requested_scopes(&provider.auth, request.scopes.as_deref())?;
        let result = if request.action == "revoke" {
            match &self.authorization_policy {
                Some(policy) => policy.authorize(owner, &provider, &request),
                // Revocation is fail-closed even when no credential service is
                // currently configured: a caller's explicit request must never
                // leave an earlier Ready route callable.
                None => api_v03::ProviderAuthorizationResult {
                    status: "revoked".to_owned(),
                    lease: None,
                },
            }
        } else {
            match (&provider.auth.kind[..], &self.authorization_policy) {
                ("none", _) => api_v03::ProviderAuthorizationResult {
                    status: "ready".to_owned(),
                    lease: None,
                },
                (_, Some(policy)) => policy.authorize(owner, &provider, &request),
                (_, None) => api_v03::ProviderAuthorizationResult {
                    status: "unavailable".to_owned(),
                    lease: None,
                },
            }
        };
        validate_authorization_result(&result)?;
        if request.action == "revoke" && result.status == "ready" {
            return Err(ExtensionProviderRegistryError::Invalid(
                "a provider revocation cannot leave the route ready".into(),
            ));
        }
        let status = ExtensionProviderAuthorizationStatus::from_wire(&result.status)
            .expect("validated authorization status");
        self.set_authorization_status(owner, &request.provider_id, status)?;
        Ok(result)
    }

    fn replace(
        &self,
        owner: ExtensionProviderOwner,
        provider: api_v03::ProviderDefinition,
        models: Vec<api_v03::ProviderModelDefinition>,
        update_only: bool,
    ) -> Result<api_v03::ProviderCatalogResult, ExtensionProviderRegistryError> {
        validate_provider(&provider, &models)?;
        let provider_id = provider.id.clone();
        let model_ids = models
            .iter()
            .map(|model| format!("{provider_id}/{}", model.id))
            .collect::<Vec<_>>();
        let model_map = models
            .into_iter()
            .map(|model| (model.id.clone(), model))
            .collect::<BTreeMap<_, _>>();
        let authorization = authorization_for(&provider.auth);
        let mut state = lock_registry(&self.state);
        match state.providers.get(&provider_id) {
            Some(existing)
                if existing.owner.extension_instance_id != owner.extension_instance_id =>
            {
                return Err(ExtensionProviderRegistryError::ProviderConflict);
            }
            Some(existing) if existing.owner.generation > owner.generation => {
                return Err(ExtensionProviderRegistryError::StaleOwner);
            }
            Some(existing) if !update_only && existing.owner == owner => {
                return Err(ExtensionProviderRegistryError::ProviderConflict);
            }
            None if update_only => return Err(ExtensionProviderRegistryError::StaleOwner),
            _ => {}
        }
        let replacing_models = state
            .providers
            .get(&provider_id)
            .map_or(0, |record| record.models.len());
        let next_providers = state
            .providers
            .contains_key(&provider_id)
            .then_some(state.providers.len())
            .unwrap_or_else(|| state.providers.len().saturating_add(1));
        if next_providers > api_v03::MAX_PROVIDERS {
            return Err(ExtensionProviderRegistryError::ResourceExhausted(
                "providers",
            ));
        }
        let model_count = state
            .providers
            .values()
            .map(|record| record.models.len())
            .sum::<usize>()
            .saturating_sub(replacing_models)
            .saturating_add(model_map.len());
        if model_count > api_v03::MAX_PROVIDER_MODELS {
            return Err(ExtensionProviderRegistryError::ResourceExhausted(
                "provider models",
            ));
        }
        state.providers.insert(
            provider_id.clone(),
            ProviderRecord {
                owner,
                provider,
                models: model_map,
                authorization,
            },
        );
        state.revision = state.revision.saturating_add(1);
        Ok(catalog_result(state.revision, vec![provider_id], model_ids))
    }
}

fn lock_registry(state: &Arc<Mutex<RegistryState>>) -> std::sync::MutexGuard<'_, RegistryState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn catalog_result(
    revision: usize,
    provider_ids: Vec<String>,
    model_ids: Vec<String>,
) -> api_v03::ProviderCatalogResult {
    api_v03::ProviderCatalogResult {
        revision,
        provider_ids,
        model_ids,
    }
}

fn authorization_for(
    auth: &api_v03::ProviderAuthRequirement,
) -> ExtensionProviderAuthorizationStatus {
    if auth.kind == "none" {
        ExtensionProviderAuthorizationStatus::Ready
    } else {
        ExtensionProviderAuthorizationStatus::Pending
    }
}

fn validate_provider(
    provider: &api_v03::ProviderDefinition,
    models: &[api_v03::ProviderModelDefinition],
) -> Result<(), ExtensionProviderRegistryError> {
    validate_identifier(&provider.id, "provider id")?;
    validate_label(&provider.label, "provider label")?;
    validate_auth_requirement(&provider.auth)?;
    if models.len() > api_v03::MAX_PROVIDER_MODELS {
        return Err(ExtensionProviderRegistryError::ResourceExhausted(
            "provider models",
        ));
    }
    let mut ids = BTreeMap::new();
    for model in models {
        validate_identifier(&model.id, "model id")?;
        if ids.insert(&model.id, ()).is_some() {
            return Err(ExtensionProviderRegistryError::Invalid(
                "provider model identifiers must be unique".into(),
            ));
        }
        if model.api_name.trim().is_empty()
            || model.api_name.len() > api_v03::MAX_PROVIDER_ID_BYTES
            || model.api_name.chars().any(char::is_control)
        {
            return Err(ExtensionProviderRegistryError::Invalid(
                "model api_name is invalid".into(),
            ));
        }
        if let Some(label) = &model.display_name {
            validate_label(label, "model display_name")?;
        }
        if model.context_window == 0 || model.max_output_tokens == 0 {
            return Err(ExtensionProviderRegistryError::Invalid(
                "model token limits must be greater than zero".into(),
            ));
        }
        if model.max_output_tokens > model.context_window {
            return Err(ExtensionProviderRegistryError::Invalid(
                "model max_output_tokens exceeds context_window".into(),
            ));
        }
        if model.capabilities.parallel_tool_calls && !model.capabilities.tools {
            return Err(ExtensionProviderRegistryError::Invalid(
                "parallel_tool_calls requires tools".into(),
            ));
        }
    }
    Ok(())
}

fn validate_auth_requirement(
    auth: &api_v03::ProviderAuthRequirement,
) -> Result<(), ExtensionProviderRegistryError> {
    match auth.kind.as_str() {
        "none" => {
            if auth.subject.is_some() || auth.scopes.is_some() {
                return Err(ExtensionProviderRegistryError::Invalid(
                    "unauthenticated providers cannot declare authorization metadata".into(),
                ));
            }
        }
        "oauth" => {
            let subject = auth.subject.as_deref().ok_or_else(|| {
                ExtensionProviderRegistryError::Invalid("OAuth providers require subject".into())
            })?;
            validate_identifier(subject, "OAuth subject")?;
            validate_scopes(auth.scopes.as_deref())?;
        }
        "host_credential" => {
            let subject = auth.subject.as_deref().ok_or_else(|| {
                ExtensionProviderRegistryError::Invalid(
                    "host credential providers require subject".into(),
                )
            })?;
            validate_identifier(subject, "credential subject")?;
            if auth.scopes.is_some() {
                return Err(ExtensionProviderRegistryError::Invalid(
                    "host credential providers cannot declare OAuth scopes".into(),
                ));
            }
        }
        _ => {
            return Err(ExtensionProviderRegistryError::Invalid(
                "unknown provider authentication kind".into(),
            ));
        }
    }
    Ok(())
}

fn validate_authorization_request(
    request: &api_v03::ProviderAuthorizationRequest,
) -> Result<(), ExtensionProviderRegistryError> {
    validate_identifier(&request.provider_id, "provider_id")?;
    if !matches!(request.action.as_str(), "authorize" | "refresh" | "revoke") {
        return Err(ExtensionProviderRegistryError::Invalid(
            "unknown authorization action".into(),
        ));
    }
    validate_scopes(request.scopes.as_deref())
}

fn validate_requested_scopes(
    auth: &api_v03::ProviderAuthRequirement,
    requested: Option<&[String]>,
) -> Result<(), ExtensionProviderRegistryError> {
    let Some(requested) = requested else {
        return Ok(());
    };
    match auth.kind.as_str() {
        "oauth" => {
            let declared = auth.scopes.as_deref().ok_or_else(|| {
                ExtensionProviderRegistryError::Invalid(
                    "OAuth provider did not declare scopes for this authorization request".into(),
                )
            })?;
            if requested.iter().any(|scope| !declared.contains(scope)) {
                return Err(ExtensionProviderRegistryError::Invalid(
                    "authorization request exceeds the provider's declared OAuth scopes".into(),
                ));
            }
        }
        "none" | "host_credential" => {
            return Err(ExtensionProviderRegistryError::Invalid(
                "only OAuth providers may request authorization scopes".into(),
            ));
        }
        _ => {
            return Err(ExtensionProviderRegistryError::Invalid(
                "unknown provider authentication kind".into(),
            ));
        }
    }
    Ok(())
}

fn validate_authorization_result(
    result: &api_v03::ProviderAuthorizationResult,
) -> Result<(), ExtensionProviderRegistryError> {
    if ExtensionProviderAuthorizationStatus::from_wire(&result.status).is_none() {
        return Err(ExtensionProviderRegistryError::Invalid(
            "unknown authorization status".into(),
        ));
    }
    if let Some(lease) = &result.lease {
        if !is_opaque(lease, api_v03::MAX_PROVIDER_AUTH_LEASE_BYTES) {
            return Err(ExtensionProviderRegistryError::Invalid(
                "authorization lease is not an opaque bounded identifier".into(),
            ));
        }
    }
    if result.status != "ready" && result.lease.is_some() {
        return Err(ExtensionProviderRegistryError::Invalid(
            "only ready authorization responses may carry a lease".into(),
        ));
    }
    Ok(())
}

fn validate_scopes(scopes: Option<&[String]>) -> Result<(), ExtensionProviderRegistryError> {
    let Some(scopes) = scopes else {
        return Ok(());
    };
    if scopes.len() > api_v03::MAX_PROVIDER_AUTH_SCOPES {
        return Err(ExtensionProviderRegistryError::ResourceExhausted(
            "authorization scopes",
        ));
    }
    let mut seen = BTreeMap::new();
    for scope in scopes {
        if !is_opaque(scope, api_v03::MAX_PROVIDER_ID_BYTES)
            || scope.chars().any(char::is_whitespace)
            || seen.insert(scope, ()).is_some()
        {
            return Err(ExtensionProviderRegistryError::Invalid(
                "authorization scopes must be unique bounded opaque names".into(),
            ));
        }
    }
    Ok(())
}

fn validate_identifier(value: &str, label: &str) -> Result<(), ExtensionProviderRegistryError> {
    if value.len() > api_v03::MAX_PROVIDER_ID_BYTES
        || value.is_empty()
        || !value.is_ascii()
        || !value.as_bytes()[0].is_ascii_lowercase()
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(ExtensionProviderRegistryError::Invalid(format!(
            "{label} must be a lowercase ASCII identifier"
        )));
    }
    Ok(())
}

fn validate_label(value: &str, label: &str) -> Result<(), ExtensionProviderRegistryError> {
    if value.trim().is_empty()
        || value.len() > api_v03::MAX_PROVIDER_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ExtensionProviderRegistryError::Invalid(format!(
            "{label} is invalid"
        )));
    }
    Ok(())
}

fn is_opaque(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(generation: u64) -> ExtensionProviderOwner {
        ExtensionProviderOwner {
            extension_instance_id: "extension-1".into(),
            generation,
        }
    }

    fn provider() -> api_v03::ProviderDefinition {
        api_v03::ProviderDefinition {
            id: "fixture".into(),
            label: "Fixture".into(),
            auth: api_v03::ProviderAuthRequirement {
                kind: "none".into(),
                subject: None,
                scopes: None,
            },
        }
    }

    fn model() -> api_v03::ProviderModelDefinition {
        api_v03::ProviderModelDefinition {
            id: "model".into(),
            api_name: "fixture-model".into(),
            protocol: "openai_chat".into(),
            context_window: 32_768,
            max_output_tokens: 4_096,
            capabilities: api_v03::ProviderModelCapabilities {
                tools: true,
                parallel_tool_calls: false,
                structured_output: true,
                reasoning: false,
            },
            display_name: None,
        }
    }

    #[test]
    fn replacement_is_generation_owned_and_stale_cleanup_is_safe() {
        let registry = ExtensionProviderRegistry::new();
        registry
            .register(
                owner(1),
                api_v03::ProviderRegisterParams {
                    provider: provider(),
                    models: vec![model()],
                },
            )
            .expect("initial registration");
        registry
            .update(
                owner(2),
                api_v03::ProviderUpdateParams {
                    provider: provider(),
                    models: vec![model()],
                },
            )
            .expect("newer generation replaces old");
        registry.remove_owner(&owner(1));
        assert_eq!(
            registry.resolve("fixture", "model").unwrap().owner,
            owner(2)
        );
        assert!(matches!(
            registry.update(
                owner(1),
                api_v03::ProviderUpdateParams {
                    provider: provider(),
                    models: vec![model()],
                },
            ),
            Err(ExtensionProviderRegistryError::StaleOwner)
        ));
    }

    #[test]
    fn unauthenticated_provider_is_callable_but_secret_fields_are_rejected() {
        let registry = ExtensionProviderRegistry::new();
        registry
            .register(
                owner(1),
                api_v03::ProviderRegisterParams {
                    provider: provider(),
                    models: vec![model()],
                },
            )
            .expect("registration");
        assert!(registry.resolve("fixture", "model").is_some());
        let mut authenticated = provider();
        authenticated.auth.kind = "host_credential".into();
        authenticated.auth.subject = Some("fixture".into());
        assert!(matches!(
            registry.update(
                owner(1),
                api_v03::ProviderUpdateParams {
                    provider: authenticated,
                    models: vec![model()],
                },
            ),
            Ok(_)
        ));
        assert!(registry.resolve("fixture", "model").is_none());
    }
}
