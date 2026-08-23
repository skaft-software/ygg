//! Loopback-only HTTP and WebSocket transport for first-party graphical clients.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path as FilePath, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{to_bytes, Body};
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, RawQuery, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    COOKIE, ETAG, HOST, LOCATION, ORIGIN, RANGE, REFERRER_POLICY, SET_COOKIE, TRANSFER_ENCODING,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot, watch, Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::embedded_web::WebBundle;
use crate::{
    AckDisposition, AttachmentError, CommandId, DeviceId, FileEntryId, GoalAction, GoalState,
    GoalStore, GoalStoreError, HostCommandEnvelope, HostService, ProjectFileSystemError, ProjectId,
    ProtocolValidation, PtyAttachment, PtyError, PtyEvent, PtyExit, PtyManager, PtyOpenRequest,
    SanitizedError, ServiceError, SessionCommand, SessionCommandEnvelope, SessionCursor, SessionId,
    SessionSupervisor, SupervisorError, TerminalConfig, MAX_ATTACHMENT_FILE_BYTES,
    MAX_COMMAND_BYTES, MAX_DOCUMENT_FILE_BYTES, MAX_PROJECT_FILE_PATH_BYTES,
    MAX_PROJECT_FILE_WRITE_BYTES, MAX_PTY_INPUT_BYTES, PROTOCOL_VERSION,
};
use crate::{CompanionControl, CompanionControlError};
use ygg_companion_protocol::{PairingDecisionRequest, PairingInvitation};

const MAX_PATH_BYTES: usize = 8 * 1024;
const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 8 * 1024;
// JSON may escape every input byte, while the decoded PTY input remains
// bounded by `MAX_PTY_INPUT_BYTES` in the manager.
const MAX_TERMINAL_WEBSOCKET_MESSAGE_BYTES: usize = MAX_PTY_INPUT_BYTES * 8;
const RATE_LIMIT_REQUESTS: usize = 240;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_ATTACHMENT_UPLOADS: usize = 4;
const MAX_CONCURRENT_SESSION_EXPORTS: usize = 1;
const MAX_COMPANION_ADMIN_REQUEST_BYTES: usize = 1024;
const COMPANION_ADMIN_BODY_TIMEOUT: Duration = Duration::from_secs(5);
const WEBSOCKET_CLOSE_GRACE: Duration = Duration::from_secs(1);
const WEBSOCKET_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
// JSON escapes can expand each accepted UTF-8 byte to six bytes.
const MAX_PROJECT_FILE_WRITE_REQUEST_BYTES: usize =
    MAX_PROJECT_FILE_WRITE_BYTES * 6 + MAX_PROJECT_FILE_PATH_BYTES * 6 + 1024;
const X_YGG_WEB_BUNDLE: HeaderName = HeaderName::from_static("x-ygg-web-bundle");
const X_YGG_GOAL_REVISION: HeaderName = HeaderName::from_static("x-ygg-goal-revision");
static NEXT_HTTP_GOAL_COMMAND_ID: AtomicU64 = AtomicU64::new(1);

/// Loopback listener configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopbackConfig {
    /// Requested TCP port. Zero asks the operating system for a free port.
    pub port: u16,
    /// Optional built graphical-shell directory.
    pub web_root: Option<PathBuf>,
    /// Optional local terminal configuration. Omit it when process execution
    /// is disabled by the host's sandbox policy.
    pub terminal: Option<TerminalConfig>,
    /// Private root directory for persistent per-session goals.
    pub goal_store_root: PathBuf,
}

/// Transport startup or task failure.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Initial host bootstrap failed.
    #[error("host startup failed")]
    Host(#[from] SupervisorError),
    /// Terminal setup failed.
    #[error("terminal setup failed")]
    Terminal(#[from] PtyError),
    /// The loopback listener could not be created.
    #[error("loopback listener failed")]
    Io(#[from] std::io::Error),
    /// Goal storage could not be initialized.
    #[error("goal storage startup failed")]
    Goal(#[from] GoalStoreError),
    /// The loopback server task ended unexpectedly.
    #[error("loopback server task failed")]
    Task(#[from] tokio::task::JoinError),
    /// An upgraded WebSocket did not stop within the shutdown bound.
    #[error("loopback WebSocket shutdown timed out")]
    WebSocketShutdown,
}

/// Authenticated application surface shared with the companion QUIC adapter.
#[derive(Clone)]
pub(crate) struct CompanionApplication {
    pub(crate) router: Router,
    pub(crate) subscribe_events:
        Arc<dyn Fn() -> broadcast::Receiver<crate::HostStreamEvent> + Send + Sync>,
}

/// Principal established by a transport boundary before application dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TransportPrincipal {
    LoopbackOwner,
    Paired { device_id: DeviceId },
}

struct UpgradeTracker {
    state: StdMutex<UpgradeTrackerState>,
    shutdown: watch::Sender<bool>,
    idle: Notify,
}

#[derive(Default)]
struct UpgradeTrackerState {
    closed: bool,
    active: usize,
}

struct UpgradeLease {
    tracker: Arc<UpgradeTracker>,
    shutdown: watch::Receiver<bool>,
}

impl UpgradeTracker {
    fn new() -> Arc<Self> {
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self {
            state: StdMutex::new(UpgradeTrackerState::default()),
            shutdown,
            idle: Notify::new(),
        })
    }

    fn register(self: &Arc<Self>) -> Option<UpgradeLease> {
        let mut state = self.state.lock().expect("upgrade tracker poisoned");
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(UpgradeLease {
            tracker: self.clone(),
            shutdown: self.shutdown.subscribe(),
        })
    }

    fn close(&self) {
        let should_signal = {
            let mut state = self.state.lock().expect("upgrade tracker poisoned");
            if state.closed {
                false
            } else {
                state.closed = true;
                true
            }
        };
        if should_signal {
            self.shutdown.send_replace(true);
        }
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if self.state.lock().expect("upgrade tracker poisoned").active == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl UpgradeLease {
    async fn run<F>(mut self, operation: F)
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(operation);
        tokio::select! {
            biased;
            _ = upgrade_shutdown(&mut self.shutdown) => {}
            _ = &mut operation => {}
        }
    }

    async fn run_with_shutdown_grace<F>(mut self, operation: F, grace: Duration)
    where
        F: Future<Output = ()>,
    {
        tokio::pin!(operation);
        tokio::select! {
            biased;
            _ = upgrade_shutdown(&mut self.shutdown) => {
                let _ = tokio::time::timeout(grace, &mut operation).await;
            }
            _ = &mut operation => {}
        }
    }
}

impl Drop for UpgradeLease {
    fn drop(&mut self) {
        let idle = {
            let mut state = self.tracker.state.lock().expect("upgrade tracker poisoned");
            debug_assert!(state.active > 0);
            state.active -= 1;
            state.active == 0
        };
        if idle {
            self.tracker.idle.notify_waiters();
        }
    }
}

async fn upgrade_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow_and_update() {
            return;
        }
    }
}

/// Running loopback server.
pub struct LoopbackServer {
    address: SocketAddr,
    launch_token: String,
    terminal: Option<PtyManager>,
    companion_application: CompanionApplication,
    upgrades: Arc<UpgradeTracker>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

impl LoopbackServer {
    /// Starts a loopback server without allocating a session.
    ///
    /// Each default root-client bootstrap creates its own provisional session.
    /// A bootstrap carrying an explicit session id restores that session, while
    /// an inventory-only bootstrap creates and selects no session.
    pub async fn start<H: HostService>(
        supervisor: Arc<SessionSupervisor<H>>,
        config: LoopbackConfig,
    ) -> Result<Self, TransportError> {
        Self::start_inner(supervisor, config, None).await
    }

    /// Starts loopback with owner-only companion administration enabled.
    pub async fn start_with_companion<H: HostService>(
        supervisor: Arc<SessionSupervisor<H>>,
        config: LoopbackConfig,
        companion: CompanionControl,
    ) -> Result<Self, TransportError> {
        Self::start_inner(supervisor, config, Some(companion)).await
    }

    async fn start_inner<H: HostService>(
        supervisor: Arc<SessionSupervisor<H>>,
        config: LoopbackConfig,
        companion: Option<CompanionControl>,
    ) -> Result<Self, TransportError> {
        let web_bundle = match config.web_root.as_deref() {
            Some(root) => WebBundle::from_root(root)?,
            None => WebBundle::embedded()?,
        };
        let terminal = config.terminal.map(PtyManager::new).transpose()?;
        let goal_store = GoalStore::open(&config.goal_store_root)?;
        let listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            config.port,
        ))
        .await?;
        let address = listener.local_addr()?;
        debug_assert!(address.ip().is_loopback());
        let launch_token = random_hex(32)?;
        let auth = TransportAuth {
            launch_token: StdMutex::new(Some(launch_token.clone())),
            cookie_name: format!("ygg_{}", random_hex(12)?),
            cookie_value: random_hex(32)?,
        };
        let upgrades = UpgradeTracker::new();

        let state = Arc::new(TransportState {
            supervisor,
            auth,
            allowed_authorities: AllowedAuthorities::new(address),
            rate_limiter: RateLimiter::default(),
            attachment_uploads: Arc::new(Semaphore::new(MAX_CONCURRENT_ATTACHMENT_UPLOADS)),
            session_exports: Arc::new(Semaphore::new(MAX_CONCURRENT_SESSION_EXPORTS)),
            terminal: terminal.clone(),
            goal_store,
            web_bundle,
            companion,
            upgrades: upgrades.clone(),
        });
        let event_state = Arc::clone(&state);
        let companion_application = CompanionApplication {
            router: build_application_router(Arc::clone(&state)),
            subscribe_events: Arc::new(move || event_state.supervisor.subscribe_events()),
        };
        let router = build_router(state);
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_receiver.await;
                })
                .await
        });
        Ok(Self {
            address,
            launch_token,
            terminal,
            companion_application,
            upgrades,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub(crate) fn companion_application(&self) -> CompanionApplication {
        self.companion_application.clone()
    }

    /// Exact bound loopback address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Same-origin browser URL.
    pub fn url(&self) -> String {
        format!("http://{}/", self.address)
    }

    /// One-use browser launch URL. Callers must not persist it or include it in
    /// logs; a successful exchange immediately redirects to [`Self::url`].
    pub fn launch_url(&self) -> String {
        format!("http://{}/__ygg/launch/{}", self.address, self.launch_token)
    }

    /// Waits until the listener exits.
    pub async fn wait(mut self) -> Result<(), TransportError> {
        let task_result = self.await_task().await;
        self.shutdown.take();
        self.stop_terminal();
        self.upgrades.close();
        let upgrade_result = self.await_upgrades().await;
        task_result?;
        upgrade_result
    }

    /// Requests graceful shutdown and waits for completion.
    pub async fn shutdown(mut self) -> Result<(), TransportError> {
        self.stop_terminal();
        self.upgrades.close();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let task_result = self.await_task().await;
        let upgrade_result = self.await_upgrades().await;
        task_result?;
        upgrade_result
    }

    async fn await_task(&mut self) -> Result<(), TransportError> {
        match self.task.take() {
            Some(task) => match task.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(TransportError::Io(error)),
                Err(error) => Err(TransportError::Task(error)),
            },
            None => Ok(()),
        }
    }

    async fn await_upgrades(&self) -> Result<(), TransportError> {
        tokio::time::timeout(WEBSOCKET_SHUTDOWN_TIMEOUT, self.upgrades.wait_idle())
            .await
            .map_err(|_| TransportError::WebSocketShutdown)
    }

    fn stop_terminal(&mut self) {
        if let Some(terminal) = self.terminal.take() {
            terminal.shutdown();
        }
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.stop_terminal();
        self.upgrades.close();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

struct TransportState<H: HostService> {
    supervisor: Arc<SessionSupervisor<H>>,
    auth: TransportAuth,
    allowed_authorities: AllowedAuthorities,
    rate_limiter: RateLimiter,
    attachment_uploads: Arc<Semaphore>,
    session_exports: Arc<Semaphore>,
    terminal: Option<PtyManager>,
    goal_store: GoalStore,
    web_bundle: WebBundle,
    companion: Option<CompanionControl>,
    upgrades: Arc<UpgradeTracker>,
}

struct TransportAuth {
    launch_token: StdMutex<Option<String>>,
    cookie_name: String,
    cookie_value: String,
}

impl TransportAuth {
    fn exchange(&self, candidate: &str) -> bool {
        let mut launch_token = self
            .launch_token
            .lock()
            .expect("transport auth token poisoned");
        let accepted = launch_token
            .as_deref()
            .is_some_and(|expected| constant_time_eq(expected.as_bytes(), candidate.as_bytes()));
        if accepted {
            launch_token.take();
        }
        accepted
    }

    fn allows_cookie(&self, headers: &HeaderMap) -> bool {
        headers.get_all(COOKIE).iter().any(|header| {
            header.to_str().ok().is_some_and(|header| {
                header.split(';').any(|pair| {
                    let Some((name, value)) = pair.trim().split_once('=') else {
                        return false;
                    };
                    constant_time_eq(name.as_bytes(), self.cookie_name.as_bytes())
                        & constant_time_eq(value.as_bytes(), self.cookie_value.as_bytes())
                })
            })
        })
    }

    fn set_cookie_value(&self) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
        HeaderValue::from_str(&format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict",
            self.cookie_name, self.cookie_value
        ))
    }
}

#[derive(Clone)]
struct AllowedAuthorities {
    host_ipv4: String,
    host_localhost: String,
    origin_ipv4: String,
    origin_localhost: String,
}

impl AllowedAuthorities {
    fn new(address: SocketAddr) -> Self {
        let port = address.port();
        Self {
            host_ipv4: format!("127.0.0.1:{port}"),
            host_localhost: format!("localhost:{port}"),
            origin_ipv4: format!("http://127.0.0.1:{port}"),
            origin_localhost: format!("http://localhost:{port}"),
        }
    }

    fn allows_host(&self, value: &str) -> bool {
        value == self.host_ipv4 || value.eq_ignore_ascii_case(&self.host_localhost)
    }

    fn allows_origin(&self, value: &str) -> bool {
        value == self.origin_ipv4 || value.eq_ignore_ascii_case(&self.origin_localhost)
    }
}

#[derive(Default)]
struct RateLimiter {
    accepted: StdMutex<RateLimitState>,
}

#[derive(Default)]
struct RateLimitState {
    owner: VecDeque<Instant>,
    companions: BTreeMap<DeviceId, VecDeque<Instant>>,
    unattributed: VecDeque<Instant>,
}

