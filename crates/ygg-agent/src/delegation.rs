//! Bounded V2 collaboration runtime for delegated coding agents.
//!
//! The model capability only advertises that collaboration is useful. This
//! module owns the host-side semantics: isolated child sessions, lifecycle and
//! message routing, bounded concurrency/depth, cancellation, and durable
//! provenance.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};
use ygg_ai::{AssistantPart, ToolDef, Usage};

use crate::agent::{Agent, AgentCompactionMode, AgentConfig, AgentError, CompletionPolicy};
use crate::effect::ToolEffect;
use crate::events::{AgentEvent, FinishReason};
use crate::extension::ExtensionHost;
use crate::secure_fs::{self, SecureFileError};
use crate::session::{Session, SessionError};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const ROOT_AGENT_ID: &str = "root";
const ROOT_AGENT_PATH: &str = "/root";
const COMMAND_CHANNEL_CAPACITY: usize = 32;
const MAX_PROVENANCE_TEXT_BYTES: usize = 128 * 1024;
const MAX_MAILBOX_MESSAGES: usize = 64;
const MAX_MAILBOX_BYTES: usize = 1024 * 1024;
// A running worker can become idle after up to one full command channel was
// accepted as steering. Preserve those already-persisted messages for its next
// task without allowing an unbounded per-agent queue.
const MAX_PENDING_MESSAGES: usize = COMMAND_CHANNEL_CAPACITY + 64;
const MAX_PENDING_MESSAGE_BYTES: usize = (COMMAND_CHANNEL_CAPACITY + 1) * MAX_PROVENANCE_TEXT_BYTES;
const MAX_QUEUED_FOLLOW_UPS: usize = COMMAND_CHANNEL_CAPACITY;
const MAX_QUEUED_FOLLOW_UP_BYTES: usize =
    (COMMAND_CHANNEL_CAPACITY + 1) * MAX_PROVENANCE_TEXT_BYTES;
const MAX_TOOL_TIMEOUT_MS: u64 = 3_600_000;
const MAX_EXTENSION_ACTIVE_CHILDREN: usize = 2;
const MAX_EXTENSION_TOTAL_TOKEN_RESERVATION: u64 = 96_000;
const MAX_EXTENSION_TOTAL_COST_RESERVATION: u64 = 500_000;
/// Host-reserved names installed by V2 collaboration overlays.
pub const COLLABORATION_TOOL_NAMES: [&str; 6] = [
    "spawn_agent",
    "followup_task",
    "send_message",
    "wait_agent",
    "list_agents",
    "interrupt_agent",
];

