use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{TransportConfig, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, TransportAddr};
use serde::Deserialize;
use tokio::sync::Mutex;
use ygg_companion_protocol::{
    expect_end, read_body, read_chunk, read_head, read_record, write_body, write_head, HttpMethod,
    PairingOperation, PairingReply, ProtocolError, RequestHead, ResponseHead, RouteLimits,
    COMPANION_ALPN, EVENT_HEARTBEAT_RECORD, MAX_EVENT_BYTES, PROTOCOL_VERSION, RESET_CANCELLED,
    RESET_FRAME_INVALID, RESET_PROTOCOL_MISMATCH, RESET_REPLAY_REQUIRED, RESET_REVOKED,
    RESET_UNAUTHORIZED,
};
use zeroize::Zeroizing;

use crate::credentials::EndpointKey;
use crate::profile::HostTarget;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECTION_QUEUE_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECTION_INVALIDATE_TIMEOUT: Duration = Duration::from_secs(45);
const ENDPOINT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const OPEN_STREAM_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const RESPONSE_BODY_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const EVENT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const PAIRING_RESPONSE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ClientError {
    #[error("the stored host address is invalid")]
    InvalidTarget,
    #[error("the host identity did not match the pinned identity")]
    IdentityMismatch,
    #[error("the companion protocol version is incompatible")]
    ProtocolMismatch,
    #[error("this device is not authorized by the host")]
    Unauthorized,
    #[error("this device has been revoked")]
    Revoked,
    #[error("event replay is required")]
    ReplayRequired,
    #[error("the companion host did not respond in time")]
    Timeout,
    #[error("the companion connection is unavailable")]
    Unavailable,
    #[error("the companion host returned an invalid response")]
    InvalidResponse,
    #[error("the companion host rejected the request")]
    Rejected,
}

#[derive(Clone)]
pub(crate) struct RemoteClient {
    endpoint: Endpoint,
    relay_map: RelayMap,
    connection: Arc<Mutex<ConnectionState>>,
    connect_gate: Arc<Mutex<()>>,
    require_relay_online: bool,
}

#[derive(Default)]
struct ConnectionState {
    cached: Option<CachedConnection>,
    generation: u64,
    closed: bool,
}

#[derive(Clone)]
struct CachedConnection {
    endpoint_id: String,
    connection: iroh::endpoint::Connection,
}

