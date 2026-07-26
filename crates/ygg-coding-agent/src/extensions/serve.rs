#![allow(missing_docs)]

//! Default-off adapter from the graphical host contracts to the real Ygg App.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sexy_tui_rs::{Color as TuiColor, TextStyle as TuiTextStyle};
use sha2::{Digest as _, Sha256};
use tokio::sync::{mpsc, oneshot};
use ygg_agent::{
    AgentEvent, Entry, EntryValue, FinishReason, InputPart, OutputChannel, RunControl, Session,
    SessionRunOutcome, SessionRunOutcomeStatus, ToolOutput, ToolProgress, UserInput,
};
use ygg_ai::{
    AssistantPart, ImageSource, Media, Message, Modality, Model, ModelCatalog, ModelId,
    ReasoningConfig, ToolCallId, ToolResultPart, UserPart,
};
use ygg_serve_backend::{
    ActorOwnerState, ArtifactId, ArtifactKind, ArtifactRef, AttachmentError, AttachmentFingerprint,
    AttachmentPolicy, AttachmentRef, AttachmentStore, AttentionState, AuthorityProfile,
    ColorScheme, ContextUsage, CreateSessionRequest, DriverCommandOutcome, DurableEntryId,
    EventPayload, FileChange, HostCapabilities, HostDescriptor, HostId, HostService, InputModality,
    ItemDelta, ItemId, ItemLifecycle, ItemPayload, LoopbackConfig, LoopbackServer, ModelSelection,
    ModelSummary, PendingRequest, ProjectId, ProjectSummary, PromptInput, ProtocolValidation,
    RequestAnswer, RequestId, RequestKind, RequestState, RunId, SemanticRole, ServiceError,
    SessionCommand, SessionCursor, SessionDriver, SessionId, SessionItem, SessionLiveState,
    SessionSeed, SessionSnapshot, SessionSummary, SessionSupervisor, SourceId, SourceKind,
    SourceRef, StoredAttachment, StoredResource, SupervisorConfig, ThemeColor, ThemeDensity,
    ThemeDto, ThemeId, ThemeMotion, ThemeOption, ThemeRoleStyle, ThemeSourceClass, ThemeTypography,
    TimestampedEvent, TurnId, UsageSnapshot, MAX_ITEM_TEXT_BYTES, MAX_PROMPT_BYTES,
};

use crate::app::bootstrap::{build_app, LaunchSelection, SessionSelection};
use crate::app::{reasoning_label, supported_levels, App, Reconfig};
use crate::config::{self, Config};
use crate::resources::compose_instructions;
use crate::session_store::{SessionMeta, SessionStore};

const DRIVER_MAILBOX_CAPACITY: usize = 64;
const DRIVER_EVENT_CAPACITY: usize = 512;
const MAX_GRAPHICAL_MODELS: usize = 256;
const MAX_PROJECTED_SESSION_ITEMS: usize = 9_000;
const MAX_OPAQUE_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_OPAQUE_RESOURCE_COUNT: usize = 2_048;
const MAX_OPAQUE_RESOURCE_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const OPAQUE_RESOURCE_HANDLE_BYTES: usize = 32;

static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct OpaqueResourceIndex {
    resources: HashMap<String, StoredResource>,
    total_bytes: usize,
}

/// Process-local immutable resource snapshots.
///
/// A client can only dereference a random host-minted handle through the
/// authenticated loopback route. Paths and URLs are never accepted by that
/// route, and every registered snapshot is independently bounded.
#[derive(Clone, Default)]
struct OpaqueResourceRegistry {
    index: Arc<Mutex<OpaqueResourceIndex>>,
}

impl OpaqueResourceRegistry {
    fn register(
        &self,
        display_name: &str,
        media_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<(String, StoredResource), ServiceError> {
        if bytes.len() > MAX_OPAQUE_RESOURCE_BYTES || !safe_media_type(media_type) {
            return Err(ServiceError::InvalidBoundary);
        }
        let display_name = safe_resource_name(display_name)?;
        let sha256 = stable_hash(&bytes);
        let mut index = self.index.lock().map_err(|_| ServiceError::Internal)?;
        if index.resources.len() >= MAX_OPAQUE_RESOURCE_COUNT
            || index
                .total_bytes
                .checked_add(bytes.len())
                .is_none_or(|total| total > MAX_OPAQUE_RESOURCE_TOTAL_BYTES)
        {
            return Err(ServiceError::Unavailable);
        }
        let handle = loop {
            let mut random = [0u8; OPAQUE_RESOURCE_HANDLE_BYTES];
            getrandom::fill(&mut random).map_err(|_| ServiceError::Internal)?;
            let candidate = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if !index.resources.contains_key(&candidate) {
                break candidate;
            }
        };
        let resource = StoredResource {
            display_name,
            media_type: media_type.to_owned(),
            bytes,
            sha256,
        };
        index.total_bytes = index.total_bytes.saturating_add(resource.bytes.len());
        index.resources.insert(handle.clone(), resource.clone());
        Ok((handle, resource))
    }

    fn content(&self, handle: &str) -> Result<StoredResource, ServiceError> {
        if handle.len() != OPAQUE_RESOURCE_HANDLE_BYTES * 2
            || !handle
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ServiceError::NotFound);
        }
        self.index
            .lock()
            .map_err(|_| ServiceError::Internal)?
            .resources
            .get(handle)
            .cloned()
            .ok_or(ServiceError::NotFound)
    }
}

fn safe_media_type(value: &str) -> bool {
    value.contains('/')
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
}

fn safe_resource_name(value: &str) -> Result<String, ServiceError> {
    let normalized = value.replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default().trim();
    if basename.is_empty() || matches!(basename, "." | "..") {
        return Err(ServiceError::InvalidBoundary);
    }
    let basename = ygg_serve_backend::sanitize_public_text(basename, 512, false);
    if basename.is_empty() {
        return Err(ServiceError::InvalidBoundary);
    }
    Ok(basename)
}

pub async fn run(
    config: Config,
    port: u16,
    no_open: bool,
    web_root: Option<PathBuf>,
) -> anyhow::Result<()> {
    let _host_lock = ServeHostLock::acquire(&config)?;
    let host = Arc::new(YggHost::new(config)?);
    let supervisor = Arc::new(SessionSupervisor::new(host, SupervisorConfig::default()));
    let server = LoopbackServer::start(
        supervisor,
        LoopbackConfig {
            port,
            web_root: web_root.clone(),
        },
    )
    .await?;
    let clean_url = server.url();
    if let Some(root) = web_root {
        crate::output::stdout_line(format!("Web app: {}", root.display()));
    } else {
        crate::output::stdout_line("Web app: embedded");
    }
    if no_open {
        // Explicit trusted terminal output: the launch capability is one-use,
        // process-local, and stripped from the browser address bar by an immediate
        // redirect. It is never persisted or included in server errors.
        crate::output::stdout_line(format!("Open ygg once: {}", server.launch_url()));
    } else {
        if let Err(error) = open_browser(&server.launch_url()) {
            crate::output::stderr_line(format!(
                "warning: could not open the browser automatically: {error}"
            ));
        }
        crate::output::stdout_line(format!("ygg graphical host: {clean_url}"));
    }
    tokio::signal::ctrl_c().await?;
    server.shutdown().await?;
    Ok(())
}

/// One graphical host per Ygg session root.
///
/// This intentionally does not claim that legacy TUI processes participate in
/// the same host lock. Individual session opens still surface the underlying
/// Ygg session lock/concurrent-modification failure rather than crossing into
/// `ygg-agent` core to change legacy ownership semantics.
struct ServeHostLock {
    _file: std::fs::File,
}

impl ServeHostLock {
    fn acquire(config: &Config) -> anyhow::Result<Self> {
        Self::acquire_at(&config.session_dir)
    }

    fn acquire_at(session_dir: &Path) -> anyhow::Result<Self> {
        use fs2::FileExt as _;

        let state_dir = secure_serve_state_dir(session_dir)?;
        let path = state_dir.join("host.lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        if !file.metadata()?.is_file() {
            anyhow::bail!("ygg serve host lock must be a regular file");
        }
        file.try_lock_exclusive().map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
            ) {
                anyhow::anyhow!("another ygg serve process already owns this session catalog")
            } else {
                anyhow::Error::from(error)
            }
        })?;
        Ok(Self { _file: file })
    }
}

fn open_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[derive(Clone)]
struct YggHost {
    config: Config,
    catalog: ModelCatalog,
    sessions: SessionStore,
    models: Vec<ModelSummary>,
    descriptor: HostDescriptor,
    project_id: ProjectId,
    themes: Vec<ThemeOption>,
    selected_theme_id: ThemeId,
    attachments: Option<AttachmentStore>,
    resources: OpaqueResourceRegistry,
}

impl YggHost {
    fn new(config: Config) -> anyhow::Result<Self> {
        let boot = crate::app::bootstrap::bootstrap(config.clone())?;
        let models = graphical_model_catalog(&boot.catalog, &config);
        if models.is_empty() {
            anyhow::bail!("no configured models are available for ygg serve");
        }
        let workspace_hash = stable_hash(config.workspace.to_string_lossy().as_bytes());
        let host_id = load_or_create_host_id(&config)?;
        let project_id = ProjectId::new(format!("project-{}", &workspace_hash[..24]))
            .map_err(anyhow::Error::msg)?;
        let workspace_name = config
            .workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("workspace");
        let descriptor = HostDescriptor {
            id: host_id,
            name: ygg_serve_backend::sanitize_public_text(
                &format!("ygg — {workspace_name}"),
                256,
                false,
            ),
        };
        let (themes, selected_theme_id) = graphical_themes(&config)?;
        let state_dir = secure_serve_state_dir(&config.session_dir)?;
        let attachments = match AttachmentStore::open(&state_dir) {
            Ok(store) => Some(store),
            Err(_) => {
                crate::output::stderr_line(
                    "warning: secure attachment storage is unavailable; image uploads are disabled",
                );
                None
            }
        };
        Ok(Self {
            config,
            catalog: boot.catalog,
            sessions: boot.sessions,
            models,
            descriptor,
            project_id,
            themes,
            selected_theme_id,
            attachments,
            resources: OpaqueResourceRegistry::default(),
        })
    }

    fn default_selection(&self) -> Result<ModelSelection, ServiceError> {
        if let Some(model_id) = &self.config.model {
            let summary = self
                .models
                .iter()
                .find(|summary| summary.id == model_id.0)
                .ok_or(ServiceError::InvalidSeed)?;
            return Ok(selection_from_summary(summary));
        }
        let summary = self.models.first().ok_or(ServiceError::InvalidSeed)?;
        Ok(selection_from_summary(summary))
    }

    fn driver_for_new(
        &self,
        request: CreateSessionRequest,
    ) -> Result<YggSessionDriver, ServiceError> {
        if request
            .project_id
            .as_ref()
            .is_some_and(|project_id| project_id != &self.project_id)
        {
            return Err(ServiceError::NotFound);
        }
        let model = match request.model {
            Some(model) => model,
            None => self.default_selection()?,
        };
        let summary = self
            .models
            .iter()
            .find(|summary| summary.provider == model.provider && summary.id == model.model)
            .ok_or(ServiceError::InvalidSeed)?;
        if !summary
            .reasoning
            .iter()
            .any(|choice| choice == &model.reasoning)
        {
            return Err(ServiceError::InvalidSeed);
        }
        let resolved = self
            .catalog
            .resolve(&ModelId(model.model.clone()))
            .map_err(|_| ServiceError::InvalidSeed)?;
        let reasoning =
            config::parse_reasoning(&model.reasoning).map_err(|_| ServiceError::InvalidSeed)?;
        let session_path = self.sessions.new_path(&crate::modes::timestamp());
        let session_id = session_id_from_path(&session_path)?;
        let generation = next_actor_generation();
        let selection = selection_for_model(&resolved, &reasoning, &self.config);
        let seed = empty_seed(
            session_id,
            request.project_id.or_else(|| Some(self.project_id.clone())),
            selection.clone(),
            request.authority,
            generation,
        );
        let plan = WorkerPlan {
            config: self.config.clone(),
            sessions: self.sessions.clone(),
            launch: LaunchSelection {
                model: resolved.spec.id.clone(),
                session: SessionSelection::CreateNew(session_path),
                reasoning,
                reasoning_mode: self.config.reasoning_mode,
            },
            authority: request.authority,
            available_models: self.models.clone(),
            actor_generation: generation,
            session_id: seed.summary.id.clone(),
            attachments: self.attachments.clone(),
            resources: self.resources.clone(),
        };
        Ok(YggSessionDriver::spawn(seed, plan, 0))
    }

    fn driver_for_existing(
        &self,
        session_id: &SessionId,
    ) -> Result<YggSessionDriver, ServiceError> {
        let path = self
            .sessions
            .path_by_id(session_id.as_str())
            .map_err(|_| ServiceError::NotFound)?;
        let session = Session::open_read_only(&path).map_err(|_| ServiceError::InvalidSeed)?;
        let selection = selection_from_session(&session, &self.catalog, &self.config)
            .or_else(|_| self.default_selection())?;
        let generation = next_actor_generation();
        let seed = seed_from_session(
            &session,
            session_id.clone(),
            SessionSeedOptions {
                project_id: Some(self.project_id.clone()),
                model: selection.clone(),
                authority: AuthorityProfile::FullAccess,
                generation,
                meta: session_meta_for_id(&self.sessions, session_id),
                attachment_store: self.attachments.as_ref(),
            },
        )?;
        let reasoning =
            config::parse_reasoning(&selection.reasoning).map_err(|_| ServiceError::InvalidSeed)?;
        let plan = WorkerPlan {
            config: self.config.clone(),
            sessions: self.sessions.clone(),
            launch: LaunchSelection {
                model: ModelId(selection.model),
                session: SessionSelection::OpenExisting(path),
                reasoning,
                reasoning_mode: self.config.reasoning_mode,
            },
            authority: AuthorityProfile::FullAccess,
            available_models: self.models.clone(),
            actor_generation: generation,
            session_id: session_id.clone(),
            attachments: self.attachments.clone(),
            resources: self.resources.clone(),
        };
        let known_entries = session.entries().len();
        Ok(YggSessionDriver::spawn(seed, plan, known_entries))
    }
}

#[async_trait]
impl HostService for YggHost {
    type Driver = YggSessionDriver;

    fn descriptor(&self) -> HostDescriptor {
        self.descriptor.clone()
    }

    fn capabilities(&self) -> HostCapabilities {
        let attachment_policy = self.attachments.as_ref().map(AttachmentStore::policy);
        HostCapabilities {
            concurrent_sessions: true,
            opaque_resources: true,
            attachments: attachment_policy.is_some(),
            attachment_policy,
            previews: false,
            connected_devices: false,
            session_metadata: true,
            lan_clients: false,
            terminal: false,
            child_agents: false,
        }
    }

