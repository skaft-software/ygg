//! Owner-layered provider catalog publication and request resolution.
//!
//! The registry keeps configuration transactions separate from request-time
//! credentials and transport callbacks. Every accepted mutation builds and
//! validates a complete candidate [`ModelCatalog`] before publishing one new
//! immutable snapshot.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::catalog::{CatalogConfig, EndpointConfig, ModelConfig};
use crate::{ConfigError, EndpointId, Model, ModelCatalog, ModelId};

/// Reserved owner identity for the immutable built-in provider layer.
pub const BUILTIN_PROVIDER_OWNER_ID: &str = "builtin";

/// Maximum UTF-8 byte length of an owner identity.
///
/// Owner identities are opaque host-assigned principals rather than display
/// names. Control characters and empty identities are rejected separately.
pub const MAX_PROVIDER_OWNER_ID_BYTES: usize = 512;

/// Maximum number of endpoint and model entries in one owner's catalog.
///
/// The limit applies both to a submitted transaction and to the prospective
/// owner catalog after an upsert, so repeated partial upserts cannot bypass it.
pub const MAX_PROVIDER_TRANSACTION_ENTRIES: usize = 1_024;

/// Maximum number of concurrently registered non-built-in owners.
pub const MAX_PROVIDER_OWNERS: usize = 64;

/// A validated opaque identity for one provider catalog owner.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderOwnerId(String);

impl ProviderOwnerId {
    /// Validates and constructs an owner identity.
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderRegistryError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_PROVIDER_OWNER_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(ProviderRegistryError::InvalidOwnerId);
        }
        Ok(Self(value))
    }

    /// Returns the reserved built-in owner identity.
    pub fn builtin() -> Self {
        Self(BUILTIN_PROVIDER_OWNER_ID.to_owned())
    }

    /// Returns the opaque identity as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProviderOwnerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ProviderOwnerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProviderOwnerId {
    type Err = ProviderRegistryError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<String> for ProviderOwnerId {
    type Error = ProviderRegistryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for ProviderOwnerId {
    type Error = ProviderRegistryError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Provenance for one effective endpoint or model definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSource {
    /// Owner that contributed the effective definition.
    pub owner: ProviderOwnerId,
    /// Stable precedence layer assigned when the owner first registered.
    ///
    /// The built-in owner is layer zero. Higher layers override lower layers,
    /// and updating an existing owner does not change its layer.
    pub layer: u64,
}

impl ProviderSource {
    /// Returns whether this source is the immutable built-in layer.
    pub fn is_builtin(&self) -> bool {
        self.layer == 0 && self.owner.as_str() == BUILTIN_PROVIDER_OWNER_ID
    }

    /// Returns the numeric precedence of this source.
    pub fn precedence(&self) -> u64 {
        self.layer
    }
}

/// One effective endpoint configuration and its source.
#[derive(Clone)]
pub struct ProviderEndpointEntry {
    config: EndpointConfig,
    source: ProviderSource,
}

impl ProviderEndpointEntry {
    /// Returns the validated effective endpoint configuration.
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// Returns the owner-layer provenance of this endpoint.
    pub fn source(&self) -> &ProviderSource {
        &self.source
    }
}

impl std::fmt::Debug for ProviderEndpointEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEndpointEntry")
            .field("id", &self.config.id)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// One effective model configuration and its source.
#[derive(Clone)]
pub struct ProviderModelEntry {
    config: ModelConfig,
    source: ProviderSource,
    endpoint_source: ProviderSource,
}

impl ProviderModelEntry {
    /// Returns the validated effective model configuration.
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// Returns the owner-layer provenance of this model definition.
    pub fn source(&self) -> &ProviderSource {
        &self.source
    }

    /// Returns the provenance of the model's effective endpoint definition.
    pub fn endpoint_source(&self) -> &ProviderSource {
        &self.endpoint_source
    }
}

impl std::fmt::Debug for ProviderModelEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderModelEntry")
            .field("id", &self.config.id)
            .field("source", &self.source)
            .field("endpoint_source", &self.endpoint_source)
            .finish_non_exhaustive()
    }
}

