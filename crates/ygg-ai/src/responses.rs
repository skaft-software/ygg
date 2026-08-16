//! Opaque OpenAI Responses state and native compaction payloads.
//!
//! Responses items deliberately retain their complete JSON object.  They are
//! provider state, not canonical messages: callers must never attempt to map
//! them through [`crate::AssistantPart`] before replaying them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AssistantMessage, CacheRetention, CompatibilityMode, Message, Model, OutputFormat,
    ReasoningConfig, ReasoningMode, ToolDef, Usage, UserMessage,
};

/// An opaque Responses input or output item.
///
/// The value is required to be a JSON object so it can safely be used wherever
/// the Responses API expects an item. Unknown fields are retained verbatim.
#[derive(Clone, Debug, PartialEq)]
pub struct ResponsesItem(serde_json::Value);

/// Error returned when an opaque Responses item is not an object.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("a Responses item must be a JSON object")]
pub struct ResponsesItemError;

impl ResponsesItem {
    /// Creates an opaque item, rejecting JSON scalars and arrays.
    pub fn new(value: serde_json::Value) -> Result<Self, ResponsesItemError> {
        if value.is_object() {
            Ok(Self(value))
        } else {
            Err(ResponsesItemError)
        }
    }

    /// Returns the original object without normalizing any provider fields.
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Consumes this item and returns its original JSON object.
    pub fn into_json(self) -> serde_json::Value {
        self.0
    }

    fn is_compaction(&self) -> bool {
        self.0.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
    }

    fn is_valid_compaction(&self) -> bool {
        self.is_compaction()
            && self
                .0
                .get("encrypted_content")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|content| !content.is_empty())
    }
}

impl Serialize for ResponsesItem {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ResponsesItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// A complete Responses API input window.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponsesInput(Vec<ResponsesItem>);

impl ResponsesInput {
    /// Creates an input window from opaque items.
    pub fn new(items: Vec<ResponsesItem>) -> Self {
        Self(items)
    }

    /// Returns the opaque items in provider order.
    pub fn items(&self) -> &[ResponsesItem] {
        &self.0
    }

    /// Consumes this input window into its opaque items.
    pub fn into_items(self) -> Vec<ResponsesItem> {
        self.0
    }

    /// Returns whether the input contains an authoritative compaction item.
    /// Providers may retain leading output items around the checkpoint, so
    /// callers must not infer compacted provenance from item zero alone.
    pub fn contains_compaction(&self) -> bool {
        self.0.iter().any(ResponsesItem::is_compaction)
    }

    /// Removes image detail hints that the Responses Lite input contract does
    /// not accept. This mirrors Codex request preparation for both messages and
    /// function-call outputs while leaving all other opaque provider fields
    /// untouched.
    pub(crate) fn strip_image_details_for_responses_lite(&mut self) {
        for item in &mut self.0 {
            let value = &mut item.0;
            let Some(object) = value.as_object_mut() else {
                continue;
            };
            let item_type = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let content = match item_type.as_deref() {
                Some("message") => object.get_mut("content"),
                Some("function_call_output") | Some("custom_tool_call_output") => {
                    object.get_mut("output")
                }
                _ => None,
            };
            let Some(content) = content.and_then(serde_json::Value::as_array_mut) else {
                continue;
            };
            for part in content {
                if part.get("type").and_then(serde_json::Value::as_str) == Some("input_image") {
                    if let Some(part) = part.as_object_mut() {
                        part.remove("detail");
                    }
                }
            }
        }
    }

    /// Prunes history before the latest compaction item for ordinary server
    /// compaction chaining. This must not be applied to standalone compact
    /// output, whose entire output is the next canonical input window.
    pub fn prune_for_server_compaction_chaining(&self) -> Self {
        let first = self
            .0
            .iter()
            .rposition(ResponsesItem::is_compaction)
            .unwrap_or(0);
        Self(self.0[first..].to_vec())
    }
}

/// One chronological component of a complete local Responses replay window.
///
/// User messages remain canonical so the normal Responses wire mapping is
/// reused for text, media, and tool results. Assistant turns and native compact
/// checkpoints are represented by their authoritative opaque output and are
/// inserted verbatim.
#[derive(Clone, Debug)]
pub enum ResponsesReplayItem {
    /// A canonical user message, including any tool-result parts.
    User(UserMessage),
    /// A deliberately local assistant boundary, such as the marker persisted
    /// after a failed turn. Durable sessions must only construct this variant
    /// from explicit local provenance; it is not a substitute for missing
    /// authoritative provider output.
    LocalAssistant(AssistantMessage),
    /// Authoritative terminal Responses output for an assistant turn.
    Output(ResponsesOutput),
    /// Complete output from `POST /responses/compact`.
    ///
    /// When this is the first replay item it replaces the earlier input
    /// window verbatim, including the instructions that were compacted.
    Compacted(ResponsesOutput),
}

/// Encodes a complete local Responses replay window.
///
/// The request-level prompt is encoded with the same `system` versus
/// `developer` role selection as an ordinary canonical request. Canonical user
/// and tool-result parts use the normal Responses mapping, while every opaque
/// output item is inserted without normalization. In particular, provider item
/// IDs, function `call_id`s, encrypted reasoning, phase fields, programmatic
/// tool fields, and unknown future fields survive unchanged.
///
/// Callers must only supply route-affine authoritative output for the selected
/// model. This low-level encoder cannot prove provenance; durable session
/// implementations should perform that association before calling it.
pub fn encode_responses_replay(
    model: &Model,
    system: Option<&str>,
    items: &[ResponsesReplayItem],
) -> ResponsesInput {
    crate::protocol::openai_responses::encode_replay_input(model, system, items)
}

/// Encodes ordinary canonical messages using the same input mapper as opaque
/// replay. This is crate-visible so the private HTTP codec cannot drift from
/// the public replay encoder.
pub(crate) fn encode_canonical_responses_input(
    model: &Model,
    system: Option<&str>,
    messages: &[Message],
    compatibility: CompatibilityMode,
) -> ResponsesInput {
    crate::protocol::openai_responses::encode_canonical_input(
        model,
        system,
        messages,
        compatibility,
    )
}

/// Complete output returned by `POST /responses/compact`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ResponsesOutput(Vec<ResponsesItem>);

impl ResponsesOutput {
    /// Creates complete compact output from opaque provider items.
    pub fn new(items: Vec<ResponsesItem>) -> Self {
        Self(items)
    }