impl RateLimiter {
    fn admit(&self, principal: Option<&Extension<TransportPrincipal>>) -> bool {
        let now = Instant::now();
        let mut accepted = self.accepted.lock().expect("rate limiter poisoned");
        let accepted = match principal {
            Some(Extension(TransportPrincipal::LoopbackOwner)) => &mut accepted.owner,
            Some(Extension(TransportPrincipal::Paired { device_id })) => {
                accepted.companions.retain(|_, requests| {
                    expire_rate_limit_requests(requests, now);
                    !requests.is_empty()
                });
                accepted.companions.entry(device_id.clone()).or_default()
            }
            None => &mut accepted.unattributed,
        };
        expire_rate_limit_requests(accepted, now);
        if accepted.len() >= RATE_LIMIT_REQUESTS {
            return false;
        }
        accepted.push_back(now);
        true
    }

    fn admit_remote_read(&self, principal: Option<&Extension<TransportPrincipal>>) -> bool {
        match principal {
            Some(Extension(TransportPrincipal::LoopbackOwner)) => true,
            Some(Extension(TransportPrincipal::Paired { .. })) | None => self.admit(principal),
        }
    }
}

fn expire_rate_limit_requests(accepted: &mut VecDeque<Instant>, now: Instant) {
    while accepted
        .front()
        .is_some_and(|instant| now.duration_since(*instant) >= RATE_LIMIT_WINDOW)
    {
        accepted.pop_front();
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    protocol: u16,
    error: SanitizedError,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapQuery {
    #[serde(default)]
    selected_session_id: Option<String>,
    #[serde(default)]
    inventory_only: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplayQuery {
    actor_generation: u64,
    sequence: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AttachmentUploadQuery {
    display_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DocumentUploadQuery {
    display_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustedFileListQuery {
    #[serde(default = "default_file_list_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustedFileSearchQuery {
    query: String,
    #[serde(default = "default_file_search_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectFileTreeQuery {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectFileReadQuery {
    path: String,
    #[serde(default)]
    start_line: Option<u32>,
    #[serde(default)]
    end_line: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectFileSearchQuery {
    query: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectFileWriteRequest {
    path: String,
    content: String,
    expected_sha256: String,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageStatsQuery {
    period: crate::UsagePeriod,
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum TerminalClientMessage {
    Open {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        owner_key: Option<String>,
        cols: u16,
        rows: u16,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
    },
    Input {
        id: String,
        data: String,
    },
    Detach {
        id: String,
    },
}

#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum TerminalServerMessage {
    Opened {
        id: String,
        owner_key: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        replay: Option<String>,
    },
    Output {
        id: String,
        data: String,
    },
    Exit {
        id: String,
        exit_code: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    Error {
        message: &'static str,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GoalRequest {
    #[serde(default)]
    objective: Option<String>,
    #[serde(default)]
    turn_budget: Option<u32>,
    #[serde(default)]
    action: Option<GoalAction>,
}

fn default_file_list_limit() -> usize {
    200
}

fn default_file_search_limit() -> usize {
    50
}

fn build_router<H: HostService>(state: Arc<TransportState<H>>) -> Router {
    build_application_router(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(state, secure_request::<H>))
}

fn build_application_router<H: HostService>(state: Arc<TransportState<H>>) -> Router {
    Router::new()
        .route("/", get(index::<H>))
        .route("/__ygg/launch/{token}", get(exchange_launch_token::<H>))
        .route("/api/v1/bootstrap", get(bootstrap::<H>))
        .route("/api/v1/projects", get(project_catalog::<H>))
        .route("/api/v1/usage/stats", get(usage_stats::<H>))
        .route("/api/v1/usage/lifetime", get(usage_lifetime::<H>))
        .route("/api/v1/usage/activity", get(usage_activity::<H>))
        .route(
            "/api/v1/projects/{project_id}/context",
            get(repository_context::<H>),
        )
        .route("/api/v1/sessions/{session_id}", get(session_snapshot::<H>))
        .route(
            "/api/v1/sessions/{session_id}/commands",
            get(command_discovery::<H>),
        )
        .route(
            "/api/v1/sessions/{session_id}/goal",
            get(session_goal::<H>)
                .post(update_goal::<H>)
                .layer(DefaultBodyLimit::max(MAX_COMMAND_BYTES)),
        )
        .route(
            "/api/v1/sessions/{session_id}/replay",
            get(session_replay::<H>),
        )
        .route(
            "/api/v1/sessions/{session_id}/export",
            get(session_export::<H>),
        )
        .route(
            "/api/v1/commands/host",
            post(host_command::<H>).layer(DefaultBodyLimit::max(MAX_COMMAND_BYTES)),
        )
        .route(
            "/api/v1/commands/session",
            post(session_command::<H>).layer(DefaultBodyLimit::max(MAX_COMMAND_BYTES)),
        )
        .route(
            "/api/v1/search",
            post(transcript_search::<H>).layer(DefaultBodyLimit::max(MAX_COMMAND_BYTES)),
        )
        .route(
            "/api/v1/attachments",
            post(ingest_attachment::<H>)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_FILE_BYTES + 1)),
        )
        .route("/api/v1/attachments/{handle}", get(attachment_content::<H>))
        .route(
            "/api/v1/sessions/{session_id}/documents",
            get(list_documents::<H>)
                .post(ingest_document::<H>)
                .layer(DefaultBodyLimit::max(MAX_DOCUMENT_FILE_BYTES + 1)),
        )
        .route(
            "/api/v1/projects/{project_id}/files",
            get(list_trusted_files::<H>),
        )
        .route(
            "/api/v1/projects/{project_id}/files/search",
            get(search_trusted_files::<H>),
        )
        .route(
            "/api/v1/projects/{project_id}/files/{entry_id}",
            get(read_trusted_file::<H>),
        )
        .route("/api/v1/fs/{project_id}/tree", get(project_file_tree::<H>))
        .route("/api/v1/fs/{project_id}/read", get(read_project_file::<H>))
        .route(
            "/api/v1/fs/{project_id}/search",
            get(search_project_files::<H>),
        )
        .route(
            "/api/v1/fs/{project_id}/write",
            post(write_project_file::<H>)
                .layer(DefaultBodyLimit::max(MAX_PROJECT_FILE_WRITE_REQUEST_BYTES)),
        )
        .route(
            "/api/v1/sessions/{session_id}/resources/{handle}",
            get(resource_content::<H>),
        )
        .route("/api/v1/events", any(events_socket::<H>))
        .route("/api/v1/terminal", any(terminal_socket::<H>))
        .route("/api/v1/companion/devices", get(companion_devices::<H>))
        .route(
            "/api/v1/companion/pairing/open",
            post(companion_pairing_open::<H>),
        )
        .route(
            "/api/v1/companion/pairing/state",
            get(companion_pairing_state::<H>),
        )
        .route(
            "/api/v1/companion/pairing/requests/{request_id}/decision",
            post(companion_pairing_decision::<H>)
                .layer(DefaultBodyLimit::max(MAX_COMPANION_ADMIN_REQUEST_BYTES)),
        )
        .route(
            "/api/v1/companion/pairing",
            axum::routing::delete(companion_pairing_close::<H>),
        )
        .route(
            "/api/v1/companion/devices/{device_id}",
            axum::routing::delete(companion_device_revoke::<H>),
        )
        .route("/{*asset}", get(static_asset::<H>))
        .fallback(not_found)
        .with_state(state)
}

fn owner_companion<H: HostService>(
    state: &TransportState<H>,
    principal: Option<Extension<TransportPrincipal>>,
) -> Result<CompanionControl, Box<Response>> {
    if !matches!(
        principal,
        Some(Extension(TransportPrincipal::LoopbackOwner))
    ) {
        return Err(Box::new(authentication_required()));
    }
    state.companion.clone().ok_or_else(|| {
        Box::new(error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "Companion mode is not enabled on this host.",
            ),
        ))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyCompanionAdminRequest {}

async fn companion_admin_body(request: Request, maximum: usize) -> Result<Bytes, Response> {
    if request.uri().query().is_some() {
        return Err(invalid_request());
    }
    let expected = declared_content_length(request.headers()).map_err(|()| invalid_request())?;
    if maximum == 0 && request.headers().contains_key(TRANSFER_ENCODING) {
        return Err(invalid_request());
    }
    if expected.is_some_and(|length| length > maximum) {
        return Err(error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            SanitizedError::public(
                crate::ErrorCode::PayloadTooLarge,
                "The companion administration request is too large.",
            ),
        ));
    }
    let body = match tokio::time::timeout(
        COMPANION_ADMIN_BODY_TIMEOUT,
        to_bytes(request.into_body(), maximum),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return Err(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                SanitizedError::public(
                    crate::ErrorCode::PayloadTooLarge,
                    "The companion administration request is too large.",
                ),
            ));
        }
        Err(_) => {
            return Err(error_response(
                StatusCode::REQUEST_TIMEOUT,
                SanitizedError::public(
                    crate::ErrorCode::Unavailable,
                    "The companion administration request body timed out.",
                )
                .with_retryable(true),
            ));
        }
    };
    if expected.is_some_and(|length| length != body.len()) {
        return Err(invalid_request());
    }
    Ok(body)
}

async fn companion_devices<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    if let Err(response) = companion_admin_body(request, 0).await {
        return response;
    }
    Json(control.catalog().devices).into_response()
}

async fn companion_pairing_state<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    if let Err(response) = companion_admin_body(request, 0).await {
        return response;
    }
    Json(control.catalog()).into_response()
}

async fn companion_pairing_open<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    let body = match companion_admin_body(request, MAX_COMPANION_ADMIN_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    if serde_json::from_slice::<EmptyCompanionAdminRequest>(&body).is_err() {
        return invalid_request();
    }
    match control.open_pairing() {
        Ok(invitation) => Json::<PairingInvitation>(invitation).into_response(),
        Err(error) => companion_control_error_response(error),
    }
}

async fn companion_pairing_decision<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(request_id): Path<String>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    let body = match companion_admin_body(request, MAX_COMPANION_ADMIN_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    if request_id.is_empty() || request_id.len() > 128 {
        return invalid_request();
    }
    let payload = match serde_json::from_slice::<PairingDecisionRequest>(&body) {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    match control.decide_pairing(&request_id, payload.decision) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => companion_control_error_response(error),
    }
}

async fn companion_pairing_close<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    if let Err(response) = companion_admin_body(request, 0).await {
        return response;
    }
    control.close_pairing();
    StatusCode::NO_CONTENT.into_response()
}

async fn companion_device_revoke<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(device_id): Path<String>,
    request: Request,
) -> Response {
    let control = match owner_companion(state.as_ref(), principal) {
        Ok(control) => control,
        Err(response) => return *response,
    };
    if let Err(response) = companion_admin_body(request, 0).await {
        return response;
    }
    if DeviceId::new(device_id.clone()).is_err() {
        return invalid_request();
    }
    match control.revoke_device(&device_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => companion_control_error_response(error),
    }
}

fn companion_control_error_response(error: CompanionControlError) -> Response {
    match error {
        CompanionControlError::Unavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "Companion networking is unavailable.",
            )
            .with_retryable(true),
        ),
        CompanionControlError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The companion record was not found.",
            ),
        ),
        CompanionControlError::Capacity => error_response(
            StatusCode::TOO_MANY_REQUESTS,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "Companion pairing is at capacity.",
            ),
        ),
        CompanionControlError::Conflict => error_response(
            StatusCode::CONFLICT,
            SanitizedError::public(
                crate::ErrorCode::InvalidCommand,
                "The pairing state changed before this request completed.",
            ),
        ),
        CompanionControlError::Storage => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