impl RemoteClient {
    pub(crate) async fn start(key: &EndpointKey) -> Result<Self, ClientError> {
        let relay_map = explicit_n0_relay_map();
        let endpoint = Endpoint::empty_builder(RelayMode::Custom(relay_map.clone()))
            .secret_key(key.clone_for_endpoint())
            .transport_config(client_transport_config())
            .bind()
            .await
            .map_err(|_| ClientError::Unavailable)?;
        Ok(Self {
            endpoint,
            relay_map,
            connection: Arc::new(Mutex::new(ConnectionState::default())),
            connect_gate: Arc::new(Mutex::new(())),
            require_relay_online: true,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            relay_map: RelayMap::empty(),
            connection: Arc::new(Mutex::new(ConnectionState::default())),
            connect_gate: Arc::new(Mutex::new(())),
            require_relay_online: false,
        }
    }

    pub(crate) async fn close(&self) {
        self.clear_connection(true, b"native shutdown").await;
        let _ = tokio::time::timeout(ENDPOINT_CLOSE_TIMEOUT, self.endpoint.close()).await;
    }

    pub(crate) async fn invalidate_connection(&self) {
        self.clear_connection(false, b"native reconnect").await;
    }

    async fn clear_connection(&self, close_client: bool, reason: &'static [u8]) {
        let Ok(mut state) =
            tokio::time::timeout(CONNECTION_INVALIDATE_TIMEOUT, self.connection.lock()).await
        else {
            return;
        };
        state.generation = state.generation.wrapping_add(1);
        if close_client {
            state.closed = true;
        }
        let cached = state.cached.take();
        drop(state);
        if let Some(cached) = cached {
            cached.connection.close(RESET_CANCELLED.into(), reason);
        }
    }

    pub(crate) async fn pair(
        &self,
        target: &HostTarget,
        operation: PairingOperation,
    ) -> Result<PairingReply, ClientError> {
        let retry = operation.clone();
        match self.pair_once(target, operation).await {
            Err(ClientError::Timeout | ClientError::Unavailable) => {
                self.pair_once(target, retry).await
            }
            result => result,
        }
    }

    async fn pair_once(
        &self,
        target: &HostTarget,
        operation: PairingOperation,
    ) -> Result<PairingReply, ClientError> {
        let request_id = random_request_id()?;
        let head = RequestHead::Pairing {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            operation,
        };
        head.validate().map_err(map_protocol_error)?;
        let (mut send, mut recv) = self.open_stream(target).await?;
        match tokio::time::timeout(REQUEST_WRITE_TIMEOUT, write_head(&mut send, &head)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                cancel_stream(send, recv);
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                cancel_stream(send, recv);
                return Err(ClientError::Timeout);
            }
        }
        if send.finish().is_err() {
            cancel_stream(send, recv);
            return Err(ClientError::Unavailable);
        }
        let response: ResponseHead =
            match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, read_head(&mut recv)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    cancel_stream(send, recv);
                    return Err(map_protocol_error(error));
                }
                Err(_) => {
                    cancel_stream(send, recv);
                    return Err(ClientError::Timeout);
                }
            };
        if let Err(error) = response.validate(&request_id) {
            cancel_stream(send, recv);
            return Err(map_protocol_error(error));
        }
        let expected_length = match response.content_length(PAIRING_RESPONSE_BYTES) {
            Ok(expected_length) => expected_length,
            Err(_) => {
                cancel_stream_with_code(send, recv, RESET_FRAME_INVALID);
                return Err(ClientError::InvalidResponse);
            }
        };
        let response_deadline = tokio::time::Instant::now() + RESPONSE_CHUNK_TIMEOUT;
        let body = match tokio::time::timeout_at(
            response_read_deadline(response_deadline),
            read_body(&mut recv, PAIRING_RESPONSE_BYTES),
        )
        .await
        {
            Ok(Ok(body)) => body,
            Ok(Err(error)) => {
                cancel_stream(send, recv);
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                cancel_stream(send, recv);
                return Err(ClientError::Timeout);
            }
        };
        let body = Zeroizing::new(body);
        match tokio::time::timeout_at(
            response_read_deadline(response_deadline),
            expect_end(&mut recv),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(ProtocolError::TrailingData)) => {
                cancel_stream_with_code(send, recv, RESET_FRAME_INVALID);
                return Err(ClientError::InvalidResponse);
            }
            Ok(Err(error)) => {
                cancel_stream(send, recv);
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                cancel_stream(send, recv);
                return Err(ClientError::Timeout);
            }
        }
        if expected_length.is_some_and(|expected| expected != body.len()) {
            cancel_stream_with_code(send, recv, RESET_FRAME_INVALID);
            return Err(ClientError::InvalidResponse);
        }
        if response.status != 200 {
            return Err(pairing_rejection(response.status, &body));
        }
        let reply: PairingReply =
            serde_json::from_slice(&body).map_err(|_| ClientError::InvalidResponse)?;
        reply.validate().map_err(|_| ClientError::InvalidResponse)?;
        Ok(reply)
    }

    pub(crate) async fn http(
        &self,
        target: &HostTarget,
        method: HttpMethod,
        path: String,
        content_type: Option<String>,
        body: Vec<u8>,
    ) -> Result<RemoteHttpResponse, ClientError> {
        let request_id = random_request_id()?;
        let head = RequestHead::Http {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            method,
            path,
            content_type,
        };
        let limits = head
            .validate()
            .map_err(map_protocol_error)?
            .ok_or(ClientError::InvalidResponse)?;
        if body.len() > limits.request_bytes {
            return Err(ClientError::InvalidResponse);
        }
        let (mut send, mut recv) = self.open_stream(target).await?;
        match tokio::time::timeout(REQUEST_WRITE_TIMEOUT, async {
            write_head(&mut send, &head).await?;
            write_body(&mut send, &body).await
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                cancel_stream(send, recv);
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                cancel_stream(send, recv);
                return Err(ClientError::Timeout);
            }
        }
        if send.finish().is_err() {
            cancel_stream(send, recv);
            return Err(ClientError::Unavailable);
        }
        let response: ResponseHead =
            match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, read_head(&mut recv)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    cancel_stream(send, recv);
                    return Err(map_protocol_error(error));
                }
                Err(_) => {
                    cancel_stream(send, recv);
                    return Err(ClientError::Timeout);
                }
            };
        if let Err(error) = response.validate(&request_id) {
            cancel_stream(send, recv);
            return Err(map_protocol_error(error));
        }
        let expected_length = match response.content_length(limits.response_bytes) {
            Ok(expected_length) => expected_length,
            Err(_) => {
                cancel_stream_with_code(send, recv, RESET_FRAME_INVALID);
                return Err(ClientError::InvalidResponse);
            }
        };
        Ok(RemoteHttpResponse {
            status: response.status,
            headers: response.headers,
            body: RemoteBody {
                send,
                recv,
                remaining: limits.response_bytes,
                expected_length,
                received: 0,
                deadline: tokio::time::Instant::now() + RESPONSE_BODY_TIMEOUT,
                complete: false,
            },
        })
    }

    pub(crate) async fn events(
        &self,
        target: &HostTarget,
    ) -> Result<RemoteEventStream, ClientError> {
        let request_id = random_request_id()?;
        let head = RequestHead::Events {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            path: "/api/v1/events".to_owned(),
        };
        head.validate().map_err(map_protocol_error)?;
        let (mut send, mut recv) = self.open_stream(target).await?;
        match tokio::time::timeout(REQUEST_WRITE_TIMEOUT, write_head(&mut send, &head)).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                cancel_stream(send, recv);
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                cancel_stream(send, recv);
                return Err(ClientError::Timeout);
            }
        }
        if send.finish().is_err() {
            cancel_stream(send, recv);
            return Err(ClientError::Unavailable);
        }
        let response: ResponseHead =
            match tokio::time::timeout(RESPONSE_HEAD_TIMEOUT, read_head(&mut recv)).await {
                Ok(Ok(response)) => response,
                Ok(Err(error)) => {
                    cancel_stream(send, recv);
                    return Err(map_protocol_error(error));
                }
                Err(_) => {
                    cancel_stream(send, recv);
                    return Err(ClientError::Timeout);
                }
            };
        if let Err(error) = response.validate(&request_id) {
            cancel_stream(send, recv);
            return Err(map_protocol_error(error));
        }
        if response.status != 200 {
            let error = match response.status {
                401 => ClientError::Unauthorized,
                403 => ClientError::Revoked,
                _ => ClientError::Rejected,
            };
            cancel_stream(send, recv);
            return Err(error);
        }
        Ok(RemoteEventStream {
            send,
            recv,
            complete: false,
        })
    }

    async fn open_stream(
        &self,
        target: &HostTarget,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream), ClientError> {
        let connection = self.connection(target).await?;
        tokio::time::timeout(OPEN_STREAM_TIMEOUT, connection.open_bi())
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|_| classify_connection(&connection))
    }

    async fn connection(
        &self,
        target: &HostTarget,
    ) -> Result<iroh::endpoint::Connection, ClientError> {
        target.validate().map_err(|_| ClientError::InvalidTarget)?;
        let address = endpoint_address(target, &self.relay_map, self.require_relay_online)?;

        let stale = {
            let mut state = tokio::time::timeout(CONNECTION_QUEUE_TIMEOUT, self.connection.lock())
                .await
                .map_err(|_| ClientError::Timeout)?;
            if state.closed {
                return Err(ClientError::Unavailable);
            }
            if let Some(existing) = state.cached.as_ref() {
                if existing.endpoint_id == target.host_endpoint_id
                    && existing.connection.close_reason().is_none()
                {
                    return Ok(existing.connection.clone());
                }
            }
            state.cached.take()
        };
        if let Some(stale) = stale {
            stale
                .connection
                .close(RESET_CANCELLED.into(), b"native reconnect");
        }

        // Serialize connection establishment without holding the cache mutex so
        // lifecycle invalidation and local access removal remain immediate.
        let _connect = tokio::time::timeout(CONNECTION_QUEUE_TIMEOUT, self.connect_gate.lock())
            .await
            .map_err(|_| ClientError::Timeout)?;
        let (generation, stale) = {
            let mut state = tokio::time::timeout(CONNECTION_QUEUE_TIMEOUT, self.connection.lock())
                .await
                .map_err(|_| ClientError::Timeout)?;
            if state.closed {
                return Err(ClientError::Unavailable);
            }
            if let Some(existing) = state.cached.as_ref() {
                if existing.endpoint_id == target.host_endpoint_id
                    && existing.connection.close_reason().is_none()
                {
                    return Ok(existing.connection.clone());
                }
            }
            (state.generation, state.cached.take())
        };
        if let Some(stale) = stale {
            stale
                .connection
                .close(RESET_CANCELLED.into(), b"native reconnect");
        }

        if self.require_relay_online {
            tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint.online())
                .await
                .map_err(|_| ClientError::Timeout)?;
        }
        let connection = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.endpoint.connect(address, COMPANION_ALPN),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|_| ClientError::Unavailable)?;
        if connection.remote_id().to_string() != target.host_endpoint_id {
            connection.close(RESET_CANCELLED.into(), b"identity mismatch");
            return Err(ClientError::IdentityMismatch);
        }

        let mut state = tokio::time::timeout(CONNECTION_QUEUE_TIMEOUT, self.connection.lock())
            .await
            .map_err(|_| {
                connection.close(RESET_CANCELLED.into(), b"connection cache unavailable");
                ClientError::Timeout
            })?;
        if state.closed || state.generation != generation {
            drop(state);
            connection.close(RESET_CANCELLED.into(), b"connection invalidated");
            return Err(ClientError::Unavailable);
        }
        state.cached = Some(CachedConnection {
            endpoint_id: target.host_endpoint_id.clone(),
            connection: connection.clone(),
        });
        Ok(connection)
    }
}

