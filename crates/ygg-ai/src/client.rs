//! The `AiClient`, the resolved `Model` handle, and request dispatch.

use async_stream::try_stream;
use futures_util::StreamExt;
use std::time::Duration;

use crate::catalog::Model;
use crate::error::{
    AiError, DecodeError, HttpError, StreamProtocolError, TransportError, TransportPhase,
};
use crate::stream::{ResponseBuilder, ResponseStream, StreamEvent};
use crate::types::{Protocol, Request, Response};

/// Hard cap on a buffered non-streaming response body before JSON decode
/// (design §20). Crossing it is a [`DecodeError::BodyTooLarge`].
const MAX_COMPLETED_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Client wrapper for executing AI service requests.
#[derive(Clone)]
pub struct AiClient {
    http: reqwest::Client,
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
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }

    /// Creates an AiClient wrapping a custom reqwest HTTP client.
    pub fn with_http_client(http: reqwest::Client) -> Self {
        Self { http }
    }

    /// Executes a request and returns a pinned Stream of StreamEvents.
    pub async fn stream(&self, model: &Model, req: Request) -> Result<ResponseStream, AiError> {
        crate::catalog::validate_endpoint(&model.endpoint)?;
        crate::catalog::validate_model_spec(&model.spec)?;
        if model.spec.endpoint != model.endpoint.id {
            return Err(crate::ConfigError::UnknownEndpoint(model.spec.endpoint.clone()).into());
        }
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
            .body(parts.body.clone())
            .timeout(model.endpoint.timeout);

        let res = builder.send().await.map_err(|e| {
            let phase = if e.is_connect() || e.is_request() {
                TransportPhase::ConnectOrHeaders
            } else {
                TransportPhase::Body
            };
            AiError::Transport(TransportError {
                phase,
                timeout: e.is_timeout(),
                message: "request transport failed".to_string(),
            })
        })?;

        // 4. Handle non-2xx HTTP errors
        let status = res.status();
        if !status.is_success() {
            let status_code = status.as_u16();
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
            while body.len() < 4096 {
                match error_stream.next().await {
                    Some(Ok(chunk)) => {
                        let remaining = 4096 - body.len();
                        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                    }
                    Some(Err(_)) | None => break,
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

            let retryable = status_code == 429 || status_code == 503;

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
                'read: while let Some(chunk_res) = stream.next().await {
                    let chunk = chunk_res.map_err(|e| {
                        AiError::Transport(TransportError {
                            phase: TransportPhase::Body,
                            timeout: e.is_timeout(),
                            message: "response body transport failed".to_string(),
                        })
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

            while let Some(chunk_res) = byte_stream.next().await {
                let chunk = chunk_res.map_err(|e| {
                    AiError::Transport(TransportError {
                        phase: TransportPhase::Body,
                        timeout: e.is_timeout(),
                        message: "response body transport failed".to_string(),
                    })
                })?;

                if body_bytes
                    .len()
                    .checked_add(chunk.len())
                    .map_or(true, |size| size > MAX_COMPLETED_BODY_BYTES)
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
