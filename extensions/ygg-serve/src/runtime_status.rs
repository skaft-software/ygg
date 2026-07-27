//! Bounded, path-free runtime status projections for graphical clients.
//!
//! This module is deliberately independent from the runtime adapter.  An adapter
//! converts authoritative runtime observations into [`RuntimeEvent`] values and
//! applies them to [`RuntimeStatusState`].  Clients receive [`RuntimeSnapshot`]
//! values; they never receive host paths, raw extension configuration, command
//! arguments, environment variables, secret values, or unredacted failures.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    marker::PhantomData,
};

use serde::{
    de::{self, SeqAccess, Visitor},
    ser::SerializeSeq,
    Deserialize, Deserializer, Serialize, Serializer,
};
use thiserror::Error;

/// Maximum UTF-8 bytes in an opaque public identity.
pub const MAX_RUNTIME_ID_BYTES: usize = 128;
/// Maximum UTF-8 bytes in a public display label.
pub const MAX_RUNTIME_LABEL_BYTES: usize = 192;
/// Maximum UTF-8 bytes in a child-agent objective.
pub const MAX_AGENT_OBJECTIVE_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes in a redacted public outcome or failure summary.
pub const MAX_RUNTIME_SUMMARY_BYTES: usize = 2 * 1024;
/// Maximum child agents in one snapshot.
pub const MAX_CHILD_AGENTS: usize = 512;
/// Maximum MCP servers in one snapshot.
pub const MAX_MCP_SERVERS: usize = 128;
/// Maximum trusted catalog entries in one snapshot.
pub const MAX_CATALOG_ENTRIES: usize = 256;
/// Maximum contributions exposed by one trusted catalog entry.
pub const MAX_ENTRY_CONTRIBUTIONS: usize = 64;
/// Maximum LSP servers in one snapshot.
pub const MAX_LSP_SERVERS: usize = 256;
/// Maximum context categories.
pub const MAX_CONTEXT_CATEGORIES: usize = 16;
/// Maximum allow or deny rules in one policy.
pub const MAX_POLICY_RULES: usize = 128;
/// Maximum diagnostic count accepted for any one severity.
pub const MAX_DIAGNOSTICS_PER_SEVERITY: u32 = 1_000_000;

/// A vector whose deserializer stops before accepting more than `MAX` items.
///
/// This type is used at every public collection boundary so a hostile payload
/// cannot make a nominally bounded DTO allocate an unbounded list first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BoundedVec<T, const MAX: usize>(Vec<T>);

impl<T, const MAX: usize> Default for BoundedVec<T, MAX> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T, const MAX: usize> BoundedVec<T, MAX> {
    /// Constructs a bounded vector.
    pub fn try_new(items: Vec<T>) -> Result<Self, RuntimeStatusError> {
        if items.len() > MAX {
            return Err(RuntimeStatusError::LimitExceeded {
                field: "boundedVector",
                limit: MAX,
            });
        }
        Ok(Self(items))
    }

    /// Returns the contained slice.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Returns the number of items.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether there are no items.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Iterates over the contained items.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// Consumes the wrapper.
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T, const MAX: usize> TryFrom<Vec<T>> for BoundedVec<T, MAX> {
    type Error = RuntimeStatusError;

    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl<T, const MAX: usize> IntoIterator for BoundedVec<T, MAX> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T, const MAX: usize> IntoIterator for &'a BoundedVec<T, MAX> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T: Serialize, const MAX: usize> Serialize for BoundedVec<T, MAX> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for item in &self.0 {
            sequence.serialize_element(item)?;
        }
        sequence.end()
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVecVisitor<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for BoundedVecVisitor<T, MAX> {
            type Value = BoundedVec<T, MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence containing at most {MAX} items")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let capacity = sequence.size_hint().unwrap_or(0).min(MAX);
                let mut items = Vec::with_capacity(capacity);
                while let Some(item) = sequence.next_element()? {
                    if items.len() == MAX {
                        return Err(de::Error::custom(format!(
                            "sequence exceeds the {MAX}-item limit"
                        )));
                    }
                    items.push(item);
                }
                Ok(BoundedVec(items))
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX>(PhantomData))
    }
}

/// Bounded, single-line, redacted public text.
///
/// Absolute host-path spellings are rejected as a final safety net.  Adapters
/// must still redact failures and outcomes before constructing this value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PublicText<const MAX: usize>(String);

impl<const MAX: usize> PublicText<MAX> {
    /// Constructs validated public text.
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeStatusError> {
        let value = value.into();
        validate_public_text(&value, MAX)?;
        Ok(Self(value))
    }

    /// Returns the public text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for PublicText<MAX> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

fn validate_public_text(value: &str, max: usize) -> Result<(), RuntimeStatusError> {
    if value.is_empty() || value.len() > max {
        return Err(RuntimeStatusError::InvalidPublicValue(
            "public text is empty or exceeds its UTF-8 byte limit",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(RuntimeStatusError::InvalidPublicValue(
            "public text contains control characters",
        ));
    }
    if contains_absolute_host_path(value) {
        return Err(RuntimeStatusError::InvalidPublicValue(
            "public text contains an absolute host path",
        ));
    }
    Ok(())
}

fn contains_absolute_host_path(value: &str) -> bool {
    if value.contains("file://") || value.contains("\\\\") {
        return true;
    }
    value.split_whitespace().any(|word| {
        let candidate = word.trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';'
            )
        });
        candidate.starts_with('/')
            || candidate.starts_with("~/")
            || candidate
                .as_bytes()
                .get(1)
                .is_some_and(|byte| *byte == b':')
                && candidate
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphabetic)
    })
}

/// Stable opaque public identity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RuntimeId(String);

impl RuntimeId {
    /// Constructs an opaque identity with no path separators.
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeStatusError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_RUNTIME_ID_BYTES {
            return Err(RuntimeStatusError::InvalidPublicValue(
                "runtime id is empty or too long",
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
        {
            return Err(RuntimeStatusError::InvalidPublicValue(
                "runtime id contains a non-opaque character",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the identity spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RuntimeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Public display label.
pub type RuntimeLabel = PublicText<MAX_RUNTIME_LABEL_BYTES>;
/// Public child-agent objective.
pub type AgentObjective = PublicText<MAX_AGENT_OBJECTIVE_BYTES>;
/// Redacted public outcome or failure summary.
pub type RuntimeSummary = PublicText<MAX_RUNTIME_SUMMARY_BYTES>;

/// Error returned when a projection or transition violates the protocol.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum RuntimeStatusError {
    /// A public value is malformed.
    #[error("{0}")]
    InvalidPublicValue(&'static str),
    /// A bounded collection exceeded its limit.
    #[error("{field} exceeds its {limit}-item limit")]
    LimitExceeded {
        /// Stable public field name.
        field: &'static str,
        /// Maximum accepted item count.
        limit: usize,
    },
    /// An identity already exists.
    #[error("{kind} identity already exists")]
    DuplicateIdentity {
        /// Public entity category.
        kind: &'static str,
    },
    /// An identity was not found.
    #[error("{kind} identity was not found")]
    UnknownIdentity {
        /// Public entity category.
        kind: &'static str,
    },
    /// A state transition is not legal.
    #[error("illegal {kind} state transition")]
    IllegalTransition {
        /// Public entity category.
        kind: &'static str,
    },
    /// A timestamp moved backwards.
    #[error("{kind} timestamp moved backwards")]
    TimestampRegression {
        /// Public entity category.
        kind: &'static str,
    },
    /// A snapshot or event contains contradictory facts.
    #[error("contradictory {field} facts")]
    ContradictoryFacts {
        /// Stable public field name.
        field: &'static str,
    },
    /// A monotonic revision or generation regressed.
    #[error("{field} must increase monotonically")]
    RevisionRegression {
        /// Stable public field name.
        field: &'static str,
    },
    /// An in-progress operation conflicts with another operation.
    #[error("{kind} operation is already active")]
    OperationInProgress {
        /// Public operation category.
        kind: &'static str,
    },
}

/// Whether an event changed state or was an exact replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The event changed authoritative projected state.
    Applied,
    /// The event exactly repeated a durable event already reflected in state.
    Replay,
}

/// Child-agent lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildAgentState {
    /// Created but not yet running.
    Queued,
    /// Actively executing.
    Running,
    /// Waiting on a bounded dependency or parent coordination.
    Waiting,
    /// Finished successfully.
    Succeeded,
    /// Finished unsuccessfully.
    Failed,
    /// Cancelled before completion.
    Cancelled,
}

impl ChildAgentState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Path-free child-agent status with explicit parentage and timing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildAgentStatus {
    /// Stable child identity.
    pub id: RuntimeId,
    /// Parent agent identity, absent only for a root runtime agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<RuntimeId>,
    /// Redacted user-visible objective.
    pub objective: AgentObjective,
    /// Current lifecycle state.
    pub state: ChildAgentState,
    /// Queue timestamp.
    pub queued_at_ms: u64,
    /// First running timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Most recent lifecycle timestamp.
    pub updated_at_ms: u64,
    /// Terminal timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// Redacted terminal outcome; present exactly for terminal states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RuntimeSummary>,
}

/// MCP server lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpServerState {
    /// Trusted host configuration is known but no process is starting.
    Configured,
    /// The host is starting or handshaking with the server.
    Starting,
    /// The server is ready for requests.
    Ready,
    /// The server failed; a redacted summary is available.
    Failed,
    /// The server is intentionally stopped.
    Stopped,
}

/// Path-free MCP server status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpServerStatus {
    /// Stable server identity.
    pub id: RuntimeId,
    /// Public display name.
    pub label: RuntimeLabel,
    /// Current lifecycle state.
    pub state: McpServerState,
    /// Number of explicit restart requests.
    pub restart_count: u32,
    /// Configuration observation timestamp.
    pub configured_at_ms: u64,
    /// Most recent lifecycle timestamp.
    pub updated_at_ms: u64,
    /// Redacted failure summary, present exactly while failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<RuntimeSummary>,
}

/// Trusted runtime catalog entry kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrustedCatalogKind {
    /// A trusted skill package.
    Skill,
    /// A trusted extension package.
    Extension,
}

/// Safe contribution kind exposed by a trusted catalog entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContributionKind {
    /// A skill workflow.
    Skill,
    /// A structured tool.
    Tool,
    /// A command palette action.
    Command,
    /// An MCP server definition.
    McpServer,
    /// A resolved theme.
    Theme,
    /// A language-server integration.
    LanguageServer,
}

/// One inert, path-free contribution projection.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogContribution {
    /// Contribution category.
    pub kind: ContributionKind,
    /// Stable contribution identity.
    pub id: RuntimeId,
    /// Public display label.
    pub label: RuntimeLabel,
}