    fn attachment_policy(&self) -> Option<AttachmentPolicy> {
        self.attachments.as_ref().map(AttachmentStore::policy)
    }

    async fn ingest_attachment(
        &self,
        display_name: &str,
        media_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<AttachmentRef, AttachmentError> {
        self.attachments
            .as_ref()
            .ok_or(AttachmentError::Unavailable)?
            .ingest(display_name, media_type, bytes)
    }

    async fn attachment_content(&self, handle: &str) -> Result<StoredAttachment, AttachmentError> {
        self.attachments
            .as_ref()
            .ok_or(AttachmentError::Unavailable)?
            .content(handle)
    }

    async fn resource_content(&self, handle: &str) -> Result<StoredResource, ServiceError> {
        self.resources.content(handle)
    }

    fn authority_ceiling(&self) -> AuthorityProfile {
        AuthorityProfile::FullAccess
    }

    fn authority_profiles(&self) -> Vec<AuthorityProfile> {
        // The first slice preserves Ygg's current authority exactly. A later
        // adapter can expose narrower profiles after it can rebuild the real
        // sandbox without changing their meaning.
        vec![AuthorityProfile::FullAccess]
    }

    fn model_catalog(&self) -> Vec<ModelSummary> {
        self.models.clone()
    }

    fn theme_catalog(&self) -> Vec<ThemeOption> {
        self.themes.clone()
    }

    fn selected_theme_id(&self) -> ThemeId {
        self.selected_theme_id.clone()
    }

    async fn list_projects(&self) -> Result<Vec<ProjectSummary>, ServiceError> {
        let session_count = self.sessions.list().len().min(u32::MAX as usize) as u32;
        Ok(vec![ProjectSummary {
            id: self.project_id.clone(),
            name: ygg_serve_backend::sanitize_public_text(
                self.config
                    .workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Workspace"),
                256,
                false,
            ),
            trusted: self.config.workspace_trusted,
            session_count,
            live_session_count: 0,
        }])
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ServiceError> {
        let fallback = self.default_selection()?;
        Ok(self
            .sessions
            .list()
            .into_iter()
            .take(2_000)
            .filter_map(|meta| {
                let selection = Session::open_read_only(&meta.path)
                    .ok()
                    .and_then(|session| {
                        selection_from_session(&session, &self.catalog, &self.config).ok()
                    })
                    .unwrap_or_else(|| fallback.clone());
                summary_from_meta(&meta, Some(self.project_id.clone()), selection).ok()
            })
            .collect())
    }

    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<Self::Driver, ServiceError> {
        self.driver_for_new(request)
    }

    async fn open_session(&self, session_id: &SessionId) -> Result<Self::Driver, ServiceError> {
        self.driver_for_existing(session_id)
    }
}

struct YggSessionDriver {
    seed: SessionSeed,
    commands: mpsc::Sender<WorkerCommand>,
    events: mpsc::Receiver<TimestampedEvent>,
}

impl YggSessionDriver {
    fn spawn(seed: SessionSeed, plan: WorkerPlan, known_entries: usize) -> Self {
        let (commands, command_receiver) = mpsc::channel(DRIVER_MAILBOX_CAPACITY);
        let (event_sender, events) = mpsc::channel(DRIVER_EVENT_CAPACITY);
        tokio::spawn(run_worker(
            plan,
            command_receiver,
            event_sender,
            known_entries,
        ));
        Self {
            seed,
            commands,
            events,
        }
    }
}

#[async_trait]
impl SessionDriver for YggSessionDriver {
    fn seed(&self) -> SessionSeed {
        self.seed.clone()
    }

    async fn dispatch(
        &mut self,
        command: SessionCommand,
    ) -> Result<DriverCommandOutcome, ServiceError> {
        let (response, receiver) = oneshot::channel();
        self.commands
            .send(WorkerCommand { command, response })
            .await
            .map_err(|_| ServiceError::Unavailable)?;
        receiver.await.map_err(|_| ServiceError::Unavailable)?
    }

    async fn next_event(&mut self) -> Option<TimestampedEvent> {
        self.events.recv().await
    }
}

struct WorkerCommand {
    command: SessionCommand,
    response: oneshot::Sender<Result<DriverCommandOutcome, ServiceError>>,
}

struct WorkerPlan {
    config: Config,
    sessions: SessionStore,
    launch: LaunchSelection,
    authority: AuthorityProfile,
    available_models: Vec<ModelSummary>,
    actor_generation: u64,
    session_id: SessionId,
    attachments: Option<AttachmentStore>,
    resources: OpaqueResourceRegistry,
}

enum PrivateResponse {
    Approval(Box<dyn FnOnce(bool) + Send + Sync>),
    Input(Box<dyn FnOnce(Option<Vec<u8>>) + Send + Sync>),
}

struct PrivateRequest {
    kind: RequestKind,
    response: PrivateResponse,
}

#[derive(Clone)]
struct ProjectedToolCall {
    name: String,
    arguments: serde_json::Value,
}

struct ProjectionState {
    known_entries: usize,
    run_counter: u64,
    user_item_counter: u64,
    request_counter: u64,
    turn_counter: u64,
    provider_attempt: u32,
    assistant_item: Option<ItemId>,
    reasoning_item: Option<ItemId>,
    completed_assistant_items: VecDeque<Option<ItemId>>,
    completed_reasoning_items: VecDeque<Option<ItemId>>,
    tool_items: HashMap<String, ItemId>,
    tool_calls: HashMap<String, ProjectedToolCall>,
    tool_progress: HashMap<String, (String, u64)>,
    private_requests: HashMap<RequestId, PrivateRequest>,
    pending_attachments: VecDeque<Vec<AttachmentRef>>,
    pending_user_items: VecDeque<ItemId>,
}

impl ProjectionState {
    fn new(known_entries: usize) -> Self {
        Self {
            known_entries,
            run_counter: 0,
            user_item_counter: 0,
            request_counter: 0,
            turn_counter: 1,
            provider_attempt: 1,
            assistant_item: None,
            reasoning_item: None,
            completed_assistant_items: VecDeque::new(),
            completed_reasoning_items: VecDeque::new(),
            tool_items: HashMap::new(),
            tool_calls: HashMap::new(),
            tool_progress: HashMap::new(),
            private_requests: HashMap::new(),
            pending_attachments: VecDeque::new(),
            pending_user_items: VecDeque::new(),
        }
    }

    fn next_run_id(&mut self, generation: u64) -> Result<RunId, ServiceError> {
        self.run_counter = self
            .run_counter
            .checked_add(1)
            .ok_or(ServiceError::Internal)?;
        RunId::new(format!("run-{generation}-{}", self.run_counter))
            .map_err(|_| ServiceError::Internal)
    }

    fn begin_run(&mut self) {
        self.user_item_counter = 0;
        self.turn_counter = 1;
        self.provider_attempt = 1;
        self.assistant_item = None;
        self.reasoning_item = None;
        self.completed_assistant_items.clear();
        self.completed_reasoning_items.clear();
        self.tool_items.clear();
        self.tool_calls.clear();
        self.tool_progress.clear();
        self.private_requests.clear();
        self.pending_attachments.clear();
        self.pending_user_items.clear();
    }

    fn next_user_item_id(&mut self, run_id: &RunId) -> Result<ItemId, ServiceError> {
        self.user_item_counter = self
            .user_item_counter
            .checked_add(1)
            .ok_or(ServiceError::Internal)?;
        self.provisional_id(run_id, "user", self.user_item_counter)
    }

    fn turn_id(&self, run_id: &RunId) -> Result<TurnId, ServiceError> {
        TurnId::new(format!("turn-{}-{}", run_id.as_str(), self.turn_counter))
            .map_err(|_| ServiceError::Internal)
    }

    fn provisional_id(
        &self,
        run_id: &RunId,
        kind: &str,
        suffix: u64,
    ) -> Result<ItemId, ServiceError> {
        ItemId::new(format!(
            "item-{}-{kind}-{}-{suffix}",
            run_id.as_str(),
            self.turn_counter
        ))
        .map_err(|_| ServiceError::Internal)
    }

    fn finish_turn(&mut self) {
        self.completed_assistant_items
            .push_back(self.assistant_item.take());
        self.completed_reasoning_items
            .push_back(self.reasoning_item.take());
        self.turn_counter = self.turn_counter.saturating_add(1);
        self.provider_attempt = 1;
    }
}

async fn run_worker(
    mut plan: WorkerPlan,
    mut commands: mpsc::Receiver<WorkerCommand>,
    events: mpsc::Sender<TimestampedEvent>,
    known_entries: usize,
) {
    let mut app: Option<App> = None;
    let mut projection = ProjectionState::new(known_entries);
    while let Some(message) = commands.recv().await {
        match message.command {
            SessionCommand::SubmitPrompt { input } => {
                let mut owned_app = match app.take() {
                    Some(app) => app,
                    None => match build_worker_app(&plan) {
                        Ok(app) => app,
                        Err(_) => {
                            let _ = message.response.send(Err(ServiceError::Internal));
                            continue;
                        }
                    },
                };
                let session_path = owned_app.agent.session().path().to_owned();
                plan.launch.session = SessionSelection::OpenExisting(session_path);
                match start_and_drive_run(
                    &mut owned_app,
                    input,
                    &plan,
                    &mut projection,
                    &mut commands,
                    &events,
                    message.response,
                )
                .await
                {
                    Ok(()) => app = Some(owned_app),
                    Err(_) => {
                        let _ = events
                            .send(event(EventPayload::SessionStateChanged {
                                state: SessionLiveState::Failed,
                                active_run_id: None,
                            }))
                            .await;
                        app = Some(owned_app);
                    }
                }
            }
            SessionCommand::ChangeModel { provider, model } => {
                let Some(summary) = plan
                    .available_models
                    .iter()
                    .find(|summary| summary.provider == provider && summary.id == model)
                    .cloned()
                else {
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                };
                let outcome = if let Some(owned_app) = app.take() {
                    match crate::app::apply_reconfig(
                        owned_app,
                        Reconfig::Model(ModelId(model.clone())),
                    ) {
                        Ok(rebuilt) => {
                            plan.launch.model = rebuilt.model.spec.id.clone();
                            plan.launch.reasoning = rebuilt.reasoning.clone();
                            plan.launch.session = SessionSelection::OpenExisting(
                                rebuilt.agent.session().path().to_owned(),
                            );
                            let selection = selection_for_model(
                                &rebuilt.model,
                                &rebuilt.reasoning,
                                &plan.config,
                            );
                            let outcome = reconfiguration_outcome(
                                &rebuilt,
                                &plan,
                                &mut projection,
                                selection,
                                plan.authority,
                            );
                            app = Some(rebuilt);
                            outcome
                        }
                        Err(_) => {
                            app = build_worker_app(&plan).ok();
                            Err(ServiceError::Internal)
                        }
                    }
                } else {
                    let next_reasoning_label = summary
                        .default_reasoning
                        .clone()
                        .or_else(|| summary.reasoning.first().cloned())
                        .unwrap_or_else(|| "off".into());
                    let next_reasoning = config::parse_reasoning(&next_reasoning_label)
                        .unwrap_or(ReasoningConfig::Off);
                    let previous_model = plan.launch.model.clone();
                    let previous_reasoning = plan.launch.reasoning.clone();
                    plan.launch.model = ModelId(model);
                    plan.launch.reasoning = next_reasoning;
                    let selection = ModelSelection {
                        provider,
                        model: plan.launch.model.0.clone(),
                        reasoning: next_reasoning_label,
                    };
                    match persist_idle_selection(&mut plan, &mut projection, selection) {
                        Ok(outcome) => Ok(outcome),
                        Err(error) => {
                            plan.launch.model = previous_model;
                            plan.launch.reasoning = previous_reasoning;
                            Err(error)
                        }
                    }
                };
                let _ = message.response.send(outcome);
            }
            SessionCommand::ChangeReasoning { reasoning } => {
                let Some(summary) = plan
                    .available_models
                    .iter()
                    .find(|summary| summary.id == plan.launch.model.0)
                    .cloned()
                else {
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                };
                if !summary.reasoning.iter().any(|choice| choice == &reasoning) {
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                }
                let provider = summary.provider.clone();
                let parsed = match config::parse_reasoning(&reasoning) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                        continue;
                    }
                };
                let outcome = if let Some(owned_app) = app.take() {
                    match crate::app::apply_reconfig(owned_app, Reconfig::Thinking(parsed.clone()))
                    {
                        Ok(rebuilt) => {
                            plan.launch.model = rebuilt.model.spec.id.clone();
                            plan.launch.reasoning = rebuilt.reasoning.clone();
                            plan.launch.session = SessionSelection::OpenExisting(
                                rebuilt.agent.session().path().to_owned(),
                            );
                            let selection = selection_for_model(
                                &rebuilt.model,
                                &rebuilt.reasoning,
                                &plan.config,
                            );
                            let outcome = reconfiguration_outcome(
                                &rebuilt,
                                &plan,
                                &mut projection,
                                selection,
                                plan.authority,
                            );
                            app = Some(rebuilt);
                            outcome
                        }
                        Err(_) => {
                            app = build_worker_app(&plan).ok();
                            Err(ServiceError::Internal)
                        }
                    }
                } else {
                    let previous_reasoning = plan.launch.reasoning.clone();
                    plan.launch.reasoning = parsed;
                    let selection = ModelSelection {
                        provider,
                        model: plan.launch.model.0.clone(),
                        reasoning,
                    };
                    match persist_idle_selection(&mut plan, &mut projection, selection) {
                        Ok(outcome) => Ok(outcome),
                        Err(error) => {
                            plan.launch.reasoning = previous_reasoning;
                            Err(error)
                        }
                    }
                };
                let _ = message.response.send(outcome);
            }
            SessionCommand::SetAuthority { authority }
                if authority == AuthorityProfile::FullAccess =>
            {
                let selection = current_selection(&plan);
                let _ = message
                    .response
                    .send(Ok(DriverCommandOutcome::with_events(vec![event(
                        EventPayload::SessionSettingsChanged {
                            model: selection,
                            authority,
                        },
                    )])));
            }
            SessionCommand::Rename { title } => {
                let _ = message.response.send(rename_session_outcome(&plan, &title));
            }
            SessionCommand::SetPinned { pinned } => {
                let _ = message.response.send(pin_session_outcome(&plan, pinned));
            }
            SessionCommand::SetArchived { archived } => {
                let _ = message
                    .response
                    .send(archive_session_outcome(&plan, archived));
            }
            _ => {
                let _ = message.response.send(Err(ServiceError::InvalidBoundary));
            }
        }
    }
    shutdown_worker_app(&mut app).await;
}

