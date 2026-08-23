use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, HOST, ORIGIN,
    SET_COOKIE,
};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{oneshot, watch, Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_io_timeout::TimeoutStream;
use tower::ServiceExt;
use ygg_companion_protocol::MAX_EVENT_BYTES;
use zeroize::Zeroizing;

use crate::client::{method_from_http, route_limits, ClientError};
use crate::core::{CoreError, NativeCore};

const MAX_HEADER_COUNT: usize = 48;
const MAX_HEADER_NAME_BYTES: usize = 64;
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
const MAX_HEADER_AGGREGATE_BYTES: usize = 16 * 1024;
const MAX_LOCAL_PATH_BYTES: usize = 8 * 1024;
const MAX_LOCAL_QUERY_BYTES: usize = 4 * 1024;
const MAX_NATIVE_BODY_BYTES: usize = 8 * 1024;
const MAX_TICKET_BYTES: usize = 4 * 1024;
const MAX_ASSET_BYTES: usize = 8 * 1024 * 1024;
const MAX_ASSET_AGGREGATE_BYTES: usize = 24 * 1024 * 1024;
const MAX_LOCAL_CONNECTIONS: usize = 32;
const MAX_HTTP1_BUFFER_BYTES: usize = 32 * 1024;
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(90);
const LOCAL_REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(15);
const LOCAL_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

const ONBOARDING_HTML: &[u8] = include_bytes!("onboarding.html");
const ONBOARDING_CSS: &[u8] = include_bytes!("onboarding.css");
const ONBOARDING_JS: &[u8] = include_bytes!("onboarding.js");
const SETTINGS_HTML: &str = include_str!("settings.html");
const SETTINGS_CSS: &[u8] = include_bytes!("settings.css");
const SETTINGS_JS: &[u8] = include_bytes!("settings.js");

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProxyError {
    #[error("the bundled web application is invalid")]
    InvalidBundle,
    #[error("the native loopback listener could not start")]
    Bind,
    #[error("secure random generation failed")]
    Random,
}

#[derive(Clone)]
pub(crate) struct AssetBundle {
    assets: Arc<HashMap<String, Asset>>,
    index: Arc<[u8]>,
}

#[derive(Clone)]
struct Asset {
    bytes: Arc<[u8]>,
    content_type: &'static str,
}

impl AssetBundle {
    pub(crate) fn verified<F>(mut load: F) -> Result<Self, ProxyError>
    where
        F: FnMut(&str) -> Option<Vec<u8>>,
    {
        // Each rejection names itself. On iOS this is the only way to tell a
        // missing embedded key from a digest mismatch without a device round
        // trip, because the process has no readable stdout/stderr.
        fn reject(reason: &str) -> ProxyError {
            crate::diagnostic(&format!("bundle rejected: {reason}"));
            ProxyError::InvalidBundle
        }

        let sums = load("SHA256SUMS").ok_or_else(|| {
            reject(
                "SHA256SUMS key absent: the binary has no embedded web bundle. This is what a \
                 dev-mode build produces (`tauri ios dev`, or any build without the \
                 `custom-protocol` feature). Use `npm run ios:build`.",
            )
        })?;
        let expected_bundle =
            load("bundle.sha256").ok_or_else(|| reject("bundle.sha256 key absent"))?;
        if sums.len() > 64 * 1024 || expected_bundle.len() > 128 {
            return Err(reject("checksum file exceeds its size bound"));
        }
        let expected = parse_sums(&sums)?;
        if expected.is_empty() || expected.len() > 256 {
            return Err(reject("manifest entry count out of range"));
        }
        let mut assets = HashMap::with_capacity(expected.len());
        let mut aggregate = 0usize;
        for (path, digest) in expected {
            let bytes = load(&path).ok_or_else(|| reject(&format!("asset key absent: {path}")))?;
            if bytes.len() > MAX_ASSET_BYTES {
                return Err(reject(&format!(
                    "asset exceeds per-file bound: {path} ({} bytes)",
                    bytes.len()
                )));
            }
            aggregate = aggregate
                .checked_add(bytes.len())
                .ok_or_else(|| reject("aggregate size overflow"))?;
            if aggregate > MAX_ASSET_AGGREGATE_BYTES {
                return Err(reject(&format!(
                    "aggregate exceeds bound at {path} ({aggregate} bytes)"
                )));
            }
            let actual = hex_sha256(&bytes);
            if actual != digest {
                return Err(reject(&format!(
                    "digest mismatch: {path} ({} bytes) expected {digest} got {actual}",
                    bytes.len()
                )));
            }
            let content_type = asset_content_type(&path)
                .ok_or_else(|| reject(&format!("unsupported content type: {path}")))?;
            assets.insert(
                format!("/{path}"),
                Asset {
                    bytes: bytes.into(),
                    content_type,
                },
            );
        }
        let expected_bundle = std::str::from_utf8(&expected_bundle)
            .map_err(|_| reject("bundle.sha256 is not UTF-8"))?
            .trim();
        if !valid_digest(expected_bundle) {
            return Err(reject("bundle.sha256 is not a valid digest"));
        }
        let actual_bundle = hex_sha256(&sums);
        if actual_bundle != expected_bundle {
            return Err(reject(&format!(
                "manifest digest mismatch: expected {expected_bundle} got {actual_bundle}"
            )));
        }
        let index = assets
            .get("/index.html")
            .ok_or_else(|| reject("manifest has no index.html entry"))?
            .bytes
            .clone();
        Ok(Self {
            assets: Arc::new(assets),
            index,
        })
    }

    fn asset(&self, path: &str) -> Option<Asset> {
        self.assets.get(path).cloned()
    }

    fn index_for_device(&self, device_id: &str, settings_url: &str) -> Result<Vec<u8>, ProxyError> {
        if device_id.is_empty()
            || device_id.len() > 128
            || !device_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || !valid_native_settings_url(settings_url)
        {
            return Err(ProxyError::InvalidBundle);
        }
        let index = std::str::from_utf8(&self.index).map_err(|_| ProxyError::InvalidBundle)?;
        let marker = "</head>";
        let position = index.find(marker).ok_or(ProxyError::InvalidBundle)?;
        let metadata = format!(
            "<meta name=\"ygg-device-id\" content=\"{device_id}\">\n<meta name=\"ygg-native-settings-url\" content=\"{settings_url}\">\n"
        );
        let mut rendered = Vec::with_capacity(index.len() + metadata.len());
        rendered.extend_from_slice(&index.as_bytes()[..position]);
        rendered.extend_from_slice(metadata.as_bytes());
        rendered.extend_from_slice(&index.as_bytes()[position..]);
        Ok(rendered)
    }
}

#[derive(Clone)]
struct ProxyState {
    core: Arc<NativeCore>,
    assets: AssetBundle,
    auth: Arc<LocalAuth>,
    settings_url: Arc<str>,
    upgrades: Arc<UpgradeTracker>,
}

#[derive(Clone)]
struct SettingsState {
    core: Arc<NativeCore>,
    auth: Arc<LocalAuth>,
    app_origin: Arc<str>,
}

struct LocalAuth {
    host: String,
    origin: String,
    cookie_name: String,
    cookie: String,
    launch_token: Mutex<Option<String>>,
}

struct UpgradeTracker {
    state: Mutex<UpgradeTrackerState>,
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
            state: Mutex::new(UpgradeTrackerState::default()),
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

pub(crate) struct ProxyHandle {
    address: SocketAddr,
    settings_address: SocketAddr,
    launch_url: String,
    shutdown: Option<oneshot::Sender<()>>,
    settings_shutdown: Option<oneshot::Sender<()>>,
    upgrades: Arc<UpgradeTracker>,
    task: tokio::task::JoinHandle<()>,
    settings_task: tokio::task::JoinHandle<()>,
}

impl ProxyHandle {
    pub(crate) async fn start(
        core: Arc<NativeCore>,
        assets: AssetBundle,
    ) -> Result<Self, ProxyError> {
        let listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .map_err(|_| ProxyError::Bind)?;
        let settings_listener =
            tokio::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
                .await
                .map_err(|_| ProxyError::Bind)?;
        let address = listener.local_addr().map_err(|_| ProxyError::Bind)?;
        let settings_address = settings_listener
            .local_addr()
            .map_err(|_| ProxyError::Bind)?;
        let host = format!("127.0.0.1:{}", address.port());
        let origin = format!("http://{host}");
        let settings_host = format!("127.0.0.1:{}", settings_address.port());
        let settings_origin = format!("http://{settings_host}");
        let launch_token = random_token(32)?;
        let settings_launch_token = random_token(32)?;
        let launch_url = format!("{origin}/_native/start/{launch_token}");
        let settings_url =
            format!("{settings_origin}/_native/settings/start/{settings_launch_token}");
        let auth = Arc::new(LocalAuth {
            host,
            origin: origin.clone(),
            cookie_name: format!("ygg-native-{}", random_token(8)?),
            cookie: random_token(32)?,
            launch_token: Mutex::new(Some(launch_token)),
        });
        let settings_auth = Arc::new(LocalAuth {
            host: settings_host,
            origin: settings_origin,
            cookie_name: format!("ygg-settings-{}", random_token(8)?),
            cookie: random_token(32)?,
            launch_token: Mutex::new(Some(settings_launch_token)),
        });
        let upgrades = UpgradeTracker::new();
        let state = ProxyState {
            core: core.clone(),
            assets,
            auth,
            settings_url: settings_url.clone().into(),
            upgrades: upgrades.clone(),
        };
        let settings_state = SettingsState {
            core,
            auth: settings_auth,
            app_origin: origin.into(),
        };
        let router = Router::new()
            .route("/_native/start/{token}", get(launch))
            .route("/_native/state", get(native_state))
            .route("/_native/pair/poll", post(poll_pairing))
            .route("/_native/pair", post(begin_pairing).delete(cancel_pairing))
            .route("/_native/access", delete(remove_access))
            .route("/api/v1/events", get(events))
            .fallback(dispatch)
            .with_state(state);
        let settings_router = Router::new()
            .route("/_native/settings/start/{token}", get(settings_launch))
            .route("/_native/state", get(settings_native_state))
            .route("/_native/access", delete(settings_remove_access))
            .route("/", get(settings_index))
            .route("/_native/settings.css", get(settings_css))
            .route("/_native/settings.js", get(settings_js))
            .fallback(|| async { plain(StatusCode::NOT_FOUND, "Not found") })
            .with_state(settings_state);
        let admission = Arc::new(Semaphore::new(MAX_LOCAL_CONNECTIONS));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (settings_shutdown_tx, settings_shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(serve_loop(listener, router, shutdown_rx, admission.clone()));
        let settings_task = tokio::spawn(serve_loop(
            settings_listener,
            settings_router,
            settings_shutdown_rx,
            admission,
        ));
        Ok(Self {
            address,
            settings_address,
            launch_url,
            shutdown: Some(shutdown_tx),
            settings_shutdown: Some(settings_shutdown_tx),
            upgrades,
            task,
            settings_task,
        })
    }

    pub(crate) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(crate) fn settings_address(&self) -> SocketAddr {
        self.settings_address
    }

    pub(crate) fn launch_url(&self) -> &str {
        &self.launch_url
    }

    pub(crate) async fn shutdown(mut self) {
        self.upgrades.close();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(shutdown) = self.settings_shutdown.take() {
            let _ = shutdown.send(());
        }
        let graceful = tokio::time::timeout(Duration::from_secs(5), async {
            let _ = tokio::join!(&mut self.task, &mut self.settings_task);
        })
        .await;
        if graceful.is_err() {
            if !self.task.is_finished() {
                self.task.abort();
            }
            if !self.settings_task.is_finished() {
                self.settings_task.abort();
            }
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                let _ = tokio::join!(&mut self.task, &mut self.settings_task);
            })
            .await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.upgrades.wait_idle()).await;
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.upgrades.close();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(shutdown) = self.settings_shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
        self.settings_task.abort();
    }
}

async fn serve_loop(
    listener: tokio::net::TcpListener,
    router: Router,
    mut shutdown: oneshot::Receiver<()>,
    admission: Arc<Semaphore>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            completed = connections.join_next(), if !connections.is_empty() => {
                let _ = completed;
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else {
                    break;
                };
                let Ok(permit) = admission.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let _ = stream.set_nodelay(true);
                let router = router.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let mut stream = TimeoutStream::new(stream);
                    stream.set_read_timeout(Some(SOCKET_READ_TIMEOUT));
                    stream.set_write_timeout(Some(LOCAL_WRITE_TIMEOUT));
                    let service = service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                        let router = router.clone();
                        async move {
                            let response = if request_target_is_oversized(request.uri()) {
                                plain(StatusCode::URI_TOO_LONG, "Request target too long")
                            } else {
                                router
                                    .oneshot(request.map(Body::new))
                                    .await
                                    .unwrap_or_else(|never| match never {})
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let connection = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(HEADER_READ_TIMEOUT)
                        .max_headers(MAX_HEADER_COUNT)
                        .max_buf_size(MAX_HTTP1_BUFFER_BYTES)
                        .serve_connection(TokioIo::new(Box::pin(stream)), service)
                        .with_upgrades();
                    let _ = connection.await;
                });
            }
        }
    }
    connections.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
}