/// One trusted skill or extension catalog entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedCatalogEntry {
    /// Stable entry identity.
    pub id: RuntimeId,
    /// Public display label.
    pub label: RuntimeLabel,
    /// Skill or extension.
    pub kind: TrustedCatalogKind,
    /// Whether the trusted host currently enables the entry.
    pub enabled: bool,
    /// Bounded inert contributions.
    pub contributions: BoundedVec<CatalogContribution, MAX_ENTRY_CONTRIBUTIONS>,
}

/// Catalog reload lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CatalogReloadStatus {
    /// No reload has been attempted.
    Idle,
    /// A reload is active against the prior committed generation.
    Running {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Generation retained while the reload is active.
        retained_generation: u64,
        /// Start timestamp.
        started_at_ms: u64,
    },
    /// A reload atomically committed.
    Succeeded {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Newly committed generation.
        generation: u64,
        /// Start timestamp.
        started_at_ms: u64,
        /// Finish timestamp.
        finished_at_ms: u64,
    },
    /// A reload failed and retained the prior generation and entries.
    Failed {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Generation retained after failure.
        retained_generation: u64,
        /// Start timestamp.
        started_at_ms: u64,
        /// Finish timestamp.
        finished_at_ms: u64,
        /// Redacted failure summary.
        failure: RuntimeSummary,
    },
}

/// Atomic trusted skill and extension catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedCatalogStatus {
    /// Monotonic committed generation.
    pub generation: u64,
    /// Last successful catalog mutation timestamp.
    pub updated_at_ms: u64,
    /// Last or active reload status.
    pub reload: CatalogReloadStatus,
    /// Committed entries.  Running and failed reloads retain this exact list.
    pub entries: BoundedVec<TrustedCatalogEntry, MAX_CATALOG_ENTRIES>,
}

impl Default for TrustedCatalogStatus {
    fn default() -> Self {
        Self {
            generation: 0,
            updated_at_ms: 0,
            reload: CatalogReloadStatus::Idle,
            entries: BoundedVec::default(),
        }
    }
}

/// LSP lifecycle state for one project/language pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LspServerState {
    /// Trusted project and language configuration is known.
    Configured,
    /// A language server is starting.
    Starting,
    /// The language server is ready.
    Ready,
    /// The language server failed.
    Failed,
    /// The language server is intentionally stopped.
    Stopped,
}

/// Bounded diagnostic counts, without source paths or message text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiagnosticCounts {
    /// Error count.
    pub errors: u32,
    /// Warning count.
    pub warnings: u32,
    /// Informational count.
    pub information: u32,
    /// Hint count.
    pub hints: u32,
}

impl DiagnosticCounts {
    fn validate(self) -> Result<(), RuntimeStatusError> {
        if [self.errors, self.warnings, self.information, self.hints]
            .into_iter()
            .any(|count| count > MAX_DIAGNOSTICS_PER_SEVERITY)
        {
            return Err(RuntimeStatusError::LimitExceeded {
                field: "diagnostics",
                limit: MAX_DIAGNOSTICS_PER_SEVERITY as usize,
            });
        }
        Ok(())
    }
}

/// Path-free status for one project/language server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LspServerStatus {
    /// Stable trusted project identity.
    pub project_id: RuntimeId,
    /// Stable language identity, such as `rust`.
    pub language_id: RuntimeId,
    /// Current lifecycle state.
    pub state: LspServerState,
    /// Number of explicit restart requests.
    pub restart_count: u32,
    /// Configuration observation timestamp.
    pub configured_at_ms: u64,
    /// Most recent lifecycle timestamp.
    pub updated_at_ms: u64,
    /// Monotonic diagnostic publication revision.
    pub diagnostic_revision: u64,
    /// Aggregate diagnostic counts.
    pub diagnostics: DiagnosticCounts,
    /// Redacted failure summary, present exactly while failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<RuntimeSummary>,
}

/// Stable context accounting category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContextCategory {
    /// Runtime and provider system instructions.
    System,
    /// Trusted project instructions.
    ProjectInstructions,
    /// User and assistant conversation history.
    Conversation,
    /// Public tool calls and results.
    ToolResults,
    /// User-approved attachments and documents.
    Attachments,
    /// Compaction summaries.
    CompactionSummaries,
    /// Other explicitly measured public context.
    Other,
}

/// Token total for one context category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextCategoryTotal {
    /// Category.
    pub category: ContextCategory,
    /// Host-measured or host-estimated token count.
    pub tokens: u64,
}

/// Reconciled context totals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextTotals {
    /// Unique category totals.
    pub categories: BoundedVec<ContextCategoryTotal, MAX_CONTEXT_CATEGORIES>,
    /// Exact checked sum of all category token counts.
    pub total_tokens: u64,
}

impl ContextTotals {
    /// Constructs totals and rejects duplicate categories or an incorrect sum.
    pub fn try_new(
        categories: Vec<ContextCategoryTotal>,
        total_tokens: u64,
    ) -> Result<Self, RuntimeStatusError> {
        let categories = BoundedVec::try_new(categories)?;
        let totals = Self {
            categories,
            total_tokens,
        };
        totals.validate()?;
        Ok(totals)
    }

    fn validate(&self) -> Result<(), RuntimeStatusError> {
        let mut seen = BTreeSet::new();
        let mut sum = 0_u64;
        for category in &self.categories {
            if !seen.insert(category.category) {
                return Err(RuntimeStatusError::DuplicateIdentity {
                    kind: "context category",
                });
            }
            sum =
                sum.checked_add(category.tokens)
                    .ok_or(RuntimeStatusError::ContradictoryFacts {
                        field: "contextTotals",
                    })?;
        }
        if sum != self.total_tokens {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "contextTotals",
            });
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ContextTotals {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            categories: BoundedVec<ContextCategoryTotal, MAX_CONTEXT_CATEGORIES>,
            total_tokens: u64,
        }

        let wire = Wire::deserialize(deserializer)?;
        let totals = ContextTotals {
            categories: wire.categories,
            total_tokens: wire.total_tokens,
        };
        totals.validate().map_err(de::Error::custom)?;
        Ok(totals)
    }
}

/// Active compaction projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveCompaction {
    /// Stable compaction identity.
    pub id: RuntimeId,
    /// Exact context totals at start.
    pub before: ContextTotals,
    /// Start timestamp.
    pub started_at_ms: u64,
}

/// Completed compaction projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompletedCompaction {
    /// Stable compaction identity.
    pub id: RuntimeId,
    /// Exact context totals at start.
    pub before: ContextTotals,
    /// Exact context totals after completion.
    pub after: ContextTotals,
    /// Exact `before.total_tokens - after.total_tokens`.
    pub reclaimed_tokens: u64,
    /// Start timestamp.
    pub started_at_ms: u64,
    /// Finish timestamp.
    pub finished_at_ms: u64,
}

/// Replayable context accounting state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContextStatus {
    /// Current reconciled totals.
    pub current: ContextTotals,
    /// Most recent ordinary update or compaction finish timestamp.
    pub updated_at_ms: u64,
    /// Active compaction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_compaction: Option<ActiveCompaction>,
    /// Last completed compaction, retained for idempotent event replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_compaction: Option<CompletedCompaction>,
}

impl ContextStatus {
    /// Constructs an empty context state.
    pub fn empty() -> Self {
        Self {
            current: ContextTotals {
                categories: BoundedVec::default(),
                total_tokens: 0,
            },
            updated_at_ms: 0,
            active_compaction: None,
            last_compaction: None,
        }
    }
}

