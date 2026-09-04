//! Host-owned GitHub Copilot provider integration.
//!
//! GitHub OAuth/device state belongs to the embedding application, not the Ygg
//! CLI credential store. This module only retains a short-lived inference
//! session in memory behind `ygg_ai::Auth::Dynamic`; no OAuth token, exchange
//! response, or dynamic header is part of a provider definition, catalog,
//! diagnostic, or persistence format.

use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ygg_ai::{
    Auth, Capabilities, CredentialResolver, CredentialScheme, EndpointTransport, ModelCatalog,
    ModelLimits, OpenAiChatRuntimeProfile, Protocol, RequestBodyEncoding, RequestRuntime,
    ResolvedCredential, ResponsesRuntimeProfile, Secret,
};

use super::contract::{
    CompatibilityProfile, DiscoveryCapabilityProfile, EndpointAuthPresentation, InventoryCacheMode,
    ModelDiscovery, ModelRouteRule, PricingProfile, ProviderAuthentication, ProviderDeclaration,
    ProviderDefinition, ProviderRoute, ProviderRuntimeConfiguration, StaticModelSet,
};

const MAX_DISCOVERED_MODELS: usize = 128;
const MAX_MODEL_ID_BYTES: usize = 256;
const MAX_MODEL_LABEL_BYTES: usize = 256;
const MAX_CONTEXT_WINDOW: u64 = 10_000_000;
const MAX_DYNAMIC_HEADERS: usize = 24;
const MAX_DYNAMIC_HEADER_VALUE_BYTES: usize = 4096;
const MAX_SESSION_CREDENTIAL_BYTES: usize = 4096;
const MAX_ENDPOINT_URL_BYTES: usize = 2048;
const MAX_SESSION_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const REFRESH_SKEW: Duration = Duration::from_secs(30);
const DEFAULT_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const COPILOT_ROUTES: &[ProviderRoute] = &[
    ProviderRoute {
        endpoint_id: "github-copilot-chat",
        base_path: "",
        protocol: Protocol::OpenAiChat,
        auth_presentation: EndpointAuthPresentation::Dynamic,
        transport: EndpointTransport::Http,
        runtime: RequestRuntime {
            body_encoding: RequestBodyEncoding::Identity,
            responses_profile: ResponsesRuntimeProfile::Default,
            openai_chat_profile: OpenAiChatRuntimeProfile::Default,
            lifecycle_feedback: false,
        },
    },
    ProviderRoute {
        endpoint_id: "github-copilot-responses",
        base_path: "",
        protocol: Protocol::OpenAiResponses,
        auth_presentation: EndpointAuthPresentation::Dynamic,
        transport: EndpointTransport::Http,
        runtime: RequestRuntime {
            body_encoding: RequestBodyEncoding::Identity,
            responses_profile: ResponsesRuntimeProfile::Default,
            openai_chat_profile: OpenAiChatRuntimeProfile::Default,
            lifecycle_feedback: false,
        },
    },
];

const COPILOT_ROUTE_RULES: &[ModelRouteRule] = &[ModelRouteRule::Default { route: 0 }];

// This declaration defines only routes and public setup metadata. Its base URL
// is never used as a preset: an embedding host must explicitly supply a vetted
// endpoint to `CopilotProvider::new` before any model is registered.
const COPILOT_DECLARATION: ProviderDeclaration = ProviderDeclaration {
    id: "github-copilot",
    name: "GitHub Copilot",
    base_url: "https://api.githubcopilot.com/",
    base_url_environment: &[],
    authentication: ProviderAuthentication::HostOwned {
        integration: "github-copilot",
    },
    runtime_configuration: ProviderRuntimeConfiguration::Default,
    model_discovery: ModelDiscovery::HostOwnedSubscription,
    discovery_capabilities: DiscoveryCapabilityProfile::Default,
    static_models: StaticModelSet::None,
    inventory_cache: InventoryCacheMode::Required,
    routes: COPILOT_ROUTES,
    route_rules: COPILOT_ROUTE_RULES,
    extra_headers: &[],
    compatibility: CompatibilityProfile::Default,
    pricing: PricingProfile::Subscription,
};

/// Bounded, credential-free outcome of a GitHub Copilot host operation.
///
/// The enum intentionally does not carry a provider response body or host
/// error string because either can contain credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CopilotAvailabilityError {
    /// The embedding host has no authenticated GitHub identity.
    #[error("GitHub Copilot sign-in is required")]
    LoginRequired,
    /// The device authorization has not completed yet.
    #[error("GitHub Copilot device authorization is pending")]
    DeviceAuthorizationPending,
    /// The device authorization expired before it completed.
    #[error("GitHub Copilot device authorization expired")]
    DeviceAuthorizationExpired,
    /// The user denied the device authorization.
    #[error("GitHub Copilot device authorization was denied")]
    DeviceAuthorizationDenied,
    /// The host could not exchange its authenticated state for an inference session.
    #[error("GitHub Copilot token exchange is unavailable")]
    TokenExchangeUnavailable,
    /// The host could not refresh the short-lived inference session.
    #[error("GitHub Copilot token refresh is unavailable")]
    TokenRefreshUnavailable,
    /// The host could not obtain the authenticated model inventory.
    #[error("GitHub Copilot model discovery is unavailable")]
    ModelDiscoveryUnavailable,
    /// A host-supplied endpoint does not satisfy the integration boundary.
    #[error("GitHub Copilot endpoint configuration is invalid")]
    InvalidEndpoint,
    /// A device-login display payload is malformed.
    #[error("GitHub Copilot device login metadata is invalid")]
    InvalidDeviceLogin,
    /// A short-lived session or dynamic-header payload is malformed.
    #[error("GitHub Copilot session metadata is invalid")]
    InvalidSession,
    /// An authenticated model inventory is malformed.
    #[error("GitHub Copilot model metadata is invalid")]
    InvalidModelMetadata,
    /// A host returned more models than the bounded integration allows.
    #[error("GitHub Copilot returned too many models")]
    TooManyModels,
    /// A successful inventory had no models that can be registered safely.
    #[error("GitHub Copilot has no eligible models")]
    NoEligibleModels,
    /// A model selected a codec family that this declaration does not expose.
    #[error("GitHub Copilot returned an unsupported model protocol")]
    UnsupportedModelProtocol,
    /// A validated staging catalog could not be merged into the host catalog.
    #[error("GitHub Copilot catalog registration is unavailable")]
    CatalogRegistrationUnavailable,
}

