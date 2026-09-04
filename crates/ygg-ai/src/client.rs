//! The `AiClient`, the resolved `Model` handle, and request dispatch.

use async_stream::try_stream;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::error::Error as _;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::auth::CredentialRedactor;
use crate::catalog::Model;
use crate::error::{
    AiError, DecodeError, HttpError, ProviderError, StreamProgress, StreamProtocolError,
    TransportError, TransportPhase,
};
use crate::host_transport::{HostStreamModel, HostStreamTransport};
use crate::responses_ws::{ResponsesWsLiveness, ResponsesWsPool};
use crate::stream::{
    ProviderLifecycle, ProviderLifecycleState, ResponseBuilder, ResponseStream, StreamEvent,
};
use crate::types::{EndpointId, Protocol, Request, Response, ToolDef};
use crate::{ResponsesCompactRequest, ResponsesCompactResponse};

/// Hard cap on a buffered non-streaming response body before JSON decode
/// (design §20). Crossing it is a [`DecodeError::BodyTooLarge`].
const MAX_COMPLETED_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Enough of an unexpected successful-status body to decode a structured
/// provider error without buffering an unbounded non-SSE response.
const MAX_SUCCESS_ERROR_BODY_BYTES: usize = 64 * 1024;
/// Bound DNS/TCP/TLS establishment independently from a provider's header
/// timeout. Without this, a dead route can consume the full endpoint timeout on
/// every retry before the UI receives an error.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to wait for the first response-body chunk after headers. A
/// provider may have accepted the request and still be processing a very large
/// prompt or loading a local model.
const DEFAULT_STREAM_INITIAL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Maximum silence allowed between SSE body chunks after the response starts.
/// Slow local servers can pause for several minutes between reasoning/output
/// chunks without being dead, especially while paging or swapping a large
/// model.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Absolute deadline for one response-body read. This remains finite so a
/// wedged provider is eventually surfaced, while leaving room for large
/// compaction/reasoning turns and rate-limited gateways.
const DEFAULT_STREAM_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// Error bodies are optional diagnostics after the status and retry metadata
/// are already known. Never let a slow snippet inherit generation-scale waits.
const MAX_ERROR_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ERROR_BODY_DEADLINE: Duration = Duration::from_secs(5);
/// Compression level used by the ChatGPT Codex SSE endpoint and the official
/// Codex-compatible client. Level 3 is fast enough to keep request preparation
/// cheap while substantially shrinking replayed tool history.
const CODEX_REQUEST_ZSTD_LEVEL: i32 = 3;

fn truncate_transport_message(message: &mut String, max_bytes: usize) {
    if message.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
}

const MAX_PROVIDER_DIAGNOSTIC_BYTES: usize = 4096;
const MAX_DIAGNOSTIC_METADATA_BYTES: usize = 512;
/// A stream can surface only a finite amount of advisory endpoint telemetry.
const MAX_PROVIDER_LIFECYCLE_EVENTS: usize = 64;
/// Lifecycle detail remains brief enough for a status row and cannot retain an
/// endpoint-controlled unbounded string.
const MAX_PROVIDER_LIFECYCLE_DETAIL_BYTES: usize = 160;
const LIFECYCLE_HEADER: &str = "x-ygg-lifecycle";
const LIFECYCLE_REQUEST_VALUE: &str = "1";
const LIFECYCLE_COMMENT_PREFIX: &str = "ygg-lifecycle:";

/// Parses Ygg's explicitly negotiated OpenAI-compatible lifecycle value.
///
/// The wire form is `state` or `state; detail`, where state is one of
/// `queued`, `loading`, or `ready`. Unknown states and malformed namespaces
/// are deliberately ignored: this is advisory telemetry, never assistant text.
fn parse_provider_lifecycle(
    value: &str,
    diagnostic_redactor: &CredentialRedactor,
) -> Option<ProviderLifecycle> {
    let (state, detail) = value
        .split_once(';')
        .map_or((value, None), |(state, detail)| (state, Some(detail)));
    let state = ProviderLifecycleState::from_wire(state)?;
    let detail = detail.and_then(|detail| {
        let detail = detail.trim();
        (!detail.is_empty()).then(|| {
            sanitize_diagnostic(
                diagnostic_redactor,
                detail,
                MAX_PROVIDER_LIFECYCLE_DETAIL_BYTES,
            )
        })
    });
    Some(ProviderLifecycle { state, detail })
}

/// Extracts a lifecycle comment without treating ordinary SSE comments as
/// provider data. The `ygg-lifecycle:` namespace is accepted only after the
/// endpoint explicitly opted in through the request header.
fn lifecycle_from_sse_comment(
    comment: &str,
    diagnostic_redactor: &CredentialRedactor,
) -> Option<ProviderLifecycle> {
    parse_provider_lifecycle(
        comment.strip_prefix(LIFECYCLE_COMMENT_PREFIX)?.trim_start(),
        diagnostic_redactor,
    )
}

fn is_bidi_format_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// Redact request credentials, render control characters inert, and retain a
/// hard post-sanitization byte bound. Provider diagnostics cross a trust
/// boundary: they must be safe to persist or print verbatim.
fn sanitize_diagnostic(redactor: &CredentialRedactor, input: &str, max_bytes: usize) -> String {
    let redacted = redactor.redact(input);
    let mut output = String::with_capacity(redacted.len().min(max_bytes));
    let mut truncated = false;

    for character in redacted.chars() {
        if character.is_control() || is_bidi_format_control(character) {
            let escaped = character.escape_default().to_string();
            if output.len().saturating_add(escaped.len()) > max_bytes {
                truncated = true;
                break;
            }
            output.push_str(&escaped);
        } else {
            if output.len().saturating_add(character.len_utf8()) > max_bytes {
                truncated = true;
                break;
            }
            output.push(character);
        }
    }

    if truncated && max_bytes >= '…'.len_utf8() {
        let limit = max_bytes - '…'.len_utf8();
        while output.len() > limit {
            let _ = output.pop();
        }
        output.push('…');
    }
    output
}

fn sanitize_optional_diagnostic(
    redactor: &CredentialRedactor,
    value: &mut Option<String>,
    max_bytes: usize,
) {
    if let Some(value) = value {
        *value = sanitize_diagnostic(redactor, value, max_bytes);
    }
}

fn sanitize_ai_error(redactor: &CredentialRedactor, mut error: AiError) -> AiError {
    match &mut error {
        AiError::Http(error) => {
            sanitize_optional_diagnostic(
                redactor,
                &mut error.request_id,
                MAX_DIAGNOSTIC_METADATA_BYTES,
            );
            sanitize_optional_diagnostic(
                redactor,
                &mut error.provider_code,
                MAX_DIAGNOSTIC_METADATA_BYTES,
            );
            sanitize_optional_diagnostic(
                redactor,
                &mut error.body_snippet,
                MAX_PROVIDER_DIAGNOSTIC_BYTES,
            );
        }
        AiError::Transport(error) => {
            error.message =
                sanitize_diagnostic(redactor, &error.message, MAX_DIAGNOSTIC_METADATA_BYTES);
        }
        AiError::Provider(error) => {
            sanitize_optional_diagnostic(redactor, &mut error.code, MAX_DIAGNOSTIC_METADATA_BYTES);
            sanitize_optional_diagnostic(redactor, &mut error.kind, MAX_DIAGNOSTIC_METADATA_BYTES);
            error.message =
                sanitize_diagnostic(redactor, &error.message, MAX_PROVIDER_DIAGNOSTIC_BYTES);
            sanitize_optional_diagnostic(
                redactor,
                &mut error.request_id,
                MAX_DIAGNOSTIC_METADATA_BYTES,
            );
        }
        AiError::Decode(
            DecodeError::Json(message) | DecodeError::InvalidProviderField(message),
        )
        | AiError::StreamProtocol(StreamProtocolError::UnexpectedEvent(message)) => {
            *message = sanitize_diagnostic(redactor, message, MAX_PROVIDER_DIAGNOSTIC_BYTES);
        }
        AiError::StreamFailure { inner, .. } => {
            // The progress counters are purely numeric and can never carry
            // provider text; only the wrapped inner error can, so sanitize
            // that in place.
            let drained = std::mem::replace(&mut **inner, AiError::Canceled);
            **inner = sanitize_ai_error(redactor, drained);
        }
        AiError::Config(_)
        | AiError::Auth(_)
        | AiError::Validation(_)
        | AiError::Unsupported(_)
        | AiError::Decode(_)
        | AiError::Pricing(_)
        | AiError::StreamProtocol(_)
        | AiError::Canceled => {}
    }
    error
}

