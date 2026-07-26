//! Loopback-only HTTP and WebSocket transport for first-party graphical clients.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path as FilePath, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    COOKIE, ETAG, HOST, LOCATION, ORIGIN, REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Semaphore};
use tokio::task::JoinHandle;

use crate::embedded_web::WebBundle;
use crate::{
    AttachmentError, HostCommandEnvelope, HostService, ProtocolValidation, SanitizedError,
    SessionCommandEnvelope, SessionCursor, SessionId, SessionSupervisor, SupervisorError,
    MAX_ATTACHMENT_FILE_BYTES, MAX_COMMAND_BYTES, PROTOCOL_VERSION,
};

const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 8 * 1024;
const RATE_LIMIT_REQUESTS: usize = 240;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_ATTACHMENT_UPLOADS: usize = 4;
const X_YGG_WEB_BUNDLE: HeaderName = HeaderName::from_static("x-ygg-web-bundle");

/// Loopback listener configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopbackConfig {
    /// Requested TCP port. Zero asks the operating system for a free port.
    pub port: u16,
    /// Optional built graphical-shell directory.
    pub web_root: Option<PathBuf>,
}

/// Transport startup or task failure.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// Initial host bootstrap failed.
    #[error("host startup failed")]
    Host(#[from] SupervisorError),
    /// The loopback listener could not be created.
    #[error("loopback listener failed")]
    Io(#[from] std::io::Error),
    /// The server task ended unexpectedly.
    #[error("loopback server task failed")]
    Task(#[from] tokio::task::JoinError),
}

/// Running loopback server.
pub struct LoopbackServer {
    address: SocketAddr,
    launch_token: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

impl LoopbackServer {
    /// Starts a loopback server without allocating a session.
    ///
    /// Each root-client bootstrap creates its own provisional session. A
    /// bootstrap carrying an explicit session id restores that session
    /// instead.
    pub async fn start<H: HostService>(
        supervisor: Arc<SessionSupervisor<H>>,
        config: LoopbackConfig,
    ) -> Result<Self, TransportError> {
        let web_bundle = match config.web_root.as_deref() {
            Some(root) => WebBundle::from_root(root)?,
            None => WebBundle::embedded()?,
        };
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

        let state = Arc::new(TransportState {
            supervisor,
            auth,
            allowed_authorities: AllowedAuthorities::new(address),
            rate_limiter: RateLimiter::default(),
            attachment_uploads: Arc::new(Semaphore::new(MAX_CONCURRENT_ATTACHMENT_UPLOADS)),
            web_bundle,
        });
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
            shutdown: Some(shutdown),
            task: Some(task),
        })
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
        if let Some(task) = self.task.take() {
            task.await??;
        }
        self.shutdown.take();
        Ok(())
    }

    /// Requests graceful shutdown and waits for completion.
    pub async fn shutdown(mut self) -> Result<(), TransportError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
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
    web_bundle: WebBundle,
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
    accepted: StdMutex<VecDeque<Instant>>,
}

impl RateLimiter {
    fn admit(&self) -> bool {
        let now = Instant::now();
        let mut accepted = self.accepted.lock().expect("rate limiter poisoned");
        while accepted
            .front()
            .is_some_and(|instant| now.duration_since(*instant) >= RATE_LIMIT_WINDOW)
        {
            accepted.pop_front();
        }
        if accepted.len() >= RATE_LIMIT_REQUESTS {
            return false;
        }
        accepted.push_back(now);
        true
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

fn build_router<H: HostService>(state: Arc<TransportState<H>>) -> Router {
    Router::new()
        .route("/", get(index::<H>))
        .route("/__ygg/launch/{token}", get(exchange_launch_token::<H>))
        .route("/api/v1/bootstrap", get(bootstrap::<H>))
        .route("/api/v1/sessions/{session_id}", get(session_snapshot::<H>))
        .route(
            "/api/v1/sessions/{session_id}/replay",
            get(session_replay::<H>),
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
            "/api/v1/attachments",
            post(ingest_attachment::<H>)
                .layer(DefaultBodyLimit::max(MAX_ATTACHMENT_FILE_BYTES + 1)),
        )
        .route("/api/v1/attachments/{handle}", get(attachment_content::<H>))
        .route("/api/v1/events", any(events_socket::<H>))
        .route("/{*asset}", get(static_asset::<H>))
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            secure_request::<H>,
        ))
        .with_state(state)
}

async fn ingest_attachment<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    query: Result<Query<AttachmentUploadQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if !state.rate_limiter.admit() {
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
    Path(handle): Path<String>,
) -> Response {
    if !state.rate_limiter.admit() {
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
    query: Result<Query<BootstrapQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if !state.rate_limiter.admit() {
        return rate_limited();
    }
    let selected = match query {
        Ok(Query(query)) => match query.selected_session_id {
            Some(raw) => match SessionId::new(raw) {
                Ok(id) => Some(id),
                Err(_) => return invalid_request(),
            },
            None => None,
        },
        Err(_) => return invalid_request(),
    };
    let result = match selected {
        Some(session_id) => match state.supervisor.open_session(&session_id).await {
            Ok(_) => state.supervisor.bootstrap(&session_id).await,
            Err(error) => Err(error),
        },
        None => state.supervisor.launch(None).await,
    };
    match result {
        Ok(bootstrap) => Json(bootstrap).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn session_snapshot<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    Path(raw_session_id): Path<String>,
) -> Response {
    let session_id = match SessionId::new(raw_session_id) {
        Ok(id) => id,
        Err(_) => return invalid_request(),
    };
    match state.supervisor.session_view(&session_id).await {
        Ok(view) => Json(view.snapshot).into_response(),
        Err(error) => supervisor_error_response(error),
    }
}

async fn session_replay<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    Path(raw_session_id): Path<String>,
    query: Result<Query<ReplayQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
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
    payload: Result<Json<HostCommandEnvelope>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit() {
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
    payload: Result<Json<SessionCommandEnvelope>, JsonRejection>,
) -> Response {
    if !state.rate_limiter.admit() {
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

async fn events_socket<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    upgrade: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
) -> Response {
    if !state.rate_limiter.admit() {
        return rate_limited();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(_) => return invalid_request(),
    };
    let events = state.supervisor.subscribe_events();
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| stream_events(socket, events))
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

async fn secure_request<H: HostService>(
    State(state): State<Arc<TransportState<H>>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let attachment_upload =
        request.method() == Method::POST && request.uri().path() == "/api/v1/attachments";
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
    let fetch_site_allowed = match headers.get("sec-fetch-site") {
        None => true,
        Some(value) => value
            .to_str()
            .ok()
            .is_some_and(|value| matches!(value, "same-origin" | "same-site" | "none")),
    };
    let query_allowed = request
        .uri()
        .query()
        .is_none_or(|query| query.len() <= MAX_QUERY_BYTES);
    let content_length_limit = if attachment_upload {
        state
            .supervisor
            .attachment_policy()
            .map(|policy| policy.max_file_bytes as usize)
            .unwrap_or(MAX_ATTACHMENT_FILE_BYTES)
    } else {
        MAX_COMMAND_BYTES
    };
    let content_length_allowed = match headers.get(CONTENT_LENGTH) {
        None => true,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length <= content_length_limit),
    };
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
                } else {
                    value
                        .split(';')
                        .next()
                        .is_some_and(|media_type| media_type.trim() == "application/json")
                }
            });
    let api_authenticated =
        !request.uri().path().starts_with("/api/v1/") || state.auth.allows_cookie(headers);

    let mut response = if !host_allowed || !origin_allowed || !fetch_site_allowed {
        error_response(
            StatusCode::FORBIDDEN,
            SanitizedError::public(
                crate::ErrorCode::Unauthorized,
                "This request is not allowed by the loopback host.",
            ),
        )
    } else if !api_authenticated {
        authentication_required()
    } else if !query_allowed || !content_length_allowed {
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
            "default-src 'self'; base-uri 'none'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'",
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

    use crate::{
        ActorOwnerState, AttachmentPolicy, AttachmentRef, AttachmentStore, AttentionState,
        AuthorityProfile, ColorScheme, ContextUsage, CreateSessionRequest, DriverCommandOutcome,
        HostCapabilities, HostDescriptor, HostId, InputModality, ModelSelection, ModelSummary,
        ServiceError, SessionCommand, SessionCursor, SessionDriver, SessionLiveState, SessionSeed,
        SessionSnapshot, SessionSummary, StoredAttachment, SupervisorConfig, ThemeDensity,
        ThemeDto, ThemeId, ThemeMotion, ThemeOption, ThemeSourceClass, ThemeTypography,
    };

    use super::*;

    #[derive(Clone)]
    struct MockHost {
        creates: Arc<AtomicUsize>,
        opens: Arc<AtomicUsize>,
        next_session: Arc<AtomicUsize>,
        seeds: Arc<Mutex<BTreeMap<SessionId, SessionSeed>>>,
        attachments: AttachmentStore,
        _attachment_root: Arc<tempfile::TempDir>,
    }

    impl MockHost {
        fn new() -> Self {
            let attachment_root = Arc::new(tempfile::tempdir().unwrap());
            let attachments = AttachmentStore::open(attachment_root.path()).unwrap();
            Self {
                creates: Arc::new(AtomicUsize::new(0)),
                opens: Arc::new(AtomicUsize::new(0)),
                next_session: Arc::new(AtomicUsize::new(1)),
                seeds: Arc::new(Mutex::new(BTreeMap::new())),
                attachments,
                _attachment_root: attachment_root,
            }
        }

        fn insert_existing(&self, id: &str) -> SessionId {
            let id = SessionId::new(id).unwrap();
            let seed = seed(id.clone(), false, 1);
            self.seeds.lock().unwrap().insert(id.clone(), seed);
            id
        }
    }

    struct MockDriver(SessionSeed);

    #[async_trait]
    impl SessionDriver for MockDriver {
        fn seed(&self) -> SessionSeed {
            self.0.clone()
        }

        async fn dispatch(
            &mut self,
            _command: SessionCommand,
        ) -> Result<DriverCommandOutcome, ServiceError> {
            Ok(DriverCommandOutcome::default())
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

        fn authority_profiles(&self) -> Vec<AuthorityProfile> {
            vec![AuthorityProfile::FullAccess]
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
            let seed = seed(id.clone(), request.provisional, number as u64 + 1);
            self.seeds.lock().unwrap().insert(id, seed.clone());
            Ok(MockDriver(seed))
        }

        async fn open_session(&self, session_id: &SessionId) -> Result<Self::Driver, ServiceError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            self.seeds
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .map(MockDriver)
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

    fn seed(id: SessionId, provisional: bool, generation: u64) -> SessionSeed {
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
                provisional,
                live_state: SessionLiveState::Idle,
                attention: AttentionState::None,
                owner: ActorOwnerState::Hosted,
                model: model.clone(),
            },
            snapshot: SessionSnapshot {
                session_id: id,
                actor_generation: generation.max(1),
                cursor: SessionCursor::zero(generation.max(1)),
                durable_head: None,
                live_state: SessionLiveState::Idle,
                active_run_id: None,
                model,
                authority: AuthorityProfile::FullAccess,
                context: ContextUsage::default(),
                items: Vec::new(),
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

    fn get_request(address: SocketAddr, path: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
    }

    fn authenticated_get_request(address: SocketAddr, path: &str, cookie: &str) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
        )
    }

    fn exchange_request(address: SocketAddr, token: &str) -> String {
        get_request(address, &format!("/__ygg/launch/{token}"))
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
    fn static_asset_allowlist_excludes_dotfiles_and_source_maps() {
        assert!(safe_relative_path(FilePath::new("assets/app.js")));
        let bundle = WebBundle::embedded().unwrap();
        assert_eq!(
            bundle.asset("assets/app.js").unwrap().media_type,
            "text/javascript; charset=utf-8"
        );
        assert!(!safe_relative_path(FilePath::new(".git/config")));
        assert!(!safe_relative_path(FilePath::new("assets/.secret")));
        assert!(bundle.asset("assets/app.js.map").is_none());
        assert!(bundle.asset("private.txt").is_none());
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
    async fn loopback_transport_rejects_cross_origin_and_oversized_requests() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(host, SupervisorConfig::default()));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
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

        let allowed = request(address, get_request(address, "/")).await;
        assert!(allowed.starts_with("HTTP/1.1 200"));
        assert_eq!(
            response_header(&allowed, "content-security-policy"),
            Some(
                "default-src 'self'; base-uri 'none'; connect-src 'self'; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'"
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
        let source_map = request(address, get_request(address, "/assets/app.js.map")).await;
        assert!(source_map.starts_with("HTTP/1.1 404"));
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn attachment_transport_is_authenticated_bounded_and_path_free() {
        let host = Arc::new(MockHost::new());
        let supervisor = Arc::new(SessionSupervisor::new(host, SupervisorConfig::default()));
        let server = LoopbackServer::start(
            supervisor,
            LoopbackConfig {
                port: 0,
                web_root: None,
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
}
