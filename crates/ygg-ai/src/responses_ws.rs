//! Cached OpenAI Responses WebSocket transport.
//!
//! The durable session remains the recovery source of truth. This module only
//! keeps a best-effort live cursor for normal turns: when the current request
//! is a strict extension of the last request plus its terminal output, the
//! wire payload carries `previous_response_id` and the new input suffix. Any
//! mismatch sends the full local replay instead.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use url::Url;

use crate::error::{AiError, ConfigError, TransportError, TransportPhase};

const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";
const EVENT_CHANNEL_CAPACITY: usize = 64;
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONNECTION_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

fn transport_error(phase: TransportPhase, message: impl Into<String>) -> AiError {
    AiError::Transport(TransportError {
        phase,
        timeout: false,
        message: message.into(),
    })
}

fn websocket_url(mut url: Url) -> Result<Url, AiError> {
    match url.scheme() {
        "http" => url
            .set_scheme("ws")
            .map_err(|_| ConfigError::Parse("could not convert Responses URL to ws".into()))?,
        "https" => url
            .set_scheme("wss")
            .map_err(|_| ConfigError::Parse("could not convert Responses URL to wss".into()))?,
        "ws" | "wss" => {}
        scheme => {
            return Err(ConfigError::Parse(format!(
                "unsupported Responses WebSocket URL scheme {scheme:?}"
            ))
            .into());
        }
    }
    Ok(url)
}

fn connect_request(
    url: Url,
    headers: &http::HeaderMap,
) -> Result<tungstenite::http::Request<()>, AiError> {
    let mut request = url.as_str().into_client_request().map_err(|error| {
        transport_error(
            TransportPhase::Connect,
            format!("websocket request: {error}"),
        )
    })?;
    for (name, value) in headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    Ok(request)
}

fn without_continuation_fields(body: &Value) -> Option<Value> {
    let Value::Object(object) = body else {
        return None;
    };
    let mut fixed = object.clone();
    fixed.remove("input");
    fixed.remove("previous_response_id");
    fixed.remove("generate");
    Some(Value::Object(fixed))
}

fn is_prefix<T: PartialEq>(prefix: &[T], values: &[T]) -> bool {
    values.len() >= prefix.len() && values[..prefix.len()] == *prefix
}

#[derive(Clone)]
struct Continuation {
    fixed_body: Value,
    request_input: Vec<Value>,
    response_output: Vec<Value>,
    response_id: String,
}

fn incremental_body(body: &Value, continuation: Option<&Continuation>) -> (Value, bool) {
    let Some(continuation) = continuation else {
        return (body.clone(), false);
    };
    let Some(Value::Array(input)) = body.get("input") else {
        return (body.clone(), false);
    };
    if body.get("previous_response_id").is_some()
        || without_continuation_fields(body) != Some(continuation.fixed_body.clone())
    {
        return (body.clone(), false);
    }

    let baseline_len = continuation
        .request_input
        .len()
        .saturating_add(continuation.response_output.len());
    if input.len() < baseline_len
        || !is_prefix(&continuation.request_input, input)
        || !is_prefix(
            &continuation.response_output,
            &input[continuation.request_input.len()..],
        )
    {
        return (body.clone(), false);
    }

    let mut incremental = body.clone();
    let Some(object) = incremental.as_object_mut() else {
        return (body.clone(), false);
    };
    object.insert(
        "previous_response_id".to_owned(),
        Value::String(continuation.response_id.clone()),
    );
    object.insert(
        "input".to_owned(),
        Value::Array(input[baseline_len..].to_vec()),
    );
    (incremental, true)
}

fn terminal_kind(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str).filter(|kind| {
        matches!(
            *kind,
            "response.completed" | "response.incomplete" | "response.failed" | "response.cancelled"
        )
    })
}

