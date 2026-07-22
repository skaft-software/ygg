//! The `AiClient`, the resolved `Model` handle, and request dispatch.

use async_stream::try_stream;
use futures_util::StreamExt;
use std::error::Error as _;
use std::time::{Duration, Instant};

use crate::catalog::Model;
use crate::error::{
    AiError, DecodeError, HttpError, StreamProtocolError, TransportError, TransportPhase,
};
use crate::stream::{ResponseBuilder, ResponseStream, StreamEvent};
use crate::types::{Protocol, Request, Response};

/// Hard cap on a buffered non-streaming response body before JSON decode
/// (design §20). Crossing it is a [`DecodeError::BodyTooLarge`].
const MAX_COMPLETED_BODY_BYTES: usize = 64 * 1024 * 1024;
/// Bound DNS/TCP/TLS establishment independently from a provider's header
/// timeout. Without this, a dead route can consume the full endpoint timeout on
/// every retry before the UI receives an error.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum silence allowed between SSE body chunks.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Absolute deadline for one generation request.
const DEFAULT_STREAM_DEADLINE: Duration = Duration::from_secs(30 * 60);
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

/// Compress the private ChatGPT Codex Responses request body.
///
/// This is deliberately endpoint-specific. OpenAI-compatible gateways do not
/// uniformly accept request `Content-Encoding`, while the Codex SSE endpoint
/// explicitly supports zstd. Compression failure is only an optimization miss:
/// preserve the valid uncompressed request instead of failing the model turn.
fn prepare_request_body(
    endpoint_id: &crate::types::EndpointId,
    protocol: Protocol,
    headers: &mut http::HeaderMap,
    body: bytes::Bytes,
) -> bytes::Bytes {
    if endpoint_id.0 != "openai-codex" || protocol != Protocol::OpenAiResponses {
        return body;
    }

    match zstd::bulk::compress(&body, CODEX_REQUEST_ZSTD_LEVEL) {
        Ok(compressed) => {
            headers.insert(
                http::header::CONTENT_ENCODING,
                http::HeaderValue::from_static("zstd"),
            );
            bytes::Bytes::from(compressed)
        }
        Err(_) => body,
    }
}

