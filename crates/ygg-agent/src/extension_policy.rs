//! Host-owned classification and single-use approvals for extension action intents.
//!
//! This module deliberately does not treat an extension's hints as authority.
//! A trusted product adapter supplies the baseline decision; extension hints can
//! only make that decision more cautious. Approval tokens are opaque, bounded,
//! short-lived capabilities tied to one process generation, parent request, and
//! canonical intent hash.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::extension_process::ExtensionRequestId;

/// Maximum number of live approval capabilities retained by one store.
pub const MAX_EXTENSION_APPROVALS: usize = 256;
/// Maximum approval lifetime accepted by [`ExtensionApprovalStore::issue`].
pub const MAX_EXTENSION_APPROVAL_TTL: Duration = Duration::from_secs(5 * 60);
/// Maximum serialized size of one canonical action intent.
pub const MAX_EXTENSION_ACTION_INTENT_BYTES: usize = 32 * 1024;

/// An extension's non-authoritative risk hints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionAdapterHints {
    /// The adapter believes the operation is read-only. The host never uses
    /// this value to reduce an authoritative risk decision.
    pub read_only: Option<bool>,
    /// The adapter believes the operation is destructive. `true` raises an
    /// otherwise-allowed decision to an interactive approval boundary.
    pub destructive: Option<bool>,
}

/// A structured action proposed by a cooperative extension or provider adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionActionIntent {
    /// Broad semantic class, such as `external_side_effect`.
    pub kind: String,
    /// Stable operation name, such as `browser.submit_form`.
    pub operation: String,
    /// Bounded target description. This is display/audit data, not authority.
    pub target: serde_json::Value,
    /// Data classes crossing the boundary, such as `user_text`.
    #[serde(default)]
    pub data_classes: Vec<String>,
    /// Non-authoritative adapter hints which may only increase caution.
    #[serde(default)]
    pub adapter_hints: ExtensionAdapterHints,
}

impl ExtensionActionIntent {
    /// Validates bounds and returns the canonical SHA-256 digest used to bind
    /// an approval token to this exact intent.
    pub fn canonical_hash(&self) -> Result<[u8; 32], ExtensionPolicyError> {
        if self.kind.trim().is_empty() || self.operation.trim().is_empty() {
            return Err(ExtensionPolicyError::InvalidIntent(
                "kind and operation must be non-empty".into(),
            ));
        }
        if !self.target.is_object() {
            return Err(ExtensionPolicyError::InvalidIntent(
                "target must be a JSON object".into(),
            ));
        }
        let value = serde_json::to_value(self)
            .map_err(|error| ExtensionPolicyError::InvalidIntent(error.to_string()))?;
        let canonical = canonicalize_json(value);
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| ExtensionPolicyError::InvalidIntent(error.to_string()))?;
        if bytes.len() > MAX_EXTENSION_ACTION_INTENT_BYTES {
            return Err(ExtensionPolicyError::IntentTooLarge {
                bytes: bytes.len(),
                limit: MAX_EXTENSION_ACTION_INTENT_BYTES,
            });
        }
        Ok(Sha256::digest(bytes).into())
    }
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(values) => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar,
    }
}

/// Host-owned decision for one action intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPolicyDecision {
    /// The authoritative host policy permits the operation.
    Allow,
    /// An interactive user decision is required.
    Ask,
    /// The host denies the operation.
    Deny,
}

/// Whether a frontend can resolve an interactive policy decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionPolicyFrontend {
    /// A trusted interactive confirmation surface is available.
    Interactive,
    /// No confirmation surface exists; unresolved asks fail closed.
    Headless,
}

/// Authoritative product policy applied to extension action intents.
#[derive(Clone, Debug)]
pub struct ExtensionIntentPolicy {
    default: ExtensionPolicyDecision,
    operations: BTreeMap<String, ExtensionPolicyDecision>,
}