/// Public display payload for a host-owned GitHub device authorization.
///
/// The embedding host keeps any verification state and OAuth credentials. The
/// verification URI and user code are intentionally redacted from `Debug`;
/// callers render them only by explicitly calling their accessors.
#[derive(Clone)]
pub struct CopilotDeviceLogin {
    verification_uri: url::Url,
    user_code: String,
    expires_in: Duration,
    poll_interval: Duration,
}

impl CopilotDeviceLogin {
    /// Create a bounded device-login display payload.
    pub fn new(
        verification_uri: url::Url,
        user_code: impl Into<String>,
        expires_in: Duration,
        poll_interval: Duration,
    ) -> Result<Self, CopilotAvailabilityError> {
        let user_code = user_code.into();
        if !valid_device_verification_uri(&verification_uri)
            || !valid_user_code(&user_code)
            || expires_in.is_zero()
            || expires_in > Duration::from_secs(30 * 60)
            || poll_interval.is_zero()
            || poll_interval > Duration::from_secs(60)
        {
            return Err(CopilotAvailabilityError::InvalidDeviceLogin);
        }
        Ok(Self {
            verification_uri,
            user_code,
            expires_in,
            poll_interval,
        })
    }

    /// URI the embedding UI should open or show to the user.
    pub fn verification_uri(&self) -> &url::Url {
        &self.verification_uri
    }

    /// Device code the embedding UI should show to the user.
    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    /// Lifetime of this device authorization.
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }

    /// Minimum interval before the host polls the device flow again.
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

impl fmt::Debug for CopilotDeviceLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotDeviceLogin")
            .field("verification_uri", &"<host-owned>")
            .field("user_code", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

/// Status of the host-owned GitHub device authorization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopilotDeviceLoginStatus {
    /// The user has not completed authorization yet.
    Pending,
    /// The host now has authenticated state and can exchange it for a session.
    Authorized,
    /// The displayed device authorization expired.
    Expired,
    /// The user denied authorization.
    Denied,
}

/// Auth-header presentation selected by a host-issued Copilot session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopilotCredentialScheme {
    /// Send the session token as `Authorization: Bearer <token>`.
    Bearer,
    /// Send the session token in the supplied primary header.
    Header(http::HeaderName),
}

/// One host-issued request header associated with a short-lived session.
///
/// Values have no public accessor and are always marked sensitive before a
/// request is made. This permits required dynamic Copilot headers without
/// letting their values enter a definition, catalog, log, or serializer.
#[derive(Clone)]
pub struct CopilotDynamicHeader {
    name: http::HeaderName,
    value: http::HeaderValue,
}

impl CopilotDynamicHeader {
    /// Create one bounded dynamic header.
    ///
    /// Hop-by-hop/framing headers and `Authorization` are rejected. The primary
    /// session credential owns authentication header composition instead.
    pub fn new(
        name: http::HeaderName,
        value: http::HeaderValue,
    ) -> Result<Self, CopilotAvailabilityError> {
        if !valid_dynamic_header_name(&name)
            || value.as_bytes().is_empty()
            || value.as_bytes().len() > MAX_DYNAMIC_HEADER_VALUE_BYTES
        {
            return Err(CopilotAvailabilityError::InvalidSession);
        }
        Ok(Self { name, value })
    }

    /// Dynamic header name. Its value intentionally has no public accessor.
    pub fn name(&self) -> &http::HeaderName {
        &self.name
    }
}

impl fmt::Debug for CopilotDynamicHeader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotDynamicHeader")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// A short-lived inference credential issued by an embedding host.
///
/// This value is in-memory only. It does not implement serialization and
/// redacts its primary credential plus every dynamic-header value in `Debug`.
#[derive(Clone)]
pub struct CopilotSession {
    credential: Secret,
    scheme: CopilotCredentialScheme,
    headers: Vec<CopilotDynamicHeader>,
    expires_at: Instant,
}

impl CopilotSession {
    /// Build a short-lived inference session.
    ///
    /// `lifetime` is relative to construction so the host never needs to expose
    /// a long-lived OAuth token or absolute token-expiry payload to Ygg.
    pub fn new(
        credential: impl Into<Secret>,
        scheme: CopilotCredentialScheme,
        headers: Vec<CopilotDynamicHeader>,
        lifetime: Duration,
    ) -> Result<Self, CopilotAvailabilityError> {
        let credential = credential.into();
        if credential.is_empty()
            || !credential.fits_within_bytes(MAX_SESSION_CREDENTIAL_BYTES)
            || !credential.is_valid_http_header_value()
            || lifetime.is_zero()
            || lifetime > MAX_SESSION_LIFETIME
            || !valid_credential_scheme(&scheme)
        {
            return Err(CopilotAvailabilityError::InvalidSession);
        }
        if headers.len() > MAX_DYNAMIC_HEADERS {
            return Err(CopilotAvailabilityError::InvalidSession);
        }
        let mut names = BTreeSet::new();
        if headers
            .iter()
            .any(|header| !names.insert(header.name.as_str()))
        {
            return Err(CopilotAvailabilityError::InvalidSession);
        }
        Ok(Self {
            credential,
            scheme,
            headers,
            expires_at: Instant::now() + lifetime,
        })
    }