async fn ingest_document<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
    query: Result<Query<DocumentUploadQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.document_ingest_supported() {
        return service_error_response(ServiceError::Unavailable);
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    let media_type = match headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    {
        Some("text/plain" | "text/markdown" | "application/pdf") => headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .unwrap_or_default()
            .to_owned(),
        _ => {
            return error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                SanitizedError::public(
                    crate::ErrorCode::InvalidCommand,
                    "Only UTF-8 text, Markdown, and ordinary PDF documents are accepted.",
                ),
            )
        }
    };
    let mut stream = body.into_data_stream();
    let mut bytes = BytesMut::with_capacity(
        headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default()
            .min(MAX_DOCUMENT_FILE_BYTES),
    );
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return invalid_request(),
        };
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_DOCUMENT_FILE_BYTES)
        {
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                SanitizedError::public(
                    crate::ErrorCode::PayloadTooLarge,
                    "The document exceeds the host limit.",
                ),
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    match state
        .supervisor
        .ingest_document(
            &session_id,
            &query.display_name,
            &media_type,
            bytes.freeze(),
        )
        .await
    {
        Ok(reference) => Json(reference).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn list_documents<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.list_documents(&session_id).await {
        Ok(documents) => Json(documents).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn list_trusted_files<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    query: Result<Query<TrustedFileListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.trusted_project_files_supported() {
        return service_error_response(ServiceError::Unavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    let summary = match state.supervisor.trusted_file_index(&project_id).await {
        Ok(summary) => summary,
        Err(error) => return service_error_response(error),
    };
    match state
        .supervisor
        .list_trusted_files(&project_id, query.limit)
        .await
    {
        Ok(files) => Json(serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "summary": summary,
            "files": files,
        }))
        .into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn repository_context<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.repository_context_supported() {
        return service_error_response(ServiceError::Unavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.repository_context(&project_id).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn search_trusted_files<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    query: Result<Query<TrustedFileSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .search_trusted_files(&project_id, &query.query, query.limit)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn read_trusted_file<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path((raw_project_id, raw_entry_id)): Path<(String, String)>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let entry_id = match FileEntryId::parse(raw_entry_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .read_trusted_file(&project_id, &entry_id)
        .await
    {
        Ok(file) => Json(file).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn project_file_tree<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    query: Result<Query<ProjectFileTreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.project_file_browser_supported() {
        return project_file_system_error_response(ProjectFileSystemError::Unavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .project_file_tree(&project_id, &query.path)
        .await
    {
        Ok(tree) => Json(tree).into_response(),
        Err(error) => project_file_system_error_response(error),
    }
}

async fn read_project_file<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    query: Result<Query<ProjectFileReadQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.project_file_browser_supported() {
        return project_file_system_error_response(ProjectFileSystemError::Unavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .read_project_file(&project_id, &query.path, query.start_line, query.end_line)
        .await
    {
        Ok(file) => Json(file).into_response(),
        Err(error) => project_file_system_error_response(error),
    }
}

async fn search_project_files<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    query: Result<Query<ProjectFileSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.project_file_browser_supported() {
        return project_file_system_error_response(ProjectFileSystemError::Unavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .search_project_files(&project_id, &query.query)
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => project_file_system_error_response(error),
    }
}

async fn write_project_file<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_project_id): Path<String>,
    payload: Result<Json<ProjectFileWriteRequest>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.project_file_write_supported() {
        return project_file_system_error_response(ProjectFileSystemError::WriteUnavailable);
    }
    let project_id = match ProjectId::new(raw_project_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .write_project_file(
            &project_id,
            &request.path,
            &request.content,
            &request.expected_sha256,
            request.force,
        )
        .await
    {
        Ok(file) => Json(file).into_response(),
        Err(error) => project_file_system_error_response(error),
    }
}

async fn ingest_attachment<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    query: Result<Query<AttachmentUploadQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    let Some(policy) = state.supervisor.attachment_policy() else {
        return attachment_error_response(AttachmentError::Unavailable);
    };
    let media_type = match headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value)
            if policy
                .accepted_media_types
                .iter()
                .any(|accepted| accepted == value) =>
        {
            value.to_owned()
        }
        _ => return attachment_error_response(AttachmentError::UnsupportedMediaType),
    };
    let permit = match Arc::clone(&state.attachment_uploads).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return attachment_error_response(AttachmentError::QuotaExceeded),
    };
    let mut stream = body.into_data_stream();
    let mut bytes = BytesMut::with_capacity(
        headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_default()
            .min(policy.max_file_bytes as usize),
    );
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => return invalid_request(),
        };
        if bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > policy.max_file_bytes as usize)
        {
            return attachment_error_response(AttachmentError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    drop(permit);
    match state
        .supervisor
        .ingest_attachment(&query.display_name, &media_type, bytes.freeze())
        .await
    {
        Ok(reference) => Json(reference).into_response(),
        Err(error) => attachment_error_response(error),
    }
}

async fn attachment_content<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(handle): Path<String>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    match state.supervisor.attachment_content(&handle).await {
        Ok(attachment) => {
            let content_type = match HeaderValue::from_str(&attachment.reference.media_type) {
                Ok(value) => value,
                Err(_) => return attachment_error_response(AttachmentError::Storage),
            };
            let content_length = match HeaderValue::from_str(&attachment.bytes.len().to_string()) {
                Ok(value) => value,
                Err(_) => return attachment_error_response(AttachmentError::Storage),
            };
            let disposition = match inline_content_disposition(&attachment.reference.display_name) {
                Ok(value) => value,
                Err(_) => return attachment_error_response(AttachmentError::Storage),
            };
            let etag = match HeaderValue::from_str(&format!("\"{}\"", attachment.sha256)) {
                Ok(value) => value,
                Err(_) => return attachment_error_response(AttachmentError::Storage),
            };
            (
                [
                    (CONTENT_TYPE, content_type),
                    (CONTENT_LENGTH, content_length),
                    (CONTENT_DISPOSITION, disposition),
                    (ETAG, etag),
                    (
                        CACHE_CONTROL,
                        HeaderValue::from_static("private, max-age=31536000, immutable"),
                    ),
                ],
                attachment.bytes,
            )
                .into_response()
        }
        Err(error) => attachment_error_response(error),
    }
}

async fn resource_content<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path((raw_session_id, handle)): Path<(String, String)>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state
        .supervisor
        .resource_content(&session_id, &handle)
        .await
    {
        Ok(resource) => {
            let (served_media_type, inline) = safe_inline_resource(&resource);
            let content_type = match HeaderValue::from_str(served_media_type) {
                Ok(value) => value,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        SanitizedError::internal(),
                    )
                }
            };
            let content_length = match HeaderValue::from_str(&resource.bytes.len().to_string()) {
                Ok(value) => value,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        SanitizedError::internal(),
                    )
                }
            };
            let disposition = match if inline {
                inline_content_disposition(&resource.display_name)
            } else {
                attachment_content_disposition(&resource.display_name)
            } {
                Ok(value) => value,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        SanitizedError::internal(),
                    )
                }
            };
            (
                [
                    (CONTENT_TYPE, content_type),
                    (CONTENT_LENGTH, content_length),
                    (CONTENT_DISPOSITION, disposition),
                    (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    (
                        HeaderName::from_static("cross-origin-resource-policy"),
                        HeaderValue::from_static("same-origin"),
                    ),
                ],
                resource.bytes,
            )
                .into_response()
        }
        Err(ServiceError::NotFound) => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(crate::ErrorCode::NotFound, "The resource was not found."),
        ),
        Err(ServiceError::Unauthorized) => error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "This resource is not authorized.",
            ),
        ),
        Err(ServiceError::Unavailable) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "Opaque resources are temporarily unavailable.",
            )
            .with_retryable(true),
        ),
        Err(ServiceError::CorruptResource) => error_response(
            StatusCode::GONE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The resource is no longer available.",
            ),
        ),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

async fn session_export<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if query.is_some() {
        return invalid_request();
    }
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let permit = match Arc::clone(&state.session_exports).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                SanitizedError::public(
                    crate::ErrorCode::Unavailable,
                    "A session export is already in progress.",
                )
                .with_retryable(true),
            )
        }
    };
    let exported = state.supervisor.session_export(&session_id).await;
    drop(permit);
    match exported {
        Ok(bytes) => {
            let content_length = match HeaderValue::from_str(&bytes.len().to_string()) {
                Ok(value) => value,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        SanitizedError::internal(),
                    )
                }
            };
            let disposition = match session_export_content_disposition(&session_id) {
                Ok(value) => value,
                Err(_) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        SanitizedError::internal(),
                    )
                }
            };
            (
                [
                    (
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/json; charset=utf-8"),
                    ),
                    (CONTENT_LENGTH, content_length),
                    (CONTENT_DISPOSITION, disposition),
                    (CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    (X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
                    (REFERRER_POLICY, HeaderValue::from_static("no-referrer")),
                ],
                bytes,
            )
                .into_response()
        }
        Err(ServiceError::NotFound) => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The requested session was not found.",
            ),
        ),
        Err(ServiceError::Unauthorized) => error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "This session export is not authorized.",
            ),
        ),
        Err(ServiceError::Unavailable) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "Session export is temporarily unavailable.",
            )
            .with_retryable(true),
        ),
        Err(ServiceError::PayloadTooLarge) => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            SanitizedError::public(
                crate::ErrorCode::PayloadTooLarge,
                "The redacted session export exceeds the download limit.",
            ),
        ),
        Err(_) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

async fn exchange_launch_token<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    Path(token): Path<String>,
) -> Response {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return authentication_required();
    }
    if !state.auth.exchange(&token) {
        return authentication_required();
    }
    let cookie = match state.auth.set_cookie_value() {
        Ok(cookie) => cookie,
        Err(_) => return authentication_required(),
    };
    (
        StatusCode::SEE_OTHER,
        [
            (LOCATION, HeaderValue::from_static("/")),
            (SET_COOKIE, cookie),
        ],
    )
        .into_response()
}

async fn index<H: HostService>(State(state): State<Arc<TransportState<H>>>) -> Response {
    web_asset_response(&state.web_bundle, "index.html")
}

async fn static_asset<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    Path(asset): Path<String>,
) -> Response {
    if asset.starts_with("api/") || asset.starts_with("__ygg/") {
        return not_found().await;
    }
    let relative = FilePath::new(&asset);
    if !safe_relative_path(relative) {
        return not_found().await;
    }
    if !asset.starts_with("assets/") {
        return if asset.contains('.') || asset == "assets" {
            not_found().await
        } else {
            web_asset_response(&state.web_bundle, "index.html")
        };
    }
    match state.web_bundle.asset(&asset) {
        Some(_) => web_asset_response(&state.web_bundle, &asset),
        None => not_found().await,
    }
}

fn web_asset_response(bundle: &WebBundle, path: &str) -> Response {
    let Some(asset) = bundle.asset(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let bundle_sha256 = HeaderValue::from_str(bundle.bundle_sha256())
        .expect("validated web bundle digest must be a valid header value");
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(asset.media_type)),
            (X_YGG_WEB_BUNDLE, bundle_sha256),
        ],
        asset.bytes,
    )
        .into_response()
}

fn safe_relative_path(path: &FilePath) -> bool {
    path.components().all(|component| match component {
        Component::Normal(value) => value
            .to_str()
            .is_some_and(|value| !value.starts_with('.') && !value.is_empty()),
        _ => false,
    })
}

async fn not_found() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        SanitizedError::public(
            crate::ErrorCode::NotFound,
            "The requested route was not found.",
        ),
    )
}

async fn bootstrap<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    query: Result<Query<BootstrapQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    if query.inventory_only && query.selected_session_id.is_some() {
        return invalid_request();
    }
    let selected = match query.selected_session_id {
        Some(raw) => match SessionId::new(raw) {
            Ok(id) => Some(id),
            Err(_) => return invalid_request(),
        },
        None => None,
    };
    let result = if query.inventory_only {
        state.supervisor.inventory_bootstrap().await
    } else {
        match selected {
            Some(session_id) => match state.supervisor.open_session(&session_id).await {
                Ok(_) => state.supervisor.bootstrap(&session_id).await,
                Err(error) => Err(error),
            },
            None => state.supervisor.launch(None).await,
        }
    };
    match result {
        Ok(mut bootstrap) => {
            match principal.map(|Extension(principal)| principal) {
                Some(TransportPrincipal::LoopbackOwner) => {
                    bootstrap.capabilities.connected_devices = state
                        .companion
                        .as_ref()
                        .is_some_and(CompanionControl::is_healthy);
                }
                Some(TransportPrincipal::Paired { .. }) | None => {
                    bootstrap.capabilities.terminal = false;
                    bootstrap.capabilities.connected_devices = false;
                }
            }
            Json(bootstrap).into_response()
        }
        Err(error) => supervisor_error_response(error),
    }
}

async fn project_catalog<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    match state.supervisor.project_catalog().await {
        Ok(catalog) => Json(catalog).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn usage_stats<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    query: Result<Query<UsageStatsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.usage_stats(query.period).await {
        Ok(stats) => Json(stats).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn usage_lifetime<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    match state.supervisor.usage_lifetime().await {
        Ok(lifetime) => Json(lifetime).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn usage_activity<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    match state.supervisor.usage_activity().await {
        Ok(activity) => Json(activity).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn session_snapshot<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
) -> Response {
    if !state.rate_limiter.admit_remote_read(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.session_view(&session_id).await {
        Ok(view) => Json(view.snapshot).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn command_discovery<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.command_discovery(&session_id).await {
        Ok(discovery) => Json(discovery).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

fn goal_response(goal: Option<GoalState>, revision: u64) -> Response {
    let mut response = Json(goal).into_response();
    response.headers_mut().insert(
        X_YGG_GOAL_REVISION,
        HeaderValue::from_str(&revision.to_string()).unwrap(),
    );
    response
}

async fn session_goal<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    if let Err(error) = state.supervisor.session_view(&session_id).await {
        return supervisor_error_response(error);
    }
    let (goal, revision) = match state.goal_store.snapshot(&session_id) {
        Ok(snapshot) => snapshot,
        Err(error) => return goal_error_response(error),
    };
    goal_response(goal, revision)
}

async fn update_goal<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
    payload: Result<Json<GoalRequest>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    let view = match state.supervisor.session_view(&session_id).await {
        Ok(view) => view,
        Err(error) => return supervisor_error_response(error),
    };
    let command = match (request.objective, request.action) {
        (Some(objective), None) => SessionCommand::SetGoal {
            objective,
            turn_budget: request.turn_budget,
        },
        (None, Some(action)) if request.turn_budget.is_none() => match action {
            GoalAction::Pause => SessionCommand::PauseGoal,
            GoalAction::Resume => SessionCommand::ResumeGoal,
            GoalAction::Clear => SessionCommand::ClearGoal,
        },
        _ => return invalid_request(),
    };
    let command_id = match CommandId::new(format!(
        "http-goal-{}",
        NEXT_HTTP_GOAL_COMMAND_ID.fetch_add(1, Ordering::Relaxed)
    )) {
        Ok(command_id) => command_id,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SanitizedError::internal(),
            )
        }
    };
    let device_id = match DeviceId::new("http-goal") {
        Ok(device_id) => device_id,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SanitizedError::internal(),
            )
        }
    };
    let envelope = SessionCommandEnvelope::new(
        state.supervisor.host_id(),
        device_id,
        session_id.clone(),
        command_id,
        now_ms(),
        Some(view.snapshot.actor_generation),
        command,
    );
    let admission = match state.supervisor.command(envelope, now_ms()).await {
        Ok(admission) => admission,
        Err(error) => return supervisor_error_response(error),
    };
    match &admission.ack.disposition {
        AckDisposition::Accepted { .. } => {
            let (goal, revision) = match state.goal_store.snapshot(&session_id) {
                Ok(snapshot) => snapshot,
                Err(error) => return goal_error_response(error),
            };
            goal_response(goal, revision)
        }
        AckDisposition::Rejected { error } => command_error_response(error.clone()),
    }
}

async fn session_replay<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    Path(raw_session_id): Path<String>,
    query: Result<Query<ReplayQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit_remote_read(principal.as_ref()) {
        return rate_limited();
    }
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return invalid_request(),
    };
    if query.actor_generation == 0 {
        return invalid_request();
    }
    match state
        .supervisor
        .replay_after(
            &session_id,
            SessionCursor {
                actor_generation: query.actor_generation,
                sequence: query.sequence,
            },
        )
        .await
    {
        Ok(replay) => Json(replay).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn host_command<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    payload: Result<Json<HostCommandEnvelope>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let Json(envelope) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.host_command(envelope, now_ms()).await {
        Ok(admission) => Json(admission.ack).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn session_command<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    payload: Result<Json<SessionCommandEnvelope>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let Json(envelope) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.command(envelope, now_ms()).await {
        Ok(admission) => Json(admission.ack).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn transcript_search<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    payload: Result<Json<crate::TranscriptSearchRequest>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    if !state.supervisor.transcript_search_supported() {
        return service_error_response(ServiceError::Unavailable);
    }
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.search_transcripts(&request).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => service_error_response(error),
    }
}

async fn events_socket<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(_) => return invalid_request(),
    };
    let events = state.supervisor.subscribe_events();
    let Some(upgrade_task) = state.upgrades.register() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| upgrade_task.run(stream_events(socket, events)))
}

