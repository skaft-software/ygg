//! Opaque OpenAI Responses state and native compaction payloads.
//!
//! Responses items deliberately retain their complete JSON object.  They are
//! provider state, not canonical messages: callers must never attempt to map
//! them through [`crate::AssistantPart`] before replaying them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
}

/// Native `POST /responses/compact` result.
#[derive(Clone, Debug, Deserialize)]
pub struct ResponsesCompactResponse {
    /// Complete, unpruned opaque output window.
    pub output: ResponsesOutput,
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
    fn chaining_prunes_only_before_latest_compaction() {
        let input = ResponsesInput::new(vec![item("message"), item("compaction"), item("message")]);
        assert_eq!(
            input.prune_for_server_compaction_chaining().items().len(),
            2
        );
        let output = ResponsesOutput::new(input.into_items());
        assert_eq!(output.into_input().items().len(), 3);
    }
}
