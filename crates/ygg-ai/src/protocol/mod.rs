//! Private wire protocol codecs. Nothing here is part of the public API.

use serde::Serialize;

use crate::types::{CacheCompatibility, CacheRetention, Request};

/// Wire cache-control marker shared by Anthropic and compatible endpoints.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct CacheControl {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ttl: Option<&'static str>,
}

pub(crate) fn cache_session_id(req: &Request) -> Option<&str> {
    (req.cache_retention != CacheRetention::None)
        .then_some(req.session_id.as_deref())
        .flatten()
        .filter(|id| !id.is_empty())
}

pub(crate) fn prompt_cache_key(req: &Request) -> Option<String> {
    let id = cache_session_id(req)?;
    let key: String = id.chars().take(64).collect();
    (!key.is_empty()).then_some(key)
}

pub(crate) fn cache_control(
    req: &Request,
    compatibility: &CacheCompatibility,
) -> Option<CacheControl> {
    if req.cache_retention == CacheRetention::None {
        return None;
    }
    Some(CacheControl {
        kind: "ephemeral",
        ttl: (req.cache_retention == CacheRetention::Long && compatibility.supports_long_retention)
            .then_some("1h"),
    })
}

pub(crate) mod anthropic;
pub(crate) mod openai_chat;
pub(crate) mod openai_responses;

pub(crate) mod sse;

#[cfg(test)]
mod cross_protocol_tests;

#[cfg(test)]
mod normalize_tool_call_id_tests {
    use super::{normalize_tool_call_id, normalize_tool_call_id_owned, MAX_TOOL_CALL_ID_LEN};

    fn is_wire_valid(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= MAX_TOOL_CALL_ID_LEN
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }

    #[test]
    fn already_valid_ids_are_untouched() {
        for id in ["call_abc123", "toolu_01A", "a", "AZ-_09"] {
            assert_eq!(normalize_tool_call_id(id), id);
        }
    }

    #[test]
    fn already_valid_owned_id_keeps_its_string_allocation() {
        let id = String::from("call_abc123");
        let allocation = id.as_ptr();
        let normalized = normalize_tool_call_id_owned(id);
        assert_eq!(normalized, "call_abc123");
        assert_eq!(normalized.as_ptr(), allocation);
    }

    #[test]
    fn long_responses_id_is_normalized_into_charset_and_length() {
        // OpenAI Responses `call_…|item_…` shape, over-length (Pi pi-ai.md).
        let raw = format!("call_{}|item_{}", "a".repeat(240), "b".repeat(240));
        let out = normalize_tool_call_id(&raw);
        assert!(
            is_wire_valid(&out),
            "normalized id must be wire-valid: {out}"
        );
        assert_ne!(out, raw);
    }

    #[test]
    fn normalization_is_deterministic_so_call_and_result_pair() {
        // A call and its result share the same canonical id; the pure transform
        // must map both to the same wire id (design §11).
        let raw = "call_x|item_y/with:invalid.chars";
        assert_eq!(normalize_tool_call_id(raw), normalize_tool_call_id(raw));
    }

    #[test]
    fn distinct_ids_do_not_collide_via_hash() {
        let a = normalize_tool_call_id(&"z".repeat(100));
        let b = normalize_tool_call_id(&"z".repeat(101));
        assert_ne!(a, b);
    }
}

/// Maximum length of a tool-call ID on the wire, matching the canonical
/// `[A-Za-z0-9_-]{1,64}` shape every provider accepts (design §11, §7).
const MAX_TOOL_CALL_ID_LEN: usize = 64;

fn tool_call_id_is_wire_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_TOOL_CALL_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// Deterministically coerce a tool-call ID into the target charset/length.
///
/// Design §11 requires temporary tool IDs to be normalized when a protocol
/// constrains the format — a stable per-request transform (truncate + hash)
/// applied identically to a `ToolCall.id` and its `ToolResult.tool_call_id`, so
/// the call/result pairing is never broken. Because this is a pure function of
/// the ID string, both sides map to the same output automatically.
///
/// IDs already matching `[A-Za-z0-9_-]{1,64}` are returned untouched (canonical
/// IDs are left alone). Otherwise a short FNV-1a hash of the full original is
/// appended to a sanitized, truncated prefix. The hash is a self-contained,
/// version-stable implementation (not [`std::hash`], whose output is not
/// guaranteed stable) so results are reproducible and testable. Cross-protocol
/// replay of long OpenAI Responses IDs (`call_…|item_…`, 450+ chars) is the
/// motivating case (Pi `pi-ai.md` §"Tool Call ID Normalization"). The
/// 64-bit hash makes collisions extremely unlikely for provider-sized IDs but
/// cannot make them impossible; this transform is a wire-format compatibility
/// aid, not a cryptographic uniqueness guarantee.
pub(crate) fn normalize_tool_call_id(id: &str) -> String {
    if tool_call_id_is_wire_valid(id) {
        return id.to_string();
    }

    normalize_invalid_tool_call_id(id)
}