fn update_continuation(full_body: &Value, value: &Value, continuation: &mut Option<Continuation>) {
    let Some(kind) = terminal_kind(value) else {
        return;
    };
    if kind == "response.failed" || kind == "response.cancelled" {
        *continuation = None;
        return;
    }
    let Some(response) = value.get("response") else {
        *continuation = None;
        return;
    };
    let Some(response_id) = response.get("id").and_then(Value::as_str) else {
        *continuation = None;
        return;
    };
    let Some(fixed_body) = without_continuation_fields(full_body) else {
        *continuation = None;
        return;
    };
    let request_input = full_body
        .get("input")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let response_output = response
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    *continuation = Some(Continuation {
        fixed_body,
        request_input,
        response_output,
        response_id: response_id.to_owned(),
    });
}

struct RequestCommand {
    body: Value,
    reply: mpsc::Sender<Result<Value, AiError>>,
    started: oneshot::Sender<Result<(), String>>,
}

#[derive(Clone)]
struct Connection {
    sender: mpsc::Sender<RequestCommand>,
    alive: Arc<AtomicBool>,
}

#[derive(Default)]
struct PoolState {
    sessions: HashMap<String, Connection>,
    disabled: HashSet<String>,
}

/// A process-local pool of session-affine Responses WebSockets.
#[derive(Clone, Default)]
pub(crate) struct ResponsesWsPool {
    state: Arc<Mutex<PoolState>>,
}

impl ResponsesWsPool {
    async fn connect(
        &self,
        key: Option<&str>,
        url: Url,
        headers: http::HeaderMap,
    ) -> Result<Connection, AiError> {
        if let Some(key) = key {
            let mut state = self.state.lock().await;
            if state.disabled.contains(key) {
                return Err(transport_error(
                    TransportPhase::Connect,
                    "Responses WebSocket disabled after an earlier failure",
                ));
            }
            if let Some(connection) = state.sessions.get(key) {
                if connection.alive.load(Ordering::Acquire) {
                    return Ok(connection.clone());
                }
            }
            state.sessions.remove(key);
        }

        // The handshake is deliberately outside the pool lock. Concurrent
        // opens are reconciled below so one slow endpoint cannot block every
        // cached session.
        let url = websocket_url(url)?;
        let request = connect_request(url, &headers)?;
        let (socket, _) = match connect_async(request).await {
            Ok(connected) => connected,
            Err(error) => {
                let error = transport_error(
                    TransportPhase::Connect,
                    format!("Responses WebSocket connect: {error}"),
                );
                if let Some(key) = key {
                    let mut state = self.state.lock().await;
                    if let Some(connection) = state.sessions.get(key) {
                        if connection.alive.load(Ordering::Acquire) {
                            return Ok(connection.clone());
                        }
                    }
                    state.sessions.remove(key);
                    state.disabled.insert(key.to_owned());
                }
                return Err(error);
            }
        };

        let (sender, receiver) = mpsc::channel(4);
        let alive = Arc::new(AtomicBool::new(true));
        let connection = Connection {
            sender,
            alive: Arc::clone(&alive),
        };

        if let Some(key) = key {
            enum Registration {
                Installed,
                Existing(Connection),
                Disabled,
            }

            let registration = {
                let mut state = self.state.lock().await;
                if state.disabled.contains(key) {
                    Registration::Disabled
                } else if let Some(existing) = state
                    .sessions
                    .get(key)
                    .filter(|existing| existing.alive.load(Ordering::Acquire))
                {
                    Registration::Existing(existing.clone())
                } else {
                    state.sessions.remove(key);
                    state.sessions.insert(key.to_owned(), connection.clone());
                    Registration::Installed
                }
            };

            match registration {
                Registration::Installed => {
                    tokio::spawn(run_connection(
                        socket,
                        receiver,
                        alive,
                        Some(key.to_owned()),
                        Arc::downgrade(&self.state),
                        CONNECTION_IDLE_TIMEOUT,
                    ));
                    Ok(connection)
                }
                Registration::Existing(existing) => {
                    // Both handshakes raced for the same key. The map's current
                    // live connection wins; close the unobserved socket rather
                    // than leaving a detached actor behind.
                    connection.alive.store(false, Ordering::Release);
                    drop(receiver);
                    retire_socket(socket);
                    Ok(existing)
                }
                Registration::Disabled => {
                    connection.alive.store(false, Ordering::Release);
                    drop(receiver);
                    retire_socket(socket);
                    Err(transport_error(
                        TransportPhase::Connect,
                        "Responses WebSocket disabled after an earlier failure",
                    ))
                }
            }
        } else {
            tokio::spawn(run_connection(
                socket,
                receiver,
                alive,
                None,
                Arc::downgrade(&self.state),
                CONNECTION_IDLE_TIMEOUT,
            ));
            Ok(connection)
        }
    }