/// An immutable effective provider catalog at one registry revision.
pub struct ProviderRegistrySnapshot {
    /// Monotonically increasing publication revision. The initial built-in
    /// snapshot is revision zero.
    pub revision: u64,
    catalog: ModelCatalog,
    endpoints: BTreeMap<String, ProviderEndpointEntry>,
    models: BTreeMap<String, ProviderModelEntry>,
}

impl ProviderRegistrySnapshot {
    /// Returns the validated model catalog represented by this snapshot.
    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    /// Returns an effective endpoint entry by identifier.
    pub fn endpoint(&self, id: &EndpointId) -> Option<&ProviderEndpointEntry> {
        self.endpoints.get(&id.0)
    }

    /// Returns an effective model entry by identifier.
    pub fn model(&self, id: &ModelId) -> Option<&ProviderModelEntry> {
        self.models.get(&id.0)
    }

    /// Iterates over effective endpoints in identifier order.
    pub fn endpoints(&self) -> impl Iterator<Item = &ProviderEndpointEntry> {
        self.endpoints.values()
    }

    /// Iterates over effective models in identifier order.
    pub fn models(&self) -> impl Iterator<Item = &ProviderModelEntry> {
        self.models.values()
    }

    /// Resolves and pins a model, endpoint, owners, and this snapshot revision.
    ///
    /// The returned value retains this snapshot's [`Arc`], so a later replace
    /// or unregister cannot alter an in-flight request.
    pub fn resolve(self: &Arc<Self>, id: &ModelId) -> Result<ResolvedProviderModel, ConfigError> {
        let model = self.catalog.resolve(id)?;
        let entry = self
            .models
            .get(&id.0)
            .expect("validated catalog and source metadata must agree");
        Ok(ResolvedProviderModel {
            model,
            revision: self.revision,
            model_source: entry.source.clone(),
            endpoint_source: entry.endpoint_source.clone(),
            snapshot: Arc::clone(self),
        })
    }
}

impl std::fmt::Debug for ProviderRegistrySnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistrySnapshot")
            .field("revision", &self.revision)
            .field("endpoints", &self.endpoints.len())
            .field("models", &self.models.len())
            .finish()
    }
}

/// A request-time model binding pinned to an immutable registry snapshot.
#[derive(Clone)]
pub struct ResolvedProviderModel {
    /// Canonical model specification and destination endpoint.
    pub model: Model,
    /// Registry revision used for this resolution.
    pub revision: u64,
    /// Source of the effective model definition. Its owner is the routing owner.
    pub model_source: ProviderSource,
    /// Source of the effective endpoint definition.
    pub endpoint_source: ProviderSource,
    snapshot: Arc<ProviderRegistrySnapshot>,
}

impl ResolvedProviderModel {
    /// Returns the owner that contributed the effective model definition.
    pub fn owner(&self) -> &ProviderOwnerId {
        &self.model_source.owner
    }

    /// Returns the immutable snapshot retained by this resolution.
    pub fn snapshot(&self) -> &ProviderRegistrySnapshot {
        &self.snapshot
    }
}

impl std::fmt::Debug for ResolvedProviderModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedProviderModel")
            .field("model", &self.model)
            .field("revision", &self.revision)
            .field("model_source", &self.model_source)
            .field("endpoint_source", &self.endpoint_source)
            .finish()
    }
}

/// Failure to validate or publish a provider registry transaction.
#[derive(Debug, thiserror::Error)]
pub enum ProviderRegistryError {
    /// The supplied owner ID is empty, too long, or contains a control character.
    #[error(
        "provider owner ID must be non-empty, control-free, and at most {MAX_PROVIDER_OWNER_ID_BYTES} bytes"
    )]
    InvalidOwnerId,
    /// The reserved built-in layer cannot be changed through owner transactions.
    #[error("the built-in provider owner is immutable")]
    BuiltinOwnerIsImmutable,
    /// An owner transaction exceeds the endpoint-plus-model entry limit.
    #[error("provider owner catalog has {entries} entries; limit is {limit}")]
    CatalogTooLarge {
        /// Number of entries in the rejected catalog.
        entries: usize,
        /// Maximum accepted number of entries.
        limit: usize,
    },
    /// The registry already has the maximum number of live owners.
    #[error("provider registry has reached its owner limit of {limit}")]
    TooManyOwners {
        /// Maximum accepted number of non-built-in owners.
        limit: usize,
    },
    /// The publication revision can no longer advance.
    #[error("provider registry revision is exhausted")]
    RevisionExhausted,
    /// The stable owner-precedence layer sequence can no longer advance.
    #[error("provider registry owner layers are exhausted")]
    OwnerLayersExhausted,
    /// The candidate catalog failed ordinary model-catalog validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Thread-safe owner-layered provider catalog registry.
