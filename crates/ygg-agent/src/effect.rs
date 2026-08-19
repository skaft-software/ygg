//! Deterministic admission control for model-requested tool effects.
//!
//! The broker is deliberately host-owned: tools classify their effects in
//! trusted Rust code, model arguments are canonicalized before policy is
//! evaluated, and unknown classifications fail closed. This is an admission
//! boundary, not an OS sandbox; process and extension effects remain denied by
//! the controlled policy until an isolated execution backend exists.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use getrandom::fill as fill_random;
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::tool::ToolProgressSink;

/// Maximum semantic size of one effect intent's JSON arguments.
///
/// This permits the built-in edit tool's two independent 32 MiB text fields
/// plus bounded field/key overhead. Canonical hashing is streamed, so escaped
/// JSON does not require a second payload-sized allocation.
pub const MAX_EFFECT_INTENT_BYTES: usize = 65 * 1024 * 1024;
/// Maximum number of unconsumed effect grants held by one broker.
pub const MAX_EFFECT_GRANTS: usize = 256;
/// Maximum lifetime of an effect grant.
pub const MAX_EFFECT_GRANT_TTL: Duration = Duration::from_secs(5 * 60);
/// Version of the deterministic policy and canonical intent envelope.
pub const EFFECT_POLICY_VERSION: u64 = 1;

const APPROVED_EFFECT_GRANT_TTL: Duration = Duration::from_secs(60);
const APPROVAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_IDENTIFIER_BYTES: usize = 512;
const MAX_CANONICAL_DEPTH: usize = 64;
const MAX_CANONICAL_NODES: usize = 16 * 1024;
const EFFECT_GRANT_PREFIX: &str = "effect_";
const EFFECT_GRANT_RANDOM_BYTES: usize = 32;
const EFFECT_GRANT_HEX_BYTES: usize = EFFECT_GRANT_RANDOM_BYTES * 2;
const EFFECT_GRANT_TOKEN_BYTES: usize = EFFECT_GRANT_PREFIX.len() + EFFECT_GRANT_HEX_BYTES;
const CONFIRMATION_PREVIEW_EDGE_BYTES: usize = 160;
const CONFIRMATION_FIELD_CHARS: usize = 128;
/// Leaves more than 1 KiB for the prompt, labels, digest, and separators in
/// frontends whose complete approval action is capped at 8 KiB.
const MAX_CONFIRMATION_PROJECTION_BYTES: usize = 7 * 1024;

/// Host-owned classification of the effects a tool call can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEffect {
    /// Deterministic in-memory computation with no externally visible effect.
    Pure,
    /// Read a path whose resolved target is inside the configured workspace.
    WorkspaceRead,
    /// Read host data outside the configured workspace.
    HostRead,
    /// Mutate a path whose resolved target is inside the configured workspace.
    WorkspaceMutation,
    /// Mutate host data outside the configured workspace.
    HostMutation,
    /// Start or communicate with a native host process.
    HostProcess,
    /// Perform network I/O outside the provider transport owned by the host.
    Network,
    /// Create or control delegated model workers.
    Delegation,
    /// Invoke executable extension code.
    Extension,
    /// The host has no authoritative classification for this tool.
    Unknown,
}

impl ToolEffect {
    fn policy_label(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::WorkspaceRead => "workspace_read",
            Self::HostRead => "host_read",
            Self::WorkspaceMutation => "workspace_mutation",
            Self::HostMutation => "host_mutation",
            Self::HostProcess => "host_process",
            Self::Network => "network",
            Self::Delegation => "delegation",
            Self::Extension => "extension",
            Self::Unknown => "unknown",
        }
    }
}

/// Deterministic broker policy selected by the trusted host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EffectPolicy {
    /// Allow pure computation and workspace reads, require an exact one-shot
    /// approval for workspace mutation and potentially auto-approved known-safe
    /// `bash` process execution, and deny every other effect.
    #[default]
    Controlled,
    /// A stricter `Controlled` mode where every `bash` process execution, even
    /// known-safe commands, requires exact one-shot approval.
    ControlledBashApproval,
    /// Allow every authoritatively classified effect. Unknown tools still fail
    /// closed. This profile runs effects with the Ygg process's ambient OS
    /// authority and is suitable only inside a separately isolated environment.
    UnsafeHost,
}

/// Why an admitted effect was authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectAuthorization {
    /// The deterministic policy allowed the effect without escalation.
    Policy,
    /// A trusted frontend approved the exact canonical intent once.
    HumanGrant,
}

/// Immutable receipt returned when the broker admits an exact effect intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectReceipt {
    intent_digest: [u8; 32],
    authorization: EffectAuthorization,
    policy_version: u64,
}

impl EffectReceipt {
    /// SHA-256 digest of the canonical admitted intent.
    pub fn intent_digest(&self) -> String {
        hex_encode(&self.intent_digest)
    }

    /// Authority that admitted the effect.
    pub fn authorization(&self) -> EffectAuthorization {
        self.authorization
    }

    /// Deterministic policy version used for admission.
    pub fn policy_version(&self) -> u64 {
        self.policy_version
    }
}

/// Canonical, capability-bound description of one requested tool effect.
#[derive(Clone, PartialEq)]
pub struct EffectIntent {
    principal: String,
    run_id: String,
    generation: u64,
    request_id: String,
    tool: String,
    effect: ToolEffect,
    confirmation_arguments: String,
    digest: [u8; 32],
    /// `bash` command safety classification was conservative and precomputed
    /// during construction so host-policy decisions stay deterministic without
    /// reparsing command payloads.
    bash_requires_approval: bool,
}

impl fmt::Debug for EffectIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectIntent")
            .field("principal", &self.principal)
            .field("run_id", &self.run_id)
            .field("generation", &self.generation)
            .field("request_id", &self.request_id)
            .field("tool", &self.tool)
            .field("effect", &self.effect)
            .field("arguments", &"[REDACTED]")
            .field("digest", &self.digest())
            .finish()
    }
}

/// Fields are deliberately declared in lexicographic order. Serde preserves
/// struct field order, making this a deterministic envelope.
#[derive(Serialize)]
struct CanonicalIntentEnvelope<'a> {
    arguments: CanonicalJson<'a>,
    effect: ToolEffect,
    generation: u64,
    policy_version: u64,
    principal: &'a str,
    request_id: &'a str,
    run_id: &'a str,
    tool: &'a str,
}