/// Owned normalization avoids copying the overwhelmingly common already-valid
/// ID when a consumed request is prepared for the wire.
pub(crate) fn normalize_tool_call_id_owned(id: String) -> String {
    if tool_call_id_is_wire_valid(&id) {
        return id;
    }
    normalize_invalid_tool_call_id(&id)
}

fn normalize_invalid_tool_call_id(id: &str) -> String {
    let is_valid_char = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';

    // FNV-1a 64-bit → 16 lowercase hex chars (all in the target charset).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hash_hex = format!("{hash:016x}");

    // Reserve room for `_<hash>`; fill the rest with the original's valid chars.
    let prefix_budget = MAX_TOOL_CALL_ID_LEN - 1 - hash_hex.len();
    let prefix: String = id
        .bytes()
        .filter(|&b| is_valid_char(b))
        .take(prefix_budget)
        .map(char::from)
        .collect();
    format!("{prefix}_{hash_hex}")
}

/// Protocol-agnostic HTTP request components prepared by a codec.
pub(crate) struct HttpRequestParts {
    /// Target endpoint URL (fully resolved).
    pub url: url::Url,
    /// Request-specific HTTP headers.
    pub headers: http::HeaderMap,
    /// Serialized request body bytes.
    pub body: bytes::Bytes,
    /// Whether the request uses SSE streaming.
    pub streaming: bool,
    /// Pre-send validation diagnostics generated during building.
    pub diagnostics: Vec<crate::error::Diagnostic>,
}

/// Shared, offline test harness for the codec fixture suites (design §19).
///
/// It feeds a captured/hand-authored `.sse` payload through the real
/// [`sse::SseDecoder`] and a codec's `decode_stream_event`, optionally
/// re-chunking the bytes at an arbitrary boundary, then runs the resulting
/// events through [`crate::stream::guard`] so every fixture also exercises the
/// §8 state-machine invariants (including `PrematureEof`).
#[cfg(test)]
pub(crate) mod harness {
    use std::sync::Arc;

    use crate::catalog::Model;
    use crate::error::AiError;
    use crate::pricing::Pricing;
    use crate::protocol::sse::{SseDecoder, SseEvent};
    use crate::stream::{guard, ResponseBuilder, StreamEvent};
    use crate::types::{
        Capabilities, Endpoint, EndpointId, Modality, ModalitySet, ModelId, ModelLimits, ModelSpec,
        Protocol, ReasoningCapability, ReasoningControl, Response,
    };

    /// A codec's per-event streaming decoder.
    pub(crate) type DecodeFn =
        fn(&Model, &SseEvent, &mut ResponseBuilder) -> Result<Vec<StreamEvent>, AiError>;