///
/// New owners receive increasing stable layers, so later registrations win.
/// Replacing or upserting an existing owner retains its original precedence.
/// Every mutation is validated against a cloned candidate and publishes one
/// new [`Arc<ProviderRegistrySnapshot>`] only after all validation succeeds.
#[derive(Clone)]
pub struct ProviderRegistry {
    state: Arc<RwLock<RegistryState>>,
}

impl ProviderRegistry {
    /// Creates a registry whose immutable layer zero is `built_in`.
    pub fn new(built_in: CatalogConfig) -> Result<Self, ProviderRegistryError> {
        let built_in = OwnerLayer {
            source: ProviderSource {
                owner: ProviderOwnerId::builtin(),
                layer: 0,
            },
            catalog: LayerCatalog::from_config(built_in)?,
        };
        let owners = BTreeMap::new();
        let snapshot = Arc::new(build_snapshot(0, &built_in, &owners)?);
        Ok(Self {
            state: Arc::new(RwLock::new(RegistryState {
                built_in,
                owners,
                next_owner_layer: 1,
                snapshot,
            })),
        })
    }

    /// Creates a registry from a built-in catalog configuration.
    ///
    /// This is an explicit alias for [`ProviderRegistry::new`] matching
    /// [`ModelCatalog::from_config`]'s naming.
    pub fn from_config(built_in: CatalogConfig) -> Result<Self, ProviderRegistryError> {
        Self::new(built_in)
    }

    /// Parses the embedded catalog as immutable layer zero.
    pub fn builtin() -> Result<Self, ProviderRegistryError> {
        let config = serde_json::from_str(include_str!("../models/catalog.json"))
            .map_err(|error| ConfigError::Parse(error.to_string()))?;
        Self::new(config)
    }

    /// Returns the current immutable snapshot.
    pub fn snapshot(&self) -> Arc<ProviderRegistrySnapshot> {
        Arc::clone(&read_state(&self.state).snapshot)
    }

    /// Resolves a request from the current snapshot and pins that revision.
    pub fn resolve(&self, id: &ModelId) -> Result<ResolvedProviderModel, ConfigError> {
        self.snapshot().resolve(id)
    }

    /// Transactionally merges endpoint and model definitions into an owner layer.
    ///
    /// Definitions with matching IDs replace that owner's previous definitions;
    /// omitted definitions remain present. Use [`ProviderRegistry::replace`] for
    /// complete refresh semantics.
    pub fn upsert(
        &self,
        owner: impl AsRef<str>,
        config: CatalogConfig,
    ) -> Result<Arc<ProviderRegistrySnapshot>, ProviderRegistryError> {
        self.update(owner.as_ref(), config, UpdateMode::Upsert)
    }

    /// Transactionally replaces one owner's complete endpoint/model catalog.
    pub fn replace(
        &self,
        owner: impl AsRef<str>,
        config: CatalogConfig,
    ) -> Result<Arc<ProviderRegistrySnapshot>, ProviderRegistryError> {
        self.update(owner.as_ref(), config, UpdateMode::Replace)
    }