/// Recursive borrowed serializer that sorts every object without cloning its
/// keys or values. This remains deterministic even if a downstream crate
/// enables serde_json's `preserve_order` feature through feature unification.
#[derive(Clone, Copy)]
struct CanonicalJson<'a>(&'a serde_json::Value);

impl Serialize for CanonicalJson<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self.0 {
            serde_json::Value::Null => serializer.serialize_unit(),
            serde_json::Value::Bool(value) => serializer.serialize_bool(*value),
            serde_json::Value::Number(value) => value.serialize(serializer),
            serde_json::Value::String(value) => serializer.serialize_str(value),
            serde_json::Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&CanonicalJson(value))?;
                }
                sequence.end()
            }
            serde_json::Value::Object(values) => {
                let mut keys = values.keys().collect::<Vec<_>>();
                keys.sort_unstable();
                let mut map = serializer.serialize_map(Some(keys.len()))?;
                for key in keys {
                    map.serialize_entry(key, &CanonicalJson(&values[key]))?;
                }
                map.end()
            }
        }
    }
}

impl EffectIntent {
    /// Validate and canonicalize one host-classified tool request.
    pub fn new(
        principal: impl Into<String>,
        run_id: impl Into<String>,
        generation: u64,
        request_id: impl Into<String>,
        tool: impl Into<String>,
        effect: ToolEffect,
        arguments: impl Borrow<serde_json::Value>,
    ) -> Result<Self, EffectBrokerError> {
        let principal = principal.into();
        let run_id = run_id.into();
        let request_id = request_id.into();
        let tool = tool.into();
        validate_identifier("principal", &principal)?;
        validate_identifier("run_id", &run_id)?;
        validate_identifier("request_id", &request_id)?;
        validate_identifier("tool", &tool)?;

        let arguments = arguments.borrow();
        let size = validate_canonical_shape(arguments)?;
        if size > MAX_EFFECT_INTENT_BYTES {
            return Err(EffectBrokerError::IntentTooLarge {
                size,
                max: MAX_EFFECT_INTENT_BYTES,
            });
        }
        let envelope = CanonicalIntentEnvelope {
            arguments: CanonicalJson(arguments),
            effect,
            generation,
            policy_version: EFFECT_POLICY_VERSION,
            principal: &principal,
            request_id: &request_id,
            run_id: &run_id,
            tool: &tool,
        };
        let digest = streaming_digest(&envelope)?;
        let confirmation_arguments = approval_projection(&tool, arguments)?;
        let bash_requires_approval = requires_bash_host_process_approval(&tool, arguments);

        Ok(Self {
            principal,
            run_id,
            generation,
            request_id,
            tool,
            effect,
            confirmation_arguments,
            digest,
            bash_requires_approval,
        })
    }

    /// Stable principal that requested the effect.
    pub fn principal(&self) -> &str {
        &self.principal
    }

    /// Host-generated run identifier.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Tool-catalog generation used to resolve the implementation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Provider request identifier bound to this one intent.
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Registered tool name.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// Host-owned effect classification.
    pub fn effect(&self) -> ToolEffect {
        self.effect
    }

    fn bash_requires_approval(&self) -> bool {
        self.bash_requires_approval
    }

    /// Bounded, spoof-resistant argument projection included in confirmations.
    /// The intent digest still binds the complete canonical argument value.
    pub fn confirmation_arguments(&self) -> &str {
        &self.confirmation_arguments
    }

    /// SHA-256 digest of the complete canonical intent envelope.
    pub fn digest(&self) -> String {
        hex_encode(&self.digest)
    }
}

/// Opaque, move-only capability for one exact canonical effect intent.
pub struct EffectGrantToken(String);

impl EffectGrantToken {
    /// Canonical wire representation of the capability.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: String) -> Result<Self, String> {
        if !valid_grant_token(&value) {
            return Err("invalid effect grant token".to_owned());
        }
        Ok(Self(value))
    }
}

impl Drop for EffectGrantToken {
    fn drop(&mut self) {
        // The token invariant is ASCII, so overwriting every byte preserves the
        // String's UTF-8 invariant. Volatile stores plus a compiler fence keep
        // the wipe from being removed as a dead store immediately before free.
        unsafe {
            for byte in self.0.as_mut_vec() {
                std::ptr::write_volatile(byte, 0);
            }
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl PartialEq for EffectGrantToken {
    fn eq(&self, other: &Self) -> bool {
        constant_time_bytes_eq(self.0.as_bytes(), other.0.as_bytes())
    }
}

impl Eq for EffectGrantToken {}

impl std::hash::Hash for EffectGrantToken {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for EffectGrantToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EffectGrantToken([REDACTED])")
    }
}

impl Serialize for EffectGrantToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EffectGrantToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug)]
struct EffectGrant {
    intent_digest: [u8; 32],
    policy_version: u64,
    expires_at: Instant,
}

#[derive(Clone)]
struct EffectGrantStore {
    // Store only a one-way verifier so a broker memory disclosure does not
    // reveal live bearer-token wire values.
    grants: Arc<Mutex<HashMap<[u8; 32], EffectGrant>>>,
}

