//! Cached OpenAI Responses WebSocket transport.
//!
//! The durable session remains the recovery source of truth. This module only
//! keeps a best-effort live cursor for normal turns: when the current request
//! is a strict extension of the last request plus its terminal output, the
//! wire payload carries `previous_response_id` and the new input suffix. Any
//! mismatch sends the full local replay instead.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{connect_async, tungstenite::client::IntoClientRequest};
use url::Url;

use crate::error::{AiError, ConfigError, TransportError, TransportPhase};

const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";
const EVENT_CHANNEL_CAPACITY: usize = 64;

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
            // Holding this small pool lock while establishing one connection
            // prevents duplicate sockets for the same session. Requests for a
            // different session still only wait for the handshake, not a file or
            // provider operation.
            let connection =
                Self::open_connection(Some(key), url, headers, Arc::clone(&self.state)).await?;
            state.sessions.insert(key.to_owned(), connection.clone());
            Ok(connection)
        } else {
            Self::open_connection(None, url, headers, Arc::clone(&self.state)).await
        }
    }

    async fn open_connection(
        key: Option<&str>,
        url: Url,
        headers: http::HeaderMap,
        state: Arc<Mutex<PoolState>>,
    ) -> Result<Connection, AiError> {
        let url = websocket_url(url)?;
        let request = connect_request(url, &headers)?;
        let (socket, _) = connect_async(request).await.map_err(|error| {
            if let Some(key) = key {
                let state = Arc::clone(&state);
                let key = key.to_owned();
                tokio::spawn(async move {
                    state.lock().await.disabled.insert(key);
                });
            }
            transport_error(
                TransportPhase::Connect,
                format!("Responses WebSocket connect: {error}"),
            )
        })?;

        let (sender, receiver) = mpsc::channel(4);
        let alive = Arc::new(AtomicBool::new(true));
        let connection = Connection {
            sender,
            alive: Arc::clone(&alive),
        };
        tokio::spawn(run_connection(
            socket,
            receiver,
            alive,
            key.map(str::to_owned),
            state,
        ));
        Ok(connection)
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
                TransportPhase::ResponseHeaders,
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
                    "Responses WebSocket actor stopped before request send",
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

async fn disable_key(state: &Arc<Mutex<PoolState>>, key: Option<&str>) {
    if let Some(key) = key {
        state.lock().await.disabled.insert(key.to_owned());
    }
}

async fn run_connection<S>(
    mut socket: S,
    mut commands: mpsc::Receiver<RequestCommand>,
    alive: Arc<AtomicBool>,
    key: Option<String>,
    state: Arc<Mutex<PoolState>>,
) where
    S: futures_core::Stream<Item = Result<Message, tungstenite::Error>>
        + futures_util::Sink<Message, Error = tungstenite::Error>
        + Unpin,
{
    let mut continuation = None;
    while let Some(command) = commands.recv().await {
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
            break;
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
                break;
            }
        };
        if let Err(error) = socket.send(Message::Text(text.into())).await {
            let message = format!("Responses WebSocket request send: {error}");
            let _ = command.started.send(Err(message.clone()));
            alive.store(false, Ordering::Release);
            disable_key(&state, key.as_deref()).await;
            break;
        }
        let _ = command.started.send(Ok(()));

        let mut terminal = false;
        while let Some(message) = socket.next().await {
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
                    return;
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
                            return;
                        }
                    };
                    let is_terminal = terminal_kind(&value).is_some();
                    if is_terminal {
                        update_continuation(&command.body, &value, &mut continuation);
                    }
                    if command.reply.send(Ok(value)).await.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        let _ = socket.close().await;
                        return;
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
                            return;
                        }
                    };
                    let is_terminal = terminal_kind(&value).is_some();
                    if is_terminal {
                        update_continuation(&command.body, &value, &mut continuation);
                    }
                    if command.reply.send(Ok(value)).await.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        let _ = socket.close().await;
                        return;
                    }
                    if is_terminal {
                        terminal = true;
                        break;
                    }
                }
                Message::Ping(payload) => {
                    if socket.send(Message::Pong(payload)).await.is_err() {
                        alive.store(false, Ordering::Release);
                        disable_key(&state, key.as_deref()).await;
                        return;
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
                    return;
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
            return;
        }
    }
    alive.store(false, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Value {
        serde_json::json!({"type": "message", "id": id})
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