/// Default decision for an allow/deny rule set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleDefault {
    /// Allow unless an exact deny rule matches.
    Allow,
    /// Deny unless an exact allow rule matches.
    Deny,
}

/// Validated bounded exact-match allow/deny rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSet<T> {
    /// Default when no exact rule matches.
    pub default: RuleDefault,
    /// Exact allowed values.
    pub allow: BoundedVec<T, MAX_POLICY_RULES>,
    /// Exact denied values. Deny wins if host code evaluates a malformed set,
    /// though malformed overlaps are rejected at construction/deserialization.
    pub deny: BoundedVec<T, MAX_POLICY_RULES>,
}

impl<T: Clone + Ord> RuleSet<T> {
    /// Constructs a rule set and rejects duplicates or allow/deny overlap.
    pub fn try_new(
        default: RuleDefault,
        allow: Vec<T>,
        deny: Vec<T>,
    ) -> Result<Self, RuntimeStatusError> {
        let rules = Self {
            default,
            allow: BoundedVec::try_new(allow)?,
            deny: BoundedVec::try_new(deny)?,
        };
        rules.validate()?;
        Ok(rules)
    }

    fn validate(&self) -> Result<(), RuntimeStatusError> {
        let allow = self
            .allow
            .as_slice()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let deny = self
            .deny
            .as_slice()
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if allow.len() != self.allow.len() || deny.len() != self.deny.len() {
            return Err(RuntimeStatusError::DuplicateIdentity {
                kind: "policy rule",
            });
        }
        if !allow.is_disjoint(&deny) {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "policyRules",
            });
        }
        Ok(())
    }

    fn allows(&self, value: &T) -> bool {
        if self.deny.as_slice().contains(value) {
            return false;
        }
        if self.allow.as_slice().contains(value) {
            return true;
        }
        self.default == RuleDefault::Allow
    }
}

impl<'de, T> Deserialize<'de> for RuleSet<T>
where
    T: Clone + Ord + Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        #[serde(bound(deserialize = "T: Deserialize<'de>"))]
        struct Wire<T> {
            default: RuleDefault,
            allow: BoundedVec<T, MAX_POLICY_RULES>,
            deny: BoundedVec<T, MAX_POLICY_RULES>,
        }

        let wire = Wire::<T>::deserialize(deserializer)?;
        let rules = RuleSet {
            default: wire.default,
            allow: wire.allow,
            deny: wire.deny,
        };
        rules.validate().map_err(de::Error::custom)?;
        Ok(rules)
    }
}

/// Exact command executable name without arguments or path separators.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CommandName(String);

impl CommandName {
    /// Constructs a safe executable token.
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeStatusError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_RUNTIME_LABEL_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-')
            })
        {
            return Err(RuntimeStatusError::InvalidPublicValue(
                "command name must be a bounded executable token without arguments or paths",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the executable token.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CommandName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Normalized exact domain name without URL, port, path, or wildcard syntax.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DomainName(String);

impl DomainName {
    /// Constructs a lowercase ASCII domain.
    pub fn new(value: impl Into<String>) -> Result<Self, RuntimeStatusError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 253
            || value != value.to_ascii_lowercase()
            || value.starts_with('.')
            || value.ends_with('.')
            || value.contains("..")
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
            || value
                .split('.')
                .any(|label| label.is_empty() || label.starts_with('-') || label.ends_with('-'))
        {
            return Err(RuntimeStatusError::InvalidPublicValue(
                "domain rule must be an exact lowercase hostname without URL, port, path, or wildcard",
            ));
        }
        Ok(Self(value))
    }

    /// Returns the exact hostname.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DomainName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Consequence when policy enforcement is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnavailableConsequence {
    /// The corresponding feature is disabled by the host.
    FeatureBlocked,
    /// The host cannot attest to behavior; clients must present it as unknown.
    HostBehaviorUnknown,
}

/// Result of evaluating a projected policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyEvaluation {
    /// Authoritative enforcement permits the operation.
    Allowed,
    /// Authoritative enforcement blocks the operation.
    Blocked,
    /// Authoritative enforcement requires a user approval decision.
    ApprovalRequired,
    /// No authoritative enforcement is available; fallback is explicit.
    Unavailable(UnavailableConsequence),
}

/// Filesystem access requested by an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilesystemAccess {
    /// No filesystem access.
    None,
    /// Read trusted project files only.
    TrustedProjectRead,
    /// Read and write trusted project files only.
    TrustedProjectReadWrite,
}