    fn is_fresh(&self) -> bool {
        self.expires_at
            .checked_duration_since(Instant::now())
            .is_some_and(|remaining| remaining > REFRESH_SKEW)
    }

    fn resolved_credential(&self) -> ResolvedCredential {
        let mut extra_headers = http::HeaderMap::new();
        for header in &self.headers {
            let mut value = header.value.clone();
            // Dynamic headers may carry token-exchange routing or session data;
            // treat every one as sensitive even if its name looks public.
            value.set_sensitive(true);
            extra_headers.insert(header.name.clone(), value);
        }
        let scheme = match &self.scheme {
            CopilotCredentialScheme::Bearer => CredentialScheme::Bearer,
            CopilotCredentialScheme::Header(name) => CredentialScheme::Header(name.clone()),
        };
        ResolvedCredential {
            scheme,
            value: self.credential.clone(),
            extra_headers,
        }
    }
}

impl fmt::Debug for CopilotSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names = self
            .headers
            .iter()
            .map(|header| header.name.as_str())
            .collect::<Vec<_>>();
        formatter
            .debug_struct("CopilotSession")
            .field("credential", &"<redacted>")
            .field("scheme", &self.scheme)
            .field("dynamic_header_names", &header_names)
            .field(
                "expires_in",
                &self.expires_at.saturating_duration_since(Instant::now()),
            )
            .finish()
    }
}

/// Credential-free model metadata supplied by an authenticated host inventory.
#[derive(Clone, Debug)]
pub struct CopilotModel {
    id: String,
    display_name: Option<String>,
    protocol: Protocol,
    capabilities: Capabilities,
    limits: ModelLimits,
}

impl CopilotModel {
    /// Create one model metadata record. Registration validates all bounds and
    /// declaration route compatibility before adding it to a catalog.
    pub fn new(
        id: impl Into<String>,
        protocol: Protocol,
        capabilities: Capabilities,
        limits: ModelLimits,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            protocol,
            capabilities,
            limits,
        }
    }

    /// Add an optional presentation label to this model metadata.
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    /// Provider API model identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Optional host-provided presentation label.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// Codec family selected for this individual model.
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Explicit model capabilities.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    /// Explicit model token limits.
    pub fn limits(&self) -> ModelLimits {
        self.limits
    }
}

/// Explicit root endpoint authority granted by an embedding host.
///
/// A host must construct this value before Copilot models can enter a catalog.
/// It accepts only an origin root, so path/query/userinfo data cannot carry
/// credential material into catalog endpoint metadata. HTTPS is required for
/// normal endpoints; HTTP is accepted only for a literal loopback address so
/// deterministic fake transports do not widen production network authority.
#[derive(Clone)]
pub struct CopilotEndpoint {
    base_url: url::Url,
    timeout: Duration,
}