/// Annotate a mid-stream failure with how far the response had progressed.
///
/// Wrapping only happens inside the response-body loop, where the builder is
/// still alive: the raw provider frame/event counts and the retained content
/// bytes are exactly what distinguishes "the provider sent 400 frames and
/// then went silent" from "the provider sent nothing". Pre-stream failures
/// (connection, headers, HTTP status) are left unannotated.
fn annotate_stream_failure(
    inner: AiError,
    builder: &ResponseBuilder,
    first_body_chunk: bool,
    started_at: Instant,
    last_event_at: Option<Instant>,
) -> AiError {
    AiError::StreamFailure {
        inner: Box::new(inner),
        progress: StreamProgress {
            provider_events: builder.provider_event_count,
            decoded_events: builder.event_count,
            content_bytes: builder.aggregate_content_bytes,
            buffered_bytes: builder.buffered_content_bytes,
            first_body_seen: !first_body_chunk,
            elapsed_ms: u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            last_event_ms: last_event_at.map(|event_at| {
                u64::try_from(event_at.duration_since(started_at).as_millis()).unwrap_or(u64::MAX)
            }),
        },
    }
}

fn reqwest_transport_error(
    error: reqwest::Error,
    phase: TransportPhase,
    operation: &str,
) -> AiError {
    let timeout = error.is_timeout();
    let category = if error.is_connect() {
        "connection failed"
    } else if timeout {
        "timed out"
    } else if error.is_body() {
        "body transfer failed"
    } else {
        "transport failed"
    };
    // Reqwest's top-level Display includes the request URL. Walk only its
    // source chain so DNS/TCP/TLS/reset details survive without endpoint paths,
    // queries, or URL credentials. Bound it because third-party TLS/DNS errors
    // are not under Ygg's control.
    let mut details = Vec::new();
    let mut source = error.source();
    while let Some(cause) = source {
        let detail = cause.to_string();
        if !detail.trim().is_empty() && details.last() != Some(&detail) {
            details.push(detail);
        }
        if details.len() == 4 {
            break;
        }
        source = cause.source();
    }
    let mut message = format!("{operation} {category}");
    if !details.is_empty() {
        message.push_str(": ");
        message.push_str(&details.join(": "));
    }
    truncate_transport_message(&mut message, 512);
    AiError::Transport(TransportError {
        phase,
        timeout,
        message,
    })
}

fn request_open_transport_error(error: reqwest::Error, operation: &str) -> AiError {
    let phase = if error.is_connect() {
        TransportPhase::Connect
    } else {
        // Once connection establishment succeeded, request-send failures and
        // response-header failures are ambiguous: the provider may have
        // accepted the POST even though no response was observed locally.
        TransportPhase::ResponseHeaders
    };
    reqwest_transport_error(error, phase, operation)
}

fn json_scalar_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Some OpenAI-compatible servers return a JSON error envelope with HTTP 200
/// for request-validation failures. Detect that envelope before the empty SSE
/// stream is misreported as a missing terminal event.
fn provider_error_from_success_body(body: &[u8]) -> Option<ProviderError> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    let mut message = error
        .get("message")
        .and_then(serde_json::Value::as_str)
        .or_else(|| error.as_str())?
        .to_owned();
    truncate_transport_message(&mut message, 4096);
    let code = error.get("code").and_then(json_scalar_string);
    let kind = error
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let request_id = value
        .get("request_id")
        .or_else(|| error.get("request_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(ProviderError {
        code,
        kind,
        message,
        request_id,
    })
}

/// Apply the request runtime selected by the endpoint declaration.
///
/// Codecs produce canonical bodies. Endpoint declarations independently opt
/// into documented transport behavior, so providers sharing a codec never need
/// a provider-name branch here. Compression failure is only an optimization
/// miss: preserve the valid uncompressed request instead of failing the model
/// turn. The zstd work runs on the blocking thread pool so multi-hundred-KB
/// request bodies never stall the async runtime worker.
async fn prepare_request_body(
    runtime: crate::types::RequestRuntime,
    headers: &mut http::HeaderMap,
    body: bytes::Bytes,
) -> bytes::Bytes {
    if runtime.body_encoding != crate::types::RequestBodyEncoding::Zstd {
        return body;
    }

    let owned = body.clone();
    match tokio::task::spawn_blocking(move || {
        zstd::bulk::compress(owned.as_ref(), CODEX_REQUEST_ZSTD_LEVEL)
    })
    .await
    {
        Ok(Ok(compressed)) => {
            headers.insert(
                http::header::CONTENT_ENCODING,
                http::HeaderValue::from_static("zstd"),
            );
            bytes::Bytes::from(compressed)
        }
        // Compression failure (or a panicked worker) is only an optimization
        // miss: send the valid uncompressed body.
        _ => body,
    }
}

async fn next_body_chunk<S>(
    stream: &mut S,
    idle_timeout: Duration,
    initial_timeout: Duration,
    first_chunk: bool,
    started_at: Instant,
    deadline: Duration,
    body_name: &'static str,
) -> Result<Option<bytes::Bytes>, AiError>
where
    S: futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
{
    let remaining = deadline.saturating_sub(started_at.elapsed());
    if remaining.is_zero() {
        return Err(AiError::Transport(TransportError {
            phase: TransportPhase::Body,
            timeout: true,
            message: format!("{body_name} exceeded its overall deadline"),
        }));
    }
    let quiet_timeout = if first_chunk {
        initial_timeout
    } else {
        idle_timeout
    };
    let wait_for = remaining.min(quiet_timeout);
    match tokio::time::timeout(wait_for, stream.next()).await {
        Err(_) => Err(AiError::Transport(TransportError {
            phase: TransportPhase::Body,
            timeout: true,
            message: if remaining <= quiet_timeout {
                format!("{body_name} exceeded its overall deadline")
            } else if first_chunk {
                format!("{body_name} was idle beyond its initial timeout")
            } else {
                format!("{body_name} was idle beyond its timeout")
            },
        })),
        Ok(Some(Err(error))) => Err(reqwest_transport_error(
            error,
            TransportPhase::Body,
            body_name,
        )),
        Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
        Ok(None) => Ok(None),
    }
}

struct HttpStreamRequest {
    model: Model,
    parts: crate::protocol::HttpRequestParts,
    headers: http::HeaderMap,
    requested_audio_format: Option<crate::types::AudioFormat>,
    tool_definitions: Vec<ToolDef>,
    pre_send_diagnostics: Vec<crate::error::Diagnostic>,
    buffer_ambiguous_compatibility_content: bool,
    diagnostic_redactor: CredentialRedactor,
}

/// Falling back is replay-safe only when opening the WebSocket failed before
/// the generation request could have been sent. Once the request actor accepts
/// a frame, every timeout, decode failure, or disconnect is ambiguous and must
/// remain terminal unless the provider supplies an idempotency contract.
fn websocket_open_failure_is_replay_safe(error: &AiError) -> bool {
    matches!(
        error,
        AiError::Transport(TransportError {
            phase: TransportPhase::Connect,
            ..
        })
    )
}

struct BedrockResponseStreamRequest {
    response: reqwest::Response,
    model: Model,
    tool_definitions: Vec<ToolDef>,
    pre_send_diagnostics: Vec<crate::error::Diagnostic>,
    buffer_ambiguous_compatibility_content: bool,
    diagnostic_redactor: CredentialRedactor,
    stream_initial_timeout: Duration,
    stream_idle_timeout: Duration,
    stream_deadline: Duration,
}