fn reconfiguration_outcome(
    app: &App,
    plan: &WorkerPlan,
    projection: &mut ProjectionState,
    selection: ModelSelection,
    authority: AuthorityProfile,
) -> Result<DriverCommandOutcome, ServiceError> {
    let mut events = vec![event(EventPayload::SessionSettingsChanged {
        model: selection,
        authority,
    })];
    for item in project_new_entries(
        app.agent.session(),
        projection,
        None,
        plan.attachments.as_ref(),
        &plan.session_id,
    )? {
        events.push(event(EventPayload::ItemCommitted { item }));
    }
    let durable_entry_id = app
        .agent
        .session()
        .head()
        .map(|head| DurableEntryId::new(head.0))
        .transpose()
        .map_err(|_| ServiceError::Internal)?;
    events.push(event(EventPayload::SessionDurableHeadChanged {
        durable_entry_id,
    }));
    Ok(DriverCommandOutcome::with_events(events))
}

fn persist_idle_selection(
    plan: &mut WorkerPlan,
    projection: &mut ProjectionState,
    selection: ModelSelection,
) -> Result<DriverCommandOutcome, ServiceError> {
    let (path, session, newly_created) = match &plan.launch.session {
        SessionSelection::CreateNew(path) => (
            path.clone(),
            Session::create(path).map_err(|_| ServiceError::Internal)?,
            true,
        ),
        SessionSelection::OpenExisting(path) => (
            path.clone(),
            Session::open(path).map_err(|_| ServiceError::Internal)?,
            false,
        ),
    };
    let mut session = session;
    let append = session.append(EntryValue::Config {
        model: Some(plan.launch.model.0.clone()),
        reasoning: Some(reasoning_label(&plan.launch.reasoning)),
        reasoning_mode: Some(
            match plan.launch.reasoning_mode {
                ygg_ai::ReasoningMode::Standard => "standard",
                ygg_ai::ReasoningMode::Pro => "pro",
            }
            .to_owned(),
        ),
    });
    if append.is_err() {
        drop(session);
        if newly_created {
            let _ = std::fs::remove_file(&path);
        }
        return Err(ServiceError::Internal);
    }
    projection.known_entries = session.entries().len();
    let durable_entry_id = session
        .head()
        .map(|head| DurableEntryId::new(head.0))
        .transpose()
        .map_err(|_| ServiceError::Internal)?;
    plan.launch.session = SessionSelection::OpenExisting(path);
    Ok(DriverCommandOutcome::with_events(vec![
        event(EventPayload::SessionSettingsChanged {
            model: selection,
            authority: plan.authority,
        }),
        event(EventPayload::SessionDurableHeadChanged { durable_entry_id }),
    ]))
}

fn rename_session_outcome(
    plan: &WorkerPlan,
    title: &str,
) -> Result<DriverCommandOutcome, ServiceError> {
    ensure_durable_session(plan)?;
    let metadata = plan
        .sessions
        .rename(plan.session_id.as_str(), title)
        .map_err(|_| ServiceError::InvalidBoundary)?;
    let title = metadata.name.ok_or(ServiceError::InvalidBoundary)?;
    Ok(session_metadata_outcome(Some(title), None, None))
}

fn pin_session_outcome(
    plan: &WorkerPlan,
    pinned: bool,
) -> Result<DriverCommandOutcome, ServiceError> {
    ensure_durable_session(plan)?;
    plan.sessions
        .set_pinned(plan.session_id.as_str(), pinned)
        .map_err(|_| ServiceError::Internal)?;
    Ok(session_metadata_outcome(None, Some(pinned), None))
}

fn archive_session_outcome(
    plan: &WorkerPlan,
    archived: bool,
) -> Result<DriverCommandOutcome, ServiceError> {
    ensure_durable_session(plan)?;
    plan.sessions
        .set_archived(plan.session_id.as_str(), archived)
        .map_err(|_| ServiceError::Internal)?;
    Ok(session_metadata_outcome(None, None, Some(archived)))
}

fn ensure_durable_session(plan: &WorkerPlan) -> Result<(), ServiceError> {
    match &plan.launch.session {
        SessionSelection::OpenExisting(path) if path.is_file() => Ok(()),
        SessionSelection::CreateNew(_) | SessionSelection::OpenExisting(_) => {
            Err(ServiceError::InvalidBoundary)
        }
    }
}

fn session_metadata_outcome(
    title: Option<String>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> DriverCommandOutcome {
    DriverCommandOutcome::with_events(vec![event(EventPayload::SessionMetadataChanged {
        title,
        pinned,
        archived,
    })])
}

async fn shutdown_worker_app(app: &mut Option<App>) {
    if let Some(mut app) = app.take() {
        app.executable_extensions.shutdown().await;
    }
}

fn build_worker_app(plan: &WorkerPlan) -> anyhow::Result<App> {
    let mut config = plan.config.clone();
    config.resume = match &plan.launch.session {
        SessionSelection::CreateNew(_) => crate::config::ResumeSelector::New,
        SessionSelection::OpenExisting(path) => crate::config::ResumeSelector::Resume(
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned),
        ),
    };
    let boot = crate::app::bootstrap::bootstrap(config)?;
    let system = compose_instructions(&boot.config)?;
    build_app(boot, plan.launch.clone(), system)
}

fn resolve_attachment_media(
    app: &App,
    plan: &WorkerPlan,
    references: &[AttachmentRef],
) -> Result<Vec<Media>, ServiceError> {
    let supports_images = app
        .model
        .spec
        .capabilities
        .input_modalities
        .contains(Modality::Image);
    resolve_stored_media(supports_images, plan.attachments.as_ref(), references)
}

fn resolve_stored_media(
    supports_images: bool,
    store: Option<&AttachmentStore>,
    references: &[AttachmentRef],
) -> Result<Vec<Media>, ServiceError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    if !supports_images {
        return Err(ServiceError::InvalidBoundary);
    }
    let store = store.ok_or(ServiceError::Unavailable)?;
    let resolved = store
        .resolve_many(references)
        .map_err(attachment_service_error)?;
    resolved
        .into_iter()
        .map(|attachment| {
            let media_type = attachment
                .reference
                .media_type
                .parse()
                .map_err(|_| ServiceError::InvalidBoundary)?;
            Ok(Media::image_bytes(attachment.bytes, media_type))
        })
        .collect()
}

fn resolve_control_input(
    plan: &WorkerPlan,
    text: String,
    references: &[AttachmentRef],
) -> Result<UserInput, ServiceError> {
    let mut parts = Vec::with_capacity(1 + references.len());
    if !text.is_empty() {
        parts.push(InputPart::Text(text));
    }
    let supports_images = plan
        .available_models
        .iter()
        .find(|summary| summary.id == plan.launch.model.0)
        .is_some_and(|summary| summary.input_modalities.contains(&InputModality::Image));
    parts.extend(
        resolve_stored_media(supports_images, plan.attachments.as_ref(), references)?
            .into_iter()
            .map(InputPart::Media),
    );
    Ok(UserInput::from(parts))
}

fn attachment_service_error(error: AttachmentError) -> ServiceError {
    match error {
        AttachmentError::Unavailable | AttachmentError::QuotaExceeded => ServiceError::Unavailable,
        AttachmentError::Storage => ServiceError::Internal,
        AttachmentError::InvalidName
        | AttachmentError::UnsupportedMediaType
        | AttachmentError::InvalidContent
        | AttachmentError::TooLarge
        | AttachmentError::NotFound
        | AttachmentError::MetadataMismatch => ServiceError::InvalidBoundary,
    }
}