async fn stream_events(
    socket: WebSocket,
    mut events: tokio::sync::broadcast::Receiver<crate::HostStreamEvent>,
) {
    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if event.validate().is_err() {
                            break;
                        }
                        let Ok(encoded) = serde_json::to_string(&event) else {
                            break;
                        };
                        if sender.send(Message::Text(encoded.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = sender.send(Message::Close(Some(CloseFrame {
                            code: 1013,
                            reason: "replay required".into(),
                        }))).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(bytes))) => {
                        if sender.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        let _ = sender.send(Message::Close(Some(CloseFrame {
                            code: 1008,
                            reason: "client messages are not accepted".into(),
                        }))).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn terminal_socket<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    principal: Option<Extension<TransportPrincipal>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !state.rate_limiter.admit(principal.as_ref()) {
        return rate_limited();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(_) => return invalid_request(),
    };
    let Some(terminal) = state.terminal.clone() else {
        return not_found().await;
    };
    let Some(upgrade_task) = state.upgrades.register() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    upgrade
        .max_message_size(MAX_TERMINAL_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_TERMINAL_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| {
            upgrade_task
                .run_with_shutdown_grace(stream_terminal(socket, terminal), WEBSOCKET_CLOSE_GRACE)
        })
}

async fn stream_terminal(socket: WebSocket, terminal: PtyManager) {
    let (mut sender, mut receiver) = socket.split();
    let mut attachment: Option<PtyAttachment> = None;
    loop {
        tokio::select! {
            event = async {
                if let Some(attachment) = attachment.as_mut() {
                    attachment.events.recv().await
                } else {
                    std::future::pending().await
                }
            } => {
                let Some(id) = attachment.as_ref().map(|attachment| attachment.id.clone()) else {
                    continue;
                };
                match event {
                    Ok(PtyEvent::Output(data)) => {
                        if !send_terminal_message(
                            &mut sender,
                            TerminalServerMessage::Output { id, data },
                        ).await {
                            break;
                        }
                    }
                    Ok(PtyEvent::Exit(exit)) => {
                        let _ = send_terminal_message(
                            &mut sender,
                            terminal_exit_message(id, exit),
                        ).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let _ = sender.send(Message::Close(Some(CloseFrame {
                            code: 1013,
                            reason: "terminal output replay required".into(),
                        }))).await;
                        break;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = receiver.next() => {
                let message = match incoming {
                    Some(Ok(Message::Text(text))) => match serde_json::from_str(text.as_str()) {
                        Ok(message) => message,
                        Err(_) => {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: "The terminal message is invalid.",
                                },
                            ).await {
                                break;
                            }
                            continue;
                        }
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(bytes))) => {
                        if sender.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                    Some(Ok(Message::Binary(_))) => {
                        let _ = sender.send(Message::Close(Some(CloseFrame {
                            code: 1003,
                            reason: "terminal messages must be text".into(),
                        }))).await;
                        break;
                    }
                };

                match message {
                    TerminalClientMessage::Open {
                        cwd,
                        owner_key,
                        cols,
                        rows,
                    } => match terminal.open(PtyOpenRequest {
                        cols,
                        rows,
                        owner_key,
                        cwd,
                    }) {
                        Ok(next) => {
                            let opened = TerminalServerMessage::Opened {
                                id: next.id.clone(),
                                owner_key: next.owner_key.clone(),
                                replay: (!next.replay.is_empty()).then_some(next.replay.clone()),
                            };
                            let exit = next.exit.clone();
                            if !send_terminal_message(&mut sender, opened).await {
                                break;
                            }
                            if let Some(previous) = attachment.replace(next) {
                                let _ = terminal.detach(&previous.id);
                            }
                            if let Some(exit) = exit {
                                let Some(id) = attachment
                                    .as_ref()
                                    .map(|attachment| attachment.id.clone())
                                else {
                                    break;
                                };
                                let _ = send_terminal_message(
                                    &mut sender,
                                    terminal_exit_message(id, exit),
                                ).await;
                                break;
                            }
                        }
                        Err(error) => {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: terminal_error_message(&error),
                                },
                            ).await {
                                break;
                            }
                        }
                    },
                    TerminalClientMessage::Resize { id, cols, rows } => {
                        if !terminal_attachment_matches(&attachment, &id) {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: "The terminal request is invalid.",
                                },
                            ).await {
                                break;
                            }
                            continue;
                        }
                        if let Err(error) = terminal.resize(&id, cols, rows) {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: terminal_error_message(&error),
                                },
                            ).await {
                                break;
                            }
                        }
                    }
                    TerminalClientMessage::Input { id, data } => {
                        if !terminal_attachment_matches(&attachment, &id) {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: "The terminal request is invalid.",
                                },
                            ).await {
                                break;
                            }
                            continue;
                        }
                        if let Err(error) = terminal.input(&id, &data) {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: terminal_error_message(&error),
                                },
                            ).await {
                                break;
                            }
                        }
                    }
                    TerminalClientMessage::Detach { id } => {
                        if !terminal_attachment_matches(&attachment, &id) {
                            if !send_terminal_message(
                                &mut sender,
                                TerminalServerMessage::Error {
                                    message: "The terminal request is invalid.",
                                },
                            ).await {
                                break;
                            }
                            continue;
                        }
                        match terminal.detach(&id) {
                            Ok(()) => {
                                attachment.take();
                            }
                            Err(error) => {
                                if !send_terminal_message(
                                    &mut sender,
                                    TerminalServerMessage::Error {
                                        message: terminal_error_message(&error),
                                    },
                                ).await {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if let Some(attachment) = attachment {
        let _ = terminal.detach(&attachment.id);
    }
}

fn terminal_attachment_matches(attachment: &Option<PtyAttachment>, id: &str) -> bool {
    attachment
        .as_ref()
        .is_some_and(|attachment| attachment.id == id)
}

async fn send_terminal_message(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: TerminalServerMessage,
) -> bool {
    let Ok(encoded) = serde_json::to_string(&message) else {
        return false;
    };
    sender.send(Message::Text(encoded.into())).await.is_ok()
}

fn terminal_exit_message(id: String, exit: PtyExit) -> TerminalServerMessage {
    TerminalServerMessage::Exit {
        id,
        exit_code: exit.exit_code,
        signal: exit.signal,
    }
}

fn terminal_error_message(error: &PtyError) -> &'static str {
    match error {
        PtyError::InvalidDimensions
        | PtyError::InvalidOwnerKey
        | PtyError::InvalidSessionId
        | PtyError::InvalidWorkingDirectory => "The terminal request is invalid.",
        PtyError::InputTooLarge => "The terminal input exceeds the size limit.",
        PtyError::NotFound => "The terminal session is no longer available.",
        PtyError::Exited => "The terminal session has exited.",
        PtyError::WorkingDirectoryUnavailable | PtyError::Start | PtyError::Io(_) => {
            "The terminal could not be started or reached."
        }
    }
}

fn declared_content_length(headers: &HeaderMap) -> Result<Option<usize>, ()> {
    let mut values = headers.get_all(CONTENT_LENGTH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(());
    }
    let value = std::str::from_utf8(bytes).map_err(|_| ())?;
    value.parse::<usize>().map(Some).map_err(|_| ())
}

async fn secure_request<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let attachment_upload =
        request.method() == Method::POST && request.uri().path() == "/api/v1/attachments";
    let document_upload = request.method() == Method::POST
        && request.uri().path().starts_with("/api/v1/sessions/")
        && request.uri().path().ends_with("/documents");
    let project_file_write = request.method() == Method::POST
        && request.uri().path().starts_with("/api/v1/fs/")
        && request.uri().path().ends_with("/write");
    let session_export = request.method() == Method::GET
        && request.uri().path().starts_with("/api/v1/sessions/")
        && request.uri().path().ends_with("/export");
    let resource_request = matches!(request.method(), &Method::GET | &Method::HEAD)
        && request.uri().path().starts_with("/api/v1/sessions/")
        && request.uri().path().contains("/resources/");
    let terminal_socket = request.uri().path() == "/api/v1/terminal";
    let host_allowed = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| state.allowed_authorities.allows_host(value));
    let origin_allowed = match headers.get(ORIGIN) {
        None => true,
        Some(value) => value
            .to_str()
            .ok()
            .is_some_and(|value| state.allowed_authorities.allows_origin(value)),
    };
    let terminal_origin_allowed = !terminal_socket
        || headers
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| state.allowed_authorities.allows_origin(value));
    let fetch_site_allowed = match headers.get("sec-fetch-site") {
        None => true,
        Some(value) => value
            .to_str()
            .ok()
            .is_some_and(|value| matches!(value, "same-origin" | "same-site" | "none")),
    };
    let path_allowed = request.uri().path().len() <= MAX_PATH_BYTES;
    let query_allowed = request
        .uri()
        .query()
        .is_none_or(|query| query.len() <= MAX_QUERY_BYTES);
    let content_length = declared_content_length(headers);
    let valid_content_length = content_length.as_ref().ok().copied().flatten();
    let bodyless_request_has_body = request.method() != Method::POST
        && (headers.contains_key(TRANSFER_ENCODING)
            || valid_content_length.is_some_and(|length| length != 0));
    let export_has_query = session_export && request.uri().query().is_some();
    let export_has_body = session_export
        && (headers.contains_key(TRANSFER_ENCODING)
            || valid_content_length.is_some_and(|length| length != 0));
    let resource_has_forbidden_shape = resource_request
        && (request.uri().query().is_some()
            || headers.contains_key(RANGE)
            || headers.contains_key(TRANSFER_ENCODING)
            || valid_content_length.is_some_and(|length| length != 0));
    let content_length_limit = if attachment_upload {
        state
            .supervisor
            .attachment_policy()
            .map(|policy| policy.max_file_bytes as usize)
            .unwrap_or(MAX_ATTACHMENT_FILE_BYTES)
    } else if document_upload {
        MAX_DOCUMENT_FILE_BYTES
    } else if project_file_write {
        MAX_PROJECT_FILE_WRITE_REQUEST_BYTES
    } else {
        MAX_COMMAND_BYTES
    };
    let content_length_allowed =
        valid_content_length.is_none_or(|length| length <= content_length_limit);
    let mutation_has_json = request.method() != Method::POST
        || headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                if attachment_upload {
                    state.supervisor.attachment_policy().is_some_and(|policy| {
                        policy
                            .accepted_media_types
                            .iter()
                            .any(|accepted| accepted == value)
                    })
                } else if document_upload {
                    value
                        .split(';')
                        .next()
                        .map(str::trim)
                        .is_some_and(|media_type| {
                            matches!(
                                media_type,
                                "text/plain" | "text/markdown" | "application/pdf"
                            )
                        })
                } else {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|media_type| media_type.trim() == "application/json")
                }
            });
    let api_authenticated =
        !request.uri().path().starts_with("/api/v1/") || state.auth.allows_cookie(headers);

    let mut response =
        if !host_allowed || !origin_allowed || !terminal_origin_allowed || !fetch_site_allowed {
            error_response(
                StatusCode::FORBIDDEN,
                SanitizedError::public(
                    crate::ErrorCode::Unauthorized,
                    "This request is not allowed by the loopback host.",
                ),
            )
        } else if !api_authenticated {
            authentication_required()
        } else if content_length.is_err()
            || bodyless_request_has_body
            || export_has_query
            || export_has_body
            || resource_has_forbidden_shape
        {
            invalid_request()
        } else if !path_allowed || !query_allowed || !content_length_allowed {
            error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                SanitizedError::public(
                    crate::ErrorCode::InvalidCommand,
                    "The request exceeds a transport limit.",
                ),
            )
        } else if !mutation_has_json {
            error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                SanitizedError::public(
                    crate::ErrorCode::InvalidCommand,
                    "This endpoint requires JSON.",
                ),
            )
        } else {
            let mut request = request;
            request
                .extensions_mut()
                .insert(TransportPrincipal::LoopbackOwner);
            next.run(request).await
        };
    apply_security_headers(response.headers_mut());
    response
}

fn supervisor_error_response(error: SupervisorError) -> Response {
    match error {
        SupervisorError::Service(crate::ServiceError::NotFound) => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The requested session was not found.",
            ),
        ),
        SupervisorError::Service(crate::ServiceError::Locked) => error_response(
            StatusCode::CONFLICT,
            SanitizedError::public(
                crate::ErrorCode::Locked,
                "Another process currently owns this session.",
            ),
        ),
        SupervisorError::Service(crate::ServiceError::Unauthorized) => error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "This request is not authorized.",
            ),
        ),
        SupervisorError::Service(crate::ServiceError::Unavailable) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The graphical host is temporarily at capacity.",
            )
            .with_retryable(true),
        ),
        _ => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

fn project_file_system_error_response(error: ProjectFileSystemError) -> Response {
    match error {
        ProjectFileSystemError::TrustRequired => error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "Explicit project trust is required for this operation.",
            ),
        ),
        ProjectFileSystemError::RootChanged => error_response(
            StatusCode::CONFLICT,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The trusted project root changed before the operation completed.",
            ),
        ),
        ProjectFileSystemError::InvalidPath
        | ProjectFileSystemError::InvalidRange
        | ProjectFileSystemError::InvalidSearch
        | ProjectFileSystemError::NotDirectory
        | ProjectFileSystemError::NotFile
        | ProjectFileSystemError::NotText => invalid_request(),
        ProjectFileSystemError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The project file was not found.",
            ),
        ),
        ProjectFileSystemError::ContentTooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            SanitizedError::public(
                crate::ErrorCode::PayloadTooLarge,
                "The project file exceeds the host limit.",
            ),
        ),
        ProjectFileSystemError::Conflict => error_response(
            StatusCode::CONFLICT,
            SanitizedError::public(
                crate::ErrorCode::Locked,
                "The project file changed before the operation completed.",
            ),
        ),
        ProjectFileSystemError::Unavailable | ProjectFileSystemError::WriteUnavailable => {
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                SanitizedError::public(
                    crate::ErrorCode::Unavailable,
                    "Project file access is not available on this host.",
                ),
            )
        }
        ProjectFileSystemError::Storage => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

fn goal_error_response(error: GoalStoreError) -> Response {
    match error {
        GoalStoreError::InvalidObjective => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidGoal,
                "The goal objective is invalid.",
            ),
        ),
        GoalStoreError::InvalidTurnBudget => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidGoal,
                "The goal turn budget is invalid.",
            ),
        ),
        GoalStoreError::InvalidTransition => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidGoal,
                "The goal cannot be changed from its current status.",
            ),
        ),
        GoalStoreError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The requested goal was not be found.",
            ),
        ),
        GoalStoreError::CorruptState | GoalStoreError::UnsafePath | GoalStoreError::Storage(_) => {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SanitizedError::internal(),
            )
        }
    }
}