impl Default for EffectGrantStore {
    fn default() -> Self {
        Self {
            grants: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// Deterministic reference monitor for model-requested tool effects.
#[derive(Clone)]
pub struct EffectBroker {
    policy: EffectPolicy,
    grants: EffectGrantStore,
}

impl fmt::Debug for EffectBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectBroker")
            .field("policy", &self.policy)
            .field("grants", &"[REDACTED]")
            .finish()
    }
}

/// Short-lived admission reserved for one exact intent. A reservation is
/// move-only; dropping it before commit atomically revokes its human grant.
#[must_use = "an effect reservation must be committed immediately before execution"]
pub(crate) struct EffectReservation {
    broker: EffectBroker,
    intent_digest: [u8; 32],
    authorization: EffectAuthorization,
    policy_version: u64,
    grant: Option<EffectGrantToken>,
}

impl fmt::Debug for EffectReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectReservation")
            .field("authorization", &self.authorization)
            .field("policy_version", &self.policy_version)
            .field("grant", &self.grant.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl EffectReservation {
    /// Consume this exact reservation immediately before dispatching the tool.
    pub(crate) fn commit(
        mut self,
        intent: &EffectIntent,
    ) -> Result<EffectReceipt, EffectBrokerError> {
        if self.policy_version != EFFECT_POLICY_VERSION
            || !constant_time_eq(&self.intent_digest, &intent.digest)
        {
            return Err(EffectBrokerError::GrantRejected);
        }
        if let Some(grant) = self.grant.take() {
            if !self
                .broker
                .consume_grant_digest(grant, &self.intent_digest)?
            {
                return Err(EffectBrokerError::GrantRejected);
            }
        }
        Ok(EffectReceipt {
            intent_digest: self.intent_digest,
            authorization: self.authorization,
            policy_version: self.policy_version,
        })
    }
}

impl Drop for EffectReservation {
    fn drop(&mut self) {
        if let Some(grant) = self.grant.take() {
            // A poisoned store is already fail-closed. The bearer token is
            // still wiped on drop and cannot subsequently be presented.
            let _ = self.broker.revoke_grant(grant);
        }
    }
}

impl EffectBroker {
    /// Construct an empty broker with the selected deterministic policy.
    pub fn new(policy: EffectPolicy) -> Self {
        Self {
            policy,
            grants: EffectGrantStore::default(),
        }
    }

    /// Selected deterministic policy.
    pub fn policy(&self) -> EffectPolicy {
        self.policy
    }

    /// Issue a bounded capability for an exact intent. Only trusted host code
    /// may call this after obtaining approval; tools never receive the broker.
    pub fn issue_grant(
        &self,
        intent: &EffectIntent,
        ttl: Duration,
    ) -> Result<EffectGrantToken, EffectBrokerError> {
        if ttl.is_zero() || ttl > MAX_EFFECT_GRANT_TTL {
            return Err(EffectBrokerError::InvalidGrantTtl);
        }
        let now = Instant::now();
        let expires_at = now
            .checked_add(ttl)
            .ok_or(EffectBrokerError::InvalidGrantTtl)?;
        let mut grants = self
            .grants
            .grants
            .lock()
            .map_err(|_| EffectBrokerError::GrantStoreUnavailable)?;
        grants.retain(|_, grant| grant.expires_at > now);
        if grants.len() >= MAX_EFFECT_GRANTS {
            return Err(EffectBrokerError::GrantCapacity);
        }

        for _ in 0..8 {
            let mut random = [0u8; EFFECT_GRANT_RANDOM_BYTES];
            fill_random(&mut random).map_err(|_| EffectBrokerError::EntropyUnavailable)?;
            let token = format!("{EFFECT_GRANT_PREFIX}{}", hex_encode(&random));
            random.fill(0);
            let verifier = grant_token_verifier(&token);
            if grants.contains_key(&verifier) {
                continue;
            }
            grants.insert(
                verifier,
                EffectGrant {
                    intent_digest: intent.digest,
                    policy_version: EFFECT_POLICY_VERSION,
                    expires_at,
                },
            );
            return Ok(EffectGrantToken(token));
        }
        Err(EffectBrokerError::EntropyUnavailable)
    }

    /// Atomically consume a capability. This takes ownership so the caller
    /// cannot accidentally retain the live token after use. Every presented
    /// verifier is removed before validation, so a mismatch, expiry, race, or
    /// replay fails closed.
    pub fn consume_grant(
        &self,
        token: EffectGrantToken,
        intent: &EffectIntent,
    ) -> Result<bool, EffectBrokerError> {
        self.consume_grant_digest(token, &intent.digest)
    }

    fn consume_grant_digest(
        &self,
        token: EffectGrantToken,
        intent_digest: &[u8; 32],
    ) -> Result<bool, EffectBrokerError> {
        let verifier = grant_token_verifier(token.as_str());
        let mut grants = self
            .grants
            .grants
            .lock()
            .map_err(|_| EffectBrokerError::GrantStoreUnavailable)?;
        let Some(grant) = grants.remove(&verifier) else {
            return Ok(false);
        };
        if grant.expires_at <= Instant::now() || grant.policy_version != EFFECT_POLICY_VERSION {
            return Ok(false);
        }
        Ok(constant_time_eq(&grant.intent_digest, intent_digest))
    }

    fn revoke_grant(&self, token: EffectGrantToken) -> Result<(), EffectBrokerError> {
        let verifier = grant_token_verifier(token.as_str());
        let mut grants = self
            .grants
            .grants
            .lock()
            .map_err(|_| EffectBrokerError::GrantStoreUnavailable)?;
        grants.remove(&verifier);
        Ok(())
    }

    /// Reserve one exact intent, optionally escalating a workspace mutation to
    /// a trusted interactive frontend. Callers must commit the returned
    /// reservation immediately before execution; dropping it revokes approval.
    pub(crate) async fn reserve(
        &self,
        intent: &EffectIntent,
        progress: Option<&ToolProgressSink>,
    ) -> Result<EffectReservation, EffectBrokerError> {
        let (authorization, grant) = match self.requirement(intent) {
            EffectRequirement::Allow => (EffectAuthorization::Policy, None),
            EffectRequirement::Deny(reason) => {
                return Err(EffectBrokerError::Denied {
                    tool: intent.tool.clone(),
                    effect: intent.effect,
                    reason,
                });
            }
            EffectRequirement::Approval => {
                let Some(progress) = progress else {
                    return Err(EffectBrokerError::ApprovalUnavailable {
                        tool: intent.tool.clone(),
                    });
                };
                let detail = format!(
                    "effect: {}\ncomplete intent sha256: {}\nbounded argument projection:\n{}",
                    intent.effect.policy_label(),
                    intent.digest(),
                    intent.confirmation_arguments,
                );
                let approved = tokio::time::timeout(
                    APPROVAL_RESPONSE_TIMEOUT,
                    progress.confirmation(
                        format!("Approve one exact `{}` tool effect?", intent.tool),
                        Some(detail),
                        true,
                        false,
                    ),
                )
                .await
                .unwrap_or(false);
                if !approved {
                    return Err(EffectBrokerError::ApprovalDenied {
                        tool: intent.tool.clone(),
                    });
                }
                (
                    EffectAuthorization::HumanGrant,
                    Some(self.issue_grant(intent, APPROVED_EFFECT_GRANT_TTL)?),
                )
            }
        };
        Ok(EffectReservation {
            broker: self.clone(),
            intent_digest: intent.digest,
            authorization,
            policy_version: EFFECT_POLICY_VERSION,
            grant,
        })
    }

    /// Reserve and immediately commit one exact intent. Agent dispatch uses the
    /// two-phase crate-visible API so hooks run before the short-lived grant is
    /// consumed; direct broker clients retain this convenience operation.
    pub async fn authorize(
        &self,
        intent: &EffectIntent,
        progress: Option<&ToolProgressSink>,
    ) -> Result<EffectReceipt, EffectBrokerError> {
        self.reserve(intent, progress).await?.commit(intent)
    }

    fn requirement(&self, intent: &EffectIntent) -> EffectRequirement {
        if intent.effect == ToolEffect::Unknown {
            return EffectRequirement::Deny(
                "tool has no host-owned effect classification".to_owned(),
            );
        }

        let is_bash = intent.tool() == "bash";
        let bash_requires_approval = match self.policy {
            EffectPolicy::UnsafeHost => false,
            EffectPolicy::ControlledBashApproval => is_bash,
            EffectPolicy::Controlled => is_bash && intent.bash_requires_approval(),
        };

        match self.policy {
            EffectPolicy::UnsafeHost => EffectRequirement::Allow,
            EffectPolicy::Controlled | EffectPolicy::ControlledBashApproval => {
                match intent.effect {
                    ToolEffect::Pure | ToolEffect::WorkspaceRead => EffectRequirement::Allow,
                    ToolEffect::WorkspaceMutation => EffectRequirement::Approval,
                    ToolEffect::HostRead => EffectRequirement::Deny(
                        "reading outside the workspace is unavailable in controlled mode"
                            .to_owned(),
                    ),
                    ToolEffect::HostMutation => EffectRequirement::Deny(
                        "mutating outside the workspace is unavailable in controlled mode"
                            .to_owned(),
                    ),
                    ToolEffect::HostProcess => {
                        if is_bash {
                            if bash_requires_approval {
                                EffectRequirement::Approval
                            } else {
                                EffectRequirement::Allow
                            }
                        } else {
                            EffectRequirement::Deny(
                                "native execution requires an OS or VM isolation backend"
                                    .to_owned(),
                            )
                        }
                    }
                    ToolEffect::Network => EffectRequirement::Deny(
                        "tool networking requires a trusted egress broker".to_owned(),
                    ),
                    ToolEffect::Delegation => EffectRequirement::Deny(
                        "delegation requires attenuated authority and a team-wide budget ledger"
                            .to_owned(),
                    ),
                    ToolEffect::Extension => EffectRequirement::Deny(
                        "executable extensions require an OS or VM isolation backend".to_owned(),
                    ),
                    ToolEffect::Unknown => unreachable!("unknown effects fail closed above"),
                }
            }
        }
    }
}

impl Default for EffectBroker {
    fn default() -> Self {
        Self::new(EffectPolicy::Controlled)
    }
}

fn requires_bash_host_process_approval(tool: &str, arguments: &serde_json::Value) -> bool {
    if tool != "bash" {
        return false;
    }
    let Some(values) = arguments.as_object() else {
        return true;
    };
    let Some(command) = values.get("command").and_then(serde_json::Value::as_str) else {
        return true;
    };
    !is_known_safe_bash_command(command)
}

fn is_known_safe_bash_command(command: &str) -> bool {
    crate::shell_safety::is_known_safe_bash_command(command)
}

#[derive(Debug)]
enum EffectRequirement {
    Allow,
    Approval,
    Deny(String),
}

/// Deterministic effect admission failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EffectBrokerError {
    /// An identifier or JSON envelope was invalid.
    #[error("invalid effect intent: {0}")]
    InvalidIntent(String),
    /// The canonical intent exceeded the broker boundary.
    #[error("effect intent is {size} bytes; maximum is {max}")]
    IntentTooLarge {
        /// Actual canonical byte size.
        size: usize,
        /// Maximum accepted canonical byte size.
        max: usize,
    },
    /// Deterministic policy denied the effect.
    #[error("effect broker denied tool `{tool}` ({effect:?}): {reason}")]
    Denied {
        /// Registered tool name.
        tool: String,
        /// Host-owned effect class.
        effect: ToolEffect,
        /// Stable denial reason.
        reason: String,
    },
    /// The effect required an interactive authority source that was absent.
    #[error("effect broker denied tool `{tool}`: exact approval is unavailable")]
    ApprovalUnavailable {
        /// Registered tool name.
        tool: String,
    },
    /// The trusted frontend rejected or dropped the approval request.
    #[error("effect broker denied tool `{tool}`: exact approval was not granted")]
    ApprovalDenied {
        /// Registered tool name.
        tool: String,
    },
    /// Grant lifetime was zero, overflowing, or above the maximum.
    #[error("invalid effect grant lifetime")]
    InvalidGrantTtl,
    /// Too many live grants exist.
    #[error("effect grant capacity reached")]
    GrantCapacity,
    /// Secure random bytes could not be obtained.
    #[error("secure randomness unavailable for effect grant")]
    EntropyUnavailable,
    /// The synchronized grant store was poisoned.
    #[error("effect grant store unavailable")]
    GrantStoreUnavailable,
    /// A newly issued grant could not be consumed for the same intent.
    #[error("effect grant was rejected")]
    GrantRejected,
}

fn validate_identifier(label: &str, value: &str) -> Result<(), EffectBrokerError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        return Err(EffectBrokerError::InvalidIntent(format!(
            "{label} must be non-empty, control-free, and at most {MAX_IDENTIFIER_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_canonical_shape(value: &serde_json::Value) -> Result<usize, EffectBrokerError> {
    fn charge(bytes: &mut usize, amount: usize) -> Result<(), EffectBrokerError> {
        *bytes = bytes.saturating_add(amount);
        if *bytes > MAX_EFFECT_INTENT_BYTES {
            return Err(EffectBrokerError::IntentTooLarge {
                size: *bytes,
                max: MAX_EFFECT_INTENT_BYTES,
            });
        }
        Ok(())
    }

    fn visit(
        value: &serde_json::Value,
        depth: usize,
        nodes: &mut usize,
        bytes: &mut usize,
    ) -> Result<(), EffectBrokerError> {
        if depth > MAX_CANONICAL_DEPTH {
            return Err(EffectBrokerError::InvalidIntent(format!(
                "JSON nesting exceeds {MAX_CANONICAL_DEPTH}"
            )));
        }
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_CANONICAL_NODES {
            return Err(EffectBrokerError::InvalidIntent(format!(
                "JSON node count exceeds {MAX_CANONICAL_NODES}"
            )));
        }
        charge(bytes, 1)?;
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) => {}
            serde_json::Value::Number(value) => charge(bytes, value.to_string().len())?,
            serde_json::Value::String(value) => charge(bytes, value.len())?,
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, depth + 1, nodes, bytes)?;
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    charge(bytes, key.len())?;
                    visit(value, depth + 1, nodes, bytes)?;
                }
            }
        }
        Ok(())
    }

    let mut nodes = 0;
    let mut bytes = 0;
    visit(value, 0, &mut nodes, &mut bytes)?;
    Ok(bytes)
}