    /// A fully capable model for the given protocol. Decoding never consults
    /// capabilities, so a permissive model keeps decode fixtures focused.
    pub(crate) fn model(protocol: Protocol, pricing: Option<Pricing>) -> Model {
        let input = ModalitySet::none()
            .with(Modality::Image)
            .with(Modality::Audio);
        let output = ModalitySet::none().with(Modality::Audio);
        let spec = ModelSpec {
            id: ModelId("fixture-model".to_string()),
            endpoint: EndpointId("fixture-ep".to_string()),
            api_name: "fixture-api-name".to_string(),
            display_name: None,
            protocol,
            capabilities: Capabilities {
                input_modalities: input,
                output_modalities: output,
                tools: true,
                parallel_tool_calls: true,
                reasoning: Some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: true,
                    supports_pro_mode: false,
                    effort_budgets: None,
                    openai_chat_mode: crate::types::OpenAiChatReasoningMode::Standard,
                    min_effort: crate::types::ReasoningEffort::Minimal,
                    max_effort: crate::types::ReasoningEffort::High,
                }),
                structured_output: true,
            },
            limits: ModelLimits {
                context_window: 200_000,
                max_output_tokens: 8192,
            },
            pricing,
            cache: crate::types::CacheCompatibility::default(),
        };
        let endpoint = Endpoint {
            id: EndpointId("fixture-ep".to_string()),
            base_url: url::Url::parse("https://api.example.test/v1/").unwrap(),
            auth: crate::auth::Auth::none(),
            default_headers: http::HeaderMap::new(),
            transport: crate::types::EndpointTransport::Http,
            timeout: std::time::Duration::from_secs(30),
        };
        Model {
            spec: Arc::new(spec),
            endpoint: Arc::new(endpoint),
        }
    }

    /// Feed `data` through the SSE decoder + codec in `chunk`-byte slices
    /// (`chunk == 0` means one shot), returning the raw codec event sequence and
    /// the first error (if any). Events emitted before the error are preserved.
    fn drive_raw_configured(
        model: &Model,
        decode: DecodeFn,
        data: &[u8],
        chunk: usize,
        buffer_ambiguous_compatibility_content: bool,
    ) -> (Vec<StreamEvent>, Option<AiError>) {
        let mut dec = SseDecoder::new();
        let mut builder = ResponseBuilder::new(
            model.spec.id.clone(),
            model.spec.protocol,
            model.spec.pricing.clone(),
        );
        builder.set_buffer_ambiguous_compatibility_content(buffer_ambiguous_compatibility_content);
        let mut out = Vec::new();

        let slices: Vec<&[u8]> = if chunk == 0 {
            vec![data]
        } else {
            data.chunks(chunk).collect()
        };
        for slice in slices {
            let sses = match dec.push(slice).map_err(AiError::Decode) {
                Ok(s) => s,
                Err(e) => return (out, Some(e)),
            };
            for sse in sses {
                match decode(model, &sse, &mut builder) {
                    Ok(evs) => out.extend(evs),
                    Err(e) => return (out, Some(e)),
                }
            }
        }
        match dec.finish().map_err(AiError::Decode) {
            Ok(Some(sse)) => match decode(model, &sse, &mut builder) {
                Ok(evs) => out.extend(evs),
                Err(e) => return (out, Some(e)),
            },
            Ok(None) => {}
            Err(e) => return (out, Some(e)),
        }
        (out, None)
    }

    pub(crate) fn drive_raw(
        model: &Model,
        decode: DecodeFn,
        data: &[u8],
        chunk: usize,
    ) -> (Vec<StreamEvent>, Option<AiError>) {
        drive_raw_configured(model, decode, data, chunk, false)
    }

    /// Like [`drive_raw`] but pipes the events through [`guard`], surfacing
    /// state-machine violations and `PrematureEof`. Returns the guarded event
    /// sequence or the first error encountered by codec or guard.
    pub(crate) async fn drive(
        model: &Model,
        decode: DecodeFn,
        data: &[u8],
        chunk: usize,
    ) -> Result<Vec<StreamEvent>, AiError> {
        let (events, trailing_err) = drive_raw(model, decode, data, chunk);
        collect_guarded(events, trailing_err).await
    }

    /// Drives a codec with ambiguous content-tool compatibility explicitly
    /// enabled, mirroring a lossy production request.
    pub(crate) async fn drive_with_compatibility_buffering(
        model: &Model,
        decode: DecodeFn,
        data: &[u8],
        chunk: usize,
    ) -> Result<Vec<StreamEvent>, AiError> {
        let (events, trailing_err) = drive_raw_configured(model, decode, data, chunk, true);
        collect_guarded(events, trailing_err).await
    }

    async fn collect_guarded(
        events: Vec<StreamEvent>,
        trailing_err: Option<AiError>,
    ) -> Result<Vec<StreamEvent>, AiError> {
        use futures_util::StreamExt;

        let base = futures_util::stream::iter(events.into_iter().map(Ok));
        let mut guarded = if let Some(err) = trailing_err {
            // Preserve the codec error as the stream's terminal item, after its
            // real event prefix, so guard validates the prefix too.
            let tail = futures_util::stream::iter(std::iter::once(Err(err)));
            guard(base.chain(tail))
        } else {
            guard(base)
        };

        let mut collected = Vec::new();
        while let Some(item) = guarded.next().await {
            collected.push(item?);
        }
        Ok(collected)
    }

    /// Extract the single terminal `Finished` response from a guarded sequence.
    pub(crate) fn finished(events: &[StreamEvent]) -> &Response {
        events
            .iter()
            .find_map(|e| match e {
                StreamEvent::Finished(r) => Some(r),
                _ => None,
            })
            .expect("event sequence must contain exactly one Finished")
    }
}