impl ExtensionIntentPolicy {
    /// Creates a policy with the supplied fail-safe default.
    pub fn new(default: ExtensionPolicyDecision) -> Self {
        Self {
            default,
            operations: BTreeMap::new(),
        }
    }

    /// Sets the authoritative decision for an exact operation name.
    pub fn set_operation(
        &mut self,
        operation: impl Into<String>,
        decision: ExtensionPolicyDecision,
    ) {
        self.operations.insert(operation.into(), decision);
    }

    /// Classifies an intent. Adapter hints can escalate but never lower the
    /// configured decision, and a headless `ask` is converted to `deny`.
    pub fn classify(
        &self,
        intent: &ExtensionActionIntent,
        frontend: ExtensionPolicyFrontend,
    ) -> Result<ExtensionPolicyDecision, ExtensionPolicyError> {
        intent.canonical_hash()?;
        let mut decision = self
            .operations
            .get(&intent.operation)
            .copied()
            .unwrap_or(self.default);
        if intent.adapter_hints.destructive == Some(true)
            && decision == ExtensionPolicyDecision::Allow
        {
            decision = ExtensionPolicyDecision::Ask;
        }
        if decision == ExtensionPolicyDecision::Ask && frontend == ExtensionPolicyFrontend::Headless
        {
            decision = ExtensionPolicyDecision::Deny;
        }
        Ok(decision)
    }
}

/// Shared zeroizing wire bytes retained by the token and bounded store.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ApprovalTokenBytes([u8; 64]);

impl Drop for ApprovalTokenBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Opaque single-use capability returned after an interactive approval.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ExtensionApprovalToken(Arc<ApprovalTokenBytes>);

impl ExtensionApprovalToken {
    /// Returns the opaque wire representation.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0.as_ref().0)
            .expect("approval tokens are lowercase hexadecimal ASCII")
    }

    fn parse(value: String) -> Result<Self, String> {
        let mut source = value.into_bytes();
        if source.len() != 64
            || !source
                .iter()
                .copied()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            source.fill(0);
            return Err("approval token must be 64 lowercase hexadecimal characters".into());
        }
        let mut bytes = [0_u8; 64];
        bytes.copy_from_slice(&source);
        source.fill(0);
        Ok(Self(Arc::new(ApprovalTokenBytes(bytes))))
    }
}

impl std::fmt::Debug for ExtensionApprovalToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExtensionApprovalToken([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for ExtensionApprovalToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

impl Serialize for ExtensionApprovalToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Clone)]
struct ApprovalGrant {
    intent_hash: [u8; 32],
    generation: u64,
    parent_request_id: ExtensionRequestId,
    expires_at: Instant,
}

#[derive(Default)]
struct ApprovalState {
    grants: BTreeMap<Arc<ApprovalTokenBytes>, ApprovalGrant>,
    insertion_order: VecDeque<Arc<ApprovalTokenBytes>>,
}

/// Bounded store for generation- and request-scoped approval capabilities.
#[derive(Default)]
pub struct ExtensionApprovalStore {
    state: Mutex<ApprovalState>,
}