/// Authoritative filesystem policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum FilesystemPolicy {
    /// Host enforcement is active with this maximum access consequence.
    Enforced {
        /// Maximum enforced access.
        access: FilesystemAccess,
    },
    /// The host cannot attest to filesystem enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl FilesystemPolicy {
    /// Evaluates requested filesystem access.
    pub fn evaluate(&self, requested: FilesystemAccess) -> PolicyEvaluation {
        match self {
            Self::Enforced { access } => {
                let rank = |value| match value {
                    FilesystemAccess::None => 0,
                    FilesystemAccess::TrustedProjectRead => 1,
                    FilesystemAccess::TrustedProjectReadWrite => 2,
                };
                if rank(requested) <= rank(*access) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

/// Authoritative tool policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ToolPolicy {
    /// Exact tool identity rules are actively enforced.
    Enforced {
        /// Exact allow/deny rules.
        rules: RuleSet<RuntimeId>,
    },
    /// The host cannot attest to tool enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl ToolPolicy {
    /// Evaluates one exact tool identity.
    pub fn evaluate(&self, tool_id: &RuntimeId) -> PolicyEvaluation {
        match self {
            Self::Enforced { rules } => {
                if rules.allows(tool_id) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

/// Authoritative command policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CommandPolicy {
    /// Exact executable-name rules are actively enforced.
    Enforced {
        /// Exact allow/deny rules without arguments or paths.
        rules: RuleSet<CommandName>,
    },
    /// The host cannot attest to command enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl CommandPolicy {
    /// Evaluates one exact executable name.
    pub fn evaluate(&self, command: &CommandName) -> PolicyEvaluation {
        match self {
            Self::Enforced { rules } => {
                if rules.allows(command) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

/// Enforced remote-read consequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteReadConsequence {
    /// All remote reads are blocked.
    Blocked,
    /// Exact-domain allow/deny rules decide remote reads.
    DomainRules {
        /// Exact domain rules.
        domains: RuleSet<DomainName>,
    },
}

/// Authoritative remote-read policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RemoteReadPolicy {
    /// Remote-read enforcement is active.
    Enforced {
        /// Enforced consequence.
        consequence: RemoteReadConsequence,
    },
    /// The host cannot attest to remote-read enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl RemoteReadPolicy {
    /// Evaluates one exact remote hostname.
    pub fn evaluate(&self, domain: &DomainName) -> PolicyEvaluation {
        match self {
            Self::Enforced {
                consequence: RemoteReadConsequence::Blocked,
            } => PolicyEvaluation::Blocked,
            Self::Enforced {
                consequence: RemoteReadConsequence::DomainRules { domains },
            } => {
                if domains.allows(domain) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

/// Enforced process-network consequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProcessNetworkConsequence {
    /// Child-process network access is blocked.
    Blocked,
    /// Exact-domain rules decide child-process network access.
    DomainRules {
        /// Exact domain rules.
        domains: RuleSet<DomainName>,
    },
}

/// Authoritative process-network policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ProcessNetworkPolicy {
    /// Process-network enforcement is active.
    Enforced {
        /// Enforced consequence.
        consequence: ProcessNetworkConsequence,
    },
    /// The host cannot attest to process-network enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl ProcessNetworkPolicy {
    /// Evaluates one exact remote hostname for a child process.
    pub fn evaluate(&self, domain: &DomainName) -> PolicyEvaluation {
        match self {
            Self::Enforced {
                consequence: ProcessNetworkConsequence::Blocked,
            } => PolicyEvaluation::Blocked,
            Self::Enforced {
                consequence: ProcessNetworkConsequence::DomainRules { domains },
            } => {
                if domains.allows(domain) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

/// Operation category that may require approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalOperation {
    /// Filesystem write.
    FilesystemWrite,
    /// Tool invocation.
    Tool,
    /// Command execution.
    Command,
    /// Remote read.
    RemoteRead,
    /// Child-process network access.
    ProcessNetwork,
    /// Named secret access.
    SecretAccess,
}

/// Enforced approval consequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ApprovalConsequence {
    /// No listed operation requires host approval.
    Never,
    /// Listed operations are blocked until the host records approval.
    RequiredFor {
        /// Unique operation categories requiring approval.
        operations: BoundedVec<ApprovalOperation, 8>,
    },
}

impl ApprovalConsequence {
    fn validate(&self) -> Result<(), RuntimeStatusError> {
        match self {
            Self::Never => Ok(()),
            Self::RequiredFor { operations } => {
                let unique = operations.iter().copied().collect::<BTreeSet<_>>();
                if operations.is_empty() || unique.len() != operations.len() {
                    return Err(RuntimeStatusError::ContradictoryFacts {
                        field: "approvalOperations",
                    });
                }
                Ok(())
            }
        }
    }
}

/// Authoritative approval policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ApprovalPolicy {
    /// Approval enforcement is active.
    Enforced {
        /// Enforced approval consequence.
        consequence: ApprovalConsequence,
    },
    /// The host cannot attest to approval enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl ApprovalPolicy {
    /// Evaluates whether one operation requires approval.
    pub fn evaluate(&self, operation: ApprovalOperation) -> PolicyEvaluation {
        match self {
            Self::Enforced {
                consequence: ApprovalConsequence::Never,
            } => PolicyEvaluation::Allowed,
            Self::Enforced {
                consequence: ApprovalConsequence::RequiredFor { operations },
            } => {
                if operations.as_slice().contains(&operation) {
                    PolicyEvaluation::ApprovalRequired
                } else {
                    PolicyEvaluation::Allowed
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

impl<'de> Deserialize<'de> for ApprovalPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(
            tag = "status",
            rename_all = "camelCase",
            rename_all_fields = "camelCase",
            deny_unknown_fields
        )]
        enum Wire {
            Enforced {
                consequence: ApprovalConsequence,
            },
            Unavailable {
                reason: RuntimeSummary,
                consequence: UnavailableConsequence,
            },
        }

        let policy = match Wire::deserialize(deserializer)? {
            Wire::Enforced { consequence } => {
                consequence.validate().map_err(de::Error::custom)?;
                Self::Enforced { consequence }
            }
            Wire::Unavailable {
                reason,
                consequence,
            } => Self::Unavailable {
                reason,
                consequence,
            },
        };
        Ok(policy)
    }
}

/// Enforced secret-access consequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum SecretsConsequence {
    /// All secret access is blocked.
    Blocked,
    /// Only exact opaque grant identities are accessible; values are never
    /// projected.
    NamedGrants {
        /// Non-empty exact grant identities.
        grants: BoundedVec<RuntimeId, MAX_POLICY_RULES>,
    },
}

impl SecretsConsequence {
    fn validate(&self) -> Result<(), RuntimeStatusError> {
        if let Self::NamedGrants { grants } = self {
            let unique = grants.iter().cloned().collect::<BTreeSet<_>>();
            if grants.is_empty() || unique.len() != grants.len() {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "secretGrants",
                });
            }
        }
        Ok(())
    }
}

/// Authoritative secrets policy or an explicit lack of enforcement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SecretsPolicy {
    /// Secret access enforcement is active.
    Enforced {
        /// Enforced consequence containing only opaque grant identities.
        consequence: SecretsConsequence,
    },
    /// The host cannot attest to secret-access enforcement.
    Unavailable {
        /// Redacted reason.
        reason: RuntimeSummary,
        /// Explicit effective fallback.
        consequence: UnavailableConsequence,
    },
}

impl SecretsPolicy {
    /// Evaluates one opaque named secret grant.
    pub fn evaluate(&self, grant: &RuntimeId) -> PolicyEvaluation {
        match self {
            Self::Enforced {
                consequence: SecretsConsequence::Blocked,
            } => PolicyEvaluation::Blocked,
            Self::Enforced {
                consequence: SecretsConsequence::NamedGrants { grants },
            } => {
                if grants.as_slice().contains(grant) {
                    PolicyEvaluation::Allowed
                } else {
                    PolicyEvaluation::Blocked
                }
            }
            Self::Unavailable { consequence, .. } => PolicyEvaluation::Unavailable(*consequence),
        }
    }
}

impl<'de> Deserialize<'de> for SecretsPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(
            tag = "status",
            rename_all = "camelCase",
            rename_all_fields = "camelCase",
            deny_unknown_fields
        )]
        enum Wire {
            Enforced {
                consequence: SecretsConsequence,
            },
            Unavailable {
                reason: RuntimeSummary,
                consequence: UnavailableConsequence,
            },
        }

        let policy = match Wire::deserialize(deserializer)? {
            Wire::Enforced { consequence } => {
                consequence.validate().map_err(de::Error::custom)?;
                Self::Enforced { consequence }
            }
            Wire::Unavailable {
                reason,
                consequence,
            } => Self::Unavailable {
                reason,
                consequence,
            },
        };
        Ok(policy)
    }
}

/// Complete authoritative runtime policy projection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyStatus {
    /// Monotonic host policy revision.
    pub revision: u64,
    /// Observation timestamp.
    pub observed_at_ms: u64,
    /// Filesystem consequence or explicit unavailability.
    pub filesystem: FilesystemPolicy,
    /// Tool consequence or explicit unavailability.
    pub tools: ToolPolicy,
    /// Command consequence or explicit unavailability.
    pub commands: CommandPolicy,
    /// Remote-read consequence or explicit unavailability.
    pub remote_read: RemoteReadPolicy,
    /// Child-process network consequence or explicit unavailability.
    pub process_network: ProcessNetworkPolicy,
    /// Approval consequence or explicit unavailability.
    pub approvals: ApprovalPolicy,
    /// Secret-access consequence or explicit unavailability.
    pub secrets: SecretsPolicy,
}

/// Durable runtime event accepted by [`RuntimeStatusState`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RuntimeEvent {
    /// Registers a queued child agent.
    ChildAgentSpawned {
        /// Stable child identity.
        id: RuntimeId,
        /// Parent identity, absent for a root runtime agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_id: Option<RuntimeId>,
        /// Redacted public objective.
        objective: AgentObjective,
        /// Queue timestamp.
        queued_at_ms: u64,
    },
    /// Changes a child-agent lifecycle state.
    ChildAgentTransitioned {
        /// Stable child identity.
        id: RuntimeId,
        /// New state.
        state: ChildAgentState,
        /// Transition timestamp.
        at_ms: u64,
        /// Redacted outcome, present exactly for a terminal state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<RuntimeSummary>,
    },
    /// Registers a configured MCP server.
    McpConfigured {
        /// Stable server identity.
        id: RuntimeId,
        /// Public display label.
        label: RuntimeLabel,
        /// Observation timestamp.
        at_ms: u64,
    },
    /// Changes MCP lifecycle state. `starting` is only legal from `configured`;
    /// later starts use [`RuntimeEvent::McpRestarted`].
    McpTransitioned {
        /// Stable server identity.
        id: RuntimeId,
        /// New lifecycle state.
        state: McpServerState,
        /// Transition timestamp.
        at_ms: u64,
        /// Redacted failure, present exactly for `failed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<RuntimeSummary>,
    },
    /// Explicitly restarts a ready, failed, or stopped MCP server.
    McpRestarted {
        /// Stable server identity.
        id: RuntimeId,
        /// Restart timestamp.
        at_ms: u64,
    },
    /// Begins an atomic trusted-catalog reload.
    CatalogReloadStarted {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Start timestamp.
        at_ms: u64,
    },
    /// Atomically commits a complete trusted catalog generation.
    CatalogReloadSucceeded {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Complete candidate entries.
        entries: BoundedVec<TrustedCatalogEntry, MAX_CATALOG_ENTRIES>,
        /// Finish timestamp.
        at_ms: u64,
    },
    /// Fails an atomic catalog reload while retaining the prior generation.
    CatalogReloadFailed {
        /// Stable reload identity.
        reload_id: RuntimeId,
        /// Redacted failure summary.
        failure: RuntimeSummary,
        /// Finish timestamp.
        at_ms: u64,
    },
    /// Changes one committed catalog entry's enabled state.
    CatalogEntryEnabled {
        /// Stable catalog entry identity.
        entry_id: RuntimeId,
        /// New enabled state.
        enabled: bool,
        /// Expected current generation, preventing lost updates.
        expected_generation: u64,
        /// Mutation timestamp.
        at_ms: u64,
    },
    /// Registers a configured LSP server for one project/language pair.
    LspConfigured {
        /// Stable trusted project identity.
        project_id: RuntimeId,
        /// Stable language identity.
        language_id: RuntimeId,
        /// Observation timestamp.
        at_ms: u64,
    },
    /// Changes an LSP lifecycle state.
    LspTransitioned {
        /// Stable trusted project identity.
        project_id: RuntimeId,
        /// Stable language identity.
        language_id: RuntimeId,
        /// New lifecycle state.
        state: LspServerState,
        /// Transition timestamp.
        at_ms: u64,
        /// Redacted failure, present exactly for `failed`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<RuntimeSummary>,
    },
    /// Explicitly restarts a ready, failed, or stopped LSP server.
    LspRestarted {
        /// Stable trusted project identity.
        project_id: RuntimeId,
        /// Stable language identity.
        language_id: RuntimeId,
        /// Restart timestamp.
        at_ms: u64,
    },
    /// Publishes aggregate diagnostics from a ready LSP server.
    LspDiagnosticsPublished {
        /// Stable trusted project identity.
        project_id: RuntimeId,
        /// Stable language identity.
        language_id: RuntimeId,
        /// Monotonic publication revision.
        revision: u64,
        /// Aggregate counts.
        counts: DiagnosticCounts,
        /// Publication timestamp.
        at_ms: u64,
    },
    /// Replaces ordinary context totals while no compaction is active.
    ContextUpdated {
        /// Reconciled totals.
        totals: ContextTotals,
        /// Observation timestamp.
        at_ms: u64,
    },
    /// Starts a replayable compaction against current totals.
    CompactionStarted {
        /// Stable compaction identity.
        id: RuntimeId,
        /// Exact current totals.
        before: ContextTotals,
        /// Start timestamp.
        at_ms: u64,
    },
    /// Finishes a replayable compaction and reconciles the reclaimed total.
    CompactionFinished {
        /// Stable compaction identity.
        id: RuntimeId,
        /// Exact resulting totals.
        after: ContextTotals,
        /// Exact number of reclaimed tokens.
        reclaimed_tokens: u64,
        /// Finish timestamp.
        at_ms: u64,
    },
    /// Publishes a complete authoritative policy status.
    PolicyPublished {
        /// Complete policy status.
        policy: Box<RuntimePolicyStatus>,
    },
}

/// Complete path-free runtime snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    /// Child-agent statuses.
    pub child_agents: BoundedVec<ChildAgentStatus, MAX_CHILD_AGENTS>,
    /// MCP server statuses.
    pub mcp_servers: BoundedVec<McpServerStatus, MAX_MCP_SERVERS>,
    /// Atomic trusted skill/extension catalog.
    pub catalog: TrustedCatalogStatus,
    /// Project/language LSP statuses.
    pub lsp_servers: BoundedVec<LspServerStatus, MAX_LSP_SERVERS>,
    /// Reconciled context status.
    pub context: ContextStatus,
    /// Latest authoritative policy status, absent until observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<RuntimePolicyStatus>,
}

impl<'de> Deserialize<'de> for RuntimeSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Wire {
            child_agents: BoundedVec<ChildAgentStatus, MAX_CHILD_AGENTS>,
            mcp_servers: BoundedVec<McpServerStatus, MAX_MCP_SERVERS>,
            catalog: TrustedCatalogStatus,
            lsp_servers: BoundedVec<LspServerStatus, MAX_LSP_SERVERS>,
            context: ContextStatus,
            #[serde(default)]
            policy: Option<RuntimePolicyStatus>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let snapshot = Self {
            child_agents: wire.child_agents,
            mcp_servers: wire.mcp_servers,
            catalog: wire.catalog,
            lsp_servers: wire.lsp_servers,
            context: wire.context,
            policy: wire.policy,
        };
        validate_snapshot(&snapshot).map_err(de::Error::custom)?;
        Ok(snapshot)
    }
}

/// In-memory reducer for authoritative runtime observations.
#[derive(Clone, Debug)]
pub struct RuntimeStatusState {
    child_agents: BTreeMap<RuntimeId, ChildAgentStatus>,
    mcp_servers: BTreeMap<RuntimeId, McpServerStatus>,
    catalog: TrustedCatalogStatus,
    lsp_servers: BTreeMap<(RuntimeId, RuntimeId), LspServerStatus>,
    context: ContextStatus,
    policy: Option<RuntimePolicyStatus>,
}

impl Default for RuntimeStatusState {
    fn default() -> Self {
        Self {
            child_agents: BTreeMap::new(),
            mcp_servers: BTreeMap::new(),
            catalog: TrustedCatalogStatus::default(),
            lsp_servers: BTreeMap::new(),
            context: ContextStatus::empty(),
            policy: None,
        }
    }
}

impl RuntimeStatusState {
    /// Restores and validates a previously serialized snapshot.
    pub fn from_snapshot(snapshot: RuntimeSnapshot) -> Result<Self, RuntimeStatusError> {
        validate_snapshot(&snapshot)?;
        Ok(Self {
            child_agents: snapshot
                .child_agents
                .into_iter()
                .map(|status| (status.id.clone(), status))
                .collect(),
            mcp_servers: snapshot
                .mcp_servers
                .into_iter()
                .map(|status| (status.id.clone(), status))
                .collect(),
            catalog: snapshot.catalog,
            lsp_servers: snapshot
                .lsp_servers
                .into_iter()
                .map(|status| {
                    (
                        (status.project_id.clone(), status.language_id.clone()),
                        status,
                    )
                })
                .collect(),
            context: snapshot.context,
            policy: snapshot.policy,
        })
    }

    /// Returns a deterministic path-free snapshot.
    pub fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeSnapshot {
            child_agents: BoundedVec(self.child_agents.values().cloned().collect::<Vec<_>>()),
            mcp_servers: BoundedVec(self.mcp_servers.values().cloned().collect::<Vec<_>>()),
            catalog: self.catalog.clone(),
            lsp_servers: BoundedVec(self.lsp_servers.values().cloned().collect::<Vec<_>>()),
            context: self.context.clone(),
            policy: self.policy.clone(),
        }
    }

    /// Applies one durable event.
    pub fn apply(&mut self, event: RuntimeEvent) -> Result<ApplyOutcome, RuntimeStatusError> {
        match event {
            RuntimeEvent::ChildAgentSpawned {
                id,
                parent_id,
                objective,
                queued_at_ms,
            } => self.spawn_child(id, parent_id, objective, queued_at_ms),
            RuntimeEvent::ChildAgentTransitioned {
                id,
                state,
                at_ms,
                outcome,
            } => self.transition_child(&id, state, at_ms, outcome),
            RuntimeEvent::McpConfigured { id, label, at_ms } => {
                self.configure_mcp(id, label, at_ms)
            }
            RuntimeEvent::McpTransitioned {
                id,
                state,
                at_ms,
                failure,
            } => self.transition_mcp(&id, state, at_ms, failure),
            RuntimeEvent::McpRestarted { id, at_ms } => self.restart_mcp(&id, at_ms),
            RuntimeEvent::CatalogReloadStarted { reload_id, at_ms } => {
                self.start_catalog_reload(reload_id, at_ms)
            }
            RuntimeEvent::CatalogReloadSucceeded {
                reload_id,
                entries,
                at_ms,
            } => self.finish_catalog_reload(reload_id, entries, at_ms),
            RuntimeEvent::CatalogReloadFailed {
                reload_id,
                failure,
                at_ms,
            } => self.fail_catalog_reload(reload_id, failure, at_ms),
            RuntimeEvent::CatalogEntryEnabled {
                entry_id,
                enabled,
                expected_generation,
                at_ms,
            } => self.set_catalog_entry_enabled(&entry_id, enabled, expected_generation, at_ms),
            RuntimeEvent::LspConfigured {
                project_id,
                language_id,
                at_ms,
            } => self.configure_lsp(project_id, language_id, at_ms),
            RuntimeEvent::LspTransitioned {
                project_id,
                language_id,
                state,
                at_ms,
                failure,
            } => self.transition_lsp(&project_id, &language_id, state, at_ms, failure),
            RuntimeEvent::LspRestarted {
                project_id,
                language_id,
                at_ms,
            } => self.restart_lsp(&project_id, &language_id, at_ms),
            RuntimeEvent::LspDiagnosticsPublished {
                project_id,
                language_id,
                revision,
                counts,
                at_ms,
            } => self.publish_lsp_diagnostics(&project_id, &language_id, revision, counts, at_ms),
            RuntimeEvent::ContextUpdated { totals, at_ms } => self.update_context(totals, at_ms),
            RuntimeEvent::CompactionStarted { id, before, at_ms } => {
                self.start_compaction(id, before, at_ms)
            }
            RuntimeEvent::CompactionFinished {
                id,
                after,
                reclaimed_tokens,
                at_ms,
            } => self.finish_compaction(id, after, reclaimed_tokens, at_ms),
            RuntimeEvent::PolicyPublished { policy } => self.publish_policy(*policy),
        }
    }

    fn spawn_child(
        &mut self,
        id: RuntimeId,
        parent_id: Option<RuntimeId>,
        objective: AgentObjective,
        queued_at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let candidate = ChildAgentStatus {
            id: id.clone(),
            parent_id: parent_id.clone(),
            objective,
            state: ChildAgentState::Queued,
            queued_at_ms,
            started_at_ms: None,
            updated_at_ms: queued_at_ms,
            finished_at_ms: None,
            outcome: None,
        };
        if let Some(existing) = self.child_agents.get(&id) {
            return if existing == &candidate {
                Ok(ApplyOutcome::Replay)
            } else {
                Err(RuntimeStatusError::DuplicateIdentity {
                    kind: "child agent",
                })
            };
        }
        if self.child_agents.len() == MAX_CHILD_AGENTS {
            return Err(RuntimeStatusError::LimitExceeded {
                field: "childAgents",
                limit: MAX_CHILD_AGENTS,
            });
        }
        if parent_id.as_ref() == Some(&id) {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "childAgentParent",
            });
        }
        if let Some(parent) = &parent_id {
            let parent =
                self.child_agents
                    .get(parent)
                    .ok_or(RuntimeStatusError::UnknownIdentity {
                        kind: "parent agent",
                    })?;
            if parent.state.is_terminal() {
                return Err(RuntimeStatusError::IllegalTransition {
                    kind: "child agent",
                });
            }
        }
        self.child_agents.insert(id, candidate);
        Ok(ApplyOutcome::Applied)
    }

    fn transition_child(
        &mut self,
        id: &RuntimeId,
        state: ChildAgentState,
        at_ms: u64,
        outcome: Option<RuntimeSummary>,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let status = self
            .child_agents
            .get_mut(id)
            .ok_or(RuntimeStatusError::UnknownIdentity {
                kind: "child agent",
            })?;
        let mut candidate = status.clone();
        if state.is_terminal() != outcome.is_some() {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "childAgentOutcome",
            });
        }
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression {
                kind: "child agent",
            });
        }
        if state == status.state && at_ms == status.updated_at_ms && outcome == status.outcome {
            return Ok(ApplyOutcome::Replay);
        }
        let legal = matches!(
            (status.state, state),
            (ChildAgentState::Queued, ChildAgentState::Running)
                | (ChildAgentState::Queued, ChildAgentState::Failed)
                | (ChildAgentState::Queued, ChildAgentState::Cancelled)
                | (ChildAgentState::Running, ChildAgentState::Waiting)
                | (ChildAgentState::Running, ChildAgentState::Succeeded)
                | (ChildAgentState::Running, ChildAgentState::Failed)
                | (ChildAgentState::Running, ChildAgentState::Cancelled)
                | (ChildAgentState::Waiting, ChildAgentState::Running)
                | (ChildAgentState::Waiting, ChildAgentState::Succeeded)
                | (ChildAgentState::Waiting, ChildAgentState::Failed)
                | (ChildAgentState::Waiting, ChildAgentState::Cancelled)
        );
        if !legal {
            return Err(RuntimeStatusError::IllegalTransition {
                kind: "child agent",
            });
        }
        candidate.state = state;
        candidate.updated_at_ms = at_ms;
        if state == ChildAgentState::Running && candidate.started_at_ms.is_none() {
            candidate.started_at_ms = Some(at_ms);
        }
        if state.is_terminal() {
            candidate.finished_at_ms = Some(at_ms);
            candidate.outcome = outcome;
        }
        *status = candidate;
        Ok(ApplyOutcome::Applied)
    }

    fn configure_mcp(
        &mut self,
        id: RuntimeId,
        label: RuntimeLabel,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let candidate = McpServerStatus {
            id: id.clone(),
            label,
            state: McpServerState::Configured,
            restart_count: 0,
            configured_at_ms: at_ms,
            updated_at_ms: at_ms,
            failure: None,
        };
        if let Some(existing) = self.mcp_servers.get(&id) {
            return if existing == &candidate {
                Ok(ApplyOutcome::Replay)
            } else {
                Err(RuntimeStatusError::DuplicateIdentity { kind: "MCP server" })
            };
        }
        if self.mcp_servers.len() == MAX_MCP_SERVERS {
            return Err(RuntimeStatusError::LimitExceeded {
                field: "mcpServers",
                limit: MAX_MCP_SERVERS,
            });
        }
        self.mcp_servers.insert(id, candidate);
        Ok(ApplyOutcome::Applied)
    }

    fn transition_mcp(
        &mut self,
        id: &RuntimeId,
        state: McpServerState,
        at_ms: u64,
        failure: Option<RuntimeSummary>,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let status = self
            .mcp_servers
            .get_mut(id)
            .ok_or(RuntimeStatusError::UnknownIdentity { kind: "MCP server" })?;
        if (state == McpServerState::Failed) != failure.is_some() {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "mcpFailure",
            });
        }
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "MCP server" });
        }
        if state == status.state && at_ms == status.updated_at_ms && failure == status.failure {
            return Ok(ApplyOutcome::Replay);
        }
        let legal = matches!(
            (status.state, state),
            (McpServerState::Configured, McpServerState::Starting)
                | (McpServerState::Configured, McpServerState::Stopped)
                | (McpServerState::Starting, McpServerState::Ready)
                | (McpServerState::Starting, McpServerState::Failed)
                | (McpServerState::Starting, McpServerState::Stopped)
                | (McpServerState::Ready, McpServerState::Failed)
                | (McpServerState::Ready, McpServerState::Stopped)
        );
        if !legal {
            return Err(RuntimeStatusError::IllegalTransition { kind: "MCP server" });
        }
        status.state = state;
        status.updated_at_ms = at_ms;
        status.failure = failure;
        Ok(ApplyOutcome::Applied)
    }

    fn restart_mcp(
        &mut self,
        id: &RuntimeId,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let status = self
            .mcp_servers
            .get_mut(id)
            .ok_or(RuntimeStatusError::UnknownIdentity { kind: "MCP server" })?;
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "MCP server" });
        }
        if !matches!(
            status.state,
            McpServerState::Ready | McpServerState::Failed | McpServerState::Stopped
        ) {
            return Err(RuntimeStatusError::IllegalTransition { kind: "MCP server" });
        }
        status.restart_count =
            status
                .restart_count
                .checked_add(1)
                .ok_or(RuntimeStatusError::ContradictoryFacts {
                    field: "mcpRestartCount",
                })?;
        status.state = McpServerState::Starting;
        status.updated_at_ms = at_ms;
        status.failure = None;
        Ok(ApplyOutcome::Applied)
    }

    fn start_catalog_reload(
        &mut self,
        reload_id: RuntimeId,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        if let CatalogReloadStatus::Running {
            reload_id: current,
            retained_generation,
            started_at_ms,
        } = &self.catalog.reload
        {
            if current == &reload_id
                && *retained_generation == self.catalog.generation
                && *started_at_ms == at_ms
            {
                return Ok(ApplyOutcome::Replay);
            }
            return Err(RuntimeStatusError::OperationInProgress {
                kind: "catalog reload",
            });
        }
        if at_ms < catalog_last_observed_at(&self.catalog) {
            return Err(RuntimeStatusError::TimestampRegression { kind: "catalog" });
        }
        self.catalog.reload = CatalogReloadStatus::Running {
            reload_id,
            retained_generation: self.catalog.generation,
            started_at_ms: at_ms,
        };
        Ok(ApplyOutcome::Applied)
    }

    fn finish_catalog_reload(
        &mut self,
        reload_id: RuntimeId,
        entries: BoundedVec<TrustedCatalogEntry, MAX_CATALOG_ENTRIES>,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        validate_catalog_entries(&entries)?;
        if let CatalogReloadStatus::Succeeded {
            reload_id: current,
            generation,
            finished_at_ms,
            ..
        } = &self.catalog.reload
        {
            if current == &reload_id
                && *generation == self.catalog.generation
                && *finished_at_ms == at_ms
                && self.catalog.entries == entries
            {
                return Ok(ApplyOutcome::Replay);
            }
        }
        let (current_id, retained_generation, started_at_ms) = match &self.catalog.reload {
            CatalogReloadStatus::Running {
                reload_id,
                retained_generation,
                started_at_ms,
            } => (reload_id, *retained_generation, *started_at_ms),
            _ => {
                return Err(RuntimeStatusError::IllegalTransition {
                    kind: "catalog reload",
                })
            }
        };
        if current_id != &reload_id {
            return Err(RuntimeStatusError::UnknownIdentity {
                kind: "catalog reload",
            });
        }
        if retained_generation != self.catalog.generation || at_ms < started_at_ms {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "catalogGeneration",
            });
        }
        let generation = self.catalog.generation.checked_add(1).ok_or(
            RuntimeStatusError::ContradictoryFacts {
                field: "catalogGeneration",
            },
        )?;
        self.catalog.entries = entries;
        self.catalog.generation = generation;
        self.catalog.updated_at_ms = at_ms;
        self.catalog.reload = CatalogReloadStatus::Succeeded {
            reload_id,
            generation,
            started_at_ms,
            finished_at_ms: at_ms,
        };
        Ok(ApplyOutcome::Applied)
    }

    fn fail_catalog_reload(
        &mut self,
        reload_id: RuntimeId,
        failure: RuntimeSummary,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        if let CatalogReloadStatus::Failed {
            reload_id: current,
            retained_generation,
            finished_at_ms,
            failure: current_failure,
            ..
        } = &self.catalog.reload
        {
            if current == &reload_id
                && *retained_generation == self.catalog.generation
                && *finished_at_ms == at_ms
                && current_failure == &failure
            {
                return Ok(ApplyOutcome::Replay);
            }
        }
        let (current_id, retained_generation, started_at_ms) = match &self.catalog.reload {
            CatalogReloadStatus::Running {
                reload_id,
                retained_generation,
                started_at_ms,
            } => (reload_id, *retained_generation, *started_at_ms),
            _ => {
                return Err(RuntimeStatusError::IllegalTransition {
                    kind: "catalog reload",
                })
            }
        };
        if current_id != &reload_id {
            return Err(RuntimeStatusError::UnknownIdentity {
                kind: "catalog reload",
            });
        }
        if retained_generation != self.catalog.generation || at_ms < started_at_ms {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "catalogGeneration",
            });
        }
        self.catalog.reload = CatalogReloadStatus::Failed {
            reload_id,
            retained_generation,
            started_at_ms,
            finished_at_ms: at_ms,
            failure,
        };
        Ok(ApplyOutcome::Applied)
    }

    fn set_catalog_entry_enabled(
        &mut self,
        entry_id: &RuntimeId,
        enabled: bool,
        expected_generation: u64,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        if matches!(self.catalog.reload, CatalogReloadStatus::Running { .. }) {
            return Err(RuntimeStatusError::OperationInProgress {
                kind: "catalog reload",
            });
        }
        if expected_generation != self.catalog.generation {
            return Err(RuntimeStatusError::RevisionRegression {
                field: "catalogGeneration",
            });
        }
        if at_ms < catalog_last_observed_at(&self.catalog) {
            return Err(RuntimeStatusError::TimestampRegression { kind: "catalog" });
        }
        let entry = self
            .catalog
            .entries
            .0
            .iter_mut()
            .find(|entry| &entry.id == entry_id)
            .ok_or(RuntimeStatusError::UnknownIdentity {
                kind: "catalog entry",
            })?;
        if entry.enabled == enabled {
            return Ok(ApplyOutcome::Replay);
        }
        entry.enabled = enabled;
        self.catalog.generation = self.catalog.generation.checked_add(1).ok_or(
            RuntimeStatusError::ContradictoryFacts {
                field: "catalogGeneration",
            },
        )?;
        self.catalog.updated_at_ms = at_ms;
        Ok(ApplyOutcome::Applied)
    }

    fn configure_lsp(
        &mut self,
        project_id: RuntimeId,
        language_id: RuntimeId,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let key = (project_id.clone(), language_id.clone());
        let candidate = LspServerStatus {
            project_id,
            language_id,
            state: LspServerState::Configured,
            restart_count: 0,
            configured_at_ms: at_ms,
            updated_at_ms: at_ms,
            diagnostic_revision: 0,
            diagnostics: DiagnosticCounts::default(),
            failure: None,
        };
        if let Some(existing) = self.lsp_servers.get(&key) {
            return if existing == &candidate {
                Ok(ApplyOutcome::Replay)
            } else {
                Err(RuntimeStatusError::DuplicateIdentity { kind: "LSP server" })
            };
        }
        if self.lsp_servers.len() == MAX_LSP_SERVERS {
            return Err(RuntimeStatusError::LimitExceeded {
                field: "lspServers",
                limit: MAX_LSP_SERVERS,
            });
        }
        self.lsp_servers.insert(key, candidate);
        Ok(ApplyOutcome::Applied)
    }

    fn transition_lsp(
        &mut self,
        project_id: &RuntimeId,
        language_id: &RuntimeId,
        state: LspServerState,
        at_ms: u64,
        failure: Option<RuntimeSummary>,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let status = self
            .lsp_servers
            .get_mut(&(project_id.clone(), language_id.clone()))
            .ok_or(RuntimeStatusError::UnknownIdentity { kind: "LSP server" })?;
        if (state == LspServerState::Failed) != failure.is_some() {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "lspFailure",
            });
        }
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "LSP server" });
        }
        if state == status.state && at_ms == status.updated_at_ms && failure == status.failure {
            return Ok(ApplyOutcome::Replay);
        }
        let legal = matches!(
            (status.state, state),
            (LspServerState::Configured, LspServerState::Starting)
                | (LspServerState::Configured, LspServerState::Stopped)
                | (LspServerState::Starting, LspServerState::Ready)
                | (LspServerState::Starting, LspServerState::Failed)
                | (LspServerState::Starting, LspServerState::Stopped)
                | (LspServerState::Ready, LspServerState::Failed)
                | (LspServerState::Ready, LspServerState::Stopped)
        );
        if !legal {
            return Err(RuntimeStatusError::IllegalTransition { kind: "LSP server" });
        }
        status.state = state;
        status.updated_at_ms = at_ms;
        status.failure = failure;
        if state != LspServerState::Ready {
            status.diagnostics = DiagnosticCounts::default();
        }
        Ok(ApplyOutcome::Applied)
    }

    fn restart_lsp(
        &mut self,
        project_id: &RuntimeId,
        language_id: &RuntimeId,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        let status = self
            .lsp_servers
            .get_mut(&(project_id.clone(), language_id.clone()))
            .ok_or(RuntimeStatusError::UnknownIdentity { kind: "LSP server" })?;
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "LSP server" });
        }
        if !matches!(
            status.state,
            LspServerState::Ready | LspServerState::Failed | LspServerState::Stopped
        ) {
            return Err(RuntimeStatusError::IllegalTransition { kind: "LSP server" });
        }
        status.restart_count =
            status
                .restart_count
                .checked_add(1)
                .ok_or(RuntimeStatusError::ContradictoryFacts {
                    field: "lspRestartCount",
                })?;
        status.state = LspServerState::Starting;
        status.updated_at_ms = at_ms;
        status.failure = None;
        status.diagnostics = DiagnosticCounts::default();
        Ok(ApplyOutcome::Applied)
    }

    fn publish_lsp_diagnostics(
        &mut self,
        project_id: &RuntimeId,
        language_id: &RuntimeId,
        revision: u64,
        counts: DiagnosticCounts,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        counts.validate()?;
        let status = self
            .lsp_servers
            .get_mut(&(project_id.clone(), language_id.clone()))
            .ok_or(RuntimeStatusError::UnknownIdentity { kind: "LSP server" })?;
        if status.state != LspServerState::Ready {
            return Err(RuntimeStatusError::IllegalTransition {
                kind: "LSP diagnostics",
            });
        }
        if revision == status.diagnostic_revision
            && counts == status.diagnostics
            && at_ms == status.updated_at_ms
        {
            return Ok(ApplyOutcome::Replay);
        }
        if revision <= status.diagnostic_revision {
            return Err(RuntimeStatusError::RevisionRegression {
                field: "diagnosticRevision",
            });
        }
        if at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "LSP server" });
        }
        status.diagnostic_revision = revision;
        status.diagnostics = counts;
        status.updated_at_ms = at_ms;
        Ok(ApplyOutcome::Applied)
    }

    fn update_context(
        &mut self,
        totals: ContextTotals,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        totals.validate()?;
        if self.context.active_compaction.is_some() {
            return Err(RuntimeStatusError::OperationInProgress { kind: "compaction" });
        }
        if at_ms < self.context.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "context" });
        }
        if totals == self.context.current && at_ms == self.context.updated_at_ms {
            return Ok(ApplyOutcome::Replay);
        }
        self.context.current = totals;
        self.context.updated_at_ms = at_ms;
        Ok(ApplyOutcome::Applied)
    }

    fn start_compaction(
        &mut self,
        id: RuntimeId,
        before: ContextTotals,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        before.validate()?;
        if let Some(active) = &self.context.active_compaction {
            if active.id == id && active.before == before && active.started_at_ms == at_ms {
                return Ok(ApplyOutcome::Replay);
            }
            return Err(RuntimeStatusError::OperationInProgress { kind: "compaction" });
        }
        if let Some(last) = &self.context.last_compaction {
            if last.id == id && last.before == before && last.started_at_ms == at_ms {
                return Ok(ApplyOutcome::Replay);
            }
        }
        if before != self.context.current {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "compactionBefore",
            });
        }
        if at_ms < self.context.updated_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "context" });
        }
        self.context.active_compaction = Some(ActiveCompaction {
            id,
            before,
            started_at_ms: at_ms,
        });
        Ok(ApplyOutcome::Applied)
    }

    fn finish_compaction(
        &mut self,
        id: RuntimeId,
        after: ContextTotals,
        reclaimed_tokens: u64,
        at_ms: u64,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        after.validate()?;
        if let Some(last) = &self.context.last_compaction {
            if self.context.active_compaction.is_none()
                && last.id == id
                && last.after == after
                && last.reclaimed_tokens == reclaimed_tokens
                && last.finished_at_ms == at_ms
            {
                return Ok(ApplyOutcome::Replay);
            }
        }
        let active = self
            .context
            .active_compaction
            .as_ref()
            .ok_or(RuntimeStatusError::IllegalTransition { kind: "compaction" })?;
        if active.id != id {
            return Err(RuntimeStatusError::UnknownIdentity { kind: "compaction" });
        }
        if at_ms < active.started_at_ms {
            return Err(RuntimeStatusError::TimestampRegression { kind: "compaction" });
        }
        let expected = active
            .before
            .total_tokens
            .checked_sub(after.total_tokens)
            .ok_or(RuntimeStatusError::ContradictoryFacts {
                field: "compactionTotals",
            })?;
        if expected != reclaimed_tokens {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "compactionTotals",
            });
        }
        let completed = CompletedCompaction {
            id,
            before: active.before.clone(),
            after: after.clone(),
            reclaimed_tokens,
            started_at_ms: active.started_at_ms,
            finished_at_ms: at_ms,
        };
        self.context.current = after;
        self.context.updated_at_ms = at_ms;
        self.context.active_compaction = None;
        self.context.last_compaction = Some(completed);
        Ok(ApplyOutcome::Applied)
    }

    fn publish_policy(
        &mut self,
        policy: RuntimePolicyStatus,
    ) -> Result<ApplyOutcome, RuntimeStatusError> {
        validate_policy(&policy)?;
        if let Some(current) = &self.policy {
            if current == &policy {
                return Ok(ApplyOutcome::Replay);
            }
            if policy.revision <= current.revision {
                return Err(RuntimeStatusError::RevisionRegression {
                    field: "policyRevision",
                });
            }
            if policy.observed_at_ms < current.observed_at_ms {
                return Err(RuntimeStatusError::TimestampRegression { kind: "policy" });
            }
        }
        self.policy = Some(policy);
        Ok(ApplyOutcome::Applied)
    }
}