fn response_read_deadline(aggregate: tokio::time::Instant) -> tokio::time::Instant {
    std::cmp::min(
        aggregate,
        tokio::time::Instant::now() + RESPONSE_CHUNK_TIMEOUT,
    )
}

pub(crate) struct RemoteHttpResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<ygg_companion_protocol::ResponseHeader>,
    pub(crate) body: RemoteBody,
}

pub(crate) struct RemoteBody {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    remaining: usize,
    expected_length: Option<usize>,
    received: usize,
    deadline: tokio::time::Instant,
    complete: bool,
}

impl RemoteBody {
    pub(crate) async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ClientError> {
        let result = tokio::time::timeout_at(
            response_read_deadline(self.deadline),
            read_chunk(&mut self.recv, self.remaining),
        )
        .await;
        let chunk = match result {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(error)) => {
                self.cancel();
                return Err(map_protocol_error(error));
            }
            Err(_) => {
                self.cancel();
                return Err(ClientError::Timeout);
            }
        };
        match chunk {
            Some(chunk) => {
                let Some(received) = self.received.checked_add(chunk.len()) else {
                    self.fail(RESET_FRAME_INVALID);
                    return Err(ClientError::InvalidResponse);
                };
                if self
                    .expected_length
                    .is_some_and(|expected| received > expected)
                {
                    self.fail(RESET_FRAME_INVALID);
                    return Err(ClientError::InvalidResponse);
                }
                self.received = received;
                self.remaining = self.remaining.saturating_sub(chunk.len());
                Ok(Some(chunk))
            }
            None => {
                if self
                    .expected_length
                    .is_some_and(|expected| self.received != expected)
                {
                    self.fail(RESET_FRAME_INVALID);
                    return Err(ClientError::InvalidResponse);
                }
                match tokio::time::timeout_at(
                    response_read_deadline(self.deadline),
                    expect_end(&mut self.recv),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(ProtocolError::TrailingData)) => {
                        self.fail(RESET_FRAME_INVALID);
                        return Err(ClientError::InvalidResponse);
                    }
                    Ok(Err(error)) => {
                        self.cancel();
                        return Err(map_protocol_error(error));
                    }
                    Err(_) => {
                        self.cancel();
                        return Err(ClientError::Timeout);
                    }
                }
                self.complete = true;
                Ok(None)
            }
        }
    }

    fn fail(&mut self, code: u32) {
        if !self.complete {
            let _ = self.send.reset(code.into());
            let _ = self.recv.stop(code.into());
            self.complete = true;
        }
    }

    fn cancel(&mut self) {
        self.fail(RESET_CANCELLED);
    }
}