async fn launch(
    State(state): State<ProxyState>,
    Path(token): Path<String>,
    request: Request,
) -> Response<Body> {
    let headers = request.headers();
    if request.uri().query().is_some()
        || validate_headers(headers).is_err()
        || host_matches(headers, &state.auth.host).is_err()
    {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    if read_request_body(request, 0).await.is_err() {
        return plain(StatusCode::BAD_REQUEST, "Request body not allowed");
    }
    let accepted = consume_launch_token(&state.auth, &token);
    if !accepted {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let mut response = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header("location", "/")
        .header(
            SET_COOKIE,
            format!(
                "{}={}; HttpOnly; SameSite=Strict; Path=/",
                state.auth.cookie_name, state.auth.cookie
            ),
        )
        .body(Body::empty())
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "Native proxy error"));
    secure_headers(response.headers_mut());
    response
}

async fn settings_launch(
    State(state): State<SettingsState>,
    Path(token): Path<String>,
    request: Request,
) -> Response<Body> {
    let headers = request.headers();
    if request.uri().query().is_some()
        || validate_headers(headers).is_err()
        || host_matches(headers, &state.auth.host).is_err()
        || token.len() != 64
        || !token.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let authenticated = auth_cookie_matches(headers, &state.auth);
    if read_request_body(request, 0).await.is_err() {
        return plain(StatusCode::BAD_REQUEST, "Request body not allowed");
    }
    let accepted = authenticated || consume_launch_token(&state.auth, &token);
    if !accepted {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    let mut builder = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header("location", "/");
    if !authenticated {
        builder = builder.header(
            SET_COOKIE,
            format!(
                "{}={}; HttpOnly; SameSite=Strict; Path=/",
                state.auth.cookie_name, state.auth.cookie
            ),
        );
    }
    let mut response = builder
        .body(Body::empty())
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "Native proxy error"));
    secure_headers(response.headers_mut());
    response
}

async fn settings_index(State(state): State<SettingsState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize_auth(&state.auth, &request, false) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return plain(StatusCode::BAD_REQUEST, "Invalid settings request");
    }
    let html = SETTINGS_HTML.replace("{{APP_ORIGIN}}", &state.app_origin);
    if html.len() > MAX_NATIVE_BODY_BYTES {
        return plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Native settings unavailable",
        );
    }
    native_asset(html.as_bytes(), "text/html; charset=utf-8", false)
}

async fn settings_css(State(state): State<SettingsState>, request: Request) -> Response<Body> {
    settings_asset(state, request, SETTINGS_CSS, "text/css; charset=utf-8").await
}