fn validate_catalog_entries(
    entries: &BoundedVec<TrustedCatalogEntry, MAX_CATALOG_ENTRIES>,
) -> Result<(), RuntimeStatusError> {
    let mut entry_ids = BTreeSet::new();
    let mut contribution_ids = BTreeSet::new();
    for entry in entries {
        if !entry_ids.insert(entry.id.clone()) {
            return Err(RuntimeStatusError::DuplicateIdentity {
                kind: "catalog entry",
            });
        }
        let mut local = BTreeSet::new();
        for contribution in &entry.contributions {
            let key = (contribution.kind, contribution.id.clone());
            if !local.insert(key.clone()) || !contribution_ids.insert(key) {
                return Err(RuntimeStatusError::DuplicateIdentity {
                    kind: "catalog contribution",
                });
            }
        }
    }
    Ok(())
}

fn validate_snapshot(snapshot: &RuntimeSnapshot) -> Result<(), RuntimeStatusError> {
    let mut child_ids = BTreeSet::new();
    for child in &snapshot.child_agents {
        if !child_ids.insert(child.id.clone()) {
            return Err(RuntimeStatusError::DuplicateIdentity {
                kind: "child agent",
            });
        }
        validate_child_status(child)?;
    }
    for child in &snapshot.child_agents {
        if let Some(parent) = &child.parent_id {
            if parent == &child.id || !child_ids.contains(parent) {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "childAgentParent",
                });
            }
            let mut cursor = Some(parent);
            let mut visited = BTreeSet::from([child.id.clone()]);
            while let Some(id) = cursor {
                if !visited.insert(id.clone()) {
                    return Err(RuntimeStatusError::ContradictoryFacts {
                        field: "childAgentParent",
                    });
                }
                cursor = snapshot
                    .child_agents
                    .as_slice()
                    .iter()
                    .find(|candidate| &candidate.id == id)
                    .and_then(|candidate| candidate.parent_id.as_ref());
            }
        }
    }

    let mut mcp_ids = BTreeSet::new();
    for server in &snapshot.mcp_servers {
        if !mcp_ids.insert(server.id.clone()) {
            return Err(RuntimeStatusError::DuplicateIdentity { kind: "MCP server" });
        }
        validate_mcp_status(server)?;
    }
    validate_catalog_entries(&snapshot.catalog.entries)?;
    validate_catalog_status(&snapshot.catalog)?;

    let mut lsp_keys = BTreeSet::new();
    for server in &snapshot.lsp_servers {
        if !lsp_keys.insert((server.project_id.clone(), server.language_id.clone())) {
            return Err(RuntimeStatusError::DuplicateIdentity { kind: "LSP server" });
        }
        validate_lsp_status(server)?;
    }
    validate_context_status(&snapshot.context)?;
    if let Some(policy) = &snapshot.policy {
        validate_policy(policy)?;
    }
    Ok(())
}