async fn start_and_drive_run(
    app: &mut App,
    input: PromptInput,
    plan: &WorkerPlan,
    projection: &mut ProjectionState,
    commands: &mut mpsc::Receiver<WorkerCommand>,
    events: &mpsc::Sender<TimestampedEvent>,
    admission: oneshot::Sender<Result<DriverCommandOutcome, ServiceError>>,
) -> Result<(), ServiceError> {
    if let Some(limit) = app.config.max_cost_microdollars {
        if app.agent.session().total_cost_microdollars() >= limit {
            let _ = admission.send(Err(ServiceError::InvalidBoundary));
            return Ok(());
        }
    }
    let PromptInput {
        text: prompt,
        attachments,
    } = input;
    let media = match resolve_attachment_media(app, plan, &attachments) {
        Ok(media) => media,
        Err(error) => {
            let _ = admission.send(Err(error));
            return Ok(());
        }
    };
    let prompt = match crate::prompts::render_configured(app, &prompt)
        .map_err(|_| ServiceError::Internal)?
    {
        Some(rendered) => rendered.text,
        None => prompt,
    };
    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    let composition = app
        .executable_extensions
        .compose_prompt(&app.system, prompt.clone())
        .await
        .map_err(|_| ServiceError::Internal)?;
    let pending_context_count = composition.pending_context_count;
    let model_prompt = composition.prompt;
    app.agent.set_system_prompt(composition.system);
    app.agent.set_prompt_display_text(Some(prompt.clone()));
    projection.begin_run();
    let run_id = projection.next_run_id(plan.actor_generation)?;
    let turn_id = projection.turn_id(&run_id)?;
    let user_item_id = projection.provisional_id(&run_id, "user", 0)?;
    let mut input_parts = Vec::with_capacity(1 + media.len());
    if !model_prompt.is_empty() {
        input_parts.push(InputPart::Text(model_prompt));
    }
    input_parts.extend(media.into_iter().map(InputPart::Media));
    let mut run = match app.agent.prompt(UserInput::from(input_parts)).await {
        Ok(run) => run,
        Err(_) => {
            let _ = admission.send(Err(ServiceError::Internal));
            return Ok(());
        }
    };
    if !attachments.is_empty() {
        projection
            .pending_attachments
            .push_back(attachments.clone());
    }
    projection
        .pending_user_items
        .push_back(user_item_id.clone());
    app.executable_extensions
        .commit_prompt_context(pending_context_count);
    let control = run.control();
    let context_limit = app.model.spec.limits.context_window;
    let immediate = vec![
        event(EventPayload::ItemStarted {
            item: SessionItem {
                id: user_item_id.clone(),
                run_id: Some(run_id.clone()),
                turn_id: Some(turn_id),
                provider_attempt: None,
                lifecycle: ItemLifecycle::Provisional,
                durable_entry_id: None,
                payload: ItemPayload::UserMessage {
                    text: bounded_text(&prompt, MAX_PROMPT_BYTES),
                    attachments: attachments.clone(),
                },
            },
        }),
        event(EventPayload::SessionStateChanged {
            state: SessionLiveState::Working,
            active_run_id: Some(run_id.clone()),
        }),
    ];
    if admission
        .send(Ok(DriverCommandOutcome::run(run_id.clone(), immediate)))
        .is_err()
    {
        control.abort();
    }

    let mut response_text = String::new();
    let mut completed = false;
    let terminal;
    loop {
        tokio::select! {
            event = run.next() => {
                let Some(agent_event) = event else {
                    terminal = TerminalProjection::failed("The run stream ended unexpectedly.");
                    break;
                };
                if let Some(outcome) = project_agent_event(
                    agent_event,
                    &run_id,
                    context_limit,
                    plan,
                    projection,
                    events,
                    &mut response_text,
                )
                .await?
                {
                    completed = outcome.state == SessionLiveState::Done;
                    terminal = outcome;
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else {
                    control.abort();
                    terminal = TerminalProjection::stopped();
                    break;
                };
                handle_active_command(command, &run_id, &control, plan, projection, events).await;
            }
        }
    }
    drop(run);
    app.agent.set_system_prompt(app.system.clone());
    app.agent
        .record_run_outcome(SessionRunOutcome {
            status: match terminal.outcome {
                ygg_serve_backend::RunOutcome::Completed => SessionRunOutcomeStatus::Completed,
                ygg_serve_backend::RunOutcome::Stopped => SessionRunOutcomeStatus::Stopped,
                ygg_serve_backend::RunOutcome::Failed => SessionRunOutcomeStatus::Failed,
            },
            message: terminal.message.clone(),
        })
        .map_err(|_| ServiceError::Internal)?;

    let committed = project_new_entries(
        app.agent.session(),
        projection,
        Some(&run_id),
        plan.attachments.as_ref(),
        &plan.session_id,
    )?;
    for item in committed {
        events
            .send(event(EventPayload::ItemCommitted { item }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    for item_id in projection.pending_user_items.drain(..) {
        events
            .send(event(EventPayload::ItemRetracted {
                item_id,
                provider_attempt: 1,
                reason: "Input was not delivered before the run ended.".into(),
            }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    projection.pending_attachments.clear();
    let durable_entry_id = app
        .agent
        .session()
        .head()
        .map(|head| DurableEntryId::new(head.0))
        .transpose()
        .map_err(|_| ServiceError::Internal)?;
    events
        .send(event(EventPayload::SessionDurableHeadChanged {
            durable_entry_id,
        }))
        .await
        .map_err(|_| ServiceError::Unavailable)?;
    expire_private_requests(projection, events, plan.actor_generation).await?;
    events
        .send(event(EventPayload::SessionStateChanged {
            state: terminal.state,
            active_run_id: None,
        }))
        .await
        .map_err(|_| ServiceError::Unavailable)?;
    if completed {
        let _ = app
            .executable_extensions
            .after_response(&response_text)
            .await;
    }
    Ok(())
}

async fn handle_active_command(
    message: WorkerCommand,
    run_id: &RunId,
    control: &RunControl,
    plan: &WorkerPlan,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) {
    let outcome = match message.command {
        SessionCommand::Steer { input } => {
            let PromptInput {
                text,
                attachments: references,
            } = input;
            match resolve_control_input(plan, text.clone(), &references) {
                Ok(input) => match control.steer(input).await {
                    Ok(()) => {
                        publish_control_user_item(run_id, text, references, projection, events)
                            .await
                    }
                    Err(_) => Err(ServiceError::InvalidBoundary),
                },
                Err(error) => Err(error),
            }
        }
        SessionCommand::FollowUp { input } => {
            let PromptInput {
                text,
                attachments: references,
            } = input;
            match resolve_control_input(plan, text.clone(), &references) {
                Ok(input) => match control.follow_up(input).await {
                    Ok(()) => {
                        publish_control_user_item(run_id, text, references, projection, events)
                            .await
                    }
                    Err(_) => Err(ServiceError::InvalidBoundary),
                },
                Err(error) => Err(error),
            }
        }
        SessionCommand::Rename { title } => rename_session_outcome(plan, &title),
        SessionCommand::SetPinned { pinned } => pin_session_outcome(plan, pinned),
        SessionCommand::SetArchived { archived } => archive_session_outcome(plan, archived),
        SessionCommand::Abort { run_id: expected }
            if expected.as_ref().is_none_or(|expected| expected == run_id) =>
        {
            control.abort();
            Ok(DriverCommandOutcome::default())
        }
        SessionCommand::AnswerRequest { request_id, answer } => {
            match projection.private_requests.remove(&request_id) {
                Some(PrivateRequest {
                    kind,
                    response: PrivateResponse::Approval(respond),
                }) => {
                    let (allowed, state) = match answer {
                        RequestAnswer::Approval { allowed } => (
                            allowed,
                            if allowed {
                                RequestState::Resolved
                            } else {
                                RequestState::Denied
                            },
                        ),
                        _ => {
                            projection.private_requests.insert(
                                request_id,
                                PrivateRequest {
                                    kind,
                                    response: PrivateResponse::Approval(respond),
                                },
                            );
                            let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                            return;
                        }
                    };
                    respond(allowed);
                    let changed = PendingRequest {
                        id: request_id,
                        actor_generation: projection_actor_generation(run_id),
                        kind,
                        state,
                    };
                    let _ = events
                        .send(event(EventPayload::PendingRequestChanged {
                            request: changed,
                        }))
                        .await;
                    let _ = events
                        .send(event(EventPayload::SessionStateChanged {
                            state: SessionLiveState::Working,
                            active_run_id: Some(run_id.clone()),
                        }))
                        .await;
                    Ok(DriverCommandOutcome::default())
                }
                Some(PrivateRequest {
                    kind,
                    response: PrivateResponse::Input(respond),
                }) => {
                    let answer = match answer {
                        RequestAnswer::Text { text } => text,
                        RequestAnswer::Choice { choice } => choice,
                        _ => {
                            projection.private_requests.insert(
                                request_id,
                                PrivateRequest {
                                    kind,
                                    response: PrivateResponse::Input(respond),
                                },
                            );
                            let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                            return;
                        }
                    };
                    respond(Some(answer.into_bytes()));
                    let changed = PendingRequest {
                        id: request_id,
                        actor_generation: projection_actor_generation(run_id),
                        kind,
                        state: RequestState::Resolved,
                    };
                    let _ = events
                        .send(event(EventPayload::PendingRequestChanged {
                            request: changed,
                        }))
                        .await;
                    let _ = events
                        .send(event(EventPayload::SessionStateChanged {
                            state: SessionLiveState::Working,
                            active_run_id: Some(run_id.clone()),
                        }))
                        .await;
                    Ok(DriverCommandOutcome::default())
                }
                None => Err(ServiceError::InvalidBoundary),
            }
        }
        _ => Err(ServiceError::InvalidBoundary),
    };
    let _ = message.response.send(outcome);
}

async fn publish_control_user_item(
    run_id: &RunId,
    text: String,
    attachments: Vec<AttachmentRef>,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) -> Result<DriverCommandOutcome, ServiceError> {
    let item_id = projection.next_user_item_id(run_id)?;
    let turn_id = projection.turn_id(run_id)?;
    projection.pending_user_items.push_back(item_id.clone());
    if !attachments.is_empty() {
        projection
            .pending_attachments
            .push_back(attachments.clone());
    }
    events
        .send(event(EventPayload::ItemStarted {
            item: SessionItem {
                id: item_id,
                run_id: Some(run_id.clone()),
                turn_id: Some(turn_id),
                provider_attempt: None,
                lifecycle: ItemLifecycle::Provisional,
                durable_entry_id: None,
                payload: ItemPayload::UserMessage {
                    text: bounded_text(&text, MAX_PROMPT_BYTES),
                    attachments,
                },
            },
        }))
        .await
        .map_err(|_| ServiceError::Unavailable)?;
    Ok(DriverCommandOutcome::default())
}

fn projection_actor_generation(run_id: &RunId) -> u64 {
    run_id
        .as_str()
        .split('-')
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(1)
}

async fn project_agent_event(
    agent_event: AgentEvent,
    run_id: &RunId,
    context_limit: u64,
    plan: &WorkerPlan,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
    response_text: &mut String,
) -> Result<Option<TerminalProjection>, ServiceError> {
    match agent_event {
        AgentEvent::OutputDelta { channel, text } => {
            let text = bounded_text(&text, MAX_ITEM_TEXT_BYTES);
            let turn_id = projection.turn_id(run_id)?;
            let (slot, kind, payload, delta) = match channel {
                OutputChannel::Text => (
                    &mut projection.assistant_item,
                    "assistant",
                    ItemPayload::AssistantMessage {
                        text: String::new(),
                    },
                    ItemDelta::AssistantText {
                        append: text.clone(),
                    },
                ),
                OutputChannel::Reasoning => (
                    &mut projection.reasoning_item,
                    "reasoning",
                    ItemPayload::Reasoning {
                        text: String::new(),
                    },
                    ItemDelta::ReasoningText {
                        append: text.clone(),
                    },
                ),
            };
            if let Some(item_id) = slot.clone() {
                events
                    .send(event(EventPayload::ItemDelta { item_id, delta }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
            } else {
                let item_id = ItemId::new(format!(
                    "item-{}-{kind}-{}-{}",
                    run_id.as_str(),
                    projection.turn_counter,
                    projection.provider_attempt
                ))
                .map_err(|_| ServiceError::Internal)?;
                let payload = match payload {
                    ItemPayload::AssistantMessage { .. } => ItemPayload::AssistantMessage { text },
                    ItemPayload::Reasoning { .. } => ItemPayload::Reasoning { text },
                    _ => unreachable!(),
                };
                events
                    .send(event(EventPayload::ItemStarted {
                        item: SessionItem {
                            id: item_id.clone(),
                            run_id: Some(run_id.clone()),
                            turn_id: Some(turn_id),
                            provider_attempt: Some(projection.provider_attempt),
                            lifecycle: ItemLifecycle::Provisional,
                            durable_entry_id: None,
                            payload,
                        },
                    }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
                *slot = Some(item_id);
            }
        }
        AgentEvent::ProviderRetry { .. } | AgentEvent::CandidateRejected { .. } => {
            retract_attempt(run_id, projection, events).await?;
            projection.provider_attempt = projection.provider_attempt.saturating_add(1);
        }
        AgentEvent::ToolStarted { id, name, args } => {
            let item_id =
                projection.provisional_id(run_id, "tool", projection.tool_items.len() as u64)?;
            projection.tool_items.insert(id.0.clone(), item_id.clone());
            let arguments = if ygg_serve_backend::validate_json("tool.arguments", &args, 256 * 1024)
                .is_ok()
            {
                args
            } else {
                serde_json::json!({"unavailable": "arguments exceeded the graphical projection limit"})
            };
            projection.tool_calls.insert(
                id.0.clone(),
                ProjectedToolCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            );
            events
                .send(event(EventPayload::ItemStarted {
                    item: SessionItem {
                        id: item_id,
                        run_id: Some(run_id.clone()),
                        turn_id: Some(projection.turn_id(run_id)?),
                        provider_attempt: Some(projection.provider_attempt),
                        lifecycle: ItemLifecycle::Provisional,
                        durable_entry_id: None,
                        payload: ItemPayload::ToolCall {
                            name: bounded_text(&name, 128),
                            arguments,
                            progress: None,
                            dropped_progress_bytes: 0,
                        },
                    },
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
        }
        AgentEvent::ToolProgress { id, progress } => {
            project_tool_progress(id, progress, run_id, projection, events).await?;
        }
        AgentEvent::ToolFinished { id, result } => {
            if let Some(item_id) = projection.tool_items.get(&id.0).cloned() {
                events
                    .send(event(EventPayload::ItemDelta {
                        item_id,
                        delta: ItemDelta::ToolProgress {
                            text: "Finished".into(),
                            dropped_bytes: projection
                                .tool_progress
                                .get(&id.0)
                                .map(|(_, dropped)| *dropped)
                                .unwrap_or_default(),
                        },
                    }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
            }
            if let (Some(tool), Some(tool_item_id), Ok(output)) = (
                projection.tool_calls.get(&id.0),
                projection.tool_items.get(&id.0),
                result.as_ref(),
            ) {
                for payload in project_tool_evidence(
                    &plan.config.workspace,
                    &plan.resources,
                    &plan.session_id,
                    run_id,
                    &projection.turn_id(run_id)?,
                    &id.0,
                    tool_item_id,
                    tool,
                    output,
                ) {
                    events
                        .send(event(payload))
                        .await
                        .map_err(|_| ServiceError::Unavailable)?;
                }
            }
        }
        AgentEvent::TurnFinished {
            message,
            turn_usage,
            ..
        } => {
            response_text.clear();
            response_text.push_str(&super::assistant_text(&message));
            events
                .send(event(EventPayload::UsageUpdated {
                    usage: UsageSnapshot {
                        input_tokens: turn_usage
                            .input_tokens
                            .saturating_add(turn_usage.cache_read_tokens)
                            .saturating_add(turn_usage.cache_write_tokens),
                        output_tokens: turn_usage.output_tokens,
                        context_tokens: turn_usage.total_tokens,
                        context_limit: Some(context_limit),
                    },
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
            projection.finish_turn();
        }
        AgentEvent::RunFinished { reason, .. } => {
            let terminal = match reason {
                FinishReason::Completed => TerminalProjection::completed(),
                FinishReason::Aborted => TerminalProjection::stopped(),
                FinishReason::Failed(_) => TerminalProjection::failed("The run failed."),
                FinishReason::MaxTurns => {
                    TerminalProjection::failed("The maximum model-turn limit was reached.")
                }
            };
            return Ok(Some(terminal));
        }
        AgentEvent::SteeringDelivered { .. }
        | AgentEvent::CompactionStarted { .. }
        | AgentEvent::CompactionFinished { .. } => {}
    }
    Ok(None)
}

struct WorkspaceFileSnapshot {
    display_path: String,
    display_name: String,
    bytes: bytes::Bytes,
    media_type: &'static str,
    artifact_kind: ArtifactKind,
}

#[allow(clippy::too_many_arguments)]
fn project_tool_evidence(
    workspace: &Path,
    resources: &OpaqueResourceRegistry,
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    tool_call_id: &str,
    tool_item_id: &ItemId,
    tool: &ProjectedToolCall,
    output: &ToolOutput,
) -> Vec<EventPayload> {
    let identity =
        stable_hash(format!("{}\0{}\0{}", session_id.as_str(), tool_call_id, tool.name).as_bytes());
    let Some(short_identity) = identity.get(..24) else {
        return Vec::new();
    };

    match tool.name.as_str() {
        "read" => {
            let Some(path) = tool
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
            else {
                return Vec::new();
            };
            let Some(snapshot) = snapshot_workspace_file(workspace, path) else {
                return Vec::new();
            };
            let Ok((handle, _)) =
                resources.register(&snapshot.display_name, snapshot.media_type, snapshot.bytes)
            else {
                return Vec::new();
            };
            let Ok(id) = SourceId::new(format!("source-{short_identity}")) else {
                return Vec::new();
            };
            let source = SourceRef {
                id,
                kind: SourceKind::File,
                title: snapshot.display_path,
                handle,
                origin_item_id: Some(tool_item_id.clone()),
                consulted_at_ms: now_ms(),
                cited: false,
                available: true,
            };
            let Some(item) = provisional_evidence_item(
                format!("item-source-{short_identity}"),
                run_id,
                turn_id,
                ItemPayload::Source(source.clone()),
            ) else {
                return Vec::new();
            };
            vec![
                EventPayload::SourceUpserted {
                    source: source.clone(),
                },
                EventPayload::ItemStarted { item },
            ]
        }
        "read_skill_resource" => {
            let title = tool
                .arguments
                .get("resource_path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("skill resource");
            let Ok((handle, _)) = resources.register(
                title,
                "text/plain",
                bytes::Bytes::copy_from_slice(output.text.as_bytes()),
            ) else {
                return Vec::new();
            };
            let Ok(id) = SourceId::new(format!("source-{short_identity}")) else {
                return Vec::new();
            };
            let source = SourceRef {
                id,
                kind: SourceKind::Resource,
                title: bounded_text(title, 512),
                handle,
                origin_item_id: Some(tool_item_id.clone()),
                consulted_at_ms: now_ms(),
                cited: false,
                available: true,
            };
            let Some(item) = provisional_evidence_item(
                format!("item-source-{short_identity}"),
                run_id,
                turn_id,
                ItemPayload::Source(source.clone()),
            ) else {
                return Vec::new();
            };
            vec![
                EventPayload::SourceUpserted {
                    source: source.clone(),
                },
                EventPayload::ItemStarted { item },
            ]
        }
        "edit" | "write" => {
            let Some(path) = tool
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
            else {
                return Vec::new();
            };
            let Some(snapshot) = snapshot_workspace_file(workspace, path) else {
                return Vec::new();
            };
            let byte_len = snapshot.bytes.len() as u64;
            let Ok((handle, stored)) =
                resources.register(&snapshot.display_name, snapshot.media_type, snapshot.bytes)
            else {
                return Vec::new();
            };
            let Ok(id) = ArtifactId::new(format!("artifact-{short_identity}")) else {
                return Vec::new();
            };
            let artifact = ArtifactRef {
                id,
                kind: snapshot.artifact_kind,
                name: snapshot.display_name,
                media_type: snapshot.media_type.to_owned(),
                handle: handle.clone(),
                byte_len,
                content_hash: Some(stored.sha256),
                origin_item_id: Some(tool_item_id.clone()),
                available: true,
            };
            let (additions, deletions) = if tool.name == "edit" {
                (
                    line_count(
                        tool.arguments
                            .get("new")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    ),
                    line_count(
                        tool.arguments
                            .get("old")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default(),
                    ),
                )
            } else {
                // A write may create or replace. The structured event does not
                // retain the pre-write file, so claiming a diff would be false.
                (0, 0)
            };
            let file_change = FileChange {
                handle,
                display_path: snapshot.display_path,
                additions,
                deletions,
            };
            let Some(change_item) = provisional_evidence_item(
                format!("item-file-change-{short_identity}"),
                run_id,
                turn_id,
                ItemPayload::FileChange(file_change),
            ) else {
                return Vec::new();
            };
            let Some(artifact_item) = provisional_evidence_item(
                format!("item-artifact-{short_identity}"),
                run_id,
                turn_id,
                ItemPayload::Artifact(artifact.clone()),
            ) else {
                return Vec::new();
            };
            vec![
                EventPayload::ItemStarted { item: change_item },
                EventPayload::ArtifactUpserted {
                    artifact: artifact.clone(),
                },
                EventPayload::ItemStarted {
                    item: artifact_item,
                },
            ]
        }
        _ => Vec::new(),
    }
}

fn provisional_evidence_item(
    id: String,
    run_id: &RunId,
    turn_id: &TurnId,
    payload: ItemPayload,
) -> Option<SessionItem> {
    Some(SessionItem {
        id: ItemId::new(id).ok()?,
        run_id: Some(run_id.clone()),
        turn_id: Some(turn_id.clone()),
        provider_attempt: None,
        lifecycle: ItemLifecycle::Provisional,
        durable_entry_id: None,
        payload,
    })
}

fn snapshot_workspace_file(workspace: &Path, requested: &str) -> Option<WorkspaceFileSnapshot> {
    let workspace = workspace.canonicalize().ok()?;
    let requested_path = if requested.contains("://") || requested.starts_with("file:") {
        let url = url::Url::parse(requested).ok()?;
        if url.scheme() != "file"
            || url.fragment().is_some()
            || (!url.username().is_empty() || url.password().is_some())
            || !matches!(url.host_str(), None | Some("") | Some("localhost"))
        {
            return None;
        }
        url.to_file_path().ok()?
    } else {
        PathBuf::from(requested)
    };
    let candidate = if requested_path.is_absolute() {
        requested_path
    } else {
        workspace.join(requested_path)
    };
    let link_metadata = candidate.symlink_metadata().ok()?;
    if link_metadata.file_type().is_symlink() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if canonical == workspace || !canonical.starts_with(&workspace) {
        return None;
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&canonical).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_OPAQUE_RESOURCE_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_OPAQUE_RESOURCE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_OPAQUE_RESOURCE_BYTES {
        return None;
    }
    let relative = canonical.strip_prefix(&workspace).ok()?;
    let display_path = bounded_text(&relative.to_string_lossy().replace('\\', "/"), 512);
    let display_name = canonical.file_name()?.to_str()?.to_owned();
    let extension = canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some(WorkspaceFileSnapshot {
        display_path,
        display_name,
        media_type: if std::str::from_utf8(&bytes).is_ok() {
            "text/plain"
        } else {
            "application/octet-stream"
        },
        artifact_kind: artifact_kind_for_extension(&extension),
        bytes: bytes::Bytes::from(bytes),
    })
}

fn artifact_kind_for_extension(extension: &str) -> ArtifactKind {
    match extension {
        "png" | "jpg" | "jpeg" | "gif" | "webp" => ArtifactKind::Image,
        "pdf" | "doc" | "docx" | "md" | "txt" => ArtifactKind::Document,
        "csv" | "tsv" | "xls" | "xlsx" => ArtifactKind::Spreadsheet,
        "ppt" | "pptx" | "key" => ArtifactKind::Presentation,
        "html" | "htm" => ArtifactKind::Site,
        "rs" | "js" | "jsx" | "ts" | "tsx" | "css" | "json" | "toml" | "yaml" | "yml" | "py"
        | "go" | "java" | "kt" | "swift" | "c" | "h" | "cpp" | "hpp" | "sh" => ArtifactKind::File,
        _ => ArtifactKind::Other,
    }
}

fn line_count(value: &str) -> u32 {
    value.lines().count().min(u32::MAX as usize) as u32
}

struct TerminalProjection {
    state: SessionLiveState,
    outcome: ygg_serve_backend::RunOutcome,
    message: Option<String>,
}

impl TerminalProjection {
    fn completed() -> Self {
        Self {
            state: SessionLiveState::Done,
            outcome: ygg_serve_backend::RunOutcome::Completed,
            message: None,
        }
    }

    fn stopped() -> Self {
        Self {
            state: SessionLiveState::Stopped,
            outcome: ygg_serve_backend::RunOutcome::Stopped,
            message: None,
        }
    }

    fn failed(message: &str) -> Self {
        Self {
            state: SessionLiveState::Failed,
            outcome: ygg_serve_backend::RunOutcome::Failed,
            message: Some(message.into()),
        }
    }
}

async fn expire_private_requests(
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
    actor_generation: u64,
) -> Result<(), ServiceError> {
    for (id, request) in projection.private_requests.drain() {
        match request.response {
            PrivateResponse::Approval(respond) => respond(false),
            PrivateResponse::Input(respond) => respond(None),
        }
        events
            .send(event(EventPayload::PendingRequestChanged {
                request: PendingRequest {
                    id,
                    actor_generation,
                    kind: request.kind,
                    state: RequestState::Expired,
                },
            }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    projection.tool_items.clear();
    projection.tool_calls.clear();
    projection.tool_progress.clear();
    Ok(())
}

async fn retract_attempt(
    _run_id: &RunId,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) -> Result<(), ServiceError> {
    for item_id in [
        projection.assistant_item.take(),
        projection.reasoning_item.take(),
    ]
    .into_iter()
    .flatten()
    {
        events
            .send(event(EventPayload::ItemRetracted {
                item_id,
                provider_attempt: projection.provider_attempt,
                reason: "The provider attempt was replaced before commit.".into(),
            }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    Ok(())
}

async fn project_tool_progress(
    id: ToolCallId,
    progress: ToolProgress,
    run_id: &RunId,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) -> Result<(), ServiceError> {
    match progress {
        ToolProgress::Output { bytes, .. } => {
            let entry = projection
                .tool_progress
                .entry(id.0.clone())
                .or_insert_with(|| (String::new(), 0));
            entry.0.push_str(&String::from_utf8_lossy(bytes.as_ref()));
            entry.0 = bounded_text(&entry.0, 16 * 1024);
            publish_tool_progress(&id.0, projection, events).await?;
        }
        ToolProgress::Status(status) => {
            let entry = projection
                .tool_progress
                .entry(id.0.clone())
                .or_insert_with(|| (String::new(), 0));
            entry.0 = bounded_text(&status, 16 * 1024);
            publish_tool_progress(&id.0, projection, events).await?;
        }
        ToolProgress::Dropped { bytes, .. } => {
            let entry = projection
                .tool_progress
                .entry(id.0.clone())
                .or_insert_with(|| (String::new(), 0));
            entry.1 = entry.1.saturating_add(bytes);
            publish_tool_progress(&id.0, projection, events).await?;
        }
        ToolProgress::Confirmation(request) => {
            projection.request_counter = projection.request_counter.saturating_add(1);
            let request_id = RequestId::new(format!(
                "request-{}-{}",
                run_id.as_str(),
                projection.request_counter
            ))
            .map_err(|_| ServiceError::Internal)?;
            let action = match &request.detail {
                Some(detail) => format!("{}\n\n{}", request.prompt, detail),
                None => request.prompt.clone(),
            };
            let pending = PendingRequest {
                id: request_id.clone(),
                actor_generation: projection_actor_generation(run_id),
                kind: RequestKind::Approval {
                    action: bounded_text(&action, 8 * 1024),
                    item_id: projection.tool_items.get(&id.0).cloned(),
                },
                state: RequestState::Pending,
            };
            let kind = pending.kind.clone();
            projection.private_requests.insert(
                request_id,
                PrivateRequest {
                    kind,
                    response: PrivateResponse::Approval(Box::new(move |allowed| {
                        request.respond(allowed);
                    })),
                },
            );
            events
                .send(event(EventPayload::PendingRequestChanged {
                    request: pending,
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
            events
                .send(event(EventPayload::SessionStateChanged {
                    state: SessionLiveState::NeedsApproval,
                    active_run_id: Some(run_id.clone()),
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
        }
        ToolProgress::Input(request) => {
            projection.request_counter = projection.request_counter.saturating_add(1);
            let request_id = RequestId::new(format!(
                "request-{}-{}",
                run_id.as_str(),
                projection.request_counter
            ))
            .map_err(|_| ServiceError::Internal)?;
            let pending = PendingRequest {
                id: request_id.clone(),
                actor_generation: projection_actor_generation(run_id),
                kind: RequestKind::UserInput {
                    prompt: bounded_text(&request.prompt, 8 * 1024),
                    choices: Vec::new(),
                },
                state: RequestState::Pending,
            };
            let kind = pending.kind.clone();
            projection.private_requests.insert(
                request_id,
                PrivateRequest {
                    kind,
                    response: PrivateResponse::Input(Box::new(move |answer| match answer {
                        Some(answer) => request.respond(answer),
                        None => request.cancel(),
                    })),
                },
            );
            events
                .send(event(EventPayload::PendingRequestChanged {
                    request: pending,
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
            events
                .send(event(EventPayload::SessionStateChanged {
                    state: SessionLiveState::NeedsInput,
                    active_run_id: Some(run_id.clone()),
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
        }
        ToolProgress::SessionEvent(_, _) => {}
    }
    Ok(())
}

async fn publish_tool_progress(
    tool_call_id: &str,
    projection: &ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) -> Result<(), ServiceError> {
    let Some(item_id) = projection.tool_items.get(tool_call_id).cloned() else {
        return Ok(());
    };
    let (text, dropped_bytes) = projection
        .tool_progress
        .get(tool_call_id)
        .cloned()
        .unwrap_or_default();
    events
        .send(event(EventPayload::ItemDelta {
            item_id,
            delta: ItemDelta::ToolProgress {
                text,
                dropped_bytes,
            },
        }))
        .await
        .map_err(|_| ServiceError::Unavailable)
}

fn project_new_entries(
    session: &Session,
    projection: &mut ProjectionState,
    run_id: Option<&RunId>,
    attachment_store: Option<&AttachmentStore>,
    session_id: &SessionId,
) -> Result<Vec<SessionItem>, ServiceError> {
    let entries = session.entries();
    let start = projection.known_entries.min(entries.len());
    let mut items = Vec::new();
    for entry in &entries[start..] {
        let attachments = attachment_refs_for_entry(
            entry,
            attachment_store,
            session_id,
            &mut projection.pending_attachments,
        )?;
        let (preferred, preferred_reasoning) = match &entry.value {
            EntryValue::Message(Message::User(message))
                if message
                    .content
                    .iter()
                    .any(|part| matches!(part, UserPart::Text(_) | UserPart::Media(_))) =>
            {
                (projection.pending_user_items.pop_front(), None)
            }
            EntryValue::Message(Message::Assistant(_)) => (
                projection.completed_assistant_items.pop_front().flatten(),
                projection.completed_reasoning_items.pop_front().flatten(),
            ),
            _ => (None, None),
        };
        items.extend(project_entry(
            entry,
            run_id.cloned(),
            preferred,
            preferred_reasoning,
            &mut projection.tool_items,
            attachments,
        )?);
    }
    projection.known_entries = entries.len();
    Ok(items)
}

fn attachment_refs_for_entry(
    entry: &Entry,
    attachment_store: Option<&AttachmentStore>,
    session_id: &SessionId,
    pending: &mut VecDeque<Vec<AttachmentRef>>,
) -> Result<Vec<AttachmentRef>, ServiceError> {
    let fingerprints = entry_image_fingerprints(entry)?;
    if fingerprints.is_empty() {
        return Ok(Vec::new());
    }
    let Some(store) = attachment_store else {
        return Ok(Vec::new());
    };
    if let Some(references) = store
        .refs_for_entry(session_id, &entry.id.0)
        .map_err(attachment_service_error)?
    {
        if references_match_fingerprints(store, &references, &fingerprints)? {
            return Ok(references);
        }
        return Err(ServiceError::Internal);
    }
    if let Some(references) = pending.front() {
        if references_match_fingerprints(store, references, &fingerprints)? {
            store
                .associate(session_id, &entry.id.0, references)
                .map_err(attachment_service_error)?;
            return Ok(pending.pop_front().unwrap_or_default());
        }
    }
    store
        .recover_association(session_id, &entry.id.0, &fingerprints)
        .map_err(attachment_service_error)
        .map(|references| references.unwrap_or_default())
}

fn entry_image_fingerprints(entry: &Entry) -> Result<Vec<AttachmentFingerprint>, ServiceError> {
    let EntryValue::Message(Message::User(message)) = &entry.value else {
        return Ok(Vec::new());
    };
    message
        .content
        .iter()
        .filter_map(|part| match part {
            UserPart::Media(Media::Image(image)) => Some(image),
            _ => None,
        })
        .filter_map(|image| match &image.source {
            ImageSource::Inline(bytes) => Some((image, bytes)),
            _ => None,
        })
        .map(|(image, bytes)| {
            let media_type = image
                .media_type
                .as_ref()
                .ok_or(ServiceError::InvalidSeed)?
                .essence_str()
                .to_owned();
            Ok(AttachmentFingerprint {
                media_type,
                byte_len: bytes.len() as u64,
                sha256: stable_hash(bytes),
            })
        })
        .collect()
}

fn references_match_fingerprints(
    store: &AttachmentStore,
    references: &[AttachmentRef],
    fingerprints: &[AttachmentFingerprint],
) -> Result<bool, ServiceError> {
    if references.len() != fingerprints.len() {
        return Ok(false);
    }
    let resolved = store
        .resolve_many(references)
        .map_err(attachment_service_error)?;
    Ok(resolved
        .iter()
        .zip(fingerprints)
        .all(|(attachment, fingerprint)| {
            attachment.reference.media_type == fingerprint.media_type
                && attachment.reference.byte_len == fingerprint.byte_len
                && attachment.sha256 == fingerprint.sha256
        }))
}

fn project_entry(
    entry: &Entry,
    run_id: Option<RunId>,
    preferred: Option<ItemId>,
    preferred_reasoning: Option<ItemId>,
    tool_items: &mut HashMap<String, ItemId>,
    attachments: Vec<AttachmentRef>,
) -> Result<Vec<SessionItem>, ServiceError> {
    let durable_id =
        DurableEntryId::new(entry.id.0.clone()).map_err(|_| ServiceError::InvalidSeed)?;
    let mut items = Vec::new();
    match &entry.value {
        EntryValue::Message(Message::User(message)) => {
            let mut user_text = Vec::new();
            for part in &message.content {
                match part {
                    UserPart::Text(text) => user_text.push(text.as_str()),
                    UserPart::Media(_) => {}
                    UserPart::ToolResult(result) => {
                        let content = result
                            .content
                            .iter()
                            .map(|part| match part {
                                ToolResultPart::Text(text) => text.as_str(),
                                ToolResultPart::Media(_) => "[media output]",
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let tool_call_item_id =
                            tool_items.get(&result.tool_call_id.0).cloned().unwrap_or(
                                ItemId::new(format!("item-entry-{}-unknown-call", entry.id.0))
                                    .map_err(|_| ServiceError::InvalidSeed)?,
                            );
                        items.push(committed_item(
                            item_id_for_entry(entry, items.len())?,
                            run_id.clone(),
                            durable_id.clone(),
                            ItemPayload::ToolResult {
                                tool_call_item_id,
                                content: bounded_text(&content, MAX_ITEM_TEXT_BYTES),
                                is_error: result.is_error,
                            },
                        ));
                    }
                }
            }
            if !user_text.is_empty()
                || message
                    .content
                    .iter()
                    .any(|part| matches!(part, UserPart::Media(_)))
            {
                let visible = entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.display_text.as_deref())
                    .unwrap_or_else(|| user_text.first().copied().unwrap_or(""));
                let text = if entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.display_text.as_ref())
                    .is_some()
                {
                    visible.to_owned()
                } else {
                    user_text.join("\n")
                };
                items.insert(
                    0,
                    committed_item(
                        preferred.unwrap_or(item_id_for_entry(entry, items.len())?),
                        run_id,
                        durable_id,
                        ItemPayload::UserMessage {
                            text: bounded_text(&text, MAX_PROMPT_BYTES),
                            attachments,
                        },
                    ),
                );
            }
        }
        EntryValue::Message(Message::Assistant(message)) => {
            let text = message
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                items.push(committed_item(
                    preferred.unwrap_or(item_id_for_entry(entry, items.len())?),
                    run_id.clone(),
                    durable_id.clone(),
                    ItemPayload::AssistantMessage {
                        text: bounded_text(&text, MAX_ITEM_TEXT_BYTES),
                    },
                ));
            }
            let reasoning = message
                .content
                .iter()
                .filter_map(|part| match part {
                    AssistantPart::Reasoning(reasoning) => reasoning.text.as_deref(),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !reasoning.is_empty() {
                items.push(committed_item(
                    preferred_reasoning.unwrap_or(item_id_for_entry(entry, items.len())?),
                    run_id.clone(),
                    durable_id.clone(),
                    ItemPayload::Reasoning {
                        text: bounded_text(&reasoning, MAX_ITEM_TEXT_BYTES),
                    },
                ));
            }
            for call in message.content.iter().filter_map(|part| match part {
                AssistantPart::ToolCall(call) => Some(call),
                _ => None,
            }) {
                let arguments = call.arguments_value().unwrap_or(serde_json::Value::Null);
                let arguments = if ygg_serve_backend::validate_json(
                    "tool.arguments",
                    &arguments,
                    256 * 1024,
                )
                .is_ok()
                {
                    arguments
                } else {
                    serde_json::json!({"unavailable": "arguments exceeded the graphical projection limit"})
                };
                let item_id = tool_items
                    .get(&call.id.0)
                    .cloned()
                    .unwrap_or(item_id_for_entry(entry, items.len())?);
                tool_items.insert(call.id.0.clone(), item_id.clone());
                items.push(committed_item(
                    item_id,
                    run_id.clone(),
                    durable_id.clone(),
                    ItemPayload::ToolCall {
                        name: bounded_text(&call.name, 128),
                        arguments,
                        progress: None,
                        dropped_progress_bytes: 0,
                    },
                ));
            }
        }
        EntryValue::Compaction { summary, .. } => {
            items.push(committed_item(
                item_id_for_entry(entry, 0)?,
                run_id,
                durable_id,
                ItemPayload::Compaction {
                    reason: bounded_text(summary, 4 * 1024),
                },
            ));
        }
        EntryValue::Config { .. } => {
            if let Some(outcome) = entry
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.run_outcome.as_ref())
            {
                let outcome = match outcome.status {
                    SessionRunOutcomeStatus::Completed => ygg_serve_backend::RunOutcome::Completed,
                    SessionRunOutcomeStatus::Stopped => ygg_serve_backend::RunOutcome::Stopped,
                    SessionRunOutcomeStatus::Failed => ygg_serve_backend::RunOutcome::Failed,
                };
                let message = entry
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.run_outcome.as_ref())
                    .and_then(|outcome| outcome.message.as_deref())
                    .map(|message| bounded_text(message, 8 * 1024));
                items.push(committed_item(
                    item_id_for_entry(entry, 0)?,
                    run_id,
                    durable_id,
                    ItemPayload::RunOutcome { outcome, message },
                ));
            }
        }
        EntryValue::ResponsesTurn { .. }
        | EntryValue::ResponsesCompaction { .. }
        | EntryValue::PromptTemplateSelected { .. }
        | EntryValue::SkillActivated { .. }
        | EntryValue::SkillResourceRead { .. }
        | EntryValue::SkillDeactivated { .. } => {}
    }
    Ok(items)
}

fn committed_item(
    id: ItemId,
    run_id: Option<RunId>,
    durable_entry_id: DurableEntryId,
    payload: ItemPayload,
) -> SessionItem {
    SessionItem {
        id,
        run_id,
        turn_id: None,
        provider_attempt: None,
        lifecycle: ItemLifecycle::Committed,
        durable_entry_id: Some(durable_entry_id),
        payload,
    }
}

fn item_id_for_entry(entry: &Entry, part: usize) -> Result<ItemId, ServiceError> {
    ItemId::new(format!("item-entry-{}-part-{part}", entry.id.0))
        .map_err(|_| ServiceError::InvalidSeed)
}

struct SessionSeedOptions<'a> {
    project_id: Option<ProjectId>,
    model: ModelSelection,
    authority: AuthorityProfile,
    generation: u64,
    meta: Option<SessionMeta>,
    attachment_store: Option<&'a AttachmentStore>,
}

fn seed_from_session(
    session: &Session,
    session_id: SessionId,
    options: SessionSeedOptions<'_>,
) -> Result<SessionSeed, ServiceError> {
    let SessionSeedOptions {
        project_id,
        model,
        authority,
        generation,
        meta,
        attachment_store,
    } = options;
    let mut chain = Vec::new();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session.entry(id).ok_or(ServiceError::InvalidSeed)?;
        chain.push(entry);
        cursor = entry.parent.as_ref();
    }
    chain.reverse();
    let mut items = Vec::new();
    let mut tool_items = HashMap::new();
    let mut pending_attachments = VecDeque::new();
    for entry in chain {
        let attachments = attachment_refs_for_entry(
            entry,
            attachment_store,
            &session_id,
            &mut pending_attachments,
        )?;
        let projected = project_entry(entry, None, None, None, &mut tool_items, attachments)?;
        items.extend(projected);
    }
    if items.len() > MAX_PROJECTED_SESSION_ITEMS {
        items = items.split_off(items.len() - MAX_PROJECTED_SESSION_ITEMS);
    }
    let modified_at_ms = meta
        .as_ref()
        .map(|meta| system_time_ms(meta.modified))
        .unwrap_or_else(now_ms);
    let title = meta
        .as_ref()
        .map(|meta| bounded_text(&meta.title, 512))
        .unwrap_or_else(|| "Session".into());
    let pinned = meta.as_ref().is_some_and(|meta| meta.pinned);
    let archived = meta.as_ref().is_some_and(|meta| meta.archived);
    let summary = SessionSummary {
        id: session_id.clone(),
        project_id,
        title,
        tags: meta.map(|meta| meta.tags).unwrap_or_default(),
        created_at_ms: modified_at_ms,
        modified_at_ms,
        pinned,
        archived,
        provisional: false,
        live_state: SessionLiveState::Idle,
        attention: AttentionState::None,
        owner: ActorOwnerState::Hosted,
        model: model.clone(),
    };
    let snapshot = SessionSnapshot {
        session_id,
        actor_generation: generation,
        cursor: SessionCursor::zero(generation),
        durable_head: session
            .head()
            .map(|head| DurableEntryId::new(head.0))
            .transpose()
            .map_err(|_| ServiceError::InvalidSeed)?,
        live_state: SessionLiveState::Idle,
        active_run_id: None,
        model,
        authority,
        context: ContextUsage::default(),
        items,
        pending_requests: Vec::new(),
        sources: Vec::new(),
        artifacts: Vec::new(),
    };
    let seed = SessionSeed { summary, snapshot };
    seed.validate()?;
    Ok(seed)
}

fn empty_seed(
    session_id: SessionId,
    project_id: Option<ProjectId>,
    model: ModelSelection,
    authority: AuthorityProfile,
    generation: u64,
) -> SessionSeed {
    let timestamp = now_ms();
    SessionSeed {
        summary: SessionSummary {
            id: session_id.clone(),
            project_id,
            title: "New session".into(),
            tags: Vec::new(),
            created_at_ms: timestamp,
            modified_at_ms: timestamp,
            pinned: false,
            archived: false,
            provisional: true,
            live_state: SessionLiveState::Idle,
            attention: AttentionState::None,
            owner: ActorOwnerState::Hosted,
            model: model.clone(),
        },
        snapshot: SessionSnapshot {
            session_id,
            actor_generation: generation,
            cursor: SessionCursor::zero(generation),
            durable_head: None,
            live_state: SessionLiveState::Idle,
            active_run_id: None,
            model,
            authority,
            context: ContextUsage::default(),
            items: Vec::new(),
            pending_requests: Vec::new(),
            sources: Vec::new(),
            artifacts: Vec::new(),
        },
    }
}

fn graphical_model_catalog(catalog: &ModelCatalog, config: &Config) -> Vec<ModelSummary> {
    let models = catalog
        .models()
        .filter_map(|spec| catalog.resolve(&spec.id).ok())
        .map(|model| {
            let reasoning = supported_levels(&model)
                .into_iter()
                .map(thinking_label)
                .collect::<Vec<_>>();
            let requested_default =
                selection_for_model(&model, &config.reasoning, config).reasoning;
            let default_reasoning = reasoning
                .iter()
                .find(|choice| choice.as_str() == requested_default.as_str())
                .cloned()
                .or_else(|| reasoning.first().cloned());
            let mut input_modalities = vec![InputModality::Text];
            if model
                .spec
                .capabilities
                .input_modalities
                .contains(Modality::Image)
            {
                input_modalities.push(InputModality::Image);
            }
            if model
                .spec
                .capabilities
                .input_modalities
                .contains(Modality::Audio)
            {
                input_modalities.push(InputModality::Audio);
            }
            ModelSummary {
                id: model.spec.id.0.clone(),
                name: model
                    .spec
                    .display_name
                    .clone()
                    .unwrap_or_else(|| model.spec.id.0.clone()),
                provider: model.endpoint.id.0.clone(),
                local: model
                    .endpoint
                    .base_url
                    .host_str()
                    .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1")),
                available: true,
                reasoning,
                default_reasoning,
                input_modalities,
            }
        })
        .collect();
    bound_graphical_models(models, config.model.as_ref())
}

fn bound_graphical_models(
    mut models: Vec<ModelSummary>,
    configured_model: Option<&ModelId>,
) -> Vec<ModelSummary> {
    let compare = |left: &ModelSummary, right: &ModelSummary| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    };
    models.sort_by(compare);
    if models.len() <= MAX_GRAPHICAL_MODELS {
        return models;
    }

    let configured = configured_model.and_then(|configured| {
        models
            .iter()
            .position(|summary| summary.id == configured.0)
            .filter(|index| *index >= MAX_GRAPHICAL_MODELS)
            .map(|index| models.remove(index))
    });
    models.truncate(MAX_GRAPHICAL_MODELS - usize::from(configured.is_some()));
    if let Some(configured) = configured {
        models.push(configured);
        models.sort_by(compare);
    }
    models
}

fn selection_from_summary(summary: &ModelSummary) -> ModelSelection {
    ModelSelection {
        provider: summary.provider.clone(),
        model: summary.id.clone(),
        reasoning: summary
            .default_reasoning
            .clone()
            .or_else(|| summary.reasoning.first().cloned())
            .unwrap_or_else(|| "off".into()),
    }
}

fn selection_for_model(
    model: &Model,
    reasoning: &ReasoningConfig,
    config: &Config,
) -> ModelSelection {
    let normalized =
        crate::app::normalize_reasoning_for_model(reasoning, model).unwrap_or(ReasoningConfig::Off);
    let portable = crate::app::level_from_reasoning(&normalized, model)
        .map(thinking_label)
        .unwrap_or_else(|_| reasoning_label(&normalized));
    let choices = supported_levels(model)
        .into_iter()
        .map(thinking_label)
        .collect::<Vec<_>>();
    let portable = choices
        .iter()
        .find(|choice| choice.as_str() == portable.as_str())
        .cloned()
        .or_else(|| choices.first().cloned())
        .unwrap_or_else(|| "off".into());
    let _ = config;
    ModelSelection {
        provider: model.endpoint.id.0.clone(),
        model: model.spec.id.0.clone(),
        reasoning: portable,
    }
}

fn selection_from_session(
    session: &Session,
    catalog: &ModelCatalog,
    config: &Config,
) -> Result<ModelSelection, ServiceError> {
    let mut model = None;
    let mut reasoning = None;
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session.entry(id).ok_or(ServiceError::InvalidSeed)?;
        if let EntryValue::Config {
            model: persisted_model,
            reasoning: persisted_reasoning,
            ..
        } = &entry.value
        {
            if model.is_none() {
                model = persisted_model.clone();
            }
            if reasoning.is_none() {
                reasoning = persisted_reasoning.clone();
            }
            if model.is_some() && reasoning.is_some() {
                break;
            }
        }
        cursor = entry.parent.as_ref();
    }
    let model_id = model
        .map(ModelId)
        .or_else(|| config.model.clone())
        .ok_or(ServiceError::InvalidSeed)?;
    let model = catalog
        .resolve(&model_id)
        .map_err(|_| ServiceError::InvalidSeed)?;
    let reasoning = reasoning
        .as_deref()
        .map(config::parse_reasoning)
        .transpose()
        .map_err(|_| ServiceError::InvalidSeed)?
        .unwrap_or_else(|| config.reasoning.clone());
    Ok(selection_for_model(&model, &reasoning, config))
}

fn current_selection(plan: &WorkerPlan) -> ModelSelection {
    let summary = plan
        .available_models
        .iter()
        .find(|summary| summary.id == plan.launch.model.0);
    match summary {
        Some(summary) => {
            let projected = reasoning_label(&plan.launch.reasoning);
            ModelSelection {
                provider: summary.provider.clone(),
                model: summary.id.clone(),
                reasoning: summary
                    .reasoning
                    .iter()
                    .find(|choice| choice.as_str() == projected.as_str())
                    .cloned()
                    .or_else(|| summary.default_reasoning.clone())
                    .or_else(|| summary.reasoning.first().cloned())
                    .unwrap_or_else(|| "off".into()),
            }
        }
        None => ModelSelection {
            provider: "unknown".into(),
            model: plan.launch.model.0.clone(),
            reasoning: "off".into(),
        },
    }
}

fn thinking_label(level: crate::config::ThinkingLevel) -> String {
    match level {
        crate::config::ThinkingLevel::Off => "off",
        crate::config::ThinkingLevel::On => "on",
        crate::config::ThinkingLevel::Minimal => "minimal",
        crate::config::ThinkingLevel::Low => "low",
        crate::config::ThinkingLevel::Medium => "medium",
        crate::config::ThinkingLevel::High => "high",
        crate::config::ThinkingLevel::Xhigh => "xhigh",
        crate::config::ThinkingLevel::Max => "max",
    }
    .into()
}

fn graphical_themes(config: &Config) -> anyhow::Result<(Vec<ThemeOption>, ThemeId)> {
    const MAX_GRAPHICAL_THEMES: usize = 64;

    let selected_name = match config.theme.as_deref() {
        Some(name) if crate::tui::theme::load_named_theme(name, config).is_ok() => name.to_owned(),
        _ => crate::tui::theme::DEFAULT_THEME_NAME.to_owned(),
    };
    let mut names = crate::tui::theme::available_themes(config);
    names.retain(|name| name != &selected_name);
    names.insert(0, selected_name.clone());

    let mut themes = Vec::new();
    for name in names.into_iter().take(MAX_GRAPHICAL_THEMES) {
        let Ok(theme) = crate::tui::theme::load_named_theme(&name, config) else {
            continue;
        };
        themes.push(graphical_theme_option(&name, &theme, config)?);
    }
    if themes.is_empty() {
        let theme = crate::tui::theme::load_theme(config);
        themes.push(graphical_theme_option(&selected_name, &theme, config)?);
    }
    let selected_theme_id = graphical_theme_id(&selected_name)?;
    if !themes.iter().any(|theme| theme.id == selected_theme_id) {
        anyhow::bail!("selected graphical theme was not projected");
    }
    Ok((themes, selected_theme_id))
}

fn graphical_theme_id(name: &str) -> anyhow::Result<ThemeId> {
    ThemeId::new(format!("theme-{}", &stable_hash(name.as_bytes())[..24]))
        .map_err(anyhow::Error::msg)
}

fn graphical_theme_option(
    name: &str,
    theme: &crate::tui::theme::YggTheme,
    config: &Config,
) -> anyhow::Result<ThemeOption> {
    const BUILT_IN_ROLES: &[&str] = &[
        "text",
        "muted",
        "subtle",
        "accent",
        "success",
        "warning",
        "error",
        "heading",
        "emphasis",
        "strong",
        "inline_code",
        "code",
        "quote",
        "border",
        "link",
        "list_marker",
        "diff_add",
        "diff_remove",
        "diff_context",
        "diff_hunk",
        "diff_header",
        "syntax_comment",
        "syntax_keyword",
        "syntax_function",
        "syntax_variable",
        "syntax_string",
        "syntax_number",
        "syntax_type",
        "syntax_operator",
        "syntax_punctuation",
    ];

    let mut role_names = BUILT_IN_ROLES
        .iter()
        .map(|role| (*role).to_owned())
        .collect::<Vec<_>>();
    role_names.extend(theme.semantic_role_names().map(str::to_owned));
    role_names.sort();
    role_names.dedup();
    // Each role may contribute one foreground and one background token.
    role_names.truncate(128);

    let mut colors = BTreeMap::new();
    let mut roles = BTreeMap::new();
    for (index, role_name) in role_names.into_iter().enumerate() {
        let Ok(role) = SemanticRole::new(role_name.clone()) else {
            continue;
        };
        let style = theme.semantic_style(&role_name);
        let foreground = graphical_color_token(&mut colors, index, "foreground", style.foreground);
        let background = graphical_color_token(&mut colors, index, "background", style.background);
        roles.insert(role, graphical_role_style(style, foreground, background));
    }

    let source = match theme.source() {
        crate::tui::theme::ThemeSource::CompiledDefault
        | crate::tui::theme::ThemeSource::Bundled(_) => ThemeSourceClass::Bundled,
        crate::tui::theme::ThemeSource::File(path) if path.starts_with(&config.workspace) => {
            ThemeSourceClass::Project
        }
        crate::tui::theme::ThemeSource::File(_) => ThemeSourceClass::Global,
    };
    let scheme = match theme.background() {
        crate::tui::theme::TerminalBackground::Dark => ColorScheme::Dark,
        crate::tui::theme::TerminalBackground::Light => ColorScheme::Light,
        crate::tui::theme::TerminalBackground::Unknown => ColorScheme::Unknown,
    };
    let density = match theme.layout().density {
        crate::tui::theme::ThemeDensity::Compact => ThemeDensity::Compact,
        crate::tui::theme::ThemeDensity::Comfortable => ThemeDensity::Comfortable,
        crate::tui::theme::ThemeDensity::Airy => ThemeDensity::Airy,
    };
    let display_name = if theme.metadata().name.trim().is_empty() {
        name
    } else {
        &theme.metadata().name
    };
    let option = ThemeOption {
        id: graphical_theme_id(name)?,
        theme: ThemeDto {
            name: bounded_text(display_name, 128),
            source,
            revision: 1,
            scheme,
            density,
            motion: ThemeMotion::Full,
            typography: ThemeTypography {
                body_family: "system-ui".into(),
                mono_family: "ui-monospace".into(),
                body_size: 17,
                display_ratio_milli: 1235,
            },
            colors,
            roles,
        },
    };
    option.validate().map_err(anyhow::Error::msg)?;
    Ok(option)
}

fn graphical_color_token(
    colors: &mut BTreeMap<String, ThemeColor>,
    index: usize,
    channel: &str,
    color: TuiColor,
) -> Option<String> {
    let projected = match color {
        TuiColor::Default => return Some("default".into()),
        TuiColor::Ansi16(index) | TuiColor::Indexed(index) => ThemeColor::Ansi { index },
        TuiColor::Rgb(red, green, blue) => ThemeColor::Rgb { red, green, blue },
    };
    let token = format!("role.{index}.{channel}");
    colors.insert(token.clone(), projected);
    Some(token)
}

fn graphical_role_style(
    style: TuiTextStyle,
    mut foreground: Option<String>,
    mut background: Option<String>,
) -> ThemeRoleStyle {
    if style.attributes.inverse {
        std::mem::swap(&mut foreground, &mut background);
    }
    ThemeRoleStyle {
        foreground,
        background,
        bold: style.attributes.bold,
        dim: style.attributes.dim,
        italic: style.attributes.italic,
        underline: style.attributes.underline,
        strikethrough: style.attributes.strikethrough,
    }
}

fn session_meta_for_id(store: &SessionStore, session_id: &SessionId) -> Option<SessionMeta> {
    store
        .list()
        .into_iter()
        .find(|meta| meta.id == session_id.as_str())
}

fn summary_from_meta(
    meta: &SessionMeta,
    project_id: Option<ProjectId>,
    model: ModelSelection,
) -> Result<SessionSummary, ServiceError> {
    let id = SessionId::new(meta.id.clone()).map_err(|_| ServiceError::InvalidSeed)?;
    let modified_at_ms = system_time_ms(meta.modified);
    Ok(SessionSummary {
        id,
        project_id,
        title: bounded_text(&meta.title, 512),
        tags: meta.tags.iter().map(|tag| bounded_text(tag, 64)).collect(),
        created_at_ms: modified_at_ms,
        modified_at_ms,
        pinned: meta.pinned,
        archived: meta.archived,
        provisional: false,
        live_state: SessionLiveState::Idle,
        attention: AttentionState::None,
        owner: ActorOwnerState::Inactive,
        model,
    })
}

fn session_id_from_path(path: &Path) -> Result<SessionId, ServiceError> {
    SessionId::new(
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or(ServiceError::InvalidSeed)?
            .to_owned(),
    )
    .map_err(|_| ServiceError::InvalidSeed)
}

fn event(payload: EventPayload) -> TimestampedEvent {
    TimestampedEvent::new(now_ms(), payload)
}

fn bounded_text(text: &str, max: usize) -> String {
    ygg_serve_backend::sanitize_public_text(text, max, true)
}

fn next_actor_generation() -> u64 {
    now_ms()
        .saturating_mul(1_000)
        .saturating_add(NEXT_ACTOR_GENERATION.fetch_add(1, Ordering::Relaxed))
        .max(1)
}

fn stable_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn load_or_create_host_id(config: &Config) -> anyhow::Result<HostId> {
    use std::io::{Read as _, Write as _};

    // Keep serve-owned state below the configured session root. The session
    // root may have a broad or user-managed parent (for example `/tmp`), so
    // never tighten permissions on its parent directory.
    let state_dir = secure_serve_state_dir(&config.session_dir)?;
    let path = state_dir.join("serve-host-id");
    let read_existing = || -> anyhow::Result<Option<HostId>> {
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > 256 {
            anyhow::bail!("invalid ygg serve host identity file");
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path)?;
        let mut value = String::new();
        file.take(256).read_to_string(&mut value)?;
        let id = HostId::new(value.trim().to_owned()).map_err(anyhow::Error::msg)?;
        Ok(Some(id))
    };
    if let Some(id) = read_existing()? {
        return Ok(id);
    }

    let mut random = [0u8; 32];
    getrandom::fill(&mut random)?;
    let id = HostId::new(format!("host-{}", stable_hash(&random))).map_err(anyhow::Error::msg)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(id.as_str().as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(id)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_existing()?
            .ok_or_else(|| anyhow::anyhow!("ygg serve host identity creation raced")),
        Err(error) => Err(error.into()),
    }
}

fn secure_serve_state_dir(session_dir: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(session_dir)?;
    let state_dir = session_dir.join(".serve");
    loop {
        match state_dir.symlink_metadata() {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                break;
            }
            Ok(_) => anyhow::bail!("ygg serve state path must be a real directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::create_dir(&state_dir) {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
        let directory = options.open(&state_dir)?;
        if !directory.metadata()?.is_dir() {
            anyhow::bail!("ygg serve state path changed during validation");
        }
        directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(state_dir)
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ygg_agent::EntryMetadata;
    use ygg_ai::{AssistantMessage, Protocol, UserMessage};
    use ygg_serve_backend::{CatalogCursor, HostBootstrap, PROTOCOL_VERSION};

    fn serve_test_config(directory: &Path) -> Config {
        Config {
            workspace: directory.to_path_buf(),
            invocation_cwd: directory.to_path_buf(),
            model: None,
            model_explicit: false,
            reasoning: ReasoningConfig::Off,
            reasoning_explicit: false,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
            reasoning_mode_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: crate::config::SandboxPolicy::default(),
            theme: None,
            theme_paths: Vec::new(),
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: directory.join("sessions"),
            compaction: crate::config::CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: Vec::new(),
            mode: crate::config::Mode::Print {
                prompt: "test".into(),
            },
            resume: crate::config::ResumeSelector::New,
            skill_paths: Vec::new(),
            extension_paths: Vec::new(),
            enabled_extensions: Vec::new(),
            trusted_extensions: Vec::new(),
            invocation_trusted_extensions: Vec::new(),
            tools: crate::config::ToolPolicy::default(),
            context_files: false,
            offline: true,
            workspace_trusted: true,
        }
    }

    fn png() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IEND");
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes
    }

    #[test]
    fn serve_host_lock_is_exclusive_for_the_session_root_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let first = ServeHostLock::acquire_at(directory.path()).unwrap();
        assert!(ServeHostLock::acquire_at(directory.path()).is_err());
        drop(first);
        ServeHostLock::acquire_at(directory.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn serve_state_directory_rejects_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        symlink(target.path(), directory.path().join(".serve")).unwrap();
        assert!(secure_serve_state_dir(directory.path()).is_err());
    }

    #[test]
    fn stored_attachments_cross_the_agent_boundary_as_native_inline_media() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let image = png();
        let reference = store
            .ingest(
                "alignment.png",
                "image/png",
                bytes::Bytes::from(image.clone()),
            )
            .unwrap();

        let media = resolve_stored_media(true, Some(&store), &[reference]).unwrap();
        assert_eq!(media.len(), 1);
        let Media::Image(image_media) = &media[0] else {
            panic!("image attachment was not represented as image media");
        };
        assert_eq!(
            image_media.media_type.as_ref().map(mime::Mime::essence_str),
            Some("image/png")
        );
        assert!(
            matches!(&image_media.source, ImageSource::Inline(bytes) if bytes.as_ref() == image)
        );
    }

    #[test]
    fn unsupported_or_tampered_attachments_fail_before_agent_input_is_built() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let reference = store
            .ingest("alignment.png", "image/png", bytes::Bytes::from(png()))
            .unwrap();

        assert_eq!(
            resolve_stored_media(false, Some(&store), std::slice::from_ref(&reference))
                .unwrap_err(),
            ServiceError::InvalidBoundary
        );
        let mut tampered = reference;
        tampered.byte_len += 1;
        assert_eq!(
            resolve_stored_media(true, Some(&store), &[tampered]).unwrap_err(),
            ServiceError::InvalidBoundary
        );
    }

    #[test]
    fn successful_read_mints_openable_path_free_source_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/lib.rs"), b"pub fn ygg() {}\n").unwrap();
        let registry = OpaqueResourceRegistry::default();
        let session_id = SessionId::new("session-evidence").unwrap();
        let run_id = RunId::new("run-evidence").unwrap();
        let turn_id = TurnId::new("turn-evidence").unwrap();
        let tool_item_id = ItemId::new("item-tool-read").unwrap();
        let tool = ProjectedToolCall {
            name: "read".into(),
            arguments: serde_json::json!({"path": "src/lib.rs"}),
        };

        let events = project_tool_evidence(
            workspace.path(),
            &registry,
            &session_id,
            &run_id,
            &turn_id,
            "call-read",
            &tool_item_id,
            &tool,
            &ToolOutput::new("1: pub fn ygg() {}"),
        );
        assert_eq!(events.len(), 2);
        let EventPayload::SourceUpserted { source } = &events[0] else {
            panic!("first event was not source evidence");
        };
        assert_eq!(source.title, "src/lib.rs");
        assert_eq!(source.kind, SourceKind::File);
        assert_eq!(source.origin_item_id.as_ref(), Some(&tool_item_id));
        assert_eq!(
            registry.content(&source.handle).unwrap().bytes,
            bytes::Bytes::from_static(b"pub fn ygg() {}\n")
        );
        assert!(!source.handle.contains("src"));
    }

    #[test]
    fn resource_projection_rejects_outside_workspace_and_snapshots_successful_edits() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let registry = OpaqueResourceRegistry::default();
        let session_id = SessionId::new("session-evidence").unwrap();
        let run_id = RunId::new("run-evidence").unwrap();
        let turn_id = TurnId::new("turn-evidence").unwrap();
        let tool_item_id = ItemId::new("item-tool").unwrap();
        let outside_tool = ProjectedToolCall {
            name: "read".into(),
            arguments: serde_json::json!({"path": outside.path()}),
        };
        assert!(project_tool_evidence(
            workspace.path(),
            &registry,
            &session_id,
            &run_id,
            &turn_id,
            "call-outside",
            &tool_item_id,
            &outside_tool,
            &ToolOutput::new("secret"),
        )
        .is_empty());

        std::fs::write(workspace.path().join("notes.md"), b"after\nsecond\n").unwrap();
        let edit = ProjectedToolCall {
            name: "edit".into(),
            arguments: serde_json::json!({
                "path": "notes.md",
                "old": "before\n",
                "new": "after\nsecond\n"
            }),
        };
        let events = project_tool_evidence(
            workspace.path(),
            &registry,
            &session_id,
            &run_id,
            &turn_id,
            "call-edit",
            &tool_item_id,
            &edit,
            &ToolOutput::new("ok modified=1"),
        );
        assert_eq!(events.len(), 3);
        let EventPayload::ItemStarted { item } = &events[0] else {
            panic!("first edit event was not a file change");
        };
        let ItemPayload::FileChange(change) = &item.payload else {
            panic!("edit item was not a file change");
        };
        assert_eq!(change.display_path, "notes.md");
        assert_eq!((change.additions, change.deletions), (2, 1));
        let EventPayload::ArtifactUpserted { artifact } = &events[1] else {
            panic!("second edit event was not an artifact");
        };
        assert_eq!(artifact.kind, ArtifactKind::Document);
        assert_eq!(
            registry.content(&artifact.handle).unwrap().bytes,
            bytes::Bytes::from_static(b"after\nsecond\n")
        );
    }

    #[test]
    fn pre_prompt_selection_is_durable_across_session_restart() {
        let directory = tempfile::tempdir().unwrap();
        let session_dir = directory.path().join("sessions");
        std::fs::create_dir(&session_dir).unwrap();
        let session_path = session_dir.join("selection.jsonl");
        let session_id = SessionId::new("selection").unwrap();
        let mut plan = WorkerPlan {
            config: serve_test_config(directory.path()),
            sessions: SessionStore::new(&session_dir, directory.path()),
            launch: LaunchSelection {
                model: ModelId("selected-model".into()),
                session: SessionSelection::CreateNew(session_path.clone()),
                reasoning: ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High),
                reasoning_mode: ygg_ai::ReasoningMode::Standard,
            },
            authority: AuthorityProfile::FullAccess,
            available_models: Vec::new(),
            actor_generation: 1,
            session_id,
            attachments: None,
            resources: OpaqueResourceRegistry::default(),
        };
        let mut projection = ProjectionState::new(0);

        let first = persist_idle_selection(
            &mut plan,
            &mut projection,
            ModelSelection {
                provider: "test".into(),
                model: "selected-model".into(),
                reasoning: "high".into(),
            },
        )
        .unwrap();
        assert_eq!(first.events.len(), 2);
        assert!(matches!(
            plan.launch.session,
            SessionSelection::OpenExisting(ref path) if path == &session_path
        ));

        plan.launch.reasoning = ReasoningConfig::Effort(ygg_ai::ReasoningEffort::Low);
        persist_idle_selection(
            &mut plan,
            &mut projection,
            ModelSelection {
                provider: "test".into(),
                model: "selected-model".into(),
                reasoning: "low".into(),
            },
        )
        .unwrap();
        drop(plan);

        let reopened = Session::open_read_only(&session_path).unwrap();
        let latest = reopened.entries().last().expect("persisted config");
        assert!(matches!(
            &latest.value,
            EntryValue::Config {
                model: Some(model),
                reasoning: Some(reasoning),
                reasoning_mode: Some(reasoning_mode),
            } if model == "selected-model"
                && reasoning == "low"
                && reasoning_mode == "standard"
        ));
    }

    #[test]
    fn session_metadata_mutations_are_durable_and_emit_exact_patches() {
        let directory = tempfile::tempdir().unwrap();
        let config = serve_test_config(directory.path());
        let sessions = SessionStore::new(&config.session_dir, &config.workspace);
        std::fs::create_dir_all(sessions.dir()).unwrap();
        let session_path = sessions.dir().join("metadata-session.jsonl");
        let mut session = Session::create(&session_path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("original title".into())],
            })))
            .unwrap();
        drop(session);
        let plan = WorkerPlan {
            config,
            sessions: sessions.clone(),
            launch: LaunchSelection {
                model: ModelId("test-model".into()),
                session: SessionSelection::OpenExisting(session_path),
                reasoning: ReasoningConfig::Off,
                reasoning_mode: ygg_ai::ReasoningMode::Standard,
            },
            authority: AuthorityProfile::FullAccess,
            available_models: Vec::new(),
            actor_generation: 1,
            session_id: SessionId::new("metadata-session").unwrap(),
            attachments: None,
            resources: OpaqueResourceRegistry::default(),
        };

        let renamed = rename_session_outcome(&plan, "  Renamed session  ").unwrap();
        assert!(matches!(
            renamed.events.as_slice(),
            [TimestampedEvent {
                payload: EventPayload::SessionMetadataChanged {
                    title: Some(title),
                    pinned: None,
                    archived: None,
                },
                ..
            }] if title == "Renamed session"
        ));
        let pinned = pin_session_outcome(&plan, true).unwrap();
        assert!(matches!(
            pinned.events.as_slice(),
            [TimestampedEvent {
                payload: EventPayload::SessionMetadataChanged {
                    title: None,
                    pinned: Some(true),
                    archived: None,
                },
                ..
            }]
        ));
        let archived = archive_session_outcome(&plan, true).unwrap();
        assert!(matches!(
            archived.events.as_slice(),
            [TimestampedEvent {
                payload: EventPayload::SessionMetadataChanged {
                    title: None,
                    pinned: None,
                    archived: Some(true),
                },
                ..
            }]
        ));

        let reopened = SessionStore::new(&plan.config.session_dir, &plan.config.workspace);
        let metadata = reopened.load_metadata("metadata-session").unwrap();
        assert_eq!(metadata.name.as_deref(), Some("Renamed session"));
        assert!(metadata.pinned);
        assert!(metadata.archived);
        let summary = summary_from_meta(
            &session_meta_for_id(&reopened, &plan.session_id).unwrap(),
            None,
            current_selection(&plan),
        )
        .unwrap();
        assert_eq!(summary.title, "Renamed session");
        assert!(summary.pinned);
        assert!(summary.archived);
    }

    #[test]
    fn durable_projection_recovers_attachment_refs_from_native_media() {
        let directory = tempfile::tempdir().unwrap();
        let store = AttachmentStore::open(directory.path()).unwrap();
        let image = png();
        let reference = store
            .ingest(
                "alignment.png",
                "image/png",
                bytes::Bytes::from(image.clone()),
            )
            .unwrap();
        let entry = Entry {
            id: ygg_agent::EntryId("entry-1".into()),
            parent: None,
            metadata: None,
            value: EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Media(Media::image_bytes(
                    bytes::Bytes::from(image),
                    "image/png".parse().unwrap(),
                ))],
            })),
        };
        let session_id = SessionId::new("session-1").unwrap();
        let mut pending = VecDeque::from([vec![reference.clone()]]);

        let associated =
            attachment_refs_for_entry(&entry, Some(&store), &session_id, &mut pending).unwrap();
        assert_eq!(associated, vec![reference.clone()]);
        assert!(pending.is_empty());

        let mut after_restart = VecDeque::new();
        let restored =
            attachment_refs_for_entry(&entry, Some(&store), &session_id, &mut after_restart)
                .unwrap();
        assert_eq!(restored, vec![reference]);
    }

    #[test]
    fn steered_prompt_attribution_is_exact_live_and_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("steer-attribution.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append_with_metadata(
                EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::Text("composed original context".into())],
                })),
                Some(EntryMetadata {
                    display_text: Some("original prompt".into()),
                    ..EntryMetadata::default()
                }),
            )
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("working".into())],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        session
            .append_with_metadata(
                EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::Text("steer exact text".into())],
                })),
                Some(EntryMetadata {
                    prompt_model: Some(ModelId("test-model".into())),
                    ..EntryMetadata::default()
                }),
            )
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("done".into())],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();

        let session_id = SessionId::new("steer-attribution").unwrap();
        let mut projection = ProjectionState::new(0);
        projection
            .pending_user_items
            .push_back(ItemId::new("live-original").unwrap());
        projection
            .pending_user_items
            .push_back(ItemId::new("live-steer").unwrap());
        let live = project_new_entries(
            &session,
            &mut projection,
            Some(&RunId::new("run-1-1").unwrap()),
            None,
            &session_id,
        )
        .unwrap();
        let live_users = live
            .iter()
            .filter_map(|item| match &item.payload {
                ItemPayload::UserMessage { text, .. } => Some((item.id.as_str(), text.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            live_users,
            vec![
                ("live-original", "original prompt"),
                ("live-steer", "steer exact text")
            ]
        );
        drop(session);

        let reopened = Session::open_read_only(&path).unwrap();
        let seed = seed_from_session(
            &reopened,
            session_id,
            SessionSeedOptions {
                project_id: None,
                model: ModelSelection {
                    provider: "test".into(),
                    model: "test-model".into(),
                    reasoning: "off".into(),
                },
                authority: AuthorityProfile::FullAccess,
                generation: 2,
                meta: None,
                attachment_store: None,
            },
        )
        .unwrap();
        let replayed_users = seed
            .snapshot
            .items
            .iter()
            .filter_map(|item| match &item.payload {
                ItemPayload::UserMessage { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(replayed_users, vec!["original prompt", "steer exact text"]);
    }

    #[tokio::test]
    async fn accepted_steer_is_visible_immediately_with_its_own_identity() {
        let run_id = RunId::new("run-1-1").unwrap();
        let mut projection = ProjectionState::new(0);
        projection.begin_run();
        let (sender, mut receiver) = mpsc::channel(1);

        publish_control_user_item(
            &run_id,
            "steer exact text".into(),
            Vec::new(),
            &mut projection,
            &sender,
        )
        .await
        .unwrap();

        let started = receiver.recv().await.expect("live steer event");
        let EventPayload::ItemStarted { item } = started.payload else {
            panic!("accepted steer did not start a visible item");
        };
        assert_eq!(item.id.as_str(), "item-run-1-1-user-1-1");
        assert!(matches!(
            item.payload,
            ItemPayload::UserMessage {
                ref text,
                ref attachments,
            } if text == "steer exact text" && attachments.is_empty()
        ));
        assert_eq!(
            projection.pending_user_items.front().map(ItemId::as_str),
            Some(item.id.as_str())
        );
    }

    #[test]
    fn run_outcome_is_committed_live_and_replayed_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("outcome-replay.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("question".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("answer".into())],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        let known_entries = session.entries().len();
        let marker_id = session
            .append_run_outcome(SessionRunOutcome {
                status: SessionRunOutcomeStatus::Completed,
                message: None,
            })
            .unwrap();
        let session_id = SessionId::new("outcome-replay").unwrap();
        let mut projection = ProjectionState::new(known_entries);
        let live = project_new_entries(
            &session,
            &mut projection,
            Some(&RunId::new("run-1-1").unwrap()),
            None,
            &session_id,
        )
        .unwrap();
        let live_outcome = live
            .iter()
            .find(|item| matches!(&item.payload, ItemPayload::RunOutcome { .. }))
            .expect("live committed outcome");
        assert_eq!(live_outcome.lifecycle, ItemLifecycle::Committed);
        assert_eq!(
            live_outcome.durable_entry_id.as_ref().map(|id| id.as_str()),
            Some(marker_id.0.as_str())
        );
        let stable_item_id = live_outcome.id.clone();
        drop(session);

        let reopened = Session::open_read_only(&path).unwrap();
        let seed = seed_from_session(
            &reopened,
            session_id,
            SessionSeedOptions {
                project_id: None,
                model: ModelSelection {
                    provider: "test".into(),
                    model: "test-model".into(),
                    reasoning: "off".into(),
                },
                authority: AuthorityProfile::FullAccess,
                generation: 2,
                meta: None,
                attachment_store: None,
            },
        )
        .unwrap();
        let replayed = seed
            .snapshot
            .items
            .iter()
            .find(|item| matches!(&item.payload, ItemPayload::RunOutcome { .. }))
            .expect("replayed committed outcome");
        assert_eq!(replayed.id, stable_item_id);
        assert!(matches!(
            &replayed.payload,
            ItemPayload::RunOutcome {
                outcome: ygg_serve_backend::RunOutcome::Completed,
                message: None,
            }
        ));
    }

    fn catalog_model(index: usize) -> ModelSummary {
        ModelSummary {
            id: format!("model-{index:03}"),
            name: format!("Model {index:03}"),
            provider: "provider".into(),
            local: false,
            available: true,
            reasoning: vec!["off".into()],
            default_reasoning: Some("off".into()),
            input_modalities: vec![InputModality::Text],
        }
    }

    #[test]
    fn graphical_catalog_is_stably_bounded_and_retains_the_configured_model() {
        let forward = (0..300).map(catalog_model).collect::<Vec<_>>();
        let mut reverse = forward.clone();
        reverse.reverse();
        let configured = ModelId("model-299".into());

        let models = bound_graphical_models(forward, Some(&configured));
        assert_eq!(models, bound_graphical_models(reverse, Some(&configured)));
        assert_eq!(models.len(), MAX_GRAPHICAL_MODELS);
        assert!(models.iter().any(|summary| summary.id == configured.0));
        assert!(!models.iter().any(|summary| summary.id == "model-255"));

        let selected = models
            .iter()
            .find(|summary| summary.id == configured.0)
            .unwrap();
        let selection = selection_from_summary(selected);
        let session_id = SessionId::new("catalog-limit-session").unwrap();
        let seed = empty_seed(
            session_id.clone(),
            None,
            selection,
            AuthorityProfile::FullAccess,
            1,
        );
        let theme_id = ThemeId::new("catalog-limit-theme").unwrap();
        let bootstrap = HostBootstrap {
            protocol: PROTOCOL_VERSION,
            host: HostDescriptor {
                id: HostId::new("catalog-limit-host").unwrap(),
                name: "ygg test".into(),
            },
            capabilities: HostCapabilities::default(),
            catalog_cursor: CatalogCursor(1),
            models,
            authority_profiles: vec![AuthorityProfile::FullAccess],
            authority_ceiling: AuthorityProfile::FullAccess,
            themes: vec![ThemeOption {
                id: theme_id.clone(),
                theme: ThemeDto {
                    name: "Test".into(),
                    source: ThemeSourceClass::Bundled,
                    revision: 1,
                    scheme: ColorScheme::Dark,
                    density: ThemeDensity::Comfortable,
                    motion: ThemeMotion::Full,
                    typography: ThemeTypography {
                        body_family: "system-ui".into(),
                        mono_family: "ui-monospace".into(),
                        body_size: 17,
                        display_ratio_milli: 1235,
                    },
                    colors: BTreeMap::new(),
                    roles: BTreeMap::new(),
                },
            }],
            selected_theme_id: theme_id,
            projects: Vec::new(),
            sessions: vec![seed.summary],
            selected_session_id: session_id,
            selected_session: seed.snapshot,
        };
        bootstrap.validate().unwrap();
    }
}