impl fmt::Debug for CopilotEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotEndpoint")
            .field("base_url", &"<host-owned>")
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl CopilotEndpoint {
    /// Validate a host-owned inference origin root with the default header
    /// timeout.
    pub fn new(base_url: url::Url) -> Result<Self, CopilotAvailabilityError> {
        Self::with_timeout(base_url, DEFAULT_RESPONSE_HEADER_TIMEOUT)
    }

    /// Validate a host-owned inference origin root and request-header timeout.
    pub fn with_timeout(
        mut base_url: url::Url,
        timeout: Duration,
    ) -> Result<Self, CopilotAvailabilityError> {
        if !valid_endpoint_url(&base_url) || timeout.is_zero() {
            return Err(CopilotAvailabilityError::InvalidEndpoint);
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        Ok(Self { base_url, timeout })
    }

    /// Validated inference origin root. It cannot contain a non-root path,
    /// userinfo, query, or fragment data.
    pub fn base_url(&self) -> &url::Url {
        &self.base_url
    }

    /// Maximum time to send a request and receive response headers.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// Host-owned GitHub device authorization, exchange/refresh, and discovery.
///
/// Implementations retain every long-lived GitHub credential and device-flow
/// state themselves. They return only a short-lived [`CopilotSession`] for Ygg
/// inference, and never pass an OAuth token through the provider definition or
/// catalog APIs.
#[async_trait::async_trait]
pub trait CopilotHost: Send + Sync {
    /// Return whether the host currently has enough authenticated state to
    /// exchange a Copilot inference session.
    async fn availability(&self) -> Result<(), CopilotAvailabilityError>;

    /// Start a device authorization and return only user-displayable data.
    async fn begin_device_login(&self) -> Result<CopilotDeviceLogin, CopilotAvailabilityError>;

    /// Poll the host-owned active device authorization.
    async fn poll_device_login(&self)
        -> Result<CopilotDeviceLoginStatus, CopilotAvailabilityError>;

    /// Exchange the host-owned authenticated state for a short-lived inference
    /// session after device login (or a recovered host session).
    async fn exchange(&self) -> Result<CopilotSession, CopilotAvailabilityError>;

    /// Refresh a stale short-lived inference session. The host retains any
    /// refresh token and GitHub OAuth state; Ygg only receives the replacement
    /// inference session.
    async fn refresh(&self) -> Result<CopilotSession, CopilotAvailabilityError>;

    /// Return credential-free metadata for the authenticated Copilot models.
    async fn discover_models(&self) -> Result<Vec<CopilotModel>, CopilotAvailabilityError>;
}

/// Host-owned GitHub Copilot provider registration.
///
/// This is not a CLI preset. Models appear only after an embedding host calls
/// [`Self::register_models`] successfully, which verifies host availability,
/// exchanges a session, bounds discovery, and stages all routes first.
pub struct CopilotProvider {
    host: Arc<dyn CopilotHost>,
    endpoint: CopilotEndpoint,
    resolver: Arc<CopilotResolver>,
    definition: ProviderDefinition,
}

impl CopilotProvider {
    /// Bind a host-owned Copilot lifecycle to one explicit endpoint authority.
    pub fn new(
        host: Arc<dyn CopilotHost>,
        endpoint: CopilotEndpoint,
    ) -> Result<Self, CopilotAvailabilityError> {
        COPILOT_DECLARATION
            .validate()
            .map_err(|_| CopilotAvailabilityError::InvalidEndpoint)?;
        let resolver = Arc::new(CopilotResolver::new(Arc::clone(&host)));
        Ok(Self {
            host,
            endpoint,
            resolver,
            definition: COPILOT_DECLARATION.definition(),
        })
    }

    /// Credential-free provider definition for host setup UI.
    pub fn definition(&self) -> &ProviderDefinition {
        &self.definition
    }

    /// Check host authentication readiness without exchanging a request token.
    pub async fn availability(&self) -> Result<(), CopilotAvailabilityError> {
        self.host.availability().await
    }

    /// Start host-owned device login.
    pub async fn begin_device_login(&self) -> Result<CopilotDeviceLogin, CopilotAvailabilityError> {
        self.host.begin_device_login().await
    }

    /// Poll host-owned device login.
    pub async fn poll_device_login(
        &self,
    ) -> Result<CopilotDeviceLoginStatus, CopilotAvailabilityError> {
        self.host.poll_device_login().await
    }

    /// Exchange host authentication for an in-memory short-lived session.
    pub async fn exchange(&self) -> Result<(), CopilotAvailabilityError> {
        let session = self.host.exchange().await?;
        self.resolver.install(session).await;
        Ok(())
    }

    /// Refresh the current in-memory short-lived session explicitly.
    pub async fn refresh(&self) -> Result<(), CopilotAvailabilityError> {
        let session = self.host.refresh().await?;
        self.resolver.install(session).await;
        Ok(())
    }

    /// Discard the in-memory inference session.
    ///
    /// Hosts can call this after an authoritative authentication rejection; the
    /// next request will invoke the host's exchange seam again. No credential is
    /// persisted or left in the catalog metadata.
    pub async fn invalidate_session(&self) {
        self.resolver.invalidate().await;
    }

    /// Verify host availability and register authenticated models atomically.
    ///
    /// Discovery results are bounded and staged in a temporary catalog before
    /// any target mutation. A missing login, failed exchange, failed discovery,
    /// or invalid model leaves every Copilot model out of the picker.
    pub async fn register_models(
        &self,
        catalog: &mut ModelCatalog,
    ) -> Result<(), CopilotAvailabilityError> {
        self.availability().await?;
        self.exchange().await?;
        let models = self.host.discover_models().await?;
        let routes = validate_discovered_models(models)?;

        let mut staged = ModelCatalog::default();
        super::catalog::register_private_endpoints_at_base_url(
            &mut staged,
            &COPILOT_DECLARATION,
            Auth::dynamic(self.resolver.clone()),
            self.endpoint.base_url(),
            self.endpoint.timeout(),
        )
        .map_err(|_| CopilotAvailabilityError::CatalogRegistrationUnavailable)?;

        for (model, route) in routes {
            super::register_discovered_model_at_route(
                &mut staged,
                &COPILOT_DECLARATION,
                route,
                super::catalog::DiscoveredModelMetadata {
                    api_name: &model.id,
                    display_name: model.display_name,
                    capabilities: model.capabilities,
                    limits: model.limits,
                    pricing: None,
                },
            )
            .map_err(|_| CopilotAvailabilityError::InvalidModelMetadata)?;
        }

        merge_staged_catalog(catalog, staged)
    }
}

impl fmt::Debug for CopilotProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CopilotProvider")
            .field("definition", &self.definition)
            .field("endpoint", &self.endpoint)
            .field("host", &"<host-owned>")
            .field("resolver", &"<dynamic>")
            .finish()
    }
}

struct CopilotResolver {
    host: Arc<dyn CopilotHost>,
    session: tokio::sync::Mutex<Option<CopilotSession>>,
}

impl CopilotResolver {
    fn new(host: Arc<dyn CopilotHost>) -> Self {
        Self {
            host,
            session: tokio::sync::Mutex::new(None),
        }
    }

    async fn install(&self, session: CopilotSession) {
        *self.session.lock().await = Some(session);
    }

    async fn invalidate(&self) {
        *self.session.lock().await = None;
    }
}

#[async_trait::async_trait]
impl CredentialResolver for CopilotResolver {
    async fn resolve(&self) -> Result<ResolvedCredential, ygg_ai::AuthError> {
        // Serialize exchange/refresh so concurrent request starts cannot fan out
        // into multiple device-session exchanges. Holding this async mutex is
        // cancellation-safe: dropping a canceled resolve releases it and no
        // token is persisted.
        let mut session = self.session.lock().await;
        if let Some(current) = session.as_ref().filter(|current| current.is_fresh()) {
            return Ok(current.resolved_credential());
        }

        let replacement = if session.take().is_some() {
            self.host.refresh().await
        } else {
            self.host.exchange().await
        }
        .map_err(|_| ygg_ai::AuthError::Resolve)?;
        let credential = replacement.resolved_credential();
        *session = Some(replacement);
        Ok(credential)
    }
}