async fn settings_js(State(state): State<SettingsState>, request: Request) -> Response<Body> {
    settings_asset(
        state,
        request,
        SETTINGS_JS,
        "text/javascript; charset=utf-8",
    )
    .await
}

async fn settings_asset(
    state: SettingsState,
    request: Request,
    bytes: &'static [u8],
    content_type: &'static str,
) -> Response<Body> {
    if let Err(response) = authorize_auth(&state.auth, &request, false) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return plain(StatusCode::BAD_REQUEST, "Invalid settings request");
    }
    native_asset(bytes, content_type, false)
}

async fn settings_native_state(
    State(state): State<SettingsState>,
    request: Request,
) -> Response<Body> {
    if let Err(response) = authorize_auth(&state.auth, &request, false) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(StatusCode::BAD_REQUEST, "This request has no body.");
    }
    json_response(StatusCode::OK, &state.core.public_state().await)
}

async fn settings_remove_access(
    State(state): State<SettingsState>,
    request: Request,
) -> Response<Body> {
    if let Err(response) = authorize_auth(&state.auth, &request, true) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(StatusCode::BAD_REQUEST, "This request has no body.");
    }
    match state.core.remove_access_from_settings().await {
        Ok(public) => json_response(StatusCode::OK, &public),
        Err(error) => core_error(error),
    }
}

async fn native_state(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, false) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(
            StatusCode::BAD_REQUEST,
            "This request has no query or body.",
        );
    }
    json_response(StatusCode::OK, &state.core.public_state().await)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BeginPairingBody {
    ticket: String,
    device_name: String,
}

async fn begin_pairing(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, true) {
        return response;
    }
    let has_query = request.uri().query().is_some();
    let bytes = match read_request_body(request, MAX_NATIVE_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return native_error(StatusCode::PAYLOAD_TOO_LARGE, "The request is too large."),
    };
    if has_query {
        return native_error(StatusCode::BAD_REQUEST, "This request has no query.");
    }
    let body: BeginPairingBody = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => return native_error(StatusCode::BAD_REQUEST, "The pairing request is invalid."),
    };
    if body.ticket.len() > MAX_TICKET_BYTES {
        return native_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "The invitation is too large.",
        );
    }
    match state
        .core
        .begin_pairing(&body.ticket, &body.device_name)
        .await
    {
        Ok(public) => json_response(StatusCode::OK, &public),
        Err(error) => core_error(error),
    }
}

async fn poll_pairing(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, true) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(
            StatusCode::BAD_REQUEST,
            "This request has no query or body.",
        );
    }
    match state.core.poll_pairing().await {
        Ok(public) => json_response(StatusCode::OK, &public),
        Err(error) => core_error(error),
    }
}

async fn cancel_pairing(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, true) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(
            StatusCode::BAD_REQUEST,
            "This request has no query or body.",
        );
    }
    match state.core.cancel_pairing().await {
        Ok(public) => json_response(StatusCode::OK, &public),
        Err(error) => core_error(error),
    }
}

async fn remove_access(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, true) {
        return response;
    }
    if request.uri().query().is_some() || read_request_body(request, 0).await.is_err() {
        return native_error(
            StatusCode::BAD_REQUEST,
            "This request has no query or body.",
        );
    }
    match state.core.remove_unpaired_access().await {
        Ok(public) => json_response(StatusCode::OK, &public),
        Err(error) => core_error(error),
    }
}

async fn dispatch(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, request.method() != Method::GET) {
        return response;
    }
    let path = request.uri().path();
    if path.starts_with("/api/") {
        proxy_http(state, request).await
    } else if request.method() == Method::GET {
        let uri = request.uri().clone();
        if read_request_body(request, 0).await.is_err() {
            return plain(StatusCode::BAD_REQUEST, "Request body not allowed");
        }
        serve_asset(state, &uri).await
    } else {
        let _ = read_request_body(request, 0).await;
        plain(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed")
    }
}

async fn proxy_http(state: ProxyState, request: Request) -> Response<Body> {
    let Some(profile) = state.core.paired_profile().await else {
        return native_error(
            StatusCode::UNAUTHORIZED,
            "Pair this device before connecting.",
        );
    };
    let method = match method_from_http(request.method()) {
        Ok(method) => method,
        Err(_) => return plain(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed"),
    };
    let path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or(request.uri().path())
        .to_owned();
    let limits = match route_limits(method, &path) {
        Ok(limits) => limits,
        Err(_) => return plain(StatusCode::NOT_FOUND, "Not found"),
    };
    let content_type = match request.headers().get(CONTENT_TYPE) {
        Some(value) => match value.to_str() {
            Ok(value) if value.len() <= 255 => Some(value.to_owned()),
            _ => return plain(StatusCode::BAD_REQUEST, "Invalid content type"),
        },
        None => None,
    };
    let declared_length = match declared_length(request.headers()) {
        Ok(length) => length,
        Err(()) => return plain(StatusCode::BAD_REQUEST, "Invalid content length"),
    };
    if limits.request_bytes == 0 && request.headers().contains_key("transfer-encoding") {
        return plain(StatusCode::BAD_REQUEST, "Request body not allowed");
    }
    if declared_length.is_some_and(|length| length > limits.request_bytes) {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
    }
    let body = match read_local_body(request.into_body(), limits.request_bytes).await {
        Ok(body) => body.to_vec(),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large"),
    };
    if declared_length.is_some_and(|length| length != body.len()) {
        return plain(StatusCode::BAD_REQUEST, "Content length did not match body");
    }
    let response = match state
        .core
        .remote()
        .http(&profile.target, method, path, content_type, body)
        .await
    {
        Ok(response) => response,
        Err(error) => return handle_client_error(&state.core, error).await,
    };
    let status = match StatusCode::from_u16(response.status) {
        Ok(status) => status,
        Err(_) => return plain(StatusCode::BAD_GATEWAY, "Invalid host response"),
    };
    let mut builder = Response::builder().status(status);
    for header in response.headers {
        let Ok(name) = HeaderName::try_from(header.name) else {
            return plain(StatusCode::BAD_GATEWAY, "Invalid host response");
        };
        let Ok(value) = HeaderValue::try_from(header.value) else {
            return plain(StatusCode::BAD_GATEWAY, "Invalid host response");
        };
        builder = builder.header(name, value);
    }
    let core = state.core.clone();
    let stream =
        futures_util::stream::try_unfold((response.body, core), |(mut body, core)| async move {
            match body.next_chunk().await {
                Ok(Some(chunk)) => Ok(Some((Bytes::from(chunk), (body, core)))),
                Ok(None) => Ok(None),
                Err(error) => {
                    if error == ClientError::Revoked {
                        core.mark_revoked().await;
                    }
                    Err(std::io::Error::other("companion response interrupted"))
                }
            }
        });
    let mut result = builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| plain(StatusCode::BAD_GATEWAY, "Invalid host response"));
    secure_headers(result.headers_mut());
    result
}

async fn serve_asset(state: ProxyState, uri: &Uri) -> Response<Body> {
    if uri.query().is_some() && uri.path() != "/" {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    if uri.path() == "/_native/onboarding.css" {
        return native_asset(ONBOARDING_CSS, "text/css; charset=utf-8", false);
    }
    if uri.path() == "/_native/onboarding.js" {
        return native_asset(ONBOARDING_JS, "text/javascript; charset=utf-8", false);
    }
    let Some(profile) = state.core.paired_profile().await else {
        return if valid_spa_path(uri.path()) {
            native_asset(ONBOARDING_HTML, "text/html; charset=utf-8", false)
        } else {
            plain(StatusCode::NOT_FOUND, "Not found")
        };
    };
    if uri.path().starts_with("/assets/") {
        return match state.assets.asset(uri.path()) {
            Some(asset) => native_asset(&asset.bytes, asset.content_type, true),
            None => plain(StatusCode::NOT_FOUND, "Not found"),
        };
    }
    if !valid_spa_path(uri.path()) {
        return plain(StatusCode::NOT_FOUND, "Not found");
    }
    match state
        .assets
        .index_for_device(&profile.device_id, &state.settings_url)
    {
        Ok(index) => native_asset(&index, "text/html; charset=utf-8", false),
        Err(_) => plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Native bundle unavailable",
        ),
    }
}