/// Returns a path-free opaque reference for one host-owned delegated session.
///
/// The reference is derived only from the cryptographically random private team
/// directory name and the host-generated child filename. It is safe to expose
/// to extension presentation and can be resolved only by a host that can
/// securely inventory its private delegation directory.
pub fn delegated_session_reference(session_path: &Path) -> Option<String> {
    let team = session_path.parent()?.file_name()?.to_str()?;
    let child = session_path.file_name()?.to_str()?;
    if !team.starts_with("team-")
        || team.len() > 128
        || !team
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || child.len() > 256
        || !child.ends_with(".jsonl")
        || !child
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(team.as_bytes());
    hasher.update(b"/");
    hasher.update(child.as_bytes());
    let digest = hasher.finalize();
    Some(format!(
        "agent-session:{}",
        digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

/// Returns whether this target build implements the advertised collaboration version.
pub fn delegation_runtime_supports(version: ygg_ai::AgentDelegation) -> bool {
    matches!(version, ygg_ai::AgentDelegation::V2)
        && cfg!(any(target_os = "linux", target_os = "macos", windows))
}

/// Whether delegated agents are merely available or should be used
/// proactively when parallel work would materially improve the result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DelegationMode {
    /// Expose collaboration tools without instructing the model to delegate.
    #[default]
    Available,
    /// Instruct the model to delegate suitable independent work proactively.
    Proactive,
}

/// Hard host-side limits for one delegation team.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DelegationLimits {
    /// Maximum number of agents executing at once, including the root agent.
    pub max_concurrent_agents: usize,
    /// Maximum child depth below the root (`1` permits children only).
    pub max_depth: usize,
    /// Maximum agents created during one owning run.
    pub max_total_agents: usize,
}

impl Default for DelegationLimits {
    fn default() -> Self {
        Self {
            max_concurrent_agents: 4,
            max_depth: 2,
            max_total_agents: 16,
        }
    }
}

/// Configuration for a durable V2 delegation team.
#[derive(Clone, Debug)]
pub struct DelegationConfig {
    /// Parent directory under which a private team directory is created.
    pub session_directory: PathBuf,
    /// Host-side bounds applied independently of model behavior.
    pub limits: DelegationLimits,
    /// Whether the root receives proactive delegation guidance.
    pub mode: DelegationMode,
}

impl DelegationConfig {
    /// Creates an available-on-demand delegation configuration.
    pub fn new(session_directory: impl Into<PathBuf>) -> Self {
        Self {
            session_directory: session_directory.into(),
            limits: DelegationLimits::default(),
            mode: DelegationMode::Available,
        }
    }

    /// Enables proactive delegation guidance.
    pub fn proactive(mut self) -> Self {
        self.mode = DelegationMode::Proactive;
        self
    }

    fn validate(&self) -> Result<(), DelegationError> {
        if self.limits.max_concurrent_agents < 2 {
            return Err(DelegationError::InvalidConfig(
                "max_concurrent_agents must be at least 2 (root plus one child)".into(),
            ));
        }
        if self.limits.max_concurrent_agents > 32 {
            return Err(DelegationError::InvalidConfig(
                "max_concurrent_agents must not exceed 32".into(),
            ));
        }
        if self.limits.max_depth == 0 || self.limits.max_depth > 8 {
            return Err(DelegationError::InvalidConfig(
                "max_depth must be between 1 and 8".into(),
            ));
        }
        if self.limits.max_total_agents < self.limits.max_concurrent_agents
            || self.limits.max_total_agents > 256
        {
            return Err(DelegationError::InvalidConfig(
                "max_total_agents must be at least max_concurrent_agents and at most 256".into(),
            ));
        }
        Ok(())
    }
}

/// Failure while configuring or operating the delegation runtime.
#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    /// The supplied host limits are invalid.
    #[error("invalid delegation configuration: {0}")]
    InvalidConfig(String),
    /// Delegation was already attached to this agent.
    #[error("delegation is already enabled for this agent")]
    AlreadyEnabled,
    /// A collaboration tool name collides with an existing host tool.
    #[error("delegation tool name is already registered: {0}")]
    DuplicateTool(String),
    /// A descriptor-bound private filesystem operation failed.
    #[error("delegation persistence failed: {0}")]
    SecureFile(#[from] SecureFileError),
    /// A filesystem operation failed.
    #[error("delegation persistence failed: {0}")]
    Io(#[from] io::Error),
    /// Child session persistence failed.
    #[error("delegated session failed: {0}")]
    Session(#[from] SessionError),
    /// A child agent could not be initialized.
    #[error("delegated agent failed: {0}")]
    Agent(#[from] AgentError),
    /// Delegation activation failed and secure rollback also could not finish.
    #[error("delegation activation failed ({activation}); rollback failed ({rollback})")]
    ActivationRollback {
        /// Original activation failure.
        activation: String,
        /// Descriptor-bound cleanup failure.
        rollback: String,
    },
}

/// Durable status exposed by `list_agents` and `wait_agent`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum DelegatedAgentStatus {
    /// The worker exists but has not started its task yet.
    Pending,
    /// The worker is executing a task.
    Running,
    /// The latest task completed successfully.
    Completed {
        /// Bounded visible output from the completed run.
        output: String,
    },
    /// The latest task was interrupted.
    Interrupted,
    /// The latest task failed.
    Failed {
        /// Bounded failure diagnostic.
        error: String,
    },
    /// The worker exceeded its host-owned wall deadline.
    TimedOut,
    /// The worker was shut down and cannot accept more work.
    Shutdown,
}

impl DelegatedAgentStatus {
    fn is_running(&self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed { .. } => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed { .. } => "failed",
            Self::TimedOut => "timed_out",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone)]
pub(crate) struct DelegationBinding {
    manager: Arc<DelegationManager>,
    identity: AgentIdentity,
    system_instructions: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ExtensionAgentSessionPolicy {
    pub(crate) tools: Vec<String>,
    pub(crate) max_depth: usize,
    pub(crate) max_concurrent_children: usize,
    pub(crate) max_turns: u64,
    pub(crate) max_tokens: u64,
    pub(crate) max_cost_microdollars: u64,
    pub(crate) max_output_bytes: usize,
    pub(crate) timeout_ms: u64,
}

impl ExtensionAgentSessionPolicy {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.tools.is_empty() || self.tools.len() > 2 {
            return Err("child tools must be a non-empty subset of read and search".into());
        }
        let tools = self.tools.iter().collect::<BTreeSet<_>>();
        if tools.len() != self.tools.len()
            || self
                .tools
                .iter()
                .any(|tool| !matches!(tool.as_str(), "read" | "search"))
        {
            return Err("child tools must be a duplicate-free subset of read and search".into());
        }
        if self.max_depth != 1 {
            return Err("extension child max_depth must be exactly 1".into());
        }
        if self.max_concurrent_children == 0
            || self.max_concurrent_children > MAX_EXTENSION_ACTIVE_CHILDREN
        {
            return Err("extension child concurrency must be between 1 and 2".into());
        }
        if !(1..=12).contains(&self.max_turns) {
            return Err("extension child max_turns must be between 1 and 12".into());
        }
        if !(1_000..=64_000).contains(&self.max_tokens) {
            return Err("extension child max_tokens must be between 1000 and 64000".into());
        }
        if !(1..=500_000).contains(&self.max_cost_microdollars) {
            return Err(
                "extension child max_cost_microdollars must be between 1 and 500000".into(),
            );
        }
        if !(512..=16 * 1024).contains(&self.max_output_bytes) {
            return Err("extension child max_output_bytes must be between 512 and 16384".into());
        }
        if !(5_000..=15 * 60 * 1_000).contains(&self.timeout_ms) {
            return Err("extension child timeout_ms must be between 5000 and 900000".into());
        }
        Ok(())
    }
}

pub(crate) struct ExtensionDelegationSpawnRequest {
    pub(crate) task_name: String,
    pub(crate) profile: Option<String>,
    pub(crate) fingerprint: Option<String>,
    pub(crate) message: String,
    pub(crate) idempotency_key: String,
    pub(crate) policy: ExtensionAgentSessionPolicy,
}

#[derive(Clone)]
pub(crate) struct ExtensionDelegationService {
    manager: Weak<DelegationManager>,
    principal: Arc<str>,
    parent_session_id: Arc<str>,
    task_prefix: Arc<str>,
    state: Arc<Mutex<ExtensionDelegationState>>,
}

#[derive(Default)]
struct ExtensionDelegationState {
    owners: BTreeMap<String, ExtensionDelegationOwnerState>,
}

#[derive(Default)]
struct ExtensionDelegationOwnerState {
    owned_agents: BTreeSet<String>,
    idempotent_spawns: BTreeMap<String, IdempotentExtensionSpawn>,
}

struct IdempotentExtensionSpawn {
    task_name: String,
    profile: Option<String>,
    fingerprint: Option<String>,
    message_sha256: String,
    policy: ExtensionAgentSessionPolicy,
    result: Value,
}

impl DelegationBinding {
    pub(crate) fn team_directory(&self) -> &Path {
        &self.manager.team_directory
    }

    pub(crate) fn open_session_reference(
        &self,
        extension_principal: &str,
        reference: &str,
    ) -> Result<Option<Session>, AgentError> {
        if !reference.starts_with("agent-session:") {
            return Ok(None);
        }
        let path = {
            let state = self
                .manager
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state
                .records
                .values()
                .find(|record| {
                    record.extension_principal.as_deref() == Some(extension_principal)
                        && delegated_session_reference(&record.session_path).as_deref()
                            == Some(reference)
                })
                .map(|record| record.session_path.clone())
        };
        let Some(path) = path else {
            return Ok(None);
        };
        let file = secure_fs::open_private_file_for_read(&path)
            .map_err(|error| AgentError::Delegation(error.to_string()))?;
        Session::open_read_only_with_file(path, file)
            .map(Some)
            .map_err(AgentError::Session)
    }

    pub(crate) fn system_instructions(&self) -> &str {
        &self.system_instructions
    }

    pub(crate) fn request_shutdown(&self) {
        self.manager.request_shutdown_descendants(&self.identity.id);
    }

    pub(crate) fn prepare_owning_run(&self) -> Result<(), AgentError> {
        self.manager
            .prepare_owning_run(&self.identity)
            .map_err(AgentError::Delegation)
    }

    pub(crate) fn update_base_system(&self, system: String) {
        if self.identity.id == ROOT_AGENT_ID {
            *self
                .manager
                .template
                .base_system
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = system;
        }
    }
    pub(crate) fn update_runtime_settings(&self, settings: DelegationRuntimeSettings) {
        if self.identity.id == ROOT_AGENT_ID {
            *self
                .manager
                .template
                .runtime
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
        }
    }

    pub(crate) fn extension_service(
        &self,
        principal: impl Into<String>,
        parent_session_id: impl Into<String>,
        root_resource_owner: impl Into<String>,
    ) -> Result<ExtensionDelegationService, String> {
        if self.identity.id != ROOT_AGENT_ID {
            return Err("extension delegation service requires the root binding".into());
        }
        let principal = principal.into();
        if principal.trim().is_empty() || principal.len() > 256 {
            return Err("extension delegation principal must be 1..=256 bytes".into());
        }
        let parent_session_id = parent_session_id.into();
        if parent_session_id.trim().is_empty()
            || parent_session_id.len() > 256
            || parent_session_id.chars().any(char::is_whitespace)
        {
            return Err(
                "extension delegation parent session must be a bounded stable identifier".into(),
            );
        }
        let root_resource_owner = root_resource_owner.into();
        ExtensionDelegationService::validate_resource_owner(&root_resource_owner)?;
        self.manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .root_resource_owner = Some(root_resource_owner);
        let digest = Sha256::digest(principal.as_bytes());
        let task_prefix = format!(
            "ext-{}",
            digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        Ok(ExtensionDelegationService {
            manager: Arc::downgrade(&self.manager),
            principal: Arc::from(principal),
            parent_session_id: Arc::from(parent_session_id),
            task_prefix: Arc::from(task_prefix),
            state: Arc::new(Mutex::new(ExtensionDelegationState::default())),
        })
    }
}

impl ExtensionDelegationService {
    fn manager(&self) -> Result<Arc<DelegationManager>, String> {
        self.manager
            .upgrade()
            .ok_or_else(|| "delegation service is no longer available".to_owned())
    }

    fn root_identity() -> AgentIdentity {
        AgentIdentity {
            id: ROOT_AGENT_ID.into(),
            path: ROOT_AGENT_PATH.into(),
            depth: 0,
        }
    }

    fn owner_identity(
        &self,
        manager: &DelegationManager,
        resource_owner: &str,
    ) -> Result<AgentIdentity, String> {
        Self::validate_resource_owner(resource_owner)?;
        let state = manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.root_resource_owner.as_deref() == Some(resource_owner) {
            return Ok(Self::root_identity());
        }
        state
            .records
            .values()
            .find(|record| record.resource_owner.as_deref() == Some(resource_owner))
            .map(|record| record.identity.clone())
            .ok_or_else(|| "extension resource owner is not an active model session".to_owned())
    }

    fn resolve_owned_target(
        &self,
        manager: &DelegationManager,
        resource_owner: &str,
        target: &str,
    ) -> Result<String, String> {
        let owned = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .owners
            .get(resource_owner)
            .map(|owner| owner.owned_agents.clone())
            .unwrap_or_default();
        if owned.is_empty() {
            return Err("extension resource owner has no child sessions".into());
        }
        let state = manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owned_paths = owned
            .iter()
            .filter_map(|id| {
                state
                    .records
                    .get(id)
                    .map(|record| record.identity.path.clone())
            })
            .collect::<Vec<_>>();
        let target_id = DelegationManager::resolve_id_locked(&state, target)
            .ok_or_else(|| format!("unknown extension delegation target: {target}"))?;
        let target_path = state
            .records
            .get(&target_id)
            .map(|record| record.identity.path.as_str())
            .ok_or_else(|| format!("unknown extension delegation target: {target}"))?;
        if !owned.contains(&target_id)
            && !owned_paths
                .iter()
                .any(|root| is_descendant_path(target_path, root))
        {
            return Err("extension principal may access only its own child-session trees".into());
        }
        Ok(target_id)
    }

    fn owner_task_prefix(&self, resource_owner: &str) -> String {
        let digest = Sha256::digest(resource_owner.as_bytes());
        format!(
            "{}-{}",
            self.task_prefix,
            digest[..4]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    }

    fn validate_resource_owner(resource_owner: &str) -> Result<(), String> {
        if resource_owner.trim().is_empty() || resource_owner.len() > 512 {
            return Err("extension resource owner must be 1..=512 bytes".into());
        }
        Ok(())
    }

    pub(crate) fn shutdown_owned(&self) {
        let roots = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .owners
            .values()
            .flat_map(|owner| owner.owned_agents.iter().cloned())
            .collect::<BTreeSet<_>>();
        if let Some(manager) = self.manager.upgrade() {
            manager.request_shutdown_agent_trees(&roots);
        }
    }

    pub(crate) fn spawn(
        &self,
        resource_owner: &str,
        request: ExtensionDelegationSpawnRequest,
    ) -> Result<Value, String> {
        let ExtensionDelegationSpawnRequest {
            task_name,
            profile,
            fingerprint,
            message,
            idempotency_key,
            policy,
        } = request;
        Self::validate_resource_owner(resource_owner)?;
        validate_task_name(&task_name)?;
        if let Some(profile) = profile.as_deref() {
            validate_task_name(profile)
                .map_err(|_| "profile must be a bounded lowercase stable identifier".to_owned())?;
        }
        if fingerprint.as_deref().is_some_and(|fingerprint| {
            fingerprint.len() != 64
                || !fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err("fingerprint must be a lowercase SHA-256 digest".into());
        }
        policy.validate()?;
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 256 {
            return Err("spawn idempotency_key must be 1..=256 bytes".into());
        }
        let message_sha256 = format!("{:x}", Sha256::digest(message.as_bytes()));
        let mut service_state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owner_state = service_state
            .owners
            .entry(resource_owner.to_owned())
            .or_default();
        if let Some(existing) = owner_state.idempotent_spawns.get(&idempotency_key) {
            if existing.task_name != task_name
                || existing.profile != profile
                || existing.fingerprint != fingerprint
                || existing.message_sha256 != message_sha256
                || existing.policy != policy
            {
                return Err("spawn idempotency_key was reused with different input".into());
            }
            return Ok(existing.result.clone());
        }
        let internal_digest = Sha256::digest(format!("{task_name}\0{idempotency_key}").as_bytes());
        let internal_task_name = format!(
            "{}-task-{}",
            self.owner_task_prefix(resource_owner),
            internal_digest[..6]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let manager = self.manager()?;
        if manager.template.model.spec.pricing.is_none() {
            return Err(
                "bounded extension child requires trusted model pricing for its hard cost ceiling"
                    .into(),
            );
        }
        let owner = self.owner_identity(&manager, resource_owner)?;
        {
            let manager_state = manager
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let active = owner_state
                .owned_agents
                .iter()
                .filter_map(|id| manager_state.records.get(id))
                .filter(|record| record.status.is_running())
                .collect::<Vec<_>>();
            if active.len() >= policy.max_concurrent_children {
                return Err(format!(
                    "extension child concurrency limit reached ({})",
                    policy.max_concurrent_children
                ));
            }
            if owner_state.owned_agents.len() >= 16 {
                return Err("extension child total limit reached (16)".into());
            }
            let reserved_tokens = active.iter().fold(0u64, |total, record| {
                total.saturating_add(
                    record
                        .extension_policy
                        .as_ref()
                        .map(|policy| policy.max_tokens)
                        .unwrap_or_default(),
                )
            });
            if reserved_tokens.saturating_add(policy.max_tokens)
                > MAX_EXTENSION_TOTAL_TOKEN_RESERVATION
            {
                return Err(format!(
                    "extension child token reservation limit reached ({MAX_EXTENSION_TOTAL_TOKEN_RESERVATION})"
                ));
            }
            let reserved_cost = active.iter().fold(0u64, |total, record| {
                total.saturating_add(
                    record
                        .extension_policy
                        .as_ref()
                        .map(|policy| policy.max_cost_microdollars)
                        .unwrap_or_default(),
                )
            });
            if reserved_cost.saturating_add(policy.max_cost_microdollars)
                > MAX_EXTENSION_TOTAL_COST_RESERVATION
            {
                return Err(format!(
                    "extension child cost reservation limit reached ({MAX_EXTENSION_TOTAL_COST_RESERVATION} microdollars)"
                ));
            }
        }
        let mut result = manager.spawn(
            &owner,
            SpawnRequest {
                task_name: internal_task_name,
                display_task_name: Some(task_name.clone()),
                message,
                extension_policy: Some(policy.clone()),
                extension_provenance: Some(ExtensionSpawnProvenance {
                    parent_session_id: self.parent_session_id.to_string(),
                    principal: self.principal.to_string(),
                    resource_owner: resource_owner.to_owned(),
                    profile: profile.clone(),
                    idempotency_key: idempotency_key.clone(),
                    fingerprint: fingerprint.clone(),
                }),
            },
        )?;
        let agent_id = result
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "delegation spawn omitted agent_id".to_owned())?;
        let agent_id = agent_id.to_owned();
        result["task_name"] = Value::String(task_name.clone());
        result["principal"] = Value::String(self.principal.to_string());
        result["resource_owner"] = Value::String(resource_owner.to_owned());
        owner_state.owned_agents.insert(agent_id);
        owner_state.idempotent_spawns.insert(
            idempotency_key,
            IdempotentExtensionSpawn {
                task_name,
                profile,
                fingerprint,
                message_sha256,
                policy,
                result: result.clone(),
            },
        );
        Ok(result)
    }

    pub(crate) async fn send_message(
        &self,
        resource_owner: &str,
        target: &str,
        message: String,
    ) -> Result<Value, String> {
        let manager = self.manager()?;
        let owner = self.owner_identity(&manager, resource_owner)?;
        let target = self.resolve_owned_target(&manager, resource_owner, target)?;
        manager.send_message(&owner, &target, message).await
    }

    pub(crate) async fn follow_up(
        &self,
        resource_owner: &str,
        target: &str,
        message: String,
    ) -> Result<Value, String> {
        let manager = self.manager()?;
        let owner = self.owner_identity(&manager, resource_owner)?;
        let target = self.resolve_owned_target(&manager, resource_owner, target)?;
        {
            let state = manager
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state
                .records
                .get(&target)
                .is_some_and(|record| record.extension_policy.is_some())
            {
                return Err("follow-up runs are disabled for bounded extension children".into());
            }
        }
        manager
            .follow_up(&owner, FollowUpRequest { target, message })
            .await
    }

    pub(crate) async fn interrupt(
        &self,
        resource_owner: &str,
        target: &str,
    ) -> Result<Value, String> {
        let manager = self.manager()?;
        let owner = self.owner_identity(&manager, resource_owner)?;
        let target = self.resolve_owned_target(&manager, resource_owner, target)?;
        manager.interrupt(&owner, &target).await
    }

    pub(crate) fn list(&self, resource_owner: &str) -> Result<Value, String> {
        Self::validate_resource_owner(resource_owner)?;
        let manager = self.manager()?;
        let owner = self.owner_identity(&manager, resource_owner)?;
        let owned = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .owners
            .get(resource_owner)
            .map(|owner| owner.owned_agents.clone())
            .unwrap_or_default();
        let state = manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        manager.ensure_owner_active_locked(&state, &owner)?;
        let owned_paths = owned
            .iter()
            .filter_map(|id| {
                state
                    .records
                    .get(id)
                    .map(|record| record.identity.path.clone())
            })
            .collect::<Vec<_>>();
        let agents = state
            .records
            .values()
            .filter(|record| {
                owned.contains(&record.identity.id)
                    || owned_paths
                        .iter()
                        .any(|root| is_descendant_path(&record.identity.path, root))
            })
            .map(|record| {
                let mut value = agent_record_value(record);
                value["session"] = delegated_session_reference(&record.session_path)
                    .map(Value::String)
                    .unwrap_or(Value::Null);
                value["provenance"] = json!({
                    "kind": "extension_agent_session",
                    "principal": self.principal.as_ref(),
                    "resource_owner": resource_owner,
                });
                value
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "principal": self.principal.as_ref(),
            "resource_owner": resource_owner,
            "agents": agents,
            "persistence_error": state.persistence_error,
        }))
    }

    pub(crate) async fn wait(
        &self,
        resource_owner: &str,
        timeout: Duration,
        cancellation: &crate::CancellationToken,
    ) -> Result<Value, String> {
        let manager = self.manager()?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = manager.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = self.list(resource_owner)?;
            let any_running = snapshot["agents"].as_array().is_some_and(|agents| {
                agents.iter().any(|agent| {
                    matches!(
                        agent["status"]["state"].as_str(),
                        Some("pending" | "running")
                    )
                })
            });
            if !any_running {
                return Ok(json!({"timed_out": false, "snapshot": snapshot}));
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return Err("extension delegation wait cancelled".into())
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(json!({"timed_out": true, "snapshot": self.list(resource_owner)?}))
                }
                _ = &mut changed => {}
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AgentIdentity {
    id: String,
    path: String,
    depth: usize,
}

#[derive(Clone)]
pub(crate) struct DelegationRuntimeSettings {
    pub(crate) compaction_model: Option<ygg_ai::Model>,
    pub(crate) auto_compaction_mode: AgentCompactionMode,
    pub(crate) auto_compaction_threshold: f64,
    pub(crate) compaction_keep_recent_tokens: u64,
    pub(crate) completion_policy: CompletionPolicy,
    pub(crate) output_modalities: ygg_ai::OutputModalities,
    pub(crate) max_output_tokens: u64,
    pub(crate) max_session_cost_microdollars: Option<u64>,
    pub(crate) provider_retries_enabled: bool,
}

pub(crate) struct DelegationTemplate {
    pub(crate) client: ygg_ai::AiClient,
    pub(crate) model: ygg_ai::Model,
    pub(crate) base_system: RwLock<String>,
    pub(crate) sandbox: crate::SandboxConfig,
    pub(crate) effect_broker: crate::EffectBroker,
    pub(crate) extensions: ExtensionHost,
    pub(crate) max_turns: Option<u64>,
    pub(crate) reasoning: ygg_ai::ReasoningConfig,
    pub(crate) reasoning_mode: ygg_ai::ReasoningMode,
    pub(crate) cache_retention: ygg_ai::CacheRetention,
    pub(crate) runtime: RwLock<DelegationRuntimeSettings>,
}

pub(crate) struct DelegationManager {
    config: DelegationConfig,
    team_directory: PathBuf,
    team_storage: Option<Arc<secure_fs::PrivateDirectory>>,
    journal: ProvenanceJournal,
    template: DelegationTemplate,
    state: Mutex<ManagerState>,
    permits: RwLock<Arc<Semaphore>>,
    changed: Notify,
}

struct ManagerState {
    next_agent_number: u64,
    next_mailbox_delivery: u64,
    total_agents: usize,
    active_waiters: usize,
    records: BTreeMap<String, AgentRecord>,
    root_mailbox: VecDeque<MailboxMessage>,
    root_mailbox_delivery: Option<MailboxDeliveryPlan>,
    persistence_error: Option<String>,
    shutting_down: bool,
    root_active: bool,
    root_resource_owner: Option<String>,
}

struct AgentRecord {
    identity: AgentIdentity,
    task_name: String,
    display_task_name: Option<String>,
    parent_id: String,
    session_path: PathBuf,
    status: DelegatedAgentStatus,
    command_tx: mpsc::Sender<WorkerCommand>,
    shutdown: crate::CancellationToken,
    interrupt_requested: bool,
    pending_messages: VecDeque<DirectedMessage>,
    reserved_messages: QueueUsage,
    queued_follow_ups: QueueUsage,
    mailbox: VecDeque<MailboxMessage>,
    mailbox_delivery: Option<MailboxDeliveryPlan>,
    resource_owner: Option<String>,
    extension_policy: Option<ExtensionAgentSessionPolicy>,
    extension_principal: Option<String>,
    extension_profile: Option<String>,
    extension_idempotency_key: Option<String>,
    extension_fingerprint: Option<String>,
    created_at_ms: u64,
    started_at_ms: Option<u64>,
    completed_at_ms: Option<u64>,
    turn_count: u64,
    usage: Usage,
    cost_microdollars: Option<u64>,
    deadline_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct QueueUsage {
    messages: usize,
    bytes: usize,
}

impl QueueUsage {
    fn add(&mut self, bytes: usize) {
        self.messages = self.messages.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }

    fn add_usage(&mut self, other: Self) {
        self.messages = self.messages.saturating_add(other.messages);
        self.bytes = self.bytes.saturating_add(other.bytes);
    }

    fn remove(&mut self, usage: Self) {
        self.messages = self.messages.saturating_sub(usage.messages);
        self.bytes = self.bytes.saturating_sub(usage.bytes);
    }
}

#[derive(Clone, Debug, Serialize)]
struct DirectedMessage {
    from: String,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
struct MailboxMessage {
    kind: &'static str,
    from: String,
    task_name: Option<String>,
    message: String,
    #[serde(skip)]
    evictable: bool,
    #[serde(skip)]
    continued: bool,
    #[serde(skip)]
    leased: bool,
}

#[derive(Clone, Copy, Debug)]
struct MailboxDeliveryPlan {
    id: u64,
    complete_messages: usize,
    partial_bytes: usize,
    touched_messages: usize,
}

struct WaitOutput {
    value: Value,
    delivery_id: Option<u64>,
}

struct SpawnRequest {
    task_name: String,
    display_task_name: Option<String>,
    message: String,
    extension_policy: Option<ExtensionAgentSessionPolicy>,
    extension_provenance: Option<ExtensionSpawnProvenance>,
}

struct ExtensionSpawnProvenance {
    parent_session_id: String,
    principal: String,
    resource_owner: String,
    profile: Option<String>,
    idempotency_key: String,
    fingerprint: Option<String>,
}

struct FollowUpRequest {
    target: String,
    message: String,
}

struct WorkerCommand {
    kind: WorkerCommandKind,
}

struct WorkerStartup {
    identity: AgentIdentity,
    session: Session,
    initial_task: String,
    commands: mpsc::Receiver<WorkerCommand>,
    shutdown: crate::CancellationToken,
    initial_permit: OwnedSemaphorePermit,
    extension_policy: Option<ExtensionAgentSessionPolicy>,
    deadline: Option<tokio::time::Instant>,
}

struct ChildRunContext<'a> {
    identity: &'a AgentIdentity,
    commands: &'a mut mpsc::Receiver<WorkerCommand>,
    shutdown: &'a crate::CancellationToken,
    extension_policy: Option<&'a ExtensionAgentSessionPolicy>,
    deadline: Option<tokio::time::Instant>,
}

impl WorkerCommand {
    fn message(message: DirectedMessage) -> Self {
        Self {
            kind: WorkerCommandKind::Message(message),
        }
    }

    fn follow_up(follow_up: QueuedFollowUp) -> Self {
        Self {
            kind: WorkerCommandKind::FollowUp(follow_up),
        }
    }

    fn shutdown() -> Self {
        Self {
            kind: WorkerCommandKind::Shutdown,
        }
    }
}

enum WorkerCommandKind {
    Message(DirectedMessage),
    FollowUp(QueuedFollowUp),
    Shutdown,
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ProvenanceEvent<'a> {
    TeamStarted {
        timestamp_ms: u128,
        root_session: &'a Path,
        limits: &'a DelegationLimits,
        mode: &'static str,
    },
    AgentSpawned {
        timestamp_ms: u128,
        agent_id: &'a str,
        agent_path: &'a str,
        parent_id: &'a str,
        task_name: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        display_task_name: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_parent_session_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_principal: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_resource_owner: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_profile: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_idempotency_key: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        extension_fingerprint: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session: Option<&'a Path>,
        #[serde(skip_serializing_if = "Option::is_none")]
        session_reference: Option<&'a str>,
    },
    AgentStatus {
        timestamp_ms: u128,
        agent_id: &'a str,
        status: &'a DelegatedAgentStatus,
    },
    Message {
        timestamp_ms: u128,
        from: &'a str,
        to: &'a str,
        kind: &'a str,
        message: &'a str,
    },
    InterruptRequested {
        timestamp_ms: u128,
        from: &'a str,
        to: &'a str,
    },
    TeamShutdown {
        timestamp_ms: u128,
    },
}

struct ProvenanceJournal {
    file: Mutex<File>,
}

impl ProvenanceJournal {
    fn create(directory: &secure_fs::PrivateDirectory) -> Result<Self, SecureFileError> {
        let path = directory.path().join("provenance.jsonl");
        let file = directory.create_regular_file_for_append(&path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    fn append(&self, event: &ProvenanceEvent<'_>) -> io::Result<()> {
        let encoded = serde_json::to_vec(event).map_err(io::Error::other)?;
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(&encoded)?;
        file.write_all(b"\n")?;
        file.sync_data()
    }
}

impl DelegationManager {
    fn create(
        config: DelegationConfig,
        template: DelegationTemplate,
        root_session: &Path,
    ) -> Result<Arc<Self>, DelegationError> {
        Self::create_with_journal(config, template, root_session, |directory| {
            Ok(ProvenanceJournal::create(directory)?)
        })
    }

    fn create_with_journal(
        config: DelegationConfig,
        template: DelegationTemplate,
        root_session: &Path,
        create_journal: impl FnOnce(
            &secure_fs::PrivateDirectory,
        ) -> Result<ProvenanceJournal, DelegationError>,
    ) -> Result<Arc<Self>, DelegationError> {
        config.validate()?;
        let team_storage = create_private_team_directory(&config.session_directory)?;
        let team_directory = team_storage.path().to_path_buf();
        let activation = (|| {
            let journal = create_journal(&team_storage)?;
            let child_slots = config.limits.max_concurrent_agents - 1;
            let manager = Arc::new(Self {
                config,
                team_directory: team_directory.clone(),
                team_storage: Some(Arc::clone(&team_storage)),
                journal,
                template,
                state: Mutex::new(ManagerState {
                    next_agent_number: 1,
                    next_mailbox_delivery: 1,
                    total_agents: 1,
                    active_waiters: 0,
                    records: BTreeMap::new(),
                    root_mailbox: VecDeque::new(),
                    root_mailbox_delivery: None,
                    persistence_error: None,
                    shutting_down: false,
                    root_active: true,
                    root_resource_owner: None,
                }),
                permits: RwLock::new(Arc::new(Semaphore::new(child_slots))),
                changed: Notify::new(),
            });
            manager.journal.append(&ProvenanceEvent::TeamStarted {
                timestamp_ms: timestamp_ms(),
                root_session,
                limits: &manager.config.limits,
                mode: match manager.config.mode {
                    DelegationMode::Available => "available",
                    DelegationMode::Proactive => "proactive",
                },
            })?;
            Ok(manager)
        })();
        match activation {
            Ok(manager) => Ok(manager),
            Err(error) => match cleanup_failed_team_activation(&team_storage) {
                Ok(()) => Err(error),
                Err(rollback) => Err(DelegationError::ActivationRollback {
                    activation: error.to_string(),
                    rollback,
                }),
            },
        }
    }

    fn root_binding(self: &Arc<Self>) -> DelegationBinding {
        DelegationBinding {
            manager: Arc::clone(self),
            identity: AgentIdentity {
                id: ROOT_AGENT_ID.into(),
                path: ROOT_AGENT_PATH.into(),
                depth: 0,
            },
            system_instructions: Arc::from(root_instructions(&self.config)),
        }
    }

    fn create_team_file(&self, path: &Path) -> Result<File, SecureFileError> {
        match &self.team_storage {
            Some(directory) => directory.create_regular_file_for_append(path),
            None => secure_fs::create_regular_file_for_append(path),
        }
    }

    fn open_team_file_for_append(&self, path: &Path) -> Result<File, SecureFileError> {
        match &self.team_storage {
            Some(directory) => directory.open_regular_file_for_append(path),
            None => secure_fs::open_regular_file_for_append(path),
        }
    }

    fn remove_team_file_if_exists(&self, path: &Path) -> Result<bool, SecureFileError> {
        match &self.team_storage {
            Some(directory) => directory.remove_regular_file_if_exists(path),
            None => secure_fs::remove_regular_file_if_exists(path),
        }
    }

    fn reopen_child_session(&self, path: &Path) -> Result<Session, DelegationError> {
        let file = self.open_team_file_for_append(path)?;
        Ok(Session::open_with_file(path, file)?)
    }

    fn tools(self: &Arc<Self>, identity: &AgentIdentity) -> Vec<Arc<dyn Tool>> {
        CollaborationToolKind::ALL
            .into_iter()
            .map(|kind| {
                Arc::new(CollaborationTool {
                    manager: Arc::downgrade(self),
                    owner: identity.clone(),
                    kind,
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn prepare_owning_run(&self, owner: &AgentIdentity) -> Result<(), String> {
        if owner.id == ROOT_AGENT_ID {
            if owner.path != ROOT_AGENT_PATH || owner.depth != 0 {
                return Err("invalid root delegation identity".into());
            }
            let child_slots = self.config.limits.max_concurrent_agents - 1;
            let mut permits = self
                .permits
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(error) = &state.persistence_error {
                return Err(format!("delegation persistence is unavailable: {error}"));
            }
            if state.shutting_down {
                return Err("delegation team is shutting down".into());
            }

            for record in state.records.values() {
                record.shutdown.cancel();
                let _ = record.command_tx.try_send(WorkerCommand::shutdown());
            }
            state.records.clear();
            state.total_agents = 1;
            state.root_active = true;
            *permits = Arc::new(Semaphore::new(child_slots));
            drop(state);
            drop(permits);
            self.changed.notify_waiters();
            return Ok(());
        }

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_owner_active_locked(&state, owner)?;
        let descendants = state
            .records
            .iter()
            .filter(|(_, record)| is_descendant_path(&record.identity.path, &owner.path))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in &descendants {
            if let Some(record) = state.records.get(id) {
                record.shutdown.cancel();
                let _ = record.command_tx.try_send(WorkerCommand::shutdown());
            }
        }
        for id in descendants {
            state.records.remove(&id);
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    fn current_permits(&self) -> Arc<Semaphore> {
        Arc::clone(
            &self
                .permits
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn spawn(
        self: &Arc<Self>,
        owner: &AgentIdentity,
        request: SpawnRequest,
    ) -> Result<Value, String> {
        let SpawnRequest {
            task_name,
            display_task_name,
            message,
            mut extension_policy,
            extension_provenance,
        } = request;
        validate_task_name(&task_name)?;
        if let Some(display_task_name) = display_task_name.as_deref() {
            validate_task_name(display_task_name)?;
        }
        validate_durable_text("spawn task", &message)?;
        if extension_provenance.is_some() != extension_policy.is_some() {
            return Err(
                "extension delegation policy and durable ownership provenance must be paired"
                    .into(),
            );
        }
        if let Some(provenance) = extension_provenance.as_ref() {
            if provenance.parent_session_id.trim().is_empty()
                || provenance.parent_session_id.len() > 256
                || provenance
                    .parent_session_id
                    .chars()
                    .any(char::is_whitespace)
            {
                return Err("invalid extension delegation parent session".into());
            }
            if provenance.principal.trim().is_empty() || provenance.principal.len() > 256 {
                return Err("invalid extension delegation principal".into());
            }
            ExtensionDelegationService::validate_resource_owner(&provenance.resource_owner)?;
        }
        if let Some(policy) = extension_policy.as_mut() {
            policy.validate()?;
            if owner.depth.saturating_add(1) > policy.max_depth {
                return Err(format!(
                    "extension child depth limit reached at {} (max depth {})",
                    owner.path, policy.max_depth
                ));
            }
            let allowed = policy.tools.iter().cloned().collect::<BTreeSet<_>>();
            let (_, effective_tools) = self.template.extensions.scoped_tool_snapshot(&allowed)?;
            policy.tools = effective_tools;
            if let Some(parent_turns) = self.template.max_turns {
                policy.max_turns = policy.max_turns.min(parent_turns);
            }
            let runtime = self
                .template
                .runtime
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(parent_cost) = runtime.max_session_cost_microdollars {
                policy.max_cost_microdollars = policy.max_cost_microdollars.min(parent_cost);
            }
        }
        let initial_task = message;
        if owner.depth >= self.config.limits.max_depth {
            return Err(format!(
                "delegation depth limit reached at {} (max depth {})",
                owner.path, self.config.limits.max_depth
            ));
        }
        let permit = self.current_permits().try_acquire_owned().map_err(|_| {
            "delegation concurrency limit reached; wait for an active agent".to_owned()
        })?;

        let created_at_ms = u64::try_from(timestamp_ms()).unwrap_or(u64::MAX);
        let deadline = extension_policy
            .as_ref()
            .map(|policy| tokio::time::Instant::now() + Duration::from_millis(policy.timeout_ms));
        let deadline_at_ms = extension_policy
            .as_ref()
            .map(|policy| created_at_ms.saturating_add(policy.timeout_ms));

        let (identity, session, command_rx, shutdown, task_name) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_owner_active_locked(&state, owner)?;
            if state.total_agents >= self.config.limits.max_total_agents {
                return Err(format!(
                    "delegation team agent limit reached ({})",
                    self.config.limits.max_total_agents
                ));
            }
            let child_path = format!("{}/{}", owner.path.trim_end_matches('/'), task_name);
            if state
                .records
                .values()
                .any(|record| record.identity.path == child_path)
            {
                return Err(format!(
                    "task name already exists under {}: {}",
                    owner.path, task_name
                ));
            }
            let number = state.next_agent_number;
            state.next_agent_number = state.next_agent_number.saturating_add(1);
            let identity = AgentIdentity {
                id: format!("agent-{number}"),
                path: child_path,
                depth: owner.depth + 1,
            };
            let session_path = self
                .team_directory
                .join(format!("{number:04}-{task_name}.jsonl"));
            // Create the isolated durable session before publishing the worker.
            let session_file = self
                .create_team_file(&session_path)
                .map_err(|error| error.to_string())?;
            let session = match Session::create_with_file(&session_path, session_file) {
                Ok(session) => session,
                Err(error) => {
                    let _ = self.remove_team_file_if_exists(&session_path);
                    return Err(error.to_string());
                }
            };
            let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
            let shutdown = crate::CancellationToken::default();
            let extension_session_reference = extension_provenance.as_ref().map(|_| {
                delegated_session_reference(&session_path)
                    .expect("generated extension child path has a delegated session reference")
            });
            if let Err(error) = self.journal.append(&ProvenanceEvent::AgentSpawned {
                timestamp_ms: u128::from(created_at_ms),
                agent_id: &identity.id,
                agent_path: &identity.path,
                parent_id: &owner.id,
                task_name: &task_name,
                display_task_name: display_task_name.as_deref(),
                extension_parent_session_id: extension_provenance
                    .as_ref()
                    .map(|provenance| provenance.parent_session_id.as_str()),
                extension_principal: extension_provenance
                    .as_ref()
                    .map(|provenance| provenance.principal.as_str()),
                extension_resource_owner: extension_provenance
                    .as_ref()
                    .map(|provenance| provenance.resource_owner.as_str()),
                extension_profile: extension_provenance
                    .as_ref()
                    .and_then(|provenance| provenance.profile.as_deref()),
                extension_idempotency_key: extension_provenance
                    .as_ref()
                    .map(|provenance| provenance.idempotency_key.as_str()),
                extension_fingerprint: extension_provenance
                    .as_ref()
                    .and_then(|provenance| provenance.fingerprint.as_deref()),
                task: extension_provenance
                    .is_none()
                    .then_some(initial_task.as_str()),
                session: extension_provenance
                    .is_none()
                    .then_some(session_path.as_path()),
                session_reference: extension_session_reference.as_deref(),
            }) {
                let message = format!("could not persist delegation provenance: {error}");
                self.fail_persistence_locked(&mut state, &error);
                drop(state);
                drop(command_tx);
                drop(session);
                let _ = self.remove_team_file_if_exists(&session_path);
                return Err(message);
            }
            state.records.insert(
                identity.id.clone(),
                AgentRecord {
                    identity: identity.clone(),
                    task_name: task_name.clone(),
                    display_task_name: display_task_name.clone(),
                    parent_id: owner.id.clone(),
                    session_path: session_path.clone(),
                    status: DelegatedAgentStatus::Pending,
                    command_tx: command_tx.clone(),
                    shutdown: shutdown.clone(),
                    interrupt_requested: false,
                    pending_messages: VecDeque::new(),
                    reserved_messages: QueueUsage::default(),
                    queued_follow_ups: QueueUsage::default(),
                    mailbox: VecDeque::new(),
                    mailbox_delivery: None,
                    resource_owner: None,
                    extension_policy: extension_policy.clone(),
                    extension_principal: extension_provenance
                        .as_ref()
                        .map(|provenance| provenance.principal.clone()),
                    extension_profile: extension_provenance
                        .as_ref()
                        .and_then(|provenance| provenance.profile.clone()),
                    extension_idempotency_key: extension_provenance
                        .as_ref()
                        .map(|provenance| provenance.idempotency_key.clone()),
                    extension_fingerprint: extension_provenance
                        .as_ref()
                        .and_then(|provenance| provenance.fingerprint.clone()),
                    created_at_ms,
                    started_at_ms: None,
                    completed_at_ms: None,
                    turn_count: 0,
                    usage: Usage::default(),
                    cost_microdollars: None,
                    deadline_at_ms,
                },
            );
            state.total_agents += 1;
            (identity, session, command_rx, shutdown, task_name)
        };

        let result_policy = extension_policy.clone();
        let manager = Arc::clone(self);
        let worker_identity = identity.clone();
        tokio::spawn(async move {
            manager
                .run_worker(WorkerStartup {
                    identity: worker_identity,
                    session,
                    initial_task,
                    commands: command_rx,
                    shutdown,
                    initial_permit: permit,
                    extension_policy,
                    deadline,
                })
                .await;
        });
        self.changed.notify_waiters();

        Ok(json!({
            "agent_id": identity.id,
            "agent_path": identity.path,
            "task_name": task_name,
            "profile": extension_provenance
                .as_ref()
                .and_then(|provenance| provenance.profile.as_deref()),
            "idempotency_key": extension_provenance
                .as_ref()
                .map(|provenance| provenance.idempotency_key.as_str()),
            "fingerprint": extension_provenance
                .as_ref()
                .and_then(|provenance| provenance.fingerprint.as_deref()),
            "status": "pending",
            "policy": result_policy,
            "created_at_ms": created_at_ms,
            "started_at_ms": Value::Null,
            "completed_at_ms": Value::Null,
            "deadline_at_ms": deadline_at_ms,
        }))
    }

    async fn run_worker(self: Arc<Self>, startup: WorkerStartup) {
        let WorkerStartup {
            identity,
            session,
            initial_task,
            mut commands,
            shutdown,
            initial_permit,
            extension_policy,
            deadline,
        } = startup;
        let session_path = session.path().to_path_buf();
        let mut unopened_session = Some(session);
        let mut agent = None;
        let mut queued_tasks = VecDeque::from([QueuedTask::Initial(initial_task)]);
        let mut initial_permit = Some(initial_permit);
        let mut retry_undelivered_task = false;
        let mut retry_pending_messages = 0;
        loop {
            if deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
                self.set_status(&identity.id, DelegatedAgentStatus::TimedOut, true);
                self.request_shutdown_descendants(&identity.id);
                return;
            }
            if shutdown.is_cancelled() {
                self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                self.request_shutdown_descendants(&identity.id);
                return;
            }
            if self.interrupt_requested(&identity.id) {
                queued_tasks.retain(|task| matches!(task, QueuedTask::FollowUp(_)));
                initial_permit.take();
                let saw_shutdown =
                    self.drain_interrupted_commands(&identity.id, &mut commands, &mut queued_tasks);
                if saw_shutdown || shutdown.is_cancelled() {
                    self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
                self.set_status(&identity.id, DelegatedAgentStatus::Interrupted, true);
                self.request_shutdown_descendants(&identity.id);
                retry_undelivered_task = false;
                continue;
            }

            if !queued_tasks.is_empty() && !retry_undelivered_task {
                if agent.is_none() {
                    let child_session = match unopened_session.take() {
                        Some(session) => Ok(session),
                        None => self.reopen_child_session(&session_path),
                    };
                    let build = child_session.and_then(|session| {
                        self.build_child_agent(session, &identity, extension_policy.as_ref())
                    });
                    match build {
                        Ok(child) => agent = Some(child),
                        Err(error) => {
                            // Initialization has not durably accepted the active
                            // task. Release its execution slot, preserve the task
                            // at the FIFO head, and retry only after explicit new
                            // work prevents a persistent failure hot loop.
                            initial_permit.take();
                            self.fail_worker_start(
                                &identity.id,
                                bounded_text(&format!(
                                    "delegated agent could not start; task retained for retry: {error}"
                                )),
                            );
                            self.request_shutdown_descendants(&identity.id);
                            retry_undelivered_task = true;
                            retry_pending_messages = self.pending_message_count(&identity.id);
                            continue;
                        }
                    }
                }
                let permit = if let Some(permit) = initial_permit.take() {
                    permit
                } else {
                    if !self.set_pending_if_needed(&identity.id) {
                        return;
                    }
                    match self.acquire_follow_up_permit(&identity.id, &shutdown).await {
                        PermitWait::Acquired(permit) => permit,
                        PermitWait::Interrupted => {
                            queued_tasks.retain(|task| matches!(task, QueuedTask::FollowUp(_)));
                            let saw_shutdown = self.drain_interrupted_commands(
                                &identity.id,
                                &mut commands,
                                &mut queued_tasks,
                            );
                            if saw_shutdown || shutdown.is_cancelled() {
                                self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                                self.request_shutdown_descendants(&identity.id);
                                return;
                            }
                            self.set_status(&identity.id, DelegatedAgentStatus::Interrupted, true);
                            self.request_shutdown_descendants(&identity.id);
                            continue;
                        }
                        PermitWait::Shutdown => {
                            self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                            self.request_shutdown_descendants(&identity.id);
                            return;
                        }
                    }
                };
                if shutdown.is_cancelled() {
                    drop(permit);
                    self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
                if !self.set_status(&identity.id, DelegatedAgentStatus::Running, true) {
                    return;
                }
                let task = queued_tasks
                    .pop_front()
                    .expect("checked delegated task queue is not empty");
                let active_follow_up_usage = match &task {
                    QueuedTask::Initial(_) => None,
                    QueuedTask::FollowUp(follow_up) => Some(follow_up.usage()),
                };
                let pending = self.take_pending_messages(&identity.id);
                let formatted_task = task.format(&pending);
                let execution = self
                    .execute_child_run(
                        agent.as_mut().expect("delegated agent initialized"),
                        formatted_task,
                        ChildRunContext {
                            identity: &identity,
                            commands: &mut commands,
                            shutdown: &shutdown,
                            extension_policy: extension_policy.as_ref(),
                            deadline,
                        },
                    )
                    .await;
                drop(permit);

                let WorkerExecution {
                    mut outcome,
                    deferred_follow_ups,
                    acknowledged_follow_ups,
                    task_delivered,
                } = execution;
                // An owning terminal signal wins a race with a child terminal
                // event that was already dequeued but not yet recorded.
                if shutdown.is_cancelled() {
                    outcome = WorkerOutcome::Shutdown;
                } else if !matches!(&outcome, WorkerOutcome::Shutdown)
                    && self.interrupt_requested(&identity.id)
                {
                    outcome = WorkerOutcome::Interrupted;
                }
                let mut delivered_follow_ups = acknowledged_follow_ups;
                if task_delivered {
                    self.release_prompt_message_reservations(&identity.id, &pending);
                } else if !matches!(&outcome, WorkerOutcome::Shutdown) {
                    self.restore_pending_messages(&identity.id, pending);
                }
                if task_delivered {
                    if let Some(usage) = active_follow_up_usage {
                        delivered_follow_ups.add_usage(usage);
                    }
                }
                let task_restored =
                    restore_undelivered_task(&mut queued_tasks, task, task_delivered, &outcome);
                self.release_follow_up_usage(&identity.id, delivered_follow_ups);
                let retained_pending_messages = self.pending_message_count(&identity.id);

                match outcome {
                    WorkerOutcome::Shutdown => {
                        self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                        self.request_shutdown_descendants(&identity.id);
                        return;
                    }
                    WorkerOutcome::TimedOut => {
                        self.set_status(&identity.id, DelegatedAgentStatus::TimedOut, true);
                        self.request_shutdown_descendants(&identity.id);
                        return;
                    }
                    WorkerOutcome::Interrupted => {
                        queued_tasks
                            .extend(deferred_follow_ups.into_iter().map(QueuedTask::FollowUp));
                        let saw_shutdown = self.drain_interrupted_commands(
                            &identity.id,
                            &mut commands,
                            &mut queued_tasks,
                        );
                        if saw_shutdown || shutdown.is_cancelled() {
                            self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                            self.request_shutdown_descendants(&identity.id);
                            return;
                        }
                        self.set_status(&identity.id, DelegatedAgentStatus::Interrupted, true);
                        self.request_shutdown_descendants(&identity.id);
                        retry_undelivered_task = false;
                        retry_pending_messages = 0;
                    }
                    WorkerOutcome::Failed(error) => {
                        self.set_status(
                            &identity.id,
                            DelegatedAgentStatus::Failed {
                                error: bounded_text(&error),
                            },
                            true,
                        );
                        self.request_shutdown_descendants(&identity.id);
                        queued_tasks
                            .extend(deferred_follow_ups.into_iter().map(QueuedTask::FollowUp));
                        // A pre-flight prompt failure did not durably accept the
                        // active task. Keep it at the FIFO head, but wait for an
                        // explicit message or follow-up instead of hot-looping
                        // on a persistent storage failure.
                        retry_undelivered_task = task_restored;
                        retry_pending_messages = retained_pending_messages;
                    }
                    WorkerOutcome::Completed(output) => {
                        self.set_status(
                            &identity.id,
                            DelegatedAgentStatus::Completed {
                                output: bounded_text(&output),
                            },
                            true,
                        );
                        queued_tasks
                            .extend(deferred_follow_ups.into_iter().map(QueuedTask::FollowUp));
                        retry_undelivered_task = false;
                        retry_pending_messages = 0;
                    }
                }
                continue;
            }

            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.interrupt_requested(&identity.id) {
                continue;
            }
            let command = tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
                _ = async {
                    if let Some(deadline) = deadline {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if deadline.is_some() => {
                    self.set_status(&identity.id, DelegatedAgentStatus::TimedOut, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
                _ = &mut notified => {
                    if retry_undelivered_task
                        && self.pending_message_count(&identity.id) > retry_pending_messages
                    {
                        retry_undelivered_task = false;
                        retry_pending_messages = 0;
                    }
                    continue;
                },
                command = commands.recv() => command,
            };
            let Some(command) = command else {
                self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, false);
                self.request_shutdown_descendants(&identity.id);
                return;
            };
            match command.kind {
                WorkerCommandKind::Message(message) => {
                    self.queue_reserved_message(&identity.id, message);
                    retry_undelivered_task = false;
                    retry_pending_messages = 0;
                }
                WorkerCommandKind::FollowUp(follow_up) => {
                    queued_tasks.push_back(QueuedTask::FollowUp(follow_up));
                    retry_undelivered_task = false;
                    retry_pending_messages = 0;
                }
                WorkerCommandKind::Shutdown => {
                    self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
            }
        }
    }

    fn build_child_agent(
        self: &Arc<Self>,
        session: Session,
        identity: &AgentIdentity,
        extension_policy: Option<&ExtensionAgentSessionPolicy>,
    ) -> Result<Agent, DelegationError> {
        let parent_path = identity
            .path
            .rsplit_once('/')
            .map(|(parent, _)| {
                if parent.is_empty() {
                    ROOT_AGENT_PATH
                } else {
                    parent
                }
            })
            .unwrap_or(ROOT_AGENT_PATH);
        let base_system = self
            .template
            .base_system
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let (system, extensions, max_turns) = if let Some(policy) = extension_policy {
            let allowed = policy.tools.iter().cloned().collect::<BTreeSet<_>>();
            let (extensions, effective) = self
                .template
                .extensions
                .scoped_tool_snapshot(&allowed)
                .map_err(DelegationError::InvalidConfig)?;
            if effective != policy.tools {
                return Err(DelegationError::InvalidConfig(
                    "effective extension child tool scope changed after spawn admission".into(),
                ));
            }
            (
                format!(
                    "{base_system}\n\nThis is a host-enforced depth-one child session. Only the listed read-only tools are installed; collaboration and mutation tools are unavailable."
                ),
                extensions,
                Some(policy.max_turns),
            )
        } else {
            (
                format!(
                    "{}\n\n{}",
                    base_system,
                    child_instructions(identity, parent_path, &self.config.limits)
                ),
                self.template.extensions.clone(),
                self.template.max_turns,
            )
        };
        let runtime = self
            .template
            .runtime
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut agent = Agent::new(AgentConfig {
            client: self.template.client.clone(),
            model: self.template.model.clone(),
            session,
            system,
            sandbox: self.template.sandbox.clone(),
            effect_broker: self.template.effect_broker.clone(),
            extensions,
            max_turns,
            reasoning: self.template.reasoning.clone(),
            reasoning_mode: self.template.reasoning_mode,
            cache_retention: self.template.cache_retention,
            session_id: None,
        })?;
        if let Some(record) = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .records
            .get_mut(&identity.id)
        {
            record.resource_owner = Some(agent.resource_owner_id().to_owned());
        }
        agent.set_compaction_model(runtime.compaction_model);
        agent.set_compaction_token_mode(
            runtime.auto_compaction_mode,
            runtime.auto_compaction_threshold,
            runtime.compaction_keep_recent_tokens,
        )?;
        agent.set_completion_policy(runtime.completion_policy);
        agent.set_output_modalities(runtime.output_modalities);
        if let Some(policy) = extension_policy {
            agent.inherit_max_output_tokens(runtime.max_output_tokens.min(policy.max_tokens));
            agent.set_max_session_tokens(Some(policy.max_tokens));
            agent.set_max_session_cost_microdollars(Some(policy.max_cost_microdollars));
        } else {
            agent.inherit_max_output_tokens(runtime.max_output_tokens);
            agent.set_max_session_cost_microdollars(runtime.max_session_cost_microdollars);
        }
        agent.set_provider_retries_enabled(runtime.provider_retries_enabled);
        if extension_policy.is_none() {
            let binding = DelegationBinding {
                manager: Arc::clone(self),
                identity: identity.clone(),
                system_instructions: Arc::from(""),
            };
            agent.install_delegation_tools(self.tools(identity));
            agent.set_delegation_binding(binding)?;
        }
        agent.finalize_tool_surface();
        Ok(agent)
    }

    async fn execute_child_run(
        self: &Arc<Self>,
        agent: &mut Agent,
        task: String,
        context: ChildRunContext<'_>,
    ) -> WorkerExecution {
        let ChildRunContext {
            identity,
            commands,
            shutdown,
            extension_policy,
            deadline,
        } = context;
        if deadline.is_some_and(|deadline| deadline <= tokio::time::Instant::now()) {
            return WorkerExecution::new(WorkerOutcome::TimedOut);
        }
        if shutdown.is_cancelled() {
            return WorkerExecution::new(WorkerOutcome::Shutdown);
        }
        if self.interrupt_requested(&identity.id) {
            return WorkerExecution::new(WorkerOutcome::Interrupted);
        }
        let entries_before_prompt = agent.session().entries().len();
        let session_path = agent.session().path().to_path_buf();
        // Bind prompt-error inspection to the exact session object already
        // owned by the child. Reopening the path after `prompt` would allow a
        // directory-entry replacement to misclassify durable task delivery.
        let inspection_file = match agent.session().try_clone_file() {
            Ok(file) => file,
            Err(error) => {
                return WorkerExecution::new(WorkerOutcome::Failed(format!(
                    "delegated task could not start because its session descriptor could not be cloned; task retained for retry: {error}"
                )));
            }
        };
        let persisted_task = task.clone();
        let prompt = agent.prompt(task).await;
        let mut run = match prompt {
            Ok(run) => run,
            Err(error) => {
                // `Run` borrows the agent on success, so inspect a clone of the
                // already-authorized session descriptor on this error path.
                let task_delivered =
                    Session::open_read_only_with_file(&session_path, inspection_file)
                        .ok()
                        .map(|session| {
                            session
                                .entries()
                                .iter()
                                .skip(entries_before_prompt)
                                .any(|entry| {
                                    matches!(
                                        &entry.value,
                                        crate::session::EntryValue::Message(
                                            ygg_ai::Message::User(message)
                                        ) if message.content.len() == 1
                                            && matches!(
                                                &message.content[0],
                                                ygg_ai::UserPart::Text(text)
                                                    if text == &persisted_task
                                            )
                                    )
                                })
                        })
                        .unwrap_or(false);
                let diagnostic = if task_delivered {
                    format!(
                        "delegated run could not start after the task was durably accepted: {error}"
                    )
                } else {
                    format!(
                        "delegated prompt was not durably accepted; task retained for retry: {error}"
                    )
                };
                let mut execution = WorkerExecution::new(WorkerOutcome::Failed(diagnostic));
                execution.task_delivered = task_delivered;
                return execution;
            }
        };
        let control = run.control();

        let output_limit = extension_policy
            .map(|policy| policy.max_output_bytes)
            .unwrap_or(MAX_PROVENANCE_TEXT_BYTES);
        let mut output = String::new();
        // A successful nonblocking control enqueue is not delivery: the agent
        // acknowledges only after the input has been appended durably. Keep the
        // original work and its queue reservation until that event so a run
        // failure cannot silently discard accepted steering or follow-ups.
        let mut submitted_messages = VecDeque::new();
        let mut deferred_messages = VecDeque::new();
        let mut defer_messages = false;
        let mut submitted_follow_ups = VecDeque::new();
        let mut deferred_follow_ups = VecDeque::new();
        let mut defer_follow_ups = false;
        let mut acknowledged_follow_ups = QueueUsage::default();
        let mut requested_interrupt = false;
        let mut requested_shutdown = false;
        let mut requested_timeout = false;
        let mut requested_token_limit = false;
        let mut commands_open = true;
        enum Next {
            Event(Option<AgentEvent>),
            Command(Option<WorkerCommand>),
            Changed,
            Shutdown,
            Deadline,
        }
        let outcome = loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !requested_interrupt && self.interrupt_requested(&identity.id) {
                requested_interrupt = true;
                control.abort();
            }
            let next = tokio::select! {
                biased;
                _ = shutdown.cancelled(), if !requested_shutdown => Next::Shutdown,
                _ = async {
                    if let Some(deadline) = deadline {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if deadline.is_some() && !requested_timeout => Next::Deadline,
                _ = &mut notified, if !requested_interrupt => Next::Changed,
                event = run.next() => Next::Event(event),
                command = commands.recv(), if commands_open => Next::Command(command),
            };
            match next {
                Next::Changed => {}
                Next::Deadline => {
                    control.abort();
                    requested_timeout = true;
                }
                Next::Shutdown => {
                    control.abort();
                    requested_shutdown = true;
                }
                Next::Command(None) => {
                    commands_open = false;
                    control.abort();
                    requested_shutdown = true;
                }
                Next::Command(Some(command)) => match command.kind {
                    WorkerCommandKind::Message(message) => {
                        if defer_messages
                            || requested_interrupt
                            || requested_shutdown
                            || control
                                .try_steer(format_direct_message(&message.from, &message.message))
                                .is_err()
                        {
                            // Once one message cannot enter the active run, keep
                            // later messages behind it to preserve FIFO order.
                            defer_messages = true;
                            deferred_messages.push_back(message);
                        } else {
                            submitted_messages.push_back(message);
                        }
                    }
                    WorkerCommandKind::FollowUp(follow_up) => {
                        if defer_follow_ups
                            || requested_interrupt
                            || requested_shutdown
                            || control
                                .try_follow_up(format_follow_up(
                                    &follow_up.from,
                                    &follow_up.message,
                                    &[],
                                ))
                                .is_err()
                        {
                            // Preserve FIFO across the active run-control queue
                            // and the worker's deferred queue.
                            defer_follow_ups = true;
                            deferred_follow_ups.push_back(follow_up);
                        } else {
                            submitted_follow_ups.push_back(follow_up);
                        }
                    }
                    WorkerCommandKind::Shutdown => {
                        requested_shutdown = true;
                        control.abort();
                    }
                },
                Next::Event(None) => {
                    break if requested_shutdown {
                        WorkerOutcome::Shutdown
                    } else if requested_timeout {
                        WorkerOutcome::TimedOut
                    } else if requested_token_limit {
                        WorkerOutcome::Failed("maximum delegated token budget reached".into())
                    } else if requested_interrupt {
                        WorkerOutcome::Interrupted
                    } else {
                        WorkerOutcome::Failed("delegated run ended without a terminal event".into())
                    };
                }
                Next::Event(Some(AgentEvent::SteeringDelivered { messages })) => {
                    for _ in 0..messages.len() {
                        let Some(message) = submitted_messages.pop_front() else {
                            debug_assert!(false, "steering acknowledgement exceeded submissions");
                            break;
                        };
                        self.release_message_reservation(&identity.id, &message);
                    }
                }
                Next::Event(Some(AgentEvent::FollowUpDelivered { messages })) => {
                    for _ in 0..messages.len() {
                        let Some(follow_up) = submitted_follow_ups.pop_front() else {
                            debug_assert!(false, "follow-up acknowledgement exceeded submissions");
                            break;
                        };
                        acknowledged_follow_ups.add_usage(follow_up.usage());
                    }
                }
                Next::Event(Some(AgentEvent::TurnFinished {
                    message,
                    usage,
                    session_cost_microdollars,
                    ..
                })) => {
                    self.update_agent_usage(&identity.id, usage, session_cost_microdollars);
                    if extension_policy
                        .is_some_and(|policy| delegation_usage_tokens(&usage) > policy.max_tokens)
                    {
                        control.abort();
                        requested_token_limit = true;
                    }
                    for part in message.content {
                        if let AssistantPart::Text(text) = part {
                            if !output.is_empty() {
                                output.push('\n');
                            }
                            output.push_str(&text);
                            if output.len() > output_limit {
                                output = bounded_text_to(&output, output_limit);
                            }
                        }
                    }
                }
                Next::Event(Some(AgentEvent::RunFinished { reason, .. })) => {
                    break if requested_shutdown {
                        WorkerOutcome::Shutdown
                    } else if requested_timeout {
                        WorkerOutcome::TimedOut
                    } else if requested_token_limit {
                        WorkerOutcome::Failed("maximum delegated token budget reached".into())
                    } else if requested_interrupt {
                        WorkerOutcome::Interrupted
                    } else {
                        match reason {
                            FinishReason::Completed => WorkerOutcome::Completed(output),
                            FinishReason::Aborted => WorkerOutcome::Interrupted,
                            FinishReason::Failed(error) => WorkerOutcome::Failed(error.to_string()),
                            FinishReason::MaxTurns => {
                                WorkerOutcome::Failed("maximum delegated turns reached".into())
                            }
                        }
                    };
                }
                Next::Event(Some(_)) => {}
            }
        };

        submitted_messages.extend(deferred_messages);
        for message in submitted_messages {
            self.queue_reserved_message(&identity.id, message);
        }
        // Any unacknowledged control submission is older than work deferred
        // after the control queue filled, so prepend it to retain acceptance
        // order for the next child run.
        submitted_follow_ups.extend(deferred_follow_ups);
        WorkerExecution {
            outcome,
            deferred_follow_ups: submitted_follow_ups,
            acknowledged_follow_ups,
            task_delivered: true,
        }
    }

    fn update_agent_usage(&self, id: &str, usage: Usage, cost_microdollars: Option<u64>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = state.records.get_mut(id) else {
            return;
        };
        record.turn_count = record.turn_count.saturating_add(1);
        record.usage = usage;
        record.cost_microdollars = cost_microdollars;
        drop(state);
        self.changed.notify_waiters();
    }

    fn set_status(&self, id: &str, status: DelegatedAgentStatus, notify_parent: bool) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.persistence_error.is_some() || !state.records.contains_key(id) {
            return false;
        }
        let record = state.records.get(id).expect("checked child exists");
        if record.interrupt_requested
            && matches!(
                status,
                DelegatedAgentStatus::Pending | DelegatedAgentStatus::Running
            )
        {
            return true;
        }
        if record.status == status {
            if matches!(status, DelegatedAgentStatus::Interrupted) {
                state
                    .records
                    .get_mut(id)
                    .expect("checked child exists")
                    .interrupt_requested = false;
            }
            return true;
        }
        let transition_ms = u64::try_from(timestamp_ms()).unwrap_or(u64::MAX);
        if let Err(error) = self.journal.append(&ProvenanceEvent::AgentStatus {
            timestamp_ms: u128::from(transition_ms),
            agent_id: id,
            status: &status,
        }) {
            self.fail_persistence_locked(&mut state, &error);
            return false;
        }
        let notification = {
            let record = state.records.get_mut(id).expect("checked child exists");
            record.status = status;
            if matches!(record.status, DelegatedAgentStatus::Running)
                && record.started_at_ms.is_none()
            {
                record.started_at_ms = Some(transition_ms);
            }
            if !record.status.is_running() && record.completed_at_ms.is_none() {
                record.completed_at_ms = Some(transition_ms);
            }
            if matches!(record.status, DelegatedAgentStatus::Interrupted) {
                record.interrupt_requested = false;
            }
            if matches!(record.status, DelegatedAgentStatus::Shutdown) {
                record.pending_messages.clear();
                record.reserved_messages = QueueUsage::default();
                record.queued_follow_ups = QueueUsage::default();
            }
            (notify_parent && !record.status.is_running()).then(|| {
                (
                    record.parent_id.clone(),
                    MailboxMessage {
                        kind: "task_status",
                        from: id.to_owned(),
                        task_name: Some(record.task_name.clone()),
                        message: status_message(&record.identity.path, &record.status),
                        evictable: true,
                        continued: false,
                        leased: false,
                    },
                )
            })
        };
        if let Some((parent_id, message)) = notification {
            push_mailbox_locked(&mut state, &parent_id, message);
        }
        drop(state);
        self.changed.notify_waiters();
        true
    }

    fn fail_worker_start(&self, id: &str, error: String) {
        self.set_status(id, DelegatedAgentStatus::Failed { error }, true);
    }

    fn set_pending_if_needed(&self, id: &str) -> bool {
        self.set_status(id, DelegatedAgentStatus::Pending, false)
    }

    fn ensure_owner_active_locked(
        &self,
        state: &ManagerState,
        owner: &AgentIdentity,
    ) -> Result<(), String> {
        if let Some(error) = &state.persistence_error {
            return Err(format!("delegation persistence is unavailable: {error}"));
        }
        if state.shutting_down {
            return Err("delegation team is shutting down".into());
        }
        if owner.id == ROOT_AGENT_ID {
            return (state.root_active && owner.path == ROOT_AGENT_PATH && owner.depth == 0)
                .then_some(())
                .ok_or_else(|| "root delegation owner is not active".to_owned());
        }
        let record = state
            .records
            .get(&owner.id)
            .ok_or_else(|| "delegation owner is no longer available".to_owned())?;
        if record.identity.path != owner.path || record.identity.depth != owner.depth {
            return Err("delegation owner identity does not match team state".into());
        }
        if !matches!(record.status, DelegatedAgentStatus::Running)
            || record.interrupt_requested
            || record.shutdown.is_cancelled()
        {
            return Err(format!(
                "delegation owner is not running: {}",
                record.identity.path
            ));
        }
        Ok(())
    }

    fn fail_persistence_locked(&self, state: &mut ManagerState, error: &io::Error) {
        if state.persistence_error.is_some() {
            return;
        }
        let diagnostic = bounded_text(&format!(
            "delegation provenance persistence failed: {error}"
        ));
        state.persistence_error = Some(diagnostic.clone());
        state.shutting_down = true;
        state.root_active = false;
        let mut notifications = Vec::new();
        for record in state.records.values_mut() {
            record.shutdown.cancel();
            let _ = record.command_tx.try_send(WorkerCommand::shutdown());
            record.pending_messages.clear();
            record.reserved_messages = QueueUsage::default();
            record.queued_follow_ups = QueueUsage::default();
            if record.status.is_running() {
                record.status = DelegatedAgentStatus::Failed {
                    error: diagnostic.clone(),
                };
                notifications.push((
                    record.parent_id.clone(),
                    MailboxMessage {
                        kind: "task_status",
                        from: record.identity.id.clone(),
                        task_name: Some(record.task_name.clone()),
                        message: status_message(&record.identity.path, &record.status),
                        evictable: true,
                        continued: false,
                        leased: false,
                    },
                ));
            }
        }
        for (parent_id, message) in notifications {
            push_mailbox_locked(state, &parent_id, message);
        }
        self.changed.notify_waiters();
    }

    fn pending_message_count(&self, id: &str) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.records.get(id).map_or(0, |record| {
            record
                .pending_messages
                .len()
                .saturating_add(record.reserved_messages.messages)
        })
    }

    fn interrupt_requested(&self, id: &str) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .records
            .get(id)
            .is_some_and(|record| record.interrupt_requested)
    }

    async fn acquire_follow_up_permit(
        &self,
        id: &str,
        shutdown: &crate::CancellationToken,
    ) -> PermitWait {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if shutdown.is_cancelled() {
                return PermitWait::Shutdown;
            }
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(record) = state.records.get(id) else {
                    return PermitWait::Shutdown;
                };
                if state.persistence_error.is_some()
                    || state.shutting_down
                    || record.shutdown.is_cancelled()
                {
                    return PermitWait::Shutdown;
                }
                if record.interrupt_requested {
                    return PermitWait::Interrupted;
                }
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return PermitWait::Shutdown,
                _ = &mut notified => {}
                permit = self.current_permits().acquire_owned() => {
                    return match permit {
                        Ok(permit) => PermitWait::Acquired(permit),
                        Err(_) => PermitWait::Shutdown,
                    };
                }
            }
        }
    }

    fn drain_interrupted_commands(
        &self,
        target: &str,
        commands: &mut mpsc::Receiver<WorkerCommand>,
        queued_tasks: &mut VecDeque<QueuedTask>,
    ) -> bool {
        let mut messages = Vec::new();
        let mut saw_shutdown = false;
        while let Ok(command) = commands.try_recv() {
            match command.kind {
                WorkerCommandKind::Message(message) => messages.push(message),
                WorkerCommandKind::FollowUp(follow_up) => {
                    queued_tasks.push_back(QueuedTask::FollowUp(follow_up));
                }
                WorkerCommandKind::Shutdown => saw_shutdown = true,
            }
        }
        for message in messages {
            self.queue_reserved_message(target, message);
        }
        saw_shutdown
    }

    fn take_pending_messages(&self, target: &str) -> Vec<DirectedMessage> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .records
            .get_mut(target)
            .map(|record| {
                let messages = record.pending_messages.drain(..).collect::<Vec<_>>();
                for message in &messages {
                    record
                        .reserved_messages
                        .add(directed_message_bytes(message));
                }
                messages
            })
            .unwrap_or_default()
    }

    fn release_prompt_message_reservations(&self, target: &str, messages: &[DirectedMessage]) {
        if messages.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.records.get_mut(target) {
            for message in messages {
                record.reserved_messages.remove(QueueUsage {
                    messages: 1,
                    bytes: directed_message_bytes(message),
                });
            }
        }
    }

    fn restore_pending_messages(&self, target: &str, messages: Vec<DirectedMessage>) {
        if messages.is_empty() {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let persistence_available = state.persistence_error.is_none();
        if let Some(record) = state.records.get_mut(target) {
            if persistence_available && !record.shutdown.is_cancelled() {
                // These messages were removed from the front immediately
                // before an attempted prompt append. Their reservations kept
                // the queue capacity occupied while delivery was provisional;
                // restore them ahead of later commands.
                for message in messages.into_iter().rev() {
                    record.reserved_messages.remove(QueueUsage {
                        messages: 1,
                        bytes: directed_message_bytes(&message),
                    });
                    debug_assert!(record_can_accept_pending_message(record, &message));
                    record.pending_messages.push_front(message);
                }
            } else {
                for message in messages {
                    record.reserved_messages.remove(QueueUsage {
                        messages: 1,
                        bytes: directed_message_bytes(&message),
                    });
                }
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn queue_reserved_message(&self, target: &str, message: DirectedMessage) {
        let usage = QueueUsage {
            messages: 1,
            bytes: directed_message_bytes(&message),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let persistence_available = state.persistence_error.is_none();
        if let Some(record) = state.records.get_mut(target) {
            record.reserved_messages.remove(usage);
            if !record.shutdown.is_cancelled() && persistence_available {
                debug_assert!(record_can_accept_pending_message(record, &message));
                record.pending_messages.push_back(message);
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn release_message_reservation(&self, target: &str, message: &DirectedMessage) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.records.get_mut(target) {
            record.reserved_messages.remove(QueueUsage {
                messages: 1,
                bytes: directed_message_bytes(message),
            });
        }
    }

    fn release_follow_up_usage(&self, target: &str, usage: QueueUsage) {
        if usage.messages == 0 {
            return;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.records.get_mut(target) {
            record.queued_follow_ups.remove(usage);
        }
    }

    fn resolve_id_locked(state: &ManagerState, target: &str) -> Option<String> {
        if target == ROOT_AGENT_ID || target == ROOT_AGENT_PATH {
            return Some(ROOT_AGENT_ID.into());
        }
        if state.records.contains_key(target) {
            return Some(target.to_owned());
        }
        state
            .records
            .iter()
            .find_map(|(id, record)| (record.identity.path == target).then(|| id.clone()))
    }

    async fn send_message(
        &self,
        owner: &AgentIdentity,
        target: &str,
        message: String,
    ) -> Result<Value, String> {
        validate_durable_text("message", &message)?;
        let candidate = DirectedMessage {
            from: owner.id.clone(),
            message,
        };
        let (target_id, delivery) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_owner_active_locked(&state, owner)?;
            let target_id = Self::resolve_id_locked(&state, target)
                .ok_or_else(|| format!("unknown delegation target: {target}"))?;

            let mut command_permit = None;
            let delivery = if target_id == ROOT_AGENT_ID {
                let mailbox_message = MailboxMessage {
                    kind: "message",
                    from: candidate.from.clone(),
                    task_name: None,
                    message: candidate.message.clone(),
                    evictable: false,
                    continued: false,
                    leased: false,
                };
                if !mailbox_can_accept_after_evicting_automatic(
                    &state.root_mailbox,
                    &mailbox_message,
                ) {
                    return Err("root delegation mailbox is full".into());
                }
                "queued"
            } else {
                let record = state
                    .records
                    .get(&target_id)
                    .expect("resolved child exists");
                if matches!(record.status, DelegatedAgentStatus::Shutdown)
                    || record.shutdown.is_cancelled()
                {
                    return Err(format!("target is shut down: {}", record.identity.path));
                }
                if record.interrupt_requested {
                    return Err(format!(
                        "target is being interrupted: {}",
                        record.identity.path
                    ));
                }
                if !record_can_accept_pending_message(record, &candidate) {
                    return Err(format!(
                        "target pending-message queue is full: {}",
                        record.identity.path
                    ));
                }
                if matches!(record.status, DelegatedAgentStatus::Running) {
                    command_permit = Some(
                        record
                            .command_tx
                            .clone()
                            .try_reserve_owned()
                            .map_err(command_queue_error)?,
                    );
                    "steering"
                } else {
                    "queued"
                }
            };

            if let Err(error) = self.journal.append(&ProvenanceEvent::Message {
                timestamp_ms: timestamp_ms(),
                from: &owner.id,
                to: &target_id,
                kind: "message",
                message: &candidate.message,
            }) {
                let message = format!("could not persist message provenance: {error}");
                self.fail_persistence_locked(&mut state, &error);
                return Err(message);
            }
            if target_id == ROOT_AGENT_ID {
                push_mailbox_bounded(
                    &mut state.root_mailbox,
                    MailboxMessage {
                        kind: "message",
                        from: candidate.from,
                        task_name: None,
                        message: candidate.message,
                        evictable: false,
                        continued: false,
                        leased: false,
                    },
                );
            } else if let Some(permit) = command_permit {
                state
                    .records
                    .get_mut(&target_id)
                    .expect("resolved child exists")
                    .reserved_messages
                    .add(directed_message_bytes(&candidate));
                permit.send(WorkerCommand::message(candidate));
            } else {
                state
                    .records
                    .get_mut(&target_id)
                    .expect("resolved child exists")
                    .pending_messages
                    .push_back(candidate);
            }
            (target_id, delivery)
        };
        self.changed.notify_waiters();
        Ok(json!({"delivered_to": target_id, "delivery": delivery}))
    }

    async fn follow_up(
        &self,
        owner: &AgentIdentity,
        request: FollowUpRequest,
    ) -> Result<Value, String> {
        validate_durable_text("follow-up", &request.message)?;
        let follow_up = QueuedFollowUp {
            from: owner.id.clone(),
            message: request.message,
        };
        let (target_id, target_path, running_now) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_owner_active_locked(&state, owner)?;
            let target_id = Self::resolve_id_locked(&state, &request.target)
                .ok_or_else(|| format!("unknown delegation target: {}", request.target))?;
            if target_id == ROOT_AGENT_ID {
                return Err("followup_task cannot target the root agent".into());
            }
            let record = state
                .records
                .get(&target_id)
                .expect("resolved child exists");
            if matches!(record.status, DelegatedAgentStatus::Shutdown)
                || record.shutdown.is_cancelled()
            {
                return Err(format!("target is shut down: {}", record.identity.path));
            }
            if record.interrupt_requested {
                return Err(format!(
                    "target is being interrupted: {}",
                    record.identity.path
                ));
            }
            if !record_can_accept_follow_up(record, &follow_up) {
                return Err(format!(
                    "target follow-up queue is full: {}",
                    record.identity.path
                ));
            }
            let running_now = record.status.is_running();
            let target_path = record.identity.path.clone();
            let command_permit = record
                .command_tx
                .clone()
                .try_reserve_owned()
                .map_err(command_queue_error)?;

            if let Err(error) = self.journal.append(&ProvenanceEvent::Message {
                timestamp_ms: timestamp_ms(),
                from: &owner.id,
                to: &target_id,
                kind: "follow_up",
                message: &follow_up.message,
            }) {
                let message = format!("could not persist follow-up provenance: {error}");
                self.fail_persistence_locked(&mut state, &error);
                return Err(message);
            }
            if !running_now {
                if let Err(error) = self.journal.append(&ProvenanceEvent::AgentStatus {
                    timestamp_ms: timestamp_ms(),
                    agent_id: &target_id,
                    status: &DelegatedAgentStatus::Pending,
                }) {
                    let message =
                        format!("could not persist pending follow-up status provenance: {error}");
                    self.fail_persistence_locked(&mut state, &error);
                    return Err(message);
                }
                state
                    .records
                    .get_mut(&target_id)
                    .expect("resolved child exists")
                    .status = DelegatedAgentStatus::Pending;
            }
            let usage = follow_up.usage();
            state
                .records
                .get_mut(&target_id)
                .expect("resolved child exists")
                .queued_follow_ups
                .add_usage(usage);
            command_permit.send(WorkerCommand::follow_up(follow_up));
            (target_id, target_path, running_now)
        };
        self.changed.notify_waiters();
        Ok(json!({
            "agent_id": target_id,
            "agent_path": target_path,
            "delivery": if running_now {"follow_up"} else {"new_run"}
        }))
    }

    async fn wait(
        self: &Arc<Self>,
        owner: &AgentIdentity,
        timeout: Duration,
        cancellation: &crate::CancellationToken,
        output_limit: usize,
    ) -> Result<WaitOutput, String> {
        if let Some(result) = self.take_wait_result(owner, output_limit)? {
            return Ok(result);
        }
        let _waiter = self.register_waiter(owner)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.take_wait_result(owner, output_limit)? {
                return Ok(result);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("wait_agent cancelled".into()),
                _ = tokio::time::sleep_until(deadline) => {
                    let agents = self.list_value_for(owner)?;
                    return Ok(WaitOutput {
                        value: json!({"timed_out": true, "messages": [], "agents": agents}),
                        delivery_id: None,
                    });
                }
                _ = &mut notified => {}
            }
        }
    }

    fn register_waiter(&self, owner: &AgentIdentity) -> Result<WaiterGuard<'_>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_owner_active_locked(&state, owner)?;
        if state.active_waiters >= self.config.limits.max_total_agents {
            return Err(format!(
                "delegation waiter limit reached ({})",
                self.config.limits.max_total_agents
            ));
        }
        state.active_waiters += 1;
        Ok(WaiterGuard { manager: self })
    }

    fn take_wait_result(
        &self,
        owner: &AgentIdentity,
        output_limit: usize,
    ) -> Result<Option<WaitOutput>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_owner_active_locked(&state, owner)?;
        let delivery_id = state.next_mailbox_delivery;
        let leased = if owner.id == ROOT_AGENT_ID {
            if state.root_mailbox.is_empty() {
                None
            } else {
                if state.root_mailbox_delivery.is_some() {
                    return Err(
                        "a root mailbox delivery is already awaiting durable acknowledgement"
                            .into(),
                    );
                }
                let (value, plan) =
                    lease_mailbox_page(&mut state.root_mailbox, delivery_id, output_limit)?;
                state.root_mailbox_delivery = Some(plan);
                Some(value)
            }
        } else {
            let record = state
                .records
                .get_mut(&owner.id)
                .expect("validated owner exists");
            if record.mailbox.is_empty() {
                None
            } else {
                if record.mailbox_delivery.is_some() {
                    return Err(
                        "an agent mailbox delivery is already awaiting durable acknowledgement"
                            .into(),
                    );
                }
                let (value, plan) =
                    lease_mailbox_page(&mut record.mailbox, delivery_id, output_limit)?;
                record.mailbox_delivery = Some(plan);
                Some(value)
            }
        };
        if let Some(value) = leased {
            state.next_mailbox_delivery = state.next_mailbox_delivery.saturating_add(1);
            return Ok(Some(WaitOutput {
                value,
                delivery_id: Some(delivery_id),
            }));
        }

        let descendants_running = state.records.values().any(|record| {
            is_descendant_path(&record.identity.path, &owner.path) && record.status.is_running()
        });
        if !descendants_running {
            Ok(Some(WaitOutput {
                value: json!({"timed_out": false, "messages": [], "agents": list_value_locked(&state)}),
                delivery_id: None,
            }))
        } else {
            Ok(None)
        }
    }

    fn resolve_mailbox_delivery(&self, owner_id: &str, delivery_id: u64, delivered: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let resolved = if owner_id == ROOT_AGENT_ID {
            match state.root_mailbox_delivery {
                Some(plan) if plan.id == delivery_id => {
                    state.root_mailbox_delivery = None;
                    resolve_mailbox_page(&mut state.root_mailbox, plan, delivered);
                    true
                }
                _ => false,
            }
        } else if let Some(record) = state.records.get_mut(owner_id) {
            match record.mailbox_delivery {
                Some(plan) if plan.id == delivery_id => {
                    record.mailbox_delivery = None;
                    resolve_mailbox_page(&mut record.mailbox, plan, delivered);
                    true
                }
                _ => false,
            }
        } else {
            false
        };
        drop(state);
        if resolved {
            self.changed.notify_waiters();
        }
    }

    fn list_value_for(&self, owner: &AgentIdentity) -> Result<Value, String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_owner_active_locked(&state, owner)?;
        Ok(list_value_locked(&state))
    }

    async fn interrupt(&self, owner: &AgentIdentity, target: &str) -> Result<Value, String> {
        let (target_id, path, status, requested) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.ensure_owner_active_locked(&state, owner)?;
            let target_id = Self::resolve_id_locked(&state, target)
                .ok_or_else(|| format!("unknown delegation target: {target}"))?;
            if target_id == ROOT_AGENT_ID {
                return Err("interrupt_agent cannot target the root agent".into());
            }
            let record = state
                .records
                .get(&target_id)
                .expect("resolved child exists");
            if !is_descendant_path(&record.identity.path, &owner.path) && owner.id != ROOT_AGENT_ID
            {
                return Err("an agent may only interrupt its descendants".into());
            }
            let path = record.identity.path.clone();
            let status = record.status.clone();
            let requested = status.is_running() && !record.interrupt_requested;
            if requested {
                if let Err(error) = self.journal.append(&ProvenanceEvent::InterruptRequested {
                    timestamp_ms: timestamp_ms(),
                    from: &owner.id,
                    to: &target_id,
                }) {
                    let message = format!("could not persist interrupt provenance: {error}");
                    self.fail_persistence_locked(&mut state, &error);
                    return Err(message);
                }
                state
                    .records
                    .get_mut(&target_id)
                    .expect("resolved child exists")
                    .interrupt_requested = true;
            }
            (target_id, path, status, requested)
        };
        if requested {
            self.request_shutdown_descendants(&target_id);
            self.changed.notify_waiters();
        }
        Ok(
            json!({"agent_id": target_id, "agent_path": path, "previous_status": status.label(), "interrupt_requested": requested}),
        )
    }

    fn request_shutdown_descendants(&self, owner_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owner_path = if owner_id == ROOT_AGENT_ID {
            state.root_active = false;
            ROOT_AGENT_PATH.to_owned()
        } else if let Some(record) = state.records.get(owner_id) {
            record.identity.path.clone()
        } else {
            return;
        };
        for record in state
            .records
            .values()
            .filter(|record| is_descendant_path(&record.identity.path, &owner_path))
        {
            record.shutdown.cancel();
            let _ = record.command_tx.try_send(WorkerCommand::shutdown());
        }
        drop(state);
        self.changed.notify_waiters();
    }

    fn request_shutdown_agent_trees(&self, roots: &BTreeSet<String>) {
        if roots.is_empty() {
            return;
        }
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let root_paths = roots
            .iter()
            .filter_map(|id| {
                state
                    .records
                    .get(id)
                    .map(|record| record.identity.path.clone())
            })
            .collect::<Vec<_>>();
        for record in state.records.values().filter(|record| {
            roots.contains(&record.identity.id)
                || root_paths
                    .iter()
                    .any(|root| is_descendant_path(&record.identity.path, root))
        }) {
            record.shutdown.cancel();
            let _ = record.command_tx.try_send(WorkerCommand::shutdown());
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

impl Drop for DelegationManager {
    fn drop(&mut self) {
        let _ = self.journal.append(&ProvenanceEvent::TeamShutdown {
            timestamp_ms: timestamp_ms(),
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.shutting_down = true;
        state.root_active = false;
        for record in state.records.values() {
            record.shutdown.cancel();
            let _ = record.command_tx.try_send(WorkerCommand::shutdown());
        }
    }
}

enum PermitWait {
    Acquired(OwnedSemaphorePermit),
    Interrupted,
    Shutdown,
}

#[derive(Clone)]
struct QueuedFollowUp {
    from: String,
    message: String,
}

impl QueuedFollowUp {
    fn usage(&self) -> QueueUsage {
        QueueUsage {
            messages: 1,
            bytes: self.from.len().saturating_add(self.message.len()),
        }
    }
}

enum QueuedTask {
    Initial(String),
    FollowUp(QueuedFollowUp),
}

impl QueuedTask {
    fn format(&self, pending: &[DirectedMessage]) -> String {
        match self {
            Self::Initial(task) => format_initial_task(task, pending),
            Self::FollowUp(follow_up) => {
                format_follow_up(&follow_up.from, &follow_up.message, pending)
            }
        }
    }
}

fn restore_undelivered_task(
    queued_tasks: &mut VecDeque<QueuedTask>,
    task: QueuedTask,
    task_delivered: bool,
    outcome: &WorkerOutcome,
) -> bool {
    let should_restore = !task_delivered
        && match outcome {
            WorkerOutcome::Shutdown | WorkerOutcome::TimedOut => false,
            WorkerOutcome::Interrupted => matches!(&task, QueuedTask::FollowUp(_)),
            WorkerOutcome::Completed(_) | WorkerOutcome::Failed(_) => true,
        };
    if !should_restore {
        return false;
    }
    queued_tasks.push_front(task);
    true
}

struct WorkerExecution {
    outcome: WorkerOutcome,
    deferred_follow_ups: VecDeque<QueuedFollowUp>,
    acknowledged_follow_ups: QueueUsage,
    task_delivered: bool,
}

impl WorkerExecution {
    fn new(outcome: WorkerOutcome) -> Self {
        Self {
            outcome,
            deferred_follow_ups: VecDeque::new(),
            acknowledged_follow_ups: QueueUsage::default(),
            task_delivered: false,
        }
    }
}

enum WorkerOutcome {
    Completed(String),
    Interrupted,
    TimedOut,
    Failed(String),
    Shutdown,
}

struct WaiterGuard<'a> {
    manager: &'a DelegationManager,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        let mut state = self
            .manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active_waiters = state.active_waiters.saturating_sub(1);
    }
}

#[derive(Clone, Copy)]
enum CollaborationToolKind {
    Spawn,
    FollowUp,
    SendMessage,
    Wait,
    List,
    Interrupt,
}

impl CollaborationToolKind {
    const ALL: [Self; 6] = [
        Self::Spawn,
        Self::FollowUp,
        Self::SendMessage,
        Self::Wait,
        Self::List,
        Self::Interrupt,
    ];

    fn definition(self) -> ToolDef {
        match self {
            Self::Spawn => tool_def(
                "spawn_agent",
                "Spawn an isolated child agent for an independent task. Returns immediately; use wait_agent or list_agents for status.",
                json!({
                    "type": "object",
                    "properties": {
                        "task_name": {"type": "string", "description": "Unique lowercase task name under this agent (letters, digits, underscore, hyphen)."},
                        "message": {"type": "string", "description": "Complete task and relevant context for the child."}
                    },
                    "required": ["task_name", "message"],
                    "additionalProperties": false
                }),
            ),
            Self::FollowUp => tool_def(
                "followup_task",
                "Send additional work to a delegated agent. It is queued after an active run or starts a new run when idle.",
                target_message_schema(),
            ),
            Self::SendMessage => tool_def(
                "send_message",
                "Send information to another agent. Active agents receive steering; idle agents receive it with their next task.",
                target_message_schema(),
            ),
            Self::Wait => tool_def(
                "wait_agent",
                "Wait for delegated-agent messages or status changes. Returns immediately if this agent has messages or no descendants are running.",
                json!({
                    "type": "object",
                    "properties": {
                        "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TOOL_TIMEOUT_MS, "description": "Maximum wait in milliseconds (default 30000)."}
                    },
                    "additionalProperties": false
                }),
            ),
            Self::List => tool_def(
                "list_agents",
                "List every agent in this delegation team, including durable session paths and current status.",
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
            ),
            Self::Interrupt => tool_def(
                "interrupt_agent",
                "Interrupt a running descendant agent and propagate cancellation to its descendants.",
                json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Agent ID or absolute delegation path."}
                    },
                    "required": ["target"],
                    "additionalProperties": false
                }),
            ),
        }
    }
}

struct CollaborationTool {
    manager: Weak<DelegationManager>,
    owner: AgentIdentity,
    kind: CollaborationToolKind,
}

#[async_trait::async_trait]
impl Tool for CollaborationTool {
    fn definition(&self) -> ToolDef {
        self.kind.definition()
    }

    fn effect(&self, _args: &Value, _ctx: &ToolContext<'_>) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Delegation)
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| ToolError::new("delegation team is no longer available"))?;
        if matches!(self.kind, CollaborationToolKind::Wait) {
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000)
                .clamp(1, MAX_TOOL_TIMEOUT_MS);
            let wait = manager
                .wait(
                    &self.owner,
                    Duration::from_millis(timeout_ms),
                    &ctx.cancellation,
                    ctx.sandbox.max_output_bytes,
                )
                .await
                .map_err(ToolError::new)?;
            let text = match serde_json::to_string(&wait.value) {
                Ok(text) => text,
                Err(error) => {
                    if let Some(delivery_id) = wait.delivery_id {
                        manager.resolve_mailbox_delivery(&self.owner.id, delivery_id, false);
                    }
                    return Err(ToolError::new(format!(
                        "could not encode collaboration result: {error}"
                    )));
                }
            };
            let mut output = ToolOutput::new(text);
            if let Some(delivery_id) = wait.delivery_id {
                let commit_manager = self.manager.clone();
                let rollback_manager = self.manager.clone();
                let commit_owner = self.owner.id.clone();
                let rollback_owner = self.owner.id.clone();
                output = output.with_delivery_commit(
                    move || {
                        if let Some(manager) = commit_manager.upgrade() {
                            manager.resolve_mailbox_delivery(&commit_owner, delivery_id, true);
                        }
                    },
                    move || {
                        if let Some(manager) = rollback_manager.upgrade() {
                            manager.resolve_mailbox_delivery(&rollback_owner, delivery_id, false);
                        }
                    },
                );
            }
            return Ok(output);
        }

        let value = match self.kind {
            CollaborationToolKind::Spawn => {
                let request = SpawnRequest {
                    task_name: required_string(&args, "task_name")?,
                    display_task_name: None,
                    message: required_string(&args, "message")?,
                    extension_policy: None,
                    extension_provenance: None,
                };
                manager.spawn(&self.owner, request)
            }
            CollaborationToolKind::FollowUp => {
                let request = FollowUpRequest {
                    target: required_string(&args, "target")?,
                    message: required_string(&args, "message")?,
                };
                manager.follow_up(&self.owner, request).await
            }
            CollaborationToolKind::SendMessage => {
                let target = required_string(&args, "target")?;
                let message = required_string(&args, "message")?;
                manager.send_message(&self.owner, &target, message).await
            }
            CollaborationToolKind::Wait => unreachable!("wait returned above"),
            CollaborationToolKind::List => manager.list_value_for(&self.owner),
            CollaborationToolKind::Interrupt => {
                let target = required_string(&args, "target")?;
                manager.interrupt(&self.owner, &target).await
            }
        }
        .map_err(ToolError::new)?;
        serde_json::to_string(&value)
            .map(ToolOutput::new)
            .map_err(|error| {
                ToolError::new(format!("could not encode collaboration result: {error}"))
            })
    }
}

pub(crate) fn enable_root_delegation(
    agent: &mut Agent,
    config: DelegationConfig,
    template: DelegationTemplate,
) -> Result<DelegationBinding, DelegationError> {
    for name in &COLLABORATION_TOOL_NAMES {
        if agent
            .registered_tool_names()
            .iter()
            .any(|registered| registered == name)
        {
            return Err(DelegationError::DuplicateTool((*name).into()));
        }
    }
    let manager = DelegationManager::create(config, template, agent.session().path())?;
    let binding = manager.root_binding();
    agent.append_system_instructions(binding.system_instructions().to_owned());
    agent.install_delegation_tools(manager.tools(&binding.identity));
    Ok(binding)
}

fn root_instructions(config: &DelegationConfig) -> String {
    let proactive = match config.mode {
        DelegationMode::Available => {
            "Delegation is available when the user or task explicitly benefits from separate agents."
        }
        DelegationMode::Proactive => {
            "Use sub-agents proactively when parallel work would materially improve speed or quality."
        }
    };
    format!(
        "<ygg_multi_agent_v2>\nYou are {ROOT_AGENT_PATH}, the root of a bounded agent team. {proactive}\nUse spawn_agent for independent work, send_message for timely context, followup_task for additional work, wait_agent/list_agents to coordinate, and interrupt_agent to stop obsolete work. Integrate and verify child results yourself; do not present unverified child output as fact. Delegation is bounded to {} concurrent agents including you, depth {}, and {} total agents.\n</ygg_multi_agent_v2>",
        config.limits.max_concurrent_agents,
        config.limits.max_depth,
        config.limits.max_total_agents
    )
}

fn child_instructions(
    identity: &AgentIdentity,
    parent_path: &str,
    limits: &DelegationLimits,
) -> String {
    format!(
        "<ygg_multi_agent_v2>\nYou are {}, delegated by {}. Complete the assigned task independently and return a concise, evidence-based result. Use send_message for information your parent needs before completion. You may spawn useful independent sub-agents within the remaining bounds (max depth {}, max {} concurrent including root). Coordinate with wait_agent/list_agents and interrupt obsolete descendants. Your final response is delivered automatically to your parent.\n</ygg_multi_agent_v2>",
        identity.path, parent_path, limits.max_depth, limits.max_concurrent_agents
    )
}

fn format_initial_task(task: &str, pending: &[DirectedMessage]) -> String {
    if pending.is_empty() {
        return task.to_owned();
    }
    let mut formatted = String::new();
    for directed in pending {
        formatted.push_str(&format_direct_message(&directed.from, &directed.message));
        formatted.push_str("\n\n");
    }
    formatted.push_str(task);
    formatted
}

fn format_direct_message(from: &str, message: &str) -> String {
    format!(
        "<agent_message from=\"{}\">\n{}\n</agent_message>",
        from, message
    )
}

fn format_follow_up(from: &str, message: &str, pending: &[DirectedMessage]) -> String {
    let mut formatted = String::new();
    for directed in pending {
        formatted.push_str(&format_direct_message(&directed.from, &directed.message));
        formatted.push_str("\n\n");
    }
    formatted.push_str(&format!(
        "<followup_task from=\"{}\">\n{}\n</followup_task>",
        from, message
    ));
    formatted
}

fn mailbox_message_bytes(message: &MailboxMessage) -> usize {
    message.kind.len()
        + message.from.len()
        + message.task_name.as_ref().map_or(0, String::len)
        + message.message.len()
}

fn mailbox_can_accept(mailbox: &VecDeque<MailboxMessage>, message: &MailboxMessage) -> bool {
    mailbox.len() < MAX_MAILBOX_MESSAGES
        && mailbox
            .iter()
            .fold(0usize, |total, item| {
                total.saturating_add(mailbox_message_bytes(item))
            })
            .saturating_add(mailbox_message_bytes(message))
            <= MAX_MAILBOX_BYTES
}

fn mailbox_can_accept_after_evicting_automatic(
    mailbox: &VecDeque<MailboxMessage>,
    message: &MailboxMessage,
) -> bool {
    let message_bytes = mailbox_message_bytes(message);
    if message_bytes > MAX_MAILBOX_BYTES {
        return false;
    }
    let mut entries = mailbox.len();
    let mut bytes = mailbox.iter().fold(0usize, |total, item| {
        total.saturating_add(mailbox_message_bytes(item))
    });
    if entries < MAX_MAILBOX_MESSAGES && bytes.saturating_add(message_bytes) <= MAX_MAILBOX_BYTES {
        return true;
    }
    for entry in mailbox
        .iter()
        .filter(|entry| entry.evictable && !entry.leased)
    {
        entries = entries.saturating_sub(1);
        bytes = bytes.saturating_sub(mailbox_message_bytes(entry));
        if entries < MAX_MAILBOX_MESSAGES
            && bytes.saturating_add(message_bytes) <= MAX_MAILBOX_BYTES
        {
            return true;
        }
    }
    false
}

fn push_mailbox_bounded(mailbox: &mut VecDeque<MailboxMessage>, message: MailboxMessage) {
    let message_bytes = mailbox_message_bytes(&message);
    if message_bytes > MAX_MAILBOX_BYTES {
        return;
    }
    while !mailbox_can_accept(mailbox, &message) {
        let Some(index) = mailbox
            .iter()
            .position(|entry| entry.evictable && !entry.leased)
        else {
            // Accepted direct messages are durable work and must never be evicted by
            // best-effort automatic notifications.
            return;
        };
        mailbox.remove(index);
    }
    mailbox.push_back(message);
}

fn mailbox_delivery_message(message: &MailboxMessage, text: &str, remaining_bytes: usize) -> Value {
    let mut value = json!({
        "kind": message.kind,
        "from": message.from,
        "task_name": message.task_name,
        "message": text,
    });
    let object = value
        .as_object_mut()
        .expect("mailbox delivery message is an object");
    if message.continued {
        object.insert("continued".into(), Value::Bool(true));
    }
    if remaining_bytes > 0 {
        object.insert("remaining_bytes".into(), json!(remaining_bytes));
    }
    value
}

fn mailbox_delivery_value(messages: Vec<Value>, more: bool) -> Value {
    json!({"timed_out": false, "messages": messages, "more": more})
}

fn encoded_value_len(value: &Value) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|encoded| encoded.len())
        .map_err(|error| format!("could not encode mailbox delivery: {error}"))
}

fn lease_mailbox_page(
    mailbox: &mut VecDeque<MailboxMessage>,
    delivery_id: u64,
    output_limit: usize,
) -> Result<(Value, MailboxDeliveryPlan), String> {
    if mailbox.iter().any(|message| message.leased) {
        return Err("a mailbox delivery is already awaiting durable acknowledgement".into());
    }

    let mut rendered = Vec::new();
    let mut complete_messages = 0usize;
    for message in mailbox.iter() {
        let mut candidate = rendered.clone();
        candidate.push(mailbox_delivery_message(message, &message.message, 0));
        // `false` is one byte longer than `true`, so this remains safe if
        // overflow means the final page needs to advertise `more: true`.
        if encoded_value_len(&mailbox_delivery_value(candidate, false))? > output_limit {
            break;
        }
        rendered.push(mailbox_delivery_message(message, &message.message, 0));
        complete_messages += 1;
    }

    let (value, partial_bytes, touched_messages) = if complete_messages > 0 {
        let more = complete_messages < mailbox.len();
        (mailbox_delivery_value(rendered, more), 0, complete_messages)
    } else {
        let message = mailbox
            .front()
            .expect("mailbox page is created only for a non-empty mailbox");
        let boundaries = message
            .message
            .char_indices()
            .map(|(index, _)| index)
            .skip(1)
            .filter(|index| *index < message.message.len())
            .collect::<Vec<_>>();
        let mut low = 0usize;
        let mut high = boundaries.len();
        let mut best = None;
        while low < high {
            let middle = low + (high - low) / 2;
            let end = boundaries[middle];
            let remaining = message.message.len() - end;
            let candidate = mailbox_delivery_value(
                vec![mailbox_delivery_message(
                    message,
                    &message.message[..end],
                    remaining,
                )],
                true,
            );
            if encoded_value_len(&candidate)? <= output_limit {
                best = Some((end, candidate));
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let Some((partial_bytes, value)) = best else {
            return Err(format!(
                "delegation tool-output limit ({output_limit} bytes) is too small for one mailbox message chunk"
            ));
        };
        (value, partial_bytes, 1)
    };

    debug_assert!(encoded_value_len(&value).is_ok_and(|length| length <= output_limit));
    for message in mailbox.iter_mut().take(touched_messages) {
        message.leased = true;
    }
    Ok((
        value,
        MailboxDeliveryPlan {
            id: delivery_id,
            complete_messages,
            partial_bytes,
            touched_messages,
        },
    ))
}

fn resolve_mailbox_page(
    mailbox: &mut VecDeque<MailboxMessage>,
    plan: MailboxDeliveryPlan,
    delivered: bool,
) {
    if !delivered {
        for message in mailbox.iter_mut().take(plan.touched_messages) {
            message.leased = false;
        }
        return;
    }

    for _ in 0..plan.complete_messages {
        let removed = mailbox
            .pop_front()
            .expect("leased complete mailbox message still exists");
        debug_assert!(removed.leased);
    }
    if plan.partial_bytes > 0 {
        let message = mailbox
            .front_mut()
            .expect("leased partial mailbox message still exists");
        debug_assert!(message.leased && message.message.is_char_boundary(plan.partial_bytes));
        message.message.drain(..plan.partial_bytes);
        message.continued = true;
        message.leased = false;
    }
}

fn push_mailbox_locked(state: &mut ManagerState, target: &str, message: MailboxMessage) {
    if target == ROOT_AGENT_ID {
        push_mailbox_bounded(&mut state.root_mailbox, message);
    } else if let Some(record) = state.records.get_mut(target) {
        push_mailbox_bounded(&mut record.mailbox, message);
    }
}

fn directed_message_bytes(message: &DirectedMessage) -> usize {
    message.from.len().saturating_add(message.message.len())
}

fn record_can_accept_pending_message(record: &AgentRecord, message: &DirectedMessage) -> bool {
    let pending_bytes = record.pending_messages.iter().fold(0usize, |total, item| {
        total.saturating_add(directed_message_bytes(item))
    });
    record
        .pending_messages
        .len()
        .saturating_add(record.reserved_messages.messages)
        < MAX_PENDING_MESSAGES
        && pending_bytes
            .saturating_add(record.reserved_messages.bytes)
            .saturating_add(directed_message_bytes(message))
            <= MAX_PENDING_MESSAGE_BYTES
}

fn record_can_accept_follow_up(record: &AgentRecord, follow_up: &QueuedFollowUp) -> bool {
    let usage = follow_up.usage();
    record.queued_follow_ups.messages < MAX_QUEUED_FOLLOW_UPS
        && record.queued_follow_ups.bytes.saturating_add(usage.bytes) <= MAX_QUEUED_FOLLOW_UP_BYTES
}

fn command_queue_error<T>(error: tokio::sync::mpsc::error::TrySendError<T>) -> String {
    match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => {
            "target delegation command queue is full".into()
        }
        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
            "target worker is no longer available".into()
        }
    }
}

fn status_message(path: &str, status: &DelegatedAgentStatus) -> String {
    match status {
        DelegatedAgentStatus::Completed { output } => {
            format!("{path} completed:\n{output}")
        }
        DelegatedAgentStatus::Failed { error } => format!("{path} failed: {error}"),
        DelegatedAgentStatus::Interrupted => format!("{path} was interrupted"),
        DelegatedAgentStatus::TimedOut => format!("{path} timed out"),
        DelegatedAgentStatus::Shutdown => format!("{path} was shut down"),
        DelegatedAgentStatus::Pending | DelegatedAgentStatus::Running => {
            format!("{path} is {}", status.label())
        }
    }
}

fn list_value_locked(state: &ManagerState) -> Value {
    let agents = state
        .records
        .values()
        .map(agent_record_value)
        .collect::<Vec<_>>();
    json!({"agents": agents, "persistence_error": state.persistence_error})
}

fn agent_record_value(record: &AgentRecord) -> Value {
    json!({
        "agent_id": record.identity.id,
        "agent_path": record.identity.path,
        "parent_id": record.parent_id,
        "task_name": record.display_task_name.as_deref().unwrap_or(record.task_name.as_str()),
        "depth": record.identity.depth,
        "session": record.session_path,
        "status": record.status,
        "policy": record.extension_policy,
        "profile": record.extension_profile,
        "idempotency_key": record.extension_idempotency_key,
        "fingerprint": record.extension_fingerprint,
        "created_at_ms": record.created_at_ms,
        "started_at_ms": record.started_at_ms,
        "completed_at_ms": record.completed_at_ms,
        "turn_count": record.turn_count,
        "usage": record.usage,
        "cost_microdollars": record.cost_microdollars,
        "deadline_at_ms": record.deadline_at_ms,
    })
}

fn target_message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "description": "Agent ID or absolute delegation path."},
            "message": {"type": "string", "description": "Information or task text to deliver."}
        },
        "required": ["target", "message"],
        "additionalProperties": false
    })
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        parameters: input_schema,
    }
}

fn required_string(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ToolError::new(format!("{key} must be a non-empty string")))
}

fn validate_task_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 48 {
        return Err("task_name must contain 1 to 48 characters".into());
    }
    if !name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
    }) {
        return Err(
            "task_name may contain only lowercase ASCII letters, digits, underscore, and hyphen"
                .into(),
        );
    }
    Ok(())
}

fn is_descendant_path(candidate: &str, parent: &str) -> bool {
    candidate.len() > parent.len()
        && candidate.starts_with(parent)
        && candidate.as_bytes().get(parent.len()) == Some(&b'/')
}

fn validate_durable_text(kind: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_PROVENANCE_TEXT_BYTES {
        return Err(format!(
            "{kind} exceeds the {}-byte delegation limit",
            MAX_PROVENANCE_TEXT_BYTES
        ));
    }
    Ok(())
}

fn delegation_usage_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage
            .input_tokens
            .saturating_add(usage.cache_read_tokens)
            .saturating_add(usage.cache_write_tokens)
            .saturating_add(usage.output_tokens)
    }
}

fn bounded_text_to(text: &str, limit: usize) -> String {
    const SUFFIX: &str = "\n...[truncated]";
    if text.len() <= limit {
        return text.to_owned();
    }
    let suffix = if limit >= SUFFIX.len() { SUFFIX } else { "" };
    let mut end = limit.saturating_sub(suffix.len()).min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], suffix)
}