impl Drop for RemoteBody {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) struct RemoteEventStream {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    complete: bool,
}

impl RemoteEventStream {
    pub(crate) async fn next(&mut self) -> Result<Vec<u8>, ClientError> {
        self.next_with_idle_timeout(EVENT_IDLE_TIMEOUT).await
    }

    async fn next_with_idle_timeout(
        &mut self,
        idle_timeout: Duration,
    ) -> Result<Vec<u8>, ClientError> {
        loop {
            match tokio::time::timeout(idle_timeout, read_record(&mut self.recv, MAX_EVENT_BYTES))
                .await
            {
                Ok(Ok(event)) if event == EVENT_HEARTBEAT_RECORD => continue,
                Ok(Ok(event)) => return Ok(event),
                Ok(Err(error)) => {
                    self.cancel();
                    return Err(map_protocol_error(error));
                }
                Err(_) => {
                    self.cancel();
                    return Err(ClientError::Timeout);
                }
            }
        }
    }

    fn cancel(&mut self) {
        if !self.complete {
            let _ = self.send.reset(RESET_CANCELLED.into());
            let _ = self.recv.stop(RESET_CANCELLED.into());
            self.complete = true;
        }
    }
}

impl Drop for RemoteEventStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn endpoint_address(
    target: &HostTarget,
    relay_map: &RelayMap,
    require_known_relay: bool,
) -> Result<EndpointAddr, ClientError> {
    let endpoint_id =
        EndpointId::from_str(&target.host_endpoint_id).map_err(|_| ClientError::InvalidTarget)?;
    let mut addresses = Vec::new();
    for raw in &target.relay_urls {
        let relay = RelayUrl::from_str(raw).map_err(|_| ClientError::InvalidTarget)?;
        if require_known_relay && !relay_map.contains(&relay) {
            return Err(ClientError::InvalidTarget);
        }
        addresses.push(TransportAddr::Relay(relay));
    }
    for raw in &target.direct_addresses {
        let address = SocketAddr::from_str(raw).map_err(|_| ClientError::InvalidTarget)?;
        addresses.push(TransportAddr::Ip(address));
    }
    if addresses.is_empty() {
        return Err(ClientError::InvalidTarget);
    }
    Ok(EndpointAddr::from_parts(endpoint_id, addresses))
}