fn command_error_response(error: SanitizedError) -> Response {
    let status = match error.code {
        crate::ErrorCode::InvalidGoal
        | crate::ErrorCode::InvalidCommand
        | crate::ErrorCode::InvalidBoundary => StatusCode::BAD_REQUEST,
        crate::ErrorCode::NotFound => StatusCode::NOT_FOUND,
        crate::ErrorCode::Unauthorized => StatusCode::FORBIDDEN,
        crate::ErrorCode::Locked => StatusCode::CONFLICT,
        crate::ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status, error)
}

fn service_error_response(error: ServiceError) -> Response {
    match error {
        ServiceError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::NotFound,
                "The requested local resource was not found.",
            ),
        ),
        ServiceError::Unauthorized => error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "Explicit project trust is required for this operation.",
            ),
        ),
        ServiceError::InvalidBoundary => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidCommand,
                "The requested local resource operation is invalid.",
            ),
        ),
        ServiceError::InvalidGoal => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidGoal,
                "The goal objective, budget, or lifecycle transition is invalid.",
            ),
        ),
        ServiceError::PayloadTooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            SanitizedError::public(
                crate::ErrorCode::PayloadTooLarge,
                "The requested local resource exceeds its bounded limit.",
            ),
        ),
        ServiceError::Unavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The requested local resource is temporarily unavailable.",
            )
            .with_retryable(true),
        ),
        ServiceError::CorruptResource => error_response(
            StatusCode::GONE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The requested local resource is no longer available.",
            ),
        ),
        ServiceError::Locked => error_response(
            StatusCode::CONFLICT,
            SanitizedError::public(
                crate::ErrorCode::Locked,
                "The requested local resource is currently locked.",
            ),
        ),
        ServiceError::InvalidSeed | ServiceError::OwnerLost | ServiceError::Internal => {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                SanitizedError::internal(),
            )
        }
    }
}

fn invalid_request() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        SanitizedError::public(crate::ErrorCode::InvalidCommand, "The request is invalid."),
    )
}

fn authentication_required() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        SanitizedError::public(
            crate::ErrorCode::Unauthorized,
            "This graphical client is not authenticated.",
        ),
    )
}

fn rate_limited() -> Response {
    error_response(
        StatusCode::TOO_MANY_REQUESTS,
        SanitizedError::public(
            crate::ErrorCode::Unavailable,
            "Too many requests. Try again shortly.",
        )
        .with_retryable(true),
    )
}

fn attachment_error_response(error: AttachmentError) -> Response {
    match error {
        AttachmentError::Unavailable => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "Attachment ingest is not available on this host.",
            ),
        ),
        AttachmentError::InvalidName => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidCommand,
                "The attachment name is invalid.",
            ),
        ),
        AttachmentError::UnsupportedMediaType => error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            SanitizedError::public(
                crate::ErrorCode::InvalidCommand,
                "Only PNG, JPEG, GIF, and WebP images are accepted.",
            ),
        ),
        AttachmentError::InvalidContent | AttachmentError::MetadataMismatch => error_response(
            StatusCode::BAD_REQUEST,
            SanitizedError::public(
                crate::ErrorCode::InvalidCommand,
                "The attachment content or metadata is invalid.",
            ),
        ),
        AttachmentError::TooLarge => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            SanitizedError::public(
                crate::ErrorCode::PayloadTooLarge,
                "The attachment exceeds the host limit.",
            ),
        ),
        AttachmentError::QuotaExceeded => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            SanitizedError::public(
                crate::ErrorCode::Unavailable,
                "The attachment store is currently full.",
            )
            .with_retryable(true),
        ),
        AttachmentError::NotFound => error_response(
            StatusCode::NOT_FOUND,
            SanitizedError::public(crate::ErrorCode::NotFound, "The attachment was not found."),
        ),
        AttachmentError::Storage => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            SanitizedError::internal(),
        ),
    }
}

fn inline_content_disposition(
    display_name: &str,
) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
    let safe = display_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')')
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    HeaderValue::from_str(&format!("inline; filename=\"{safe}\""))
}

fn attachment_content_disposition(
    display_name: &str,
) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
    let safe = display_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')')
            {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    HeaderValue::from_str(&format!("attachment; filename=\"{safe}\""))
}

fn safe_inline_resource(resource: &crate::StoredResource) -> (&'static str, bool) {
    match resource.media_type.as_str() {
        "text/plain" if std::str::from_utf8(&resource.bytes).is_ok() => ("text/plain", true),
        "image/png" if resource.bytes.starts_with(b"\x89PNG\r\n\x1a\n") => ("image/png", true),
        "image/jpeg"
            if resource.bytes.starts_with(&[0xff, 0xd8, 0xff])
                && resource.bytes.ends_with(&[0xff, 0xd9]) =>
        {
            ("image/jpeg", true)
        }
        "image/gif"
            if resource.bytes.starts_with(b"GIF87a") || resource.bytes.starts_with(b"GIF89a") =>
        {
            ("image/gif", true)
        }
        "image/webp"
            if resource.bytes.len() >= 12
                && resource.bytes.starts_with(b"RIFF")
                && resource.bytes.get(8..12) == Some(b"WEBP") =>
        {
            ("image/webp", true)
        }
        _ => ("application/octet-stream", false),
    }
}

fn session_export_content_disposition(
    session_id: &SessionId,
) -> Result<HeaderValue, axum::http::header::InvalidHeaderValue> {
    HeaderValue::from_str(&format!(
        "attachment; filename=\"ygg-session-{}.json\"",
        session_id.as_str()
    ))
}

fn error_response(status: StatusCode, error: SanitizedError) -> Response {
    (
        status,
        Json(ErrorResponse {
            protocol: PROTOCOL_VERSION,
            error,
        }),
    )
        .into_response()
}