    async fn remove(&self, key: &str, connection: &Connection) {
        let mut state = self.state.lock().await;
        if state
            .sessions
            .get(key)
            .is_some_and(|current| current.sender.same_channel(&connection.sender))
        {
            state.sessions.remove(key);
        }
    }

    /// Sends one full request to a cached or one-shot connection and returns
    /// the raw JSON event stream. The caller owns protocol decoding and stream
    /// deadlines.
    pub(crate) async fn request(
        &self,
        key: Option<&str>,
        url: Url,
        headers: http::HeaderMap,
        body: Value,
    ) -> Result<mpsc::Receiver<Result<Value, AiError>>, AiError> {
        let connection = self.connect(key, url, headers).await?;
        let (reply, events) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let (started, started_result) = oneshot::channel();
        let command = RequestCommand {
            body,
            reply,
            started,
        };
        if connection.sender.send(command).await.is_err() {
            connection.alive.store(false, Ordering::Release);
            if let Some(key) = key {
                self.remove(key, &connection).await;
            }
            return Err(transport_error(
                TransportPhase::Connect,
                "Responses WebSocket connection closed before request send",
            ));
        }
        match started_result.await {
            Ok(Ok(())) => Ok(events),
            Ok(Err(message)) => {
                connection.alive.store(false, Ordering::Release);
                if let Some(key) = key {
                    self.remove(key, &connection).await;
                }
                Err(transport_error(TransportPhase::ResponseHeaders, message))
            }
            Err(_) => {
                connection.alive.store(false, Ordering::Release);
                if let Some(key) = key {
                    self.remove(key, &connection).await;
                }
                Err(transport_error(
                    TransportPhase::ResponseHeaders,
                    "Responses WebSocket actor stopped before request start was acknowledged",
                ))
            }
        }
    }

    /// Performs a best-effort `generate=false` request used to establish a
    /// provider-side continuation while a caller is still preparing a turn.
    pub(crate) async fn prewarm(
        &self,
        key: &str,
        url: Url,
        headers: http::HeaderMap,
        mut body: Value,
    ) -> Result<(), AiError> {
        let Some(object) = body.as_object_mut() else {
            return Err(crate::error::DecodeError::Json(
                "Responses request body is not an object".to_owned(),
            )
            .into());
        };
        object.insert("generate".to_owned(), Value::Bool(false));
        let mut events = self.request(Some(key), url, headers, body).await?;
        while let Some(event) = events.recv().await {
            let event = event?;
            if terminal_kind(&event).is_some() {
                return Ok(());
            }
        }
        Err(transport_error(
            TransportPhase::Body,
            "Responses WebSocket prewarm ended before completion",
        ))
    }

    /// Header value required by the current Codex Responses WebSocket route.
    pub(crate) fn beta_header_value() -> &'static str {
        RESPONSES_WEBSOCKETS_BETA
    }
}

fn connection_refresh_error(value: &Value) -> bool {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"))
        .or_else(|| value.get("error"));
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .or_else(|| value.get("code").and_then(Value::as_str));
    let Some(code) = code else {
        return false;
    };
    let code = code.to_ascii_lowercase();
    code == "websocket_connection_limit_reached"
        || (code.contains("websocket") && code.contains("connection") && code.contains("limit"))
}

async fn disable_key(state: &Weak<Mutex<PoolState>>, key: Option<&str>) {
    let (Some(state), Some(key)) = (state.upgrade(), key) else {
        return;
    };
    state.lock().await.disabled.insert(key.to_owned());
}

async fn remove_connection(
    state: &Weak<Mutex<PoolState>>,
    key: Option<&str>,
    alive: &Arc<AtomicBool>,
) {
    let (Some(state), Some(key)) = (state.upgrade(), key) else {
        return;
    };
    let mut state = state.lock().await;
    if state
        .sessions
        .get(key)
        .is_some_and(|connection| Arc::ptr_eq(&connection.alive, alive))
    {
        state.sessions.remove(key);
    }
}