async fn events(State(state): State<ProxyState>, request: Request) -> Response<Body> {
    if let Err(response) = authorize(&state, &request, true) {
        return response;
    }
    let declared_length = match declared_length(request.headers()) {
        Ok(length) => length,
        Err(()) => return plain(StatusCode::BAD_REQUEST, "Invalid WebSocket request"),
    };
    if request.uri().query().is_some()
        || declared_length.is_some_and(|length| length != 0)
        || request.headers().contains_key("transfer-encoding")
    {
        return plain(StatusCode::BAD_REQUEST, "Invalid WebSocket request");
    }
    let Some(profile) = state.core.paired_profile().await else {
        return native_error(
            StatusCode::UNAUTHORIZED,
            "Pair this device before connecting.",
        );
    };
    let (mut parts, body) = request.into_parts();
    drop(body);
    let websocket = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade
            .max_message_size(MAX_EVENT_BYTES)
            .max_frame_size(MAX_EVENT_BYTES),
        Err(_) => return plain(StatusCode::BAD_REQUEST, "Invalid WebSocket upgrade"),
    };
    let Some(upgrade_task) = state.upgrades.register() else {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "Native proxy is shutting down",
        );
    };
    websocket
        .on_upgrade(move |socket| {
            upgrade_task.run(bridge_events(socket, state.core, profile.target))
        })
        .into_response()
}

async fn bridge_events(
    socket: WebSocket,
    core: Arc<NativeCore>,
    target: crate::profile::HostTarget,
) {
    let mut remote = match core.remote().events(&target).await {
        Ok(remote) => remote,
        Err(error) => {
            if error == ClientError::Revoked {
                core.mark_revoked().await;
            }
            return;
        }
    };
    let (mut browser_send, mut browser_recv) = socket.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_browser_frame = tokio::time::Instant::now();
    loop {
        tokio::select! {
            remote_event = remote.next() => {
                let bytes = match remote_event {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        let (code, reason) = match error {
                            ClientError::Revoked => {
                                core.mark_revoked().await;
                                (close_code::POLICY, "device revoked")
                            }
                            ClientError::ReplayRequired => (close_code::RESTART, "replay required"),
                            _ => (close_code::ERROR, "host event stream unavailable"),
                        };
                        let _ = browser_send.send(Message::Close(Some(CloseFrame {
                            code,
                            reason: reason.into(),
                        }))).await;
                        break;
                    }
                };
                let text = match String::from_utf8(bytes) {
                    Ok(text) if serde_json::from_str::<serde_json::Value>(&text).is_ok() => text,
                    _ => {
                        let _ = browser_send.send(Message::Close(Some(CloseFrame {
                            code: close_code::ERROR,
                            reason: "invalid host event".into(),
                        }))).await;
                        break;
                    }
                };
                let sent = tokio::time::timeout(
                    LOCAL_WRITE_TIMEOUT,
                    browser_send.send(Message::Text(text.into())),
                )
                .await;
                if !matches!(sent, Ok(Ok(()))) {
                    break;
                }
            }
            _ = heartbeat.tick() => {
                if last_browser_frame.elapsed() > Duration::from_secs(75) {
                    let _ = browser_send.send(Message::Close(Some(CloseFrame {
                        code: close_code::AWAY,
                        reason: "browser heartbeat timed out".into(),
                    }))).await;
                    break;
                }
                let sent = tokio::time::timeout(
                    LOCAL_WRITE_TIMEOUT,
                    browser_send.send(Message::Ping(Bytes::from_static(b"ygg"))),
                ).await;
                if !matches!(sent, Ok(Ok(()))) {
                    break;
                }
            }
            inbound = browser_recv.next() => {
                last_browser_frame = tokio::time::Instant::now();
                match inbound {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Text(_))) | Some(Ok(Message::Binary(_))) => {
                        let _ = browser_send.send(Message::Close(Some(CloseFrame {
                            code: close_code::POLICY,
                            reason: "client messages are not accepted".into(),
                        }))).await;
                        break;
                    }
                }
            }
        }
    }
}

fn request_target_is_oversized(uri: &Uri) -> bool {
    uri.path().len() > MAX_LOCAL_PATH_BYTES
        || uri
            .query()
            .is_some_and(|query| query.len() > MAX_LOCAL_QUERY_BYTES)
}

fn authorize(
    state: &ProxyState,
    request: &Request,
    require_origin: bool,
) -> Result<(), Response<Body>> {
    authorize_auth(&state.auth, request, require_origin)
}

fn authorize_auth(
    auth: &LocalAuth,
    request: &Request,
    require_origin: bool,
) -> Result<(), Response<Body>> {
    let headers = request.headers();
    validate_headers(headers).map_err(|_| plain(StatusCode::BAD_REQUEST, "Invalid headers"))?;
    host_matches(headers, &auth.host).map_err(|_| plain(StatusCode::NOT_FOUND, "Not found"))?;
    if request_target_is_oversized(request.uri()) {
        return Err(plain(StatusCode::URI_TOO_LONG, "Request target too long"));
    }
    if !auth_cookie_matches(headers, auth) {
        return Err(plain(
            StatusCode::UNAUTHORIZED,
            "Native authentication required",
        ));
    }
    if require_origin {
        let origin = headers
            .get(ORIGIN)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| plain(StatusCode::FORBIDDEN, "Origin required"))?;
        if origin.as_bytes().ct_eq(auth.origin.as_bytes()).unwrap_u8() != 1 {
            return Err(plain(StatusCode::FORBIDDEN, "Origin rejected"));
        }
    }
    if !fetch_site_allowed(headers) {
        return Err(plain(StatusCode::FORBIDDEN, "Request context rejected"));
    }
    Ok(())
}

/// Whether the request's Fetch Metadata context is acceptable.
///
/// WebKit attaches `Sec-Fetch-*` metadata to every request it makes.
/// Requests associated with a document carry that document's origin
/// relationship (`same-origin`, `cross-site`, ...), but a top-level
/// document navigation has no associated origin, so WebKit stamps it
/// `Sec-Fetch-Site: none`. That includes the redirect the webview
/// follows after the one-use launch-token URL: without accepting it,
/// the app can never open its own UI on a real device. `none` is
/// therefore accepted, but only when the request identifies itself as
/// a document navigation (`Sec-Fetch-Mode: navigate`, or a WebKit that
/// ships the site value without the mode value). Drive-by requests
/// from a foreign page still arrive as `cross-site` (or `same-site`)
/// and are rejected.
fn fetch_site_allowed(headers: &HeaderMap) -> bool {
    let Some(site) = headers.get("sec-fetch-site") else {
        // Pre-metadata WebKit sends nothing: there is nothing to act
        // on, and the host, cookie, and origin gates above still
        // apply.
        return true;
    };
    if site.as_bytes() == b"same-origin" {
        return true;
    }
    site.as_bytes() == b"none"
        && headers
            .get("sec-fetch-mode")
            .map(|mode| mode.as_bytes() == b"navigate")
            .unwrap_or(true)
}

fn auth_cookie_matches(headers: &HeaderMap, auth: &LocalAuth) -> bool {
    headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| cookie_value(value, &auth.cookie_name))
        .is_some_and(|cookie| cookie.as_bytes().ct_eq(auth.cookie.as_bytes()).unwrap_u8() == 1)
}

fn consume_launch_token(auth: &LocalAuth, token: &str) -> bool {
    auth.launch_token.lock().ok().is_some_and(|mut expected| {
        let matches = expected.as_ref().is_some_and(|expected| {
            token.len() <= 128 && token.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1
        });
        if matches {
            expected.take();
        }
        matches
    })
}