impl ExtensionApprovalStore {
    /// Creates an empty approval store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a short-lived capability for an already-approved intent.
    pub fn issue(
        &self,
        intent: &ExtensionActionIntent,
        generation: u64,
        parent_request_id: ExtensionRequestId,
        ttl: Duration,
    ) -> Result<ExtensionApprovalToken, ExtensionPolicyError> {
        if ttl.is_zero() || ttl > MAX_EXTENSION_APPROVAL_TTL {
            return Err(ExtensionPolicyError::InvalidTtl);
        }
        let intent_hash = intent.canonical_hash()?;
        let expires_at = Instant::now()
            .checked_add(ttl)
            .ok_or(ExtensionPolicyError::InvalidTtl)?;
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random)
            .map_err(|error| ExtensionPolicyError::Random(error.to_string()))?;
        let token = Arc::new(ApprovalTokenBytes(hex(&random)));
        random.fill(0);
        let mut state = lock_state(&self.state);
        prune_expired(&mut state, Instant::now());
        while state.grants.len() >= MAX_EXTENSION_APPROVALS {
            let Some(oldest) = state.insertion_order.pop_front() else {
                break;
            };
            state.grants.remove(&oldest);
        }
        state.insertion_order.push_back(token.clone());
        state.grants.insert(
            token.clone(),
            ApprovalGrant {
                intent_hash,
                generation,
                parent_request_id,
                expires_at,
            },
        );
        Ok(ExtensionApprovalToken(token))
    }

    /// Atomically consumes a capability. A mismatch, stale generation, wrong
    /// parent, expiry, or prior use all fail and still invalidate that token.
    pub fn consume(
        &self,
        token: &ExtensionApprovalToken,
        intent: &ExtensionActionIntent,
        generation: u64,
        parent_request_id: &ExtensionRequestId,
    ) -> Result<bool, ExtensionPolicyError> {
        let intent_hash = intent.canonical_hash()?;
        let now = Instant::now();
        let mut state = lock_state(&self.state);
        prune_expired(&mut state, now);
        let Some(grant) = state.grants.remove(&token.0) else {
            return Ok(false);
        };
        state
            .insertion_order
            .retain(|candidate| candidate != &token.0);
        Ok(grant.expires_at > now
            && grant.intent_hash == intent_hash
            && grant.generation == generation
            && &grant.parent_request_id == parent_request_id)
    }

    /// Invalidates every outstanding token owned by a process generation.
    pub fn invalidate_generation(&self, generation: u64) {
        let mut state = lock_state(&self.state);
        state
            .grants
            .retain(|_, grant| grant.generation != generation);
        let live = state
            .grants
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        state.insertion_order.retain(|token| live.contains(token));
    }

    /// Returns the number of live, unexpired capabilities.
    pub fn live_len(&self) -> usize {
        let mut state = lock_state(&self.state);
        prune_expired(&mut state, Instant::now());
        state.grants.len()
    }

    /// Returns whether no live, unexpired capabilities remain.
    pub fn is_empty(&self) -> bool {
        self.live_len() == 0
    }
}

fn lock_state(state: &Mutex<ApprovalState>) -> MutexGuard<'_, ApprovalState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn prune_expired(state: &mut ApprovalState, now: Instant) {
    state.grants.retain(|_, grant| grant.expires_at > now);
    let live = state
        .grants
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    state.insertion_order.retain(|token| live.contains(token));
}

fn hex(bytes: &[u8; 32]) -> [u8; 64] {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = [0_u8; 64];
    for (index, byte) in bytes.iter().enumerate() {
        encoded[index * 2] = DIGITS[usize::from(byte >> 4)];
        encoded[index * 2 + 1] = DIGITS[usize::from(byte & 0x0f)];
    }
    encoded
}

