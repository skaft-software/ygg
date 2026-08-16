//! Bounded V2 collaboration runtime for delegated coding agents.
//!
//! The model capability only advertises that collaboration is useful. This
//! module owns the host-side semantics: isolated child sessions, lifecycle and
//! message routing, bounded concurrency/depth, cancellation, and durable
//! provenance.

use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};
use ygg_ai::{AssistantPart, ToolDef};

use crate::agent::{Agent, AgentCompactionMode, AgentConfig, AgentError, CompletionPolicy};
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
const COLLABORATION_TOOL_NAMES: [&str; 6] = [
    "spawn_agent",
    "followup_task",
    "send_message",
    "wait_agent",
    "list_agents",
    "interrupt_agent",
];

/// Returns whether this target build implements the advertised collaboration version.
pub fn delegation_runtime_supports(version: ygg_ai::AgentDelegation) -> bool {
    matches!(version, ygg_ai::AgentDelegation::V2) && cfg!(any(unix, windows))
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
    /// Maximum agents created over the lifetime of the team.
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

impl DelegationBinding {
    pub(crate) fn team_directory(&self) -> &Path {
        &self.manager.team_directory
    }

    pub(crate) fn system_instructions(&self) -> &str {
        &self.system_instructions
    }

    pub(crate) fn request_shutdown(&self) {
        self.manager.request_shutdown_descendants(&self.identity.id);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AgentIdentity {
    id: String,
    path: String,
    depth: usize,
}

#[derive(Clone)]
pub(crate) struct DelegationTemplate {
    pub(crate) client: ygg_ai::AiClient,
    pub(crate) model: ygg_ai::Model,
    pub(crate) base_system: String,
    pub(crate) sandbox: crate::SandboxConfig,
    pub(crate) extensions: ExtensionHost,
    pub(crate) max_turns: Option<u64>,
    pub(crate) reasoning: ygg_ai::ReasoningConfig,
    pub(crate) reasoning_mode: ygg_ai::ReasoningMode,
    pub(crate) cache_retention: ygg_ai::CacheRetention,
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

pub(crate) struct DelegationManager {
    config: DelegationConfig,
    team_directory: PathBuf,
    journal: ProvenanceJournal,
    template: DelegationTemplate,
    state: Mutex<ManagerState>,
    permits: Arc<Semaphore>,
    changed: Notify,
}

struct ManagerState {
    next_agent_number: u64,
    total_agents: usize,
    active_waiters: usize,
    records: BTreeMap<String, AgentRecord>,
    root_mailbox: VecDeque<MailboxMessage>,
    persistence_error: Option<String>,
    shutting_down: bool,
}

struct AgentRecord {
    identity: AgentIdentity,
    task_name: String,
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
}

struct SpawnRequest {
    task_name: String,
    message: String,
}

struct FollowUpRequest {
    target: String,
    message: String,
}

struct WorkerCommand {
    kind: WorkerCommandKind,
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
        task: &'a str,
        session: &'a Path,
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
    fn create(path: &Path) -> Result<Self, SecureFileError> {
        let file = secure_fs::create_regular_file_for_append(path)?;
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
        config.validate()?;
        let team_directory = create_private_team_directory(&config.session_directory)?;
        let journal = ProvenanceJournal::create(&team_directory.join("provenance.jsonl"))?;
        let child_slots = config.limits.max_concurrent_agents - 1;
        let manager = Arc::new(Self {
            config,
            team_directory,
            journal,
            template,
            state: Mutex::new(ManagerState {
                next_agent_number: 1,
                total_agents: 1,
                active_waiters: 0,
                records: BTreeMap::new(),
                root_mailbox: VecDeque::new(),
                persistence_error: None,
                shutting_down: false,
            }),
            permits: Arc::new(Semaphore::new(child_slots)),
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

    fn spawn(
        self: &Arc<Self>,
        owner: &AgentIdentity,
        request: SpawnRequest,
    ) -> Result<Value, String> {
        let SpawnRequest { task_name, message } = request;
        validate_task_name(&task_name)?;
        let initial_task = bounded_text(&message);
        drop(message);
        if owner.depth >= self.config.limits.max_depth {
            return Err(format!(
                "delegation depth limit reached at {} (max depth {})",
                owner.path, self.config.limits.max_depth
            ));
        }
        let permit = Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
            "delegation concurrency limit reached; wait for an active agent".to_owned()
        })?;

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
            let session_file = secure_fs::create_regular_file_for_append(&session_path)
                .map_err(|error| error.to_string())?;
            let session = match Session::create_with_file(&session_path, session_file) {
                Ok(session) => session,
                Err(error) => {
                    let _ = secure_fs::remove_regular_file_if_exists(&session_path);
                    return Err(error.to_string());
                }
            };
            let (command_tx, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
            let shutdown = crate::CancellationToken::default();
            if let Err(error) = self.journal.append(&ProvenanceEvent::AgentSpawned {
                timestamp_ms: timestamp_ms(),
                agent_id: &identity.id,
                agent_path: &identity.path,
                parent_id: &owner.id,
                task_name: &task_name,
                task: &initial_task,
                session: &session_path,
            }) {
                let message = format!("could not persist delegation provenance: {error}");
                self.fail_persistence_locked(&mut state, &error);
                drop(state);
                drop(command_tx);
                drop(session);
                let _ = secure_fs::remove_regular_file_if_exists(&session_path);
                return Err(message);
            }
            state.records.insert(
                identity.id.clone(),
                AgentRecord {
                    identity: identity.clone(),
                    task_name: task_name.clone(),
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
                },
            );
            state.total_agents += 1;
            (identity, session, command_rx, shutdown, task_name)
        };

        let manager = Arc::clone(self);
        let worker_identity = identity.clone();
        tokio::spawn(async move {
            manager
                .run_worker(
                    worker_identity,
                    session,
                    initial_task,
                    command_rx,
                    shutdown,
                    permit,
                )
                .await;
        });
        self.changed.notify_waiters();

        Ok(json!({
            "agent_id": identity.id,
            "agent_path": identity.path,
            "task_name": task_name,
            "status": "pending"
        }))
    }

    async fn run_worker(
        self: Arc<Self>,
        identity: AgentIdentity,
        session: Session,
        initial_task: String,
        mut commands: mpsc::Receiver<WorkerCommand>,
        shutdown: crate::CancellationToken,
        initial_permit: OwnedSemaphorePermit,
    ) {
        let mut agent = match self.build_child_agent(session, &identity) {
            Ok(agent) => agent,
            Err(error) => {
                self.fail_worker_start(&identity.id, bounded_text(&error.to_string()));
                self.request_shutdown_descendants(&identity.id);
                return;
            }
        };

        let mut queued_tasks = VecDeque::from([QueuedTask::Initial(initial_task)]);
        let mut initial_permit = Some(initial_permit);
        loop {
            if shutdown.is_cancelled() {
                self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                self.request_shutdown_descendants(&identity.id);
                return;
            }
            if self.interrupt_requested(&identity.id) {
                self.discard_queued_tasks(&identity.id, &mut queued_tasks);
                initial_permit.take();
                let saw_shutdown = self.drain_interrupted_commands(&identity.id, &mut commands);
                if saw_shutdown || shutdown.is_cancelled() {
                    self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                    self.request_shutdown_descendants(&identity.id);
                    return;
                }
                self.set_status(&identity.id, DelegatedAgentStatus::Interrupted, true);
                self.request_shutdown_descendants(&identity.id);
                continue;
            }

            if !queued_tasks.is_empty() {
                let permit = if let Some(permit) = initial_permit.take() {
                    permit
                } else {
                    if !self.set_pending_if_needed(&identity.id) {
                        return;
                    }
                    match self.acquire_follow_up_permit(&identity.id, &shutdown).await {
                        PermitWait::Acquired(permit) => permit,
                        PermitWait::Interrupted => {
                            self.discard_queued_tasks(&identity.id, &mut queued_tasks);
                            let saw_shutdown =
                                self.drain_interrupted_commands(&identity.id, &mut commands);
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
                let active_follow_up = task.follow_up_usage();
                let pending = self.take_pending_messages(&identity.id);
                let task = task.format(&pending);
                let execution = self
                    .execute_child_run(&mut agent, &identity, task, &mut commands, &shutdown)
                    .await;
                drop(permit);

                let WorkerExecution {
                    outcome,
                    deferred_follow_ups,
                    mut acknowledged_follow_ups,
                } = execution;
                acknowledged_follow_ups.add_usage(active_follow_up);
                self.release_follow_up_usage(&identity.id, acknowledged_follow_ups);

                match outcome {
                    WorkerOutcome::Shutdown => {
                        self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                        self.request_shutdown_descendants(&identity.id);
                        return;
                    }
                    WorkerOutcome::Interrupted => {
                        self.discard_queued_tasks(&identity.id, &mut queued_tasks);
                        self.release_follow_up_usage(
                            &identity.id,
                            follow_up_queue_usage(&deferred_follow_ups),
                        );
                        let saw_shutdown =
                            self.drain_interrupted_commands(&identity.id, &mut commands);
                        if saw_shutdown || shutdown.is_cancelled() {
                            self.set_status(&identity.id, DelegatedAgentStatus::Shutdown, true);
                            self.request_shutdown_descendants(&identity.id);
                            return;
                        }
                        self.set_status(&identity.id, DelegatedAgentStatus::Interrupted, true);
                        self.request_shutdown_descendants(&identity.id);
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
                _ = &mut notified => continue,
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
                }
                WorkerCommandKind::FollowUp(follow_up) => {
                    queued_tasks.push_back(QueuedTask::FollowUp(follow_up));
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
        let system = format!(
            "{}\n\n{}",
            self.template.base_system,
            child_instructions(identity, parent_path, &self.config.limits)
        );
        let mut agent = Agent::new(AgentConfig {
            client: self.template.client.clone(),
            model: self.template.model.clone(),
            session,
            system,
            sandbox: self.template.sandbox.clone(),
            extensions: self.template.extensions.clone(),
            max_turns: self.template.max_turns,
            reasoning: self.template.reasoning.clone(),
            reasoning_mode: self.template.reasoning_mode,
            cache_retention: self.template.cache_retention,
            session_id: None,
        })?;
        agent.set_compaction_model(self.template.compaction_model.clone());
        agent.set_compaction_token_mode(
            self.template.auto_compaction_mode,
            self.template.auto_compaction_threshold,
            self.template.compaction_keep_recent_tokens,
        )?;
        agent.set_completion_policy(self.template.completion_policy);
        agent.set_output_modalities(self.template.output_modalities.clone());
        agent.inherit_max_output_tokens(self.template.max_output_tokens);
        agent.set_max_session_cost_microdollars(self.template.max_session_cost_microdollars);
        agent.set_provider_retries_enabled(self.template.provider_retries_enabled);
        let binding = DelegationBinding {
            manager: Arc::clone(self),
            identity: identity.clone(),
            system_instructions: Arc::from(""),
        };
        agent.install_delegation_tools(self.tools(identity));
        agent.set_delegation_binding(binding)?;
        Ok(agent)
    }

    async fn execute_child_run(
        self: &Arc<Self>,
        agent: &mut Agent,
        identity: &AgentIdentity,
        task: String,
        commands: &mut mpsc::Receiver<WorkerCommand>,
        shutdown: &crate::CancellationToken,
    ) -> WorkerExecution {
        if shutdown.is_cancelled() {
            return WorkerExecution::new(WorkerOutcome::Shutdown);
        }
        if self.interrupt_requested(&identity.id) {
            return WorkerExecution::new(WorkerOutcome::Interrupted);
        }
        let mut run = match agent.prompt(task).await {
            Ok(run) => run,
            Err(error) => return WorkerExecution::new(WorkerOutcome::Failed(error.to_string())),
        };
        let control = run.control();

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
        let mut commands_open = true;
        enum Next {
            Event(Option<AgentEvent>),
            Command(Option<WorkerCommand>),
            Changed,
            Shutdown,
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
                _ = &mut notified, if !requested_interrupt => Next::Changed,
                event = run.next() => Next::Event(event),
                command = commands.recv(), if commands_open => Next::Command(command),
            };
            match next {
                Next::Changed => {}
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
                Next::Event(Some(AgentEvent::TurnFinished { message, .. })) => {
                    for part in message.content {
                        if let AssistantPart::Text(text) = part {
                            if !output.is_empty() {
                                output.push('\n');
                            }
                            output.push_str(&text);
                            if output.len() > MAX_PROVENANCE_TEXT_BYTES {
                                output = bounded_text(&output);
                            }
                        }
                    }
                }
                Next::Event(Some(AgentEvent::RunFinished { reason, .. })) => {
                    break if requested_shutdown {
                        WorkerOutcome::Shutdown
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
        }
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
        if let Err(error) = self.journal.append(&ProvenanceEvent::AgentStatus {
            timestamp_ms: timestamp_ms(),
            agent_id: id,
            status: &status,
        }) {
            self.fail_persistence_locked(&mut state, &error);
            return false;
        }
        let notification = {
            let record = state.records.get_mut(id).expect("checked child exists");
            record.status = status;
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
        let status = DelegatedAgentStatus::Failed { error };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.persistence_error.is_some() || !state.records.contains_key(id) {
            return;
        }
        if let Err(error) = self.journal.append(&ProvenanceEvent::AgentStatus {
            timestamp_ms: timestamp_ms(),
            agent_id: id,
            status: &status,
        }) {
            self.fail_persistence_locked(&mut state, &error);
            return;
        }
        let notification = {
            let record = state.records.get_mut(id).expect("checked child exists");
            record.status = status;
            record.shutdown.cancel();
            record.pending_messages.clear();
            record.reserved_messages = QueueUsage::default();
            record.queued_follow_ups = QueueUsage::default();
            (
                record.parent_id.clone(),
                MailboxMessage {
                    kind: "task_status",
                    from: id.to_owned(),
                    task_name: Some(record.task_name.clone()),
                    message: status_message(&record.identity.path, &record.status),
                    evictable: true,
                },
            )
        };
        push_mailbox_locked(&mut state, &notification.0, notification.1);
        drop(state);
        self.changed.notify_waiters();
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
            return (owner.path == ROOT_AGENT_PATH && owner.depth == 0)
                .then_some(())
                .ok_or_else(|| "invalid root delegation identity".to_owned());
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
                    },
                ));
            }
        }
        for (parent_id, message) in notifications {
            push_mailbox_locked(state, &parent_id, message);
        }
        self.changed.notify_waiters();
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
                permit = Arc::clone(&self.permits).acquire_owned() => {
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
    ) -> bool {
        let mut messages = Vec::new();
        let mut discarded_follow_ups = QueueUsage::default();
        let mut saw_shutdown = false;
        while let Ok(command) = commands.try_recv() {
            match command.kind {
                WorkerCommandKind::Message(message) => messages.push(message),
                WorkerCommandKind::FollowUp(follow_up) => {
                    discarded_follow_ups.add_usage(follow_up.usage());
                }
                WorkerCommandKind::Shutdown => saw_shutdown = true,
            }
        }
        for message in messages {
            self.queue_reserved_message(target, message);
        }
        self.release_follow_up_usage(target, discarded_follow_ups);
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
            .map(|record| record.pending_messages.drain(..).collect())
            .unwrap_or_default()
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
                debug_assert!(pending_messages_can_accept(
                    &record.pending_messages,
                    &message
                ));
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

    fn discard_queued_tasks(&self, target: &str, tasks: &mut VecDeque<QueuedTask>) {
        let usage = tasks.iter().fold(QueueUsage::default(), |mut usage, task| {
            usage.add_usage(task.follow_up_usage());
            usage
        });
        tasks.clear();
        self.release_follow_up_usage(target, usage);
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
        let candidate = DirectedMessage {
            from: owner.id.clone(),
            message: bounded_text(&message),
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
        let follow_up = QueuedFollowUp {
            from: owner.id.clone(),
            message: bounded_text(&request.message),
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
        &self,
        owner: &AgentIdentity,
        timeout: Duration,
        cancellation: &crate::CancellationToken,
    ) -> Result<Value, String> {
        if let Some(result) = self.take_wait_result(owner)? {
            return Ok(result);
        }
        let _waiter = self.register_waiter(owner)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.take_wait_result(owner)? {
                return Ok(result);
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("wait_agent cancelled".into()),
                _ = tokio::time::sleep_until(deadline) => {
                    let agents = self.list_value_for(owner)?;
                    return Ok(json!({"timed_out": true, "messages": [], "agents": agents}));
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

    fn take_wait_result(&self, owner: &AgentIdentity) -> Result<Option<Value>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_owner_active_locked(&state, owner)?;
        let mailbox = if owner.id == ROOT_AGENT_ID {
            &mut state.root_mailbox
        } else {
            &mut state
                .records
                .get_mut(&owner.id)
                .expect("validated owner exists")
                .mailbox
        };
        if !mailbox.is_empty() {
            let messages = mailbox.drain(..).collect::<Vec<_>>();
            return Ok(Some(json!({"timed_out": false, "messages": messages})));
        }
        let descendants_running = state.records.values().any(|record| {
            is_descendant_path(&record.identity.path, &owner.path) && record.status.is_running()
        });
        if !descendants_running {
            Ok(Some(
                json!({"timed_out": false, "messages": [], "agents": list_value_locked(&state)}),
            ))
        } else {
            Ok(None)
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
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let owner_path = if owner_id == ROOT_AGENT_ID {
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
    fn follow_up_usage(&self) -> QueueUsage {
        match self {
            Self::Initial(_) => QueueUsage::default(),
            Self::FollowUp(follow_up) => follow_up.usage(),
        }
    }

    fn format(self, pending: &[DirectedMessage]) -> String {
        match self {
            Self::Initial(task) => format_initial_task(&task, pending),
            Self::FollowUp(follow_up) => {
                format_follow_up(&follow_up.from, &follow_up.message, pending)
            }
        }
    }
}

struct WorkerExecution {
    outcome: WorkerOutcome,
    deferred_follow_ups: VecDeque<QueuedFollowUp>,
    acknowledged_follow_ups: QueueUsage,
}

impl WorkerExecution {
    fn new(outcome: WorkerOutcome) -> Self {
        Self {
            outcome,
            deferred_follow_ups: VecDeque::new(),
            acknowledged_follow_ups: QueueUsage::default(),
        }
    }
}

enum WorkerOutcome {
    Completed(String),
    Interrupted,
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

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let manager = self
            .manager
            .upgrade()
            .ok_or_else(|| ToolError::new("delegation team is no longer available"))?;
        let value = match self.kind {
            CollaborationToolKind::Spawn => {
                let request = SpawnRequest {
                    task_name: required_string(&args, "task_name")?,
                    message: required_string(&args, "message")?,
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
            CollaborationToolKind::Wait => {
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(30_000)
                    .clamp(1, MAX_TOOL_TIMEOUT_MS);
                manager
                    .wait(
                        &self.owner,
                        Duration::from_millis(timeout_ms),
                        &ctx.cancellation,
                    )
                    .await
            }
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
    for entry in mailbox.iter().filter(|entry| entry.evictable) {
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
        let Some(index) = mailbox.iter().position(|entry| entry.evictable) else {
            // Accepted direct messages are durable work and must never be evicted by
            // best-effort automatic notifications.
            return;
        };
        mailbox.remove(index);
    }
    mailbox.push_back(message);
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

fn pending_messages_can_accept(
    messages: &VecDeque<DirectedMessage>,
    message: &DirectedMessage,
) -> bool {
    messages.len() < MAX_PENDING_MESSAGES
        && messages
            .iter()
            .fold(0usize, |total, item| {
                total.saturating_add(directed_message_bytes(item))
            })
            .saturating_add(directed_message_bytes(message))
            <= MAX_PENDING_MESSAGE_BYTES
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

fn follow_up_queue_usage(follow_ups: &VecDeque<QueuedFollowUp>) -> QueueUsage {
    follow_ups
        .iter()
        .fold(QueueUsage::default(), |mut usage, follow_up| {
            usage.add_usage(follow_up.usage());
            usage
        })
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
        .map(|record| {
            json!({
                "agent_id": record.identity.id,
                "agent_path": record.identity.path,
                "parent_id": record.parent_id,
                "task_name": record.task_name,
                "depth": record.identity.depth,
                "session": record.session_path,
                "status": record.status
            })
        })
        .collect::<Vec<_>>();
    json!({"agents": agents, "persistence_error": state.persistence_error})
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

fn bounded_text(text: &str) -> String {
    const SUFFIX: &str = "\n...[truncated]";
    if text.len() <= MAX_PROVENANCE_TEXT_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_PROVENANCE_TEXT_BYTES.saturating_sub(SUFFIX.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], SUFFIX)
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn create_private_team_directory(parent: &Path) -> Result<PathBuf, DelegationError> {
    let parent = std::path::absolute(parent)?;
    Ok(secure_fs::create_unique_private_directory(
        &parent, "team-",
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with_journal(file: File, directory: &Path) -> Arc<DelegationManager> {
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        let max_output_tokens = model.spec.limits.max_output_tokens;
        Arc::new(DelegationManager {
            config: DelegationConfig::new(directory),
            team_directory: directory.to_path_buf(),
            journal: ProvenanceJournal {
                file: Mutex::new(file),
            },
            template: DelegationTemplate {
                client: ygg_ai::AiClient::new(),
                model,
                base_system: "test".into(),
                sandbox: crate::SandboxConfig::new(directory),
                extensions: ExtensionHost::new(),
                max_turns: Some(4),
                reasoning: ygg_ai::ReasoningConfig::Off,
                reasoning_mode: ygg_ai::ReasoningMode::Standard,
                cache_retention: ygg_ai::CacheRetention::Short,
                compaction_model: None,
                auto_compaction_mode: AgentCompactionMode::Local,
                auto_compaction_threshold: 0.85,
                compaction_keep_recent_tokens: 1_024,
                completion_policy: CompletionPolicy::Natural,
                output_modalities: ygg_ai::OutputModalities::Text,
                max_output_tokens,
                max_session_cost_microdollars: None,
                provider_retries_enabled: true,
            },
            state: Mutex::new(ManagerState {
                next_agent_number: 1,
                total_agents: 1,
                active_waiters: 0,
                records: BTreeMap::new(),
                root_mailbox: VecDeque::new(),
                persistence_error: None,
                shutting_down: false,
            }),
            permits: Arc::new(Semaphore::new(3)),
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
                },
            );
        }
        let direct = MailboxMessage {
            kind: "message",
            from: "agent-a".into(),
            task_name: None,
            message: "durable".into(),
            evictable: false,
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
    async fn follow_up_queue_is_bounded_and_interrupt_drain_releases_reservations() {
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

        assert!(!manager.drain_interrupted_commands("agent-1", &mut command_rx));
        assert_eq!(
            manager.state.lock().unwrap().records["agent-1"].queued_follow_ups,
            QueueUsage::default()
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
        match follow_result {
            Ok(_) => assert_eq!(command_rx.len(), 1),
            Err(error) => {
                assert!(error.contains("being interrupted"), "{error}");
                assert_eq!(command_rx.len(), 0);
            }
        }
        manager.drain_interrupted_commands("agent-1", &mut command_rx);
        assert_eq!(
            manager.state.lock().unwrap().records["agent-1"].queued_follow_ups,
            QueueUsage::default()
        );
    }

    #[test]
    fn child_inherits_the_roots_resolved_output_token_policy() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = writable_manager(directory.path());
        Arc::get_mut(&mut manager)
            .unwrap()
            .template
            .max_output_tokens = 777;
        let session = Session::create(directory.path().join("child.jsonl")).unwrap();
        let identity = AgentIdentity {
            id: "agent-1".into(),
            path: "/root/child".into(),
            depth: 1,
        };

        let child = manager.build_child_agent(session, &identity).unwrap();
        assert_eq!(child.max_output_tokens(), 777);
    }

    #[tokio::test]
    async fn worker_start_failure_closes_worker_and_clears_accepted_work() {
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
            assert!(record.shutdown.is_cancelled());
            assert!(record.pending_messages.is_empty());
            assert_eq!(record.reserved_messages, QueueUsage::default());
            assert_eq!(record.queued_follow_ups, QueueUsage::default());
        }
        let error = manager
            .send_message(&root_identity(), &identity.id, "too late".into())
            .await
            .unwrap_err();
        assert!(error.contains("target is shut down"), "{error}");
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
                    message: "must not launch".into(),
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
                    message: "still closed".into(),
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