fn validate_headers(headers: &HeaderMap) -> Result<(), ()> {
    if headers.len() > MAX_HEADER_COUNT {
        return Err(());
    }
    let mut aggregate = 0usize;
    for (name, value) in headers {
        if name.as_str().len() > MAX_HEADER_NAME_BYTES || value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(());
        }
        aggregate = aggregate
            .checked_add(name.as_str().len() + value.len())
            .ok_or(())?;
        if aggregate > MAX_HEADER_AGGREGATE_BYTES {
            return Err(());
        }
    }
    Ok(())
}

fn host_matches(headers: &HeaderMap, expected: &str) -> Result<(), ()> {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or(())?;
    if host == expected {
        Ok(())
    } else {
        Err(())
    }
}

fn cookie_value<'a>(cookies: &'a str, name: &str) -> Option<&'a str> {
    cookies.split(';').find_map(|part| {
        let (candidate, value) = part.trim().split_once('=')?;
        (candidate == name && !value.is_empty() && value.len() <= 128).then_some(value)
    })
}

async fn read_request_body(request: Request, limit: usize) -> Result<Bytes, ()> {
    let expected = declared_length(request.headers())?;
    if expected.is_some_and(|length| length > limit)
        || (limit == 0 && request.headers().contains_key("transfer-encoding"))
    {
        return Err(());
    }
    let body = read_local_body(request.into_body(), limit).await?;
    if expected.is_some_and(|length| length != body.len()) {
        return Err(());
    }
    Ok(body)
}

async fn read_local_body(body: Body, limit: usize) -> Result<Bytes, ()> {
    read_local_body_with_timeout(body, limit, LOCAL_REQUEST_BODY_TIMEOUT).await
}

async fn read_local_body_with_timeout(
    body: Body,
    limit: usize,
    timeout: Duration,
) -> Result<Bytes, ()> {
    tokio::time::timeout(timeout, to_bytes(body, limit))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

fn declared_length(headers: &HeaderMap) -> Result<Option<usize>, ()> {
    let mut values = headers.get_all(CONTENT_LENGTH).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some()
        || value.as_bytes().is_empty()
        || value.as_bytes().iter().any(|byte| !byte.is_ascii_digit())
    {
        return Err(());
    }
    let value = std::str::from_utf8(value.as_bytes()).map_err(|_| ())?;
    let value = value.parse::<u64>().map_err(|_| ())?;
    Ok(Some(usize::try_from(value).map_err(|_| ())?))
}

fn valid_native_settings_url(value: &str) -> bool {
    let Some(authority_and_path) = value.strip_prefix("http://127.0.0.1:") else {
        return false;
    };
    let Some((port, path)) = authority_and_path.split_once('/') else {
        return false;
    };
    let Some(token) = path.strip_prefix("_native/settings/start/") else {
        return false;
    };
    port.parse::<u16>().is_ok_and(|port| port != 0)
        && token.len() == 64
        && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_spa_path(path: &str) -> bool {
    path == "/"
        || (path.len() <= MAX_LOCAL_PATH_BYTES
            && path.starts_with('/')
            && !path.starts_with("//")
            && !path.contains("..")
            && !path.contains('%')
            && !path.contains('\\')
            && path
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.".contains(&byte)))
}

fn native_asset(bytes: &[u8], content_type: &'static str, immutable: bool) -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(
            CACHE_CONTROL,
            if immutable {
                "public, max-age=31536000, immutable"
            } else {
                "no-store"
            },
        )
        .body(Body::from(bytes.to_vec()))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "Native proxy error"));
    secure_headers(response.headers_mut());
    response
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> Response<Body> {
    match serde_json::to_vec(value) {
        Ok(body) if body.len() <= MAX_NATIVE_BODY_BYTES => {
            let mut response = Response::builder()
                .status(status)
                .header(CONTENT_TYPE, "application/json")
                .header(CACHE_CONTROL, "no-store")
                .body(Body::from(body))
                .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "Native proxy error"));
            secure_headers(response.headers_mut());
            response
        }
        _ => plain(StatusCode::INTERNAL_SERVER_ERROR, "Native proxy error"),
    }
}

#[derive(serde::Serialize)]
struct ErrorBody<'a> {
    message: &'a str,
}

fn native_error(status: StatusCode, message: &'static str) -> Response<Body> {
    json_response(status, &ErrorBody { message })
}

fn core_error(error: CoreError) -> Response<Body> {
    match error {
        CoreError::InvalidTicket => native_error(
            StatusCode::BAD_REQUEST,
            "The pairing invitation is invalid or expired.",
        ),
        CoreError::Conflict => native_error(
            StatusCode::CONFLICT,
            "Finish or cancel the current companion operation first.",
        ),
        CoreError::PairedRemovalRequiresSettings => native_error(
            StatusCode::CONFLICT,
            "Paired access can only be removed from native app settings.",
        ),
        CoreError::InvalidPairing => native_error(
            StatusCode::BAD_GATEWAY,
            "The host returned inconsistent pairing state.",
        ),
        CoreError::Client(client) => client_error_response(client),
        CoreError::Credential(_) | CoreError::Profile(_) => native_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Protected local state is unavailable.",
        ),
    }
}

async fn handle_client_error(core: &Arc<NativeCore>, error: ClientError) -> Response<Body> {
    if error == ClientError::Revoked {
        core.mark_revoked().await;
    }
    client_error_response(error)
}

fn client_error_response(error: ClientError) -> Response<Body> {
    match error {
        ClientError::Timeout => native_error(
            StatusCode::GATEWAY_TIMEOUT,
            "The Ygg host did not respond in time.",
        ),
        ClientError::Unauthorized | ClientError::Revoked => native_error(
            StatusCode::FORBIDDEN,
            "This device is not authorized by the Ygg host.",
        ),
        ClientError::IdentityMismatch => native_error(
            StatusCode::CONFLICT,
            "The host identity did not match the invitation.",
        ),
        ClientError::ProtocolMismatch => native_error(
            StatusCode::BAD_GATEWAY,
            "The host uses an incompatible companion protocol.",
        ),
        ClientError::InvalidTarget | ClientError::InvalidResponse | ClientError::ReplayRequired => {
            native_error(
                StatusCode::BAD_GATEWAY,
                "The host returned an invalid companion response.",
            )
        }
        ClientError::Unavailable => native_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The Ygg host is unavailable.",
        ),
        ClientError::Rejected => native_error(
            StatusCode::BAD_GATEWAY,
            "The Ygg host rejected the companion request.",
        ),
    }
}

fn plain(status: StatusCode, message: &'static str) -> Response<Body> {
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(CACHE_CONTROL, "no-store")
        .body(Body::from(message))
        .unwrap_or_else(|_| Response::new(Body::empty()));
    secure_headers(response.headers_mut());
    response
}

fn secure_headers(headers: &mut HeaderMap) {
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; font-src 'self' data:; form-action 'none'; frame-ancestors 'none'; img-src 'self' data: blob:; object-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'",
        ),
    );
    if !headers.contains_key(CACHE_CONTROL) {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    headers.remove("access-control-allow-origin");
}

fn parse_sums(bytes: &[u8]) -> Result<BTreeMap<String, String>, ProxyError> {
    let text = std::str::from_utf8(bytes).map_err(|_| ProxyError::InvalidBundle)?;
    let mut entries = BTreeMap::new();
    for line in text.lines() {
        let (digest, path) = line.split_once("  ").ok_or(ProxyError::InvalidBundle)?;
        if !valid_digest(digest)
            || path.is_empty()
            || path.len() > 512
            || path.starts_with('/')
            || path.contains("..")
            || path.contains('\\')
            || path.chars().any(char::is_control)
            || entries.insert(path.to_owned(), digest.to_owned()).is_some()
        {
            return Err(ProxyError::InvalidBundle);
        }
    }
    Ok(entries)
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn asset_content_type(path: &str) -> Option<&'static str> {
    if path.ends_with(".html") {
        Some("text/html; charset=utf-8")
    } else if path.ends_with(".js") {
        Some("text/javascript; charset=utf-8")
    } else if path.ends_with(".css") {
        Some("text/css; charset=utf-8")
    } else if path.ends_with(".json") {
        Some("application/json")
    } else if path.ends_with(".svg") {
        Some("image/svg+xml")
    } else if path.ends_with(".png") {
        Some("image/png")
    } else if path.ends_with(".woff2") {
        Some("font/woff2")
    } else {
        None
    }
}