fn bounded_text(text: &str) -> String {
    bounded_text_to(text, MAX_PROVENANCE_TEXT_BYTES)
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn create_private_team_directory(
    parent: &Path,
) -> Result<Arc<secure_fs::PrivateDirectory>, DelegationError> {
    let parent = std::path::absolute(parent)?;
    Ok(Arc::new(secure_fs::create_bound_private_directory(
        &parent, "team-",
    )?))
}

fn cleanup_failed_team_activation(
    team_directory: &secure_fs::PrivateDirectory,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let journal = team_directory.path().join("provenance.jsonl");
    if let Err(error) = team_directory.remove_regular_file_if_exists(&journal) {
        failures.push(format!("remove provenance journal: {error}"));
    }
    if let Err(error) = team_directory.remove_empty_if_exists() {
        failures.push(format!("remove team directory: {error}"));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_session_references_are_stable_path_free_and_strict() {
        let first =
            Path::new("/private/sessions/.delegation/team-0123456789abcdef/0001-review.jsonl");
        let same_leaf_elsewhere =
            Path::new("/other/private/team-0123456789abcdef/0001-review.jsonl");
        let reference = delegated_session_reference(first).unwrap();
        assert_eq!(reference, delegated_session_reference(first).unwrap());
        assert_eq!(
            reference,
            delegated_session_reference(same_leaf_elsewhere).unwrap()
        );
        assert!(reference.starts_with("agent-session:"));
        assert_eq!(reference.len(), "agent-session:".len() + 64);
        assert!(!reference.contains("review"));
        assert!(delegated_session_reference(Path::new(
            "/private/sessions/.delegation/not-a-team/0001-review.jsonl"
        ))
        .is_none());
        assert!(delegated_session_reference(Path::new(
            "/private/sessions/.delegation/team-safe/../outside.jsonl"
        ))
        .is_none());
    }

    fn test_extension_policy() -> ExtensionAgentSessionPolicy {
        ExtensionAgentSessionPolicy {
            tools: vec!["read".into(), "search".into()],
            max_depth: 1,
            max_concurrent_children: 2,
            max_turns: 4,
            max_tokens: 32_000,
            max_cost_microdollars: 200_000,
            max_output_bytes: 8 * 1024,
            timeout_ms: 300_000,
        }
    }

    fn test_extension_spawn(
        task_name: &str,
        profile: Option<&str>,
        fingerprint: Option<&str>,
        message: &str,
        idempotency_key: &str,
    ) -> ExtensionDelegationSpawnRequest {
        ExtensionDelegationSpawnRequest {
            task_name: task_name.into(),
            profile: profile.map(str::to_owned),
            fingerprint: fingerprint.map(str::to_owned),
            message: message.into(),
            idempotency_key: idempotency_key.into(),
            policy: test_extension_policy(),
        }
    }

    fn test_template(directory: &Path) -> DelegationTemplate {
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        let max_output_tokens = model.spec.limits.max_output_tokens;
        DelegationTemplate {
            client: ygg_ai::AiClient::new(),
            model,
            base_system: RwLock::new("test".into()),
            sandbox: crate::SandboxConfig::new(directory),
            effect_broker: crate::EffectBroker::default(),
            extensions: ExtensionHost::new(),
            max_turns: Some(4),
            reasoning: ygg_ai::ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
            cache_retention: ygg_ai::CacheRetention::Short,
            runtime: RwLock::new(DelegationRuntimeSettings {
                compaction_model: None,
                auto_compaction_mode: AgentCompactionMode::Local,
                auto_compaction_threshold: 0.85,
                compaction_keep_recent_tokens: 1_024,
                completion_policy: CompletionPolicy::Natural,
                output_modalities: ygg_ai::OutputModalities::Text,
                max_output_tokens,
                max_session_cost_microdollars: None,
                provider_retries_enabled: true,
            }),
        }
    }

    fn manager_with_journal(file: File, directory: &Path) -> Arc<DelegationManager> {
        Arc::new(DelegationManager {
            config: DelegationConfig::new(directory),
            team_directory: directory.to_path_buf(),
            team_storage: None,
            journal: ProvenanceJournal {
                file: Mutex::new(file),
            },
            template: test_template(directory),
            state: Mutex::new(ManagerState {
                next_agent_number: 1,
                total_agents: 1,
                active_waiters: 0,
                records: BTreeMap::new(),
                root_mailbox: VecDeque::new(),
                next_mailbox_delivery: 1,
                root_mailbox_delivery: None,
                persistence_error: None,
                shutting_down: false,
                root_active: true,
                root_resource_owner: None,
            }),
            permits: RwLock::new(Arc::new(Semaphore::new(3))),
            changed: Notify::new(),
        })
    }

    fn read_only_journal(directory: &Path) -> File {
        let path = directory.join("read-only-journal");
        std::fs::write(&path, b"").unwrap();
        File::open(path).unwrap()
    }

    fn writable_manager(directory: &Path) -> Arc<DelegationManager> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(directory.join("provenance.jsonl"))
            .unwrap();
        manager_with_journal(file, directory)
    }

    fn writable_manager_with_core_tools(directory: &Path) -> Arc<DelegationManager> {
        let mut manager = writable_manager(directory);
        let manager_mut = Arc::get_mut(&mut manager).expect("new manager is uniquely owned");
        manager_mut.template.extensions.load(&crate::CoreTools);
        manager
    }

    #[cfg(unix)]
    fn writable_manager_with_workspace(
        directory: &Path,
        workspace: &Path,
    ) -> Arc<DelegationManager> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(directory.join("provenance.jsonl"))
            .unwrap();
        Arc::new(DelegationManager {
            config: DelegationConfig::new(directory),
            team_directory: directory.to_path_buf(),
            team_storage: None,
            journal: ProvenanceJournal {
                file: Mutex::new(file),
            },
            template: test_template(workspace),
            state: Mutex::new(ManagerState {
                next_agent_number: 1,
                total_agents: 1,
                active_waiters: 0,
                records: BTreeMap::new(),
                root_mailbox: VecDeque::new(),
                next_mailbox_delivery: 1,
                root_mailbox_delivery: None,
                persistence_error: None,
                shutting_down: false,
                root_active: true,
                root_resource_owner: None,
            }),
            permits: RwLock::new(Arc::new(Semaphore::new(3))),
            changed: Notify::new(),
        })
    }

    #[test]
    fn owning_run_restart_reactivates_root_with_fresh_execution_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let root = manager.root_binding().identity;
        let old_permits = manager.current_permits();
        let _old_slots = (0..3)
            .map(|_| Arc::clone(&old_permits).try_acquire_owned().unwrap())
            .collect::<Vec<_>>();
        {
            let mut state = manager.state.lock().unwrap();
            state.root_mailbox.push_back(MailboxMessage {
                kind: "message",
                from: "agent-old".into(),
                task_name: None,
                message: "durable root message".into(),
                evictable: false,
                continued: false,
                leased: false,
            });
            state.root_mailbox.push_back(MailboxMessage {
                kind: "task_status",
                from: "agent-old".into(),
                task_name: Some("old".into()),
                message: "stale status".into(),
                evictable: true,
                continued: false,
                leased: false,
            });
        }

        manager.request_shutdown_descendants(ROOT_AGENT_ID);
        assert!(manager.list_value_for(&root).is_err());

        manager.prepare_owning_run(&root).unwrap();
        assert!(manager.list_value_for(&root).is_ok());
        assert!(manager.current_permits().try_acquire_owned().is_ok());
        let state = manager
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(state.root_active);
        assert_eq!(state.total_agents, 1);
        assert!(state.records.is_empty());
        assert_eq!(state.root_mailbox.len(), 2);
        assert_eq!(state.root_mailbox[0].message, "durable root message");
        assert_eq!(state.root_mailbox[1].message, "stale status");
    }

    #[test]
    fn child_owning_run_restart_cancels_descendants_and_preserves_durable_mail() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _child_commands) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        let grandchild = AgentIdentity {
            id: "agent-2".into(),
            path: "/root/child/grandchild".into(),
            depth: 2,
        };
        let grandchild_shutdown = crate::CancellationToken::default();
        let (command_tx, _grandchild_commands) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        {
            let mut state = manager.state.lock().unwrap();
            let child_record = state.records.get_mut(&child.id).unwrap();
            child_record.mailbox.push_back(MailboxMessage {
                kind: "message",
                from: ROOT_AGENT_ID.into(),
                task_name: None,
                message: "leased message from the prior owning run".into(),
                evictable: false,
                continued: false,
                leased: true,
            });
            child_record.mailbox_delivery = Some(MailboxDeliveryPlan {
                id: 1,
                complete_messages: 1,
                partial_bytes: 0,
                touched_messages: 1,
            });
            child_record.mailbox.push_back(MailboxMessage {
                kind: "task_status",
                from: "agent-old".into(),
                task_name: Some("old".into()),
                message: "stale automatic status".into(),
                evictable: true,
                continued: false,
                leased: false,
            });
            child_record.mailbox.push_back(MailboxMessage {
                kind: "message",
                from: ROOT_AGENT_ID.into(),
                task_name: None,
                message: "unleased durable message".into(),
                evictable: false,
                continued: false,
                leased: false,
            });
            state.records.insert(
                grandchild.id.clone(),
                AgentRecord {
                    identity: grandchild.clone(),
                    task_name: "grandchild".into(),
                    display_task_name: None,
                    parent_id: child.id.clone(),
                    session_path: manager.team_directory.join("grandchild.jsonl"),
                    status: DelegatedAgentStatus::Running,
                    command_tx,
                    shutdown: grandchild_shutdown.clone(),
                    interrupt_requested: false,
                    pending_messages: VecDeque::new(),
                    reserved_messages: QueueUsage::default(),
                    queued_follow_ups: QueueUsage::default(),
                    mailbox: VecDeque::new(),
                    mailbox_delivery: None,
                    resource_owner: None,
                    extension_policy: None,
                    extension_principal: None,
                    extension_profile: None,
                    extension_idempotency_key: None,
                    extension_fingerprint: None,
                    created_at_ms: 1,
                    started_at_ms: Some(1),
                    completed_at_ms: None,
                    turn_count: 0,
                    usage: Usage::default(),
                    cost_microdollars: None,
                    deadline_at_ms: None,
                },
            );
            state.total_agents += 1;
        }

        manager.prepare_owning_run(&child).unwrap();

        let state = manager.state.lock().unwrap();
        assert!(grandchild_shutdown.is_cancelled());
        assert!(!state.records.contains_key(&grandchild.id));
        let child_record = &state.records[&child.id];
        assert_eq!(child_record.mailbox.len(), 3);
        assert_eq!(
            child_record
                .mailbox
                .iter()
                .map(|message| message.message.as_str())
                .collect::<Vec<_>>(),
            [
                "leased message from the prior owning run",
                "stale automatic status",
                "unleased durable message"
            ]
        );
        assert!(child_record.mailbox.front().unwrap().leased);
        assert!(child_record.mailbox_delivery.is_some());
    }

    #[tokio::test]
    async fn prompt_failure_after_durable_task_append_is_not_retried() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _child_commands) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        let session_path = directory.path().join("prompt-classification.jsonl");
        let session_file = secure_fs::create_regular_file_for_append(&session_path).unwrap();
        let session = Session::create_with_file(&session_path, session_file).unwrap();
        let mut agent = manager.build_child_agent(session, &child, None).unwrap();
        {
            let mut state = manager.state.lock().unwrap();
            state.records.get_mut(&child.id).unwrap().session_path = session_path.clone();
            state.persistence_error = Some("forced owning-run preparation failure".into());
        }
        let (_command_tx, mut commands) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let shutdown = crate::CancellationToken::default();

        let execution = manager
            .execute_child_run(
                &mut agent,
                "task accepted before startup failed".into(),
                ChildRunContext {
                    identity: &child,
                    commands: &mut commands,
                    shutdown: &shutdown,
                    extension_policy: None,
                    deadline: None,
                },
            )
            .await;

        assert!(execution.task_delivered);
        match execution.outcome {
            WorkerOutcome::Failed(error) => {
                assert!(error
                    .contains("delegated run could not start after the task was durably accepted"))
            }
            _ => panic!("expected startup failure"),
        }
        let snapshot = Session::open_read_only(&session_path).unwrap();
        assert_eq!(
            snapshot
                .entries()
                .iter()
                .filter(|entry| matches!(
                    &entry.value,
                    crate::session::EntryValue::Message(ygg_ai::Message::User(message))
                        if message.content.len() == 1
                            && matches!(
                                &message.content[0],
                                ygg_ai::UserPart::Text(text)
                                    if text == "task accepted before startup failed"
                            )
                ))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_failure_inspection_stays_bound_to_the_original_session_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _child_commands) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        let session_path = directory.path().join("prompt-replacement.jsonl");
        let session_file = secure_fs::create_regular_file_for_append(&session_path).unwrap();
        let session = Session::create_with_file(&session_path, session_file).unwrap();
        let mut agent = manager.build_child_agent(session, &child, None).unwrap();
        {
            let mut state = manager.state.lock().unwrap();
            state.records.get_mut(&child.id).unwrap().session_path = session_path.clone();
            state.persistence_error = Some("forced owning-run preparation failure".into());
        }

        let original_path = session_path.with_extension("jsonl.original");
        std::fs::rename(&session_path, &original_path).unwrap();
        drop(Session::create(&session_path).unwrap());

        let (_command_tx, mut commands) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let shutdown = crate::CancellationToken::default();
        let execution = manager
            .execute_child_run(
                &mut agent,
                "task persisted through the original descriptor".into(),
                ChildRunContext {
                    identity: &child,
                    commands: &mut commands,
                    shutdown: &shutdown,
                    extension_policy: None,
                    deadline: None,
                },
            )
            .await;

        assert!(execution.task_delivered);
        assert!(matches!(execution.outcome, WorkerOutcome::Failed(_)));
        let original = Session::open_read_only(&original_path).unwrap();
        assert!(original.entries().iter().any(|entry| matches!(
            &entry.value,
            crate::session::EntryValue::Message(ygg_ai::Message::User(message))
                if message.content.len() == 1
                    && matches!(
                        &message.content[0],
                        ygg_ai::UserPart::Text(text)
                            if text == "task persisted through the original descriptor"
                    )
        )));
        assert!(Session::open_read_only(&session_path)
            .unwrap()
            .entries()
            .is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_reopen_failure_retains_initial_and_follow_up_work() {
        use std::os::unix::fs::symlink;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let manager = writable_manager_with_workspace(directory.path(), &workspace);
        let root = manager.root_binding().identity;
        let spawned = manager
            .spawn(
                &root,
                SpawnRequest {
                    task_name: "reopen-failure".into(),
                    display_task_name: None,
                    message: "initial task must remain first".into(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap();
        let child_id = spawned["agent_id"].as_str().unwrap().to_owned();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let failed = {
                    let state = manager.state.lock().unwrap();
                    matches!(
                        &state.records[&child_id].status,
                        DelegatedAgentStatus::Failed { error }
                            if error.contains("task retained for retry")
                    )
                };
                if failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("initial child startup did not fail");

        let session_path = {
            manager.state.lock().unwrap().records[&child_id]
                .session_path
                .clone()
        };
        let saved_session = session_path.with_extension("jsonl.saved");
        std::fs::rename(&session_path, &saved_session).unwrap();
        let outside = directory.path().join("outside-session");
        std::fs::write(&outside, b"outside must not be opened\n").unwrap();
        symlink(&outside, &session_path).unwrap();
        std::fs::create_dir(&workspace).unwrap();

        manager
            .follow_up(
                &root,
                FollowUpRequest {
                    target: child_id.clone(),
                    message: "accepted follow-up must remain behind initial".into(),
                },
            )
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let failed = {
                    let state = manager.state.lock().unwrap();
                    matches!(
                        &state.records[&child_id].status,
                        DelegatedAgentStatus::Failed { error }
                            if error.contains("task retained for retry")
                    ) && state.records[&child_id].queued_follow_ups.messages == 1
                };
                if failed {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("descriptor-bound child reopen did not fail");

        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"outside must not be opened\n"
        );
        assert!(std::fs::symlink_metadata(&session_path)
            .unwrap()
            .file_type()
            .is_symlink());
        let state = manager.state.lock().unwrap();
        let record = &state.records[&child_id];
        assert_eq!(record.queued_follow_ups.messages, 1);
        assert!(matches!(
            &record.status,
            DelegatedAgentStatus::Failed { error }
                if error.contains("task retained for retry")
        ));
        drop(state);

        manager.request_shutdown_descendants(ROOT_AGENT_ID);
        std::fs::remove_file(&session_path).unwrap();
        std::fs::rename(saved_session, session_path).unwrap();
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn failed_team_activation_removes_the_allocated_team_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let teams = root.join("teams");
        let root_session = root.join("root.jsonl");
        let result = DelegationManager::create_with_journal(
            DelegationConfig::new(&teams),
            test_template(&root),
            &root_session,
            |directory| {
                let path = directory.path().join("provenance.jsonl");
                drop(directory.create_regular_file_for_append(&path)?);
                Ok(ProvenanceJournal {
                    file: Mutex::new(directory.open_regular_file_for_read(&path)?),
                })
            },
        );

        let error = result
            .err()
            .expect("read-only journal must fail activation");
        assert!(error.to_string().contains("delegation persistence failed"));
        assert!(teams.exists());
        assert_eq!(std::fs::read_dir(teams).unwrap().count(), 0);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn failed_team_activation_does_not_remove_a_replacement_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let teams = root.join("teams");
        let original = teams.join("original-team");
        let mut replacement_marker = None;
        let result = DelegationManager::create_with_journal(
            DelegationConfig::new(&teams),
            test_template(&root),
            &root.join("root.jsonl"),
            |directory| {
                std::fs::rename(directory.path(), &original).unwrap();
                secure_fs::create_private_directory_all(directory.path()).unwrap();
                let marker = directory.path().join("replacement-marker");
                std::fs::write(&marker, b"replacement").unwrap();
                replacement_marker = Some(marker);
                Err(DelegationError::InvalidConfig(
                    "forced activation failure".into(),
                ))
            },
        );

        let error = result.err().expect("activation must fail");
        assert!(matches!(error, DelegationError::ActivationRollback { .. }));
        assert!(original.exists());
        assert_eq!(
            std::fs::read(replacement_marker.unwrap()).unwrap(),
            b"replacement"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn child_session_creation_rejects_a_replaced_team_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let teams = root.join("teams");
        let manager = DelegationManager::create(
            DelegationConfig::new(&teams),
            test_template(&root),
            &root.join("root.jsonl"),
        )
        .unwrap();
        let team_directory = manager.team_directory.clone();
        let original = teams.join("original-team");
        std::fs::rename(&team_directory, &original).unwrap();
        secure_fs::create_private_directory_all(&team_directory).unwrap();
        let marker = team_directory.join("replacement-marker");
        std::fs::write(&marker, b"replacement").unwrap();

        let error = manager
            .spawn(
                &root_identity(),
                SpawnRequest {
                    task_name: "child".into(),
                    display_task_name: None,
                    message: "do work".into(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap_err();

        assert!(error.contains("changed"), "unexpected error: {error}");
        assert_eq!(std::fs::read(&marker).unwrap(), b"replacement");
        assert!(!team_directory.join("0001-child.jsonl").exists());
        assert!(original.join("provenance.jsonl").exists());
    }

    fn root_identity() -> AgentIdentity {
        AgentIdentity {
            id: ROOT_AGENT_ID.into(),
            path: ROOT_AGENT_PATH.into(),
            depth: 0,
        }
    }

    fn insert_test_record(
        manager: &DelegationManager,
        status: DelegatedAgentStatus,
    ) -> (AgentIdentity, mpsc::Receiver<WorkerCommand>) {
        let identity = AgentIdentity {
            id: "agent-1".into(),
            path: "/root/child".into(),
            depth: 1,
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let mut state = manager.state.lock().unwrap();
        state.records.insert(
            identity.id.clone(),
            AgentRecord {
                identity: identity.clone(),
                task_name: "child".into(),
                display_task_name: None,
                parent_id: ROOT_AGENT_ID.into(),
                session_path: manager.team_directory.join("child.jsonl"),
                status,
                command_tx,
                shutdown: crate::CancellationToken::default(),
                interrupt_requested: false,
                pending_messages: VecDeque::new(),
                reserved_messages: QueueUsage::default(),
                queued_follow_ups: QueueUsage::default(),
                mailbox: VecDeque::new(),
                mailbox_delivery: None,
                resource_owner: None,
                extension_policy: None,
                extension_principal: None,
                extension_profile: None,
                extension_idempotency_key: None,
                extension_fingerprint: None,
                created_at_ms: 1,
                started_at_ms: Some(1),
                completed_at_ms: None,
                turn_count: 0,
                usage: Usage::default(),
                cost_microdollars: None,
                deadline_at_ms: None,
            },
        );
        state.total_agents += 1;
        drop(state);
        (identity, command_rx)
    }

    #[test]
    fn task_names_and_descendant_paths_are_strict() {
        assert!(validate_task_name("review_2").is_ok());
        assert!(validate_task_name("Review").is_err());
        assert!(validate_task_name("../escape").is_err());
        assert!(is_descendant_path("/root/a/b", "/root/a"));
        assert!(!is_descendant_path("/root/ab", "/root/a"));
    }

    #[test]
    fn config_requires_real_bounded_child_capacity() {
        let mut config = DelegationConfig::new("ignored");
        config.limits.max_concurrent_agents = 1;
        assert!(config.validate().is_err());
        config.limits.max_concurrent_agents = 4;
        config.limits.max_total_agents = 3;
        assert!(config.validate().is_err());
    }

    #[test]
    fn extension_child_policy_installs_only_detached_read_only_tools_and_lowers_parent_limits() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = writable_manager_with_core_tools(directory.path());
        {
            let manager_mut = Arc::get_mut(&mut manager).expect("manager remains unique");
            manager_mut.template.max_turns = Some(2);
            manager_mut
                .template
                .runtime
                .get_mut()
                .unwrap()
                .max_session_cost_microdollars = Some(50);
        }
        let identity = AgentIdentity {
            id: "agent-policy".into(),
            path: "/root/policy".into(),
            depth: 1,
        };
        let mut policy = test_extension_policy();
        policy.max_turns = 8;
        policy.max_cost_microdollars = 200;
        let allowed = policy.tools.iter().cloned().collect::<BTreeSet<_>>();
        let (_, effective) = manager
            .template
            .extensions
            .scoped_tool_snapshot(&allowed)
            .unwrap();
        policy.tools = effective;
        policy.max_turns = policy.max_turns.min(manager.template.max_turns.unwrap());
        policy.max_cost_microdollars = 50;
        let session = Session::create(directory.path().join("policy-child.jsonl")).unwrap();
        let child = manager
            .build_child_agent(session, &identity, Some(&policy))
            .unwrap();
        assert_eq!(
            child.registered_tool_names(),
            vec!["read".to_owned(), "search".to_owned()]
        );
        assert!(child
            .registered_tool_names()
            .iter()
            .all(|name| !COLLABORATION_TOOL_NAMES.contains(&name.as_str())));
        assert!(manager
            .template
            .extensions
            .tool_definitions()
            .iter()
            .any(|tool| tool.name == "write"));
    }

    #[test]
    fn extension_child_rejects_unpriced_model_before_session_creation() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = writable_manager_with_core_tools(directory.path());
        let manager_mut = Arc::get_mut(&mut manager).unwrap();
        Arc::make_mut(&mut manager_mut.template.model.spec).pricing = None;
        let binding = manager.root_binding();
        let service = binding
            .extension_service("extension-policy", "parent-session", "root-owner")
            .unwrap();

        let error = service
            .spawn(
                "root-owner",
                test_extension_spawn("unpriced", None, None, "must not run", "unpriced-key"),
            )
            .unwrap_err();
        assert!(error.contains("trusted model pricing"), "{error}");
        assert!(manager.state.lock().unwrap().records.is_empty());
    }

    #[tokio::test]
    async fn extension_service_enforces_concurrency_depth_deadline_and_list_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let team = directory.path().join("team-extension-policy");
        std::fs::create_dir(&team).unwrap();
        let manager = writable_manager_with_core_tools(&team);
        let binding = manager.root_binding();
        let service = binding
            .extension_service("extension-policy", "parent-session", "root-owner")
            .unwrap();
        let first = service
            .spawn(
                "root-owner",
                test_extension_spawn(
                    "first",
                    Some("review"),
                    Some(&"f".repeat(64)),
                    "first bounded task",
                    "first-key",
                ),
            )
            .unwrap();
        let second = service
            .spawn(
                "root-owner",
                test_extension_spawn("second", None, None, "second bounded task", "second-key"),
            )
            .unwrap();
        let error = service
            .spawn(
                "root-owner",
                test_extension_spawn("third", None, None, "third bounded task", "third-key"),
            )
            .unwrap_err();
        assert!(error.contains("concurrency limit"), "{error}");

        let first_id = first["agent_id"].as_str().unwrap();
        {
            let mut state = manager.state.lock().unwrap();
            let record = state.records.get_mut(first_id).unwrap();
            record.turn_count = 2;
            record.usage = Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Usage::default()
            };
            record.cost_microdollars = Some(7);
        }
        let listed = service.list("root-owner").unwrap();
        let record = listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["agent_id"] == first_id)
            .unwrap();
        assert_eq!(record["task_name"], "first");
        assert_eq!(record["policy"]["tools"], json!(["read", "search"]));
        assert_eq!(record["turn_count"], 2);
        assert_eq!(record["usage"]["total_tokens"], 15);
        assert_eq!(record["cost_microdollars"], 7);
        assert_eq!(record["profile"], "review");
        assert_eq!(record["idempotency_key"], "first-key");
        assert_eq!(record["fingerprint"], "f".repeat(64));
        assert!(record["created_at_ms"].as_u64().is_some());
        assert!(record["deadline_at_ms"].as_u64().is_some());
        assert_eq!(record["provenance"]["principal"], "extension-policy");
        assert_eq!(record["provenance"]["resource_owner"], "root-owner");
        let reference = record["session"].as_str().unwrap();
        let mut inspection = binding
            .open_session_reference("extension-policy", reference)
            .unwrap()
            .unwrap();
        assert!(inspection
            .append(crate::EntryValue::Config {
                model: None,
                reasoning: None,
                reasoning_mode: None,
            })
            .is_err());
        assert!(binding
            .open_session_reference("another-extension", reference)
            .unwrap()
            .is_none());
        assert!(binding
            .open_session_reference(
                "extension-policy",
                &format!("agent-session:{}", "0".repeat(64)),
            )
            .unwrap()
            .is_none());

        let journal =
            std::fs::read_to_string(manager.team_directory.join("provenance.jsonl")).unwrap();
        let persisted = journal
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|event| event["event"] == "agent_spawned" && event["agent_id"] == first_id)
            .unwrap();
        assert_eq!(persisted["extension_parent_session_id"], "parent-session");
        assert_eq!(persisted["extension_principal"], "extension-policy");
        assert_eq!(persisted["extension_resource_owner"], "root-owner");
        assert_eq!(persisted["extension_profile"], "review");
        assert_eq!(persisted["extension_idempotency_key"], "first-key");
        assert_eq!(persisted["extension_fingerprint"], "f".repeat(64));
        assert!(persisted["session_reference"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("agent-session:")));
        assert!(persisted.get("task").is_none());
        assert!(persisted.get("session").is_none());
        assert!(!journal.contains("first bounded task"));

        let nested_owner = AgentIdentity {
            id: first_id.into(),
            path: first["agent_path"].as_str().unwrap().into(),
            depth: 1,
        };
        let nested_error = manager
            .spawn(
                &nested_owner,
                SpawnRequest {
                    task_name: "nested".into(),
                    display_task_name: None,
                    message: "must not create a session".into(),
                    extension_policy: Some(test_extension_policy()),
                    extension_provenance: Some(ExtensionSpawnProvenance {
                        parent_session_id: "parent-session".into(),
                        principal: "extension-policy".into(),
                        resource_owner: "child-owner".into(),
                        profile: None,
                        idempotency_key: "nested-key".into(),
                        fingerprint: None,
                    }),
                },
            )
            .unwrap_err();
        assert!(nested_error.contains("depth limit"), "{nested_error}");
        assert_eq!(second["policy"]["max_concurrent_children"], 2);
    }

    #[tokio::test]
    async fn elapsed_extension_deadline_settles_before_provider_or_tool_execution() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (identity, mut commands) = insert_test_record(&manager, DelegatedAgentStatus::Pending);
        let session = Session::create(directory.path().join("deadline-child.jsonl")).unwrap();
        let mut agent = manager.build_child_agent(session, &identity, None).unwrap();
        let shutdown = crate::CancellationToken::default();
        let policy = test_extension_policy();
        let outcome = manager
            .execute_child_run(
                &mut agent,
                "must not execute".into(),
                ChildRunContext {
                    identity: &identity,
                    commands: &mut commands,
                    shutdown: &shutdown,
                    extension_policy: Some(&policy),
                    deadline: Some(tokio::time::Instant::now()),
                },
            )
            .await;
        assert!(matches!(outcome.outcome, WorkerOutcome::TimedOut));
        assert!(agent.session().entries().is_empty());
    }

    #[test]
    fn extension_output_bound_is_exact_and_utf8_safe() {
        let output = bounded_text_to(&"é".repeat(10_000), 513);
        assert!(output.len() <= 513);
        assert!(output.is_char_boundary(output.len()));
        assert!(output.ends_with("...[truncated]"));
    }

    #[tokio::test]
    async fn extension_services_are_idempotent_and_isolated_by_principal_and_owner() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let binding = manager.root_binding();
        let service_a = binding
            .extension_service("extension-a", "parent-session", "root-owner")
            .unwrap();
        let service_b = binding
            .extension_service("extension-b", "parent-session", "root-owner")
            .unwrap();
        let (identity, mut commands) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        {
            let mut state = service_a.state.lock().unwrap();
            let owner = state.owners.entry("root-owner".into()).or_default();
            owner.owned_agents.insert(identity.id.clone());
            owner.idempotent_spawns.insert(
                "spawn-1".into(),
                IdempotentExtensionSpawn {
                    task_name: "research".into(),
                    profile: None,
                    fingerprint: None,
                    message_sha256: format!("{:x}", Sha256::digest(b"find it")),
                    policy: test_extension_policy(),
                    result: json!({"agent_id":identity.id.clone(),"status":"pending"}),
                },
            );
        }

        let cached = service_a
            .spawn(
                "root-owner",
                test_extension_spawn("research", None, None, "find it", "spawn-1"),
            )
            .unwrap();
        assert_eq!(cached["agent_id"], "agent-1");
        assert!(service_a
            .spawn(
                "root-owner",
                test_extension_spawn("research", None, None, "different", "spawn-1"),
            )
            .unwrap_err()
            .contains("different input"));

        assert!(service_b
            .send_message("root-owner", &identity.id, "cross-principal".into())
            .await
            .unwrap_err()
            .contains("no child sessions"));
        assert!(service_a
            .list("different-owner")
            .unwrap_err()
            .contains("not an active"));

        service_a
            .send_message("root-owner", &identity.id, "owned".into())
            .await
            .unwrap();
        let command = commands.recv().await.unwrap();
        assert!(matches!(command.kind, WorkerCommandKind::Message(_)));
        service_a.shutdown_owned();
        assert!(manager.state.lock().unwrap().records[&identity.id]
            .shutdown
            .is_cancelled());
    }

    #[test]
    fn automatic_mailbox_eviction_is_oldest_first_and_stays_bounded() {
        let mut mailbox = VecDeque::new();
        for index in 0..(MAX_MAILBOX_MESSAGES + 5) {
            push_mailbox_bounded(
                &mut mailbox,
                MailboxMessage {
                    kind: "task_status",
                    from: ROOT_AGENT_ID.into(),
                    task_name: None,
                    message: index.to_string(),
                    evictable: true,
                    continued: false,
                    leased: false,
                },
            );
        }

        assert_eq!(mailbox.len(), MAX_MAILBOX_MESSAGES);
        assert_eq!(mailbox.front().unwrap().message, "5");
        assert!(mailbox.iter().map(mailbox_message_bytes).sum::<usize>() <= MAX_MAILBOX_BYTES);
    }

    #[test]
    fn automatic_mailbox_notifications_never_evict_direct_messages() {
        let mut mailbox = VecDeque::new();
        push_mailbox_bounded(
            &mut mailbox,
            MailboxMessage {
                kind: "message",
                from: "agent-a".into(),
                task_name: None,
                message: "durable".into(),
                evictable: false,
                continued: false,
                leased: false,
            },
        );
        for index in 0..MAX_MAILBOX_MESSAGES {
            push_mailbox_bounded(
                &mut mailbox,
                MailboxMessage {
                    kind: "task_status",
                    from: "agent-b".into(),
                    task_name: None,
                    message: index.to_string(),
                    evictable: true,
                    continued: false,
                    leased: false,
                },
            );
        }

        assert_eq!(mailbox.len(), MAX_MAILBOX_MESSAGES);
        assert!(mailbox
            .iter()
            .any(|message| message.message == "durable" && !message.evictable));
        assert_eq!(
            mailbox.back().unwrap().message,
            (MAX_MAILBOX_MESSAGES - 1).to_string()
        );
    }

    #[test]
    fn direct_mailbox_messages_displace_only_automatic_notifications() {
        let mut mailbox = VecDeque::new();
        for index in 0..MAX_MAILBOX_MESSAGES {
            push_mailbox_bounded(
                &mut mailbox,
                MailboxMessage {
                    kind: "task_status",
                    from: "agent-b".into(),
                    task_name: None,
                    message: index.to_string(),
                    evictable: true,
                    continued: false,
                    leased: false,
                },
            );
        }
        let direct = MailboxMessage {
            kind: "message",
            from: "agent-a".into(),
            task_name: None,
            message: "durable".into(),
            evictable: false,
            continued: false,
            leased: false,
        };

        assert!(mailbox_can_accept_after_evicting_automatic(
            &mailbox, &direct
        ));
        push_mailbox_bounded(&mut mailbox, direct);

        assert_eq!(mailbox.len(), MAX_MAILBOX_MESSAGES);
        assert_eq!(mailbox.front().unwrap().message, "1");
        assert_eq!(mailbox.back().unwrap().message, "durable");
        assert!(!mailbox.back().unwrap().evictable);
    }

    #[test]
    fn mailbox_pages_commit_only_after_acknowledgement_and_preserve_utf8() {
        const OUTPUT_LIMIT: usize = 512;
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let owner = root_identity();
        let original = "αβγ delegated evidence ".repeat(180);
        {
            let mut state = manager.state.lock().unwrap();
            state.root_mailbox.push_back(MailboxMessage {
                kind: "message",
                from: "agent-a".into(),
                task_name: None,
                message: original.clone(),
                evictable: false,
                continued: false,
                leased: false,
            });
        }

        let first = manager
            .take_wait_result(&owner, OUTPUT_LIMIT)
            .unwrap()
            .unwrap();
        let first_delivery = first.delivery_id.unwrap();
        assert!(serde_json::to_string(&first.value).unwrap().len() <= OUTPUT_LIMIT);
        {
            let state = manager.state.lock().unwrap();
            assert_eq!(state.root_mailbox.len(), 1);
            assert!(state.root_mailbox.front().unwrap().leased);
        }
        manager.resolve_mailbox_delivery(ROOT_AGENT_ID, first_delivery, false);
        {
            let state = manager.state.lock().unwrap();
            assert_eq!(state.root_mailbox.front().unwrap().message, original);
            assert!(!state.root_mailbox.front().unwrap().leased);
        }

        let mut reconstructed = String::new();
        let mut page_index = 0usize;
        loop {
            let page = manager
                .take_wait_result(&owner, OUTPUT_LIMIT)
                .unwrap()
                .unwrap();
            let Some(delivery_id) = page.delivery_id else {
                break;
            };
            let encoded = serde_json::to_string(&page.value).unwrap();
            assert!(encoded.len() <= OUTPUT_LIMIT, "{}", encoded.len());
            let messages = page.value["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 1);
            let chunk = messages[0]["message"].as_str().unwrap();
            assert!(!chunk.is_empty());
            if page_index == 0 {
                assert_ne!(messages[0]["continued"], true);
            } else {
                assert_eq!(messages[0]["continued"], true);
            }
            reconstructed.push_str(chunk);
            manager.resolve_mailbox_delivery(ROOT_AGENT_ID, delivery_id, true);
            page_index += 1;
            if !page.value["more"].as_bool().unwrap() {
                break;
            }
        }

        assert!(page_index > 1);
        assert_eq!(reconstructed, original);
        assert!(manager.state.lock().unwrap().root_mailbox.is_empty());
    }

    #[test]
    fn mailbox_delivery_ids_are_bound_to_the_owning_agent() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        {
            let mut state = manager.state.lock().unwrap();
            state.root_mailbox.push_back(MailboxMessage {
                kind: "message",
                from: "root-peer".into(),
                task_name: None,
                message: "root-only".into(),
                evictable: false,
                continued: false,
                leased: false,
            });
            state
                .records
                .get_mut(&child.id)
                .unwrap()
                .mailbox
                .push_back(MailboxMessage {
                    kind: "message",
                    from: ROOT_AGENT_ID.into(),
                    task_name: None,
                    message: "child-only".into(),
                    evictable: false,
                    continued: false,
                    leased: false,
                });
        }

        let page = manager.take_wait_result(&child, 512).unwrap().unwrap();
        let delivery_id = page.delivery_id.unwrap();
        assert_eq!(page.value["messages"][0]["message"], "child-only");

        manager.resolve_mailbox_delivery(ROOT_AGENT_ID, delivery_id, true);
        {
            let state = manager.state.lock().unwrap();
            assert_eq!(state.root_mailbox.front().unwrap().message, "root-only");
            assert!(state.records[&child.id].mailbox.front().unwrap().leased);
        }

        manager.resolve_mailbox_delivery(&child.id, delivery_id, true);
        let state = manager.state.lock().unwrap();
        assert_eq!(state.root_mailbox.front().unwrap().message, "root-only");
        assert!(state.records[&child.id].mailbox.is_empty());
    }

    #[tokio::test]
    async fn oversized_durable_tasks_messages_and_followups_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let owner = root_identity();
        let oversized = "é".repeat(MAX_PROVENANCE_TEXT_BYTES / 2 + 1);

        let error = manager
            .spawn(
                &owner,
                SpawnRequest {
                    task_name: "oversized".into(),
                    display_task_name: None,
                    message: oversized.clone(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap_err();
        assert!(error.contains("spawn task exceeds"), "{error}");
        assert!(manager.state.lock().unwrap().records.is_empty());

        let error = manager
            .spawn(
                &owner,
                SpawnRequest {
                    task_name: oversized.clone(),
                    display_task_name: None,
                    message: "work".into(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap_err();
        assert!(error.contains("task_name must contain"), "{error}");
        assert!(manager.state.lock().unwrap().records.is_empty());

        let (_identity, command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        let error = manager
            .send_message(&owner, "/root/child", oversized.clone())
            .await
            .unwrap_err();
        assert!(error.contains("message exceeds"), "{error}");
        let error = manager
            .follow_up(
                &owner,
                FollowUpRequest {
                    target: "/root/child".into(),
                    message: oversized,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("follow-up exceeds"), "{error}");
        assert_eq!(command_rx.len(), 0);
        let state = manager.state.lock().unwrap();
        assert_eq!(
            state.records["agent-1"].reserved_messages,
            QueueUsage::default()
        );
        assert_eq!(
            state.records["agent-1"].queued_follow_ups,
            QueueUsage::default()
        );
    }

    #[test]
    fn undelivered_prompt_messages_are_restored_in_fifo_order() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        {
            let mut state = manager.state.lock().unwrap();
            let pending = &mut state.records.get_mut(&child.id).unwrap().pending_messages;
            pending.push_back(DirectedMessage {
                from: "older-a".into(),
                message: "first".into(),
            });
            pending.push_back(DirectedMessage {
                from: "older-b".into(),
                message: "second".into(),
            });
        }
        let leased = manager.take_pending_messages(&child.id);
        {
            let mut state = manager.state.lock().unwrap();
            let record = state.records.get_mut(&child.id).unwrap();
            assert_eq!(record.reserved_messages.messages, 2);
            record.pending_messages.push_back(DirectedMessage {
                from: "newer".into(),
                message: "third".into(),
            });
        }

        manager.restore_pending_messages(&child.id, leased);

        let state = manager.state.lock().unwrap();
        let record = &state.records[&child.id];
        let messages = record
            .pending_messages
            .iter()
            .map(|message| message.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(messages, vec!["first", "second", "third"]);
        assert_eq!(record.reserved_messages, QueueUsage::default());
    }

    #[test]
    fn undelivered_initial_task_returns_to_the_fifo_head_once() {
        let mut queued_tasks = VecDeque::from([
            QueuedTask::FollowUp(QueuedFollowUp {
                from: ROOT_AGENT_ID.into(),
                message: "older follow-up".into(),
            }),
            QueuedTask::FollowUp(QueuedFollowUp {
                from: ROOT_AGENT_ID.into(),
                message: "newer follow-up".into(),
            }),
        ]);

        assert!(restore_undelivered_task(
            &mut queued_tasks,
            QueuedTask::Initial("initial task".into()),
            false,
            &WorkerOutcome::Failed("session append failed".into()),
        ));
        let labels = queued_tasks
            .iter()
            .map(|task| match task {
                QueuedTask::Initial(task) => task.as_str(),
                QueuedTask::FollowUp(follow_up) => follow_up.message.as_str(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["initial task", "older follow-up", "newer follow-up"]
        );

        let delivered = queued_tasks.pop_front().unwrap();
        assert!(!restore_undelivered_task(
            &mut queued_tasks,
            delivered,
            true,
            &WorkerOutcome::Completed(String::new()),
        ));
        let labels = queued_tasks
            .iter()
            .map(|task| match task {
                QueuedTask::Initial(task) => task.as_str(),
                QueuedTask::FollowUp(follow_up) => follow_up.message.as_str(),
            })
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["older follow-up", "newer follow-up"]);
    }

    #[test]
    fn prompt_message_reservations_hold_queue_capacity_until_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (child, _command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        {
            let mut state = manager.state.lock().unwrap();
            let pending = &mut state.records.get_mut(&child.id).unwrap().pending_messages;
            for index in 0..MAX_PENDING_MESSAGES {
                pending.push_back(DirectedMessage {
                    from: ROOT_AGENT_ID.into(),
                    message: format!("message-{index}"),
                });
            }
        }

        let leased = manager.take_pending_messages(&child.id);
        let candidate = DirectedMessage {
            from: ROOT_AGENT_ID.into(),
            message: "overflow".into(),
        };
        {
            let state = manager.state.lock().unwrap();
            let record = &state.records[&child.id];
            assert!(record.pending_messages.is_empty());
            assert_eq!(record.reserved_messages.messages, MAX_PENDING_MESSAGES);
            assert!(!record_can_accept_pending_message(record, &candidate));
        }

        manager.release_prompt_message_reservations(&child.id, &leased);
        let state = manager.state.lock().unwrap();
        assert_eq!(
            state.records[&child.id].reserved_messages,
            QueueUsage::default()
        );
    }

    #[tokio::test]
    async fn pending_messages_reject_overflow_without_evicting_durable_work() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (_identity, _command_rx) = insert_test_record(
            &manager,
            DelegatedAgentStatus::Completed {
                output: "done".into(),
            },
        );
        let owner = root_identity();

        for index in 0..MAX_PENDING_MESSAGES {
            manager
                .send_message(&owner, "/root/child", format!("pending-message-{index}"))
                .await
                .unwrap();
        }
        let error = manager
            .send_message(&owner, "/root/child", "overflow".into())
            .await
            .unwrap_err();
        assert!(error.contains("pending-message queue is full"), "{error}");

        let state = manager.state.lock().unwrap();
        let pending = &state.records["agent-1"].pending_messages;
        assert_eq!(pending.len(), MAX_PENDING_MESSAGES);
        assert_eq!(pending.front().unwrap().message, "pending-message-0");
        assert_eq!(
            pending.back().unwrap().message,
            format!("pending-message-{}", MAX_PENDING_MESSAGES - 1)
        );
    }

    #[tokio::test]
    async fn follow_up_queue_is_bounded_and_interrupt_drain_preserves_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (_identity, mut command_rx) = insert_test_record(
            &manager,
            DelegatedAgentStatus::Completed {
                output: "done".into(),
            },
        );
        let owner = root_identity();

        for index in 0..MAX_QUEUED_FOLLOW_UPS {
            manager
                .follow_up(
                    &owner,
                    FollowUpRequest {
                        target: "/root/child".into(),
                        message: format!("follow-up-{index}"),
                    },
                )
                .await
                .unwrap();
        }
        let error = manager
            .follow_up(
                &owner,
                FollowUpRequest {
                    target: "/root/child".into(),
                    message: "overflow".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("follow-up queue is full"), "{error}");
        assert_eq!(
            manager.state.lock().unwrap().records["agent-1"]
                .queued_follow_ups
                .messages,
            MAX_QUEUED_FOLLOW_UPS
        );

        let mut queued_tasks = VecDeque::new();
        assert!(!manager.drain_interrupted_commands("agent-1", &mut command_rx, &mut queued_tasks,));
        assert_eq!(queued_tasks.len(), MAX_QUEUED_FOLLOW_UPS);
        assert!(queued_tasks
            .iter()
            .all(|task| matches!(task, QueuedTask::FollowUp(_))));
        assert_eq!(
            manager.state.lock().unwrap().records["agent-1"]
                .queued_follow_ups
                .messages,
            MAX_QUEUED_FOLLOW_UPS
        );
    }

    #[test]
    fn waiter_registration_is_bounded_and_released_by_raii() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let owner = root_identity();
        let limit = manager.config.limits.max_total_agents;
        let guards = (0..limit)
            .map(|_| manager.register_waiter(&owner).unwrap())
            .collect::<Vec<_>>();

        let error = manager
            .register_waiter(&owner)
            .err()
            .expect("waiter overflow must be rejected");
        assert!(error.contains("waiter limit reached"), "{error}");
        assert_eq!(manager.state.lock().unwrap().active_waiters, limit);
        drop(guards);
        assert_eq!(manager.state.lock().unwrap().active_waiters, 0);
    }

    #[tokio::test]
    async fn child_actions_require_the_matching_running_owner() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (identity, _command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Running);
        assert!(manager.list_value_for(&identity).is_ok());

        let mut spoofed = identity.clone();
        spoofed.path = "/root/not-child".into();
        let error = manager.list_value_for(&spoofed).unwrap_err();
        assert!(error.contains("identity does not match"), "{error}");

        manager
            .state
            .lock()
            .unwrap()
            .records
            .get_mut("agent-1")
            .unwrap()
            .status = DelegatedAgentStatus::Completed {
            output: "done".into(),
        };
        let error = manager
            .send_message(&identity, ROOT_AGENT_ID, "stale child".into())
            .await
            .unwrap_err();
        assert!(error.contains("owner is not running"), "{error}");

        let mut state = manager.state.lock().unwrap();
        let record = state.records.get_mut("agent-1").unwrap();
        record.status = DelegatedAgentStatus::Running;
        record.shutdown.cancel();
        drop(state);
        let error = manager.list_value_for(&identity).unwrap_err();
        assert!(error.contains("owner is not running"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupt_and_follow_up_publication_are_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (_identity, mut command_rx) =
            insert_test_record(&manager, DelegatedAgentStatus::Running);
        let owner = root_identity();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let follow_manager = Arc::clone(&manager);
        let follow_owner = owner.clone();
        let follow_barrier = Arc::clone(&barrier);
        let follow = tokio::spawn(async move {
            follow_barrier.wait().await;
            follow_manager
                .follow_up(
                    &follow_owner,
                    FollowUpRequest {
                        target: "/root/child".into(),
                        message: "race".into(),
                    },
                )
                .await
        });
        let interrupt_manager = Arc::clone(&manager);
        let interrupt_owner = owner.clone();
        let interrupt_barrier = Arc::clone(&barrier);
        let interrupt = tokio::spawn(async move {
            interrupt_barrier.wait().await;
            interrupt_manager
                .interrupt(&interrupt_owner, "/root/child")
                .await
        });
        barrier.wait().await;
        let follow_result = follow.await.unwrap();
        let interrupt_result = interrupt.await.unwrap().unwrap();

        assert_eq!(interrupt_result["interrupt_requested"], true);
        let accepted = match follow_result {
            Ok(_) => {
                assert_eq!(command_rx.len(), 1);
                true
            }
            Err(error) => {
                assert!(error.contains("being interrupted"), "{error}");
                assert_eq!(command_rx.len(), 0);
                false
            }
        };
        let mut queued_tasks = VecDeque::new();
        manager.drain_interrupted_commands("agent-1", &mut command_rx, &mut queued_tasks);
        assert_eq!(queued_tasks.len(), usize::from(accepted));
        assert_eq!(
            manager.state.lock().unwrap().records["agent-1"]
                .queued_follow_ups
                .messages,
            usize::from(accepted)
        );
    }

    #[test]
    fn child_uses_runtime_settings_updated_after_delegation_activation() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let binding = manager.root_binding();
        let compaction_model = manager.template.model.clone();
        let audio = ygg_ai::OutputModalities::TextAndAudio(ygg_ai::AudioOutputOptions {
            format: ygg_ai::AudioFormat::Wav,
            voice: ygg_ai::AudioVoice::Named("alloy".into()),
        });
        let mut settings = manager.template.runtime.read().unwrap().clone();
        settings.compaction_model = Some(compaction_model.clone());
        settings.auto_compaction_mode = AgentCompactionMode::Disabled;
        settings.auto_compaction_threshold = 0.7;
        settings.compaction_keep_recent_tokens = 777;
        settings.completion_policy = CompletionPolicy::TerminalGate;
        settings.output_modalities = audio.clone();
        settings.max_output_tokens = 777;
        settings.max_session_cost_microdollars = Some(42);
        settings.provider_retries_enabled = false;
        binding.update_runtime_settings(settings);

        let session = Session::create(directory.path().join("child.jsonl")).unwrap();
        let identity = AgentIdentity {
            id: "agent-1".into(),
            path: "/root/child".into(),
            depth: 1,
        };
        let child = manager.build_child_agent(session, &identity, None).unwrap();

        assert_eq!(
            child.compaction_model().unwrap().spec.id,
            compaction_model.spec.id
        );
        assert_eq!(child.compaction_mode(), AgentCompactionMode::Disabled);
        assert_eq!(child.compaction_token_policy(), (false, 0.7, 777));
        assert_eq!(child.completion_policy(), CompletionPolicy::TerminalGate);
        assert_eq!(child.output_modalities(), &audio);
        assert_eq!(child.max_output_tokens(), 777);
        let settings = manager.template.runtime.read().unwrap();
        assert_eq!(settings.max_session_cost_microdollars, Some(42));
        assert!(!settings.provider_retries_enabled);
    }

    #[tokio::test]
    async fn worker_start_failure_retains_accepted_work_for_explicit_retry() {
        let directory = tempfile::tempdir().unwrap();
        let manager = writable_manager(directory.path());
        let (identity, _command_rx) = insert_test_record(&manager, DelegatedAgentStatus::Pending);
        {
            let mut state = manager.state.lock().unwrap();
            let record = state.records.get_mut(&identity.id).unwrap();
            record.pending_messages.push_back(DirectedMessage {
                from: ROOT_AGENT_ID.into(),
                message: "queued".into(),
            });
            record.reserved_messages = QueueUsage {
                messages: 1,
                bytes: 6,
            };
            record.queued_follow_ups = QueueUsage {
                messages: 1,
                bytes: 8,
            };
        }

        manager.fail_worker_start(&identity.id, "could not build child".into());

        {
            let state = manager.state.lock().unwrap();
            let record = &state.records[&identity.id];
            assert!(matches!(record.status, DelegatedAgentStatus::Failed { .. }));
            assert!(!record.shutdown.is_cancelled());
            assert_eq!(record.pending_messages.len(), 1);
            assert_eq!(record.reserved_messages.messages, 1);
            assert_eq!(record.queued_follow_ups.messages, 1);
        }
        let delivered = manager
            .send_message(&root_identity(), &identity.id, "too late".into())
            .await
            .unwrap();
        assert_eq!(delivered["delivery"], "queued");
        assert_eq!(
            manager.state.lock().unwrap().records[&identity.id]
                .pending_messages
                .len(),
            2
        );
    }

    #[test]
    fn provenance_failure_rolls_back_spawn_and_fails_the_team_closed() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager_with_journal(read_only_journal(directory.path()), directory.path());
        let owner = AgentIdentity {
            id: ROOT_AGENT_ID.into(),
            path: ROOT_AGENT_PATH.into(),
            depth: 0,
        };

        let error = manager
            .spawn(
                &owner,
                SpawnRequest {
                    task_name: "child".into(),
                    display_task_name: None,
                    message: "must not launch".into(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap_err();
        assert!(error.contains("persist delegation provenance"), "{error}");
        let state = manager.state.lock().unwrap();
        assert!(state.records.is_empty());
        assert_eq!(state.total_agents, 1);
        assert!(state.persistence_error.is_some());
        assert!(state.shutting_down);
        drop(state);
        assert!(!directory.path().join("0001-child.jsonl").exists());

        let second_error = manager
            .spawn(
                &owner,
                SpawnRequest {
                    task_name: "second".into(),
                    display_task_name: None,
                    message: "still closed".into(),
                    extension_policy: None,
                    extension_provenance: None,
                },
            )
            .unwrap_err();
        assert!(second_error.contains("persistence is unavailable"));
    }

    #[tokio::test]
    async fn message_is_not_delivered_when_provenance_cannot_be_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let manager = manager_with_journal(read_only_journal(directory.path()), directory.path());
        let (command_tx, _command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        {
            let mut state = manager.state.lock().unwrap();
            state.records.insert(
                "agent-1".into(),
                AgentRecord {
                    identity: AgentIdentity {
                        id: "agent-1".into(),
                        path: "/root/child".into(),
                        depth: 1,
                    },
                    task_name: "child".into(),
                    display_task_name: None,
                    parent_id: ROOT_AGENT_ID.into(),
                    session_path: directory.path().join("child.jsonl"),
                    status: DelegatedAgentStatus::Completed {
                        output: "done".into(),
                    },
                    command_tx,
                    shutdown: crate::CancellationToken::default(),
                    interrupt_requested: false,
                    pending_messages: VecDeque::new(),
                    reserved_messages: QueueUsage::default(),
                    queued_follow_ups: QueueUsage::default(),
                    mailbox: VecDeque::new(),
                    mailbox_delivery: None,
                    resource_owner: None,
                    extension_policy: None,
                    extension_principal: None,
                    extension_profile: None,
                    extension_idempotency_key: None,
                    extension_fingerprint: None,
                    created_at_ms: 1,
                    started_at_ms: Some(1),
                    completed_at_ms: None,
                    turn_count: 0,
                    usage: Usage::default(),
                    cost_microdollars: None,
                    deadline_at_ms: None,
                },
            );
        }
        let owner = AgentIdentity {
            id: ROOT_AGENT_ID.into(),
            path: ROOT_AGENT_PATH.into(),
            depth: 0,
        };

        let error = manager
            .send_message(&owner, "/root/child", "not durable".into())
            .await
            .unwrap_err();
        assert!(error.contains("persist message provenance"), "{error}");
        let state = manager.state.lock().unwrap();
        assert!(state.persistence_error.is_some());
        assert!(state.records["agent-1"].pending_messages.is_empty());
    }

    #[test]
    fn bounded_text_preserves_utf8_boundaries() {
        let input = "é".repeat(MAX_PROVENANCE_TEXT_BYTES);
        let output = bounded_text(&input);
        assert!(output.ends_with("...[truncated]"));
        assert!(output.is_char_boundary(output.len()));
    }
}