fn client_transport_config() -> TransportConfig {
    let mut config = TransportConfig::default();
    config.keep_alive_interval(Some(Duration::from_secs(1)));
    config.max_concurrent_bidi_streams(VarInt::from_u32(0));
    config.max_concurrent_uni_streams(VarInt::from_u32(0));
    config
}

fn explicit_n0_relay_map() -> RelayMap {
    RelayMap::from_iter([
        iroh::defaults::prod::default_na_east_relay(),
        iroh::defaults::prod::default_na_west_relay(),
        iroh::defaults::prod::default_eu_relay(),
        iroh::defaults::prod::default_ap_relay(),
    ])
}

fn random_request_id() -> Result<String, ClientError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ClientError::Unavailable)?;
    let mut encoded = String::with_capacity(36);
    encoded.push_str("native-");
    for byte in bytes {
        use std::fmt::Write as _;
        write!(encoded, "{byte:02x}").map_err(|_| ClientError::Unavailable)?;
    }
    Ok(encoded)
}

fn cancel_stream(send: iroh::endpoint::SendStream, recv: iroh::endpoint::RecvStream) {
    cancel_stream_with_code(send, recv, RESET_CANCELLED);
}

fn cancel_stream_with_code(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    code: u32,
) {
    let _ = send.reset(code.into());
    let _ = recv.stop(code.into());
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingErrorEnvelope {
    error: PairingErrorCode,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairingErrorCode {
    code: String,
}

fn pairing_rejection(status: u16, body: &[u8]) -> ClientError {
    let code = serde_json::from_slice::<PairingErrorEnvelope>(body)
        .ok()
        .filter(|error| error.error.code.len() <= 64)
        .map(|error| error.error.code);
    match code.as_deref() {
        Some("identityMismatch") => ClientError::IdentityMismatch,
        Some("invalidCapability") => ClientError::Unauthorized,
        Some("revoked") => ClientError::Revoked,
        _ => match status {
            401 => ClientError::Unauthorized,
            403 => ClientError::Revoked,
            _ => ClientError::Rejected,
        },
    }
}

fn map_protocol_error(error: ProtocolError) -> ClientError {
    if let ProtocolError::ProtocolMismatch = error {
        return ClientError::ProtocolMismatch;
    }
    if let ProtocolError::Io(io_error) = &error {
        if let Some(read_error) = io_error
            .get_ref()
            .and_then(|source| source.downcast_ref::<iroh::endpoint::ReadError>())
        {
            match read_error {
                iroh::endpoint::ReadError::Reset(code) => return classify_reset(*code),
                iroh::endpoint::ReadError::ConnectionLost(error) => {
                    return classify_connection_error(error)
                }
                _ => {}
            }
        }
        if let Some(write_error) = io_error
            .get_ref()
            .and_then(|source| source.downcast_ref::<iroh::endpoint::WriteError>())
        {
            match write_error {
                iroh::endpoint::WriteError::Stopped(code) => return classify_reset(*code),
                iroh::endpoint::WriteError::ConnectionLost(error) => {
                    return classify_connection_error(error)
                }
                _ => {}
            }
        }
    }
    ClientError::InvalidResponse
}

fn classify_reset(code: iroh::endpoint::VarInt) -> ClientError {
    match code.into_inner() as u32 {
        RESET_PROTOCOL_MISMATCH => ClientError::ProtocolMismatch,
        RESET_FRAME_INVALID => ClientError::InvalidResponse,
        RESET_UNAUTHORIZED => ClientError::Unauthorized,
        RESET_REVOKED => ClientError::Revoked,
        RESET_REPLAY_REQUIRED => ClientError::ReplayRequired,
        _ => ClientError::Unavailable,
    }
}

fn classify_connection_error(error: &iroh::endpoint::ConnectionError) -> ClientError {
    match error {
        iroh::endpoint::ConnectionError::ApplicationClosed(close) => {
            classify_reset(close.error_code)
        }
        _ => ClientError::Unavailable,
    }
}

fn classify_connection(connection: &iroh::endpoint::Connection) -> ClientError {
    connection
        .close_reason()
        .as_ref()
        .map(classify_connection_error)
        .unwrap_or(ClientError::Unavailable)
}

pub(crate) fn method_from_http(method: &axum::http::Method) -> Result<HttpMethod, ClientError> {
    match *method {
        axum::http::Method::GET => Ok(HttpMethod::Get),
        axum::http::Method::POST => Ok(HttpMethod::Post),
        _ => Err(ClientError::Rejected),
    }
}

pub(crate) fn route_limits(method: HttpMethod, path: &str) -> Result<RouteLimits, ClientError> {
    ygg_companion_protocol::classify_operational_route(method, path).map_err(map_protocol_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_companion_protocol::{
        expect_end, read_body, read_head, write_body, write_chunk, write_head, write_record,
        PairingReply, PairingStatusRequest, RequestHead, ResponseHeader, Secret32,
    };

    #[test]
    fn pairing_error_codes_are_not_inferred_from_conflict_status() {
        assert_eq!(
            pairing_rejection(409, br#"{"error":{"code":"conflict"}}"#),
            ClientError::Rejected
        );
        assert_eq!(
            pairing_rejection(409, br#"{"error":{"code":"identityMismatch"}}"#),
            ClientError::IdentityMismatch
        );
    }

    #[test]
    fn target_rejects_relays_outside_explicit_map() {
        let key = iroh::SecretKey::from_bytes(&[7; 32]);
        let target = HostTarget {
            host_id: "host-1".to_owned(),
            host_endpoint_id: key.public().to_string(),
            relay_urls: vec!["https://relay.example.invalid".to_owned()],
            direct_addresses: vec!["127.0.0.1:7777".to_owned()],
        };
        assert_eq!(
            endpoint_address(&target, &explicit_n0_relay_map(), true),
            Err(ClientError::InvalidTarget)
        );
    }

    #[tokio::test]
    async fn pinned_identity_mismatch_fails_before_application_dispatch() {
        let (server, client, mut target) = test_endpoints().await;
        target.host_endpoint_id = iroh::SecretKey::from_bytes(&[19; 32]).public().to_string();

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            client.http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            ),
        )
        .await;
        assert!(!matches!(outcome, Ok(Ok(_))));

        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn bounded_http_round_trip_uses_pinned_direct_endpoint() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(matches!(
                    &head,
                    RequestHead::Http {
                        method: HttpMethod::Get,
                        path,
                        ..
                    } if path == "/api/v1/bootstrap"
                ));
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                let request_id = head.request_id().to_owned();
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id,
                        status: 200,
                        headers: vec![
                            ResponseHeader {
                                name: "content-type".to_owned(),
                                value: "application/json".to_owned(),
                            },
                            ResponseHeader {
                                name: "content-length".to_owned(),
                                value: "11".to_owned(),
                            },
                        ],
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, br#"{"ok":true}"#).await.unwrap();
                let _ = send.finish();
                connection
            }
        });

        let mut response = client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let mut body = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await.unwrap() {
            body.extend_from_slice(&chunk);
        }
        assert_eq!(body, br#"{"ok":true}"#);

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn response_body_has_an_aggregate_deadline() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_chunk(&mut send, b"first").await.unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = write_body(&mut send, b"late").await;
                let _ = send.finish();
                connection
            }
        });

        let mut response = client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        response.body.deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        assert_eq!(
            response.body.next_chunk().await.unwrap(),
            Some(b"first".to_vec())
        );
        assert_eq!(response.body.next_chunk().await, Err(ClientError::Timeout));

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn response_body_must_match_declared_content_length() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ResponseHeader {
                            name: "content-length".to_owned(),
                            value: "20".to_owned(),
                        }],
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, b"short").await.unwrap();
                let _ = send.finish();
                connection
            }
        });

        let mut response = client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.body.next_chunk().await.unwrap(),
            Some(b"short".to_vec())
        );
        assert_eq!(
            response.body.next_chunk().await,
            Err(ClientError::InvalidResponse)
        );

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn response_rejects_bytes_after_the_framed_terminator() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ResponseHeader {
                            name: "content-length".to_owned(),
                            value: "5".to_owned(),
                        }],
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, b"valid").await.unwrap();
                tokio::io::AsyncWriteExt::write_all(&mut send, b"trailing")
                    .await
                    .unwrap();
                send.finish().unwrap();
                connection
            }
        });

        let mut response = client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.body.next_chunk().await.unwrap(),
            Some(b"valid".to_vec())
        );
        assert_eq!(
            response.body.next_chunk().await,
            Err(ClientError::InvalidResponse)
        );

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pairing_cancel_round_trip_uses_pairing_framing() {
        let (server, client, target) = test_endpoints().await;
        let poll_token = Secret32::from_bytes([11; 32]);
        let operation = PairingOperation::Cancel(PairingStatusRequest {
            request_id: "pairing-1".to_owned(),
            poll_token: poll_token.clone(),
        });
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                let RequestHead::Pairing {
                    request_id,
                    operation: PairingOperation::Cancel(request),
                    ..
                } = head
                else {
                    panic!("expected a pairing cancellation");
                };
                assert_eq!(request.request_id, "pairing-1");
                assert!(request.poll_token.constant_time_eq(&poll_token));
                let body = serde_json::to_vec(&PairingReply::Cancelled).unwrap();
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id,
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, &body).await.unwrap();
                let _ = send.finish();
                connection
            }
        });

        let reply = client.pair(&target, operation).await.unwrap();
        assert!(matches!(reply, PairingReply::Cancelled));

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pairing_retries_the_same_operation_after_a_lost_response() {
        let (server, client, target) = test_endpoints().await;
        let poll_token = Secret32::from_bytes([12; 32]);
        let operation = PairingOperation::Status(PairingStatusRequest {
            request_id: "pairing-retry".to_owned(),
            poll_token: poll_token.clone(),
        });
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                for attempt in 0..2 {
                    let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                    let head: RequestHead = read_head(&mut recv).await.unwrap();
                    let RequestHead::Pairing {
                        request_id,
                        operation: PairingOperation::Status(request),
                        ..
                    } = head
                    else {
                        panic!("expected a pairing status request");
                    };
                    assert_eq!(request.request_id, "pairing-retry");
                    assert!(request.poll_token.constant_time_eq(&poll_token));
                    expect_end(&mut recv).await.unwrap();
                    if attempt == 0 {
                        send.reset(RESET_CANCELLED.into()).unwrap();
                        continue;
                    }
                    let body = serde_json::to_vec(&PairingReply::Pending {
                        phrase: "amber · birch · cedar · dusk · ember · fjord".to_owned(),
                        expires_at_ms: 10,
                    })
                    .unwrap();
                    write_head(
                        &mut send,
                        &ResponseHead {
                            protocol: PROTOCOL_VERSION,
                            request_id,
                            status: 200,
                            headers: vec![ResponseHeader {
                                name: "content-length".to_owned(),
                                value: body.len().to_string(),
                            }],
                        },
                    )
                    .await
                    .unwrap();
                    write_body(&mut send, &body).await.unwrap();
                    send.finish().unwrap();
                }
                connection
            }
        });

        let reply = client.pair(&target, operation).await.unwrap();
        assert!(matches!(
            reply,
            PairingReply::Pending {
                expires_at_ms: 10,
                ..
            }
        ));

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pairing_response_must_match_declared_content_length() {
        let (server, client, target) = test_endpoints().await;
        let operation = PairingOperation::Status(PairingStatusRequest {
            request_id: "pairing-1".to_owned(),
            poll_token: Secret32::from_bytes([13; 32]),
        });
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                let body = serde_json::to_vec(&PairingReply::Cancelled).unwrap();
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ResponseHeader {
                            name: "content-length".to_owned(),
                            value: (body.len() + 1).to_string(),
                        }],
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, &body).await.unwrap();
                let _ = send.finish();
                connection
            }
        });

        assert!(matches!(
            client.pair(&target, operation).await,
            Err(ClientError::InvalidResponse)
        ));

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pairing_response_rejects_invalid_variant_fields() {
        let (server, client, target) = test_endpoints().await;
        let operation = PairingOperation::Status(PairingStatusRequest {
            request_id: "pairing-invalid".to_owned(),
            poll_token: Secret32::from_bytes([14; 32]),
        });
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                let body = serde_json::to_vec(&PairingReply::Pending {
                    phrase: "invalid\u{7f}phrase".to_owned(),
                    expires_at_ms: 10,
                })
                .unwrap();
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: vec![ResponseHeader {
                            name: "content-length".to_owned(),
                            value: body.len().to_string(),
                        }],
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, &body).await.unwrap();
                send.finish().unwrap();
                connection
            }
        });

        assert!(matches!(
            client.pair(&target, operation).await,
            Err(ClientError::InvalidResponse)
        ));

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn pairing_identity_mismatch_response_is_typed() {
        let (server, client, target) = test_endpoints().await;
        let operation = PairingOperation::Status(PairingStatusRequest {
            request_id: "pairing-1".to_owned(),
            poll_token: Secret32::from_bytes([17; 32]),
        });
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(matches!(&head, RequestHead::Pairing { .. }));
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 409,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_body(&mut send, br#"{"error":{"code":"identityMismatch"}}"#)
                    .await
                    .unwrap();
                let _ = send.finish();
                connection
            }
        });

        let error = match client.pair(&target, operation).await {
            Ok(_) => panic!("identity mismatch unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(error, ClientError::IdentityMismatch);

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn event_stream_reads_bounded_records() {
        let (server, client, target) = test_endpoints().await;
        let event = br#"{"type":"snapshotRequired","cursor":42}"#;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(matches!(
                    &head,
                    RequestHead::Events { path, .. } if path == "/api/v1/events"
                ));
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
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
                connection
            }
        });

        let mut events = client.events(&target).await.unwrap();
        assert_eq!(events.next().await.unwrap(), event);
        drop(events);

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn event_heartbeats_keep_quiet_streams_alive_without_becoming_events() {
        let (server, client, target) = test_endpoints().await;
        let event = br#"{"type":"snapshotRequired","cursor":43}"#;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                for _ in 0..6 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    write_record(&mut send, EVENT_HEARTBEAT_RECORD, MAX_EVENT_BYTES)
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                write_record(&mut send, event, MAX_EVENT_BYTES)
                    .await
                    .unwrap();
                connection
            }
        });

        let mut events = client.events(&target).await.unwrap();
        let idle_timeout = Duration::from_millis(250);
        let started = tokio::time::Instant::now();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                events.next_with_idle_timeout(idle_timeout),
            )
            .await
            .unwrap()
            .unwrap(),
            event,
        );
        assert!(started.elapsed() > idle_timeout);
        drop(events);

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn dropping_response_body_cancels_remote_send() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let head: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                write_head(
                    &mut send,
                    &ResponseHead {
                        protocol: PROTOCOL_VERSION,
                        request_id: head.request_id().to_owned(),
                        status: 200,
                        headers: Vec::new(),
                    },
                )
                .await
                .unwrap();
                write_chunk(&mut send, b"partial response").await.unwrap();
                let reset = send.stopped().await.unwrap().unwrap();
                (connection, reset)
            }
        });

        let response = client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
            .unwrap();
        drop(response);

        let (_, reset) = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reset.into_inner() as u32, RESET_CANCELLED);
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn stream_reset_surfaces_revocation() {
        let (server, client, target) = test_endpoints().await;
        let server_task = tokio::spawn({
            let server = server.clone();
            async move {
                let connection = server.accept().await.unwrap().await.unwrap();
                let (mut send, mut recv) = connection.accept_bi().await.unwrap();
                let _: RequestHead = read_head(&mut recv).await.unwrap();
                assert!(read_body(&mut recv, 0).await.unwrap().is_empty());
                send.reset(RESET_REVOKED.into()).unwrap();
                recv.stop(RESET_REVOKED.into()).unwrap();
                connection
            }
        });

        let error = match client
            .http(
                &target,
                HttpMethod::Get,
                "/api/v1/bootstrap".to_owned(),
                None,
                Vec::new(),
            )
            .await
        {
            Ok(_) => panic!("revoked stream unexpectedly returned a response"),
            Err(error) => error,
        };
        assert_eq!(error, ClientError::Revoked);

        server_task.await.unwrap();
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn invalidation_does_not_wait_for_connection_attempt_serialization() {
        let (server, client, target) = test_endpoints().await;
        let accepted = tokio::spawn({
            let server = server.clone();
            async move { server.accept().await.unwrap().await.unwrap() }
        });
        let cached = client.connection(&target).await.unwrap();
        let server_connection = accepted.await.unwrap();
        let connect_gate = client.connect_gate.lock().await;

        tokio::time::timeout(Duration::from_millis(200), client.invalidate_connection())
            .await
            .expect("connection invalidation waited for the connect gate");
        assert!(client.connection.lock().await.cached.is_none());
        tokio::time::timeout(Duration::from_secs(1), async {
            while cached.close_reason().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        drop(connect_gate);
        drop(server_connection);
        client.close().await;
        server.close().await;
    }

    #[tokio::test]
    async fn client_transport_accepts_no_remote_streams() {
        let (server, client, target) = test_endpoints().await;
        let accepted = tokio::spawn({
            let server = server.clone();
            async move { server.accept().await.unwrap().await.unwrap() }
        });
        let _client_connection = client.connection(&target).await.unwrap();
        let server_connection = accepted.await.unwrap();

        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(200), server_connection.open_bi()).await,
            Ok(Ok(_))
        ));
        assert!(!matches!(
            tokio::time::timeout(Duration::from_millis(200), server_connection.open_uni()).await,
            Ok(Ok(_))
        ));

        server_connection.close(RESET_CANCELLED.into(), b"test complete");
        client.close().await;
        server.close().await;
    }

    async fn test_endpoints() -> (Endpoint, RemoteClient, HostTarget) {
        let server = Endpoint::empty_builder(RelayMode::Disabled)
            .alpns(vec![COMPANION_ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let target = HostTarget {
            host_id: "host-1".to_owned(),
            host_endpoint_id: server.id().to_string(),
            relay_urls: vec![iroh::defaults::prod::default_na_east_relay()
                .url
                .to_string()],
            direct_addresses: server.addr().ip_addrs().map(ToString::to_string).collect(),
        };
        assert!(!target.direct_addresses.is_empty());
        let endpoint = Endpoint::empty_builder(RelayMode::Disabled)
            .transport_config(client_transport_config())
            .bind()
            .await
            .unwrap();
        (server, RemoteClient::for_test(endpoint), target)
    }
}