fn random_token(bytes: usize) -> Result<String, ProxyError> {
    let mut random = Zeroizing::new(vec![0u8; bytes]);
    getrandom::fill(random.as_mut_slice()).map_err(|_| ProxyError::Random)?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in random.iter() {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").map_err(|_| ProxyError::Random)?;
    }
    Ok(encoded)
}

use axum::extract::FromRequestParts;
use subtle::ConstantTimeEq;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    use iroh::{Endpoint, RelayMode};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::tungstenite::Message as ClientWebSocketMessage;
    use ygg_companion_protocol::{
        read_head, write_head, write_record, DevicePlatform, DeviceSummary, RequestHead,
        ResponseHead, RESET_CANCELLED,
    };

    use crate::client::RemoteClient;
    use crate::credentials::{tests::MemoryCredentials, EndpointKey, SharedCredentials};
    use crate::profile::{HostProfile, HostTarget, ProfileStore};

    #[test]
    fn bundle_verification_rejects_tampering_and_injects_device_metadata() {
        let files = test_bundle_files();
        let bundle = AssetBundle::verified(|path| files.get(path).cloned()).unwrap();
        let index = bundle
            .index_for_device(
                "device-1",
                "http://127.0.0.1:1234/_native/settings/start/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap();
        let index = std::str::from_utf8(&index).unwrap();
        assert!(index.contains("<meta name=\"ygg-device-id\" content=\"device-1\">"));
        assert!(index.contains("<meta name=\"ygg-native-settings-url\""));
        assert!(bundle
            .index_for_device(
                "bad\"device",
                "http://127.0.0.1:1234/_native/settings/start/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .is_err());

        let mut tampered = files;
        tampered
            .get_mut("index.html")
            .unwrap()
            .extend_from_slice(b"tampered");
        assert!(matches!(
            AssetBundle::verified(|path| tampered.get(path).cloned()),
            Err(ProxyError::InvalidBundle)
        ));
    }

    #[tokio::test]
    async fn body_reader_enforces_size_and_deadline() {
        assert!(
            read_local_body_with_timeout(Body::from(vec![0u8; 5]), 4, Duration::from_secs(1))
                .await
                .is_err()
        );

        let pending = futures_util::stream::pending::<Result<Bytes, io::Error>>();
        assert!(read_local_body_with_timeout(
            Body::from_stream(pending),
            4,
            Duration::from_millis(20)
        )
        .await
        .is_err());
    }

    #[test]
    fn request_target_limits_path_and_query_independently() {
        let path = format!("/{}", "a".repeat(MAX_LOCAL_PATH_BYTES - 1));
        let query = "b".repeat(MAX_LOCAL_QUERY_BYTES);
        let uri: Uri = format!("{path}?{query}").parse().unwrap();
        assert!(!request_target_is_oversized(&uri));

        let oversized_path: Uri = format!("{path}a").parse().unwrap();
        assert!(request_target_is_oversized(&oversized_path));
        let oversized_query: Uri = format!("/?{query}b").parse().unwrap();
        assert!(request_target_is_oversized(&oversized_query));
    }

    #[test]
    fn content_length_is_unique_ascii_decimal() {
        let mut headers = HeaderMap::new();
        assert_eq!(declared_length(&headers), Ok(None));

        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("12"));
        assert_eq!(declared_length(&headers), Ok(Some(12)));

        headers.append(CONTENT_LENGTH, HeaderValue::from_static("12"));
        assert_eq!(declared_length(&headers), Err(()));

        headers.remove(CONTENT_LENGTH);
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("+12"));
        assert_eq!(declared_length(&headers), Err(()));

        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_static("184467440737095516160"),
        );
        assert_eq!(declared_length(&headers), Err(()));
    }

    #[test]
    fn security_headers_default_to_no_store_without_overriding_explicit_cache_policy() {
        let mut headers = HeaderMap::new();
        secure_headers(&mut headers);
        assert_eq!(
            headers.get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-store"))
        );

        headers.insert(
            CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        secure_headers(&mut headers);
        assert_eq!(
            headers.get(CACHE_CONTROL),
            Some(&HeaderValue::from_static(
                "public, max-age=31536000, immutable"
            ))
        );
    }

    #[tokio::test]
    async fn native_request_bodies_match_declared_content_length() {
        let mut exact = Request::new(Body::from("body"));
        exact
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("4"));
        assert_eq!(read_request_body(exact, 4).await.unwrap(), "body");

        let mut duplicate = Request::new(Body::from("body"));
        duplicate
            .headers_mut()
            .append(CONTENT_LENGTH, HeaderValue::from_static("4"));
        duplicate
            .headers_mut()
            .append(CONTENT_LENGTH, HeaderValue::from_static("4"));
        assert!(read_request_body(duplicate, 4).await.is_err());

        let mut mismatch = Request::new(Body::from("body"));
        mismatch
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("3"));
        assert!(read_request_body(mismatch, 4).await.is_err());

        let mut oversized = Request::new(Body::from("body"));
        oversized
            .headers_mut()
            .insert(CONTENT_LENGTH, HeaderValue::from_static("5"));
        assert!(read_request_body(oversized, 4).await.is_err());

        let mut transfer_encoded = Request::new(Body::empty());
        transfer_encoded
            .headers_mut()
            .insert("transfer-encoding", HeaderValue::from_static("chunked"));
        assert!(read_request_body(transfer_encoded, 0).await.is_err());
    }

    #[tokio::test]
    async fn loopback_auth_is_one_use_exact_origin_and_header_bounded() {
        let fixture = TestProxy::start().await;
        let address = fixture.handle.address();
        let host = format!("127.0.0.1:{}", address.port());
        let origin = format!("http://{host}");
        let launch_path = fixture
            .handle
            .launch_url()
            .strip_prefix(&origin)
            .unwrap()
            .to_owned();

        let excessive_headers = (0..MAX_HEADER_COUNT)
            .map(|index| format!("x-test-{index}: value\r\n"))
            .collect::<String>();
        let oversized = raw_exchange(
            address,
            &format!(
                "GET {launch_path} HTTP/1.1\r\nHost: {host}\r\n{excessive_headers}Connection: close\r\n\r\n"
            ),
        )
        .await;
        assert_ne!(response_status(&oversized), Some(303));

        let oversized_query = "x".repeat(MAX_LOCAL_QUERY_BYTES + 1);
        let rejected_query = raw_exchange(
            address,
            &format!(
                "GET {launch_path}?{oversized_query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&rejected_query), Some(414));

        let launched = raw_exchange(
            address,
            &format!("GET {launch_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&launched), Some(303));
        let set_cookie = response_header(&launched, "set-cookie").unwrap();
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap().to_owned();

        let replay = raw_exchange(
            address,
            &format!("GET {launch_path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&replay), Some(404));

        let missing_cookie = raw_exchange(
            address,
            &format!("GET /_native/state HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&missing_cookie), Some(401));

        let wrong_host = raw_exchange(
            address,
            &format!(
                "GET /_native/state HTTP/1.1\r\nHost: localhost:{}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n",
                address.port()
            ),
        )
        .await;
        assert_eq!(response_status(&wrong_host), Some(404));

        let authenticated = raw_exchange(
            address,
            &format!(
                "GET /_native/state HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&authenticated), Some(200));
        assert!(response_header(&authenticated, "content-security-policy").is_some());
        assert!(response_header(&authenticated, "access-control-allow-origin").is_none());

        let native_query = raw_exchange(
            address,
            &format!(
                "GET /_native/state?unexpected=true HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&native_query), Some(400));

        let wrong_origin = raw_exchange(
            address,
            &format!(
                "POST /_native/pair/poll HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nOrigin: http://attacker.invalid\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&wrong_origin), Some(403));

        let cross_site = raw_exchange(
            address,
            &format!(
                "POST /_native/pair/poll HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nOrigin: {origin}\r\nSec-Fetch-Site: cross-site\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&cross_site), Some(403));

        let valid_origin = raw_exchange(
            address,
            &format!(
                "POST /_native/pair/poll HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nOrigin: {origin}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&valid_origin), Some(200));

        let oversized_body = vec![b'x'; MAX_NATIVE_BODY_BYTES + 1];
        let mut request = format!(
            "POST /_native/pair HTTP/1.1\r\nHost: {host}\r\nCookie: {cookie}\r\nOrigin: {origin}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            oversized_body.len()
        )
        .into_bytes();
        request.extend_from_slice(&oversized_body);
        let oversized_body_response = raw_exchange_bytes(address, &request).await;
        assert_eq!(response_status(&oversized_body_response), Some(413));

        let websocket = raw_exchange(
            address,
            &format!(
                "GET /api/v1/events HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&websocket), Some(401));

        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn webkit_top_level_navigations_pass_the_context_check() {
        let host_endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let fixture = TestProxy::start_paired(&host_endpoint).await;
        let (_, app_cookie) = authenticate(&fixture).await;
        let app_address = fixture.handle.address();
        let app_host = format!("127.0.0.1:{}", app_address.port());

        // WebKit stamps the redirect it follows after the 303 with
        // `Sec-Fetch-Site: none` (a top-level navigation has no
        // associated origin). Requiring `same-origin` here is what
        // turned every real-device app open into a black screen.
        let navigated = raw_exchange(
            app_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nSec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&navigated), Some(200));

        // A drive-by from a foreign page still arrives `cross-site`
        // and is still rejected.
        let cross_site = raw_exchange(
            app_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nSec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&cross_site), Some(403));
        assert!(std::str::from_utf8(&cross_site)
            .unwrap()
            .contains("Request context rejected"));

        // The `none` exception is navigation-only: a non-navigate
        // request cannot claim an unassociated context.
        let none_without_navigation = raw_exchange(
            app_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nSec-Fetch-Site: none\r\nSec-Fetch-Mode: cors\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&none_without_navigation), Some(403));

        // The settings origin runs the same gate on a second
        // loopback port, through its own launch-token flow.
        let index = raw_exchange(
            app_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        let index = std::str::from_utf8(&index).unwrap();
        let settings_url = index
            .split_once("<meta name=\"ygg-native-settings-url\" content=\"")
            .and_then(|(_, rest)| rest.split_once("\">"))
            .map(|(url, _)| url)
            .unwrap();
        let settings_address = fixture.handle.settings_address();
        let settings_host = format!("127.0.0.1:{}", settings_address.port());
        let settings_origin = format!("http://{settings_host}");
        let settings_path = settings_url.strip_prefix(&settings_origin).unwrap();

        let settings_launched = raw_exchange(
            settings_address,
            &format!("GET {settings_path} HTTP/1.1\r\nHost: {settings_host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&settings_launched), Some(303));
        let settings_cookie = response_header(&settings_launched, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let settings_navigated = raw_exchange(
            settings_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {settings_host}\r\nCookie: {settings_cookie}\r\nSec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\nSec-Fetch-Dest: document\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&settings_navigated), Some(200));

        fixture.shutdown().await;
        host_endpoint.close().await;
    }

    #[tokio::test]
    async fn paired_access_removal_is_isolated_to_native_settings_origin() {
        let host_endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let fixture = TestProxy::start_paired(&host_endpoint).await;
        let (app_origin, app_cookie) = authenticate(&fixture).await;
        let app_address = fixture.handle.address();
        let app_host = format!("127.0.0.1:{}", app_address.port());

        let app_removal = raw_exchange(
            app_address,
            &format!(
                "DELETE /_native/access HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nOrigin: {app_origin}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&app_removal), Some(409));
        assert!(std::str::from_utf8(&app_removal)
            .unwrap()
            .contains("Paired access can only be removed from native app settings."));
        assert!(fixture.core.paired_profile().await.is_some());

        let index = raw_exchange(
            app_address,
            &format!(
                "GET / HTTP/1.1\r\nHost: {app_host}\r\nCookie: {app_cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&index), Some(200));
        let index = std::str::from_utf8(&index).unwrap();
        let marker = "<meta name=\"ygg-native-settings-url\" content=\"";
        let settings_url = index
            .split_once(marker)
            .and_then(|(_, rest)| rest.split_once("\">"))
            .map(|(url, _)| url)
            .unwrap();
        let settings_address = fixture.handle.settings_address();
        let settings_host = format!("127.0.0.1:{}", settings_address.port());
        let settings_origin = format!("http://{settings_host}");
        let settings_path = settings_url.strip_prefix(&settings_origin).unwrap();

        let unauthenticated = raw_exchange(
            settings_address,
            &format!("GET / HTTP/1.1\r\nHost: {settings_host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&unauthenticated), Some(401));

        let launched = raw_exchange(
            settings_address,
            &format!(
                "GET {settings_path} HTTP/1.1\r\nHost: {settings_host}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&launched), Some(303));
        let settings_cookie = response_header(&launched, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let launch_replay = raw_exchange(
            settings_address,
            &format!(
                "GET {settings_path} HTTP/1.1\r\nHost: {settings_host}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&launch_replay), Some(404));
        let authenticated_reentry = raw_exchange(
            settings_address,
            &format!(
                "GET {settings_path} HTTP/1.1\r\nHost: {settings_host}\r\nCookie: {settings_cookie}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&authenticated_reentry), Some(303));
        assert!(response_header(&authenticated_reentry, "set-cookie").is_none());

        let cross_origin = raw_exchange(
            settings_address,
            &format!(
                "DELETE /_native/access HTTP/1.1\r\nHost: {settings_host}\r\nCookie: {settings_cookie}\r\nOrigin: {app_origin}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&cross_origin), Some(403));
        assert!(fixture.core.paired_profile().await.is_some());

        let removed = raw_exchange(
            settings_address,
            &format!(
                "DELETE /_native/access HTTP/1.1\r\nHost: {settings_host}\r\nCookie: {settings_cookie}\r\nOrigin: {settings_origin}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        assert_eq!(response_status(&removed), Some(200));
        assert!(std::str::from_utf8(&removed)
            .unwrap()
            .contains("\"phase\":\"restartRequired\""));
        assert!(fixture.core.paired_profile().await.is_none());

        fixture.shutdown().await;
        host_endpoint.close().await;
    }

    #[tokio::test]
    async fn websocket_bridge_forwards_events_and_rejects_browser_messages() {
        let host_endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let fixture = TestProxy::start_paired(&host_endpoint).await;
        let (origin, cookie) = authenticate(&fixture).await;
        let loopback_host = format!("127.0.0.1:{}", fixture.handle.address().port());
        let event = br#"{"type":"snapshotRequired","cursor":9}"#;

        let host_task = tokio::spawn({
            let host_endpoint = host_endpoint.clone();
            async move {
                let connection = host_endpoint.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(matches!(
                    &head,
                    RequestHead::Events { path, .. } if path == "/api/v1/events"
                ));
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: ygg_companion_protocol::PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_record(&mut send, event, MAX_EVENT_BYTES)
                    .await
                    .unwrap();
                let reset = send.stopped().await.unwrap().unwrap();
                (connection, reset)
            }
        });

        let mut request = format!("ws://{loopback_host}/api/v1/events")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        request
            .headers_mut()
            .insert("sec-fetch-site", "same-origin".parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), 101);

        let forwarded = next_websocket_text(&mut socket).await;
        assert_eq!(forwarded.as_bytes(), event);
        socket
            .send(ClientWebSocketMessage::Text("not accepted".into()))
            .await
            .unwrap();
        let close = next_websocket_close(&mut socket).await;
        assert_eq!(close.code, CloseCode::Policy);
        assert_eq!(close.reason, "client messages are not accepted");

        let (_, reset) = tokio::time::timeout(Duration::from_secs(5), host_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset.into_inner() as u32, RESET_CANCELLED);
        fixture.shutdown().await;
        host_endpoint.close().await;
    }

    #[tokio::test]
    async fn proxy_shutdown_cancels_detached_websocket_bridges() {
        let host_endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![ygg_companion_protocol::COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let fixture = TestProxy::start_paired(&host_endpoint).await;
        let (origin, cookie) = authenticate(&fixture).await;
        let loopback_host = format!("127.0.0.1:{}", fixture.handle.address().port());
        let event = br#"{"type":"snapshotRequired","cursor":10}"#;

        let host_task = tokio::spawn({
            let host_endpoint = host_endpoint.clone();
            async move {
                let connection = host_endpoint.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(matches!(&head, RequestHead::Events { .. }));
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: ygg_companion_protocol::PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_record(&mut send, event, MAX_EVENT_BYTES)
                    .await
                    .unwrap();
                let reset = send.stopped().await.unwrap().unwrap();
                (connection, reset)
            }
        });

        let mut request = format!("ws://{loopback_host}/api/v1/events")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("origin", origin.parse().unwrap());
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        request
            .headers_mut()
            .insert("sec-fetch-site", "same-origin".parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), 101);
        assert_eq!(next_websocket_text(&mut socket).await.as_bytes(), event);

        let TestProxy {
            handle,
            core,
            _directory,
        } = fixture;
        handle.shutdown().await;
        let (_, reset) = tokio::time::timeout(Duration::from_secs(5), host_task)
            .await
            .expect("remote event stream survived proxy shutdown")
            .unwrap();
        assert_eq!(reset.into_inner() as u32, RESET_CANCELLED);

        drop(socket);
        core.remote().close().await;
        host_endpoint.close().await;
    }

    struct TestProxy {
        handle: ProxyHandle,
        core: Arc<NativeCore>,
        _directory: TempDir,
    }

    impl TestProxy {
        async fn start() -> Self {
            let directory = TempDir::new().unwrap();
            let profiles = ProfileStore::open(directory.path().join("state")).unwrap();
            let credentials: SharedCredentials = Arc::new(MemoryCredentials::default());
            let key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
            let endpoint = Endpoint::empty_builder(RelayMode::Disabled)
                .secret_key(key.clone_for_endpoint())
                .bind()
                .await
                .unwrap();
            let remote = RemoteClient::for_test(endpoint);
            let core = NativeCore::for_test(credentials, key, profiles, remote, None).unwrap();
            let files = test_bundle_files();
            let assets = AssetBundle::verified(|path| files.get(path).cloned()).unwrap();
            let handle = ProxyHandle::start(core.clone(), assets).await.unwrap();
            Self {
                handle,
                core,
                _directory: directory,
            }
        }

        async fn start_paired(host_endpoint: &Endpoint) -> Self {
            let directory = TempDir::new().unwrap();
            let profiles = ProfileStore::open(directory.path().join("state")).unwrap();
            let credentials: SharedCredentials = Arc::new(MemoryCredentials::default());
            let key = EndpointKey::load_or_create(credentials.as_ref()).unwrap();
            let endpoint = Endpoint::empty_builder(RelayMode::Disabled)
                .secret_key(key.clone_for_endpoint())
                .bind()
                .await
                .unwrap();
            let target = HostTarget {
                host_id: "host-test".to_owned(),
                host_endpoint_id: host_endpoint.id().to_string(),
                relay_urls: vec![iroh::defaults::prod::default_na_east_relay()
                    .url
                    .to_string()],
                direct_addresses: host_endpoint
                    .addr()
                    .ip_addrs()
                    .map(ToString::to_string)
                    .collect(),
            };
            let profile = HostProfile::from_approval(
                target,
                key.public_id(),
                &DeviceSummary {
                    id: "device-test".to_owned(),
                    name: "Test phone".to_owned(),
                    platform: DevicePlatform::Other,
                    paired_at_ms: 1,
                    last_seen_at_ms: None,
                    revoked_at_ms: None,
                    connected: true,
                },
            )
            .unwrap();
            let remote = RemoteClient::for_test(endpoint);
            let core =
                NativeCore::for_test(credentials, key, profiles, remote, Some(profile)).unwrap();
            let files = test_bundle_files();
            let assets = AssetBundle::verified(|path| files.get(path).cloned()).unwrap();
            let handle = ProxyHandle::start(core.clone(), assets).await.unwrap();
            Self {
                handle,
                core,
                _directory: directory,
            }
        }

        async fn shutdown(self) {
            self.handle.shutdown().await;
            self.core.remote().close().await;
        }
    }

    async fn authenticate(fixture: &TestProxy) -> (String, String) {
        let address = fixture.handle.address();
        let host = format!("127.0.0.1:{}", address.port());
        let origin = format!("http://{host}");
        let path = fixture.handle.launch_url().strip_prefix(&origin).unwrap();
        let response = raw_exchange(
            address,
            &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(response_status(&response), Some(303));
        let cookie = response_header(&response, "set-cookie")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        (origin, cookie)
    }

    async fn next_websocket_text(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> String {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            match message {
                ClientWebSocketMessage::Text(text) => return text.to_string(),
                ClientWebSocketMessage::Ping(_) | ClientWebSocketMessage::Pong(_) => {}
                other => panic!("expected text event, received {other:?}"),
            }
        }
    }

    async fn next_websocket_close(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> tokio_tungstenite::tungstenite::protocol::CloseFrame {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            match message {
                ClientWebSocketMessage::Close(Some(close)) => return close,
                ClientWebSocketMessage::Ping(_) | ClientWebSocketMessage::Pong(_) => {}
                other => panic!("expected close frame, received {other:?}"),
            }
        }
    }

    fn test_bundle_files() -> HashMap<String, Vec<u8>> {
        let index = b"<!doctype html><html><head></head><body></body></html>".to_vec();
        let script = b"console.log('test');".to_vec();
        let sums = format!(
            "{}  assets/app.js\n{}  index.html\n",
            hex_sha256(&script),
            hex_sha256(&index)
        )
        .into_bytes();
        HashMap::from([
            ("SHA256SUMS".to_owned(), sums.clone()),
            ("bundle.sha256".to_owned(), hex_sha256(&sums).into_bytes()),
            ("assets/app.js".to_owned(), script),
            ("index.html".to_owned(), index),
        ])
    }

    async fn raw_exchange(address: SocketAddr, request: &str) -> Vec<u8> {
        raw_exchange_bytes(address, request.as_bytes()).await
    }

    async fn raw_exchange_bytes(address: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(request).await.unwrap();
        let mut response = Vec::new();
        let mut buffer = [0u8; 4096];
        while !http_response_complete(&response) {
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            if read == 0 {
                break;
            }
            response.extend_from_slice(&buffer[..read]);
            assert!(response.len() <= MAX_HTTP1_BUFFER_BYTES + MAX_NATIVE_BODY_BYTES);
        }
        response
    }

    fn http_response_complete(response: &[u8]) -> bool {
        let Some(head_end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            return false;
        };
        let body_offset = head_end + 4;
        if response_status(response) == Some(101) {
            return true;
        }
        if let Some(length) = response_header(response, "content-length")
            .and_then(|value| value.parse::<usize>().ok())
        {
            return response.len() >= body_offset.saturating_add(length);
        }
        response[body_offset..].ends_with(b"\r\n0\r\n\r\n")
    }

    fn response_status(response: &[u8]) -> Option<u16> {
        let head = response.split(|byte| *byte == b'\n').next()?;
        std::str::from_utf8(head)
            .ok()?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    fn response_header(response: &[u8], expected_name: &str) -> Option<String> {
        let head_end = response.windows(4).position(|bytes| bytes == b"\r\n\r\n")?;
        std::str::from_utf8(&response[..head_end])
            .ok()?
            .lines()
            .skip(1)
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case(expected_name)
                    .then(|| value.trim().to_owned())
            })
    }
}