async fn close_socket<S>(socket: &mut S)
where
    S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let _ = tokio::time::timeout(CONNECTION_CLOSE_TIMEOUT, socket.close()).await;
}

fn retire_socket<S>(mut socket: S)
where
    S: futures_util::Sink<Message, Error = tungstenite::Error> + Send + Unpin + 'static,
{
    tokio::spawn(async move {
        close_socket(&mut socket).await;
    });
}

async fn run_connection<S>(
    mut socket: S,
    mut commands: mpsc::Receiver<RequestCommand>,
    alive: Arc<AtomicBool>,
    key: Option<String>,
    state: Weak<Mutex<PoolState>>,
    idle_timeout: Duration,
) where
    S: futures_core::Stream<Item = Result<Message, tungstenite::Error>>
        + futures_util::Sink<Message, Error = tungstenite::Error>
        + Unpin,
{
    let mut continuation = None;
    'actor: loop {
        // Poll the socket even without an active request so peer closes and
        // control frames are handled promptly. Control traffic does not extend
        // the request-idle lifetime.
        let idle = tokio::time::sleep(idle_timeout);
        tokio::pin!(idle);
        let command = loop {
            if idle.is_elapsed() {
                break 'actor;
            }
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        break 'actor;
                    };
                    break command;
                }
                message = socket.next() => {
                    match message {
                        Some(Ok(Message::Ping(payload))) => {
                            if !matches!(
                                tokio::time::timeout(
                                    CONNECTION_CLOSE_TIMEOUT,
                                    socket.send(Message::Pong(payload)),
                                )
                                .await,
                                Ok(Ok(()))
                            ) {
                                break 'actor;
                            }
                        }
                        Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                            break 'actor;
                        }
                        Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                            // A data event outside a request cannot safely be
                            // associated with a later generation.
                            alive.store(false, Ordering::Release);
                            disable_key(&state, key.as_deref()).await;
                            break 'actor;
                        }
                    }
                }
                _ = &mut idle => break 'actor,
            }
        };

        // A request future can be cancelled while this command waits behind an
        // active turn. Do not send an orphaned generation after its receiver is
        // already gone.
        if command.reply.is_closed() {
            continue;
        }
        let (wire_body, _) = incremental_body(&command.body, continuation.as_ref());
        let Value::Object(mut payload) = wire_body else {
            let _ = command
                .started
                .send(Err("Responses WebSocket body is not an object".to_owned()));
            let _ = command
                .reply
                .send(Err(transport_error(
                    TransportPhase::ResponseHeaders,
                    "Responses WebSocket body is not an object",
                )))
                .await;
            break 'actor;
        };
        payload.insert(
            "type".to_owned(),
            Value::String("response.create".to_owned()),
        );
        let text = match serde_json::to_string(&Value::Object(payload)) {
            Ok(text) => text,
            Err(error) => {
                let message = format!("Responses WebSocket request encoding: {error}");
                let _ = command.started.send(Err(message.clone()));
                let _ = command
                    .reply
                    .send(Err(transport_error(
                        TransportPhase::ResponseHeaders,
                        message,
                    )))
                    .await;
                break 'actor;
            }
        };
        if command.reply.is_closed() {
            continue;
        }
        let send_result = tokio::select! {
            biased;
            _ = command.reply.closed() => {
                // Cancelling a send can leave the WebSocket sink in an
                // indeterminate state. Discard it rather than reusing a socket
                // that may have transmitted part or all of the frame.
                alive.store(false, Ordering::Release);
                disable_key(&state, key.as_deref()).await;
                break 'actor;
            }
            result = socket.send(Message::Text(text.into())) => result,
        };
        if let Err(error) = send_result {
            let message = format!("Responses WebSocket request send: {error}");
            let _ = command.started.send(Err(message.clone()));
            alive.store(false, Ordering::Release);
            disable_key(&state, key.as_deref()).await;
            break 'actor;
        }
        let _ = command.started.send(Ok(()));

        let mut terminal = false;
        loop {
            let message = tokio::select! {
                biased;
                _ = command.reply.closed() => {
                    // The consumer dropped or timed out. Stop the provider-side
                    // stream instead of leaving this actor and socket blocked
                    // forever waiting for another frame.
                    alive.store(false, Ordering::Release);
                    disable_key(&state, key.as_deref()).await;
                    break 'actor;
                }
                message = socket.next() => message,
            };
            let Some(message) = message else {
                break;
            };
            let message = match message {
                Ok(message) => message,
                Err(error) => {
                    let _ = command
                        .reply
                        .send(Err(transport_error(
                            TransportPhase::Body,
                            format!("Responses WebSocket read: {error}"),
                        )))
                        .await;
                    alive.store(false, Ordering::Release);
                    disable_key(&state, key.as_deref()).await;
                    break 'actor;
                }
            };
            match message {
                Message::Text(text) => {
                    let value = match serde_json::from_str::<Value>(text.as_ref()) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = command
                                .reply
                                .send(Err(AiError::Decode(crate::error::DecodeError::Json(
                                    format!("invalid Responses WebSocket event: {error}"),
                                ))))
                                .await;
                            alive.store(false, Ordering::Release);
                            disable_key(&state, key.as_deref()).await;
                            break 'actor;
                        }
                    };
                    let is_terminal = terminal_kind(&value).is_some();
                    if is_terminal {
                        update_continuation(&command.body, &value, &mut continuation);
                    }
                    let connection_refresh = connection_refresh_error(&value);
                    if connection_refresh {
                        // Retire the poisoned socket before publishing the
                        // provider error. An immediate agent retry must observe
                        // the disabled key and take the safe HTTP fallback,
                        // never race another command onto this actor.
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                    }
                    if command.reply.send(Ok(value)).await.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        break 'actor;
                    }
                    if connection_refresh {
                        break 'actor;
                    }
                    if is_terminal {
                        terminal = true;
                        break;
                    }
                }
                Message::Binary(bytes) => {
                    let value = match serde_json::from_slice::<Value>(&bytes) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = command
                                .reply
                                .send(Err(AiError::Decode(crate::error::DecodeError::Json(
                                    format!("invalid Responses WebSocket event: {error}"),
                                ))))
                                .await;
                            alive.store(false, Ordering::Release);
                            disable_key(&state, key.as_deref()).await;
                            break 'actor;
                        }
                    };
                    let is_terminal = terminal_kind(&value).is_some();
                    if is_terminal {
                        update_continuation(&command.body, &value, &mut continuation);
                    }
                    let connection_refresh = connection_refresh_error(&value);
                    if connection_refresh {
                        // Retire the poisoned socket before publishing the
                        // provider error. An immediate agent retry must observe
                        // the disabled key and take the safe HTTP fallback,
                        // never race another command onto this actor.
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                    }
                    if command.reply.send(Ok(value)).await.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        break 'actor;
                    }
                    if connection_refresh {
                        break 'actor;
                    }
                    if is_terminal {
                        terminal = true;
                        break;
                    }
                }
                Message::Ping(payload) => {
                    let pong_result = tokio::select! {
                        biased;
                        _ = command.reply.closed() => {
                            alive.store(false, Ordering::Release);
                            disable_key(&state, key.as_deref()).await;
                            break 'actor;
                        }
                        result = socket.send(Message::Pong(payload)) => result,
                    };
                    if pong_result.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        break 'actor;
                    }
                }
                Message::Close(_) => {
                    let _ = command
                        .reply
                        .send(Err(transport_error(
                            TransportPhase::Body,
                            "Responses WebSocket closed before completion",
                        )))
                        .await;
                    alive.store(false, Ordering::Release);
                    disable_key(&state, key.as_deref()).await;
                    break 'actor;
                }
                Message::Pong(_) | Message::Frame(_) => {}
            }
        }
        if !terminal {
            let _ = command
                .reply
                .send(Err(transport_error(
                    TransportPhase::Body,
                    "Responses WebSocket ended before completion",
                )))
                .await;
            alive.store(false, Ordering::Release);
            disable_key(&state, key.as_deref()).await;
            break 'actor;
        }
    }

    alive.store(false, Ordering::Release);
    remove_connection(&state, key.as_deref(), &alive).await;
    close_socket(&mut socket).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::{accept_async, MaybeTlsStream, WebSocketStream};

    type ClientSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;
    type ServerSocket = WebSocketStream<TcpStream>;

    fn item(id: &str) -> Value {
        serde_json::json!({"type": "message", "id": id})
    }

    async fn websocket_pair() -> (ClientSocket, ServerSocket) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_async(stream).await.unwrap()
        });
        let (client, _) = connect_async(format!("ws://{address}/")).await.unwrap();
        (client, server.await.unwrap())
    }

    async fn spawn_test_actor(
        socket: ClientSocket,
        state: &Arc<Mutex<PoolState>>,
        key: &str,
        idle_timeout: Duration,
    ) -> (Connection, JoinHandle<()>) {
        let (sender, receiver) = mpsc::channel(4);
        let alive = Arc::new(AtomicBool::new(true));
        let connection = Connection {
            sender,
            alive: Arc::clone(&alive),
        };
        state
            .lock()
            .await
            .sessions
            .insert(key.to_owned(), connection.clone());
        let actor = tokio::spawn(run_connection(
            socket,
            receiver,
            alive,
            Some(key.to_owned()),
            Arc::downgrade(state),
            idle_timeout,
        ));
        (connection, actor)
    }

    #[test]
    fn detects_provider_connection_lifetime_errors() {
        assert!(connection_refresh_error(&serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {
                    "code": "websocket_connection_limit_reached",
                    "message": "Create a new websocket connection to continue."
                }
            }
        })));
        assert!(connection_refresh_error(&serde_json::json!({
            "type": "error",
            "code": "websocket_connection_limit_reached",
            "message": "connection limit reached"
        })));
        assert!(connection_refresh_error(&serde_json::json!({
            "type": "error",
            "error": {
                "code": "gateway_websocket_connection_limit",
                "message": "connection limit reached"
            }
        })));
        assert!(!connection_refresh_error(&serde_json::json!({
            "type": "response.failed",
            "response": {
                "error": {"code": "invalid_request", "message": "bad request"}
            }
        })));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_same_key_handshakes_share_one_connection_and_close_the_loser() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (closed, closed_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            // Do not complete either handshake until both clients arrive. This
            // would deadlock if the first handshake held the global pool lock.
            let (first, _) = listener.accept().await.unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let (first, second) = tokio::join!(accept_async(first), accept_async(second));
            let mut first = first.unwrap();
            let mut second = second.unwrap();
            let closed_message = tokio::select! {
                message = first.next() => message,
                message = second.next() => message,
            };
            let _ = closed.send(matches!(closed_message, Some(Ok(Message::Close(_))) | None));
            let _ = release_rx.await;
        });

        let pool = ResponsesWsPool::default();
        let url = Url::parse(&format!("ws://{address}/")).unwrap();
        let first = tokio::spawn({
            let pool = pool.clone();
            let url = url.clone();
            async move {
                pool.connect(Some("shared"), url, http::HeaderMap::new())
                    .await
            }
        });
        let second = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.connect(Some("shared"), url, http::HeaderMap::new())
                    .await
            }
        });
        let (first, second) = tokio::time::timeout(Duration::from_secs(3), async move {
            tokio::join!(first, second)
        })
        .await
        .expect("same-key handshakes must not serialize on the global pool lock");
        let first = first.unwrap().unwrap();
        let second = second.unwrap().unwrap();

        assert!(first.sender.same_channel(&second.sender));
        assert_eq!(pool.state.lock().await.sessions.len(), 1);
        assert!(tokio::time::timeout(Duration::from_secs(1), closed_rx)
            .await
            .expect("racing socket was not retired")
            .unwrap());

        let _ = release.send(());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn idle_actor_detects_peer_close_and_evicts_itself() {
        let (client, mut server) = websocket_pair().await;
        let state = Arc::new(Mutex::new(PoolState::default()));
        let (connection, actor) =
            spawn_test_actor(client, &state, "closed", Duration::from_secs(60)).await;

        server.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .expect("idle actor did not observe peer close")
            .unwrap();

        assert!(!connection.alive.load(Ordering::Acquire));
        let state = state.lock().await;
        assert!(!state.sessions.contains_key("closed"));
        assert!(!state.disabled.contains("closed"));
    }

    #[tokio::test]
    async fn idle_actor_times_out_and_evicts_itself() {
        let (client, mut server) = websocket_pair().await;
        let state = Arc::new(Mutex::new(PoolState::default()));
        let (connection, actor) =
            spawn_test_actor(client, &state, "idle", Duration::from_millis(20)).await;

        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .expect("idle actor was retained past its timeout")
            .unwrap();

        assert!(!connection.alive.load(Ordering::Acquire));
        assert!(!state.lock().await.sessions.contains_key("idle"));
        let close = tokio::time::timeout(Duration::from_secs(1), server.next())
            .await
            .expect("idle socket was not closed");
        assert!(matches!(close, Some(Ok(Message::Close(_))) | None));
    }

    #[tokio::test]
    async fn idle_actor_does_not_retain_pool_state() {
        let (client, _server) = websocket_pair().await;
        let state = Arc::new(Mutex::new(PoolState::default()));
        let (connection, actor) =
            spawn_test_actor(client, &state, "cycle", Duration::from_secs(60)).await;
        let weak_state = Arc::downgrade(&state);

        drop(connection);
        drop(state);
        assert!(weak_state.upgrade().is_none());
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .expect("actor did not stop after its pool was dropped")
            .unwrap();
    }

    #[tokio::test]
    async fn dropping_an_active_response_retires_the_socket_and_provider_work() {
        let (client, mut server) = websocket_pair().await;
        let state = Arc::new(Mutex::new(PoolState::default()));
        let (connection, actor) =
            spawn_test_actor(client, &state, "cancel", Duration::from_secs(60)).await;
        let (reply, events) = mpsc::channel(1);
        let (started, started_rx) = oneshot::channel();
        connection
            .sender
            .send(RequestCommand {
                body: serde_json::json!({"model": "gpt", "input": []}),
                reply,
                started,
            })
            .await
            .unwrap();

        assert!(matches!(server.next().await, Some(Ok(Message::Text(_)))));
        started_rx.await.unwrap().unwrap();
        drop(events);

        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .expect("cancelled request left its WebSocket actor running")
            .unwrap();
        let close = tokio::time::timeout(Duration::from_secs(1), server.next())
            .await
            .expect("cancelled request did not close the provider socket");
        assert!(matches!(close, Some(Ok(Message::Close(_))) | None));
        assert!(!connection.alive.load(Ordering::Acquire));
        let state = state.lock().await;
        assert!(!state.sessions.contains_key("cancel"));
        assert!(state.disabled.contains("cancel"));
    }

    #[test]
    fn continuation_replaces_only_the_new_input_suffix() {
        let first = serde_json::json!({
            "model": "gpt",
            "input": [item("user"), item("tool")],
            "tools": []
        });
        let output = item("assistant");
        let continuation = Continuation {
            fixed_body: without_continuation_fields(&first).unwrap(),
            request_input: first["input"].as_array().unwrap().clone(),
            response_output: vec![output.clone()],
            response_id: "resp_1".to_owned(),
        };
        let next = serde_json::json!({
            "model": "gpt",
            "input": [item("user"), item("tool"), output, item("next")],
            "tools": []
        });
        let (wire, incremental) = incremental_body(&next, Some(&continuation));
        assert!(incremental);
        assert_eq!(wire["previous_response_id"], "resp_1");
        assert_eq!(wire["input"], serde_json::json!([item("next")]));
    }

    #[test]
    fn continuation_falls_back_on_branch_or_shape_change() {
        let first = serde_json::json!({"model": "gpt", "input": [item("user")]});
        let continuation = Continuation {
            fixed_body: without_continuation_fields(&first).unwrap(),
            request_input: first["input"].as_array().unwrap().clone(),
            response_output: vec![item("assistant")],
            response_id: "resp_1".to_owned(),
        };
        let branch = serde_json::json!({"model": "gpt", "input": [item("other")]});
        let (wire, incremental) = incremental_body(&branch, Some(&continuation));
        assert!(!incremental);
        assert_eq!(wire, branch);
    }
}