#[derive(Default)]
struct DigestWriter(Sha256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Digest::update(&mut self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn streaming_digest(value: &impl Serialize) -> Result<[u8; 32], EffectBrokerError> {
    let mut writer = DigestWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| EffectBrokerError::InvalidIntent(error.to_string()))?;
    Ok(writer.0.finalize().into())
}

struct PreviewWriter {
    digest: Sha256,
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl PreviewWriter {
    fn new() -> Self {
        Self {
            digest: Sha256::new(),
            head: Vec::with_capacity(CONFIRMATION_PREVIEW_EDGE_BYTES),
            tail: Vec::with_capacity(CONFIRMATION_PREVIEW_EDGE_BYTES),
            total: 0,
        }
    }

    fn finish(self) -> (Vec<u8>, Vec<u8>, usize, [u8; 32]) {
        (
            self.head,
            self.tail,
            self.total,
            self.digest.finalize().into(),
        )
    }
}

impl Write for PreviewWriter {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let written = bytes.len();
        Digest::update(&mut self.digest, bytes);
        self.total = self.total.saturating_add(bytes.len());

        let head_remaining = CONFIRMATION_PREVIEW_EDGE_BYTES.saturating_sub(self.head.len());
        let take = head_remaining.min(bytes.len());
        self.head.extend_from_slice(&bytes[..take]);
        bytes = &bytes[take..];
        if bytes.len() >= CONFIRMATION_PREVIEW_EDGE_BYTES {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - CONFIRMATION_PREVIEW_EDGE_BYTES..]);
        } else if !bytes.is_empty() {
            let overflow = self
                .tail
                .len()
                .saturating_add(bytes.len())
                .saturating_sub(CONFIRMATION_PREVIEW_EDGE_BYTES);
            if overflow > 0 {
                self.tail.copy_within(overflow.., 0);
                self.tail.truncate(self.tail.len() - overflow);
            }
            self.tail.extend_from_slice(bytes);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn approval_projection(
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<String, EffectBrokerError> {
    let mut projection = String::new();
    if let Some(values) = arguments.as_object() {
        let fields: &[&str] = match tool {
            "write" => &["path", "content", "expected_hash"],
            "edit" => &["path", "old", "new", "expected_hash"],
            _ => &["path"],
        };
        for field in fields {
            if let Some(value) = values.get(*field) {
                projection.push_str(&project_field(field, value)?);
                projection.push('\n');
            }
        }
    }

    let mut writer = PreviewWriter::new();
    serde_json::to_writer(&mut writer, &CanonicalJson(arguments))
        .map_err(|error| EffectBrokerError::InvalidIntent(error.to_string()))?;
    let (head, tail, total, argument_digest) = writer.finish();
    if total <= CONFIRMATION_PREVIEW_EDGE_BYTES * 2 {
        let mut complete = head;
        complete.extend_from_slice(&tail);
        projection.push_str("complete canonical arguments: ");
        projection.push_str(&sanitize_json_bytes(&complete));
    } else {
        let omitted = total.saturating_sub(head.len() + tail.len());
        projection.push_str("canonical argument preview (NOT COMPLETE): ");
        projection.push_str(&sanitize_json_bytes(&head));
        projection.push_str(&format!(
            "\n<<< OMITTED {omitted} CANONICAL JSON BYTES >>>\n"
        ));
        projection.push_str(&sanitize_json_bytes(&tail));
        projection.push_str(&format!(
            "\ncanonical arguments sha256 (arguments only, not the complete intent): {}",
            hex_encode(&argument_digest)
        ));
    }
    if projection.len() > MAX_CONFIRMATION_PROJECTION_BYTES {
        return Err(EffectBrokerError::InvalidIntent(format!(
            "bounded approval projection exceeds {MAX_CONFIRMATION_PROJECTION_BYTES} bytes"
        )));
    }
    Ok(projection)
}

fn project_field(name: &str, value: &serde_json::Value) -> Result<String, EffectBrokerError> {
    let Some(value) = value.as_str() else {
        let mut writer = PreviewWriter::new();
        serde_json::to_writer(&mut writer, &CanonicalJson(value))
            .map_err(|error| EffectBrokerError::InvalidIntent(error.to_string()))?;
        let (head, tail, total, digest) = writer.finish();
        let mut preview = head;
        if total > CONFIRMATION_PREVIEW_EDGE_BYTES * 2 {
            preview.extend_from_slice(b"<<< OMITTED >>>");
        }
        preview.extend_from_slice(&tail);
        return Ok(format!(
            "{name}: non-string JSON value; {total} canonical bytes; field sha256: {}; preview: {}",
            hex_encode(&digest),
            sanitize_json_bytes(&preview)
        ));
    };
    if value.chars().count() <= CONFIRMATION_FIELD_CHARS {
        let encoded = serde_json::to_vec(value)
            .map_err(|error| EffectBrokerError::InvalidIntent(error.to_string()))?;
        return Ok(format!("{name}: {}", sanitize_json_bytes(&encoded)));
    }

    let preview = clip_chars(value, CONFIRMATION_FIELD_CHARS);
    let encoded = serde_json::to_vec(&preview)
        .map_err(|error| EffectBrokerError::InvalidIntent(error.to_string()))?;
    Ok(format!(
        "{name}: {} bytes; field sha256: {}; escaped UTF-8 head/tail preview with omission marker: {}",
        value.len(),
        hex_encode(&Sha256::digest(value.as_bytes())),
        sanitize_json_bytes(&encoded)
    ))
}

fn sanitize_json_bytes(mut bytes: &[u8]) -> String {
    let mut display = String::with_capacity(bytes.len());
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(value) => {
                append_sanitized_unicode(&mut display, value);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // SAFETY: `valid_up_to` is guaranteed to delimit UTF-8.
                    let value = unsafe { std::str::from_utf8_unchecked(&bytes[..valid]) };
                    append_sanitized_unicode(&mut display, value);
                }
                let invalid = error
                    .error_len()
                    .unwrap_or_else(|| bytes.len().saturating_sub(valid));
                for byte in &bytes[valid..valid + invalid] {
                    display.push_str(&format!("\\x{byte:02x}"));
                }
                bytes = &bytes[valid + invalid..];
            }
        }
    }
    display
}