fn validate_discovered_models(
    models: Vec<CopilotModel>,
) -> Result<Vec<(CopilotModel, &'static ProviderRoute)>, CopilotAvailabilityError> {
    if models.len() > MAX_DISCOVERED_MODELS {
        return Err(CopilotAvailabilityError::TooManyModels);
    }
    if models.is_empty() {
        return Err(CopilotAvailabilityError::NoEligibleModels);
    }

    let mut ids = BTreeSet::new();
    let mut routes = Vec::with_capacity(models.len());
    for model in models {
        if !valid_model_metadata(&model) || !ids.insert(model.id.clone()) {
            return Err(CopilotAvailabilityError::InvalidModelMetadata);
        }
        let route = COPILOT_DECLARATION
            .route_for_protocol(model.protocol)
            .ok_or(CopilotAvailabilityError::UnsupportedModelProtocol)?;
        routes.push((model, route));
    }
    Ok(routes)
}

fn merge_staged_catalog(
    target: &mut ModelCatalog,
    staged: ModelCatalog,
) -> Result<(), CopilotAvailabilityError> {
    let models = staged.models().cloned().collect::<Vec<_>>();

    // Fixed route and model identities must never silently bind a Copilot model
    // to an endpoint registered by somebody else. Preflight every actual route
    // before cloning/mutating the caller's catalog so a collision cannot leave a
    // partially visible Copilot inventory in the picker.
    for spec in &models {
        let model = staged
            .resolve(&spec.id)
            .map_err(|_| CopilotAvailabilityError::CatalogRegistrationUnavailable)?;
        if target.has_endpoint(&model.endpoint.id) || target.resolve(&spec.id).is_ok() {
            return Err(CopilotAvailabilityError::CatalogRegistrationUnavailable);
        }
    }

    let mut merged = target.clone();
    for spec in models {
        let model = staged
            .resolve(&spec.id)
            .map_err(|_| CopilotAvailabilityError::CatalogRegistrationUnavailable)?;
        if !merged.has_endpoint(&model.endpoint.id) {
            merged
                .register_endpoint((*model.endpoint).clone())
                .map_err(|_| CopilotAvailabilityError::CatalogRegistrationUnavailable)?;
        }
        merged
            .register_model(spec)
            .map_err(|_| CopilotAvailabilityError::CatalogRegistrationUnavailable)?;
    }
    *target = merged;
    Ok(())
}

fn valid_device_verification_uri(url: &url::Url) -> bool {
    url.as_str().len() <= MAX_ENDPOINT_URL_BYTES
        && url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn valid_user_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_endpoint_url(url: &url::Url) -> bool {
    let http_loopback = url.scheme() == "http"
        && url
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
    (url.scheme() == "https" || http_loopback)
        && url.as_str().len() <= MAX_ENDPOINT_URL_BYTES
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(url.path(), "" | "/")
        && url.query().is_none()
        && url.fragment().is_none()
}

fn valid_dynamic_header_name(name: &http::HeaderName) -> bool {
    !forbidden_transport_header(name) && name != http::header::AUTHORIZATION
}

fn valid_credential_scheme(scheme: &CopilotCredentialScheme) -> bool {
    match scheme {
        CopilotCredentialScheme::Bearer => true,
        CopilotCredentialScheme::Header(name) => !forbidden_transport_header(name),
    }
}

fn forbidden_transport_header(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "upgrade"
    )
}

fn valid_model_metadata(model: &CopilotModel) -> bool {
    valid_model_id(&model.id)
        && model.display_name.as_deref().is_none_or(valid_model_label)
        && model.limits.context_window > 0
        && model.limits.context_window <= MAX_CONTEXT_WINDOW
        && model.limits.max_output_tokens > 0
        && model.limits.max_output_tokens <= model.limits.context_window
        && (!model.capabilities.parallel_tool_calls || model.capabilities.tools)
        && (!model.capabilities.deferred_tool_loading || model.capabilities.tools)
}

fn valid_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_MODEL_ID_BYTES
        && value.trim() == value
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'+')
        })
}