/// Validation or capability-issuance failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ExtensionPolicyError {
    /// The intent is structurally invalid.
    #[error("invalid extension action intent: {0}")]
    InvalidIntent(String),
    /// The canonical intent exceeds its wire/storage bound.
    #[error("extension action intent is {bytes} bytes; limit is {limit}")]
    IntentTooLarge {
        /// Canonical serialized size.
        bytes: usize,
        /// Maximum accepted serialized size.
        limit: usize,
    },
    /// The requested token lifetime is zero, too long, or overflows time.
    #[error("invalid extension approval token lifetime")]
    InvalidTtl,
    /// Secure random token generation failed.
    #[error("cannot generate extension approval token: {0}")]
    Random(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(operation: &str) -> ExtensionActionIntent {
        ExtensionActionIntent {
            kind: "external_side_effect".into(),
            operation: operation.into(),
            target: serde_json::json!({"label": "Publish", "origin": "https://example.com"}),
            data_classes: vec!["user_text".into()],
            adapter_hints: ExtensionAdapterHints::default(),
        }
    }

    #[test]
    fn adapter_hints_never_lower_authoritative_policy() {
        let mut policy = ExtensionIntentPolicy::new(ExtensionPolicyDecision::Deny);
        let mut action = intent("browser.read");
        action.adapter_hints.read_only = Some(true);
        assert_eq!(
            policy
                .classify(&action, ExtensionPolicyFrontend::Interactive)
                .unwrap(),
            ExtensionPolicyDecision::Deny
        );

        policy.set_operation("browser.submit", ExtensionPolicyDecision::Allow);
        action.operation = "browser.submit".into();
        action.adapter_hints.destructive = Some(true);
        assert_eq!(
            policy
                .classify(&action, ExtensionPolicyFrontend::Interactive)
                .unwrap(),
            ExtensionPolicyDecision::Ask
        );
        assert_eq!(
            policy
                .classify(&action, ExtensionPolicyFrontend::Headless)
                .unwrap(),
            ExtensionPolicyDecision::Deny
        );
    }

    #[test]
    fn approval_is_single_use_and_bound_to_intent_generation_and_parent() {
        let store = ExtensionApprovalStore::new();
        let parent = ExtensionRequestId::Number(7);
        let action = intent("browser.submit");

        let wrong_intent_token = store
            .issue(&action, 2, parent.clone(), Duration::from_secs(30))
            .unwrap();
        assert!(!store
            .consume(&wrong_intent_token, &intent("browser.publish"), 2, &parent)
            .unwrap());
        assert!(!store
            .consume(&wrong_intent_token, &action, 2, &parent)
            .unwrap());

        let wrong_generation = store
            .issue(&action, 2, parent.clone(), Duration::from_secs(30))
            .unwrap();
        assert!(!store
            .consume(&wrong_generation, &action, 3, &parent)
            .unwrap());

        let wrong_parent = store
            .issue(&action, 2, parent.clone(), Duration::from_secs(30))
            .unwrap();
        assert!(!store
            .consume(&wrong_parent, &action, 2, &ExtensionRequestId::Number(8))
            .unwrap());

        let valid = store
            .issue(&action, 2, parent.clone(), Duration::from_secs(30))
            .unwrap();
        assert!(store.consume(&valid, &action, 2, &parent).unwrap());
        assert!(!store.consume(&valid, &action, 2, &parent).unwrap());
    }

    #[test]
    fn generation_invalidation_rejects_stale_capabilities() {
        let store = ExtensionApprovalStore::new();
        let parent = ExtensionRequestId::String("parent".into());
        let action = intent("browser.submit");
        let stale = store
            .issue(&action, 4, parent.clone(), Duration::from_secs(30))
            .unwrap();
        let live = store
            .issue(&action, 5, parent.clone(), Duration::from_secs(30))
            .unwrap();
        store.invalidate_generation(4);
        assert!(!store.consume(&stale, &action, 4, &parent).unwrap());
        assert!(store.consume(&live, &action, 5, &parent).unwrap());
    }

    #[test]
    fn approval_token_wire_is_canonical_and_debug_is_redacted() {
        let store = ExtensionApprovalStore::new();
        let action = intent("browser.submit");
        let token = store
            .issue(
                &action,
                2,
                ExtensionRequestId::Number(7),
                Duration::from_secs(30),
            )
            .unwrap();
        let wire = serde_json::to_string(&token).unwrap();
        assert_eq!(wire.len(), 66);
        assert_eq!(
            serde_json::from_str::<ExtensionApprovalToken>(&wire).unwrap(),
            token
        );
        assert_eq!(format!("{token:?}"), "ExtensionApprovalToken([REDACTED])");
        assert!(!format!("{token:?}").contains(token.as_str()));
        assert!(serde_json::from_str::<ExtensionApprovalToken>("\"approval_01\"").is_err());
        assert!(
            serde_json::from_str::<ExtensionApprovalToken>(&format!("\"{}\"", "A".repeat(64)))
                .is_err()
        );
    }
}