fn apply_security_headers(headers: &mut HeaderMap) {
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self' data:; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'",
        ),
    );
    if !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn random_hex(bytes: usize) -> Result<String, std::io::Error> {
    let mut random = vec![0u8; bytes];
    getrandom::fill(&mut random)
        .map_err(|_| std::io::Error::other("secure transport randomness unavailable"))?;
    Ok(random
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    #[cfg(unix)]
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::sync::Notify;
    #[cfg(unix)]
    use tokio_tungstenite::tungstenite::{
        client::IntoClientRequest, http::HeaderValue as TungsteniteHeaderValue,
        Message as TungsteniteMessage,
    };

    use crate::{
        ActorOwnerState, AttachmentPolicy, AttachmentRef, AttachmentStore, AttentionState,
        AuthorityProfile, ColorScheme, ContextUsage, CreateSessionRequest, DriverCommandOutcome,
        GoalAction, GoalStore, HostCapabilities, HostDescriptor, HostId, InputModality,
        ModelSelection, ModelSummary, ProjectFileEntry, ProjectFileEntryKind, ProjectFileRead,
        ProjectFileSearchResult, ProjectFileSystemError, ProjectFileTree, ProjectFileWrite,
        ServiceError, SessionCommand, SessionCursor, SessionDriver, SessionLiveState, SessionSeed,
        SessionSnapshot, SessionSummary, StoredAttachment, StoredResource, SupervisorConfig,
        ThemeDensity, ThemeDto, ThemeId, ThemeMotion, ThemeOption, ThemeSourceClass,
        ThemeTypography,
    };

    use super::*;

    type ProjectFileWriteRecord = (String, String, String, bool);

    #[derive(Clone)]
    struct MockHost {
        creates: Arc<AtomicUsize>,
        opens: Arc<AtomicUsize>,
        next_session: Arc<AtomicUsize>,
        seeds: Arc<Mutex<BTreeMap<SessionId, SessionSeed>>>,
        attachments: AttachmentStore,
        _attachment_root: Arc<tempfile::TempDir>,
        project_file_browser: bool,
        project_file_write: bool,
        project_file_writes: Arc<Mutex<Vec<ProjectFileWriteRecord>>>,
        goals: GoalStore,
        _goal_root: Arc<tempfile::TempDir>,
        export_started: Arc<Notify>,
        export_release: Arc<Notify>,
    }

    impl MockHost {
        fn new() -> Self {
            let attachment_root = Arc::new(tempfile::tempdir().unwrap());
            let attachments = AttachmentStore::open(attachment_root.path()).unwrap();
            let goal_root = Arc::new(tempfile::tempdir().unwrap());
            let goals = GoalStore::open(&goal_root.path().join("goals")).unwrap();
            Self {
                creates: Arc::new(AtomicUsize::new(0)),
                opens: Arc::new(AtomicUsize::new(0)),
                next_session: Arc::new(AtomicUsize::new(1)),
                seeds: Arc::new(Mutex::new(BTreeMap::new())),
                attachments,
                _attachment_root: attachment_root,
                project_file_browser: false,
                project_file_write: false,
                project_file_writes: Arc::new(Mutex::new(Vec::new())),
                goals,
                _goal_root: goal_root,
                export_started: Arc::new(Notify::new()),
                export_release: Arc::new(Notify::new()),
            }
        }

        fn with_project_files() -> Self {
            let mut host = Self::new();
            host.project_file_browser = true;
            host.project_file_write = true;
            host
        }

        fn insert_existing(&self, id: &str) -> SessionId {
            let id = SessionId::new(id).unwrap();
            let seed = seed(id.clone(), AuthorityProfile::Workspace, false, 1);
            self.seeds.lock().unwrap().insert(id.clone(), seed);
            id
        }
    }

    struct MockDriver(SessionSeed, GoalStore);

    #[async_trait]
    impl SessionDriver for MockDriver {
        fn seed(&self) -> SessionSeed {
            self.0.clone()
        }

        async fn dispatch(
            &mut self,
            command: SessionCommand,
        ) -> Result<DriverCommandOutcome, ServiceError> {
            let session_id = &self.0.snapshot.session_id;
            let result = match command {
                SessionCommand::SetGoal {
                    objective,
                    turn_budget,
                } => self.1.set(session_id, &objective, turn_budget).map(|_| ()),
                SessionCommand::PauseGoal => {
                    self.1.apply(session_id, GoalAction::Pause).map(|_| ())
                }
                SessionCommand::ResumeGoal => {
                    self.1.apply(session_id, GoalAction::Resume).map(|_| ())
                }
                SessionCommand::ClearGoal => {
                    self.1.apply(session_id, GoalAction::Clear).map(|_| ())
                }
                _ => return Ok(DriverCommandOutcome::default()),
            };
            result
                .map(|_| DriverCommandOutcome::default())
                .map_err(|_| ServiceError::InvalidGoal)
        }
    }

    #[async_trait]
    impl HostService for MockHost {
        type Driver = MockDriver;

        fn descriptor(&self) -> HostDescriptor {
            HostDescriptor {
                id: HostId::new("host-transport-test").unwrap(),
                name: "Transport test host".into(),
            }
        }

        fn capabilities(&self) -> HostCapabilities {
            HostCapabilities {
                attachments: true,
                attachment_policy: Some(AttachmentPolicy::image_defaults()),
                project_file_browser: self.project_file_browser,
                project_file_write: self.project_file_write,
                session_export: true,
                ..HostCapabilities::default()
            }
        }

        fn attachment_policy(&self) -> Option<AttachmentPolicy> {
            Some(self.attachments.policy())
        }

        async fn ingest_attachment(
            &self,
            display_name: &str,
            media_type: &str,
            bytes: bytes::Bytes,
        ) -> Result<AttachmentRef, AttachmentError> {
            self.attachments.ingest(display_name, media_type, bytes)
        }

        async fn attachment_content(
            &self,
            handle: &str,
        ) -> Result<StoredAttachment, AttachmentError> {
            self.attachments.content(handle)
        }

        fn project_file_browser_supported(&self) -> bool {
            self.project_file_browser
        }

        fn project_file_write_supported(&self) -> bool {
            self.project_file_write
        }

        async fn project_file_tree(
            &self,
            _project_id: &ProjectId,
            path: &str,
        ) -> Result<ProjectFileTree, ProjectFileSystemError> {
            if !self.project_file_browser {
                return Err(ProjectFileSystemError::Unavailable);
            }
            if path == "conflict" {
                return Err(ProjectFileSystemError::Conflict);
            }
            Ok(ProjectFileTree {
                path: path.to_owned(),
                entries: vec![ProjectFileEntry {
                    name: "src".into(),
                    kind: ProjectFileEntryKind::Directory,
                    size: 0,
                    modified_at_ms: None,
                    git_status: Vec::new(),
                }],
                truncated: false,
                git_status_truncated: false,
            })
        }

        async fn read_project_file(
            &self,
            _project_id: &ProjectId,
            path: &str,
            start_line: Option<u32>,
            end_line: Option<u32>,
        ) -> Result<ProjectFileRead, ProjectFileSystemError> {
            if !self.project_file_browser {
                return Err(ProjectFileSystemError::Unavailable);
            }
            if path == "conflict" {
                return Err(ProjectFileSystemError::Conflict);
            }
            if start_line == Some(0) || end_line == Some(0) {
                return Err(ProjectFileSystemError::InvalidRange);
            }
            Ok(ProjectFileRead {
                path: path.to_owned(),
                content: "fn main() {}\n".into(),
                start_line: start_line.unwrap_or(1),
                end_line: end_line.unwrap_or(1),
                line_count: 1,
                truncated: false,
                sha256: Some(
                    "f1b0dcd7f39a36e5f99d602a87616e4e13f9d7dcf99096f1d7ec179c34d93d1e".into(),
                ),
            })
        }

        async fn search_project_files(
            &self,
            _project_id: &ProjectId,
            query: &str,
        ) -> Result<ProjectFileSearchResult, ProjectFileSystemError> {
            if !self.project_file_browser {
                return Err(ProjectFileSystemError::Unavailable);
            }
            if query == "invalid" {
                return Err(ProjectFileSystemError::InvalidSearch);
            }
            Ok(ProjectFileSearchResult {
                hits: Vec::new(),
                truncated: false,
                scanned_bytes: 0,
            })
        }

        async fn write_project_file(
            &self,
            _project_id: &ProjectId,
            path: &str,
            content: &str,
            expected_sha256: &str,
            force: bool,
        ) -> Result<ProjectFileWrite, ProjectFileSystemError> {
            if !self.project_file_write {
                return Err(ProjectFileSystemError::WriteUnavailable);
            }
            if path == "conflict" {
                return Err(ProjectFileSystemError::Conflict);
            }
            self.project_file_writes.lock().unwrap().push((
                path.to_owned(),
                content.to_owned(),
                expected_sha256.to_owned(),
                force,
            ));
            Ok(ProjectFileWrite {
                path: path.to_owned(),
                sha256: "fb1b5b67516d1d8348a527480830b9717bc8c0f9f7f405d5e5196ae8e251066e".into(),
                modified_at_ms: Some(1),
            })
        }

        async fn resource_content(
            &self,
            session_id: &SessionId,
            handle: &str,
        ) -> Result<StoredResource, ServiceError> {
            if session_id.as_str() != "session-resource-test" {
                return Err(ServiceError::NotFound);
            }
            match handle {
                "opaque-resource-test" => Ok(StoredResource {
                    display_name: "evidence.txt".into(),
                    media_type: "text/plain".into(),
                    bytes: bytes::Bytes::from_static(b"structured evidence"),
                    sha256: "eea41f02aac250f283eb2f77556760364cfcce0e4522b126fdbe21469aaa756e"
                        .into(),
                }),
                "active-html-test" => Ok(StoredResource {
                    display_name: "preview.html".into(),
                    media_type: "text/html".into(),
                    bytes: bytes::Bytes::from_static(b"<script>alert(1)</script>"),
                    sha256: "3a86d1ad640f1586c7f9e7ff31e42a009f41f7f609f732875c41226d3f3a5583"
                        .into(),
                }),
                "corrupt-resource-test" => Err(ServiceError::CorruptResource),
                _ => Err(ServiceError::NotFound),
            }
        }

        async fn session_export(
            &self,
            session_id: &SessionId,
        ) -> Result<bytes::Bytes, ServiceError> {
            match session_id.as_str() {
                "missing-export" => Err(ServiceError::NotFound),
                "oversized-export" => Err(ServiceError::PayloadTooLarge),
                "blocked-export" => {
                    self.export_started.notify_one();
                    self.export_release.notified().await;
                    Ok(bytes::Bytes::from_static(
                        br#"{"format":"ygg-session-export","redacted":true}"#,
                    ))
                }
                _ => Ok(bytes::Bytes::from_static(
                    br#"{"format":"ygg-session-export","redacted":true}"#,
                )),
            }
        }

        async fn usage_stats(
            &self,
            period: crate::UsagePeriod,
        ) -> Result<crate::UsageStats, ServiceError> {
            Ok(crate::UsageStats {
                period,
                prompt_tokens: 11,
                completion_tokens: 7,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
                cache_write_1h_tokens: 1,
                reasoning_tokens: 2,
                total_tokens: 21,
                request_count: 2,
                models: vec![crate::ModelUsage {
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    prompt_tokens: 11,
                    completion_tokens: 7,
                    cache_read_tokens: 3,
                    cache_write_tokens: 2,
                    cache_write_1h_tokens: 1,
                    reasoning_tokens: 2,
                    total_tokens: 21,
                    request_count: 2,
                }],
                models_truncated: false,
            })
        }

        async fn usage_lifetime(&self) -> Result<crate::LifetimeUsage, ServiceError> {
            Ok(crate::LifetimeUsage {
                prompt_tokens: 110,
                completion_tokens: 70,
                cache_read_tokens: 30,
                cache_write_tokens: 20,
                cache_write_1h_tokens: 10,
                reasoning_tokens: 20,
                total_tokens: 210,
                request_count: 20,
                models: vec![crate::ModelUsage {
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    prompt_tokens: 110,
                    completion_tokens: 70,
                    cache_read_tokens: 30,
                    cache_write_tokens: 20,
                    cache_write_1h_tokens: 10,
                    reasoning_tokens: 20,
                    total_tokens: 210,
                    request_count: 20,
                }],
                models_truncated: false,
                first_request_at_ms: Some(1_700_000_000_000),
                last_request_at_ms: Some(1_700_086_400_000),
            })
        }

        async fn usage_activity(&self) -> Result<crate::UsageActivity, ServiceError> {
            Ok(crate::UsageActivity {
                days: vec![crate::UsageActivityDay {
                    date: "2025-01-02".into(),
                    tokens: 21,
                    request_count: 2,
                }],
                current_streak: 2,
                longest_streak: 5,
            })
        }

        fn authority_ceiling(&self) -> AuthorityProfile {
            AuthorityProfile::Workspace
        }

        fn authority_profiles(&self) -> Vec<AuthorityProfile> {
            vec![AuthorityProfile::ReadOnly, AuthorityProfile::Workspace]
        }

        fn model_catalog(&self) -> Vec<ModelSummary> {
            vec![ModelSummary {
                id: "mock-model".into(),
                name: "Mock model".into(),
                provider: "mock".into(),
                local: true,
                available: true,
                reasoning: vec!["off".into()],
                default_reasoning: Some("off".into()),
                input_pricing: None,
                input_modalities: vec![InputModality::Text],
            }]
        }

        fn theme_catalog(&self) -> Vec<ThemeOption> {
            vec![ThemeOption {
                id: ThemeId::new("mock-theme").unwrap(),
                theme: ThemeDto {
                    name: "Mock theme".into(),
                    source: ThemeSourceClass::Bundled,
                    revision: 1,
                    scheme: ColorScheme::Dark,
                    density: ThemeDensity::Comfortable,
                    motion: ThemeMotion::Full,
                    typography: ThemeTypography {
                        body_family: "system-ui".into(),
                        mono_family: "ui-monospace".into(),
                        body_size: 17,
                        display_ratio_milli: 1235,
                    },
                    colors: BTreeMap::new(),
                    roles: BTreeMap::new(),
                },
            }]
        }

        fn selected_theme_id(&self) -> ThemeId {
            ThemeId::new("mock-theme").unwrap()
        }

        async fn list_projects(&self) -> Result<Vec<crate::ProjectSummary>, ServiceError> {
            Ok(Vec::new())
        }

        async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ServiceError> {
            Ok(self
                .seeds
                .lock()
                .unwrap()
                .values()
                .map(|seed| seed.summary.clone())
                .collect())
        }

        async fn create_session(
            &self,
            request: CreateSessionRequest,
        ) -> Result<Self::Driver, ServiceError> {
            self.creates.fetch_add(1, Ordering::Relaxed);
            let number = self.next_session.fetch_add(1, Ordering::Relaxed);
            let id = SessionId::new(format!("fresh-{number}")).unwrap();
            let seed = seed(
                id.clone(),
                request.authority,
                request.provisional,
                number as u64 + 1,
            );
            self.seeds.lock().unwrap().insert(id, seed.clone());
            Ok(MockDriver(seed, self.goals.clone()))
        }

        async fn open_session(&self, session_id: &SessionId) -> Result<Self::Driver, ServiceError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            self.seeds
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .map(|seed| MockDriver(seed, self.goals.clone()))
                .ok_or(ServiceError::NotFound)
        }
    }

    fn model_selection() -> ModelSelection {
        ModelSelection {
            provider: "mock".into(),
            model: "mock-model".into(),
            reasoning: "off".into(),
        }
    }

    fn seed(
        id: SessionId,
        authority: AuthorityProfile,
        provisional: bool,
        generation: u64,
    ) -> SessionSeed {
        let model = model_selection();
        SessionSeed {
            summary: SessionSummary {
                id: id.clone(),
                project_id: None,
                title: "Session".into(),
                tags: Vec::new(),
                created_at_ms: generation,
                modified_at_ms: generation,
                pinned: false,
                archived: false,
                lifecycle: crate::SessionCatalogState::Active,
                retention: None,
                forked_from: None,
                provisional,
                live_state: SessionLiveState::Idle,
                attention: AttentionState::None,
                pull_request: None,
                owner: ActorOwnerState::Hosted,
                model: model.clone(),
            },
            snapshot: SessionSnapshot {
                session_id: id,
                delegated_parent_session_id: None,
                actor_generation: generation.max(1),
                cursor: SessionCursor::zero(generation.max(1)),
                durable_head: None,
                branches: crate::SessionBranchGraph::default(),
                live_state: SessionLiveState::Idle,
                active_run_id: None,
                model,
                authority,
                context: ContextUsage::default(),
                items: Vec::new(),
                extension_presentations: Vec::new(),
                pending_requests: Vec::new(),
                sources: Vec::new(),
                artifacts: Vec::new(),
            },
        }
    }

    async fn request_bytes(address: SocketAddr, request: Vec<u8>) -> Vec<u8> {
        tokio::task::spawn_blocking(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream.write_all(&request).unwrap();
            let mut response = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        response.extend_from_slice(&buffer[..read]);
                        if let Some(header_end) =
                            response.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&response[..header_end]);
                            let content_length = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            });
                            if content_length.is_some_and(|length| {
                                response.len()
                                    >= header_end.saturating_add(4).saturating_add(length)
                            }) {
                                break;
                            }
                        }
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(error) => panic!("response read failed: {error}"),
                }
            }
            response
        })
        .await
        .unwrap()
    }

    async fn request(address: SocketAddr, request: String) -> String {
        String::from_utf8(request_bytes(address, request.into_bytes()).await).unwrap()
    }

    #[cfg(unix)]
    type TerminalSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    #[cfg(unix)]
    async fn terminal_socket(address: SocketAddr, cookie: &str) -> TerminalSocket {
        let mut request = format!("ws://{address}/api/v1/terminal")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "origin",
            format!("http://{address}")
                .parse::<TungsteniteHeaderValue>()
                .unwrap(),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse::<TungsteniteHeaderValue>().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    #[cfg(unix)]
    async fn events_socket(address: SocketAddr, cookie: &str) -> TerminalSocket {
        let mut request = format!("ws://{address}/api/v1/events")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "origin",
            format!("http://{address}")
                .parse::<TungsteniteHeaderValue>()
                .unwrap(),
        );
        request
            .headers_mut()
            .insert("cookie", cookie.parse::<TungsteniteHeaderValue>().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    #[cfg(unix)]
    async fn next_terminal_message(socket: &mut TerminalSocket) -> serde_json::Value {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
                .await
                .expect("terminal message timed out")
                .expect("terminal websocket closed unexpectedly")
                .expect("terminal websocket message failed");
            match frame {
                TungsteniteMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                TungsteniteMessage::Ping(_) | TungsteniteMessage::Pong(_) => {}
                TungsteniteMessage::Close(frame) => {
                    panic!("terminal websocket closed unexpectedly: {frame:?}")
                }
                frame => panic!("unexpected terminal websocket frame: {frame:?}"),
            }
        }
    }

    fn get_request(address: SocketAddr, path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
    }

    fn authenticated_get_request(address: SocketAddr, path: &str, cookie: &str) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
        )
    }

    fn authenticated_json_post_request(
        address: SocketAddr,
        path: &str,
        cookie: &str,
        body: &str,
    ) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn exchange_request(address: SocketAddr, token: &str) -> String {
        get_request(address, &format!("/__ygg/launch/{token}"))
    }

    fn terminal_upgrade_request(
        address: SocketAddr,
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> String {
        let mut request = format!(
            "GET /api/v1/terminal HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
        );
        if let Some(cookie) = cookie {
            request.push_str(&format!("Cookie: {cookie}\r\n"));
        }
        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("\r\n");
        request
    }

    fn response_header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
        response
            .split("\r\n")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .find_map(|line| {
                let (header_name, value) = line.split_once(':')?;
                header_name
                    .eq_ignore_ascii_case(name)
                    .then_some(value.trim())
            })
    }

    fn response_json(response: &str) -> serde_json::Value {
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        serde_json::from_str(body).unwrap()
    }

    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IEND");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    fn upload_request(
        address: SocketAddr,
        cookie: Option<&str>,
        origin: Option<&str>,
        media_type: &str,
        body: &[u8],
    ) -> Vec<u8> {
        let mut request = format!(
            "POST /api/v1/attachments?displayName=alignment.png HTTP/1.1\r\nHost: {address}\r\nContent-Type: {media_type}\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(cookie) = cookie {
            request.push_str(&format!("Cookie: {cookie}\r\n"));
        }
        if let Some(origin) = origin {
            request.push_str(&format!("Origin: {origin}\r\n"));
        }
        request.push_str("Connection: close\r\n\r\n");
        let mut bytes = request.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn binary_response_header<'a>(response: &'a [u8], name: &str) -> Option<&'a str> {
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&response[..header_end]).ok()?;
        response_header(headers, name)
    }

    fn binary_response_body(response: &[u8]) -> &[u8] {
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        &response[header_end + 4..]
    }

    #[test]
    fn content_length_is_unique_ascii_decimal() {
        let mut headers = HeaderMap::new();
        assert_eq!(declared_content_length(&headers), Ok(None));

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("42"));
        assert_eq!(declared_content_length(&headers), Ok(Some(42)));

        for invalid in ["", "+1", "-1", " 1", "1 ", "1, 1", "1a"] {
            let value = HeaderValue::from_bytes(invalid.as_bytes()).unwrap();
            headers.insert(CONTENT_LENGTH, value);
            assert_eq!(declared_content_length(&headers), Err(()), "{invalid:?}");
        }

        headers.remove(CONTENT_LENGTH);
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("1"));
        headers.append(CONTENT_LENGTH, HeaderValue::from_static("1"));
        assert_eq!(declared_content_length(&headers), Err(()));

        headers.remove(CONTENT_LENGTH);
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&format!("{}0", usize::MAX)).unwrap(),
        );
        assert_eq!(declared_content_length(&headers), Err(()));
    }

    #[tokio::test]
    async fn companion_admin_bodies_enforce_declared_length_and_bodyless_framing() {
        let mut exact = Request::new(Body::from("{}"));
        exact
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("2"));
        assert_eq!(
            companion_admin_body(exact, MAX_COMPANION_ADMIN_REQUEST_BYTES)
                .await
                .unwrap(),
            "{}"
        );

        let mut mismatch = Request::new(Body::from("{}"));
        mismatch
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("1"));
        assert_eq!(
            companion_admin_body(mismatch, MAX_COMPANION_ADMIN_REQUEST_BYTES)
                .await
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );

        let mut oversized = Request::new(Body::empty());
        oversized
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("1025"));
        assert_eq!(
            companion_admin_body(oversized, MAX_COMPANION_ADMIN_REQUEST_BYTES)
                .await
                .unwrap_err()
                .status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let mut transfer_encoded = Request::new(Body::empty());
        transfer_encoded
            .headers_mut()
            .insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        assert_eq!(
            companion_admin_body(transfer_encoded, 0)
                .await
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn static_asset_allowlist_excludes_dotfiles_and_source_maps() {
        assert!(safe_relative_path(FilePath::new("assets/app.js")));
        assert!(safe_relative_path(FilePath::new(
            "assets/chunk-FilesPanel.js"
        )));
        assert!(safe_relative_path(FilePath::new(
            "assets/chunk-file-languages.js"
        )));
        assert!(safe_relative_path(FilePath::new(
            "assets/chunk-rolldown-runtime.js"
        )));
        assert!(safe_relative_path(FilePath::new(
            "assets/chunk-MarkdownMessage.js"
        )));
        let bundle = WebBundle::embedded().unwrap();
        assert_eq!(
            bundle.asset("assets/app.js").unwrap().media_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-FilesPanel.js")
                .unwrap()
                .media_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-file-languages.js")
                .unwrap()
                .media_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-rolldown-runtime.js")
                .unwrap()
                .media_type,
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            bundle
                .asset("assets/chunk-MarkdownMessage.js")
                .unwrap()
                .media_type,
            "text/javascript; charset=utf-8"
        );
        assert!(!safe_relative_path(FilePath::new(".git/config")));
        assert!(!safe_relative_path(FilePath::new("assets/.secret")));
        assert!(bundle.asset("assets/app.js.map").is_none());
        assert!(bundle.asset("private.txt").is_none());
    }

    #[test]
    fn companion_rate_limits_are_isolated_by_device_and_from_the_owner() {
        let limiter = RateLimiter::default();
        let companion = Extension(TransportPrincipal::Paired {
            device_id: DeviceId::new("device-rate-limit").unwrap(),
        });
        let other_companion = Extension(TransportPrincipal::Paired {
            device_id: DeviceId::new("device-other-rate-limit").unwrap(),
        });
        let owner = Extension(TransportPrincipal::LoopbackOwner);

        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.admit(Some(&companion)));
        }
        assert!(!limiter.admit(Some(&companion)));
        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.admit(Some(&other_companion)));
        }
        assert!(!limiter.admit(Some(&other_companion)));
        for _ in 0..RATE_LIMIT_REQUESTS {
            assert!(limiter.admit(Some(&owner)));
        }
        assert!(!limiter.admit(Some(&owner)));

        assert!(!limiter.admit_remote_read(Some(&companion)));
        for _ in 0..(RATE_LIMIT_REQUESTS * 2) {
            assert!(limiter.admit_remote_read(Some(&owner)));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn event_websocket_stops_with_the_server() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let mut socket = events_socket(address, &cookie).await;

        let shutdown = tokio::spawn(async move { server.shutdown().await });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await {
                    Some(Ok(TungsteniteMessage::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("event WebSocket remained open after server shutdown");
        tokio::time::timeout(Duration::from_secs(3), shutdown)
            .await
            .expect("server shutdown timed out")
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_socket_authenticates_replays_and_stops_with_the_server() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let workspace = tempfile::tempdir().unwrap();
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: Some(TerminalConfig {
                    cwd: workspace.path().to_path_buf(),
                    shell: Some(PathBuf::from("/bin/sh")),
                }),
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let origin = format!("http://{address}");

        let unauthenticated = request(
            address,
            terminal_upgrade_request(address, None, Some(&origin)),
        )
        .await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let missing_origin = request(
            address,
            terminal_upgrade_request(address, Some(cookie), None),
        )
        .await;
        assert!(missing_origin.starts_with("HTTP/1.1 403"));

        let mut first = terminal_socket(address, cookie).await;
        first
            .send(TungsteniteMessage::Text("{not-json".into()))
            .await
            .unwrap();
        let malformed = next_terminal_message(&mut first).await;
        assert_eq!(malformed["type"], "error");

        first
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "input",
                    "id": "f".repeat(32),
                    "data": "echo rejected\\n",
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let unattached = next_terminal_message(&mut first).await;
        assert_eq!(unattached["type"], "error");

        first
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "open",
                    "cols": 80,
                    "rows": 24,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let opened = next_terminal_message(&mut first).await;
        assert_eq!(opened["type"], "opened");
        let id = opened["id"].as_str().unwrap().to_owned();
        let owner_key = opened["ownerKey"].as_str().unwrap().to_owned();

        first
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "resize",
                    "id": id.clone(),
                    "cols": 100,
                    "rows": 30,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        first
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "input",
                    "id": id.clone(),
                    "data": "printf 'terminal websocket works\\n'\n",
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        loop {
            let output = next_terminal_message(&mut first).await;
            if output["type"] == "output"
                && output["data"]
                    .as_str()
                    .is_some_and(|data| data.contains("terminal websocket works"))
            {
                break;
            }
        }

        first
            .send(TungsteniteMessage::Text(
                serde_json::json!({ "type": "detach", "id": id.clone() })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        drop(first);

        let mut second = terminal_socket(address, cookie).await;
        second
            .send(TungsteniteMessage::Text(
                serde_json::json!({
                    "type": "open",
                    "ownerKey": owner_key,
                    "cols": 100,
                    "rows": 30,
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let reopened = next_terminal_message(&mut second).await;
        assert_eq!(reopened["type"], "opened");
        assert_eq!(reopened["id"], id);
        assert!(reopened["replay"]
            .as_str()
            .is_some_and(|replay| replay.contains("terminal websocket works")));

        let shutdown = tokio::spawn(async move { server.shutdown().await });
        loop {
            let message = next_terminal_message(&mut second).await;
            if message["type"] == "exit" {
                assert_eq!(message["id"], id);
                break;
            }
        }
        drop(second);
        tokio::time::timeout(Duration::from_secs(3), shutdown)
            .await
            .expect("server shutdown timed out")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bootstrap_is_fresh_per_root_client_and_explicit_restore_does_not_create() {
        let host = Arc::new(MockHost::new());
        let existing = host.insert_existing("durable-existing");
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        assert_eq!(host.creates.load(Ordering::Relaxed), 0);

        let unauthenticated = request(address, get_request(address, "/api/v1/bootstrap")).await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));
        let invalid = request(address, exchange_request(address, &"0".repeat(64))).await;
        assert!(invalid.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        assert!(exchanged.starts_with("HTTP/1.1 303"));
        assert_eq!(response_header(&exchanged, "location"), Some("/"));
        let set_cookie = response_header(&exchanged, "set-cookie").unwrap();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap();
        let reused = request(address, exchange_request(address, &server.launch_token)).await;
        assert!(reused.starts_with("HTTP/1.1 401"));

        let project_catalog = request(
            address,
            authenticated_get_request(address, "/api/v1/projects", cookie),
        )
        .await;
        assert!(project_catalog.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&project_catalog)["protocol"], 1);
        assert_eq!(
            response_json(&project_catalog)["host"]["id"],
            "host-transport-test"
        );
        assert_eq!(
            response_json(&project_catalog)["lifecycleMutationsSupported"],
            false
        );
        assert_eq!(response_json(&project_catalog)["importSupported"], false);
        assert_eq!(host.creates.load(Ordering::Relaxed), 0);

        let inventory = request(
            address,
            authenticated_get_request(address, "/api/v1/bootstrap?inventoryOnly=true", cookie),
        )
        .await;
        assert!(inventory.starts_with("HTTP/1.1 200"));
        let inventory = response_json(&inventory);
        assert!(inventory["selectedSessionId"].is_null());
        assert!(inventory["selectedSession"].is_null());
        assert!(inventory["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|summary| summary["id"] == existing.as_str()));
        assert_eq!(host.creates.load(Ordering::Relaxed), 0);
        assert_eq!(host.opens.load(Ordering::Relaxed), 0);

        let conflicting_inventory = request(
            address,
            authenticated_get_request(
                address,
                &format!(
                    "/api/v1/bootstrap?inventoryOnly=true&selectedSessionId={}",
                    existing.as_str()
                ),
                cookie,
            ),
        )
        .await;
        assert!(conflicting_inventory.starts_with("HTTP/1.1 400"));
        assert_eq!(host.creates.load(Ordering::Relaxed), 0);
        assert_eq!(host.opens.load(Ordering::Relaxed), 0);

        let first = request(
            address,
            authenticated_get_request(address, "/api/v1/bootstrap", cookie),
        )
        .await;
        let second = request(
            address,
            authenticated_get_request(address, "/api/v1/bootstrap", cookie),
        )
        .await;
        assert!(first.starts_with("HTTP/1.1 200"));
        assert!(second.starts_with("HTTP/1.1 200"));
        let first_id = response_json(&first)["selectedSessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        let second_id = response_json(&second)["selectedSessionId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(first_id, second_id);
        assert_eq!(host.creates.load(Ordering::Relaxed), 2);

        let restored = request(
            address,
            authenticated_get_request(
                address,
                &format!("/api/v1/bootstrap?selectedSessionId={}", existing.as_str()),
                cookie,
            ),
        )
        .await;
        assert!(restored.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_json(&restored)["selectedSessionId"]
                .as_str()
                .unwrap(),
            existing.as_str()
        );
        assert_eq!(host.creates.load(Ordering::Relaxed), 2);
        assert_eq!(host.opens.load(Ordering::Relaxed), 1);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn usage_transport_is_authenticated_and_validates_periods() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();

        let unauthenticated = request(
            address,
            get_request(address, "/api/v1/usage/stats?period=daily"),
        )
        .await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let invalid = request(
            address,
            authenticated_get_request(address, "/api/v1/usage/stats?period=monthly", cookie),
        )
        .await;
        assert!(invalid.starts_with("HTTP/1.1 400"));

        let daily = request(
            address,
            authenticated_get_request(address, "/api/v1/usage/stats?period=daily", cookie),
        )
        .await;
        assert!(daily.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&daily)["period"], "daily");
        assert_eq!(response_json(&daily)["prompt_tokens"], 11);
        assert_eq!(response_json(&daily)["completion_tokens"], 7);
        assert_eq!(response_json(&daily)["cache_read_tokens"], 3);
        assert_eq!(response_json(&daily)["cache_write_tokens"], 2);
        assert_eq!(response_json(&daily)["cache_write_1h_tokens"], 1);
        assert_eq!(response_json(&daily)["reasoning_tokens"], 2);
        assert_eq!(response_json(&daily)["total_tokens"], 21);
        assert_eq!(response_json(&daily)["request_count"], 2);
        assert_eq!(response_json(&daily)["models"][0]["provider"], "mock");
        assert_eq!(response_json(&daily)["models"][0]["model"], "mock-model");
        assert_eq!(response_json(&daily)["models"][0]["total_tokens"], 21);
        assert_eq!(response_json(&daily)["models_truncated"], false);

        let weekly = request(
            address,
            authenticated_get_request(address, "/api/v1/usage/stats?period=weekly", cookie),
        )
        .await;
        assert_eq!(response_json(&weekly)["period"], "weekly");

        let lifetime = request(
            address,
            authenticated_get_request(address, "/api/v1/usage/lifetime", cookie),
        )
        .await;
        assert_eq!(response_json(&lifetime)["total_tokens"], 210);
        assert_eq!(response_json(&lifetime)["request_count"], 20);
        assert_eq!(response_json(&lifetime)["cache_read_tokens"], 30);
        assert_eq!(response_json(&lifetime)["cache_write_tokens"], 20);
        assert_eq!(response_json(&lifetime)["cache_write_1h_tokens"], 10);

        let activity = request(
            address,
            authenticated_get_request(address, "/api/v1/usage/activity", cookie),
        )
        .await;
        assert_eq!(response_json(&activity)["current_streak"], 2);
        assert_eq!(response_json(&activity)["longest_streak"], 5);
        assert_eq!(response_json(&activity)["days"][0]["date"], "2025-01-02");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_file_transport_is_authenticated_validated_and_conflict_aware() {
        let host = Arc::new(MockHost::with_project_files());
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();

        let unauthenticated = request(
            address,
            get_request(address, "/api/v1/fs/project-test/tree"),
        )
        .await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();

        let invalid_project = request(
            address,
            authenticated_get_request(address, "/api/v1/fs/project%24/tree", cookie),
        )
        .await;
        assert!(invalid_project.starts_with("HTTP/1.1 400"));

        let tree = request(
            address,
            authenticated_get_request(address, "/api/v1/fs/project-test/tree", cookie),
        )
        .await;
        assert!(tree.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&tree)["path"], "");
        assert_eq!(response_json(&tree)["entries"][0]["name"], "src");

        let invalid_range = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/fs/project-test/read?path=src%2Fmain.rs&startLine=0",
                cookie,
            ),
        )
        .await;
        assert!(invalid_range.starts_with("HTTP/1.1 400"));

        let file = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/fs/project-test/read?path=src%2Fmain.rs&startLine=1&endLine=1",
                cookie,
            ),
        )
        .await;
        assert!(file.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&file)["path"], "src/main.rs");
        assert!(response_json(&file)["sha256"].is_string());

        let search = request(
            address,
            authenticated_get_request(address, "/api/v1/fs/project-test/search?query=main", cookie),
        )
        .await;
        assert!(search.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&search)["hits"], serde_json::json!([]));

        let expected_sha256 = "a".repeat(64);
        let write_body = serde_json::json!({
            "path": "src/main.rs",
            "content": "updated",
            "expectedSha256": expected_sha256,
            "force": true,
        })
        .to_string();
        let write = request(
            address,
            authenticated_json_post_request(
                address,
                "/api/v1/fs/project-test/write",
                cookie,
                &write_body,
            ),
        )
        .await;
        assert!(write.starts_with("HTTP/1.1 200"));
        assert_eq!(response_json(&write)["path"], "src/main.rs");
        assert_eq!(
            host.project_file_writes.lock().unwrap().as_slice(),
            [(
                "src/main.rs".into(),
                "updated".into(),
                expected_sha256,
                true
            )]
        );

        let malformed_write = request(
            address,
            authenticated_json_post_request(
                address,
                "/api/v1/fs/project-test/write",
                cookie,
                r#"{"path":"src/main.rs","content":"updated"}"#,
            ),
        )
        .await;
        assert!(malformed_write.starts_with("HTTP/1.1 400"));

        let conflict_body = serde_json::json!({
            "path": "conflict",
            "content": "updated",
            "expectedSha256": "a".repeat(64),
        })
        .to_string();
        let conflict = request(
            address,
            authenticated_json_post_request(
                address,
                "/api/v1/fs/project-test/write",
                cookie,
                &conflict_body,
            ),
        )
        .await;
        assert!(conflict.starts_with("HTTP/1.1 409"));
        assert_eq!(response_json(&conflict)["error"]["code"], "locked");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn project_file_transport_honors_host_capabilities() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();

        let unavailable = request(
            address,
            authenticated_get_request(address, "/api/v1/fs/project-test/tree", cookie),
        )
        .await;
        assert!(unavailable.starts_with("HTTP/1.1 503"));
        assert_eq!(response_json(&unavailable)["error"]["code"], "unavailable");
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn goal_transport_supports_lifecycle_and_restart_persistence() {
        let host = Arc::new(MockHost::new());
        let session_id = host.insert_existing("goal-transport-session");
        let goal_store_root = host._goal_root.path().join("goals");
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            Arc::clone(&supervisor),
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: goal_store_root.clone(),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let path = format!("/api/v1/sessions/{}/goal", session_id.as_str());
        let created = request(
            address,
            authenticated_json_post_request(
                address,
                &path,
                &cookie,
                r#"{"objective":"Ship the README","turnBudget":10}"#,
            ),
        )
        .await;
        assert!(created.starts_with("HTTP/1.1 200"));
        assert_eq!(response_header(&created, "x-ygg-goal-revision"), Some("1"));
        assert_eq!(response_json(&created)["objective"], "Ship the README");
        assert_eq!(response_json(&created)["status"], "active");
        assert_eq!(response_json(&created)["turnBudget"], 10);
        server.shutdown().await.unwrap();

        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root,
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let restored = request(address, authenticated_get_request(address, &path, cookie)).await;
        assert!(restored.starts_with("HTTP/1.1 200"));
        assert_eq!(response_header(&restored, "x-ygg-goal-revision"), Some("1"));
        assert_eq!(response_json(&restored)["objective"], "Ship the README");

        let paused = request(
            address,
            authenticated_json_post_request(address, &path, cookie, r#"{"action":"pause"}"#),
        )
        .await;
        assert!(paused.starts_with("HTTP/1.1 200"));
        assert_eq!(response_header(&paused, "x-ygg-goal-revision"), Some("2"));
        assert_eq!(response_json(&paused)["status"], "paused");
        let resumed = request(
            address,
            authenticated_json_post_request(address, &path, cookie, r#"{"action":"resume"}"#),
        )
        .await;
        assert!(resumed.starts_with("HTTP/1.1 200"));
        assert_eq!(response_header(&resumed, "x-ygg-goal-revision"), Some("3"));
        assert_eq!(response_json(&resumed)["status"], "active");
        let cleared = request(
            address,
            authenticated_json_post_request(address, &path, cookie, r#"{"action":"clear"}"#),
        )
        .await;
        assert!(cleared.starts_with("HTTP/1.1 200"));
        assert_eq!(response_header(&cleared, "x-ygg-goal-revision"), Some("4"));
        assert_eq!(response_json(&cleared), serde_json::Value::Null);
        let after_clear = request(address, authenticated_get_request(address, &path, cookie)).await;
        assert!(after_clear.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&after_clear, "x-ygg-goal-revision"),
            Some("4")
        );
        assert_eq!(response_json(&after_clear), serde_json::Value::Null);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loopback_transport_rejects_cross_origin_and_oversized_requests() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();

        let forbidden_host = request(
            address,
            "GET / HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n".into(),
        )
        .await;
        assert!(forbidden_host.starts_with("HTTP/1.1 403"));

        let forbidden_origin = request(
            address,
            format!(
                "GET / HTTP/1.1\r\nHost: {address}\r\nOrigin: https://attacker.example\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(forbidden_origin.starts_with("HTTP/1.1 403"));

        let unauthenticated_websocket = request(
            address,
            format!(
                "GET /api/v1/events HTTP/1.1\r\nHost: {address}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            ),
        )
        .await;
        assert!(unauthenticated_websocket.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        assert!(exchanged.starts_with("HTTP/1.1 303"));
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let oversized = request(
            address,
            format!(
                "POST /api/v1/commands/host HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_COMMAND_BYTES + 1
            ),
        )
        .await;
        assert!(oversized.starts_with("HTTP/1.1 413"));

        let body_on_get = request(
            address,
            format!(
                "GET /api/v1/projects HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
            ),
        )
        .await;
        assert!(body_on_get.starts_with("HTTP/1.1 400"));

        let allowed = request(address, get_request(address, "/")).await;
        assert!(allowed.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&allowed, "content-security-policy"),
            Some(
                "default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self' data:; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'"
            )
        );
        assert!(allowed
            .to_ascii_lowercase()
            .contains("cache-control: no-store"));
        assert!(allowed
            .to_ascii_lowercase()
            .contains("x-content-type-options: nosniff"));
        assert_eq!(
            response_header(&allowed, "x-ygg-web-bundle"),
            Some(include_str!("../web/bundle.sha256"))
        );
        assert_eq!(
            allowed.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            include_bytes!("../web/index.html")
        );

        let javascript = request(address, get_request(address, "/assets/app.js")).await;
        assert!(javascript.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&javascript, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            javascript.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            include_bytes!("../web/assets/app.js")
        );
        let files_panel =
            request(address, get_request(address, "/assets/chunk-FilesPanel.js")).await;
        assert!(files_panel.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&files_panel, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            files_panel.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            include_bytes!("../web/assets/chunk-FilesPanel.js")
        );
        let jsx_runtime = request(
            address,
            get_request(address, "/assets/chunk-rolldown-runtime.js"),
        )
        .await;
        assert!(jsx_runtime.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&jsx_runtime, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            jsx_runtime.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            include_bytes!("../web/assets/chunk-rolldown-runtime.js")
        );
        let markdown = request(
            address,
            get_request(address, "/assets/chunk-MarkdownMessage.js"),
        )
        .await;
        assert!(markdown.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&markdown, "content-type"),
            Some("text/javascript; charset=utf-8")
        );
        assert_eq!(
            markdown.split_once("\r\n\r\n").unwrap().1.as_bytes(),
            include_bytes!("../web/assets/chunk-MarkdownMessage.js")
        );
        let source_map = request(address, get_request(address, "/assets/app.js.map")).await;
        assert!(source_map.starts_with("HTTP/1.1 404"));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn attachment_transport_is_authenticated_bounded_and_path_free() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let image = png();

        let unauthenticated = request_bytes(
            address,
            upload_request(address, None, None, "image/png", &image),
        )
        .await;
        assert!(unauthenticated.starts_with(b"HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let cross_origin = request_bytes(
            address,
            upload_request(
                address,
                Some(cookie),
                Some("https://attacker.example"),
                "image/png",
                &image,
            ),
        )
        .await;
        assert!(cross_origin.starts_with(b"HTTP/1.1 403"));

        let declared_oversize = request(
            address,
            format!(
                "POST /api/v1/attachments?displayName=large.png HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_ATTACHMENT_FILE_BYTES + 1
            ),
        )
        .await;
        assert!(declared_oversize.starts_with("HTTP/1.1 413"));

        let spoof = request_bytes(
            address,
            upload_request(address, Some(cookie), None, "image/jpeg", &image),
        )
        .await;
        assert!(spoof.starts_with(b"HTTP/1.1 400"));

        let uploaded = request_bytes(
            address,
            upload_request(address, Some(cookie), None, "image/png", &image),
        )
        .await;
        assert!(uploaded.starts_with(b"HTTP/1.1 200"));
        let uploaded_text = String::from_utf8(uploaded).unwrap();
        let uploaded_json = response_json(&uploaded_text);
        let fields = uploaded_json.as_object().unwrap();
        assert_eq!(fields.len(), 4);
        for expected in ["handle", "displayName", "mediaType", "byteLen"] {
            assert!(fields.contains_key(expected));
        }
        let reference: AttachmentRef = serde_json::from_value(uploaded_json).unwrap();
        assert_eq!(reference.display_name, "alignment.png");
        assert_eq!(reference.media_type, "image/png");
        assert_eq!(reference.byte_len, image.len() as u64);
        assert_eq!(reference.handle.len(), 64);
        assert!(reference
            .handle
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));

        let unauthenticated_content = request(
            address,
            get_request(
                address,
                &format!("/api/v1/attachments/{}", reference.handle),
            ),
        )
        .await;
        assert!(unauthenticated_content.starts_with("HTTP/1.1 401"));

        let content = request_bytes(
            address,
            authenticated_get_request(
                address,
                &format!("/api/v1/attachments/{}", reference.handle),
                cookie,
            )
            .into_bytes(),
        )
        .await;
        assert!(content.starts_with(b"HTTP/1.1 200"));
        assert_eq!(
            binary_response_header(&content, "content-type"),
            Some("image/png")
        );
        assert_eq!(
            binary_response_header(&content, "x-content-type-options"),
            Some("nosniff")
        );
        let expected_content_length = image.len().to_string();
        assert_eq!(
            binary_response_header(&content, "content-length"),
            Some(expected_content_length.as_str())
        );
        assert_eq!(
            binary_response_header(&content, "content-disposition"),
            Some("inline; filename=\"alignment.png\"")
        );
        assert_eq!(
            binary_response_header(&content, "cache-control"),
            Some("private, max-age=31536000, immutable")
        );
        let etag = binary_response_header(&content, "etag").unwrap();
        assert_eq!(etag.len(), 66);
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        assert!(etag[1..65].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(binary_response_body(&content), image);

        let unknown = request(
            address,
            authenticated_get_request(
                address,
                &format!("/api/v1/attachments/{}", "a".repeat(64)),
                cookie,
            ),
        )
        .await;
        assert!(unknown.starts_with("HTTP/1.1 404"));

        let mut chunked = format!(
            "POST /api/v1/attachments?displayName=chunked.png HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Type: image/png\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            MAX_ATTACHMENT_FILE_BYTES + 1
        )
        .into_bytes();
        chunked.extend(std::iter::repeat_n(0u8, MAX_ATTACHMENT_FILE_BYTES + 1));
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let chunked_oversize = request_bytes(address, chunked).await;
        assert!(chunked_oversize.starts_with(b"HTTP/1.1 413"));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_export_is_authenticated_redacted_bounded_and_single_flight() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let path = "/api/v1/sessions/safe-export/export";

        let unauthenticated = request(address, get_request(address, path)).await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));
        let forbidden_host = request(
            address,
            format!("GET {path} HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert!(forbidden_host.starts_with("HTTP/1.1 403"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let cross_origin = request(
            address,
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nOrigin: https://attacker.example\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert!(cross_origin.starts_with("HTTP/1.1 403"));

        let with_query = request(
            address,
            authenticated_get_request(address, &format!("{path}?raw=true"), cookie),
        )
        .await;
        assert!(with_query.starts_with("HTTP/1.1 400"));
        let with_body = request(
            address,
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        assert!(with_body.starts_with("HTTP/1.1 400"));
        let invalid_id = request(
            address,
            authenticated_get_request(address, "/api/v1/sessions/bad$id/export", cookie),
        )
        .await;
        assert!(invalid_id.starts_with("HTTP/1.1 400"));

        let exported = request(address, authenticated_get_request(address, path, cookie)).await;
        assert!(exported.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&exported, "content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            response_header(&exported, "content-disposition"),
            Some("attachment; filename=\"ygg-session-safe-export.json\"")
        );
        assert_eq!(
            response_header(&exported, "cache-control"),
            Some("no-store")
        );
        assert_eq!(
            response_header(&exported, "x-content-type-options"),
            Some("nosniff")
        );
        assert_eq!(
            response_header(&exported, "referrer-policy"),
            Some("no-referrer")
        );
        assert_eq!(response_header(&exported, "etag"), None);
        let body = exported.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            response_header(&exported, "content-length"),
            Some(body.len().to_string().as_str())
        );
        assert_eq!(body, r#"{"format":"ygg-session-export","redacted":true}"#);
        assert!(!body.contains("includeSecrets"));

        let missing = request(
            address,
            authenticated_get_request(address, "/api/v1/sessions/missing-export/export", cookie),
        )
        .await;
        assert!(missing.starts_with("HTTP/1.1 404"));
        let oversized = request(
            address,
            authenticated_get_request(address, "/api/v1/sessions/oversized-export/export", cookie),
        )
        .await;
        assert!(oversized.starts_with("HTTP/1.1 413"));

        let blocked_request =
            authenticated_get_request(address, "/api/v1/sessions/blocked-export/export", cookie);
        let blocked = tokio::spawn(request(address, blocked_request));
        host.export_started.notified().await;
        let busy = request(address, authenticated_get_request(address, path, cookie)).await;
        assert!(busy.starts_with("HTTP/1.1 503"));
        host.export_release.notify_one();
        assert!(blocked.await.unwrap().starts_with("HTTP/1.1 200"));

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn opaque_resource_transport_requires_auth_and_never_interprets_handles() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(
            host.clone(),
            SupervisorConfig::default(),
        ));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
                terminal: None,
                goal_store_root: host._goal_root.path().join("goals"),
            },
        )
        .await
        .unwrap();
        let address = server.address();
        let path = "/api/v1/sessions/session-resource-test/resources/opaque-resource-test";

        let unauthenticated = request(address, get_request(address, path)).await;
        assert!(unauthenticated.starts_with("HTTP/1.1 401"));

        let exchanged = request(address, exchange_request(address, &server.launch_token)).await;
        let cookie = response_header(&exchanged, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let content = request(address, authenticated_get_request(address, path, cookie)).await;
        assert!(content.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&content, "content-type"),
            Some("text/plain")
        );
        assert_eq!(
            response_header(&content, "content-disposition"),
            Some("inline; filename=\"evidence.txt\"")
        );
        assert_eq!(response_header(&content, "cache-control"), Some("no-store"));
        assert_eq!(response_header(&content, "etag"), None);
        assert_eq!(
            response_header(&content, "cross-origin-resource-policy"),
            Some("same-origin")
        );
        assert_eq!(
            response_header(&content, "x-content-type-options"),
            Some("nosniff")
        );
        assert_eq!(
            response_header(&content, "referrer-policy"),
            Some("no-referrer")
        );
        assert_eq!(
            content.split_once("\r\n\r\n").unwrap().1,
            "structured evidence"
        );

        let traversal = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/sessions/session-resource-test/resources/..%2Fsecret",
                cookie,
            ),
        )
        .await;
        assert!(traversal.starts_with("HTTP/1.1 404"));
        let wrong_session = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/sessions/session-wrong/resources/opaque-resource-test",
                cookie,
            ),
        )
        .await;
        assert!(wrong_session.starts_with("HTTP/1.1 404"));

        let active = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/sessions/session-resource-test/resources/active-html-test",
                cookie,
            ),
        )
        .await;
        assert!(active.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&active, "content-type"),
            Some("application/octet-stream")
        );
        assert_eq!(
            response_header(&active, "content-disposition"),
            Some("attachment; filename=\"preview.html\"")
        );

        let corrupt = request(
            address,
            authenticated_get_request(
                address,
                "/api/v1/sessions/session-resource-test/resources/corrupt-resource-test",
                cookie,
            ),
        )
        .await;
        assert!(corrupt.starts_with("HTTP/1.1 410"));

        for forbidden in [
            format!(
                "GET {path}?download=1 HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nRange: bytes=0-3\r\nConnection: close\r\n\r\n"
            ),
            format!(
                "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
            ),
            format!(
                "HEAD {path}?download=1 HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        ] {
            let response = request(address, forbidden).await;
            assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        }
        server.shutdown().await.unwrap();
    }
}