fn validate_child_status(status: &ChildAgentStatus) -> Result<(), RuntimeStatusError> {
    if status.updated_at_ms < status.queued_at_ms
        || status
            .started_at_ms
            .is_some_and(|started| started < status.queued_at_ms || started > status.updated_at_ms)
        || status.finished_at_ms.is_some_and(|finished| {
            finished < status.queued_at_ms || finished != status.updated_at_ms
        })
    {
        return Err(RuntimeStatusError::ContradictoryFacts {
            field: "childAgentTiming",
        });
    }
    if status.state.is_terminal() != (status.finished_at_ms.is_some() && status.outcome.is_some()) {
        return Err(RuntimeStatusError::ContradictoryFacts {
            field: "childAgentOutcome",
        });
    }
    if matches!(
        status.state,
        ChildAgentState::Running | ChildAgentState::Waiting
    ) && status.started_at_ms.is_none()
    {
        return Err(RuntimeStatusError::ContradictoryFacts {
            field: "childAgentTiming",
        });
    }
    if status.state == ChildAgentState::Queued && status.started_at_ms.is_some() {
        return Err(RuntimeStatusError::ContradictoryFacts {
            field: "childAgentTiming",
        });
    }
    Ok(())
}

fn validate_mcp_status(status: &McpServerStatus) -> Result<(), RuntimeStatusError> {
    if status.updated_at_ms < status.configured_at_ms
        || (status.state == McpServerState::Failed) != status.failure.is_some()
    {
        return Err(RuntimeStatusError::ContradictoryFacts { field: "mcpStatus" });
    }
    Ok(())
}