fn valid_model_label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= MAX_MODEL_LABEL_BYTES
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;

    use futures_util::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use ygg_ai::{
        AiClient, CompatibilityMode, Message, ModalitySet, OutputFormat, OutputModalities, Request,
        StreamEvent, ToolChoice, UserMessage, UserPart,
    };

    const PRIMARY_TOKEN: &str = "copilot-primary-token";
    const DYNAMIC_TOKEN: &str = "copilot-dynamic-header-token";
    const REFRESHED_TOKEN: &str = "copilot-refreshed-token";

    struct FakeHost {
        availability: Option<CopilotAvailabilityError>,
        device_login: CopilotDeviceLogin,
        poll_statuses: Mutex<VecDeque<CopilotDeviceLoginStatus>>,
        exchanges: Mutex<VecDeque<CopilotSession>>,
        refreshes: Mutex<VecDeque<CopilotSession>>,
        models: Vec<CopilotModel>,
        exchange_calls: AtomicUsize,
        refresh_calls: AtomicUsize,
    }

    impl FakeHost {
        fn with_state(
            availability: Option<CopilotAvailabilityError>,
            exchanges: Vec<CopilotSession>,
            refreshes: Vec<CopilotSession>,
            models: Vec<CopilotModel>,
        ) -> Self {
            Self {
                availability,
                device_login: CopilotDeviceLogin::new(
                    url::Url::parse("https://github.example.test/login/device").unwrap(),
                    "DEVICE-CODE-123",
                    Duration::from_secs(600),
                    Duration::from_secs(5),
                )
                .unwrap(),
                poll_statuses: Mutex::new(VecDeque::from([
                    CopilotDeviceLoginStatus::Pending,
                    CopilotDeviceLoginStatus::Authorized,
                ])),
                exchanges: Mutex::new(exchanges.into()),
                refreshes: Mutex::new(refreshes.into()),
                models,
                exchange_calls: AtomicUsize::new(0),
                refresh_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl CopilotHost for FakeHost {
        async fn availability(&self) -> Result<(), CopilotAvailabilityError> {
            self.availability.map_or(Ok(()), Err)
        }

        async fn begin_device_login(&self) -> Result<CopilotDeviceLogin, CopilotAvailabilityError> {
            self.availability
                .map_or_else(|| Ok(self.device_login.clone()), Err)
        }

        async fn poll_device_login(
            &self,
        ) -> Result<CopilotDeviceLoginStatus, CopilotAvailabilityError> {
            self.availability.map_or_else(
                || {
                    Ok(self
                        .poll_statuses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or(CopilotDeviceLoginStatus::Authorized))
                },
                Err,
            )
        }

        async fn exchange(&self) -> Result<CopilotSession, CopilotAvailabilityError> {
            self.exchange_calls.fetch_add(1, Ordering::SeqCst);
            self.exchanges
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(CopilotAvailabilityError::TokenExchangeUnavailable)
        }

        async fn refresh(&self) -> Result<CopilotSession, CopilotAvailabilityError> {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            self.refreshes
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(CopilotAvailabilityError::TokenRefreshUnavailable)
        }

        async fn discover_models(&self) -> Result<Vec<CopilotModel>, CopilotAvailabilityError> {
            Ok(self.models.clone())
        }
    }

    fn fake_session(token: &str, lifetime: Duration) -> CopilotSession {
        let dynamic = CopilotDynamicHeader::new(
            http::HeaderName::from_static("x-copilot-session"),
            http::HeaderValue::from_static(DYNAMIC_TOKEN),
        )
        .unwrap();
        CopilotSession::new(
            token,
            CopilotCredentialScheme::Bearer,
            vec![dynamic],
            lifetime,
        )
        .unwrap()
    }

    fn fake_model(id: &str, protocol: Protocol) -> CopilotModel {
        CopilotModel::new(
            id,
            protocol,
            Capabilities {
                input_modalities: ModalitySet::none(),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: None,
                responses_lite: false,
                agent_delegation: None,
                structured_output: protocol != Protocol::OpenAiChat,
                deferred_tool_loading: false,
            },
            ModelLimits {
                context_window: 128_000,
                max_output_tokens: 16_384,
            },
        )
    }

    fn provider_for(host: Arc<FakeHost>, endpoint: &str) -> CopilotProvider {
        CopilotProvider::new(
            host,
            CopilotEndpoint::new(url::Url::parse(endpoint).unwrap()).unwrap(),
        )
        .unwrap()
    }

    fn text_request() -> Request {
        Request {
            system: None,
            messages: vec![Message::User(UserMessage {
                content: vec![UserPart::Text("hello".to_owned())],
            })],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: None,
            temperature: None,
            stop: vec![],
            reasoning: ygg_ai::ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
            responses: None,
            output_format: OutputFormat::Text,
            output_modalities: OutputModalities::Text,
            compatibility: CompatibilityMode::Strict,
            cache_retention: ygg_ai::CacheRetention::Short,
            session_id: None,
        }
    }

    #[tokio::test]
    async fn host_owns_device_login_exchange_and_setup_diagnostic() {
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![fake_model("chat-model", Protocol::OpenAiChat)],
        ));
        let provider = provider_for(Arc::clone(&host), "https://api.example.test/");

        let login = provider.begin_device_login().await.unwrap();
        assert_eq!(login.user_code(), "DEVICE-CODE-123");
        assert_eq!(
            provider.poll_device_login().await.unwrap(),
            CopilotDeviceLoginStatus::Pending
        );
        assert_eq!(
            provider.poll_device_login().await.unwrap(),
            CopilotDeviceLoginStatus::Authorized
        );
        provider.exchange().await.unwrap();
        assert_eq!(host.exchange_calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            provider.definition().authentication(),
            super::super::ProviderAccess::HostOwned { integration }
                if integration == "github-copilot"
        ));
        assert_eq!(
            provider.definition().catalog(),
            super::super::ProviderCatalogKind::Subscription
        );
        let diagnostic = super::super::ProviderDiagnostic::login_required(provider.definition());
        assert!(diagnostic.action().contains("embedding host"));
        assert!(!diagnostic.action().contains("--login"));
        assert!(!super::super::builtin_provider_definitions()
            .iter()
            .any(|definition| definition.id() == "github-copilot"));
    }

    #[tokio::test]
    async fn unavailable_and_oversized_discovery_never_add_models_to_picker() {
        let unavailable = Arc::new(FakeHost::with_state(
            Some(CopilotAvailabilityError::LoginRequired),
            vec![],
            vec![],
            vec![fake_model("chat-model", Protocol::OpenAiChat)],
        ));
        let provider = provider_for(Arc::clone(&unavailable), "https://api.example.test/");
        let mut catalog = ModelCatalog::default();
        assert_eq!(
            provider.register_models(&mut catalog).await.unwrap_err(),
            CopilotAvailabilityError::LoginRequired
        );
        assert_eq!(catalog.models().count(), 0);

        let oversized = (0..=MAX_DISCOVERED_MODELS)
            .map(|index| fake_model(&format!("model-{index}"), Protocol::OpenAiChat))
            .collect::<Vec<_>>();
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            oversized,
        ));
        let provider = provider_for(host, "https://api.example.test/");
        assert_eq!(
            provider.register_models(&mut catalog).await.unwrap_err(),
            CopilotAvailabilityError::TooManyModels
        );
        assert_eq!(catalog.models().count(), 0);
    }

    #[tokio::test]
    async fn unsupported_protocol_metadata_is_not_registered() {
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![fake_model("unsupported", Protocol::AnthropicMessages)],
        ));
        let provider = provider_for(host, "https://api.example.test/");
        let mut catalog = ModelCatalog::default();
        assert_eq!(
            provider.register_models(&mut catalog).await.unwrap_err(),
            CopilotAvailabilityError::UnsupportedModelProtocol
        );
        assert_eq!(catalog.models().count(), 0);
    }

    #[tokio::test]
    async fn endpoint_collisions_leave_discovered_models_out_of_the_catalog() {
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![
                fake_model("chat-model", Protocol::OpenAiChat),
                fake_model("responses-model", Protocol::OpenAiResponses),
            ],
        ));
        let provider = provider_for(host, "https://api.example.test/");
        let mut catalog = ModelCatalog::default();
        catalog
            .register_endpoint(ygg_ai::Endpoint {
                id: ygg_ai::EndpointId("github-copilot-chat".to_owned()),
                base_url: url::Url::parse("https://unrelated.example.test/").unwrap(),
                auth: ygg_ai::Auth::none(),
                default_headers: http::HeaderMap::new(),
                transport: ygg_ai::EndpointTransport::Http,
                runtime: ygg_ai::RequestRuntime::default(),
                timeout: Duration::from_secs(1),
            })
            .unwrap();

        assert_eq!(
            provider.register_models(&mut catalog).await.unwrap_err(),
            CopilotAvailabilityError::CatalogRegistrationUnavailable
        );
        assert_eq!(catalog.models().count(), 0);
        assert!(!catalog.has_endpoint(&ygg_ai::EndpointId("github-copilot-responses".to_owned())));
    }

    #[tokio::test]
    async fn authenticated_model_metadata_selects_each_declared_protocol_route() {
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![
                fake_model("chat-model", Protocol::OpenAiChat).with_display_name("Chat"),
                fake_model("responses-model", Protocol::OpenAiResponses)
                    .with_display_name("Responses"),
            ],
        ));
        let provider = provider_for(host, "https://api.example.test/");
        let mut catalog = ModelCatalog::default();
        provider.register_models(&mut catalog).await.unwrap();

        let chat = catalog
            .resolve(&ygg_ai::ModelId("github-copilot/chat-model".to_owned()))
            .unwrap();
        assert_eq!(chat.spec.protocol, Protocol::OpenAiChat);
        assert_eq!(chat.endpoint.id.0, "github-copilot-chat");
        let responses = catalog
            .resolve(&ygg_ai::ModelId(
                "github-copilot/responses-model".to_owned(),
            ))
            .unwrap();
        assert_eq!(responses.spec.protocol, Protocol::OpenAiResponses);
        assert_eq!(responses.endpoint.id.0, "github-copilot-responses");
    }

    #[tokio::test]
    async fn responses_model_metadata_uses_the_responses_codec() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("responses"))
            .and(header("authorization", format!("Bearer {PRIMARY_TOKEN}")))
            .and(header("x-copilot-session", DYNAMIC_TOKEN))
            .and(body_string_contains("responses-model"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!(
                        "../../fixtures/providers/github-copilot/responses_stream.sse"
                    ))
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![fake_model("responses-model", Protocol::OpenAiResponses)],
        ));
        let provider = provider_for(host, &server.uri());
        let mut catalog = ModelCatalog::default();
        provider.register_models(&mut catalog).await.unwrap();
        let model = catalog
            .resolve(&ygg_ai::ModelId(
                "github-copilot/responses-model".to_owned(),
            ))
            .unwrap();

        let events = AiClient::new()
            .stream(&model, text_request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::TextDelta { delta, .. } if delta == "hello from Responses")
        ));
        let usage = events.iter().find_map(|event| match event {
            StreamEvent::Usage(usage) => Some(usage),
            _ => None,
        });
        assert_eq!(usage.expect("usage fixture event").total_tokens, 12);
    }

    #[tokio::test]
    async fn fake_transport_covers_request_stream_usage_and_proactive_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("chat/completions"))
            .and(header("authorization", format!("Bearer {REFRESHED_TOKEN}")))
            .and(header("x-copilot-session", DYNAMIC_TOKEN))
            .and(body_string_contains("chat-model"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(include_str!(
                        "../../fixtures/providers/github-copilot/chat_stream.sse"
                    ))
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        // The initial one-second session is deliberately inside the 30-second
        // skew window. Request resolution must call the host refresh seam.
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(1))],
            vec![fake_session(REFRESHED_TOKEN, Duration::from_secs(600))],
            vec![fake_model("chat-model", Protocol::OpenAiChat)],
        ));
        let provider = provider_for(Arc::clone(&host), &server.uri());
        let mut catalog = ModelCatalog::default();
        provider.register_models(&mut catalog).await.unwrap();
        let model = catalog
            .resolve(&ygg_ai::ModelId("github-copilot/chat-model".to_owned()))
            .unwrap();

        let events = AiClient::new()
            .stream(&model, text_request())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(events.iter().any(
            |event| matches!(event, StreamEvent::TextDelta { delta, .. } if delta == "hello")
        ));
        let usage = events.iter().find_map(|event| match event {
            StreamEvent::Usage(usage) => Some(usage),
            _ => None,
        });
        assert_eq!(usage.expect("usage fixture event").total_tokens, 10);
        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        assert_eq!(host.exchange_calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fake_error_and_debug_output_redact_dynamic_and_primary_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("chat/completions"))
            .and(header("authorization", format!("Bearer {PRIMARY_TOKEN}")))
            .and(header("x-copilot-session", DYNAMIC_TOKEN))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!(
                        "../../fixtures/providers/github-copilot/error.json"
                    ))
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let session = fake_session(PRIMARY_TOKEN, Duration::from_secs(600));
        let host = Arc::new(FakeHost::with_state(
            None,
            vec![session.clone()],
            vec![],
            vec![fake_model("chat-model", Protocol::OpenAiChat)],
        ));
        let provider = provider_for(Arc::clone(&host), &server.uri());
        let mut catalog = ModelCatalog::default();
        provider.register_models(&mut catalog).await.unwrap();
        let model = catalog
            .resolve(&ygg_ai::ModelId("github-copilot/chat-model".to_owned()))
            .unwrap();

        let error = match AiClient::new().stream(&model, text_request()).await {
            Ok(_) => panic!("fake transport should reject the request"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:?} {error}");
        assert!(diagnostic.contains("[REDACTED]"));
        for value in [PRIMARY_TOKEN, DYNAMIC_TOKEN] {
            assert!(!diagnostic.contains(value));
            assert!(!format!("{provider:?}{:?}{session:?}", model.endpoint).contains(value));
        }
        let resolved = provider.resolver.resolve().await.unwrap();
        assert!(resolved.extra_headers["x-copilot-session"].is_sensitive());
        assert!(!format!("{:?}", provider.definition()).contains(PRIMARY_TOKEN));
    }

    #[tokio::test]
    async fn dropping_a_copilot_stream_closes_the_fake_transport() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let disconnected = Arc::new(AtomicBool::new(false));
        let observed_disconnect = Arc::clone(&disconnected);
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let header_end = loop {
                let Ok(read) = socket.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(index) = request.windows(4).position(|value| value == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let Ok(read) = socket.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
            }

            let event = "data: {\"id\":\"copilot-drop\",\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: keep-alive\r\n\r\n{:x}\r\n{}\r\n",
                event.len(),
                event
            );
            if socket.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            let mut tail = [0_u8; 128];
            match socket.read(&mut tail).await {
                Ok(0) | Err(_) => observed_disconnect.store(true, Ordering::SeqCst),
                Ok(_) => {}
            }
        });

        let host = Arc::new(FakeHost::with_state(
            None,
            vec![fake_session(PRIMARY_TOKEN, Duration::from_secs(600))],
            vec![],
            vec![fake_model("chat-model", Protocol::OpenAiChat)],
        ));
        let provider = provider_for(host, &format!("http://{address}/"));
        let mut catalog = ModelCatalog::default();
        provider.register_models(&mut catalog).await.unwrap();
        let model = catalog
            .resolve(&ygg_ai::ModelId("github-copilot/chat-model".to_owned()))
            .unwrap();
        let mut stream = AiClient::new()
            .stream(&model, text_request())
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamEvent::Started { .. }
        ));
        drop(stream);

        for _ in 0..20 {
            if disconnected.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if !disconnected.load(Ordering::SeqCst) {
            server.abort();
        }
        assert!(
            disconnected.load(Ordering::SeqCst),
            "dropping the provider stream must close the fake transport"
        );
    }

    #[test]
    fn rejects_secret_bearing_endpoint_device_and_session_inputs() {
        assert!(CopilotEndpoint::new(url::Url::parse("http://example.test/").unwrap()).is_err());
        assert!(
            CopilotEndpoint::new(url::Url::parse("https://user:token@example.test/").unwrap())
                .is_err()
        );
        assert!(CopilotEndpoint::new(
            url::Url::parse("https://example.test/?token=not-allowed").unwrap()
        )
        .is_err());
        assert!(CopilotDeviceLogin::new(
            url::Url::parse("https://github.example.test/device?token=not-allowed").unwrap(),
            "DEVICE-CODE",
            Duration::from_secs(60),
            Duration::from_secs(5),
        )
        .is_err());
        let hidden_url_segment = "credential-in-host-owned-path";
        assert!(CopilotEndpoint::new(
            url::Url::parse(&format!("https://example.test/{hidden_url_segment}/")).unwrap(),
        )
        .is_err());
        let endpoint =
            CopilotEndpoint::new(url::Url::parse("https://example.test/").unwrap()).unwrap();
        let login = CopilotDeviceLogin::new(
            url::Url::parse(&format!("https://github.example.test/{hidden_url_segment}")).unwrap(),
            "DEVICE-CODE",
            Duration::from_secs(60),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!format!("{endpoint:?}{login:?}").contains(hidden_url_segment));
        assert!(CopilotSession::new(
            "   ",
            CopilotCredentialScheme::Bearer,
            vec![],
            Duration::from_secs(60),
        )
        .is_err());
        assert!(CopilotSession::new(
            "contains\r\nheader-injection",
            CopilotCredentialScheme::Bearer,
            vec![],
            Duration::from_secs(60),
        )
        .is_err());
        assert!(CopilotSession::new(
            "x".repeat(MAX_SESSION_CREDENTIAL_BYTES + 1),
            CopilotCredentialScheme::Bearer,
            vec![],
            Duration::from_secs(60),
        )
        .is_err());
        assert!(CopilotDynamicHeader::new(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("not-allowed"),
        )
        .is_err());
        assert!(CopilotDynamicHeader::new(
            http::header::PROXY_AUTHORIZATION,
            http::HeaderValue::from_static("not-allowed"),
        )
        .is_err());
        assert!(CopilotDynamicHeader::new(
            http::header::TE,
            http::HeaderValue::from_static("trailers"),
        )
        .is_err());
    }
}