fn bedrock_response_stream(request: BedrockResponseStreamRequest) -> ResponseStream {
    let BedrockResponseStreamRequest {
        response,
        model,
        tool_definitions,
        pre_send_diagnostics,
        buffer_ambiguous_compatibility_content,
        diagnostic_redactor,
        stream_initial_timeout,
        stream_idle_timeout,
        stream_deadline,
    } = request;
    let raw_event_stream = try_stream! {
        let mut decoder = crate::protocol::bedrock::BedrockEventStreamDecoder::new();
        let mut state = crate::protocol::bedrock::BedrockStreamState::default();
        let mut builder = ResponseBuilder::new(
            model.spec.id.clone(),
            model.spec.protocol,
            model.spec.pricing.clone(),
        );
        builder.set_tool_definitions(&tool_definitions)?;
        builder.set_buffer_ambiguous_compatibility_content(
            buffer_ambiguous_compatibility_content,
        );
        for diagnostic in &pre_send_diagnostics {
            builder.add_diagnostic(diagnostic.clone());
        }

        let mut stream = response.bytes_stream();
        let mut terminal_seen = false;
        let mut provider_event_seen = false;
        let mut successful_body_prefix = Vec::new();
        let mut first_body_chunk = true;
        let started_at = Instant::now();
        let mut last_event_at = None;
        'read: loop {
            let remaining = stream_deadline.saturating_sub(started_at.elapsed());
            if remaining.is_zero() {
                Err(annotate_stream_failure(
                    AiError::Transport(TransportError {
                        phase: TransportPhase::Body,
                        timeout: true,
                        message: "stream exceeded its overall deadline".to_owned(),
                    }),
                    &builder,
                    first_body_chunk,
                    started_at,
                    last_event_at,
                ))?;
            }
            let quiet_timeout = if first_body_chunk {
                stream_initial_timeout
            } else {
                stream_idle_timeout
            };
            let wait_for = remaining.min(quiet_timeout);
            let chunk_result = tokio::time::timeout(wait_for, stream.next())
                .await
                .map_err(|_| {
                    annotate_stream_failure(
                        AiError::Transport(TransportError {
                            phase: TransportPhase::Body,
                            timeout: true,
                            message: if remaining <= quiet_timeout {
                                "stream exceeded its overall deadline".to_owned()
                            } else if first_body_chunk {
                                "stream was idle beyond its initial timeout".to_owned()
                            } else {
                                "stream was idle beyond its timeout".to_owned()
                            },
                        }),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
            let Some(chunk_result) = chunk_result else {
                break;
            };
            let chunk = chunk_result.map_err(|error| {
                annotate_stream_failure(
                    reqwest_transport_error(error, TransportPhase::Body, "Bedrock response body"),
                    &builder,
                    first_body_chunk,
                    started_at,
                    last_event_at,
                )
            })?;
            first_body_chunk = false;
            if !provider_event_seen && successful_body_prefix.len() < MAX_SUCCESS_ERROR_BODY_BYTES {
                let remaining = MAX_SUCCESS_ERROR_BODY_BYTES - successful_body_prefix.len();
                successful_body_prefix.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            let messages = decoder.push(&chunk).map_err(|error| {
                annotate_stream_failure(
                    AiError::Decode(error),
                    &builder,
                    first_body_chunk,
                    started_at,
                    last_event_at,
                )
            })?;
            if !messages.is_empty() {
                provider_event_seen = true;
                last_event_at = Some(Instant::now());
                successful_body_prefix.clear();
            }
            for message in messages {
                let events = crate::protocol::bedrock::decode_stream_event(
                    &model,
                    &message,
                    &mut builder,
                    &mut state,
                )
                .map_err(|error| {
                    annotate_stream_failure(
                        error,
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
                for event in events {
                    let terminal = matches!(event, StreamEvent::Finished(_));
                    yield event;
                    if terminal {
                        terminal_seen = true;
                        break 'read;
                    }
                }
            }
        }

        if !terminal_seen {
            decoder.finish().map_err(|error| {
                annotate_stream_failure(
                    AiError::Decode(error),
                    &builder,
                    first_body_chunk,
                    started_at,
                    last_event_at,
                )
            })?;
            let mut final_events = Vec::new();
            crate::protocol::bedrock::finish_stream(&mut builder, &mut state, &mut final_events)
                .map_err(|error| {
                    annotate_stream_failure(
                        error,
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
            for event in final_events {
                let terminal = matches!(event, StreamEvent::Finished(_));
                yield event;
                terminal_seen |= terminal;
            }
        }
        if !terminal_seen && !provider_event_seen {
            if let Some(error) = provider_error_from_success_body(&successful_body_prefix) {
                Err(annotate_stream_failure(
                    AiError::Provider(error),
                    &builder,
                    first_body_chunk,
                    started_at,
                    last_event_at,
                ))?;
            }
        }
    };
    let sanitized = raw_event_stream
        .map(move |event| event.map_err(|error| sanitize_ai_error(&diagnostic_redactor, error)));
    crate::stream::guard(sanitized)
}

async fn stream_http(
    http: reqwest::Client,
    request: HttpStreamRequest,
    stream_initial_timeout: Duration,
    stream_idle_timeout: Duration,
    stream_deadline: Duration,
) -> Result<ResponseStream, AiError> {
    let HttpStreamRequest {
        model,
        parts,
        mut headers,
        requested_audio_format,
        tool_definitions,
        pre_send_diagnostics,
        buffer_ambiguous_compatibility_content,
        mut diagnostic_redactor,
    } = request;
    let lifecycle_feedback = parts.streaming
        && model.spec.protocol == Protocol::OpenAiChat
        && model.endpoint.runtime.lifecycle_feedback;
    if lifecycle_feedback {
        headers.insert(
            http::HeaderName::from_static(LIFECYCLE_HEADER),
            http::HeaderValue::from_static(LIFECYCLE_REQUEST_VALUE),
        );
    }
    let request_body =
        prepare_request_body(model.endpoint.runtime, &mut headers, parts.body.clone()).await;
    if matches!(&model.endpoint.auth, crate::auth::Auth::RequestSigner(_)) {
        let resolved = crate::auth::resolve_headers_for_request(
            &model.endpoint.auth,
            http::Method::POST,
            parts.url.clone(),
            request_body.clone(),
            headers.clone(),
        )
        .await
        .map_err(AiError::Auth)?;
        diagnostic_redactor = resolved.redactor;
        diagnostic_redactor.include_header_values(&model.endpoint.default_headers);
        let mut current_key = None;
        for (key, value) in resolved.headers {
            if let Some(key) = key {
                current_key = Some(key.clone());
                headers.insert(key, value);
            } else if let Some(key) = &current_key {
                headers.append(key.clone(), value);
            }
        }
    }

    // 3. Send the HTTP request
    let builder = http
        .post(parts.url.clone())
        .headers(headers)
        .body(request_body);

    // `RequestBuilder::timeout` applies until the response body is fully
    // consumed, which kills valid long-running SSE generations. Bound only
    // the pre-stream phase instead: after headers arrive, the caller owns
    // the stream lifetime and may cancel by dropping it.
    let res = tokio::time::timeout(model.endpoint.timeout, builder.send())
        .await
        .map_err(|_| {
            AiError::Transport(TransportError {
                phase: TransportPhase::ResponseHeaders,
                timeout: true,
                message: "request timed out waiting for response headers".to_string(),
            })
        })?
        .map_err(|error| request_open_transport_error(error, "request"))
        .map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))?;

    // 4. Handle non-2xx HTTP errors
    let status = res.status();
    if !status.is_success() {
        // Extract only the two headers needed for the structured error
        // before consuming the response. Cloning the whole HeaderMap
        // here adds an allocation on every non-2xx response.
        let request_id = res
            .headers()
            .get("x-request-id")
            .or_else(|| res.headers().get("x-amzn-requestid"))
            .or_else(|| res.headers().get("request-id"))
            .and_then(|h| h.to_str().ok())
            .map(String::from);
        let retry_after = res
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);

        let mut body = Vec::with_capacity(4096);
        let mut error_stream = res.bytes_stream();
        let started_at = Instant::now();
        while body.len() < 4096 {
            match next_body_chunk(
                &mut error_stream,
                stream_idle_timeout.min(MAX_ERROR_BODY_IDLE_TIMEOUT),
                stream_idle_timeout.min(MAX_ERROR_BODY_IDLE_TIMEOUT),
                false,
                started_at,
                stream_deadline.min(MAX_ERROR_BODY_DEADLINE),
                "HTTP error response body",
            )
            .await
            {
                Ok(Some(chunk)) => {
                    let remaining = 4096 - body.len();
                    body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }
                // The status and retry metadata are already known. Preserve
                // that structured HTTP error if its optional snippet stalls.
                Ok(None) | Err(_) => break,
            }
        }
        let body_bytes = String::from_utf8_lossy(&body).into_owned();

        let mut code = None;

        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&body_bytes) {
            if let Some(err_obj) = val.get("error") {
                code = err_obj.get("code").and_then(json_scalar_string);
            }
        }

        // Mark only gateway/transient statuses as replay-safe. The agent
        // still gates retries on having seen no generated bytes, so a
        // POST cannot duplicate a completed tool-producing turn.
        let retryable = matches!(
            status,
            http::StatusCode::REQUEST_TIMEOUT
                | http::StatusCode::INTERNAL_SERVER_ERROR
                | http::StatusCode::TOO_MANY_REQUESTS
                | http::StatusCode::BAD_GATEWAY
                | http::StatusCode::SERVICE_UNAVAILABLE
                | http::StatusCode::GATEWAY_TIMEOUT
        );

        return Err(sanitize_ai_error(
            &diagnostic_redactor,
            AiError::Http(HttpError {
                status,
                request_id,
                retry_after,
                provider_code: code,
                body_snippet: if body_bytes.is_empty() {
                    None
                } else {
                    Some(body_bytes)
                },
                retryable,
            }),
        ));
    }

    // 5. Decode ResponseStream
    let initial_lifecycle = lifecycle_feedback
        .then(|| {
            res.headers()
                .get(LIFECYCLE_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| parse_provider_lifecycle(value, &diagnostic_redactor))
        })
        .flatten();
    let model_clone = model.clone();
    if parts.streaming && model.spec.protocol == Protocol::BedrockConverse {
        return Ok(bedrock_response_stream(BedrockResponseStreamRequest {
            response: res,
            model: model_clone,
            tool_definitions,
            pre_send_diagnostics,
            buffer_ambiguous_compatibility_content,
            diagnostic_redactor,
            stream_initial_timeout,
            stream_idle_timeout,
            stream_deadline,
        }));
    }
    if parts.streaming {
        let byte_stream = res.bytes_stream();
        let diags = pre_send_diagnostics;
        let lifecycle_redactor = diagnostic_redactor.clone();
        let raw_event_stream = try_stream! {
            let mut sse_decoder = crate::protocol::sse::SseDecoder::new();
            let mut builder = ResponseBuilder::new(
                model_clone.spec.id.clone(),
                model_clone.spec.protocol,
                model_clone.spec.pricing.clone()
            );
            builder.set_tool_definitions(&tool_definitions)?;
            builder.set_buffer_ambiguous_compatibility_content(
                buffer_ambiguous_compatibility_content,
            );
            for d in &diags {
                builder.add_diagnostic(d.clone());
            }

            let mut lifecycle_stream_started = false;
            let mut lifecycle_events_emitted = 0usize;
            if let Some(lifecycle) = initial_lifecycle {
                // Header feedback arrives before any provider SSE event. Seed
                // the canonical stream first so advisory telemetry still obeys
                // the `Started`-is-first invariant.
                let started = StreamEvent::Started { response_id: None };
                builder.on_event(&started)?;
                yield started;
                lifecycle_stream_started = true;
                lifecycle_events_emitted += 1;
                yield StreamEvent::ProviderLifecycle(lifecycle);
            }

            let mut stream = byte_stream;
            // The provider's terminal event (`[DONE]` / `response.completed`
            // / `message_stop`) yields a `Finished`. Per design §8 ("No events
            // after `Finished"), the HTTP body read must stop there: reading
            // further can block after success, surface a late body transport
            // error, or feed post-terminal frames into the codec. We stop the
            // instant the codec emits `Finished`.
            let mut terminal_seen = false;
            let mut provider_event_seen = false;
            let mut successful_body_prefix = Vec::new();
            let mut first_body_chunk = true;
            let started_at = Instant::now();
            let mut last_event_at = None;
            'read: loop {
                let remaining = stream_deadline.saturating_sub(started_at.elapsed());
                if remaining.is_zero() {
                    Err(annotate_stream_failure(
                        AiError::Transport(TransportError {
                            phase: TransportPhase::Body,
                            timeout: true,
                            message: "stream exceeded its overall deadline".to_string(),
                        }),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    ))?;
                }
                let quiet_timeout = if first_body_chunk {
                    stream_initial_timeout
                } else {
                    stream_idle_timeout
                };
                let wait_for = remaining.min(quiet_timeout);
                let chunk_res = tokio::time::timeout(wait_for, stream.next())
                    .await
                    .map_err(|_| {
                        annotate_stream_failure(
                            AiError::Transport(TransportError {
                                phase: TransportPhase::Body,
                                timeout: true,
                                message: if remaining <= quiet_timeout {
                                    "stream exceeded its overall deadline".to_string()
                                } else if first_body_chunk {
                                    "stream was idle beyond its initial timeout".to_string()
                                } else {
                                    "stream was idle beyond its timeout".to_string()
                                },
                            }),
                            &builder,
                            first_body_chunk,
                            started_at,
                            last_event_at,
                        )
                    })?;
                let Some(chunk_res) = chunk_res else {
                    break;
                };
                let chunk = chunk_res.map_err(|error| {
                    annotate_stream_failure(
                        reqwest_transport_error(error, TransportPhase::Body, "response body"),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
                first_body_chunk = false;

                if !provider_event_seen
                    && successful_body_prefix.len() < MAX_SUCCESS_ERROR_BODY_BYTES
                {
                    let remaining = MAX_SUCCESS_ERROR_BODY_BYTES - successful_body_prefix.len();
                    successful_body_prefix
                        .extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                }

                let sse_frames = if lifecycle_feedback {
                    sse_decoder.push_frames(&chunk)
                } else {
                    sse_decoder.push(&chunk).map(|events| {
                        events
                            .into_iter()
                            .map(crate::protocol::sse::SseFrame::Event)
                            .collect()
                    })
                }
                .map_err(|error| {
                    annotate_stream_failure(
                        AiError::Decode(error),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
                let lifecycle_frame_seen = lifecycle_feedback
                    && lifecycle_events_emitted < MAX_PROVIDER_LIFECYCLE_EVENTS
                    && sse_frames.iter().any(|frame| {
                        matches!(
                            frame,
                            crate::protocol::sse::SseFrame::Comment(comment)
                                if lifecycle_from_sse_comment(comment, &lifecycle_redactor).is_some()
                        )
                    });
                if sse_frames.iter().any(|frame| matches!(frame, crate::protocol::sse::SseFrame::Event(_)))
                    || lifecycle_frame_seen
                {
                    provider_event_seen = true;
                    last_event_at = Some(Instant::now());
                    successful_body_prefix.clear();
                }

                for frame in sse_frames {
                    match frame {
                        crate::protocol::sse::SseFrame::Comment(comment) => {
                            if lifecycle_feedback
                                && lifecycle_events_emitted < MAX_PROVIDER_LIFECYCLE_EVENTS
                            {
                                if let Some(lifecycle) =
                                    lifecycle_from_sse_comment(&comment, &lifecycle_redactor)
                                {
                                    if !lifecycle_stream_started {
                                        let started = StreamEvent::Started { response_id: None };
                                        builder.on_event(&started)?;
                                        yield started;
                                        lifecycle_stream_started = true;
                                    }
                                    lifecycle_events_emitted += 1;
                                    yield StreamEvent::ProviderLifecycle(lifecycle);
                                }
                            }
                        }
                        crate::protocol::sse::SseFrame::Event(sse) => {
                            let stream_events = match model_clone.spec.protocol {
                                Protocol::OpenAiChat => crate::protocol::openai_chat::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::AnthropicMessages => crate::protocol::anthropic::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::OpenAiResponses => crate::protocol::openai_responses::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::BedrockConverse => unreachable!("Bedrock uses AWS Event Stream, not SSE"),
                                Protocol::GoogleGenerativeAi => crate::protocol::google::decode_stream_event(&model_clone, &sse, &mut builder),
                            }
                            .map_err(|error| {
                                annotate_stream_failure(
                                    error,
                                    &builder,
                                    first_body_chunk,
                                    started_at,
                                    last_event_at,
                                )
                            })?;
                            for ev in stream_events {
                                let started = matches!(ev, StreamEvent::Started { .. });
                                let terminal = matches!(ev, StreamEvent::Finished(_));
                                yield ev;
                                lifecycle_stream_started |= started;
                                if terminal {
                                    terminal_seen = true;
                                    break 'read;
                                }
                            }
                        }
                    }
                }
            }

            // Only flush trailing SSE frames if no terminal event was seen;
            // after `Finished` the stream is closed and any residue is ignored
            // rather than decoded into post-terminal events.
            if !terminal_seen {
                let trailing_frames = if lifecycle_feedback {
                    sse_decoder.finish_frames()
                } else {
                    sse_decoder.finish().map(|event| {
                        event
                            .into_iter()
                            .map(crate::protocol::sse::SseFrame::Event)
                            .collect()
                    })
                }
                .map_err(|error| {
                    annotate_stream_failure(
                        AiError::Decode(error),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    )
                })?;
                let lifecycle_frame_seen = lifecycle_feedback
                    && lifecycle_events_emitted < MAX_PROVIDER_LIFECYCLE_EVENTS
                    && trailing_frames.iter().any(|frame| {
                        matches!(
                            frame,
                            crate::protocol::sse::SseFrame::Comment(comment)
                                if lifecycle_from_sse_comment(comment, &lifecycle_redactor).is_some()
                        )
                    });
                if trailing_frames.iter().any(|frame| matches!(frame, crate::protocol::sse::SseFrame::Event(_)))
                    || lifecycle_frame_seen
                {
                    provider_event_seen = true;
                    last_event_at = Some(Instant::now());
                    successful_body_prefix.clear();
                }

                for frame in trailing_frames {
                    if terminal_seen {
                        break;
                    }
                    match frame {
                        crate::protocol::sse::SseFrame::Comment(comment) => {
                            if lifecycle_feedback
                                && lifecycle_events_emitted < MAX_PROVIDER_LIFECYCLE_EVENTS
                            {
                                if let Some(lifecycle) =
                                    lifecycle_from_sse_comment(&comment, &lifecycle_redactor)
                                {
                                    if !lifecycle_stream_started {
                                        let started = StreamEvent::Started { response_id: None };
                                        builder.on_event(&started)?;
                                        yield started;
                                        lifecycle_stream_started = true;
                                    }
                                    lifecycle_events_emitted += 1;
                                    yield StreamEvent::ProviderLifecycle(lifecycle);
                                }
                            }
                        }
                        crate::protocol::sse::SseFrame::Event(sse) => {
                            let stream_events = match model_clone.spec.protocol {
                                Protocol::OpenAiChat => crate::protocol::openai_chat::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::AnthropicMessages => crate::protocol::anthropic::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::OpenAiResponses => crate::protocol::openai_responses::decode_stream_event(&model_clone, &sse, &mut builder),
                                Protocol::BedrockConverse => unreachable!("Bedrock uses AWS Event Stream, not SSE"),
                                Protocol::GoogleGenerativeAi => crate::protocol::google::decode_stream_event(&model_clone, &sse, &mut builder),
                            }
                            .map_err(|error| {
                                annotate_stream_failure(
                                    error,
                                    &builder,
                                    first_body_chunk,
                                    started_at,
                                    last_event_at,
                                )
                            })?;
                            for ev in stream_events {
                                let started = matches!(ev, StreamEvent::Started { .. });
                                let terminal = matches!(ev, StreamEvent::Finished(_));
                                yield ev;
                                lifecycle_stream_started |= started;
                                if terminal {
                                    terminal_seen = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if !terminal_seen && !provider_event_seen {
                if let Some(error) =
                    provider_error_from_success_body(&successful_body_prefix)
                {
                    Err(annotate_stream_failure(
                        AiError::Provider(error),
                        &builder,
                        first_body_chunk,
                        started_at,
                        last_event_at,
                    ))?;
                }
            }
        };

        let sanitized_event_stream = raw_event_stream.map(move |event| {
            event.map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))
        });
        Ok(crate::stream::guard(sanitized_event_stream))
    } else {
        // Non-streaming path (completed response, e.g. Chat Audio output)
        let mut body_bytes = Vec::new();
        let mut byte_stream = res.bytes_stream();
        let mut first_body_chunk = true;
        let started_at = Instant::now();

        while let Some(chunk) = next_body_chunk(
            &mut byte_stream,
            stream_idle_timeout,
            stream_initial_timeout,
            first_body_chunk,
            started_at,
            stream_deadline,
            "completed response body",
        )
        .await
        .map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))?
        {
            first_body_chunk = false;
            if body_bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > MAX_COMPLETED_BODY_BYTES)
            {
                return Err(AiError::Decode(DecodeError::BodyTooLarge));
            }
            body_bytes.extend_from_slice(&chunk);
        }

        // The non-streaming path exists solely for the OpenAI Chat audio-output
        // request (design §12.1). Only that codec sets `streaming = false`;
        // Responses and Anthropic always stream, so no other codec needs a
        // non-streaming decoder. This is an invariant of `build_request`, not
        // a runtime branch, so no per-codec `decode_response` stub exists.
        debug_assert!(
            matches!(model_clone.spec.protocol, Protocol::OpenAiChat),
            "non-streaming path is Chat-only",
        );
        let mut response = crate::protocol::openai_chat::decode_response_with_tools(
            &model_clone,
            &body_bytes,
            requested_audio_format,
            &tool_definitions,
        )
        .map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))?;
        response.diagnostics.extend(pre_send_diagnostics);

        let response_id = response.response_id.clone();
        let message = response.message.clone();
        let usage = response.usage;

        let raw_event_stream = try_stream! {
            yield StreamEvent::Started { response_id: response_id.clone() };

            let mut index_counter = 0;
            for part in &message.content {
                match part {
                    crate::types::AssistantPart::ProviderMetadata(_) => {}
                    crate::types::AssistantPart::Text(text) => {
                        let idx = index_counter;
                        index_counter += 1;
                        yield StreamEvent::TextStart { index: idx };
                        yield StreamEvent::TextDelta { index: idx, delta: text.clone() };
                        yield StreamEvent::TextEnd { index: idx };
                    }
                    crate::types::AssistantPart::Reasoning(reasoning) => {
                        let idx = index_counter;
                        index_counter += 1;
                        yield StreamEvent::ReasoningStart { index: idx };
                        if let Some(ref text) = reasoning.text {
                            yield StreamEvent::ReasoningDelta { index: idx, delta: text.clone() };
                        }
                        yield StreamEvent::ReasoningEnd { index: idx };
                    }
                    crate::types::AssistantPart::Media(media) => {
                        let idx = index_counter;
                        index_counter += 1;
                        yield StreamEvent::MediaCompleted { index: idx, media: media.clone() };
                    }
                    crate::types::AssistantPart::ToolCall(tc) => {
                        let idx = index_counter;
                        index_counter += 1;
                        yield StreamEvent::ToolCallStart {
                            index: idx,
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                        };
                        yield StreamEvent::ToolCallArgsDelta {
                            index: idx,
                            delta: tc.arguments_json.clone(),
                        };
                        yield StreamEvent::ToolCallEnd {
                            index: idx,
                            argument_error: tc.argument_error,
                        };
                    }
                }
            }

            yield StreamEvent::Usage(usage);
            yield StreamEvent::Finished(response);
        };

        Ok(crate::stream::guard(raw_event_stream))
    }
}

/// Decode a cached Responses WebSocket using the same protocol builder as the
/// ordinary SSE path. The wire event shape is JSON rather than `data:` framed
/// SSE, so each message is wrapped in the codec's private event view.
#[allow(clippy::too_many_arguments)]
fn responses_websocket_stream(
    model: Model,
    mut events: mpsc::Receiver<Result<serde_json::Value, AiError>>,
    diagnostics: Vec<crate::error::Diagnostic>,
    tool_definitions: Vec<ToolDef>,
    buffer_ambiguous_compatibility_content: bool,
    diagnostic_redactor: CredentialRedactor,
    stream_initial_timeout: Duration,
    stream_idle_timeout: Duration,
    stream_deadline: Duration,
) -> ResponseStream {
    let raw_event_stream = try_stream! {
        let mut builder = ResponseBuilder::new(
            model.spec.id.clone(),
            model.spec.protocol,
            model.spec.pricing.clone(),
        );
        builder.set_tool_definitions(&tool_definitions)?;
        builder.set_buffer_ambiguous_compatibility_content(
            buffer_ambiguous_compatibility_content,
        );
        for diagnostic in diagnostics {
            builder.add_diagnostic(diagnostic);
        }

        let started_at = Instant::now();
        let mut terminal_seen = false;
        let mut emitted_event = false;
        let mut first_provider_event = false;
        let mut last_event_at = None;
        while !terminal_seen {
            let remaining = stream_deadline.saturating_sub(started_at.elapsed());
            let event_result = if remaining.is_zero() {
                Err(AiError::Transport(TransportError {
                    phase: TransportPhase::Body,
                    timeout: true,
                    message: "websocket stream exceeded its overall deadline".to_owned(),
                }))
            } else {
                let quiet_timeout = if emitted_event {
                    stream_idle_timeout
                } else {
                    stream_initial_timeout
                };
                tokio::time::timeout(remaining.min(quiet_timeout), events.recv())
                    .await
                    .map_err(|_| AiError::Transport(TransportError {
                        phase: TransportPhase::Body,
                        timeout: true,
                        message: if remaining <= quiet_timeout {
                            "websocket stream exceeded its overall deadline".to_owned()
                        } else if emitted_event {
                            "websocket stream was idle beyond its timeout".to_owned()
                        } else {
                            "websocket stream was idle beyond its initial timeout".to_owned()
                        },
                    }))
            };
            let event = match event_result {
                Ok(Some(event)) => event,
                Ok(None) => Err(AiError::Transport(TransportError {
                    phase: TransportPhase::Body,
                    timeout: false,
                    message: "Responses WebSocket ended before completion".to_owned(),
                })),
                Err(error) => Err(error),
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    events.close();
                    Err(annotate_stream_failure(
                        error,
                        &builder,
                        !first_provider_event,
                        started_at,
                        last_event_at,
                    ))?
                }
            };
            first_provider_event = true;
            last_event_at = Some(Instant::now());
            let data = match serde_json::to_string(&event) {
                Ok(data) => data,
                Err(error) => {
                    events.close();
                    Err(annotate_stream_failure(
                        AiError::Decode(DecodeError::Json(error.to_string())),
                        &builder,
                        !first_provider_event,
                        started_at,
                        last_event_at,
                    ))?
                }
            };
            let sse_event = crate::protocol::sse::SseEvent {
                event: None,
                data,
            };
            let decoded = match crate::protocol::openai_responses::decode_stream_event(
                &model,
                &sse_event,
                &mut builder,
            ) {
                Ok(decoded) => decoded,
                Err(error) => {
                    events.close();
                    Err(annotate_stream_failure(
                        error,
                        &builder,
                        !first_provider_event,
                        started_at,
                        last_event_at,
                    ))?
                }
            };
            for event in decoded {
                let terminal = matches!(event, StreamEvent::Finished(_));
                emitted_event = true;
                yield event;
                if terminal {
                    terminal_seen = true;
                    break;
                }
            }
        }
    };
    let sanitized_event_stream = raw_event_stream
        .map(move |event| event.map_err(|error| sanitize_ai_error(&diagnostic_redactor, error)));
    crate::stream::guard(sanitized_event_stream)
}

/// Client wrapper for executing AI service requests.
#[derive(Clone)]
pub struct AiClient {
    http: reqwest::Client,
    responses_ws: ResponsesWsPool,
    host_stream_transports: Arc<StdMutex<HashMap<EndpointId, Arc<dyn HostStreamTransport>>>>,
    stream_initial_timeout: Duration,
    stream_idle_timeout: Duration,
    stream_deadline: Duration,
}

impl Default for AiClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AiClient {
    /// Creates a new AiClient using the default reqwest client.
    ///
    /// [`Self::try_new`] is available to callers that need to handle client
    /// construction errors. This convenience constructor fails loudly rather
    /// than silently replacing the explicit no-redirect policy with reqwest's
    /// redirect-following default.
    pub fn new() -> Self {
        Self::try_new().expect("failed to initialize the ygg HTTP client")
    }

    /// Creates a new AiClient, preserving ygg's no-redirect transport policy.
    ///
    /// Reqwest has no useful generation deadline by itself. Ygg applies the
    /// endpoint timeout while waiting for headers, then allows a generous
    /// first body chunk before enforcing inter-chunk idle and overall body
    /// deadlines in [`Self::stream`].
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            responses_ws: ResponsesWsPool::default(),
            host_stream_transports: Arc::new(StdMutex::new(HashMap::new())),
            stream_initial_timeout: DEFAULT_STREAM_INITIAL_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            stream_deadline: DEFAULT_STREAM_DEADLINE,
        })
    }

    /// Creates an AiClient wrapping a custom reqwest HTTP client.
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self {
            http,
            responses_ws: ResponsesWsPool::default(),
            host_stream_transports: Arc::new(StdMutex::new(HashMap::new())),
            stream_initial_timeout: DEFAULT_STREAM_INITIAL_TIMEOUT,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            stream_deadline: DEFAULT_STREAM_DEADLINE,
        }
    }

    /// Registers a host-owned stream transport for one catalog endpoint.
    ///
    /// The registration is shared by clones of this client. The transport sees
    /// only canonical request data and [`crate::HostStreamModel`]; resolved
    /// credentials, endpoint URLs, and headers are deliberately not exposed.
    /// Replacing a transport is an explicit host authority action.
    pub fn register_host_stream_transport(
        &self,
        endpoint: EndpointId,
        transport: Arc<dyn HostStreamTransport>,
    ) {
        self.host_stream_transports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(endpoint, transport);
    }

    /// Removes a host-owned stream transport for one endpoint.
    pub fn remove_host_stream_transport(&self, endpoint: &EndpointId) {
        self.host_stream_transports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(endpoint);
    }

    /// Sets the maximum quiet interval and absolute lifetime of response bodies,
    /// including SSE and completed JSON. Bounded HTTP error snippets also obey
    /// these values but retain shorter internal ceilings so an already-known
    /// status is surfaced promptly.
    /// The idle value also bounds the first body chunk; callers that need a
    /// longer time-to-first-body-byte can override it with
    /// [`Self::with_initial_stream_timeout`].
    /// Callers can use shorter values in tests or batch workers.
    pub fn with_stream_timeouts(mut self, idle_timeout: Duration, deadline: Duration) -> Self {
        let idle_timeout = idle_timeout.max(Duration::from_millis(1));
        self.stream_initial_timeout = idle_timeout;
        self.stream_idle_timeout = idle_timeout;
        self.stream_deadline = deadline.max(Duration::from_millis(1));
        self
    }

    /// Sets the maximum time allowed for the first successful response-body
    /// chunk after headers arrive, or the first decoded WebSocket event. This is
    /// independent from the shorter inter-chunk idle timeout and is useful for
    /// large prompts or cold local model servers. Bounded error bodies use a
    /// separate short ceiling so their already-known HTTP status is surfaced
    /// promptly.
    pub fn with_initial_stream_timeout(mut self, timeout: Duration) -> Self {
        self.stream_initial_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Executes one provider request and returns a pinned stream of events.
    ///
    /// This transport deliberately performs no automatic retries. Callers own
    /// retry count, backoff, cancellation, and idempotency policy; structured
    /// HTTP errors retain `retry_after` and `retryable` metadata for that use.
    pub async fn stream(&self, model: &Model, req: Request) -> Result<ResponseStream, AiError> {
        self.stream_once(model, req).await
    }

    /// Best-effort prewarms a cached OpenAI Responses WebSocket.
    ///
    /// The request is sent with the provider-specific `generate=false` flag,
    /// so this establishes connection/continuation state without consuming a
    /// model turn. Callers can invoke it from an input/composer task and ignore
    /// the result; ordinary [`Self::stream`] calls always retain HTTP/SSE
    /// fallback behavior.
    pub async fn prewarm_responses(&self, model: &Model, req: Request) -> Result<(), AiError> {
        crate::catalog::validate_endpoint(&model.endpoint)?;
        crate::catalog::validate_model_spec(&model.spec)?;
        if model.spec.endpoint != model.endpoint.id {
            return Err(crate::ConfigError::UnknownEndpoint(model.spec.endpoint.clone()).into());
        }
        if !matches!(
            model.endpoint.transport,
            crate::types::EndpointTransport::WebSocketPreferred
        ) || model.spec.protocol != Protocol::OpenAiResponses
        {
            return Ok(());
        }
        let session_id = req.session_id.clone().filter(|id| !id.is_empty());
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let mut req = req;
        req.messages = crate::transform::transform_request_messages_owned(req.messages, model);
        crate::json_repair::validate_tool_definitions(&req.tools).map_err(AiError::Decode)?;
        let parts = crate::protocol::openai_responses::build_request(model, &req)?;
        let mut headers = http::HeaderMap::new();
        for (key, value) in &model.endpoint.default_headers {
            headers.insert(key.clone(), value.clone());
        }
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        for (key, value) in &parts.headers {
            headers.insert(key.clone(), value.clone());
        }
        let resolved_headers = crate::auth::resolve_headers(&model.endpoint.auth)
            .await
            .map_err(AiError::Auth)?;
        let mut diagnostic_redactor = resolved_headers.redactor;
        diagnostic_redactor.include_header_values(&model.endpoint.default_headers);
        let mut current_key = None;
        for (key, value) in resolved_headers.headers {
            if let Some(key) = key {
                current_key = Some(key.clone());
                headers.insert(key, value);
            } else if let Some(key) = &current_key {
                headers.append(key.clone(), value);
            }
        }
        if model
            .endpoint
            .runtime
            .responses_profile
            .sends_websocket_beta_header()
        {
            headers.insert(
                http::HeaderName::from_static("openai-beta"),
                http::HeaderValue::from_static(ResponsesWsPool::beta_header_value()),
            );
        }
        let body = serde_json::from_slice::<serde_json::Value>(&parts.body)
            .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
        let key = format!("{}:{}:{session_id}", model.endpoint.id.0, model.spec.id.0);
        let result = tokio::time::timeout(
            model.endpoint.timeout.min(DEFAULT_CONNECT_TIMEOUT),
            self.responses_ws.prewarm(
                &key,
                parts.url,
                headers,
                body,
                ResponsesWsLiveness::for_response_idle(self.stream_idle_timeout),
            ),
        )
        .await
        .map_err(|_| {
            AiError::Transport(TransportError {
                phase: TransportPhase::Connect,
                timeout: true,
                message: "Responses WebSocket prewarm timed out".to_owned(),
            })
        })?;
        result.map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))
    }

    async fn stream_once(&self, model: &Model, req: Request) -> Result<ResponseStream, AiError> {
        crate::catalog::validate_endpoint(&model.endpoint)?;
        crate::catalog::validate_model_spec(&model.spec)?;
        if model.spec.endpoint != model.endpoint.id {
            return Err(crate::ConfigError::UnknownEndpoint(model.spec.endpoint.clone()).into());
        }

        let host_transport = self
            .host_stream_transports
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&model.endpoint.id)
            .cloned();
        if let Some(transport) = host_transport {
            // Keep host-mediated transports on the canonical side of the same
            // replay-history and capability boundary as HTTP codecs. Unlike a
            // protocol codec they cannot safely perform lossy wire-specific
            // degradation, so validate strictly rather than exposing an
            // unsupported canonical feature to an extension transport.
            let mut request = req;
            request.messages =
                crate::transform::transform_request_messages_owned(request.messages, model);
            let request =
                crate::validate::normalize_request_reasoning(&request, &model.spec.capabilities)
                    .into_owned();
            let diagnostics = crate::validate::validate_request(
                &request,
                &model.spec.capabilities,
                &model.spec.limits,
                model.spec.protocol,
                &model.spec.id,
                crate::CompatibilityMode::Strict,
            )?;
            let stream = transport
                .stream(HostStreamModel::from(model), request, diagnostics)
                .await?;
            return Ok(crate::stream::guard(stream));
        }

        // Derive target-compatible replay history without mutating the caller's
        // canonical conversation. This must happen before strict validation:
        // cross-model reasoning, unsupported historical media, and interrupted
        // tool turns are normalized into valid canonical messages first.
        let mut req = req;
        req.messages = crate::transform::transform_request_messages_owned(req.messages, model);
        let tool_definitions = req.tools.clone();
        // Reject malformed schemas before a provider request can consume them.
        // The same immutable snapshot is retained by response assembly below.
        crate::json_repair::validate_tool_definitions(&tool_definitions)
            .map_err(AiError::Decode)?;
        // Ambiguous bare JSON must remain visible in the default strict stream.
        // Lossy mode is the explicit opt-in for holding it to EOF and
        // interpreting a provider's text as compatibility tool syntax.
        let buffer_ambiguous_compatibility_content =
            req.compatibility == crate::CompatibilityMode::Lossy;

        let requested_audio_format = match &req.output_modalities {
            crate::types::OutputModalities::TextAndAudio(options) => Some(options.format),
            crate::types::OutputModalities::Text => None,
        };
        // 1. Build the HTTP request parts via the protocol codec
        let parts = match model.spec.protocol {
            Protocol::OpenAiChat => crate::protocol::openai_chat::build_request(model, &req)?,
            Protocol::AnthropicMessages => crate::protocol::anthropic::build_request(model, &req)?,
            Protocol::OpenAiResponses => {
                crate::protocol::openai_responses::build_request(model, &req)?
            }
            Protocol::BedrockConverse => crate::protocol::bedrock::build_request(model, &req)?,
            Protocol::GoogleGenerativeAi => crate::protocol::google::build_request(model, &req)?,
        };

        // Pre-send Lossy diagnostics (capability drops computed in `build_request`)
        // must reach the terminal `Finished` response (design §7). Capture them
        // here and seed the assembly with them below.
        let pre_send_diagnostics = parts.diagnostics.clone();

        // 2. Compose headers in precedence order:
        //    a. Endpoint default headers
        //    b. Request-specific/codec headers
        //    c. Dynamic/Resolved auth headers
        let mut headers = http::HeaderMap::new();

        for (k, v) in &model.endpoint.default_headers {
            headers.insert(k.clone(), v.clone());
        }

        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        for (k, v) in &parts.headers {
            headers.insert(k.clone(), v.clone());
        }

        // Request-aware signers (SigV4) must run after body encoding, so the
        // exact body and final header set are covered. Ordinary auth remains
        // resolved here so the Responses WebSocket path can use it directly.
        let request_aware_signer =
            matches!(&model.endpoint.auth, crate::auth::Auth::RequestSigner(_));
        let mut diagnostic_redactor = CredentialRedactor::default();
        if !request_aware_signer {
            let resolved_headers = crate::auth::resolve_headers(&model.endpoint.auth)
                .await
                .map_err(AiError::Auth)?;
            diagnostic_redactor = resolved_headers.redactor;
            let mut current_key = None;
            for (key, value) in resolved_headers.headers {
                if let Some(key) = key {
                    current_key = Some(key.clone());
                    headers.insert(key, value);
                } else if let Some(key) = &current_key {
                    headers.append(key.clone(), value);
                }
            }
        }
        diagnostic_redactor.include_header_values(&model.endpoint.default_headers);

        let fallback_request = HttpStreamRequest {
            model: model.clone(),
            parts,
            headers,
            requested_audio_format,
            tool_definitions,
            pre_send_diagnostics,
            buffer_ambiguous_compatibility_content,
            diagnostic_redactor: diagnostic_redactor.clone(),
        };

        // Responses WebSockets are deliberately opt-in per endpoint. A
        // connection/handshake failure is replay-safe and falls back to the
        // ordinary HTTP/SSE request below. Once the generation frame may have
        // been sent, every timeout or disconnect is terminal: silently replaying
        // the POST could duplicate provider work and billing.
        if matches!(
            model.endpoint.transport,
            crate::types::EndpointTransport::WebSocketPreferred
        ) && model.spec.protocol == Protocol::OpenAiResponses
            && !request_aware_signer
            && fallback_request.parts.streaming
        {
            let session_key = req.session_id.as_deref().filter(|id| !id.is_empty());
            let websocket_key = session_key
                .map(|session| format!("{}:{}:{session}", model.endpoint.id.0, model.spec.id.0));
            let mut ws_headers = fallback_request.headers.clone();
            if model
                .endpoint
                .runtime
                .responses_profile
                .sends_websocket_beta_header()
            {
                ws_headers.insert(
                    http::HeaderName::from_static("openai-beta"),
                    http::HeaderValue::from_static(ResponsesWsPool::beta_header_value()),
                );
            }
            if let Ok(body) =
                serde_json::from_slice::<serde_json::Value>(&fallback_request.parts.body)
            {
                let connect_timeout = model.endpoint.timeout.min(DEFAULT_CONNECT_TIMEOUT);
                let result = tokio::time::timeout(
                    connect_timeout,
                    self.responses_ws.request(
                        websocket_key.as_deref(),
                        fallback_request.parts.url.clone(),
                        ws_headers,
                        body,
                        ResponsesWsLiveness::for_response_idle(self.stream_idle_timeout),
                    ),
                )
                .await;
                match result {
                    Ok(Ok(events)) => {
                        return Ok(responses_websocket_stream(
                            model.clone(),
                            events,
                            fallback_request.pre_send_diagnostics.clone(),
                            fallback_request.tool_definitions.clone(),
                            buffer_ambiguous_compatibility_content,
                            diagnostic_redactor,
                            self.stream_initial_timeout,
                            self.stream_idle_timeout,
                            self.stream_deadline,
                        ));
                    }
                    Ok(Err(error)) if websocket_open_failure_is_replay_safe(&error) => {}
                    Ok(Err(error)) => {
                        return Err(sanitize_ai_error(&diagnostic_redactor, error));
                    }
                    Err(_) => {
                        return Err(AiError::Transport(TransportError {
                            phase: TransportPhase::ResponseHeaders,
                            timeout: true,
                            message: "Responses WebSocket request timed out before stream start"
                                .to_owned(),
                        }));
                    }
                }
            }
        }

        stream_http(
            self.http.clone(),
            fallback_request,
            self.stream_initial_timeout,
            self.stream_idle_timeout,
            self.stream_deadline,
        )
        .await
    }

    /// Calls OpenAI's native `POST /responses/compact` endpoint.
    ///
    /// The returned output is opaque and intentionally unpruned; callers may
    /// use [`crate::ResponsesCompactResponse::output`] directly as the next
    /// full replay window.
    pub async fn compact_responses(
        &self,
        model: &Model,
        mut request: ResponsesCompactRequest,
    ) -> Result<ResponsesCompactResponse, AiError> {
        crate::catalog::validate_endpoint(&model.endpoint)?;
        crate::catalog::validate_model_spec(&model.spec)?;
        if model.spec.endpoint != model.endpoint.id {
            return Err(crate::ConfigError::UnknownEndpoint(model.spec.endpoint.clone()).into());
        }
        if model.spec.protocol != Protocol::OpenAiResponses {
            return Err(crate::error::UnsupportedError::ResponsesOptions.into());
        }
        if request.model != model.spec.api_name {
            return Err(crate::ConfigError::Parse(format!(
                "compact request model {:?} does not match selected model {:?}",
                request.model, model.spec.api_name
            ))
            .into());
        }
        let rich_codex_schema = model
            .endpoint
            .runtime
            .responses_profile
            .supports_rich_compact_schema()
            || model.spec.cache.session_affinity_format
                == Some(crate::types::SessionAffinityFormat::Codex)
            || model.spec.capabilities.responses_lite;
        if !rich_codex_schema {
            // Public OpenAI compact has a narrower body than the private Codex
            // and Responses Lite contracts. Fail closed at the transport
            // boundary even for callers that constructed the public DTO
            // manually.
            request.tools = None;
            request.parallel_tool_calls = None;
            request.reasoning = None;
            request.text = None;
        }
        let url = model
            .endpoint
            .base_url
            .join("responses/compact")
            .map_err(|error| crate::error::ConfigError::Parse(error.to_string()))?;
        let body = serde_json::to_vec(&request)
            .map_err(|error| AiError::Decode(DecodeError::Json(error.to_string())))?;
        let mut headers = model.endpoint.default_headers.clone();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        for (key, value) in crate::protocol::openai_responses::responses_affinity_headers(
            model,
            request.session_id.as_deref(),
        )? {
            if let Some(key) = key {
                headers.insert(key, value);
            }
        }
        // Codex compresses ordinary streaming Responses requests, but its
        // compact endpoint contract is plain JSON. Do not apply the normal
        // Responses transport compression policy here.
        let body = bytes::Bytes::from(body);
        let resolved_headers = crate::auth::resolve_headers(&model.endpoint.auth)
            .await
            .map_err(AiError::Auth)?;
        let mut diagnostic_redactor = resolved_headers.redactor;
        diagnostic_redactor.include_header_values(&model.endpoint.default_headers);
        let mut current_key = None;
        for (key, value) in resolved_headers.headers {
            if let Some(key) = key {
                current_key = Some(key.clone());
                headers.insert(key, value);
            } else if let Some(key) = &current_key {
                headers.append(key.clone(), value);
            }
        }
        let response = tokio::time::timeout(
            model.endpoint.timeout,
            self.http.post(url).headers(headers).body(body).send(),
        )
        .await
        .map_err(|_| {
            AiError::Transport(TransportError {
                phase: TransportPhase::ResponseHeaders,
                timeout: true,
                message: "compact request timed out waiting for response headers".to_owned(),
            })
        })?
        .map_err(|error| request_open_transport_error(error, "compact request"))
        .map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("request-id"))
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        if !status.is_success() {
            let mut body = Vec::with_capacity(4096);
            let mut body_stream = response.bytes_stream();
            let started_at = Instant::now();
            while body.len() < 4096 {
                match next_body_chunk(
                    &mut body_stream,
                    self.stream_idle_timeout.min(MAX_ERROR_BODY_IDLE_TIMEOUT),
                    self.stream_idle_timeout.min(MAX_ERROR_BODY_IDLE_TIMEOUT),
                    false,
                    started_at,
                    self.stream_deadline.min(MAX_ERROR_BODY_DEADLINE),
                    "compact HTTP error response body",
                )
                .await
                {
                    Ok(Some(chunk)) => {
                        let remaining = 4096 - body.len();
                        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            let snippet = String::from_utf8_lossy(&body).into_owned();
            let provider_code = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(|error| error.get("code"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            let retryable = matches!(
                status,
                http::StatusCode::REQUEST_TIMEOUT
                    | http::StatusCode::TOO_MANY_REQUESTS
                    | http::StatusCode::BAD_GATEWAY
                    | http::StatusCode::SERVICE_UNAVAILABLE
                    | http::StatusCode::GATEWAY_TIMEOUT
            );
            return Err(sanitize_ai_error(
                &diagnostic_redactor,
                HttpError {
                    status,
                    request_id,
                    retry_after,
                    provider_code,
                    body_snippet: (!snippet.is_empty()).then_some(snippet),
                    retryable,
                }
                .into(),
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_COMPLETED_BODY_BYTES as u64)
        {
            return Err(DecodeError::BodyTooLarge.into());
        }
        let mut body = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or_default()
                .min(MAX_COMPLETED_BODY_BYTES as u64) as usize,
        );
        let mut body_stream = response.bytes_stream();
        let mut first_body_chunk = true;
        let started_at = Instant::now();
        while let Some(chunk) = next_body_chunk(
            &mut body_stream,
            self.stream_idle_timeout,
            self.stream_initial_timeout,
            first_body_chunk,
            started_at,
            self.stream_deadline,
            "compact response body",
        )
        .await
        .map_err(|error| sanitize_ai_error(&diagnostic_redactor, error))?
        {
            first_body_chunk = false;
            if body
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > MAX_COMPLETED_BODY_BYTES)
            {
                return Err(DecodeError::BodyTooLarge.into());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|error| {
            sanitize_ai_error(
                &diagnostic_redactor,
                AiError::Decode(DecodeError::Json(error.to_string())),
            )
        })
    }

    /// Executes a request and drives the stream to completion, returning the final Response.
    pub async fn complete(&self, model: &Model, req: Request) -> Result<Response, AiError> {
        let mut stream = self.stream(model, req).await?;
        let mut final_response = None;

        while let Some(ev_res) = stream.next().await {
            let ev = ev_res?;
            if let StreamEvent::Finished(resp) = ev {
                final_response = Some(resp);
            }
        }

        final_response.ok_or_else(|| AiError::StreamProtocol(StreamProtocolError::MissingFinish))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn declared_request_runtime_compresses_without_provider_identity() {
        let original = bytes::Bytes::from(vec![b'a'; 128 * 1024]);
        let mut headers = http::HeaderMap::new();
        let compressed = prepare_request_body(
            crate::types::RequestRuntime {
                body_encoding: crate::types::RequestBodyEncoding::Zstd,
                ..crate::types::RequestRuntime::default()
            },
            &mut headers,
            original.clone(),
        )
        .await;
        assert_eq!(headers[http::header::CONTENT_ENCODING], "zstd");
        assert!(compressed.len() < original.len() / 10);
        assert_eq!(
            zstd::stream::decode_all(compressed.as_ref()).unwrap(),
            original.as_ref()
        );

        let mut generic_headers = http::HeaderMap::new();
        let generic = prepare_request_body(
            crate::types::RequestRuntime::default(),
            &mut generic_headers,
            original.clone(),
        )
        .await;
        assert_eq!(generic, original);
        assert!(generic_headers
            .get(http::header::CONTENT_ENCODING)
            .is_none());
    }

    #[tokio::test]
    async fn transport_diagnostic_keeps_cause_but_removes_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let secret = "must-not-appear";
        let url = format!("http://{address}/private/catalog?token={secret}");
        let error = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .expect_err("the released listener must refuse the connection");
        let AiError::Transport(error) = request_open_transport_error(error, "request") else {
            unreachable!()
        };
        assert_eq!(error.phase, TransportPhase::Connect);
        assert!(error.message.starts_with("request connection failed:"));
        assert!(error.message.contains("refused") || error.message.contains("connect"));
        assert!(!error.message.contains(secret));
        assert!(!error.message.contains("/private/catalog"));
        assert!(!error.message.contains(&address.to_string()));
    }

    #[test]
    fn lifecycle_details_are_redacted_control_safe_and_bounded() {
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", "Bearer lifecycle-secret".parse().unwrap());
        let mut redactor = CredentialRedactor::default();
        redactor.include_header_values(&headers);
        let detail = format!("Bearer lifecycle-secret \x1b{}", "é".repeat(200));

        let lifecycle = parse_provider_lifecycle(&format!("loading; {detail}"), &redactor)
            .expect("known lifecycle state");
        assert_eq!(lifecycle.state, ProviderLifecycleState::Loading);
        let detail = lifecycle.detail.expect("nonempty detail");
        assert!(detail.len() <= MAX_PROVIDER_LIFECYCLE_DETAIL_BYTES);
        assert!(detail.is_char_boundary(detail.len()));
        assert!(detail.contains("[REDACTED]"));
        assert!(!detail.contains("lifecycle-secret"));
        assert!(!detail.chars().any(char::is_control));
        assert!(lifecycle_from_sse_comment("ordinary keepalive", &redactor).is_none());
        assert!(parse_provider_lifecycle("unknown; ignored", &redactor).is_none());
    }

    #[test]
    fn provider_diagnostics_are_control_safe_and_post_sanitize_bounded() {
        let input = format!("\x1b\x07\u{202e}{}", "é".repeat(3_000));
        let output = sanitize_diagnostic(
            &CredentialRedactor::default(),
            &input,
            MAX_PROVIDER_DIAGNOSTIC_BYTES,
        );
        assert!(output.len() <= MAX_PROVIDER_DIAGNOSTIC_BYTES);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with('…'));
        assert!(!output.chars().any(char::is_control));
        assert!(!output.contains('\u{202e}'));
        assert!(output.contains(r"\u{1b}"));
        assert!(output.contains(r"\u{7}"));
        assert!(output.contains(r"\u{202e}"));
    }

    #[test]
    fn transport_diagnostic_truncation_preserves_utf8_boundaries() {
        let mut message = format!("{}étail", "a".repeat(511));
        truncate_transport_message(&mut message, 512);
        assert_eq!(message.len(), 511);
        assert!(message.chars().all(|character| character == 'a'));
    }
}