fn validate_catalog_status(status: &TrustedCatalogStatus) -> Result<(), RuntimeStatusError> {
    match &status.reload {
        CatalogReloadStatus::Idle => {
            if status.generation != 0 {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "catalogReload",
                });
            }
        }
        CatalogReloadStatus::Running {
            retained_generation,
            started_at_ms,
            ..
        } => {
            if *retained_generation != status.generation || *started_at_ms < status.updated_at_ms {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "catalogReload",
                });
            }
        }
        CatalogReloadStatus::Succeeded {
            generation,
            started_at_ms,
            finished_at_ms,
            ..
        } => {
            if *generation > status.generation
                || *finished_at_ms > status.updated_at_ms
                || finished_at_ms < started_at_ms
            {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "catalogReload",
                });
            }
        }
        CatalogReloadStatus::Failed {
            retained_generation,
            started_at_ms,
            finished_at_ms,
            ..
        } => {
            if *retained_generation > status.generation || finished_at_ms < started_at_ms {
                return Err(RuntimeStatusError::ContradictoryFacts {
                    field: "catalogReload",
                });
            }
        }
    }
    Ok(())
}

fn catalog_last_observed_at(status: &TrustedCatalogStatus) -> u64 {
    match &status.reload {
        CatalogReloadStatus::Idle => status.updated_at_ms,
        CatalogReloadStatus::Running { started_at_ms, .. } => *started_at_ms,
        CatalogReloadStatus::Succeeded { finished_at_ms, .. }
        | CatalogReloadStatus::Failed { finished_at_ms, .. } => {
            status.updated_at_ms.max(*finished_at_ms)
        }
    }
}