fn append_sanitized_unicode(display: &mut String, value: &str) {
    for character in value.chars() {
        if character.is_ascii() {
            // serde_json already escaped ASCII controls. Preserve JSON syntax.
            display.push(character);
        } else {
            // Expose bidi, zero-width, and other formatting controls while
            // preserving ordinary printable Unicode.
            display.extend(character.escape_debug());
        }
    }
}

fn valid_grant_token(value: &str) -> bool {
    value.len() == EFFECT_GRANT_TOKEN_BYTES
        && value.starts_with(EFFECT_GRANT_PREFIX)
        && value[EFFECT_GRANT_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn grant_token_verifier(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn constant_time_bytes_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && bool::from(left.ct_eq(right))
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    bool::from(left.ct_eq(right))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn clip_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    let kept = max_chars.saturating_sub(32) / 2;
    let head = value.chars().take(kept).collect::<String>();
    let tail = value
        .chars()
        .rev()
        .take(kept)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    let omitted = count.saturating_sub(head.chars().count() + tail.chars().count());
    format!("{head}<<< OMITTED {omitted} UTF-8 CHARACTERS >>>{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolProgress;

    async fn approve_reservation(
        broker: &EffectBroker,
        intent: &EffectIntent,
    ) -> (EffectReservation, String) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let sink = ToolProgressSink::live(tx);
        let responder = tokio::spawn(async move {
            let Some(ToolProgress::Confirmation(request)) = rx.recv().await else {
                panic!("expected one effect confirmation");
            };
            assert!(request.destructive);
            assert!(!request.default);
            let detail = request.detail.clone().expect("effect detail");
            request.respond(true);
            detail
        });
        let reservation = broker.reserve(intent, Some(&sink)).await.unwrap();
        let detail = responder.await.unwrap();
        (reservation, detail)
    }

    fn intent(arguments: serde_json::Value) -> EffectIntent {
        EffectIntent::new(
            "principal",
            "run-1",
            7,
            "call-1",
            "write",
            ToolEffect::WorkspaceMutation,
            arguments,
        )
        .unwrap()
    }

    fn duplicate_token(token: &EffectGrantToken) -> EffectGrantToken {
        EffectGrantToken::parse(token.as_str().to_owned()).unwrap()
    }

    #[test]
    fn canonical_digest_is_independent_of_object_insertion_order() {
        let left = intent(serde_json::json!({"path": "a", "content": {"b": 2, "a": 1}}));
        let right = intent(serde_json::json!({"content": {"a": 1, "b": 2}, "path": "a"}));
        assert_eq!(left.digest(), right.digest());
        assert_eq!(
            left.confirmation_arguments(),
            right.confirmation_arguments()
        );
        assert_eq!(
            left.digest(),
            "6e66594192f9e608344971e73142af1d24335437dccef8b70d7f606c4d0afb67"
        );
    }

    #[test]
    fn mutation_projection_is_bounded_exact_when_small_and_honest_when_elided() {
        let small = intent(serde_json::json!({
            "path": "src/main.rs",
            "content": "fn main() {}\n"
        }));
        let small_projection = small.confirmation_arguments();
        assert!(small_projection.contains("path: \"src/main.rs\""));
        assert!(small_projection.contains("content: \"fn main() {}\\n\""));
        assert!(small_projection.contains("complete canonical arguments:"));
        assert!(small_projection.len() <= MAX_CONFIRMATION_PROJECTION_BYTES);

        let left_content = format!("{}MIDDLE-LEFT{}", "a".repeat(2_000), "z".repeat(2_000));
        let right_content = format!("{}MIDDLE-RGHT{}", "a".repeat(2_000), "z".repeat(2_000));
        let left = intent(serde_json::json!({"path": "a", "content": left_content}));
        let right = intent(serde_json::json!({"path": "a", "content": right_content}));
        let projection = left.confirmation_arguments();
        assert!(projection.contains("field sha256:"));
        assert!(projection.contains("<<< OMITTED"));
        assert!(projection.contains("not the complete intent"));
        assert!(!projection.contains("MIDDLE-LEFT"));
        assert_ne!(
            left.confirmation_arguments(),
            right.confirmation_arguments()
        );
        assert_ne!(left.digest(), right.digest());
        assert!(projection.len() <= MAX_CONFIRMATION_PROJECTION_BYTES);

        let spoof = intent(serde_json::json!({
            "path": "safe\u{202e}txt",
            "content": "ok"
        }));
        assert!(spoof.confirmation_arguments().contains("\\u{202e}"));
        assert!(!spoof.confirmation_arguments().contains('\u{202e}'));
    }

    #[test]
    fn file_tool_payload_contract_fits_streaming_intent_boundary() {
        const FILE_BYTES: usize = 32 * 1024 * 1024;
        let edit = EffectIntent::new(
            "principal",
            "run-1",
            7,
            "call-1",
            "edit",
            ToolEffect::WorkspaceMutation,
            serde_json::json!({
                "path": "a",
                "old": "o".repeat(FILE_BYTES),
                "new": "n".repeat(FILE_BYTES),
            }),
        );
        assert!(edit.is_ok());
        drop(edit);

        let oversized = EffectIntent::new(
            "principal",
            "run-1",
            7,
            "call-1",
            "write",
            ToolEffect::WorkspaceMutation,
            serde_json::Value::String("x".repeat(MAX_EFFECT_INTENT_BYTES)),
        );
        assert!(matches!(
            oversized,
            Err(EffectBrokerError::IntentTooLarge { .. })
        ));

        fn nested_arrays(depth: usize) -> serde_json::Value {
            (0..depth).fold(serde_json::Value::Null, |value, _| {
                serde_json::Value::Array(vec![value])
            })
        }
        assert!(EffectIntent::new(
            "p",
            "r",
            0,
            "c",
            "read",
            ToolEffect::WorkspaceRead,
            nested_arrays(MAX_CANONICAL_DEPTH),
        )
        .is_ok());
        let too_deep = EffectIntent::new(
            "p",
            "r",
            0,
            "c",
            "read",
            ToolEffect::WorkspaceRead,
            nested_arrays(MAX_CANONICAL_DEPTH + 1),
        )
        .unwrap_err();
        assert!(matches!(&too_deep, EffectBrokerError::InvalidIntent(_)));
        assert!(too_deep.to_string().contains("nesting exceeds"));

        let at_node_limit =
            serde_json::Value::Array(vec![serde_json::Value::Null; MAX_CANONICAL_NODES - 1]);
        assert!(EffectIntent::new(
            "p",
            "r",
            0,
            "c",
            "read",
            ToolEffect::WorkspaceRead,
            &at_node_limit,
        )
        .is_ok());
        let over_node_limit =
            serde_json::Value::Array(vec![serde_json::Value::Null; MAX_CANONICAL_NODES]);
        let too_many_nodes = EffectIntent::new(
            "p",
            "r",
            0,
            "c",
            "read",
            ToolEffect::WorkspaceRead,
            &over_node_limit,
        )
        .unwrap_err();
        assert!(matches!(
            &too_many_nodes,
            EffectBrokerError::InvalidIntent(_)
        ));
        assert!(too_many_nodes.to_string().contains("node count"));
    }

    #[test]
    fn grant_is_single_use_and_mismatch_consumes_it() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let original = intent(serde_json::json!({"path": "a", "content": "one"}));
        let changed = intent(serde_json::json!({"path": "a", "content": "two"}));

        let mismatch = broker
            .issue_grant(&original, Duration::from_secs(30))
            .unwrap();
        let mismatch_replay = duplicate_token(&mismatch);
        assert!(!broker.consume_grant(mismatch, &changed).unwrap());
        assert!(!broker.consume_grant(mismatch_replay, &original).unwrap());

        let valid = broker
            .issue_grant(&original, Duration::from_secs(30))
            .unwrap();
        let valid_replay = duplicate_token(&valid);
        assert!(broker.consume_grant(valid, &original).unwrap());
        assert!(!broker.consume_grant(valid_replay, &original).unwrap());
    }

    #[test]
    fn grant_binds_principal_run_generation_request_and_tool() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let original = intent(serde_json::json!({"path": "a"}));
        let variants = [
            EffectIntent::new(
                "other",
                "run-1",
                7,
                "call-1",
                "write",
                ToolEffect::WorkspaceMutation,
                serde_json::json!({"path": "a"}),
            )
            .unwrap(),
            EffectIntent::new(
                "principal",
                "run-2",
                7,
                "call-1",
                "write",
                ToolEffect::WorkspaceMutation,
                serde_json::json!({"path": "a"}),
            )
            .unwrap(),
            EffectIntent::new(
                "principal",
                "run-1",
                8,
                "call-1",
                "write",
                ToolEffect::WorkspaceMutation,
                serde_json::json!({"path": "a"}),
            )
            .unwrap(),
            EffectIntent::new(
                "principal",
                "run-1",
                7,
                "call-2",
                "write",
                ToolEffect::WorkspaceMutation,
                serde_json::json!({"path": "a"}),
            )
            .unwrap(),
            EffectIntent::new(
                "principal",
                "run-1",
                7,
                "call-1",
                "edit",
                ToolEffect::WorkspaceMutation,
                serde_json::json!({"path": "a"}),
            )
            .unwrap(),
        ];
        for variant in variants {
            let token = broker
                .issue_grant(&original, Duration::from_secs(30))
                .unwrap();
            assert!(!broker.consume_grant(token, &variant).unwrap());
        }
    }

    #[tokio::test]
    async fn reservation_commit_is_exact_and_drop_revokes_the_grant() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let original = intent(serde_json::json!({"path": "a", "content": "one"}));
        let changed = intent(serde_json::json!({"path": "a", "content": "two"}));

        let (reservation, detail) = approve_reservation(&broker, &original).await;
        assert!(detail.contains("complete intent sha256:"));
        assert!(detail.contains(&original.digest()));
        assert!(detail.len() + "Approve one exact `write` tool effect?".len() < 8 * 1024);
        let replay = duplicate_token(reservation.grant.as_ref().unwrap());
        assert!(matches!(
            reservation.commit(&changed),
            Err(EffectBrokerError::GrantRejected)
        ));
        assert!(!broker.consume_grant(replay, &original).unwrap());

        let (reservation, _) = approve_reservation(&broker, &original).await;
        let replay = duplicate_token(reservation.grant.as_ref().unwrap());
        drop(reservation);
        assert!(!broker.consume_grant(replay, &original).unwrap());

        let (reservation, _) = approve_reservation(&broker, &original).await;
        let receipt = reservation.commit(&original).unwrap();
        assert_eq!(receipt.authorization(), EffectAuthorization::HumanGrant);
        assert_eq!(receipt.intent_digest(), original.digest());
    }

    #[test]
    fn expired_grants_ttl_bounds_and_capacity_fail_closed() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let intent = intent(serde_json::json!({"path": "a"}));
        assert!(matches!(
            broker.issue_grant(&intent, Duration::ZERO),
            Err(EffectBrokerError::InvalidGrantTtl)
        ));
        assert!(matches!(
            broker.issue_grant(&intent, MAX_EFFECT_GRANT_TTL + Duration::from_nanos(1)),
            Err(EffectBrokerError::InvalidGrantTtl)
        ));

        let expired = broker
            .issue_grant(&intent, Duration::from_millis(1))
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));
        assert!(!broker.consume_grant(expired, &intent).unwrap());

        let reservation = EffectReservation {
            broker: broker.clone(),
            intent_digest: intent.digest,
            authorization: EffectAuthorization::HumanGrant,
            policy_version: EFFECT_POLICY_VERSION,
            grant: Some(
                broker
                    .issue_grant(&intent, Duration::from_millis(1))
                    .unwrap(),
            ),
        };
        std::thread::sleep(Duration::from_millis(5));
        assert!(matches!(
            reservation.commit(&intent),
            Err(EffectBrokerError::GrantRejected)
        ));

        let mut live = Vec::with_capacity(MAX_EFFECT_GRANTS);
        for _ in 0..MAX_EFFECT_GRANTS {
            live.push(broker.issue_grant(&intent, MAX_EFFECT_GRANT_TTL).unwrap());
        }
        assert!(matches!(
            broker.issue_grant(&intent, MAX_EFFECT_GRANT_TTL),
            Err(EffectBrokerError::GrantCapacity)
        ));
        assert!(broker.consume_grant(live.pop().unwrap(), &intent).unwrap());
        assert!(broker.issue_grant(&intent, MAX_EFFECT_GRANT_TTL).is_ok());
    }

    #[test]
    fn concurrent_grant_consumption_has_exactly_one_winner() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let intent = std::sync::Arc::new(intent(serde_json::json!({"path": "a"})));
        let token = broker
            .issue_grant(&intent, Duration::from_secs(30))
            .unwrap();
        let wire = token.as_str().to_owned();
        drop(token);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut threads = Vec::new();
        for _ in 0..16 {
            let broker = broker.clone();
            let intent = intent.clone();
            let barrier = barrier.clone();
            let wire = wire.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                broker
                    .consume_grant(EffectGrantToken::parse(wire).unwrap(), &intent)
                    .unwrap()
            }));
        }
        let winners: usize = threads
            .into_iter()
            .map(|thread| usize::from(thread.join().unwrap()))
            .sum();
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn unknown_effect_fails_closed_even_in_unsafe_profile() {
        let broker = EffectBroker::new(EffectPolicy::UnsafeHost);
        let unknown = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "mystery",
            ToolEffect::Unknown,
            serde_json::json!({}),
        )
        .unwrap();
        let error = broker.authorize(&unknown, None).await.unwrap_err();
        assert!(matches!(error, EffectBrokerError::Denied { .. }));
    }

    #[tokio::test]
    async fn controlled_policy_denies_unisolated_effects_without_approval_channel() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        for effect in [
            ToolEffect::HostRead,
            ToolEffect::HostMutation,
            ToolEffect::Network,
            ToolEffect::Delegation,
            ToolEffect::Extension,
            ToolEffect::Unknown,
        ] {
            let intent = EffectIntent::new(
                "principal",
                "run-1",
                1,
                "call-1",
                "tool",
                effect,
                serde_json::json!({}),
            )
            .unwrap();
            assert!(matches!(
                broker.authorize(&intent, None).await,
                Err(EffectBrokerError::Denied { .. })
            ));
        }
        let bash = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({}),
        )
        .unwrap();
        assert!(matches!(
            broker.authorize(&bash, None).await,
            Err(EffectBrokerError::ApprovalUnavailable { .. })
        ));
        let process = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "search",
            ToolEffect::HostProcess,
            serde_json::json!({}),
        )
        .unwrap();
        assert!(matches!(
            broker.authorize(&process, None).await,
            Err(EffectBrokerError::Denied { .. })
        ));
        let mutation = intent(serde_json::json!({"path": "a"}));
        assert!(matches!(
            broker.authorize(&mutation, None).await,
            Err(EffectBrokerError::ApprovalUnavailable { .. })
        ));
    }

    #[test]
    fn known_safe_bash_command_matches_codex_like_sequences() {
        assert!(is_known_safe_bash_command("ls && pwd"));
        assert!(is_known_safe_bash_command("echo 'hi' ; ls | wc -l"));
        assert!(is_known_safe_bash_command(
            "bash -lc \"ls && grep 'fn' Cargo.toml\""
        ));

        assert!(!is_known_safe_bash_command("ls || (pwd && echo hi)"));
        assert!(!is_known_safe_bash_command(
            "printf 'owned' > /tmp/owned.txt"
        ));
        assert!(!is_known_safe_bash_command("find . -name file.txt -delete"));
        assert!(!is_known_safe_bash_command("bash -ic 'ls'"));
    }

    #[tokio::test]
    async fn controlled_policy_approves_safe_bash_without_progress_channel() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let safe = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({"command": "ls -la"}),
        )
        .unwrap();
        assert!(broker.authorize(&safe, None).await.is_ok());
    }

    #[tokio::test]
    async fn controlled_policy_approves_complex_safe_bash_sequence_without_progress_channel() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let safe = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({"command": "ls && pwd; echo 'hi there' | wc -l"}),
        )
        .unwrap();
        assert!(broker.authorize(&safe, None).await.is_ok());
    }

    #[tokio::test]
    async fn controlled_policy_requires_approval_for_complex_bash_without_progress_channel() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let complex = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({"command": "printf 'owned' > /tmp/owned.txt"}),
        )
        .unwrap();
        assert!(matches!(
            broker.authorize(&complex, None).await,
            Err(EffectBrokerError::ApprovalUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn controlled_policy_requires_approval_for_unsafe_wrapped_bash_without_progress_channel()
    {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let complex = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({"command": "bash -lc 'printf \'owned\' > /tmp/owned.txt'"}),
        )
        .unwrap();
        assert!(matches!(
            broker.authorize(&complex, None).await,
            Err(EffectBrokerError::ApprovalUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn controlled_bash_approval_profile_requires_all_bash_approvals_without_progress_channel()
    {
        let broker = EffectBroker::new(EffectPolicy::ControlledBashApproval);
        let safe = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "bash",
            ToolEffect::HostProcess,
            serde_json::json!({"command": "ls"}),
        )
        .unwrap();
        assert!(matches!(
            broker.authorize(&safe, None).await,
            Err(EffectBrokerError::ApprovalUnavailable { .. })
        ));
    }

    #[test]
    fn grant_wire_and_sensitive_debug_output_are_redacted() {
        let broker = EffectBroker::new(EffectPolicy::Controlled);
        let secret = "never-log-this-model-payload";
        let intent = intent(serde_json::json!({"path": "a", "content": secret}));
        let intent_debug = format!("{intent:?}");
        assert!(intent_debug.contains("[REDACTED]"));
        assert!(!intent_debug.contains(secret));
        let token = broker
            .issue_grant(&intent, Duration::from_secs(30))
            .unwrap();
        let wire = serde_json::to_string(&token).unwrap();
        assert_eq!(wire.len(), EFFECT_GRANT_TOKEN_BYTES + 2);
        assert_eq!(
            serde_json::from_str::<EffectGrantToken>(&wire).unwrap(),
            token
        );
        assert_eq!(format!("{token:?}"), "EffectGrantToken([REDACTED])");
        assert!(!format!("{token:?}").contains(token.as_str()));
    }

    #[test]
    fn oversized_and_deep_intents_are_rejected() {
        let oversized = EffectIntent::new(
            "principal",
            "run-1",
            1,
            "call-1",
            "write",
            ToolEffect::WorkspaceMutation,
            serde_json::json!({"content": "x".repeat(MAX_EFFECT_INTENT_BYTES)}),
        );
        assert!(matches!(
            oversized,
            Err(EffectBrokerError::IntentTooLarge { .. })
        ));

        let mut deep = serde_json::Value::Null;
        for _ in 0..=MAX_CANONICAL_DEPTH {
            deep = serde_json::json!([deep]);
        }
        assert!(matches!(
            EffectIntent::new(
                "principal",
                "run-1",
                1,
                "call-1",
                "tool",
                ToolEffect::Pure,
                deep,
            ),
            Err(EffectBrokerError::InvalidIntent(_))
        ));
    }
}