    /// Transactionally removes an owner layer, revealing lower definitions.
    ///
    /// Removing an unknown owner is idempotent and returns the current snapshot
    /// without advancing its revision. An unregister that would orphan any
    /// retained layered model is rejected atomically.
    pub fn unregister(
        &self,
        owner: impl AsRef<str>,
    ) -> Result<Arc<ProviderRegistrySnapshot>, ProviderRegistryError> {
        let owner = validate_mutable_owner(owner.as_ref())?;
        let mut state = write_state(&self.state);
        if !state.owners.contains_key(&owner) {
            return Ok(Arc::clone(&state.snapshot));
        }

        let mut owners = state.owners.clone();
        owners.remove(&owner);
        publish_candidate(&mut state, owners, None)
    }

    fn update(
        &self,
        owner: &str,
        config: CatalogConfig,
        mode: UpdateMode,
    ) -> Result<Arc<ProviderRegistrySnapshot>, ProviderRegistryError> {
        let owner = validate_mutable_owner(owner)?;
        let incoming = LayerCatalog::from_config(config)?;
        let mut state = write_state(&self.state);
        let mut owners = state.owners.clone();
        let mut next_owner_layer = None;

        if let Some(layer) = owners.get_mut(&owner) {
            match mode {
                UpdateMode::Upsert => layer.catalog.merge(incoming)?,
                UpdateMode::Replace => layer.catalog = incoming,
            }
        } else {
            if owners.len() >= MAX_PROVIDER_OWNERS {
                return Err(ProviderRegistryError::TooManyOwners {
                    limit: MAX_PROVIDER_OWNERS,
                });
            }
            let layer_number = state.next_owner_layer;
            let advanced = layer_number
                .checked_add(1)
                .ok_or(ProviderRegistryError::OwnerLayersExhausted)?;
            owners.insert(
                owner.clone(),
                OwnerLayer {
                    source: ProviderSource {
                        owner,
                        layer: layer_number,
                    },
                    catalog: incoming,
                },
            );
            next_owner_layer = Some(advanced);
        }

        publish_candidate(&mut state, owners, next_owner_layer)
    }
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = read_state(&self.state);
        f.debug_struct("ProviderRegistry")
            .field("revision", &state.snapshot.revision)
            .field("owners", &state.owners.len())
            .finish()
    }
}

#[derive(Clone, Copy)]
enum UpdateMode {
    Upsert,
    Replace,
}

struct RegistryState {
    built_in: OwnerLayer,
    owners: BTreeMap<ProviderOwnerId, OwnerLayer>,
    next_owner_layer: u64,
    snapshot: Arc<ProviderRegistrySnapshot>,
}

#[derive(Clone)]
struct OwnerLayer {
    source: ProviderSource,
    catalog: LayerCatalog,
}

#[derive(Clone, Default)]
struct LayerCatalog {
    endpoints: BTreeMap<String, EndpointConfig>,
    models: BTreeMap<String, ModelConfig>,
}

impl LayerCatalog {
    fn from_config(config: CatalogConfig) -> Result<Self, ProviderRegistryError> {
        ensure_catalog_size(config.endpoints.len(), config.models.len())?;

        let mut endpoints = BTreeMap::new();
        for endpoint in config.endpoints {
            let id = endpoint.id.clone();
            if endpoints.contains_key(&id.0) {
                return Err(ConfigError::DuplicateEndpoint(id).into());
            }
            endpoints.insert(id.0.clone(), endpoint);
        }

        let mut models = BTreeMap::new();
        for model in config.models {
            let id = model.id.clone();
            if models.contains_key(&id.0) {
                return Err(ConfigError::DuplicateModel(id).into());
            }
            models.insert(id.0.clone(), model);
        }

        Ok(Self { endpoints, models })
    }

    fn merge(&mut self, incoming: Self) -> Result<(), ProviderRegistryError> {
        let endpoint_count = self
            .endpoints
            .keys()
            .filter(|id| !incoming.endpoints.contains_key(*id))
            .count()
            + incoming.endpoints.len();
        let model_count = self
            .models
            .keys()
            .filter(|id| !incoming.models.contains_key(*id))
            .count()
            + incoming.models.len();
        ensure_catalog_size(endpoint_count, model_count)?;
        self.endpoints.extend(incoming.endpoints);
        self.models.extend(incoming.models);
        Ok(())
    }
}