fn validate_lsp_status(status: &LspServerStatus) -> Result<(), RuntimeStatusError> {
    status.diagnostics.validate()?;
    if status.updated_at_ms < status.configured_at_ms
        || (status.state == LspServerState::Failed) != status.failure.is_some()
        || (status.state != LspServerState::Ready
            && status.diagnostics != DiagnosticCounts::default())
    {
        return Err(RuntimeStatusError::ContradictoryFacts { field: "lspStatus" });
    }
    Ok(())
}

fn validate_context_status(status: &ContextStatus) -> Result<(), RuntimeStatusError> {
    status.current.validate()?;
    if let Some(active) = &status.active_compaction {
        active.before.validate()?;
        if active.before != status.current || active.started_at_ms < status.updated_at_ms {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "activeCompaction",
            });
        }
    }
    if let Some(completed) = &status.last_compaction {
        completed.before.validate()?;
        completed.after.validate()?;
        if completed.finished_at_ms < completed.started_at_ms
            || completed
                .before
                .total_tokens
                .checked_sub(completed.after.total_tokens)
                != Some(completed.reclaimed_tokens)
        {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "completedCompaction",
            });
        }
        if status.active_compaction.is_none()
            && (status.current != completed.after
                || status.updated_at_ms < completed.finished_at_ms)
        {
            return Err(RuntimeStatusError::ContradictoryFacts {
                field: "completedCompaction",
            });
        }
    }
    Ok(())
}

fn validate_policy(policy: &RuntimePolicyStatus) -> Result<(), RuntimeStatusError> {
    if let ToolPolicy::Enforced { rules } = &policy.tools {
        rules.validate()?;
    }
    if let CommandPolicy::Enforced { rules } = &policy.commands {
        rules.validate()?;
    }
    if let RemoteReadPolicy::Enforced {
        consequence: RemoteReadConsequence::DomainRules { domains },
    } = &policy.remote_read
    {
        domains.validate()?;
    }
    if let ProcessNetworkPolicy::Enforced {
        consequence: ProcessNetworkConsequence::DomainRules { domains },
    } = &policy.process_network
    {
        domains.validate()?;
    }
    if let ApprovalPolicy::Enforced { consequence } = &policy.approvals {
        consequence.validate()?;
    }
    if let SecretsPolicy::Enforced { consequence } = &policy.secrets {
        consequence.validate()?;
    }
    Ok(())
}