    /// Returns the opaque compact output in provider order.
    pub fn items(&self) -> &[ResponsesItem] {
        &self.0
    }

    /// Converts compact output to the next input window without pruning it.
    pub fn into_input(self) -> ResponsesInput {
        ResponsesInput(self.0)
    }

    /// Returns whether native compact output contains exactly one structurally
    /// complete provider checkpoint.
    pub fn has_valid_compaction(&self) -> bool {
        let mut compactions = self.0.iter().filter(|item| item.is_compaction());
        compactions
            .next()
            .is_some_and(ResponsesItem::is_valid_compaction)
            && compactions.next().is_none()
    }

    /// Returns whether the provider output has no replay items.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Responses-specific request state.
///
/// `input` is mutually exclusive with `previous_response_id`: full local
/// replay and server-side chaining are distinct continuation policies.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ResponsesOptions {
    /// Full opaque local replay window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<ResponsesInput>,
    /// Server-side continuation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// Optional native context-management configuration forwarded unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_management: Option<serde_json::Value>,
    /// Responses storage policy. Durable agent replay uses `false`.
    #[serde(default)]
    pub store: bool,
}

impl ResponsesOptions {
    /// Creates durable full-local-replay options.
    ///
    /// Full input is deliberately never mixed with `previous_response_id`, and
    /// storage is explicitly disabled rather than delegated to a provider
    /// default.
    pub fn full_replay(input: ResponsesInput) -> Self {
        Self {
            input: Some(input),
            previous_response_id: None,
            context_management: None,
            store: false,
        }
    }
}

/// Native `POST /responses/compact` request.
#[derive(Clone, Debug, Serialize)]
pub struct ResponsesCompactRequest {
    /// Provider model name.
    pub model: String,
    /// Complete input window to compact.
    pub input: ResponsesInput,
    /// Optional instructions forwarded to the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Function tool schemas active for the replay window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    /// Whether the selected model may emit parallel function calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Responses reasoning controls, shaped exactly like a normal request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
    /// Responses text controls, shaped exactly like a normal request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<serde_json::Value>,
    /// Stable provider prompt-cache key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Stable transport affinity identifier. This is sent only as headers.
    #[serde(skip)]
    pub session_id: Option<String>,
}

impl ResponsesCompactRequest {
    /// Builds a native compact request with the same tool, reasoning, text,
    /// cache-key, and session-affinity controls as an ordinary Responses call.
    #[allow(clippy::too_many_arguments)]
    pub fn for_model(
        model: &Model,
        input: ResponsesInput,
        instructions: Option<String>,
        tools: &[ToolDef],
        reasoning: &ReasoningConfig,
        reasoning_mode: ReasoningMode,
        output_format: &OutputFormat,
        cache_retention: CacheRetention,
        session_id: Option<&str>,
    ) -> Self {
        crate::protocol::openai_responses::build_compact_request(
            model,
            input,
            instructions,
            tools,
            reasoning,
            reasoning_mode,
            output_format,
            cache_retention,
            session_id,
        )
    }
}

/// Native `POST /responses/compact` result.
#[derive(Clone, Debug, Deserialize)]
pub struct ResponsesCompactResponse {
    /// Complete, unpruned opaque output window.
    pub output: ResponsesOutput,
    /// Provider-reported compact-request usage in canonical disjoint buckets.
    #[serde(default, deserialize_with = "deserialize_compact_usage")]
    pub usage: Usage,
}

#[derive(Default, Deserialize)]
struct CompactUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<CompactInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<CompactOutputTokenDetails>,
}

#[derive(Default, Deserialize)]
struct CompactInputTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    cache_write_tokens: u64,
}