fn validate_mutable_owner(owner: &str) -> Result<ProviderOwnerId, ProviderRegistryError> {
    let owner = ProviderOwnerId::new(owner)?;
    if owner.as_str() == BUILTIN_PROVIDER_OWNER_ID {
        return Err(ProviderRegistryError::BuiltinOwnerIsImmutable);
    }
    Ok(owner)
}

fn ensure_catalog_size(
    endpoint_count: usize,
    model_count: usize,
) -> Result<(), ProviderRegistryError> {
    let entries = endpoint_count.saturating_add(model_count);
    if entries > MAX_PROVIDER_TRANSACTION_ENTRIES {
        return Err(ProviderRegistryError::CatalogTooLarge {
            entries,
            limit: MAX_PROVIDER_TRANSACTION_ENTRIES,
        });
    }
    Ok(())
}

fn publish_candidate(
    state: &mut RegistryState,
    owners: BTreeMap<ProviderOwnerId, OwnerLayer>,
    next_owner_layer: Option<u64>,
) -> Result<Arc<ProviderRegistrySnapshot>, ProviderRegistryError> {
    let revision = state
        .snapshot
        .revision
        .checked_add(1)
        .ok_or(ProviderRegistryError::RevisionExhausted)?;
    let snapshot = Arc::new(build_snapshot(revision, &state.built_in, &owners)?);

    state.owners = owners;
    if let Some(next_owner_layer) = next_owner_layer {
        state.next_owner_layer = next_owner_layer;
    }
    state.snapshot = Arc::clone(&snapshot);
    Ok(snapshot)
}

fn build_snapshot(
    revision: u64,
    built_in: &OwnerLayer,
    owners: &BTreeMap<ProviderOwnerId, OwnerLayer>,
) -> Result<ProviderRegistrySnapshot, ProviderRegistryError> {
    let mut layers = Vec::with_capacity(owners.len() + 1);
    layers.push(built_in);
    layers.extend(owners.values());
    layers.sort_unstable_by_key(|layer| layer.source.layer);

    let mut effective_endpoints = BTreeMap::new();
    let mut effective_models = BTreeMap::new();
    for layer in &layers {
        for (id, endpoint) in &layer.catalog.endpoints {
            effective_endpoints.insert(id.clone(), (endpoint.clone(), layer.source.clone()));
        }
        for (id, model) in &layer.catalog.models {
            effective_models.insert(id.clone(), (model.clone(), layer.source.clone()));
        }
    }

    // Validate every stored definition, not only entries that happen to be
    // effective today. This guarantees that unregister can reveal only data
    // that passed ordinary ModelCatalog validation. Models may intentionally
    // refer to endpoints from another owner layer.
    for layer in &layers {
        ModelCatalog::from_config(CatalogConfig {
            endpoints: layer.catalog.endpoints.values().cloned().collect(),
            models: Vec::new(),
        })?;

        let mut referenced_endpoints = BTreeMap::new();
        for model in layer.catalog.models.values() {
            let Some((endpoint, _)) = effective_endpoints.get(&model.endpoint.0) else {
                return Err(ConfigError::UnknownEndpoint(model.endpoint.clone()).into());
            };
            referenced_endpoints.insert(model.endpoint.0.clone(), endpoint.clone());
        }
        ModelCatalog::from_config(CatalogConfig {
            endpoints: referenced_endpoints.into_values().collect(),
            models: layer.catalog.models.values().cloned().collect(),
        })?;
    }

    let catalog = ModelCatalog::from_config(CatalogConfig {
        endpoints: effective_endpoints
            .values()
            .map(|(config, _)| config.clone())
            .collect(),
        models: effective_models
            .values()
            .map(|(config, _)| config.clone())
            .collect(),
    })?;

    let endpoints = effective_endpoints
        .into_iter()
        .map(|(id, (config, source))| (id, ProviderEndpointEntry { config, source }))
        .collect::<BTreeMap<_, _>>();
    let models = effective_models
        .into_iter()
        .map(|(id, (config, source))| {
            let endpoint_source = endpoints
                .get(&config.endpoint.0)
                .expect("validated model endpoint must have source metadata")
                .source
                .clone();
            (
                id,
                ProviderModelEntry {
                    config,
                    source,
                    endpoint_source,
                },
            )
        })
        .collect();

    Ok(ProviderRegistrySnapshot {
        revision,
        catalog,
        endpoints,
        models,
    })
}