async fn next_body_chunk<S>(
    stream: &mut S,
    idle_timeout: Duration,
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
    let wait_for = remaining.min(idle_timeout);
    match tokio::time::timeout(wait_for, stream.next()).await {
        Err(_) => Err(AiError::Transport(TransportError {
            phase: TransportPhase::Body,
            timeout: true,
            message: if remaining <= idle_timeout {
                format!("{body_name} exceeded its overall deadline")
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

/// Client wrapper for executing AI service requests.
#[derive(Clone)]
pub struct AiClient {
    http: reqwest::Client,
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
    /// endpoint timeout while waiting for headers, then enforces stream-idle
    /// and overall generation deadlines in [`Self::stream`].
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            stream_deadline: DEFAULT_STREAM_DEADLINE,
        })
    }

    /// Creates an AiClient wrapping a custom reqwest HTTP client.
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self {
            http,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            stream_deadline: DEFAULT_STREAM_DEADLINE,
        }
    }

    /// Sets the maximum quiet interval and absolute lifetime of every response
    /// body, including SSE, completed JSON, and bounded HTTP error bodies.
    /// Callers can use shorter values in tests or batch workers.
    pub fn with_stream_timeouts(mut self, idle_timeout: Duration, deadline: Duration) -> Self {
        self.stream_idle_timeout = idle_timeout.max(Duration::from_millis(1));
        self.stream_deadline = deadline.max(Duration::from_millis(1));
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

    async fn stream_once(&self, model: &Model, req: Request) -> Result<ResponseStream, AiError> {
        crate::catalog::validate_endpoint(&model.endpoint)?;
        crate::catalog::validate_model_spec(&model.spec)?;
        if model.spec.endpoint != model.endpoint.id {
            return Err(crate::ConfigError::UnknownEndpoint(model.spec.endpoint.clone()).into());
        }

        // Derive target-compatible replay history without mutating the caller's
        // canonical conversation. This must happen before strict validation:
        // cross-model reasoning, unsupported historical media, and interrupted
        // tool turns are normalized into valid canonical messages first.
        let mut req = req;
        req.messages = crate::transform::transform_request_messages_owned(req.messages, model);
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

        let request_body = prepare_request_body(
            &model.endpoint.id,
            model.spec.protocol,
            &mut headers,
            parts.body.clone(),
        );

        let auth_headers = crate::auth::resolve_headers(&model.endpoint.auth)
            .await
            .map_err(AiError::Auth)?;

        let mut current_key = None;
        for (k, v) in auth_headers {
            if let Some(key) = k {
                current_key = Some(key.clone());
                headers.insert(key, v);
            } else if let Some(ref key) = current_key {
                headers.append(key.clone(), v);
            }
        }

        // 3. Send the HTTP request
        let builder = self
            .http
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
                    phase: TransportPhase::ConnectOrHeaders,
                    timeout: true,
                    message: "request timed out waiting for response headers".to_string(),
                })
            })?
            .map_err(|error| {
                let phase = if error.is_connect() || error.is_request() {
                    TransportPhase::ConnectOrHeaders
                } else {
                    TransportPhase::Body
                };
                reqwest_transport_error(error, phase, "request")
            })?;

        // 4. Handle non-2xx HTTP errors
        let status = res.status();
        if !status.is_success() {
            // Extract only the two headers needed for the structured error
            // before consuming the response body. Cloning the whole HeaderMap
            // here adds an allocation on every non-2xx response.
            let request_id = res
                .headers()
                .get("x-request-id")
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
                    self.stream_idle_timeout,
                    started_at,
                    self.stream_deadline,
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
                    code = err_obj
                        .get("code")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                }
            }

            // Mark only gateway/transient statuses as replay-safe. The agent
            // still gates retries on having seen no generated bytes, so a
            // POST cannot duplicate a completed tool-producing turn.
            let retryable = matches!(
                status,
                http::StatusCode::REQUEST_TIMEOUT
                    | http::StatusCode::TOO_MANY_REQUESTS
                    | http::StatusCode::BAD_GATEWAY
                    | http::StatusCode::SERVICE_UNAVAILABLE
                    | http::StatusCode::GATEWAY_TIMEOUT
            );

            return Err(AiError::Http(HttpError {
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
            }));
        }

        // 5. Decode ResponseStream
        let model_clone = model.clone();
        let stream_idle_timeout = self.stream_idle_timeout;
        let stream_deadline = self.stream_deadline;
        if parts.streaming {
            let byte_stream = res.bytes_stream();
            let diags = pre_send_diagnostics;
            let raw_event_stream = try_stream! {
                let mut sse_decoder = crate::protocol::sse::SseDecoder::new();
                let mut builder = ResponseBuilder::new(
                    model_clone.spec.id.clone(),
                    model_clone.spec.protocol,
                    model_clone.spec.pricing.clone()
                );
                builder.set_buffer_ambiguous_compatibility_content(
                    buffer_ambiguous_compatibility_content,
                );
                for d in &diags {
                    builder.add_diagnostic(d.clone());
                }

                let mut stream = byte_stream;
                // The provider's terminal event (`[DONE]` / `response.completed`
                // / `message_stop`) yields a `Finished`. Per design §8 ("No events
                // after `Finished`"), the HTTP body read must stop there: reading
                // further can block after success, surface a late body transport
                // error, or feed post-terminal frames into the codec. We stop the
                // instant the codec emits `Finished`.
                let mut terminal_seen = false;
                let started_at = Instant::now();
                'read: loop {
                    let remaining = stream_deadline.saturating_sub(started_at.elapsed());
                    if remaining.is_zero() {
                        Err(AiError::Transport(TransportError {
                            phase: TransportPhase::Body,
                            timeout: true,
                            message: "stream exceeded its overall deadline".to_string(),
                        }))?;
                    }
                    let wait_for = remaining.min(stream_idle_timeout);
                    let chunk_res = tokio::time::timeout(wait_for, stream.next())
                        .await
                        .map_err(|_| {
                            AiError::Transport(TransportError {
                                phase: TransportPhase::Body,
                                timeout: true,
                                message: if remaining <= stream_idle_timeout {
                                    "stream exceeded its overall deadline".to_string()
                                } else {
                                    "stream was idle beyond its timeout".to_string()
                                },
                            })
                        })?;
                    let Some(chunk_res) = chunk_res else {
                        break;
                    };
                    let chunk = chunk_res.map_err(|error| {
                        reqwest_transport_error(
                            error,
                            TransportPhase::Body,
                            "response body",
                        )
                    })?;

                    let sse_events = sse_decoder.push(&chunk).map_err(AiError::Decode)?;

                    for sse in sse_events {
                        let stream_events = match model_clone.spec.protocol {
                            Protocol::OpenAiChat => crate::protocol::openai_chat::decode_stream_event(&model_clone, &sse, &mut builder)?,
                            Protocol::AnthropicMessages => crate::protocol::anthropic::decode_stream_event(&model_clone, &sse, &mut builder)?,
                            Protocol::OpenAiResponses => crate::protocol::openai_responses::decode_stream_event(&model_clone, &sse, &mut builder)?,
                        };
                        for ev in stream_events {
                            let terminal = matches!(ev, StreamEvent::Finished(_));
                            yield ev;
                            if terminal {
                                terminal_seen = true;
                                break 'read;
                            }
                        }
                    }
                }

                // Only flush a trailing partial SSE frame if no terminal event was
                // seen; after `Finished` the stream is closed and any residue is
                // ignored rather than decoded into post-terminal events.
                if !terminal_seen {
                    if let Some(sse) = sse_decoder.finish().map_err(AiError::Decode)? {
                        let stream_events = match model_clone.spec.protocol {
                            Protocol::OpenAiChat => crate::protocol::openai_chat::decode_stream_event(&model_clone, &sse, &mut builder)?,
                            Protocol::AnthropicMessages => crate::protocol::anthropic::decode_stream_event(&model_clone, &sse, &mut builder)?,
                            Protocol::OpenAiResponses => crate::protocol::openai_responses::decode_stream_event(&model_clone, &sse, &mut builder)?,
                        };
                        for ev in stream_events {
                            yield ev;
                        }
                    }
                }
            };

            Ok(crate::stream::guard(raw_event_stream))
        } else {
            // Non-streaming path (completed response, e.g. Chat Audio output)
            let mut body_bytes = Vec::new();
            let mut byte_stream = res.bytes_stream();
            let started_at = Instant::now();

            while let Some(chunk) = next_body_chunk(
                &mut byte_stream,
                stream_idle_timeout,
                started_at,
                stream_deadline,
                "completed response body",
            )
            .await?
            {
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
            let mut response = crate::protocol::openai_chat::decode_response(
                &model_clone,
                &body_bytes,
                requested_audio_format,
            )?;
            response.diagnostics.extend(pre_send_diagnostics);

            let response_id = response.response_id.clone();
            let message = response.message.clone();
            let usage = response.usage;

            let raw_event_stream = try_stream! {
                yield StreamEvent::Started { response_id: response_id.clone() };

                let mut index_counter = 0;
                for part in &message.content {
                    match part {
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
                            yield StreamEvent::ToolCallEnd { index: idx };
                        }
                    }
                }

                yield StreamEvent::Usage(usage);
                yield StreamEvent::Finished(response);
            };

            Ok(crate::stream::guard(raw_event_stream))
        }
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

    #[test]
    fn codex_responses_body_is_zstd_compressed_and_other_routes_are_untouched() {
        let original = bytes::Bytes::from(vec![b'a'; 128 * 1024]);
        let mut headers = http::HeaderMap::new();
        let compressed = prepare_request_body(
            &crate::types::EndpointId("openai-codex".to_owned()),
            Protocol::OpenAiResponses,
            &mut headers,
            original.clone(),
        );
        assert_eq!(headers[http::header::CONTENT_ENCODING], "zstd");
        assert!(compressed.len() < original.len() / 10);
        assert_eq!(
            zstd::stream::decode_all(compressed.as_ref()).unwrap(),
            original.as_ref()
        );

        let mut generic_headers = http::HeaderMap::new();
        let generic = prepare_request_body(
            &crate::types::EndpointId("openai".to_owned()),
            Protocol::OpenAiResponses,
            &mut generic_headers,
            original.clone(),
        );
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
        let AiError::Transport(error) =
            reqwest_transport_error(error, TransportPhase::ConnectOrHeaders, "request")
        else {
            unreachable!()
        };
        assert!(error.message.starts_with("request connection failed:"));
        assert!(error.message.contains("refused") || error.message.contains("connect"));
        assert!(!error.message.contains(secret));
        assert!(!error.message.contains("/private/catalog"));
        assert!(!error.message.contains(&address.to_string()));
    }

    #[test]
    fn transport_diagnostic_truncation_preserves_utf8_boundaries() {
        let mut message = format!("{}étail", "a".repeat(511));
        truncate_transport_message(&mut message, 512);
        assert_eq!(message.len(), 511);
        assert!(message.chars().all(|character| character == 'a'));
    }
}