#[derive(Default, Deserialize)]
struct CompactOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

fn deserialize_compact_usage<'de, D>(deserializer: D) -> Result<Usage, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(usage) = Option::<CompactUsage>::deserialize(deserializer)? else {
        return Ok(Usage::default());
    };
    let details = usage.input_tokens_details.unwrap_or_default();
    let output_details = usage.output_tokens_details.unwrap_or_default();
    let input_tokens = usage
        .input_tokens
        .saturating_sub(details.cached_tokens)
        .saturating_sub(details.cache_write_tokens);
    normalize_responses_usage(
        input_tokens,
        details.cached_tokens,
        details.cache_write_tokens,
        usage.output_tokens,
        output_details.reasoning_tokens,
    )
    .ok_or_else(|| serde::de::Error::custom("compact usage token total overflow"))
}

pub(crate) fn normalize_responses_usage(
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
) -> Option<Usage> {
    let output_tokens = if reasoning_tokens > output_tokens {
        output_tokens.checked_add(reasoning_tokens)?
    } else {
        output_tokens
    };
    let total_tokens = input_tokens
        .checked_add(cache_read_tokens)?
        .checked_add(cache_write_tokens)?
        .checked_add(output_tokens)?;
    Some(Usage {
        input_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cache_write_1h_tokens: 0,
        output_tokens,
        reasoning_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str) -> ResponsesItem {
        ResponsesItem::new(serde_json::json!({"type": kind, "unknown": {"x": 1}})).unwrap()
    }

    #[test]
    fn items_preserve_unknown_fields_and_reject_non_objects() {
        let item = item("reasoning");
        assert_eq!(item.as_json()["unknown"]["x"], 1);
        assert!(ResponsesItem::new(serde_json::json!(["bad"])).is_err());
    }

    #[test]
    fn responses_lite_strips_only_input_image_detail_hints() {
        let mut input = ResponsesInput::new(vec![
            ResponsesItem::new(serde_json::json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,eA==", "detail": "high", "future": true},
                    {"type": "input_text", "text": "keep", "detail": "future-value"}
                ],
                "unknown": {"detail": "keep"}
            }))
            .unwrap(),
            ResponsesItem::new(serde_json::json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": [
                    {"type": "input_image", "image_url": "data:image/png;base64,eA==", "detail": "low"}
                ]
            }))
            .unwrap(),
        ]);

        input.strip_image_details_for_responses_lite();

        assert!(input.items()[0].as_json()["content"][0]
            .get("detail")
            .is_none());
        assert_eq!(input.items()[0].as_json()["content"][0]["future"], true);
        assert_eq!(
            input.items()[0].as_json()["content"][1]["detail"],
            "future-value"
        );
        assert_eq!(input.items()[0].as_json()["unknown"]["detail"], "keep");
        assert!(input.items()[1].as_json()["output"][0]
            .get("detail")
            .is_none());
    }

    #[test]
    fn chaining_prunes_only_before_latest_compaction() {
        let input = ResponsesInput::new(vec![item("message"), item("compaction"), item("message")]);
        assert_eq!(
            input.prune_for_server_compaction_chaining().items().len(),
            2
        );
        let output = ResponsesOutput::new(input.into_items());
        assert_eq!(output.into_input().items().len(), 3);
    }

    #[test]
    fn full_replay_never_mixes_server_chaining_or_storage() {
        let options = ResponsesOptions::full_replay(ResponsesInput::new(vec![item("message")]));
        assert!(options.input.is_some());
        assert_eq!(options.previous_response_id, None);
        assert!(!options.store);
    }

    #[test]
    fn compact_usage_preserves_reasoning_detail_beyond_the_aggregate() {
        let response: ResponsesCompactResponse = serde_json::from_value(serde_json::json!({
            "output": [{"type": "compaction", "encrypted_content": "opaque"}],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "output_tokens_details": {"reasoning_tokens": 5}
            }
        }))
        .unwrap();
        assert_eq!(response.usage.output_tokens, 8);
        assert_eq!(response.usage.reasoning_tokens, 5);
        assert_eq!(response.usage.total_tokens, 18);
    }

    #[test]
    fn compact_output_requires_one_nonempty_encrypted_checkpoint() {
        for value in [
            serde_json::json!([]),
            serde_json::json!([{"type": "message"}]),
            serde_json::json!([{"type": "compaction"}]),
            serde_json::json!([{"type": "compaction", "encrypted_content": ""}]),
            serde_json::json!([
                {"type": "compaction", "encrypted_content": "one"},
                {"type": "compaction", "encrypted_content": "two"}
            ]),
        ] {
            let output: ResponsesOutput = serde_json::from_value(value).unwrap();
            assert!(!output.has_valid_compaction());
        }

        let output: ResponsesOutput = serde_json::from_value(serde_json::json!([
            {"type": "message", "id": "leading"},
            {"type": "compaction", "encrypted_content": "opaque"},
            {"type": "message", "id": "trailing"}
        ]))
        .unwrap();
        assert!(output.has_valid_compaction());
    }
}