fn read_state(lock: &RwLock<RegistryState>) -> std::sync::RwLockReadGuard<'_, RegistryState> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_state(lock: &RwLock<RegistryState>) -> std::sync::RwLockWriteGuard<'_, RegistryState> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded_config() -> CatalogConfig {
        serde_json::from_str(include_str!("../models/catalog.json")).unwrap()
    }

    fn base_config() -> CatalogConfig {
        let embedded = embedded_config();
        let mut endpoint = embedded.endpoints[0].clone();
        endpoint.id = EndpointId("endpoint".into());
        endpoint.base_url = url::Url::parse("https://builtin.example/v1/").unwrap();

        let mut model = embedded.models[0].clone();
        model.id = ModelId("model".into());
        model.endpoint = endpoint.id.clone();
        model.api_name = "builtin-model".into();

        CatalogConfig {
            endpoints: vec![endpoint],
            models: vec![model],
        }
    }

    fn overlay_config(owner_name: &str) -> CatalogConfig {
        let base = base_config();
        let mut endpoint = base.endpoints[0].clone();
        endpoint.base_url = url::Url::parse(&format!("https://{owner_name}.example/v1/")).unwrap();
        let mut model = base.models[0].clone();
        model.api_name = format!("{owner_name}-model");
        CatalogConfig {
            endpoints: vec![endpoint],
            models: vec![model],
        }
    }

    #[test]
    fn override_and_unregister_restore_builtins() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        let initial = registry.snapshot();
        assert_eq!(initial.revision, 0);
        assert!(initial
            .model(&ModelId("model".into()))
            .unwrap()
            .source()
            .is_builtin());

        let overridden = registry
            .replace("extension-a", overlay_config("extension-a"))
            .unwrap();
        assert_eq!(overridden.revision, 1);
        assert_eq!(
            overridden
                .model(&ModelId("model".into()))
                .unwrap()
                .source()
                .owner
                .as_str(),
            "extension-a"
        );
        assert_eq!(
            overridden
                .endpoint(&EndpointId("endpoint".into()))
                .unwrap()
                .config()
                .base_url
                .host_str(),
            Some("extension-a.example")
        );

        let restored = registry.unregister("extension-a").unwrap();
        assert_eq!(restored.revision, 2);
        assert_eq!(
            restored
                .catalog()
                .resolve(&ModelId("model".into()))
                .unwrap()
                .spec
                .api_name,
            "builtin-model"
        );
        assert!(restored
            .endpoint(&EndpointId("endpoint".into()))
            .unwrap()
            .source()
            .is_builtin());
    }

    #[test]
    fn invalid_transaction_is_atomic() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        let before = registry.snapshot();
        let mut invalid_model = base_config().models.remove(0);
        invalid_model.endpoint = EndpointId("absent".into());

        let error = registry
            .replace(
                "invalid-owner",
                CatalogConfig {
                    endpoints: Vec::new(),
                    models: vec![invalid_model],
                },
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ProviderRegistryError::Config(ConfigError::UnknownEndpoint(_))
        ));

        let after = registry.snapshot();
        assert!(Arc::ptr_eq(&before, &after));
        assert_eq!(after.revision, 0);

        // A failed first transaction must not consume owner precedence either.
        let valid = registry
            .replace("valid-owner", overlay_config("valid-owner"))
            .unwrap();
        assert_eq!(
            valid
                .model(&ModelId("model".into()))
                .unwrap()
                .source()
                .layer,
            1
        );
    }

    #[test]
    fn resolved_request_retains_immutable_snapshot() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        registry
            .replace("extension-a", overlay_config("extension-a"))
            .unwrap();
        let in_flight = registry.resolve(&ModelId("model".into())).unwrap();

        registry.unregister("extension-a").unwrap();
        let current = registry.resolve(&ModelId("model".into())).unwrap();

        assert_eq!(in_flight.revision, 1);
        assert_eq!(in_flight.owner().as_str(), "extension-a");
        assert_eq!(in_flight.model.spec.api_name, "extension-a-model");
        assert_eq!(
            in_flight.model.endpoint.base_url.host_str(),
            Some("extension-a.example")
        );
        assert_eq!(in_flight.snapshot().revision, 1);

        assert_eq!(current.revision, 2);
        assert!(current.model_source.is_builtin());
        assert_eq!(current.model.spec.api_name, "builtin-model");
    }

    #[test]
    fn owner_precedence_is_stable_across_updates() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        registry
            .replace("owner-a", overlay_config("owner-a"))
            .unwrap();
        registry
            .replace("owner-b", overlay_config("owner-b"))
            .unwrap();

        let mut owner_a_update = overlay_config("owner-a-updated");
        // The owner identity, not values inside the transaction, determines
        // the stable layer being updated.
        owner_a_update.models[0].api_name = "owner-a-updated-model".into();
        registry.upsert("owner-a", owner_a_update).unwrap();

        let effective = registry.snapshot();
        let model = effective.model(&ModelId("model".into())).unwrap();
        assert_eq!(model.source().owner.as_str(), "owner-b");
        assert_eq!(model.source().layer, 2);

        let revealed = registry.unregister("owner-b").unwrap();
        let model = revealed.model(&ModelId("model".into())).unwrap();
        assert_eq!(model.source().owner.as_str(), "owner-a");
        assert_eq!(model.source().layer, 1);
        assert_eq!(model.config().api_name, "owner-a-updated-model");
    }

    #[test]
    fn owner_and_catalog_bounds_are_enforced() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        assert!(matches!(
            registry.replace(
                "",
                CatalogConfig {
                    endpoints: vec![],
                    models: vec![]
                }
            ),
            Err(ProviderRegistryError::InvalidOwnerId)
        ));
        assert!(matches!(
            registry.replace(
                "x".repeat(MAX_PROVIDER_OWNER_ID_BYTES + 1),
                CatalogConfig {
                    endpoints: vec![],
                    models: vec![]
                }
            ),
            Err(ProviderRegistryError::InvalidOwnerId)
        ));

        let template = base_config().models.remove(0);
        let models = (0..=MAX_PROVIDER_TRANSACTION_ENTRIES)
            .map(|index| {
                let mut model = template.clone();
                model.id = ModelId(format!("model-{index}"));
                model
            })
            .collect();
        assert!(matches!(
            registry.replace(
                "large-owner",
                CatalogConfig {
                    endpoints: Vec::new(),
                    models,
                }
            ),
            Err(ProviderRegistryError::CatalogTooLarge { .. })
        ));

        for index in 0..MAX_PROVIDER_OWNERS {
            registry
                .replace(
                    format!("owner-{index}"),
                    CatalogConfig {
                        endpoints: Vec::new(),
                        models: Vec::new(),
                    },
                )
                .unwrap();
        }
        assert!(matches!(
            registry.replace(
                "one-owner-too-many",
                CatalogConfig {
                    endpoints: Vec::new(),
                    models: Vec::new(),
                }
            ),
            Err(ProviderRegistryError::TooManyOwners { .. })
        ));
    }

    #[test]
    fn duplicate_endpoint_and_model_ids_are_rejected() {
        let registry = ProviderRegistry::new(base_config()).unwrap();
        let endpoint = base_config().endpoints.remove(0);
        assert!(matches!(
            registry.replace(
                "duplicate-endpoints",
                CatalogConfig {
                    endpoints: vec![endpoint.clone(), endpoint],
                    models: Vec::new(),
                }
            ),
            Err(ProviderRegistryError::Config(
                ConfigError::DuplicateEndpoint(_)
            ))
        ));

        let model = base_config().models.remove(0);
        assert!(matches!(
            registry.replace(
                "duplicate-models",
                CatalogConfig {
                    endpoints: Vec::new(),
                    models: vec![model.clone(), model],
                }
            ),
            Err(ProviderRegistryError::Config(ConfigError::DuplicateModel(
                _
            )))
        ));
        assert_eq!(registry.snapshot().revision, 0);
    }
}
