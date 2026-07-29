#![allow(missing_docs)]

//! Default-off adapter from the graphical host contracts to the real Ygg App.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use sexy_tui_rs::{Color as TuiColor, TextStyle as TuiTextStyle};
use sha2::{Digest as _, Sha256};
use tokio::sync::{mpsc, oneshot};
use ygg_agent::{
    AgentEvent, Entry, EntryId, EntryValue, FinishReason, InputPart, OutputChannel, RunControl,
    Session, SessionRunOutcome, SessionRunOutcomeStatus, ToolError, ToolOutput, ToolProgress,
    UserInput,
};
use ygg_ai::{
    AssistantPart, ImageSource, Media, Message, Modality, Model, ModelCatalog, ModelId,
    ReasoningConfig, ToolCallId, ToolResultPart, UserPart,
};
use ygg_serve_backend::{
    parse_test_output, refresh_repository_context, ActivityPhase, ActivityPhaseSummary,
    ActorOwnerState, ArtifactId, ArtifactKind, ArtifactRef, AttachmentError, AttachmentFingerprint,
    AttachmentPolicy, AttachmentRef, AttachmentStore, AttentionState, AuthorityProfile,
    ColorScheme, CompletionReview, ContextUsage, ConversationBranchOperation,
    ConversationBranchProvenance, CreateSessionRequest, DocumentReference, DocumentStore,
    DocumentStoreError, DriverCommandOutcome, DurableEntryId, EventPayload, EvidenceCoverage,
    FileChange, FileEntryId, FinalizeCompletion, FinalizeDecision, HostCapabilities,
    HostDescriptor, HostId, HostService, InferenceRequest, InferenceRequestStore, InputModality,
    ItemDelta, ItemId, ItemLifecycle, ItemPayload, LifetimeUsage, LoopbackConfig, LoopbackServer,
    ModelInputPricing, ModelInputPricingTier, ModelSelection, ModelSummary, PendingRequest,
    PermanentDeleteConfirmation, ProjectId, ProjectRegistry, ProjectRegistryError, ProjectSummary,
    PromptInput, ProtocolValidation, RegistryProjectId, RegistryProjectState,
    RepositoryContextError, RepositoryContextSnapshot, RequestAnswer, RequestId, RequestKind,
    RequestState, RunId, SearchDocument, SearchDocumentKind, SearchError, SemanticRole,
    ServiceError, SessionBranchEntry, SessionBranchEntryKind, SessionBranchGraph,
    SessionCatalogState, SessionCommand, SessionCursor, SessionDriver, SessionId, SessionItem,
    SessionLiveState, SessionRetention, SessionSeed, SessionSnapshot, SessionSummary,
    SessionSupervisor, SourceId, SourceKind, SourceRef, StoredAttachment, StoredResource,
    StructuredTestResults, SupervisorConfig, TestCommandOutcome, TestCommandStatus, TestFramework,
    TestOutputInput, ThemeColor, ThemeDensity, ThemeDto, ThemeId, ThemeMotion, ThemeOption,
    ThemeRoleStyle, ThemeSourceClass, ThemeTypography, TimestampedEvent, ToolActivity,
    ToolActivityStatus, ToolKind, ToolResultSummary, TranscriptSearchIndex,
    TranscriptSearchRequest, TranscriptSearchResult, TrustedFileEntry, TrustedFileError,
    TrustedFileIndexSummary, TrustedFileRead, TrustedFileSearchResult, TrustedProjectFiles, TurnId,
    UsageActivity, UsagePeriod, UsageSnapshot, UsageStats, UsageStoreError, UserMessageDelivery,
    MAX_ITEM_TEXT_BYTES, MAX_MODEL_INPUT_PRICING_TIERS, MAX_PROMPT_BYTES, MAX_TEST_OUTPUT_BYTES,
};

use crate::app::bootstrap::{build_app, rebuild_app, LaunchSelection, SessionSelection};
use crate::app::{reasoning_label, supported_levels, App, Reconfig};
use crate::config::{self, Config};
use crate::resources::compose_instructions;
use crate::session_store::{SessionMeta, SessionStorageLifecycle, SessionStore};

const DRIVER_MAILBOX_CAPACITY: usize = 64;
const DRIVER_EVENT_CAPACITY: usize = 512;
const MAX_GRAPHICAL_MODELS: usize = 256;
const MAX_PROJECTED_SESSION_ITEMS: usize = 9_000;
const MAX_PROJECTED_BRANCH_ENTRIES: usize = 2_048;
const MAX_BRANCH_DELTA_ENTRIES: usize = 128;
const MAX_GRAPHICAL_SESSION_EXPORT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OPAQUE_RESOURCE_BYTES: usize = 8 * 1024 * 1024;
const EXTERNAL_EFFECTS_WARNING: &str = "Conversation branching changes only Ygg's transcript. Filesystem, command, network, and other external effects from later work are not rolled back.";

static NEXT_ACTOR_GENERATION: AtomicU64 = AtomicU64::new(1);

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
    models: Vec<ModelSummary>,
    descriptor: HostDescriptor,
    projects: Arc<Mutex<ProjectRegistry>>,
    launch_project_id: ProjectId,
    themes: Vec<ThemeOption>,
    selected_theme_id: ThemeId,
    attachments: Option<AttachmentStore>,
    documents: Option<DocumentStore>,
    trusted_files: Arc<Mutex<HashMap<String, TrustedProjectFiles>>>,
    search_index: Arc<Mutex<TranscriptSearchIndex>>,
    resources: Option<ygg_serve_backend::ResourceStore>,
    usage: Arc<Mutex<InferenceRequestStore>>,
    serve_state_dir: PathBuf,
    #[cfg(test)]
    checkout_hooks: Arc<Mutex<VecDeque<CheckoutTestHooks>>>,
    #[cfg(test)]
    open_count: Arc<AtomicU64>,
}

struct ProjectContext {
    project_id: ProjectId,
    config: Config,
    sessions: SessionStore,
}

impl YggHost {
    fn new(config: Config) -> anyhow::Result<Self> {
        #[cfg(not(unix))]
        anyhow::bail!(
            "ygg serve project trust is unavailable on this platform because stable directory identity checks are not implemented"
        );
        let boot = crate::app::bootstrap::bootstrap(config.clone())?;
        let models = graphical_model_catalog(&boot.catalog, &config);
        if models.is_empty() {
            anyhow::bail!("no configured models are available for ygg serve");
        }
        let host_id = load_or_create_host_id(&config)?;
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
        let mut projects = ProjectRegistry::open(state_dir.join("projects"))?;
        let existing_session_ids = boot
            .sessions
            .list()
            .into_iter()
            .map(|meta| meta.id)
            .collect::<Vec<_>>();
        let launch_project = match projects.find_by_root(&config.workspace)? {
            Some(project) => {
                projects
                    .bind_sessions(&project.id, existing_session_ids.iter().map(String::as_str))?;
                project
            }
            None => projects.import_with_sessions(
                &config.workspace,
                Some(workspace_name),
                existing_session_ids.iter().map(String::as_str),
            )?,
        };
        if config.workspace_trusted && launch_project.state == RegistryProjectState::Untrusted {
            projects.grant_trust(&launch_project.id)?;
        }
        if projects.default_project().is_none()
            && launch_project.state != RegistryProjectState::Archived
        {
            projects.set_default(&launch_project.id)?;
        }
        let launch_project_id =
            ProjectId::new(launch_project.id.as_str()).map_err(anyhow::Error::msg)?;
        let attachments = match AttachmentStore::open(&state_dir) {
            Ok(store) => Some(store),
            Err(_) => {
                crate::output::stderr_line(
                    "warning: secure attachment storage is unavailable; image uploads are disabled",
                );
                None
            }
        };
        let documents = match DocumentStore::open(&state_dir) {
            Ok(store) => Some(store),
            Err(_) => {
                crate::output::stderr_line(
                    "warning: secure document storage is unavailable; text, Markdown, and PDF uploads are disabled",
                );
                None
            }
        };
        let resources = match ygg_serve_backend::ResourceStore::open(&state_dir) {
            Ok(store) => Some(store),
            Err(_) => {
                crate::output::stderr_line(
                    "warning: secure evidence storage is unavailable; durable sources and outputs are disabled",
                );
                None
            }
        };
        let mut usage = InferenceRequestStore::open(&state_dir)?;
        backfill_usage_store(&config, &projects, &mut usage)?;
        Ok(Self {
            config,
            catalog: boot.catalog,
            models,
            descriptor,
            projects: Arc::new(Mutex::new(projects)),
            launch_project_id,
            themes,
            selected_theme_id,
            attachments,
            documents,
            trusted_files: Arc::new(Mutex::new(HashMap::new())),
            search_index: Arc::new(Mutex::new(TranscriptSearchIndex::new())),
            resources,
            usage: Arc::new(Mutex::new(usage)),
            serve_state_dir: state_dir,
            #[cfg(test)]
            checkout_hooks: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(test)]
            open_count: Arc::new(AtomicU64::new(0)),
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

    fn project_context(
        &self,
        requested: Option<&ProjectId>,
    ) -> Result<ProjectContext, ServiceError> {
        let projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
        let registry_id = match requested {
            Some(project_id) => registry_project_id(project_id)?,
            None => {
                let default = projects.default_project().map(|project| project.id);
                let mut candidates = default.into_iter().collect::<Vec<_>>();
                let launch_project_id = registry_project_id(&self.launch_project_id)?;
                if !candidates.contains(&launch_project_id) {
                    candidates.push(launch_project_id);
                }
                for project_id in projects.list().into_iter().map(|project| project.id) {
                    if !candidates.contains(&project_id) {
                        candidates.push(project_id);
                    }
                }
                candidates
                    .into_iter()
                    .find(|project_id| projects.resolve_trusted_root(project_id).is_ok())
                    .ok_or(ServiceError::Unauthorized)?
            }
        };
        let root = projects
            .resolve_trusted_root(&registry_id)
            .map_err(project_registry_service_error)?;
        let project_id =
            ProjectId::new(registry_id.as_str()).map_err(|_| ServiceError::Internal)?;
        let mut config = self.config.clone();
        config.workspace = root.as_path().to_owned();
        config.invocation_cwd = root.as_path().to_owned();
        config.workspace_trusted = true;
        let sessions = SessionStore::new(&config.session_dir, root.as_path());
        Ok(ProjectContext {
            project_id,
            config,
            sessions,
        })
    }

    fn project_context_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<ProjectContext, ServiceError> {
        let project_id = {
            let projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
            projects
                .project_for_session(session_id.as_str())
                .ok_or(ServiceError::NotFound)?
        };
        let project_id = ProjectId::new(project_id.as_str()).map_err(|_| ServiceError::Internal)?;
        self.project_context(Some(&project_id))
    }

    fn storage_context_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<ProjectContext, ServiceError> {
        let projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
        let registry_id = projects
            .project_for_session(session_id.as_str())
            .ok_or(ServiceError::NotFound)?;
        let root = projects
            .resolve_root(&registry_id)
            .map_err(project_registry_service_error)?;
        let project_id =
            ProjectId::new(registry_id.as_str()).map_err(|_| ServiceError::Internal)?;
        let mut config = self.config.clone();
        config.workspace = root.as_path().to_owned();
        config.invocation_cwd = root.as_path().to_owned();
        config.workspace_trusted = false;
        let sessions = SessionStore::new(&config.session_dir, root.as_path());
        Ok(ProjectContext {
            project_id,
            config,
            sessions,
        })
    }

    fn driver_for_new(
        &self,
        request: CreateSessionRequest,
    ) -> Result<YggSessionDriver, ServiceError> {
        let context = self.project_context(request.project_id.as_ref())?;
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
        let session_path = context.sessions.new_path(&crate::modes::timestamp());
        let session_id = session_id_from_path(&session_path)?;
        {
            let mut projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
            let registry_id = registry_project_id(&context.project_id)?;
            projects
                .bind_session(session_id.as_str(), &registry_id)
                .map_err(project_registry_service_error)?;
        }
        let generation = next_actor_generation();
        let selection = selection_for_model(&resolved, &reasoning, &context.config);
        let project_id = Some(context.project_id.clone());
        let seed = empty_seed(
            session_id,
            project_id.clone(),
            selection.clone(),
            request.authority,
            generation,
        );
        let plan = WorkerPlan {
            config: context.config,
            sessions: context.sessions,
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
            project_id,
            attachments: self.attachments.clone(),
            documents: self.documents.clone(),
            projects: Arc::clone(&self.projects),
            trusted_files: Arc::clone(&self.trusted_files),
            search_index: Arc::clone(&self.search_index),
            resources: self.resources.clone(),
            usage: Arc::clone(&self.usage),
            #[cfg(test)]
            checkout_hooks: CheckoutTestHooks::default(),
        };
        Ok(YggSessionDriver::spawn(seed, plan, 0))
    }

    fn driver_for_existing(
        &self,
        session_id: &SessionId,
    ) -> Result<YggSessionDriver, ServiceError> {
        #[cfg(test)]
        self.open_count.fetch_add(1, Ordering::Relaxed);
        let context = self.project_context_for_session(session_id)?;
        if session_meta_for_id(&context.sessions, session_id)
            .is_some_and(|meta| meta.trashed_at_ms.is_some())
        {
            return Err(ServiceError::InvalidBoundary);
        }
        let path = context
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
                workspace: &context.config.workspace,
                project_id: Some(context.project_id.clone()),
                model: selection.clone(),
                authority: AuthorityProfile::FullAccess,
                generation,
                meta: session_meta_for_id(&context.sessions, session_id),
                attachment_store: self.attachments.as_ref(),
                resource_store: self.resources.as_ref(),
            },
        )?;
        let reasoning =
            config::parse_reasoning(&selection.reasoning).map_err(|_| ServiceError::InvalidSeed)?;
        let plan = WorkerPlan {
            config: context.config,
            sessions: context.sessions,
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
            project_id: Some(context.project_id),
            attachments: self.attachments.clone(),
            documents: self.documents.clone(),
            projects: Arc::clone(&self.projects),
            trusted_files: Arc::clone(&self.trusted_files),
            search_index: Arc::clone(&self.search_index),
            resources: self.resources.clone(),
            usage: Arc::clone(&self.usage),
            #[cfg(test)]
            checkout_hooks: self
                .checkout_hooks
                .lock()
                .map_err(|_| ServiceError::Internal)?
                .pop_front()
                .unwrap_or_default(),
        };
        let known_entries = session.entries().len();
        Ok(YggSessionDriver::spawn(seed, plan, known_entries))
    }
}

fn backfill_usage_store(
    config: &Config,
    projects: &ProjectRegistry,
    usage: &mut InferenceRequestStore,
) -> anyhow::Result<()> {
    for project in projects.list() {
        let Ok(root) = projects.resolve_root(&project.id) else {
            continue;
        };
        let sessions = SessionStore::new(&config.session_dir, root.as_path());
        let bound = projects
            .sessions_for_project(&project.id)
            .into_iter()
            .collect::<BTreeSet<_>>();
        for meta in sessions.list() {
            if !bound.contains(&meta.id) {
                continue;
            }
            let Ok(session) = Session::open_read_only(&meta.path) else {
                continue;
            };
            usage.record_all(project_session_usage(&meta.id, &session)?)?;
        }
    }
    Ok(())
}

fn project_session_usage(
    session_id: &str,
    session: &Session,
) -> Result<Vec<InferenceRequest>, UsageStoreError> {
    session
        .usage_records()
        .iter()
        .enumerate()
        .map(|(ordinal, record)| {
            let request_ordinal =
                u64::try_from(ordinal).map_err(|_| UsageStoreError::InvalidRecord)?;
            Ok(InferenceRequest {
                session_id: session_id.to_owned(),
                request_ordinal,
                provider: record
                    .endpoint
                    .as_ref()
                    .map_or("unknown", |endpoint| endpoint.0.as_str())
                    .to_owned(),
                model: record
                    .model
                    .as_ref()
                    .map_or("unknown", |model| model.0.as_str())
                    .to_owned(),
                timestamp_ms: record.completed_at_unix_ms.unwrap_or_default(),
                prompt_tokens: record.usage.input_tokens,
                completion_tokens: record.usage.output_tokens,
                cache_read_tokens: record.usage.cache_read_tokens,
                cache_write_tokens: record.usage.cache_write_tokens,
                cache_write_1h_tokens: record.usage.cache_write_1h_tokens,
                reasoning_tokens: record.usage.reasoning_tokens,
                total_tokens: record.usage.total_tokens,
            })
        })
        .collect()
}

fn sync_session_usage(
    usage: &Arc<Mutex<InferenceRequestStore>>,
    session_id: &SessionId,
    session: &Session,
) -> Result<(), ServiceError> {
    let requests =
        project_session_usage(session_id.as_str(), session).map_err(usage_store_service_error)?;
    usage
        .lock()
        .map_err(|_| ServiceError::Internal)?
        .record_all(requests)
        .map_err(usage_store_service_error)?;
    Ok(())
}

fn usage_store_service_error(error: UsageStoreError) -> ServiceError {
    match error {
        UsageStoreError::QuotaExceeded => ServiceError::Unavailable,
        UsageStoreError::InvalidRecord
        | UsageStoreError::Conflict
        | UsageStoreError::Corrupt
        | UsageStoreError::Storage => ServiceError::Internal,
    }
}

fn registry_project_id(project_id: &ProjectId) -> Result<RegistryProjectId, ServiceError> {
    RegistryProjectId::parse(project_id.as_str()).map_err(project_registry_service_error)
}

fn project_registry_service_error(error: ProjectRegistryError) -> ServiceError {
    match error {
        ProjectRegistryError::ProjectNotFound => ServiceError::NotFound,
        ProjectRegistryError::ProjectUntrusted => ServiceError::Unauthorized,
        ProjectRegistryError::ProjectArchived => ServiceError::InvalidBoundary,
        ProjectRegistryError::RootUnavailable
        | ProjectRegistryError::RootIdentityChanged
        | ProjectRegistryError::RootSymlink
        | ProjectRegistryError::RootNotDirectory => ServiceError::Unavailable,
        ProjectRegistryError::RelativePath
        | ProjectRegistryError::PathTraversal
        | ProjectRegistryError::InvalidProjectId
        | ProjectRegistryError::ProjectLimitReached
        | ProjectRegistryError::InvalidDisplayName
        | ProjectRegistryError::InvalidCanonicalRoot
        | ProjectRegistryError::RootOverlapsState
        | ProjectRegistryError::DuplicateRoot
        | ProjectRegistryError::InvalidSessionId
        | ProjectRegistryError::SessionAlreadyBound
        | ProjectRegistryError::SessionBindingLimitReached => ServiceError::InvalidBoundary,
        ProjectRegistryError::StateParentUnavailable
        | ProjectRegistryError::UnsafeStatePath
        | ProjectRegistryError::UnsafePermissions
        | ProjectRegistryError::StateTooLarge
        | ProjectRegistryError::CorruptState
        | ProjectRegistryError::UnsupportedStateVersion
        | ProjectRegistryError::RevisionExhausted
        | ProjectRegistryError::RandomnessUnavailable
        | ProjectRegistryError::Storage(_) => ServiceError::Internal,
    }
}

fn document_store_service_error(error: DocumentStoreError) -> ServiceError {
    match error {
        DocumentStoreError::InvalidAssociation
        | DocumentStoreError::InvalidDocumentId
        | DocumentStoreError::Ingest(_) => ServiceError::InvalidBoundary,
        DocumentStoreError::QuotaExceeded => ServiceError::Unavailable,
        DocumentStoreError::PromptLimitExceeded => ServiceError::PayloadTooLarge,
        DocumentStoreError::NotFound => ServiceError::NotFound,
        DocumentStoreError::Corrupt => ServiceError::CorruptResource,
        DocumentStoreError::Storage => ServiceError::Internal,
    }
}

fn trusted_file_service_error(error: TrustedFileError) -> ServiceError {
    match error {
        TrustedFileError::TrustRequired => ServiceError::Unauthorized,
        TrustedFileError::RootChanged
        | TrustedFileError::ChangedSinceIndex
        | TrustedFileError::Storage => ServiceError::Unavailable,
        TrustedFileError::NotFound => ServiceError::NotFound,
        TrustedFileError::InvalidEntryId
        | TrustedFileError::InvalidSearch
        | TrustedFileError::NotText => ServiceError::InvalidBoundary,
        TrustedFileError::ContextLimitExceeded => ServiceError::PayloadTooLarge,
    }
}

fn repository_context_service_error(error: RepositoryContextError) -> ServiceError {
    match error {
        RepositoryContextError::TrustRequired => ServiceError::Unauthorized,
        RepositoryContextError::RootChanged => ServiceError::Unavailable,
    }
}

fn transcript_search_service_error(error: SearchError) -> ServiceError {
    match error {
        SearchError::EmptyQuery
        | SearchError::TooLarge
        | SearchError::InvalidText
        | SearchError::InvalidLimit
        | SearchError::InvalidLimits => ServiceError::InvalidBoundary,
        SearchError::Capacity => ServiceError::Unavailable,
    }
}

fn search_document_for_item(
    session_id: &SessionId,
    session_title: &str,
    fallback_timestamp_ms: u64,
    item: &SessionItem,
) -> Option<SearchDocument> {
    if item.lifecycle != ItemLifecycle::Committed {
        return None;
    }
    let (kind, text, timestamp_ms) = match &item.payload {
        ItemPayload::UserMessage {
            text,
            attachments,
            documents,
            project_files,
            ..
        } => {
            let mut visible = Vec::new();
            if !text.trim().is_empty() {
                visible.push(text.clone());
            }
            visible.extend(
                attachments
                    .iter()
                    .map(|attachment| attachment.display_name.clone()),
            );
            visible.extend(
                documents
                    .iter()
                    .map(|document| document.display_name.clone()),
            );
            visible.extend(project_files.iter().map(|file| file.relative_path.clone()));
            (
                SearchDocumentKind::User,
                visible.join("\n"),
                fallback_timestamp_ms,
            )
        }
        ItemPayload::AssistantMessage { text } => (
            SearchDocumentKind::Assistant,
            text.clone(),
            fallback_timestamp_ms,
        ),
        ItemPayload::ToolCall(activity) => {
            let text = [
                Some(activity.title.as_str()),
                activity.summary.as_deref(),
                activity.target.as_deref(),
                activity.output_summary.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
            (
                if activity.status == ToolActivityStatus::Failed {
                    SearchDocumentKind::Error
                } else {
                    SearchDocumentKind::Tool
                },
                text,
                activity.completed_at_ms.unwrap_or(activity.started_at_ms),
            )
        }
        ItemPayload::ToolResult(result) => (
            if result.status == ToolActivityStatus::Failed {
                SearchDocumentKind::Error
            } else {
                SearchDocumentKind::Tool
            },
            [
                Some(result.summary.as_str()),
                result.output_summary.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n"),
            result.completed_at_ms,
        ),
        ItemPayload::RunOutcome {
            outcome: ygg_serve_backend::RunOutcome::Failed,
            message,
            ..
        } => (
            SearchDocumentKind::Error,
            message
                .clone()
                .unwrap_or_else(|| "The run failed.".to_owned()),
            fallback_timestamp_ms,
        ),
        ItemPayload::Source(source) if source.kind == SourceKind::Attachment => (
            SearchDocumentKind::Attachment,
            source.title.clone(),
            source.consulted_at_ms,
        ),
        _ => return None,
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(SearchDocument {
        session_id: session_id.as_str().to_owned(),
        item_id: item.id.as_str().to_owned(),
        kind,
        session_title: bounded_text(session_title, 512),
        text: bounded_text(&text, ygg_serve_backend::MAX_SEARCH_DOCUMENT_TEXT_BYTES),
        timestamp_ms,
    })
}

fn search_documents_for_seed(seed: &SessionSeed) -> Vec<SearchDocument> {
    seed.snapshot
        .items
        .iter()
        .filter_map(|item| {
            search_document_for_item(
                &seed.snapshot.session_id,
                &seed.summary.title,
                seed.summary.modified_at_ms,
                item,
            )
        })
        .collect()
}

fn with_trusted_project_files<T>(
    projects: &Arc<Mutex<ProjectRegistry>>,
    trusted_files: &Arc<Mutex<HashMap<String, TrustedProjectFiles>>>,
    project_id: &ProjectId,
    operation: impl FnOnce(&TrustedProjectFiles, &ProjectRegistry) -> Result<T, TrustedFileError>,
) -> Result<T, ServiceError> {
    let registry_id = registry_project_id(project_id)?;
    let projects = projects.lock().map_err(|_| ServiceError::Internal)?;
    let service = {
        let mut services = trusted_files.lock().map_err(|_| ServiceError::Internal)?;
        match services.get(registry_id.as_str()) {
            Some(service) => service.clone(),
            None => {
                let service = TrustedProjectFiles::open(&projects, &registry_id)
                    .map_err(trusted_file_service_error)?;
                services.insert(registry_id.as_str().to_owned(), service.clone());
                service
            }
        }
    };
    operation(&service, &projects).map_err(trusted_file_service_error)
}

fn public_project_summary(
    registry: &ProjectRegistry,
    project: ygg_serve_backend::RegistryProjectSummary,
) -> Result<ProjectSummary, ServiceError> {
    let session_count = registry
        .sessions_for_project(&project.id)
        .len()
        .min(u32::MAX as usize) as u32;
    Ok(ProjectSummary {
        id: ProjectId::new(project.id.as_str()).map_err(|_| ServiceError::Internal)?,
        name: ygg_serve_backend::sanitize_public_text(&project.display_name, 256, false),
        trusted: project.state == RegistryProjectState::Trusted,
        archived: project.state == RegistryProjectState::Archived,
        available: project.available,
        is_default: project.is_default,
        session_count,
        live_session_count: 0,
    })
}

fn export_session_bytes(
    sessions: &SessionStore,
    session_id: &SessionId,
    serve_state_dir: &Path,
    max_bytes: usize,
) -> Result<bytes::Bytes, ServiceError> {
    sessions
        .path_by_id(session_id.as_str())
        .map_err(|_| ServiceError::NotFound)?;
    let serve_state_dir = serve_state_dir
        .canonicalize()
        .map_err(|_| ServiceError::Internal)?;
    let temporary = tempfile::Builder::new()
        .prefix(".session-export-")
        .tempdir_in(&serve_state_dir)
        .map_err(|_| ServiceError::Internal)?;
    let destination = temporary.path().join("session.json");
    let report = crate::session_commands::export_portable(
        sessions,
        session_id.as_str(),
        Some(destination),
        temporary.path(),
        false,
        false,
    )
    .map_err(|_| ServiceError::Internal)?;
    if report.included_secrets {
        return Err(ServiceError::Internal);
    }
    let bytes =
        match ygg_agent::secure_fs::read_regular_file_bounded(&report.destination, max_bytes) {
            Ok(bytes) => bytes,
            Err(ygg_agent::secure_fs::SecureFileError::TooLarge { .. }) => {
                return Err(ServiceError::PayloadTooLarge)
            }
            Err(_) => return Err(ServiceError::Internal),
        };
    Ok(bytes::Bytes::from(bytes))
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
            opaque_resources: self.resources.is_some(),
            attachments: attachment_policy.is_some(),
            attachment_policy,
            documents: self.documents.is_some(),
            trusted_project_files: cfg!(unix),
            transcript_search: true,
            previews: false,
            connected_devices: false,
            session_metadata: true,
            session_branches: true,
            conversation_branching: true,
            session_trash: true,
            session_export: true,
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

    fn document_ingest_supported(&self) -> bool {
        self.documents.is_some()
    }

    async fn ingest_document(
        &self,
        session_id: &SessionId,
        display_name: &str,
        media_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<DocumentReference, ServiceError> {
        let context = self.project_context_for_session(session_id)?;
        let store = self.documents.clone().ok_or(ServiceError::Unavailable)?;
        store
            .ingest_async(
                context.project_id.as_str().to_owned(),
                session_id.as_str().to_owned(),
                display_name.to_owned(),
                media_type.to_owned(),
                bytes,
            )
            .await
            .map_err(document_store_service_error)
    }

    async fn list_documents(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<DocumentReference>, ServiceError> {
        let context = self.project_context_for_session(session_id)?;
        self.documents
            .as_ref()
            .ok_or(ServiceError::Unavailable)?
            .list_for_session(context.project_id.as_str(), session_id.as_str())
            .map_err(document_store_service_error)
    }

    fn trusted_project_files_supported(&self) -> bool {
        cfg!(unix)
    }

    async fn trusted_file_index(
        &self,
        project_id: &ProjectId,
    ) -> Result<TrustedFileIndexSummary, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let trusted_files = Arc::clone(&self.trusted_files);
        let project_id = project_id.clone();
        tokio::task::spawn_blocking(move || {
            with_trusted_project_files(
                &projects,
                &trusted_files,
                &project_id,
                |service, registry| service.summary(registry),
            )
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn list_trusted_files(
        &self,
        project_id: &ProjectId,
        limit: usize,
    ) -> Result<Vec<TrustedFileEntry>, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let trusted_files = Arc::clone(&self.trusted_files);
        let project_id = project_id.clone();
        tokio::task::spawn_blocking(move || {
            with_trusted_project_files(
                &projects,
                &trusted_files,
                &project_id,
                |service, registry| service.list(registry, limit),
            )
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn search_trusted_files(
        &self,
        project_id: &ProjectId,
        query: &str,
        limit: usize,
    ) -> Result<TrustedFileSearchResult, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let trusted_files = Arc::clone(&self.trusted_files);
        let project_id = project_id.clone();
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || {
            with_trusted_project_files(
                &projects,
                &trusted_files,
                &project_id,
                |service, registry| service.search(registry, &query, limit),
            )
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn read_trusted_file(
        &self,
        project_id: &ProjectId,
        entry_id: &FileEntryId,
    ) -> Result<TrustedFileRead, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let trusted_files = Arc::clone(&self.trusted_files);
        let project_id = project_id.clone();
        let entry_id = entry_id.clone();
        tokio::task::spawn_blocking(move || {
            with_trusted_project_files(
                &projects,
                &trusted_files,
                &project_id,
                |service, registry| service.read(registry, &entry_id),
            )
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    fn transcript_search_supported(&self) -> bool {
        true
    }

    async fn search_transcripts(
        &self,
        request: &TranscriptSearchRequest,
    ) -> Result<TranscriptSearchResult, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let search_index = Arc::clone(&self.search_index);
        let base_config = self.config.clone();
        let catalog = self.catalog.clone();
        let fallback = self.default_selection()?;
        let attachments = self.attachments.clone();
        let resources = self.resources.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            let projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let mut rebuilt = TranscriptSearchIndex::new();
            for project in projects.list() {
                let Ok(root) = projects.resolve_trusted_root(&project.id) else {
                    continue;
                };
                let sessions = SessionStore::new(&base_config.session_dir, root.as_path());
                let bound = projects
                    .sessions_for_project(&project.id)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let public_project_id =
                    ProjectId::new(project.id.as_str()).map_err(|_| ServiceError::Internal)?;
                let mut project_config = base_config.clone();
                project_config.workspace = root.as_path().to_owned();
                project_config.invocation_cwd = root.as_path().to_owned();
                project_config.workspace_trusted = true;
                for meta in sessions.list() {
                    if !bound.contains(&meta.id) {
                        continue;
                    }
                    let Ok(session_id) = SessionId::new(meta.id.clone()) else {
                        continue;
                    };
                    let Ok(session) = Session::open_read_only(&meta.path) else {
                        continue;
                    };
                    let selection = selection_from_session(&session, &catalog, &project_config)
                        .unwrap_or_else(|_| fallback.clone());
                    let seed = seed_from_session(
                        &session,
                        session_id.clone(),
                        SessionSeedOptions {
                            workspace: &project_config.workspace,
                            project_id: Some(public_project_id.clone()),
                            model: selection,
                            authority: AuthorityProfile::FullAccess,
                            generation: 1,
                            meta: Some(meta),
                            attachment_store: attachments.as_ref(),
                            resource_store: resources.as_ref(),
                        },
                    )?;
                    rebuilt
                        .replace_session(session_id.as_str(), search_documents_for_seed(&seed))
                        .map_err(transcript_search_service_error)?;
                }
            }
            let result = rebuilt
                .search_request(&request)
                .map_err(transcript_search_service_error)?;
            *search_index.lock().map_err(|_| ServiceError::Internal)? = rebuilt;
            Ok(result)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    fn repository_context_supported(&self) -> bool {
        cfg!(unix)
    }

    async fn repository_context(
        &self,
        project_id: &ProjectId,
    ) -> Result<RepositoryContextSnapshot, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let project_id = registry_project_id(project_id)?;
        tokio::task::spawn_blocking(move || {
            let projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            refresh_repository_context(&projects, &project_id)
                .map_err(repository_context_service_error)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn resource_content(
        &self,
        session_id: &SessionId,
        handle: &str,
    ) -> Result<StoredResource, ServiceError> {
        self.resources
            .as_ref()
            .ok_or(ServiceError::Unavailable)?
            .content(session_id, handle)
            .map_err(resource_store_service_error)
    }

    async fn session_export(&self, session_id: &SessionId) -> Result<bytes::Bytes, ServiceError> {
        let sessions = self.project_context_for_session(session_id)?.sessions;
        let session_id = session_id.clone();
        let serve_state_dir = self.serve_state_dir.clone();
        tokio::task::spawn_blocking(move || {
            export_session_bytes(
                &sessions,
                &session_id,
                &serve_state_dir,
                MAX_GRAPHICAL_SESSION_EXPORT_BYTES,
            )
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn usage_stats(&self, period: UsagePeriod) -> Result<UsageStats, ServiceError> {
        Ok(self
            .usage
            .lock()
            .map_err(|_| ServiceError::Internal)?
            .stats(period))
    }

    async fn usage_lifetime(&self) -> Result<LifetimeUsage, ServiceError> {
        Ok(self
            .usage
            .lock()
            .map_err(|_| ServiceError::Internal)?
            .lifetime())
    }

    async fn usage_activity(&self) -> Result<UsageActivity, ServiceError> {
        Ok(self
            .usage
            .lock()
            .map_err(|_| ServiceError::Internal)?
            .activity())
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
        let projects = Arc::clone(&self.projects);
        tokio::task::spawn_blocking(move || {
            let projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            projects
                .list()
                .into_iter()
                .map(|project| public_project_summary(&projects, project))
                .collect()
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    fn project_lifecycle_mutations_supported(&self) -> bool {
        cfg!(unix)
    }

    fn project_import_supported(&self) -> bool {
        false
    }

    async fn import_project(
        &self,
        _candidate_id: &str,
        display_name: Option<&str>,
    ) -> Result<ProjectSummary, ServiceError> {
        let _ = display_name;
        // The browser transport has no native folder picker. Real roots are
        // imported from the trusted launch/CLI workspace; this command remains
        // unavailable until a host UI can mint one-use opaque candidates.
        Err(ServiceError::Unavailable)
    }

    async fn rename_project(
        &self,
        project_id: &ProjectId,
        display_name: &str,
    ) -> Result<ProjectSummary, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let project_id = registry_project_id(project_id)?;
        let display_name = display_name.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let project = projects
                .update_display_name(&project_id, &display_name)
                .map_err(project_registry_service_error)?;
            public_project_summary(&projects, project)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn set_default_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<ProjectSummary, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let project_id = registry_project_id(project_id)?;
        tokio::task::spawn_blocking(move || {
            let mut projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let project = projects
                .set_default(&project_id)
                .map_err(project_registry_service_error)?;
            public_project_summary(&projects, project)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn clear_default_project(&self) -> Result<(), ServiceError> {
        let projects = Arc::clone(&self.projects);
        tokio::task::spawn_blocking(move || {
            projects
                .lock()
                .map_err(|_| ServiceError::Internal)?
                .clear_default()
                .map_err(project_registry_service_error)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn set_project_trust(
        &self,
        project_id: &ProjectId,
        trusted: bool,
    ) -> Result<ProjectSummary, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let project_id = registry_project_id(project_id)?;
        tokio::task::spawn_blocking(move || {
            let mut projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let project = if trusted {
                projects.grant_trust(&project_id)
            } else {
                projects.revoke_trust(&project_id)
            }
            .map_err(project_registry_service_error)?;
            public_project_summary(&projects, project)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    async fn archive_project(
        &self,
        project_id: &ProjectId,
    ) -> Result<ProjectSummary, ServiceError> {
        let projects = Arc::clone(&self.projects);
        let project_id = registry_project_id(project_id)?;
        tokio::task::spawn_blocking(move || {
            let mut projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let project = projects
                .archive(&project_id)
                .map_err(project_registry_service_error)?;
            public_project_summary(&projects, project)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
    }

    fn session_trash_supported(&self) -> bool {
        true
    }

    async fn set_session_lifecycle(
        &self,
        session_id: &SessionId,
        lifecycle: SessionCatalogState,
        changed_at_ms: u64,
    ) -> Result<SessionSummary, ServiceError> {
        let context = self.storage_context_for_session(session_id)?;
        let storage_lifecycle = match lifecycle {
            SessionCatalogState::Active => SessionStorageLifecycle::Active,
            SessionCatalogState::Archived => SessionStorageLifecycle::Archived,
            SessionCatalogState::Trash => SessionStorageLifecycle::Trash,
        };
        context
            .sessions
            .set_lifecycle(session_id.as_str(), storage_lifecycle, changed_at_ms)
            .map_err(|_| ServiceError::Internal)?;
        let meta =
            session_meta_for_id(&context.sessions, session_id).ok_or(ServiceError::NotFound)?;
        let selection = Session::open_read_only(&meta.path)
            .ok()
            .and_then(|session| {
                selection_from_session(&session, &self.catalog, &context.config).ok()
            })
            .unwrap_or(self.default_selection()?);
        summary_from_meta(&meta, Some(context.project_id), selection)
    }

    async fn delete_session_permanently(
        &self,
        session_id: &SessionId,
        confirmation: &PermanentDeleteConfirmation,
    ) -> Result<(), ServiceError> {
        if &confirmation.session_id != session_id
            || confirmation.phrase != format!("permanently delete {}", session_id.as_str())
        {
            return Err(ServiceError::InvalidBoundary);
        }
        let context = self.storage_context_for_session(session_id)?;
        let previous_project = {
            let mut projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
            projects
                .unbind_session(session_id.as_str())
                .map_err(project_registry_service_error)?
        };
        if let Err(error) = context
            .sessions
            .delete_permanently(session_id.as_str(), confirmation.trashed_at_ms)
        {
            if let Some(project_id) = previous_project {
                let mut projects = self.projects.lock().map_err(|_| ServiceError::Internal)?;
                projects
                    .bind_session(session_id.as_str(), &project_id)
                    .map_err(project_registry_service_error)?;
            }
            let _ = error;
            return Err(ServiceError::InvalidBoundary);
        }
        if let Ok(mut search_index) = self.search_index.lock() {
            let _ = search_index.remove_session(session_id.as_str());
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ServiceError> {
        let fallback = self.default_selection()?;
        let projects = Arc::clone(&self.projects);
        let catalog = self.catalog.clone();
        let base_config = self.config.clone();
        tokio::task::spawn_blocking(move || {
            let projects = projects.lock().map_err(|_| ServiceError::Internal)?;
            let mut summaries = Vec::new();
            for project in projects.list() {
                if summaries.len() >= 2_000 || project.state == RegistryProjectState::Archived {
                    continue;
                }
                let Ok(root) = projects.resolve_root(&project.id) else {
                    continue;
                };
                let sessions = SessionStore::new(&base_config.session_dir, root.as_path());
                let bound = projects
                    .sessions_for_project(&project.id)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                let public_project_id =
                    ProjectId::new(project.id.as_str()).map_err(|_| ServiceError::Internal)?;
                let mut project_config = base_config.clone();
                project_config.workspace = root.as_path().to_owned();
                project_config.invocation_cwd = root.as_path().to_owned();
                project_config.workspace_trusted = project.state == RegistryProjectState::Trusted;
                for meta in sessions.list() {
                    if summaries.len() >= 2_000 || !bound.contains(&meta.id) {
                        continue;
                    }
                    let selection = Session::open_read_only(&meta.path)
                        .ok()
                        .and_then(|session| {
                            selection_from_session(&session, &catalog, &project_config).ok()
                        })
                        .unwrap_or_else(|| fallback.clone());
                    if let Ok(summary) =
                        summary_from_meta(&meta, Some(public_project_id.clone()), selection)
                    {
                        summaries.push(summary);
                    }
                }
            }
            Ok(summaries)
        })
        .await
        .map_err(|_| ServiceError::Internal)?
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
    commands: Option<mpsc::Sender<WorkerCommand>>,
    events: mpsc::Receiver<TimestampedEvent>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl YggSessionDriver {
    fn spawn(seed: SessionSeed, plan: WorkerPlan, known_entries: usize) -> Self {
        let (commands, command_receiver) = mpsc::channel(DRIVER_MAILBOX_CAPACITY);
        let (event_sender, events) = mpsc::channel(DRIVER_EVENT_CAPACITY);
        let worker = tokio::spawn(run_worker(
            plan,
            command_receiver,
            event_sender,
            known_entries,
        ));
        Self {
            seed,
            commands: Some(commands),
            events,
            worker: Some(worker),
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
            .as_ref()
            .ok_or(ServiceError::OwnerLost)?
            .send(WorkerCommand { command, response })
            .await
            .map_err(|_| ServiceError::Unavailable)?;
        receiver.await.map_err(|_| ServiceError::Unavailable)?
    }

    async fn next_event(&mut self) -> Option<TimestampedEvent> {
        self.events.recv().await
    }

    async fn shutdown(&mut self) {
        self.commands.take();
        self.events.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.await;
        }
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
    project_id: Option<ProjectId>,
    attachments: Option<AttachmentStore>,
    documents: Option<DocumentStore>,
    projects: Arc<Mutex<ProjectRegistry>>,
    trusted_files: Arc<Mutex<HashMap<String, TrustedProjectFiles>>>,
    search_index: Arc<Mutex<TranscriptSearchIndex>>,
    resources: Option<ygg_serve_backend::ResourceStore>,
    usage: Arc<Mutex<InferenceRequestStore>>,
    #[cfg(test)]
    checkout_hooks: CheckoutTestHooks,
}

#[cfg(test)]
#[derive(Clone, Default)]
struct CheckoutTestHooks {
    rollback_gate: Option<CheckoutRollbackGate>,
    corrupt_replacement_identity: bool,
    fail_seed_after_checkout: bool,
    fail_rollback: bool,
}

#[cfg(test)]
#[derive(Clone)]
struct CheckoutRollbackGate {
    entered: Arc<tokio::sync::Barrier>,
    release: Arc<tokio::sync::Barrier>,
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
    activity: ToolActivity,
    result: Option<ToolResultSummary>,
    turn_id: TurnId,
}

#[derive(Clone, Default)]
struct ProjectedToolProgress {
    observed_output_bytes: u64,
    dropped_output_bytes: u64,
}

struct CompletedToolEvidence {
    tool_call_id: String,
    tool_item_id: ItemId,
    turn_id: TurnId,
    tool: ProjectedToolCall,
    output: ToolOutput,
}

struct PendingUserItem {
    id: ItemId,
    delivery: UserMessageDelivery,
    turn_id: TurnId,
    documents: Vec<DocumentReference>,
    project_files: Vec<TrustedFileEntry>,
    branch_provenance: Option<ConversationBranchProvenance>,
}

struct ResolvedPromptInput {
    display_text: String,
    model_text: String,
    attachments: Vec<AttachmentRef>,
    documents: Vec<DocumentReference>,
    project_files: Vec<TrustedFileEntry>,
}

enum RunPromptInput {
    New(PromptInput),
    Replay(ResolvedPromptInput),
}

enum RunDriveOutcome {
    Admitted,
    Rejected {
        admission: oneshot::Sender<Result<DriverCommandOutcome, ServiceError>>,
        error: ServiceError,
    },
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
    completed_assistant_items: VecDeque<Option<(ItemId, TurnId)>>,
    completed_reasoning_items: VecDeque<Option<(ItemId, TurnId)>>,
    tool_items: HashMap<String, ItemId>,
    tool_calls: HashMap<String, ProjectedToolCall>,
    pending_tool_evidence: VecDeque<CompletedToolEvidence>,
    tool_progress: HashMap<String, ProjectedToolProgress>,
    test_results: Vec<StructuredTestResults>,
    item_turns: HashMap<ItemId, TurnId>,
    run_started_at_ms: u64,
    private_requests: HashMap<RequestId, PrivateRequest>,
    pending_attachments: VecDeque<Vec<AttachmentRef>>,
    pending_user_items: VecDeque<PendingUserItem>,
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
            pending_tool_evidence: VecDeque::new(),
            tool_progress: HashMap::new(),
            test_results: Vec::new(),
            item_turns: HashMap::new(),
            run_started_at_ms: now_ms(),
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
        self.test_results.clear();
        self.item_turns.clear();
        self.run_started_at_ms = now_ms();
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
        let turn_id = self
            .assistant_item
            .as_ref()
            .or(self.reasoning_item.as_ref())
            .and_then(|item_id| self.item_turns.get(item_id))
            .cloned();
        self.completed_assistant_items
            .push_back(self.assistant_item.take().zip(turn_id.clone()));
        self.completed_reasoning_items
            .push_back(self.reasoning_item.take().zip(turn_id));
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
                    RunPromptInput::New(input),
                    None,
                    &plan,
                    &mut projection,
                    &mut commands,
                    &events,
                    message.response,
                )
                .await
                {
                    Ok(RunDriveOutcome::Admitted) => app = Some(owned_app),
                    Ok(RunDriveOutcome::Rejected { admission, error }) => {
                        let _ = admission.send(Err(error));
                        app = Some(owned_app);
                    }
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
            SessionCommand::EditUserTurn {
                source_user_entry_id,
                input,
            } => {
                let owned_app = match app.take() {
                    Some(app) => app,
                    None => match build_worker_app(&plan) {
                        Ok(app) => app,
                        Err(_) => {
                            let _ = message.response.send(Err(ServiceError::Internal));
                            continue;
                        }
                    },
                };
                let source_entry = EntryId(source_user_entry_id.as_str().to_owned());
                if owned_app
                    .agent
                    .session()
                    .entry(&source_entry)
                    .is_none_or(|entry| !is_user_authored_entry(entry))
                {
                    app = Some(owned_app);
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                }
                let provenance = ConversationBranchProvenance {
                    operation: ConversationBranchOperation::EditUserTurn,
                    source_session_id: plan.session_id.clone(),
                    source_entry_id: source_user_entry_id,
                    originating_user_entry_id: None,
                    model_override: None,
                    external_effects_preserved: true,
                    warning: EXTERNAL_EFFECTS_WARNING.to_owned(),
                };
                match drive_sibling_conversation_branch(
                    owned_app,
                    source_entry,
                    RunPromptInput::New(input),
                    provenance,
                    None,
                    &mut plan,
                    &mut projection,
                    &mut commands,
                    &events,
                    message.response,
                )
                .await
                {
                    Ok((owned_app, post_ack_failed)) => {
                        app = Some(owned_app);
                        if post_ack_failed {
                            let _ = events
                                .send(event(EventPayload::SessionStateChanged {
                                    state: SessionLiveState::Failed,
                                    active_run_id: None,
                                }))
                                .await;
                        }
                    }
                    Err(_) => {
                        app = None;
                        break;
                    }
                }
            }
            SessionCommand::RetryResponse {
                source_assistant_entry_id,
                model,
            } => {
                let owned_app = match app.take() {
                    Some(app) => app,
                    None => match build_worker_app(&plan) {
                        Ok(app) => app,
                        Err(_) => {
                            let _ = message.response.send(Err(ServiceError::Internal));
                            continue;
                        }
                    },
                };
                let assistant_entry = EntryId(source_assistant_entry_id.as_str().to_owned());
                let source_user_entry =
                    match retry_originating_user_entry(owned_app.agent.session(), &assistant_entry)
                    {
                        Ok(entry) => entry,
                        Err(error) => {
                            app = Some(owned_app);
                            let _ = message.response.send(Err(error));
                            continue;
                        }
                    };
                let replay =
                    match replay_prompt_input(owned_app.agent.session(), &source_user_entry, &plan)
                    {
                        Ok(replay) => replay,
                        Err(error) => {
                            app = Some(owned_app);
                            let _ = message.response.send(Err(error));
                            continue;
                        }
                    };
                let originating_user_entry_id =
                    match DurableEntryId::new(source_user_entry.0.clone()) {
                        Ok(entry) => entry,
                        Err(_) => {
                            app = Some(owned_app);
                            let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                            continue;
                        }
                    };
                let provenance = ConversationBranchProvenance {
                    operation: ConversationBranchOperation::RetryResponse,
                    source_session_id: plan.session_id.clone(),
                    source_entry_id: source_assistant_entry_id,
                    originating_user_entry_id: Some(originating_user_entry_id),
                    model_override: model.clone(),
                    external_effects_preserved: true,
                    warning: EXTERNAL_EFFECTS_WARNING.to_owned(),
                };
                match drive_sibling_conversation_branch(
                    owned_app,
                    source_user_entry,
                    RunPromptInput::Replay(replay),
                    provenance,
                    model,
                    &mut plan,
                    &mut projection,
                    &mut commands,
                    &events,
                    message.response,
                )
                .await
                {
                    Ok((owned_app, post_ack_failed)) => {
                        app = Some(owned_app);
                        if post_ack_failed {
                            let _ = events
                                .send(event(EventPayload::SessionStateChanged {
                                    state: SessionLiveState::Failed,
                                    active_run_id: None,
                                }))
                                .await;
                        }
                    }
                    Err(_) => {
                        app = None;
                        break;
                    }
                }
            }
            SessionCommand::ForkConversation { entry_id } => {
                let owned_app = match app.take() {
                    Some(app) => app,
                    None => match build_worker_app(&plan) {
                        Ok(app) => app,
                        Err(_) => {
                            let _ = message.response.send(Err(ServiceError::Internal));
                            continue;
                        }
                    },
                };
                match create_conversation_fork(&owned_app, &plan, &entry_id) {
                    Ok(created_session_id) => {
                        let outcome = DriverCommandOutcome::fork(created_session_id.clone());
                        if message.response.send(Ok(outcome)).is_err() {
                            let _ = rollback_conversation_fork(&plan, &created_session_id);
                        }
                        app = Some(owned_app);
                    }
                    Err(error) => {
                        app = Some(owned_app);
                        let _ = message.response.send(Err(error));
                    }
                }
            }
            SessionCommand::Checkout { entry_id } => {
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
                let path = owned_app.agent.session().path().to_owned();
                let Some(previous_head) = owned_app.agent.session().head() else {
                    app = Some(owned_app);
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                };
                if owned_app
                    .agent
                    .session_mut()
                    .checkout(EntryId(entry_id.as_str().to_owned()))
                    .is_err()
                {
                    app = Some(owned_app);
                    let _ = message.response.send(Err(ServiceError::InvalidBoundary));
                    continue;
                }

                let selection = SessionSelection::OpenExisting(path.clone());
                let rebuilt =
                    match rebuild_app(owned_app, None, None, None, Some(selection.clone())) {
                        Ok(rebuilt) => rebuilt,
                        Err(_) => {
                            match checkout_rejection_after_rollback(
                                restore_checkout_owner(&path, previous_head, &plan),
                                ServiceError::Internal,
                            ) {
                                Ok((restored, rejection)) => {
                                    app = Some(restored);
                                    let _ = message.response.send(Err(rejection));
                                    continue;
                                }
                                Err(owner_lost) => {
                                    app = None;
                                    let _ = message.response.send(Err(owner_lost));
                                    break;
                                }
                            }
                        }
                    };
                let model = selection_for_model(&rebuilt.model, &rebuilt.reasoning, &plan.config);
                let replacement = seed_from_session(
                    rebuilt.agent.session(),
                    plan.session_id.clone(),
                    SessionSeedOptions {
                        workspace: &plan.config.workspace,
                        project_id: plan.project_id.clone(),
                        model,
                        authority: plan.authority,
                        generation: plan.actor_generation,
                        meta: session_meta_for_id(&plan.sessions, &plan.session_id),
                        attachment_store: plan.attachments.as_ref(),
                        resource_store: plan.resources.as_ref(),
                    },
                );
                #[cfg(test)]
                let mut replacement = replacement;
                #[cfg(test)]
                {
                    if plan.checkout_hooks.fail_seed_after_checkout {
                        replacement = Err(ServiceError::InvalidSeed);
                    } else if plan.checkout_hooks.corrupt_replacement_identity {
                        if let Ok(seed) = replacement.as_mut() {
                            let wrong =
                                SessionId::new("test-corrupt-replacement").expect("test ID");
                            seed.summary.id = wrong.clone();
                            seed.snapshot.session_id = wrong;
                        }
                    }
                }
                match replacement {
                    Ok(seed) => {
                        let (outcome, mut finalizer) = DriverCommandOutcome::guarded_replace(seed);
                        let _ = message.response.send(Ok(outcome));
                        match finalizer.decision().await {
                            Ok(FinalizeDecision::Commit) => {
                                plan.launch.model = rebuilt.model.spec.id.clone();
                                plan.launch.reasoning = rebuilt.reasoning.clone();
                                plan.launch.reasoning_mode = rebuilt.reasoning_mode;
                                plan.launch.session = selection;
                                projection.begin_run();
                                projection.known_entries = rebuilt.agent.session().entries().len();
                                app = Some(rebuilt);
                                let _ = finalizer.complete(Ok(FinalizeCompletion::Committed));
                            }
                            Ok(FinalizeDecision::Rollback) => {
                                wait_for_checkout_rollback_gate(&plan).await;
                                match rollback_checkout_candidate(
                                    rebuilt,
                                    &path,
                                    previous_head,
                                    &plan,
                                ) {
                                    Ok(restored) => {
                                        app = Some(restored);
                                        let _ =
                                            finalizer.complete(Ok(FinalizeCompletion::RolledBack));
                                    }
                                    Err(_) => {
                                        app = None;
                                        let _ = finalizer.complete(Err(ServiceError::OwnerLost));
                                    }
                                }
                            }
                            Err(_) => {
                                wait_for_checkout_rollback_gate(&plan).await;
                                app = rollback_checkout_candidate(
                                    rebuilt,
                                    &path,
                                    previous_head,
                                    &plan,
                                )
                                .ok();
                                if app.is_none() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        match checkout_rejection_after_rollback(
                            rollback_checkout_candidate(rebuilt, &path, previous_head, &plan),
                            error,
                        ) {
                            Ok((restored, rejection)) => {
                                app = Some(restored);
                                let _ = message.response.send(Err(rejection));
                            }
                            Err(owner_lost) => {
                                app = None;
                                let _ = message.response.send(Err(owner_lost));
                                break;
                            }
                        }
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
    let branch_start = projection.known_entries;
    for item in project_new_entries(
        app.agent.session(),
        &plan.config.workspace,
        projection,
        None,
        None,
        plan.attachments.as_ref(),
        &plan.session_id,
    )? {
        events.push(event(EventPayload::ItemCommitted { item }));
    }
    events.extend(branch_delta_events(app.agent.session(), branch_start)?);
    Ok(DriverCommandOutcome::with_events(events))
}

fn persist_idle_selection(
    plan: &mut WorkerPlan,
    projection: &mut ProjectionState,
    selection: ModelSelection,
) -> Result<DriverCommandOutcome, ServiceError> {
    let branch_start = projection.known_entries;
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
    plan.launch.session = SessionSelection::OpenExisting(path);
    let mut events = vec![event(EventPayload::SessionSettingsChanged {
        model: selection,
        authority: plan.authority,
    })];
    events.extend(branch_delta_events(&session, branch_start)?);
    Ok(DriverCommandOutcome::with_events(events))
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
    if let Ok(mut search_index) = plan.search_index.lock() {
        let _ = search_index.update_session_title(plan.session_id.as_str(), &title);
    }
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

fn restore_session_head(path: &std::path::Path, head: EntryId) -> Result<(), ServiceError> {
    let mut session = Session::open(path).map_err(|_| ServiceError::Internal)?;
    session.checkout(head).map_err(|_| ServiceError::Internal)
}

fn checkout_before_user_entry(
    session: &mut Session,
    source_user_entry_id: &EntryId,
) -> Result<(), ServiceError> {
    let source = session
        .entry(source_user_entry_id)
        .ok_or(ServiceError::InvalidBoundary)?;
    if !is_user_authored_entry(source) {
        return Err(ServiceError::InvalidBoundary);
    }
    match source.parent.clone() {
        Some(parent) => session
            .checkout(parent)
            .map_err(|_| ServiceError::InvalidBoundary),
        None => session
            .checkout_root()
            .map_err(|_| ServiceError::InvalidBoundary),
    }
}

fn is_user_authored_entry(entry: &Entry) -> bool {
    matches!(
        &entry.value,
        EntryValue::Message(Message::User(message))
            if !message.content.is_empty()
                && message
                    .content
                    .iter()
                    .all(|part| matches!(part, UserPart::Text(_) | UserPart::Media(_)))
    )
}

fn retry_originating_user_entry(
    session: &Session,
    source_assistant_entry_id: &EntryId,
) -> Result<EntryId, ServiceError> {
    let assistant = session
        .entry(source_assistant_entry_id)
        .ok_or(ServiceError::InvalidBoundary)?;
    if !matches!(&assistant.value, EntryValue::Message(Message::Assistant(_))) {
        return Err(ServiceError::InvalidBoundary);
    }
    let mut cursor = assistant.parent.as_ref();
    while let Some(id) = cursor {
        let entry = session.entry(id).ok_or(ServiceError::InvalidBoundary)?;
        if is_user_authored_entry(entry) {
            return Ok(entry.id.clone());
        }
        cursor = entry.parent.as_ref();
    }
    Err(ServiceError::InvalidBoundary)
}

fn replay_prompt_input(
    session: &Session,
    source_user_entry_id: &EntryId,
    plan: &WorkerPlan,
) -> Result<ResolvedPromptInput, ServiceError> {
    let entry = session
        .entry(source_user_entry_id)
        .ok_or(ServiceError::InvalidBoundary)?;
    let EntryValue::Message(Message::User(message)) = &entry.value else {
        return Err(ServiceError::InvalidBoundary);
    };
    if !is_user_authored_entry(entry) {
        return Err(ServiceError::InvalidBoundary);
    }
    let mut model_text = String::new();
    for part in &message.content {
        if let UserPart::Text(text) = part {
            model_text.push_str(text);
        }
    }
    let display_text = entry
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.display_text.clone())
        .unwrap_or_else(|| model_text.clone());
    let attachments = if message
        .content
        .iter()
        .any(|part| matches!(part, UserPart::Media(_)))
    {
        let store = plan.attachments.as_ref().ok_or(ServiceError::Unavailable)?;
        store
            .refs_for_entry(&plan.session_id, &entry.id.0)
            .map_err(attachment_service_error)?
            .ok_or(ServiceError::InvalidBoundary)?
    } else {
        Vec::new()
    };
    let (documents, project_files) = stored_prompt_context_for_entry(
        session,
        plan.resources.as_ref(),
        &plan.session_id,
        &entry.id.0,
    );
    Ok(ResolvedPromptInput {
        display_text,
        model_text,
        attachments,
        documents,
        project_files,
    })
}

fn stored_prompt_context_for_entry(
    session: &Session,
    resources: Option<&ygg_serve_backend::ResourceStore>,
    session_id: &SessionId,
    durable_entry_id: &str,
) -> (Vec<DocumentReference>, Vec<TrustedFileEntry>) {
    let Some(resources) = resources else {
        return (Vec::new(), Vec::new());
    };
    for entry in session.entries().iter().rev() {
        if !entry
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.run_outcome.is_some())
        {
            continue;
        }
        let Ok(outcome_entry_id) = DurableEntryId::new(entry.id.0.clone()) else {
            continue;
        };
        let Some(record) = load_stored_run_record(resources, session_id, &outcome_entry_id) else {
            continue;
        };
        if let Some(item) = record
            .items
            .into_iter()
            .find(|item| item.durable_entry_id == durable_entry_id)
        {
            return (item.documents, item.project_files);
        }
    }
    (Vec::new(), Vec::new())
}

async fn drive_sibling_conversation_branch(
    mut owned_app: App,
    source_user_entry_id: EntryId,
    input: RunPromptInput,
    provenance: ConversationBranchProvenance,
    model_override: Option<ModelSelection>,
    plan: &mut WorkerPlan,
    projection: &mut ProjectionState,
    commands: &mut mpsc::Receiver<WorkerCommand>,
    events: &mpsc::Sender<TimestampedEvent>,
    admission: oneshot::Sender<Result<DriverCommandOutcome, ServiceError>>,
) -> Result<(App, bool), ServiceError> {
    let path = owned_app.agent.session().path().to_owned();
    let previous_head = owned_app
        .agent
        .session()
        .head()
        .ok_or(ServiceError::InvalidBoundary)?;
    let (new_model, new_reasoning) = match model_override.as_ref() {
        Some(selection) => {
            let available = plan.available_models.iter().any(|model| {
                model.available
                    && model.provider == selection.provider
                    && model.id == selection.model
                    && model
                        .reasoning
                        .iter()
                        .any(|reasoning| reasoning == &selection.reasoning)
            });
            if !available {
                let _ = admission.send(Err(ServiceError::InvalidBoundary));
                return Ok((owned_app, false));
            }
            let model = match owned_app.catalog.resolve(&ModelId(selection.model.clone())) {
                Ok(model) => model,
                Err(_) => {
                    let _ = admission.send(Err(ServiceError::InvalidBoundary));
                    return Ok((owned_app, false));
                }
            };
            let reasoning = match config::parse_reasoning(&selection.reasoning) {
                Ok(reasoning) => reasoning,
                Err(_) => {
                    let _ = admission.send(Err(ServiceError::InvalidBoundary));
                    return Ok((owned_app, false));
                }
            };
            (Some(model), Some(reasoning))
        }
        None => (None, None),
    };
    if let Err(error) =
        checkout_before_user_entry(owned_app.agent.session_mut(), &source_user_entry_id)
    {
        let _ = admission.send(Err(error));
        return Ok((owned_app, false));
    }
    let selection = SessionSelection::OpenExisting(path.clone());
    let mut candidate = match rebuild_app(
        owned_app,
        new_model,
        new_reasoning,
        None,
        Some(selection.clone()),
    ) {
        Ok(candidate) => candidate,
        Err(_) => {
            let restored = restore_checkout_owner(&path, previous_head, plan)?;
            let _ = admission.send(Err(ServiceError::Internal));
            return Ok((restored, false));
        }
    };
    let previous_model = plan.launch.model.clone();
    let previous_reasoning = plan.launch.reasoning.clone();
    let previous_reasoning_mode = plan.launch.reasoning_mode;
    plan.launch.model = candidate.model.spec.id.clone();
    plan.launch.reasoning = candidate.reasoning.clone();
    plan.launch.reasoning_mode = candidate.reasoning_mode;
    plan.launch.session = selection;
    match start_and_drive_run(
        &mut candidate,
        input,
        Some(provenance),
        plan,
        projection,
        commands,
        events,
        admission,
    )
    .await
    {
        Ok(RunDriveOutcome::Admitted) => Ok((candidate, false)),
        Ok(RunDriveOutcome::Rejected { admission, error }) => {
            plan.launch.model = previous_model;
            plan.launch.reasoning = previous_reasoning;
            plan.launch.reasoning_mode = previous_reasoning_mode;
            let restored = rollback_checkout_candidate(candidate, &path, previous_head, plan)?;
            let _ = admission.send(Err(error));
            Ok((restored, false))
        }
        Err(_) => Ok((candidate, true)),
    }
}

fn create_conversation_fork(
    app: &App,
    plan: &WorkerPlan,
    source_entry_id: &DurableEntryId,
) -> Result<SessionId, ServiceError> {
    let source_entry = EntryId(source_entry_id.as_str().to_owned());
    let entry = app
        .agent
        .session()
        .entry(&source_entry)
        .ok_or(ServiceError::InvalidBoundary)?;
    if !matches!(
        &entry.value,
        EntryValue::Message(Message::User(_))
            | EntryValue::Message(Message::Assistant(_))
            | EntryValue::Compaction { .. }
    ) {
        return Err(ServiceError::InvalidBoundary);
    }
    let project_id = plan
        .project_id
        .as_ref()
        .ok_or(ServiceError::InvalidBoundary)
        .and_then(registry_project_id)?;
    let destination = plan.sessions.new_path(&crate::modes::timestamp());
    let created_session_id = session_id_from_path(&destination)?;
    let forked = app
        .agent
        .session()
        .fork_to(&destination, source_entry)
        .map_err(|_| ServiceError::Internal)?;
    drop(forked);
    if plan
        .sessions
        .set_fork_provenance(
            created_session_id.as_str(),
            plan.session_id.as_str(),
            source_entry_id.as_str(),
        )
        .is_err()
    {
        let _ = plan
            .sessions
            .discard_unacknowledged(created_session_id.as_str());
        return Err(ServiceError::Internal);
    }
    let mut projects = plan.projects.lock().map_err(|_| ServiceError::Internal)?;
    if let Err(error) = projects.bind_session(created_session_id.as_str(), &project_id) {
        drop(projects);
        let _ = plan
            .sessions
            .discard_unacknowledged(created_session_id.as_str());
        return Err(project_registry_service_error(error));
    }
    Ok(created_session_id)
}

fn rollback_conversation_fork(
    plan: &WorkerPlan,
    created_session_id: &SessionId,
) -> Result<(), ServiceError> {
    let previous_project = {
        let mut projects = plan.projects.lock().map_err(|_| ServiceError::Internal)?;
        projects
            .unbind_session(created_session_id.as_str())
            .map_err(project_registry_service_error)?
    };
    if let Err(error) = plan
        .sessions
        .discard_unacknowledged(created_session_id.as_str())
    {
        if let Some(project_id) = previous_project {
            let mut projects = plan.projects.lock().map_err(|_| ServiceError::Internal)?;
            projects
                .bind_session(created_session_id.as_str(), &project_id)
                .map_err(project_registry_service_error)?;
        }
        let _ = error;
        return Err(ServiceError::Internal);
    }
    Ok(())
}

fn rollback_checkout_candidate(
    mut candidate: App,
    path: &Path,
    previous_head: EntryId,
    plan: &WorkerPlan,
) -> Result<App, ServiceError> {
    candidate.executable_extensions.shutdown_blocking();
    drop(candidate);
    restore_checkout_owner(path, previous_head, plan)
}

fn restore_checkout_owner(
    path: &Path,
    previous_head: EntryId,
    plan: &WorkerPlan,
) -> Result<App, ServiceError> {
    #[cfg(test)]
    if plan.checkout_hooks.fail_rollback {
        return Err(ServiceError::Internal);
    }
    restore_session_head(path, previous_head)?;
    build_worker_app(plan).map_err(|_| ServiceError::Internal)
}

#[cfg(test)]
async fn wait_for_checkout_rollback_gate(plan: &WorkerPlan) {
    if let Some(gate) = &plan.checkout_hooks.rollback_gate {
        gate.entered.wait().await;
        gate.release.wait().await;
    }
}

#[cfg(not(test))]
async fn wait_for_checkout_rollback_gate(_plan: &WorkerPlan) {}

fn checkout_rejection_after_rollback<T>(
    rollback: Result<T, ServiceError>,
    rejection: ServiceError,
) -> Result<(T, ServiceError), ServiceError> {
    rollback
        .map(|owner| (owner, rejection))
        .map_err(|_| ServiceError::OwnerLost)
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

async fn resolve_prompt_input(
    plan: &WorkerPlan,
    input: PromptInput,
) -> Result<ResolvedPromptInput, ServiceError> {
    let PromptInput {
        text,
        attachments,
        document_ids,
        project_file_ids,
    } = input;
    let project_id = plan.project_id.as_ref();
    let document_context = if document_ids.is_empty() {
        None
    } else {
        let project_id = project_id.ok_or(ServiceError::Unauthorized)?.clone();
        let session_id = plan.session_id.clone();
        let store = plan.documents.clone().ok_or(ServiceError::Unavailable)?;
        Some(
            tokio::task::spawn_blocking(move || {
                store.prompt_context(project_id.as_str(), session_id.as_str(), &document_ids)
            })
            .await
            .map_err(|_| ServiceError::Internal)?
            .map_err(document_store_service_error)?,
        )
    };
    let project_file_context = if project_file_ids.is_empty() {
        None
    } else {
        let project_id = project_id.ok_or(ServiceError::Unauthorized)?.clone();
        let projects = Arc::clone(&plan.projects);
        let trusted_files = Arc::clone(&plan.trusted_files);
        Some(
            tokio::task::spawn_blocking(move || {
                with_trusted_project_files(
                    &projects,
                    &trusted_files,
                    &project_id,
                    |service, registry| service.attach_as_context(registry, &project_file_ids),
                )
            })
            .await
            .map_err(|_| ServiceError::Internal)??,
        )
    };
    let composed = ygg_serve_backend::compose_prompt_text(
        &text,
        document_context
            .as_ref()
            .map(|context| context.text.as_str()),
        project_file_context
            .as_ref()
            .map(|context| context.text.as_str()),
    )
    .map_err(|error| match error {
        ygg_serve_backend::PromptContextError::InvalidUserText
        | ygg_serve_backend::PromptContextError::InvalidDocumentContext
        | ygg_serve_backend::PromptContextError::InvalidProjectFileContext => {
            ServiceError::InvalidBoundary
        }
        ygg_serve_backend::PromptContextError::DocumentContextTooLarge
        | ygg_serve_backend::PromptContextError::ProjectFileContextTooLarge
        | ygg_serve_backend::PromptContextError::AuxiliaryContextTooLarge
        | ygg_serve_backend::PromptContextError::PromptTooLarge => ServiceError::PayloadTooLarge,
    })?;
    Ok(ResolvedPromptInput {
        display_text: text,
        model_text: composed.into_string(),
        attachments,
        documents: document_context
            .map(|context| context.documents)
            .unwrap_or_default(),
        project_files: project_file_context
            .map(|context| context.files)
            .unwrap_or_default(),
    })
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

fn resource_store_service_error(error: ygg_serve_backend::ResourceStoreError) -> ServiceError {
    match error {
        ygg_serve_backend::ResourceStoreError::InvalidBoundary => ServiceError::InvalidBoundary,
        ygg_serve_backend::ResourceStoreError::QuotaExceeded => ServiceError::Unavailable,
        ygg_serve_backend::ResourceStoreError::NotFound => ServiceError::NotFound,
        ygg_serve_backend::ResourceStoreError::Corrupt => ServiceError::CorruptResource,
        ygg_serve_backend::ResourceStoreError::Storage => ServiceError::Internal,
    }
}

async fn start_and_drive_run(
    app: &mut App,
    input: RunPromptInput,
    branch_provenance: Option<ConversationBranchProvenance>,
    plan: &WorkerPlan,
    projection: &mut ProjectionState,
    commands: &mut mpsc::Receiver<WorkerCommand>,
    events: &mpsc::Sender<TimestampedEvent>,
    admission: oneshot::Sender<Result<DriverCommandOutcome, ServiceError>>,
) -> Result<RunDriveOutcome, ServiceError> {
    if let Some(limit) = app.config.max_cost_microdollars {
        if app.agent.session().total_cost_microdollars() >= limit {
            return Ok(RunDriveOutcome::Rejected {
                admission,
                error: ServiceError::InvalidBoundary,
            });
        }
    }
    let (resolved, replay_exact) = match input {
        RunPromptInput::New(input) => match resolve_prompt_input(plan, input).await {
            Ok(resolved) => (resolved, false),
            Err(error) => {
                return Ok(RunDriveOutcome::Rejected { admission, error });
            }
        },
        RunPromptInput::Replay(resolved) => (resolved, true),
    };
    let ResolvedPromptInput {
        display_text,
        model_text,
        attachments,
        documents,
        project_files,
    } = resolved;
    let media = match resolve_attachment_media(app, plan, &attachments) {
        Ok(media) => media,
        Err(error) => {
            return Ok(RunDriveOutcome::Rejected { admission, error });
        }
    };
    let prompt = if replay_exact {
        model_text
    } else {
        match crate::prompts::render_configured(app, &model_text) {
            Err(_) => {
                return Ok(RunDriveOutcome::Rejected {
                    admission,
                    error: ServiceError::Internal,
                })
            }
            Ok(Some(rendered)) => rendered.text,
            Ok(None) => model_text,
        }
    };
    app.executable_extensions.refresh_host_state(
        app.agent.session(),
        &app.model,
        &app.reasoning,
        &app.sessions,
    );
    let (pending_context_count, model_prompt) = if replay_exact {
        app.agent.set_system_prompt(app.system.clone());
        (0, prompt)
    } else {
        let composition = match app
            .executable_extensions
            .compose_prompt(&app.system, prompt.clone())
            .await
        {
            Ok(composition) => composition,
            Err(_) => {
                return Ok(RunDriveOutcome::Rejected {
                    admission,
                    error: ServiceError::Internal,
                })
            }
        };
        let pending_context_count = composition.pending_context_count;
        let model_prompt = composition.prompt;
        app.agent.set_system_prompt(composition.system);
        (pending_context_count, model_prompt)
    };
    app.agent
        .set_prompt_display_text(Some(display_text.clone()));
    projection.begin_run();
    let run_id = match projection.next_run_id(plan.actor_generation) {
        Ok(run_id) => run_id,
        Err(error) => return Ok(RunDriveOutcome::Rejected { admission, error }),
    };
    let turn_id = match projection.turn_id(&run_id) {
        Ok(turn_id) => turn_id,
        Err(error) => return Ok(RunDriveOutcome::Rejected { admission, error }),
    };
    let user_item_id = match projection.provisional_id(&run_id, "user", 0) {
        Ok(item_id) => item_id,
        Err(error) => return Ok(RunDriveOutcome::Rejected { admission, error }),
    };
    projection
        .item_turns
        .insert(user_item_id.clone(), turn_id.clone());
    let mut input_parts = Vec::with_capacity(1 + media.len());
    if !model_prompt.is_empty() {
        input_parts.push(InputPart::Text(model_prompt));
    }
    input_parts.extend(media.into_iter().map(InputPart::Media));
    let mut run = match app.agent.prompt(UserInput::from(input_parts)).await {
        Ok(run) => run,
        Err(_) => {
            return Ok(RunDriveOutcome::Rejected {
                admission,
                error: ServiceError::Internal,
            });
        }
    };
    if !attachments.is_empty() {
        projection
            .pending_attachments
            .push_back(attachments.clone());
    }
    projection.pending_user_items.push_back(PendingUserItem {
        id: user_item_id.clone(),
        delivery: UserMessageDelivery::Submit,
        turn_id: turn_id.clone(),
        documents: documents.clone(),
        project_files: project_files.clone(),
        branch_provenance: branch_provenance.clone(),
    });
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
                    text: bounded_text(&display_text, MAX_PROMPT_BYTES),
                    attachments: attachments.clone(),
                    documents,
                    project_files,
                    delivery: Some(UserMessageDelivery::Submit),
                    branch_provenance,
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
    sync_session_usage(&plan.usage, &plan.session_id, app.agent.session())?;
    let settled_at_ms = now_ms();
    let unfinished = projection
        .tool_calls
        .iter()
        .filter_map(|(tool_call_id, tool)| {
            (tool.activity.status == ToolActivityStatus::Running).then(|| tool_call_id.clone())
        })
        .collect::<Vec<_>>();
    let mut stopped_updates = Vec::new();
    for tool_call_id in unfinished {
        let Some(item_id) = projection.tool_items.get(&tool_call_id).cloned() else {
            continue;
        };
        let progress = projection
            .tool_progress
            .remove(&tool_call_id)
            .unwrap_or_default();
        let Some(tool) = projection.tool_calls.get_mut(&tool_call_id) else {
            continue;
        };
        tool.activity.status = ToolActivityStatus::Stopped;
        tool.activity.summary = Some("Stopped".into());
        tool.activity.completed_at_ms = Some(settled_at_ms.max(tool.activity.started_at_ms));
        tool.activity.duration_ms = Some(settled_at_ms.saturating_sub(tool.activity.started_at_ms));
        tool.activity.output_summary = Some("Tool stopped before completion".into());
        tool.activity.observed_output_bytes = progress.observed_output_bytes;
        tool.activity.dropped_output_bytes = progress.dropped_output_bytes;
        tool.result = Some(ToolResultSummary {
            tool_call_item_id: item_id.clone(),
            status: ToolActivityStatus::Stopped,
            summary: "Stopped".into(),
            output_summary: tool.activity.output_summary.clone(),
            output_handle: None,
            exit_code: None,
            signal: None,
            completed_at_ms: tool.activity.completed_at_ms.unwrap_or(settled_at_ms),
            duration_ms: tool.activity.duration_ms.unwrap_or_default(),
            observed_output_bytes: tool.activity.observed_output_bytes,
            dropped_output_bytes: tool.activity.dropped_output_bytes,
        });
        stopped_updates.push((item_id, tool.activity.clone()));
    }
    for (item_id, activity) in stopped_updates {
        events
            .send(event(EventPayload::ItemDelta {
                item_id,
                delta: ItemDelta::ToolActivity { activity },
            }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    let mut changed_file_item_ids = BTreeSet::new();
    let mut source_ids = BTreeSet::new();
    let mut output_ids = BTreeSet::new();
    if let Some(resources) = plan.resources.as_ref() {
        let completed = std::mem::take(&mut projection.pending_tool_evidence);
        for completed in completed {
            let payloads = project_tool_evidence(
                app.agent.session(),
                &plan.config.workspace,
                resources,
                &plan.session_id,
                &run_id,
                &completed.turn_id,
                &completed.tool_call_id,
                &completed.tool_item_id,
                &completed.tool,
                &completed.output,
            );
            let mut changed_paths = BTreeSet::new();
            let mut linked_sources = BTreeSet::new();
            let mut linked_outputs = BTreeSet::new();
            for payload in &payloads {
                match payload {
                    EventPayload::SourceUpserted { source } => {
                        source_ids.insert(source.id.clone());
                        linked_sources.insert(source.id.clone());
                    }
                    EventPayload::ArtifactUpserted { artifact } => {
                        output_ids.insert(artifact.id.clone());
                        linked_outputs.insert(artifact.id.clone());
                    }
                    EventPayload::ItemCommitted { item } => match &item.payload {
                        ItemPayload::FileChange(change) => {
                            changed_file_item_ids.insert(item.id.clone());
                            changed_paths.insert(change.display_path.clone());
                        }
                        ItemPayload::Source(source) => {
                            source_ids.insert(source.id.clone());
                            linked_sources.insert(source.id.clone());
                        }
                        ItemPayload::Artifact(artifact) => {
                            output_ids.insert(artifact.id.clone());
                            linked_outputs.insert(artifact.id.clone());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if let Some(tool) = projection.tool_calls.get_mut(&completed.tool_call_id) {
                tool.activity.changed_paths = changed_paths.into_iter().collect();
                tool.activity.source_ids = linked_sources.into_iter().collect();
                tool.activity.artifact_ids = linked_outputs.into_iter().collect();
                events
                    .send(event(EventPayload::ItemDelta {
                        item_id: completed.tool_item_id.clone(),
                        delta: ItemDelta::ToolActivity {
                            activity: tool.activity.clone(),
                        },
                    }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
            }
            for payload in payloads {
                events
                    .send(event(payload))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
            }
        }
    } else {
        projection.pending_tool_evidence.clear();
    }
    let review = build_completion_review(
        &terminal,
        projection.run_started_at_ms,
        settled_at_ms,
        projection,
        changed_file_item_ids,
        source_ids,
        output_ids,
    );
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

    let branch_start = projection.known_entries;
    let committed = project_new_entries(
        app.agent.session(),
        &plan.config.workspace,
        projection,
        Some(&run_id),
        Some(&review),
        plan.attachments.as_ref(),
        &plan.session_id,
    )?;
    let search_title = session_meta_for_id(&plan.sessions, &plan.session_id)
        .map(|meta| meta.name.unwrap_or(meta.title))
        .unwrap_or_else(|| "Session".to_owned());
    if let Ok(mut search_index) = plan.search_index.lock() {
        for item in &committed {
            if let Some(document) =
                search_document_for_item(&plan.session_id, &search_title, settled_at_ms, item)
            {
                let _ = search_index.upsert_document(document);
            }
        }
    }
    if let Some(resources) = plan.resources.as_ref() {
        persist_run_projection(
            resources,
            &plan.session_id,
            &run_id,
            projection.run_started_at_ms,
            settled_at_ms,
            projection,
            &committed,
            &review,
        )?;
    }
    for item in committed {
        events
            .send(event(EventPayload::ItemCommitted { item }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    for pending in projection.pending_user_items.drain(..) {
        events
            .send(event(EventPayload::ItemRetracted {
                item_id: pending.id,
                provider_attempt: 1,
                reason: "Input was not delivered before the run ended.".into(),
            }))
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
    projection.pending_attachments.clear();
    for branch_event in branch_delta_events(app.agent.session(), branch_start)? {
        events
            .send(branch_event)
            .await
            .map_err(|_| ServiceError::Unavailable)?;
    }
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
    Ok(RunDriveOutcome::Admitted)
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
        SessionCommand::Steer { input } => match resolve_prompt_input(plan, input).await {
            Ok(resolved) => match resolve_control_input(
                plan,
                resolved.model_text.clone(),
                &resolved.attachments,
            ) {
                Ok(input) => match control.steer(input).await {
                    Ok(()) => {
                        publish_control_user_item(
                            run_id,
                            resolved,
                            UserMessageDelivery::Steer,
                            projection,
                            events,
                        )
                        .await
                    }
                    Err(_) => Err(ServiceError::InvalidBoundary),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
        SessionCommand::FollowUp { input } => match resolve_prompt_input(plan, input).await {
            Ok(resolved) => match resolve_control_input(
                plan,
                resolved.model_text.clone(),
                &resolved.attachments,
            ) {
                Ok(input) => match control.follow_up(input).await {
                    Ok(()) => {
                        publish_control_user_item(
                            run_id,
                            resolved,
                            UserMessageDelivery::FollowUp,
                            projection,
                            events,
                        )
                        .await
                    }
                    Err(_) => Err(ServiceError::InvalidBoundary),
                },
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        },
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
    resolved: ResolvedPromptInput,
    delivery: UserMessageDelivery,
    projection: &mut ProjectionState,
    events: &mpsc::Sender<TimestampedEvent>,
) -> Result<DriverCommandOutcome, ServiceError> {
    let ResolvedPromptInput {
        display_text,
        model_text: _,
        attachments,
        documents,
        project_files,
    } = resolved;
    let item_id = projection.next_user_item_id(run_id)?;
    let turn_id = projection.turn_id(run_id)?;
    projection.pending_user_items.push_back(PendingUserItem {
        id: item_id.clone(),
        delivery,
        turn_id: turn_id.clone(),
        documents: documents.clone(),
        project_files: project_files.clone(),
        branch_provenance: None,
    });
    projection
        .item_turns
        .insert(item_id.clone(), turn_id.clone());
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
                    text: bounded_text(&display_text, MAX_PROMPT_BYTES),
                    attachments,
                    documents,
                    project_files,
                    delivery: Some(delivery),
                    branch_provenance: None,
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

fn normalized_tool_name(name: &str) -> String {
    let normalized = name
        .chars()
        .take(128)
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | ':') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if normalized.trim_matches('_').is_empty() {
        "tool".into()
    } else {
        normalized
    }
}

fn safe_relative_path(value: &str) -> Option<String> {
    if value.is_empty() || value.contains('\0') {
        return None;
    }
    let mut components = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if components.is_empty() {
        Some(".".into())
    } else {
        Some(components.join("/"))
    }
}

fn safe_public_target(workspace: &Path, value: &str) -> Option<String> {
    if value.contains("://") {
        let url = url::Url::parse(value).ok()?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let host = url.host_str()?;
        let port = url
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        let path = if url.path().is_empty() {
            "/"
        } else {
            url.path()
        };
        return Some(bounded_text(
            &format!("{}://{host}{port}{path}", url.scheme()),
            1024,
        ));
    }
    let source = Path::new(value);
    if !source.is_absolute() {
        return safe_relative_path(value);
    }
    let workspace = workspace.canonicalize().ok()?;
    let candidate = source.canonicalize().ok()?;
    let relative = candidate.strip_prefix(workspace).ok()?;
    if relative.as_os_str().is_empty() {
        return Some(".".into());
    }
    safe_relative_path(&relative.to_string_lossy())
}

fn safe_workspace_path(workspace: &Path, value: &str) -> Option<String> {
    if value.contains("://") {
        return None;
    }
    safe_public_target(workspace, value)
}

fn safe_public_query(workspace: &Path, value: &str) -> Option<String> {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.contains("://") && url::Url::parse(&normalized).is_ok() {
        return safe_public_target(workspace, &normalized);
    }

    let lower = normalized.to_ascii_lowercase();
    let sensitive_assignments = [
        "api_key=",
        "api_key:",
        "api-key=",
        "api-key:",
        "apikey=",
        "apikey:",
        "access_token=",
        "access_token:",
        "access-token=",
        "access-token:",
        "auth_token=",
        "auth_token:",
        "authorization=",
        "authorization:",
        "bearer ",
        "basic ",
        "client_secret=",
        "client_secret:",
        "cookie=",
        "cookie:",
        "credential=",
        "credential:",
        "password=",
        "password:",
        "password ",
        "secret=",
        "secret:",
        "secret ",
        "session_token=",
        "session_token:",
        "token=",
        "token:",
    ];
    let known_token_prefixes = [
        "akia",
        "asia",
        "aiza",
        "dop_v1_",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "hf_",
        "npm_",
        "pypi-",
        "sk-",
        "sk_live_",
        "rk_live_",
        "xoxb-",
        "xoxa-",
        "xoxp-",
        "xoxr-",
        "xoxs-",
        "xoxapp-",
        "ya29.",
    ];
    let contains_known_token = normalized.split_ascii_whitespace().any(|word| {
        let word = word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '_' | '-' | '.')
        });
        let word = word.to_ascii_lowercase();
        word.len() >= 12
            && known_token_prefixes
                .iter()
                .any(|prefix| word.starts_with(prefix))
    });
    if sensitive_assignments
        .iter()
        .any(|needle| lower.contains(needle))
        || contains_known_token
    {
        return Some("[redacted query]".into());
    }

    Some(bounded_text(&normalized, 512))
}

fn search_target(query: Option<String>, path: Option<String>) -> Option<String> {
    match (query, path) {
        (Some(query), Some(path)) => Some(bounded_text(&format!("{query} in {path}"), 1024)),
        (Some(query), None) => Some(query),
        (None, Some(path)) => Some(path),
        (None, None) => None,
    }
}

fn safe_command_shape(
    name: &str,
    arguments: &serde_json::Value,
    workspace: &Path,
) -> (Option<String>, bool) {
    if !matches!(name, "bash" | "exec") {
        return (None, false);
    }
    // Reuse the native presentation normalizer only as an internal first pass.
    // None of its argument-derived strings cross the public boundary until the
    // strict allowlist below has reduced the command to a known shape.
    let normalized =
        crate::presentation::summarize_tool_with_workspace(name, arguments, Some(workspace))
            .shell_command
            .unwrap_or_default();
    if normalized.is_empty()
        || normalized.chars().any(|character| {
            matches!(
                character,
                '\n' | '\r' | ';' | '|' | '&' | '>' | '<' | '`' | '$' | '\'' | '"'
            )
        })
    {
        return (None, false);
    }
    let words = normalized.split_ascii_whitespace().collect::<Vec<_>>();
    let program = words
        .first()
        .and_then(|word| Path::new(word).file_name())
        .and_then(|word| word.to_str())
        .unwrap_or_default();
    let subcommand = words.get(1).copied().unwrap_or_default();
    let shape = match (program, subcommand, words.get(2).copied()) {
        ("cargo", command, _)
            if matches!(
                command,
                "test" | "check" | "clippy" | "fmt" | "build" | "doc"
            ) =>
        {
            Some(format!("cargo {command}"))
        }
        ("npm" | "pnpm" | "yarn" | "bun", "test" | "build" | "lint" | "check", _) => {
            Some(format!("{program} {subcommand}"))
        }
        ("npm" | "pnpm" | "yarn" | "bun", "run", Some(command))
            if matches!(command, "test" | "build" | "lint" | "check" | "typecheck") =>
        {
            Some(format!("{program} run {command}"))
        }
        ("pytest", _, _) => Some("pytest".into()),
        ("python" | "python3", "-m", Some("pytest" | "unittest")) => {
            Some(format!("{program} -m {}", words[2]))
        }
        ("go", "test" | "vet" | "build", _) => Some(format!("go {subcommand}")),
        ("rustc", _, _) => Some("rustc".into()),
        ("git", "status" | "diff" | "log" | "show", _) => Some(format!("git {subcommand}")),
        _ => None,
    };
    let verification = matches!(
        (program, subcommand, words.get(2).copied()),
        (
            "cargo",
            "test" | "check" | "clippy" | "fmt" | "build" | "doc",
            _
        ) | (
            "npm" | "pnpm" | "yarn" | "bun",
            "test" | "build" | "lint" | "check",
            _
        ) | (
            "npm" | "pnpm" | "yarn" | "bun",
            "run",
            Some("test" | "build" | "lint" | "check" | "typecheck")
        ) | ("pytest", _, _)
            | ("python" | "python3", "-m", Some("pytest" | "unittest"))
            | ("go", "test" | "vet" | "build", _)
            | ("rustc", _, _)
    );
    (shape.map(|shape| bounded_text(&shape, 1024)), verification)
}

fn semantic_tool_activity(
    name: &str,
    arguments: &serde_json::Value,
    workspace: &Path,
    started_at_ms: u64,
) -> ToolActivity {
    let path = arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_public_target(workspace, value));
    let resource_path = arguments
        .get("resource_path")
        .and_then(serde_json::Value::as_str)
        .and_then(safe_relative_path);
    let query = arguments
        .get("query")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_public_query(workspace, value));
    let url = arguments
        .get("url")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_public_target(workspace, value));
    let cwd = arguments
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| safe_workspace_path(workspace, value));
    let (command_preview, verification) = safe_command_shape(name, arguments, workspace);
    let remote_read = name == "read"
        && arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.starts_with("http://") || value.starts_with("https://"));
    let (kind, phase, title, target) = match name {
        "read" if remote_read => (
            ToolKind::Web,
            ActivityPhase::Investigated,
            path.as_ref()
                .map(|target| format!("Read {target}"))
                .unwrap_or_else(|| "Read remote resource".into()),
            path,
        ),
        "read" => (
            ToolKind::Read,
            ActivityPhase::Investigated,
            path.as_ref()
                .map(|target| format!("Read {target}"))
                .unwrap_or_else(|| "Read file".into()),
            path,
        ),
        "search" => {
            let target = search_target(query, path);
            (
                ToolKind::Search,
                ActivityPhase::Investigated,
                target
                    .as_ref()
                    .map(|target| format!("Search {target}"))
                    .unwrap_or_else(|| "Search workspace".into()),
                target,
            )
        }
        "edit" => (
            ToolKind::Edit,
            ActivityPhase::Changed,
            path.as_ref()
                .map(|target| format!("Update {target}"))
                .unwrap_or_else(|| "Update file".into()),
            path,
        ),
        "write" => (
            ToolKind::Write,
            ActivityPhase::Changed,
            path.as_ref()
                .map(|target| format!("Write {target}"))
                .unwrap_or_else(|| "Write file".into()),
            path,
        ),
        "bash" | "exec" => (
            ToolKind::Command,
            if verification {
                ActivityPhase::Verified
            } else {
                ActivityPhase::Other
            },
            command_preview
                .as_ref()
                .map(|command| format!("Run {command}"))
                .unwrap_or_else(|| "Run command".into()),
            None,
        ),
        "read_skill_resource" => (
            ToolKind::Skill,
            ActivityPhase::Investigated,
            resource_path
                .as_ref()
                .map(|target| format!("Read skill resource {target}"))
                .unwrap_or_else(|| "Read skill resource".into()),
            resource_path,
        ),
        "web_search" => {
            let target = url.or(query);
            (
                ToolKind::Web,
                ActivityPhase::Investigated,
                target
                    .as_ref()
                    .map(|target| format!("Search the web for {target}"))
                    .unwrap_or_else(|| "Search the web".into()),
                target,
            )
        }
        _ => (
            ToolKind::Other,
            ActivityPhase::Other,
            format!(
                "Run {}",
                normalized_tool_name(name).replace(['_', '-'], " ")
            ),
            None,
        ),
    };
    ToolActivity {
        raw_tool_name: normalized_tool_name(name),
        kind,
        phase,
        status: ToolActivityStatus::Running,
        title: bounded_text(&title, 512),
        summary: Some("Running".into()),
        target,
        cwd,
        command_preview,
        exit_code: None,
        signal: None,
        started_at_ms: started_at_ms.max(1),
        completed_at_ms: None,
        duration_ms: None,
        output_summary: None,
        output_handle: None,
        observed_output_bytes: 0,
        dropped_output_bytes: 0,
        changed_paths: Vec::new(),
        source_ids: Vec::new(),
        artifact_ids: Vec::new(),
    }
}

fn parse_process_metadata(text: &str) -> (Option<i32>, Option<i32>, Option<u64>) {
    let mut exit_code = None;
    let mut signal = None;
    let mut duration_ms = None;
    for token in text.lines().take(4).flat_map(str::split_ascii_whitespace) {
        if let Some(value) = token.strip_prefix("exit=") {
            if let Some(value) = value.strip_prefix("signal:") {
                signal = value.parse::<i32>().ok();
            } else if value != "unknown" {
                exit_code = value.parse::<i32>().ok();
            }
        } else if let Some(value) = token
            .strip_prefix("duration=")
            .and_then(|value| value.strip_suffix('s'))
        {
            duration_ms = value
                .parse::<f64>()
                .ok()
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(|seconds| (seconds * 1_000.0).round().min(u64::MAX as f64) as u64);
        }
    }
    (exit_code, signal, duration_ms)
}

fn complete_tool_activity(
    mut activity: ToolActivity,
    name: &str,
    result: &Result<ToolOutput, ToolError>,
    completed_at_ms: u64,
    progress: ProjectedToolProgress,
) -> (ToolActivity, ToolResultSummary) {
    let raw_result = match result {
        Ok(output) => output.text.as_str(),
        Err(error) => error.message.as_str(),
    };
    let (exit_code, signal, parsed_duration_ms) = if matches!(name, "bash" | "exec") {
        parse_process_metadata(raw_result)
    } else {
        (None, None, None)
    };
    let failed = crate::presentation::tool_result_is_failure(name, result);
    let status = if failed {
        ToolActivityStatus::Failed
    } else {
        ToolActivityStatus::Succeeded
    };
    let duration_ms = parsed_duration_ms
        .unwrap_or_else(|| completed_at_ms.saturating_sub(activity.started_at_ms));
    let output_summary = match status {
        ToolActivityStatus::Succeeded if activity.phase == ActivityPhase::Verified => {
            Some("Verification completed".into())
        }
        ToolActivityStatus::Succeeded if activity.kind == ToolKind::Read => {
            Some("Read completed".into())
        }
        ToolActivityStatus::Succeeded if activity.kind == ToolKind::Search => {
            Some("Search completed".into())
        }
        ToolActivityStatus::Succeeded if activity.kind == ToolKind::Edit => {
            Some("File updated".into())
        }
        ToolActivityStatus::Succeeded if activity.kind == ToolKind::Write => {
            Some("File written".into())
        }
        ToolActivityStatus::Succeeded if activity.kind == ToolKind::Web => {
            Some("Remote lookup completed".into())
        }
        ToolActivityStatus::Succeeded => Some("Tool completed".into()),
        ToolActivityStatus::Failed if activity.phase == ActivityPhase::Verified => {
            Some("Verification failed".into())
        }
        ToolActivityStatus::Failed if exit_code.is_some() => {
            Some(format!("Command exited {}", exit_code.unwrap_or_default()))
        }
        ToolActivityStatus::Failed if signal.is_some() => Some(format!(
            "Command stopped by signal {}",
            signal.unwrap_or_default()
        )),
        ToolActivityStatus::Failed => Some("Tool failed".into()),
        ToolActivityStatus::Running | ToolActivityStatus::Stopped => None,
    };
    let final_bytes = raw_result.len().min(u64::MAX as usize) as u64;
    activity.status = status;
    activity.summary = Some(match status {
        ToolActivityStatus::Succeeded => "Completed".into(),
        ToolActivityStatus::Failed => "Failed".into(),
        ToolActivityStatus::Stopped => "Stopped".into(),
        ToolActivityStatus::Running => "Running".into(),
    });
    activity.exit_code = exit_code;
    activity.signal = signal;
    activity.completed_at_ms = Some(completed_at_ms.max(activity.started_at_ms));
    activity.duration_ms = Some(duration_ms);
    activity.output_summary = output_summary.clone();
    activity.observed_output_bytes = progress.observed_output_bytes.max(final_bytes);
    activity.dropped_output_bytes = progress.dropped_output_bytes;
    let summary = activity
        .summary
        .clone()
        .unwrap_or_else(|| "Completed".into());
    let result = ToolResultSummary {
        tool_call_item_id: ItemId::new("placeholder").expect("static item ID is valid"),
        status,
        summary,
        output_summary,
        output_handle: activity.output_handle.clone(),
        exit_code,
        signal,
        completed_at_ms: activity.completed_at_ms.unwrap_or(completed_at_ms),
        duration_ms,
        observed_output_bytes: activity.observed_output_bytes,
        dropped_output_bytes: activity.dropped_output_bytes,
    };
    (activity, result)
}

fn test_framework_hint(activity: &ToolActivity) -> Option<TestFramework> {
    match activity.command_preview.as_deref() {
        Some("cargo test") => Some(TestFramework::CargoLibtest),
        Some("pytest") | Some("python -m pytest") | Some("python3 -m pytest") => {
            Some(TestFramework::Pytest)
        }
        Some("go test") => Some(TestFramework::GoTest),
        // Package runners may dispatch either Vitest or Jest, so their
        // deterministic reporter markers select the parser.
        _ => None,
    }
}

fn project_test_results(
    item_id: &ItemId,
    activity: &ToolActivity,
    output: &ToolOutput,
) -> Option<StructuredTestResults> {
    if activity.kind != ToolKind::Command || activity.phase != ActivityPhase::Verified {
        return None;
    }
    let status = match activity.status {
        ToolActivityStatus::Succeeded => TestCommandStatus::Succeeded,
        ToolActivityStatus::Failed => TestCommandStatus::Failed,
        ToolActivityStatus::Stopped => TestCommandStatus::Stopped,
        ToolActivityStatus::Running => return None,
    };
    let bytes = output.text.as_bytes();
    let retained_len = bytes.len().min(MAX_TEST_OUTPUT_BYTES);
    parse_test_output(TestOutputInput {
        origin_item_id: item_id.clone(),
        output: &bytes[..retained_len],
        input_truncated: bytes.len() > retained_len || activity.dropped_output_bytes > 0,
        command: TestCommandOutcome {
            status,
            exit_code: activity.exit_code,
            signal: activity.signal,
        },
        framework_hint: test_framework_hint(activity),
    })
    .ok()
}

fn stable_tool_item_id(tool_call_id: &str) -> Result<ItemId, ServiceError> {
    let hash = stable_hash(tool_call_id.as_bytes());
    ItemId::new(format!("item-tool-{}", &hash[..24])).map_err(|_| ServiceError::Internal)
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
                            turn_id: Some(turn_id.clone()),
                            provider_attempt: Some(projection.provider_attempt),
                            lifecycle: ItemLifecycle::Provisional,
                            durable_entry_id: None,
                            payload,
                        },
                    }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
                projection.item_turns.insert(item_id.clone(), turn_id);
                *slot = Some(item_id);
            }
        }
        AgentEvent::ProviderRetry { .. } | AgentEvent::CandidateRejected { .. } => {
            retract_attempt(run_id, projection, events).await?;
            projection.provider_attempt = projection.provider_attempt.saturating_add(1);
        }
        AgentEvent::ToolStarted { id, name, args } => {
            let item_id = stable_tool_item_id(&id.0)?;
            let turn_id = projection.turn_id(run_id)?;
            let started_at_ms = now_ms();
            projection.tool_items.insert(id.0.clone(), item_id.clone());
            let arguments =
                if ygg_serve_backend::validate_json("tool.arguments", &args, 256 * 1024).is_ok() {
                    args
                } else {
                    serde_json::Value::Null
                };
            let activity =
                semantic_tool_activity(&name, &arguments, &plan.config.workspace, started_at_ms);
            projection.tool_calls.insert(
                id.0.clone(),
                ProjectedToolCall {
                    name: name.clone(),
                    arguments,
                    activity: activity.clone(),
                    result: None,
                    turn_id: turn_id.clone(),
                },
            );
            projection
                .item_turns
                .insert(item_id.clone(), turn_id.clone());
            events
                .send(event(EventPayload::ItemStarted {
                    item: SessionItem {
                        id: item_id,
                        run_id: Some(run_id.clone()),
                        turn_id: Some(turn_id),
                        provider_attempt: Some(projection.provider_attempt),
                        lifecycle: ItemLifecycle::Provisional,
                        durable_entry_id: None,
                        payload: ItemPayload::ToolCall(activity),
                    },
                }))
                .await
                .map_err(|_| ServiceError::Unavailable)?;
        }
        AgentEvent::ToolProgress { id, progress } => {
            project_tool_progress(id, progress, run_id, projection, events).await?;
        }
        AgentEvent::ToolFinished { id, result } => {
            let tool_item_id = projection.tool_items.get(&id.0).cloned();
            let projected = projection.tool_calls.get(&id.0).cloned();
            if let (Some(item_id), Some(mut tool)) = (tool_item_id.clone(), projected) {
                let progress = projection.tool_progress.remove(&id.0).unwrap_or_default();
                let (activity, mut semantic_result) = complete_tool_activity(
                    tool.activity.clone(),
                    &tool.name,
                    &result,
                    now_ms(),
                    progress,
                );
                semantic_result.tool_call_item_id = item_id.clone();
                tool.activity = activity.clone();
                tool.result = Some(semantic_result);
                projection.tool_calls.insert(id.0.clone(), tool);
                if let Ok(output) = result.as_ref() {
                    if let Some(test_results) = project_test_results(&item_id, &activity, output) {
                        projection.test_results.push(test_results);
                    }
                }
                events
                    .send(event(EventPayload::ItemDelta {
                        item_id,
                        delta: ItemDelta::ToolActivity { activity },
                    }))
                    .await
                    .map_err(|_| ServiceError::Unavailable)?;
            }
            if let (Some(_), Some(tool), Some(tool_item_id), Ok(output)) = (
                plan.resources.as_ref(),
                projection.tool_calls.get(&id.0).cloned(),
                tool_item_id,
                result.as_ref(),
            ) {
                projection
                    .pending_tool_evidence
                    .push_back(CompletedToolEvidence {
                        tool_call_id: id.0,
                        tool_item_id,
                        turn_id: projection.turn_id(run_id)?,
                        tool,
                        output: output.clone(),
                    });
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

const STORED_EVIDENCE_VERSION: u16 = 2;
const STORED_RUN_RECORD_VERSION: u16 = 1;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRunItemAttribution {
    durable_entry_id: String,
    ordinal: u32,
    item_id: String,
    turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_delivery: Option<UserMessageDelivery>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    documents: Vec<DocumentReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    project_files: Vec<TrustedFileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_provenance: Option<ConversationBranchProvenance>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRunTool {
    tool_call_id: String,
    item_id: String,
    turn_id: String,
    activity: ToolActivity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<ToolResultSummary>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredRunRecord {
    version: u16,
    session_id: String,
    run_id: String,
    outcome_entry_id: String,
    started_at_ms: u64,
    completed_at_ms: u64,
    items: Vec<StoredRunItemAttribution>,
    tools: Vec<StoredRunTool>,
    review: CompletionReview,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredToolEvidence {
    version: u16,
    session_id: String,
    tool_call_id: String,
    call_entry_id: String,
    result_entry_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin_item_id: Option<String>,
    entries: Vec<StoredEvidenceEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredEvidenceEntry {
    Source {
        item_id: String,
        source_id: String,
        source_kind: SourceKind,
        title: String,
        handle: String,
        consulted_at_ms: u64,
    },
    FileChange {
        item_id: String,
        diff_handle: String,
        result_handle: String,
        display_path: String,
        additions: u32,
        deletions: u32,
    },
    Artifact {
        item_id: String,
        artifact_id: String,
        artifact_kind: ArtifactKind,
        name: String,
        media_type: String,
        handle: String,
        byte_len: u64,
        content_hash: String,
    },
}

struct EvidenceProjection {
    items: Vec<SessionItem>,
    sources: Vec<SourceRef>,
    artifacts: Vec<ArtifactRef>,
}

#[allow(clippy::too_many_arguments)]
fn project_tool_evidence(
    session: &Session,
    workspace: &Path,
    resources: &ygg_serve_backend::ResourceStore,
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    tool_call_id: &str,
    tool_item_id: &ItemId,
    tool: &ProjectedToolCall,
    output: &ToolOutput,
) -> Vec<EventPayload> {
    match project_tool_evidence_inner(
        session,
        workspace,
        resources,
        session_id,
        run_id,
        turn_id,
        tool_call_id,
        tool_item_id,
        tool,
        output,
    ) {
        Ok(events) => events,
        Err(_) => {
            let _ = resources.rollback_uncommitted_tool_resources(session_id, tool_call_id);
            Vec::new()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn project_tool_evidence_inner(
    session: &Session,
    workspace: &Path,
    resources: &ygg_serve_backend::ResourceStore,
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    tool_call_id: &str,
    tool_item_id: &ItemId,
    tool: &ProjectedToolCall,
    output: &ToolOutput,
) -> Result<Vec<EventPayload>, ServiceError> {
    let (call_entry_id, result_entry_id) =
        durable_tool_anchor(session, tool_call_id).ok_or(ServiceError::InvalidBoundary)?;
    let identity = stable_hash(
        format!(
            "{}\0{}\0{}\0{}",
            session_id.as_str(),
            call_entry_id.as_str(),
            result_entry_id.as_str(),
            tool_call_id
        )
        .as_bytes(),
    );
    let Some(short_identity) = identity.get(..24) else {
        return Err(ServiceError::Internal);
    };

    let entries = match tool.name.as_str() {
        "read" => {
            let Some(path) = tool
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
            else {
                return Ok(Vec::new());
            };
            let Some(snapshot) = snapshot_workspace_file(workspace, path) else {
                return Ok(Vec::new());
            };
            if trusted_output_hash(&output.text).as_deref()
                != Some(stable_hash(&snapshot.bytes).as_str())
            {
                return Ok(Vec::new());
            }
            let stored = resources
                .register(
                    session_id,
                    tool_call_id,
                    "source",
                    &snapshot.display_name,
                    snapshot.media_type,
                    snapshot.bytes,
                )
                .map_err(resource_store_service_error)?;
            vec![StoredEvidenceEntry::Source {
                item_id: format!("item-source-{short_identity}"),
                source_id: format!("source-{short_identity}"),
                source_kind: SourceKind::File,
                title: snapshot.display_path,
                handle: stored.handle,
                consulted_at_ms: now_ms(),
            }]
        }
        "read_skill_resource" => {
            let Some(title) = tool
                .arguments
                .get("resource_path")
                .and_then(serde_json::Value::as_str)
                .and_then(safe_relative_path)
            else {
                return Ok(Vec::new());
            };
            let stored = resources
                .register(
                    session_id,
                    tool_call_id,
                    "source",
                    &title,
                    "text/plain",
                    bytes::Bytes::copy_from_slice(output.text.as_bytes()),
                )
                .map_err(resource_store_service_error)?;
            vec![StoredEvidenceEntry::Source {
                item_id: format!("item-source-{short_identity}"),
                source_id: format!("source-{short_identity}"),
                source_kind: SourceKind::Resource,
                title: bounded_text(&title, 512),
                handle: stored.handle,
                consulted_at_ms: now_ms(),
            }]
        }
        "edit" | "write" => {
            let Some(path) = tool
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
            else {
                return Ok(Vec::new());
            };
            let Some(snapshot) = snapshot_workspace_file(workspace, path) else {
                return Ok(Vec::new());
            };
            let snapshot_hash = stable_hash(&snapshot.bytes);
            if trusted_output_hash(&output.text).as_deref() != Some(snapshot_hash.as_str()) {
                return Ok(Vec::new());
            }
            let write_created = tool.name == "write" && output_reports_created(&output.text);
            if tool.name == "write" && output.text.contains("\n(no change)") {
                return Ok(Vec::new());
            }
            let diff = if write_created {
                let Some(content) = tool
                    .arguments
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                else {
                    return Ok(Vec::new());
                };
                creation_diff(&snapshot.display_path, content)
            } else {
                let Some(detail) = output.text.splitn(3, '\n').nth(2) else {
                    return Ok(Vec::new());
                };
                if !detail.starts_with("--- ") {
                    return Ok(Vec::new());
                }
                detail.to_owned()
            };
            if diff.is_empty() || diff.len() > MAX_OPAQUE_RESOURCE_BYTES {
                return Ok(Vec::new());
            }
            let diff_name = format!("{}.diff", snapshot.display_name);
            let stored_diff = resources
                .register(
                    session_id,
                    tool_call_id,
                    "diff",
                    &diff_name,
                    "text/plain",
                    bytes::Bytes::from(diff.clone()),
                )
                .map_err(resource_store_service_error)?;
            let stored_result = resources
                .register(
                    session_id,
                    tool_call_id,
                    "result",
                    &snapshot.display_name,
                    snapshot.media_type,
                    snapshot.bytes,
                )
                .map_err(resource_store_service_error)?;
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
                diff_line_counts(&diff)
            };
            let mut entries = vec![StoredEvidenceEntry::FileChange {
                item_id: format!("item-file-change-{short_identity}"),
                diff_handle: stored_diff.handle,
                result_handle: stored_result.handle.clone(),
                display_path: snapshot.display_path,
                additions,
                deletions,
            }];
            if write_created && is_deliverable_artifact(snapshot.artifact_kind) {
                entries.push(StoredEvidenceEntry::Artifact {
                    item_id: format!("item-artifact-{short_identity}"),
                    artifact_id: format!("artifact-{short_identity}"),
                    artifact_kind: snapshot.artifact_kind,
                    name: snapshot.display_name,
                    media_type: snapshot.media_type.to_owned(),
                    handle: stored_result.handle,
                    byte_len: stored_result.byte_len,
                    content_hash: stored_result.sha256,
                });
            }
            entries
        }
        _ => return Ok(Vec::new()),
    };

    let record = StoredToolEvidence {
        version: STORED_EVIDENCE_VERSION,
        session_id: session_id.as_str().to_owned(),
        tool_call_id: tool_call_id.to_owned(),
        call_entry_id: call_entry_id.as_str().to_owned(),
        result_entry_id: result_entry_id.as_str().to_owned(),
        run_id: Some(run_id.as_str().to_owned()),
        turn_id: Some(turn_id.as_str().to_owned()),
        origin_item_id: Some(tool_item_id.as_str().to_owned()),
        entries,
    };
    let record_bytes = serde_json::to_vec(&record).map_err(|_| ServiceError::Internal)?;
    resources
        .persist_record(session_id, &result_entry_id, tool_call_id, &record_bytes)
        .map_err(resource_store_service_error)?;
    let projection = project_stored_evidence(
        resources,
        session_id,
        &record,
        Some(run_id.clone()),
        Some(turn_id.clone()),
        Some(tool_item_id.clone()),
    )?;
    let mut events = Vec::new();
    for source in projection.sources {
        events.push(EventPayload::SourceUpserted { source });
    }
    for artifact in projection.artifacts {
        events.push(EventPayload::ArtifactUpserted { artifact });
    }
    for item in projection.items {
        events.push(EventPayload::ItemCommitted { item });
    }
    Ok(events)
}

fn durable_tool_anchor(
    session: &Session,
    tool_call_id: &str,
) -> Option<(DurableEntryId, DurableEntryId)> {
    let mut cursor = session.head_ref();
    let mut result_entry_id = None;
    while let Some(entry_id) = cursor {
        let entry = session.entry(entry_id)?;
        match &entry.value {
            EntryValue::Message(Message::User(message))
                if result_entry_id.is_none()
                    && message.content.iter().any(|part| {
                        matches!(
                            part,
                            UserPart::ToolResult(result)
                                if result.tool_call_id.0 == tool_call_id && !result.is_error
                        )
                    }) =>
            {
                result_entry_id = DurableEntryId::new(entry.id.0.clone()).ok();
            }
            EntryValue::Message(Message::Assistant(message))
                if result_entry_id.is_some()
                    && message.content.iter().any(|part| {
                        matches!(
                            part,
                            AssistantPart::ToolCall(call) if call.id.0 == tool_call_id
                        )
                    }) =>
            {
                return Some((
                    DurableEntryId::new(entry.id.0.clone()).ok()?,
                    result_entry_id?,
                ));
            }
            _ => {}
        }
        cursor = entry.parent.as_ref();
    }
    None
}

fn trusted_output_hash(text: &str) -> Option<String> {
    text.split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("hash="))
        .filter(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .map(str::to_owned)
}

fn output_reports_created(text: &str) -> bool {
    text.lines()
        .nth(1)
        .is_some_and(|line| line.contains("  created hash="))
}

fn creation_diff(path: &str, content: &str) -> String {
    let total = content.lines().count();
    let mut diff = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{total} @@\n");
    for line in content.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}

fn diff_line_counts(diff: &str) -> (u32, u32) {
    let additions = diff
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count()
        .min(u32::MAX as usize) as u32;
    let deletions = diff
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .count()
        .min(u32::MAX as usize) as u32;
    (additions, deletions)
}

fn is_deliverable_artifact(kind: ArtifactKind) -> bool {
    matches!(
        kind,
        ArtifactKind::Site
            | ArtifactKind::Document
            | ArtifactKind::Spreadsheet
            | ArtifactKind::Presentation
    )
}

fn project_stored_evidence(
    resources: &ygg_serve_backend::ResourceStore,
    session_id: &SessionId,
    record: &StoredToolEvidence,
    run_id: Option<RunId>,
    turn_id: Option<TurnId>,
    origin_item_id: Option<ItemId>,
) -> Result<EvidenceProjection, ServiceError> {
    if !matches!(record.version, 1 | STORED_EVIDENCE_VERSION)
        || record.session_id != session_id.as_str()
        || record.entries.is_empty()
    {
        return Err(ServiceError::InvalidSeed);
    }
    let durable_entry_id = DurableEntryId::new(record.result_entry_id.clone())
        .map_err(|_| ServiceError::InvalidSeed)?;
    let run_id = record
        .run_id
        .clone()
        .and_then(|value| RunId::new(value).ok())
        .or(run_id);
    let turn_id = record
        .turn_id
        .clone()
        .and_then(|value| TurnId::new(value).ok())
        .or(turn_id);
    let origin_item_id = record
        .origin_item_id
        .clone()
        .and_then(|value| ItemId::new(value).ok())
        .or(origin_item_id);
    let mut projection = EvidenceProjection {
        items: Vec::new(),
        sources: Vec::new(),
        artifacts: Vec::new(),
    };
    for entry in &record.entries {
        let (item_id, payload) = match entry {
            StoredEvidenceEntry::Source {
                item_id,
                source_id,
                source_kind,
                title,
                handle,
                consulted_at_ms,
            } => {
                let source = SourceRef {
                    id: SourceId::new(source_id.clone()).map_err(|_| ServiceError::InvalidSeed)?,
                    kind: *source_kind,
                    title: bounded_text(title, 512),
                    handle: handle.clone(),
                    origin_item_id: origin_item_id.clone(),
                    consulted_at_ms: *consulted_at_ms,
                    cited: false,
                    available: resources.content(session_id, handle).is_ok(),
                };
                projection.sources.push(source.clone());
                (item_id, ItemPayload::Source(source))
            }
            StoredEvidenceEntry::FileChange {
                item_id,
                diff_handle,
                result_handle,
                display_path,
                additions,
                deletions,
            } => (
                item_id,
                ItemPayload::FileChange(FileChange {
                    handle: diff_handle.clone(),
                    result_handle: Some(result_handle.clone()),
                    display_path: bounded_text(display_path, 1024),
                    origin_item_id: origin_item_id.clone(),
                    additions: *additions,
                    deletions: *deletions,
                }),
            ),
            StoredEvidenceEntry::Artifact {
                item_id,
                artifact_id,
                artifact_kind,
                name,
                media_type,
                handle,
                byte_len,
                content_hash,
            } => {
                let artifact = ArtifactRef {
                    id: ArtifactId::new(artifact_id.clone())
                        .map_err(|_| ServiceError::InvalidSeed)?,
                    kind: *artifact_kind,
                    name: bounded_text(name, 512),
                    media_type: media_type.clone(),
                    handle: handle.clone(),
                    byte_len: *byte_len,
                    content_hash: Some(content_hash.clone()),
                    origin_item_id: origin_item_id.clone(),
                    available: resources.content(session_id, handle).is_ok(),
                };
                projection.artifacts.push(artifact.clone());
                (item_id, ItemPayload::Artifact(artifact))
            }
        };
        projection.items.push(SessionItem {
            id: ItemId::new(item_id.clone()).map_err(|_| ServiceError::InvalidSeed)?,
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            provider_attempt: None,
            lifecycle: ItemLifecycle::Committed,
            durable_entry_id: Some(durable_entry_id.clone()),
            payload,
        });
    }
    Ok(projection)
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
        media_type: workspace_media_type(&extension, &bytes),
        artifact_kind: artifact_kind_for_extension(&extension),
        bytes: bytes::Bytes::from(bytes),
    })
}

fn workspace_media_type(extension: &str, bytes: &[u8]) -> &'static str {
    if std::str::from_utf8(bytes).is_err() {
        return match extension {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
    }
    match extension {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "jsx" | "mjs" | "cjs" => "text/javascript",
        "json" => "application/json",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "svg" => "image/svg+xml",
        _ => "text/plain",
    }
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

fn default_completion_review(
    outcome: ygg_serve_backend::RunOutcome,
    duration_ms: u64,
    message: Option<&str>,
) -> CompletionReview {
    let summary = message.map_or_else(
        || match outcome {
            ygg_serve_backend::RunOutcome::Completed => "Run completed.".into(),
            ygg_serve_backend::RunOutcome::Stopped => "Run stopped.".into(),
            ygg_serve_backend::RunOutcome::Failed => "Run failed.".into(),
        },
        |message| bounded_text(message, 2 * 1024),
    );
    CompletionReview {
        summary,
        duration_ms,
        action_count: 0,
        phases: Vec::new(),
        changed_file_item_ids: Vec::new(),
        verification_action_item_ids: Vec::new(),
        failed_action_item_ids: Vec::new(),
        warning_action_item_ids: Vec::new(),
        source_ids: Vec::new(),
        output_ids: Vec::new(),
        test_results: Vec::new(),
        evidence_coverage: EvidenceCoverage::None,
        open_questions: Vec::new(),
    }
}

fn build_completion_review(
    terminal: &TerminalProjection,
    started_at_ms: u64,
    completed_at_ms: u64,
    projection: &ProjectionState,
    changed_file_item_ids: BTreeSet<ItemId>,
    source_ids: BTreeSet<SourceId>,
    output_ids: BTreeSet<ArtifactId>,
) -> CompletionReview {
    let mut phases = BTreeMap::<ActivityPhase, ActivityPhaseSummary>::new();
    let mut verification_action_item_ids = Vec::new();
    let mut failed_action_item_ids = Vec::new();
    let mut warning_action_item_ids = Vec::new();
    let mut activities = projection
        .tool_items
        .iter()
        .filter_map(|(call_id, item_id)| {
            projection
                .tool_calls
                .get(call_id)
                .map(|tool| (item_id.clone(), tool.activity.clone()))
        })
        .collect::<Vec<_>>();
    activities.sort_by(|left, right| {
        left.1
            .started_at_ms
            .cmp(&right.1.started_at_ms)
            .then_with(|| left.0.as_str().cmp(right.0.as_str()))
    });
    for (item_id, activity) in &activities {
        let phase = phases
            .entry(activity.phase)
            .or_insert(ActivityPhaseSummary {
                phase: activity.phase,
                action_count: 0,
                succeeded_count: 0,
                failed_count: 0,
                stopped_count: 0,
            });
        phase.action_count = phase.action_count.saturating_add(1);
        match activity.status {
            ToolActivityStatus::Succeeded => {
                phase.succeeded_count = phase.succeeded_count.saturating_add(1)
            }
            ToolActivityStatus::Failed => {
                phase.failed_count = phase.failed_count.saturating_add(1);
                failed_action_item_ids.push(item_id.clone());
                if terminal.outcome == ygg_serve_backend::RunOutcome::Completed {
                    warning_action_item_ids.push(item_id.clone());
                }
            }
            ToolActivityStatus::Stopped | ToolActivityStatus::Running => {
                phase.stopped_count = phase.stopped_count.saturating_add(1)
            }
        }
        if activity.phase == ActivityPhase::Verified {
            verification_action_item_ids.push(item_id.clone());
        }
    }
    let phase_summaries = phases.into_values().collect::<Vec<_>>();
    let changed_file_item_ids = changed_file_item_ids.into_iter().collect::<Vec<_>>();
    let source_ids = source_ids.into_iter().collect::<Vec<_>>();
    let output_ids = output_ids.into_iter().collect::<Vec<_>>();
    let action_count = activities.len().min(u32::MAX as usize) as u32;
    let has_unbounded_mutator = activities
        .iter()
        .any(|(_, activity)| matches!(activity.kind, ToolKind::Command | ToolKind::Other));
    let linked_evidence =
        !changed_file_item_ids.is_empty() || !source_ids.is_empty() || !output_ids.is_empty();
    let evidence_coverage = if has_unbounded_mutator {
        EvidenceCoverage::Partial
    } else if !linked_evidence {
        EvidenceCoverage::None
    } else if activities.iter().all(|(_, activity)| match activity.kind {
        ToolKind::Read | ToolKind::Web | ToolKind::Skill => !activity.source_ids.is_empty(),
        ToolKind::Edit | ToolKind::Write => {
            activity.status != ToolActivityStatus::Succeeded || !activity.changed_paths.is_empty()
        }
        ToolKind::Search => false,
        ToolKind::Command | ToolKind::Other => false,
    }) {
        EvidenceCoverage::Complete
    } else {
        EvidenceCoverage::Partial
    };
    let summary = format!(
        "{} {} action{}, {} changed file{}, {} verification{}, {} failure{}, {} warning{}, and {} output{}.",
        match terminal.outcome {
            ygg_serve_backend::RunOutcome::Completed => "Completed",
            ygg_serve_backend::RunOutcome::Stopped => "Stopped after",
            ygg_serve_backend::RunOutcome::Failed => "Failed after",
        },
        action_count,
        if action_count == 1 { "" } else { "s" },
        changed_file_item_ids.len(),
        if changed_file_item_ids.len() == 1 { "" } else { "s" },
        verification_action_item_ids.len(),
        if verification_action_item_ids.len() == 1 {
            ""
        } else {
            "s"
        },
        failed_action_item_ids.len(),
        if failed_action_item_ids.len() == 1 { "" } else { "s" },
        warning_action_item_ids.len(),
        if warning_action_item_ids.len() == 1 {
            ""
        } else {
            "s"
        },
        output_ids.len(),
        if output_ids.len() == 1 { "" } else { "s" },
    );
    CompletionReview {
        summary: bounded_text(&summary, 2 * 1024),
        duration_ms: completed_at_ms.saturating_sub(started_at_ms),
        action_count,
        phases: phase_summaries,
        changed_file_item_ids,
        verification_action_item_ids,
        failed_action_item_ids,
        warning_action_item_ids,
        source_ids,
        output_ids,
        test_results: projection.test_results.clone(),
        evidence_coverage,
        // The adapter cannot infer unresolved questions from prose safely.
        open_questions: Vec::new(),
    }
}

fn persist_run_projection(
    resources: &ygg_serve_backend::ResourceStore,
    session_id: &SessionId,
    run_id: &RunId,
    started_at_ms: u64,
    completed_at_ms: u64,
    projection: &ProjectionState,
    committed: &[SessionItem],
    review: &CompletionReview,
) -> Result<(), ServiceError> {
    let outcome_entry_id = committed
        .iter()
        .find_map(|item| {
            matches!(&item.payload, ItemPayload::RunOutcome { .. })
                .then(|| item.durable_entry_id.clone())
                .flatten()
        })
        .ok_or(ServiceError::Internal)?;
    let fallback_turn = projection.turn_id(run_id)?;
    let mut ordinals = HashMap::<String, u32>::new();
    let mut items = Vec::with_capacity(committed.len());
    for item in committed {
        let Some(durable_entry_id) = item.durable_entry_id.as_ref() else {
            continue;
        };
        let ordinal = ordinals
            .entry(durable_entry_id.as_str().to_owned())
            .or_default();
        items.push(StoredRunItemAttribution {
            durable_entry_id: durable_entry_id.as_str().to_owned(),
            ordinal: *ordinal,
            item_id: item.id.as_str().to_owned(),
            turn_id: item
                .turn_id
                .as_ref()
                .unwrap_or(&fallback_turn)
                .as_str()
                .to_owned(),
            user_delivery: match &item.payload {
                ItemPayload::UserMessage { delivery, .. } => *delivery,
                _ => None,
            },
            documents: match &item.payload {
                ItemPayload::UserMessage { documents, .. } => documents.clone(),
                _ => Vec::new(),
            },
            project_files: match &item.payload {
                ItemPayload::UserMessage { project_files, .. } => project_files.clone(),
                _ => Vec::new(),
            },
            branch_provenance: match &item.payload {
                ItemPayload::UserMessage {
                    branch_provenance, ..
                } => branch_provenance.clone(),
                _ => None,
            },
        });
        *ordinal = ordinal.saturating_add(1);
    }
    let mut tools = projection
        .tool_items
        .iter()
        .filter_map(|(tool_call_id, item_id)| {
            let tool = projection.tool_calls.get(tool_call_id)?;
            Some(StoredRunTool {
                tool_call_id: tool_call_id.clone(),
                item_id: item_id.as_str().to_owned(),
                turn_id: tool.turn_id.as_str().to_owned(),
                activity: tool.activity.clone(),
                result: tool.result.clone(),
            })
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| {
        left.activity
            .started_at_ms
            .cmp(&right.activity.started_at_ms)
            .then_with(|| left.item_id.cmp(&right.item_id))
    });
    let record = StoredRunRecord {
        version: STORED_RUN_RECORD_VERSION,
        session_id: session_id.as_str().to_owned(),
        run_id: run_id.as_str().to_owned(),
        outcome_entry_id: outcome_entry_id.as_str().to_owned(),
        started_at_ms,
        completed_at_ms,
        items,
        tools,
        review: review.clone(),
    };
    let bytes = serde_json::to_vec(&record).map_err(|_| ServiceError::Internal)?;
    resources
        .persist_run_record(session_id, &outcome_entry_id, &bytes)
        .map_err(resource_store_service_error)
}

fn load_stored_run_record(
    resources: &ygg_serve_backend::ResourceStore,
    session_id: &SessionId,
    outcome_entry_id: &DurableEntryId,
) -> Option<StoredRunRecord> {
    let bytes = resources.run_record(session_id, outcome_entry_id).ok()?;
    let record = serde_json::from_slice::<StoredRunRecord>(&bytes).ok()?;
    if record.version != STORED_RUN_RECORD_VERSION
        || record.session_id != session_id.as_str()
        || record.outcome_entry_id != outcome_entry_id.as_str()
        || record.started_at_ms == 0
        || record.completed_at_ms < record.started_at_ms
        || RunId::new(record.run_id.clone()).is_err()
        || record.review.validate().is_err()
    {
        return None;
    }
    for item in &record.items {
        if DurableEntryId::new(item.durable_entry_id.clone()).is_err()
            || ItemId::new(item.item_id.clone()).is_err()
            || TurnId::new(item.turn_id.clone()).is_err()
            || (ItemPayload::UserMessage {
                text: String::new(),
                attachments: Vec::new(),
                documents: item.documents.clone(),
                project_files: item.project_files.clone(),
                delivery: item.user_delivery,
                branch_provenance: item.branch_provenance.clone(),
            })
            .validate()
            .is_err()
        {
            return None;
        }
    }
    for tool in &record.tools {
        if tool.tool_call_id.len() > 512
            || ItemId::new(tool.item_id.clone()).is_err()
            || TurnId::new(tool.turn_id.clone()).is_err()
            || tool.activity.validate().is_err()
            || tool
                .result
                .as_ref()
                .is_some_and(|result| result.validate().is_err())
        {
            return None;
        }
    }
    Some(record)
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
            let entry = projection.tool_progress.entry(id.0.clone()).or_default();
            entry.observed_output_bytes = entry
                .observed_output_bytes
                .saturating_add(bytes.len() as u64);
            publish_tool_progress(&id.0, projection, events).await?;
        }
        ToolProgress::Status(_) => {}
        ToolProgress::Dropped { bytes, .. } => {
            let entry = projection.tool_progress.entry(id.0.clone()).or_default();
            entry.dropped_output_bytes = entry.dropped_output_bytes.saturating_add(bytes);
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
            let action = projection
                .tool_calls
                .get(&id.0)
                .map(|tool| format!("Approve {}?", tool.activity.title))
                .unwrap_or_else(|| "Approve this tool action?".into());
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
                    // The extension-owned prompt is private tool progress. Do
                    // not forward it verbatim across the public boundary.
                    prompt: "A tool needs additional input to continue.".into(),
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
    let Some(tool) = projection.tool_calls.get(tool_call_id) else {
        return Ok(());
    };
    let progress = projection
        .tool_progress
        .get(tool_call_id)
        .cloned()
        .unwrap_or_default();
    let mut activity = tool.activity.clone();
    activity.observed_output_bytes = progress.observed_output_bytes;
    activity.dropped_output_bytes = progress.dropped_output_bytes;
    events
        .send(event(EventPayload::ItemDelta {
            item_id,
            delta: ItemDelta::ToolActivity { activity },
        }))
        .await
        .map_err(|_| ServiceError::Unavailable)
}

fn project_new_entries(
    session: &Session,
    workspace: &Path,
    projection: &mut ProjectionState,
    run_id: Option<&RunId>,
    completion_review: Option<&CompletionReview>,
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
        let (
            preferred,
            user_delivery,
            preferred_reasoning,
            resolved_documents,
            resolved_project_files,
            branch_provenance,
        ) = match &entry.value {
            EntryValue::Message(Message::User(message))
                if message
                    .content
                    .iter()
                    .any(|part| matches!(part, UserPart::Text(_) | UserPart::Media(_))) =>
            {
                match projection.pending_user_items.pop_front() {
                    Some(pending) => {
                        projection
                            .item_turns
                            .insert(pending.id.clone(), pending.turn_id);
                        (
                            Some(pending.id),
                            Some(pending.delivery),
                            None,
                            pending.documents,
                            pending.project_files,
                            pending.branch_provenance,
                        )
                    }
                    None => (None, None, None, Vec::new(), Vec::new(), None),
                }
            }
            EntryValue::Message(Message::Assistant(_)) => (
                projection
                    .completed_assistant_items
                    .pop_front()
                    .flatten()
                    .map(|(item_id, turn_id)| {
                        projection.item_turns.insert(item_id.clone(), turn_id);
                        item_id
                    }),
                None,
                projection
                    .completed_reasoning_items
                    .pop_front()
                    .flatten()
                    .map(|(item_id, turn_id)| {
                        projection.item_turns.insert(item_id.clone(), turn_id);
                        item_id
                    }),
                Vec::new(),
                Vec::new(),
                None,
            ),
            _ => (None, None, None, Vec::new(), Vec::new(), None),
        };
        let mut projected = project_entry(
            entry,
            workspace,
            run_id.cloned(),
            preferred,
            user_delivery,
            preferred_reasoning,
            &mut projection.tool_items,
            &mut projection.tool_calls,
            completion_review,
            attachments,
        )?;
        if let Some(user_item) = projected
            .iter_mut()
            .find(|item| matches!(item.payload, ItemPayload::UserMessage { .. }))
        {
            if let ItemPayload::UserMessage {
                documents,
                project_files,
                branch_provenance: projected_provenance,
                ..
            } = &mut user_item.payload
            {
                *documents = resolved_documents;
                *project_files = resolved_project_files;
                *projected_provenance = branch_provenance;
            }
        }
        for item in &mut projected {
            let turn_id =
                projection
                    .item_turns
                    .get(&item.id)
                    .cloned()
                    .or_else(|| match &item.payload {
                        ItemPayload::ToolCall(_) => {
                            projection.tool_items.iter().find_map(|(call_id, item_id)| {
                                if item_id == &item.id {
                                    projection
                                        .tool_calls
                                        .get(call_id)
                                        .map(|tool| tool.turn_id.clone())
                                } else {
                                    None
                                }
                            })
                        }
                        ItemPayload::ToolResult(result) => projection
                            .item_turns
                            .get(&result.tool_call_item_id)
                            .cloned(),
                        _ => run_id.and_then(|run_id| projection.turn_id(run_id).ok()),
                    });
            item.turn_id = turn_id;
        }
        items.extend(projected);
    }
    projection.known_entries = entries.len();
    Ok(items)
}

fn branch_graph(session: &Session) -> Result<SessionBranchGraph, ServiceError> {
    let all_entries = session.entries();
    let head = session.head();
    let mut selected_indices = (all_entries
        .len()
        .saturating_sub(MAX_PROJECTED_BRANCH_ENTRIES)
        ..all_entries.len())
        .collect::<Vec<_>>();
    if let Some(head) = head.as_ref() {
        let head_index = all_entries
            .iter()
            .position(|entry| &entry.id == head)
            .ok_or(ServiceError::InvalidSeed)?;
        if !selected_indices.contains(&head_index) {
            if selected_indices.len() == MAX_PROJECTED_BRANCH_ENTRIES {
                selected_indices.remove(0);
            }
            selected_indices.push(head_index);
            selected_indices.sort_unstable();
        }
    }
    Ok(SessionBranchGraph {
        head: head
            .map(|head| DurableEntryId::new(head.0))
            .transpose()
            .map_err(|_| ServiceError::InvalidSeed)?,
        entries: selected_indices
            .iter()
            .map(|index| project_branch_entry(&all_entries[*index]))
            .collect::<Result<_, _>>()?,
        truncated: selected_indices.len() < all_entries.len(),
    })
}

fn branch_delta_events(
    session: &Session,
    start: usize,
) -> Result<Vec<TimestampedEvent>, ServiceError> {
    let entries = session.entries();
    if start > entries.len() {
        return Err(ServiceError::InvalidSeed);
    }
    let mut events = entries[start..]
        .chunks(MAX_BRANCH_DELTA_ENTRIES)
        .map(|chunk| {
            Ok(event(EventPayload::SessionBranchEntriesAppended {
                entries: chunk
                    .iter()
                    .map(project_branch_entry)
                    .collect::<Result<_, _>>()?,
            }))
        })
        .collect::<Result<Vec<_>, ServiceError>>()?;
    let durable_entry_id = session
        .head()
        .map(|head| DurableEntryId::new(head.0))
        .transpose()
        .map_err(|_| ServiceError::Internal)?;
    events.push(event(EventPayload::SessionDurableHeadChanged {
        durable_entry_id,
    }));
    Ok(events)
}

fn project_branch_entry(entry: &Entry) -> Result<SessionBranchEntry, ServiceError> {
    let kind = match &entry.value {
        EntryValue::Message(Message::User(_)) => SessionBranchEntryKind::UserMessage,
        EntryValue::Message(Message::Assistant(_)) => SessionBranchEntryKind::AssistantMessage,
        EntryValue::Compaction { .. } => SessionBranchEntryKind::Compaction,
        _ => SessionBranchEntryKind::Internal,
    };
    Ok(SessionBranchEntry {
        entry_id: DurableEntryId::new(entry.id.0.clone()).map_err(|_| ServiceError::InvalidSeed)?,
        parent_entry_id: entry
            .parent
            .as_ref()
            .map(|parent| DurableEntryId::new(parent.0.clone()))
            .transpose()
            .map_err(|_| ServiceError::InvalidSeed)?,
        checkoutable: kind != SessionBranchEntryKind::Internal,
        kind,
        label: branch_entry_label(entry),
    })
}

fn branch_entry_label(entry: &Entry) -> String {
    let candidate = match &entry.value {
        EntryValue::Message(Message::User(message)) => entry
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.display_text.as_deref())
            .or_else(|| {
                message.content.iter().find_map(|part| match part {
                    UserPart::Text(text) => Some(text.as_str()),
                    UserPart::Media(_) | UserPart::ToolResult(_) => None,
                })
            })
            .unwrap_or("User input"),
        EntryValue::Message(Message::Assistant(message)) => message
            .content
            .iter()
            .find_map(|part| match part {
                AssistantPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or("Assistant response"),
        EntryValue::Config { .. } => "Internal session state",
        EntryValue::Compaction { .. } => "Compaction",
        EntryValue::ResponsesTurn { .. }
        | EntryValue::ResponsesCompaction { .. }
        | EntryValue::PromptTemplateSelected { .. }
        | EntryValue::SkillActivated { .. }
        | EntryValue::SkillResourceRead { .. }
        | EntryValue::SkillDeactivated { .. } => "Internal session state",
    };
    let first_line = candidate.lines().find(|line| !line.trim().is_empty());
    bounded_text(first_line.unwrap_or("Session entry"), 256)
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
    workspace: &Path,
    run_id: Option<RunId>,
    preferred: Option<ItemId>,
    user_delivery: Option<UserMessageDelivery>,
    preferred_reasoning: Option<ItemId>,
    tool_items: &mut HashMap<String, ItemId>,
    tool_calls: &mut HashMap<String, ProjectedToolCall>,
    completion_review: Option<&CompletionReview>,
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
                        let tool_call_item_id = tool_items
                            .get(&result.tool_call_id.0)
                            .cloned()
                            .unwrap_or(stable_tool_item_id(&result.tool_call_id.0)?);
                        let durable_result = if result.is_error {
                            Err(ToolError::new(content.clone()))
                        } else {
                            Ok(ToolOutput::new(content.clone()))
                        };
                        let semantic_result = if let Some(mut tool) =
                            tool_calls.get(&result.tool_call_id.0).cloned()
                        {
                            if let Some(summary) = tool.result.clone() {
                                summary
                            } else {
                                let (activity, mut summary) = complete_tool_activity(
                                    tool.activity,
                                    &tool.name,
                                    &durable_result,
                                    1,
                                    ProjectedToolProgress::default(),
                                );
                                summary.tool_call_item_id = tool_call_item_id.clone();
                                tool.activity = activity;
                                tool.result = Some(summary.clone());
                                tool_calls.insert(result.tool_call_id.0.clone(), tool);
                                summary
                            }
                        } else {
                            let fallback_turn = TurnId::new("turn-history")
                                .map_err(|_| ServiceError::InvalidSeed)?;
                            let mut fallback = ProjectedToolCall {
                                name: "tool".into(),
                                arguments: serde_json::Value::Null,
                                activity: semantic_tool_activity(
                                    "tool",
                                    &serde_json::Value::Null,
                                    workspace,
                                    1,
                                ),
                                result: None,
                                turn_id: fallback_turn,
                            };
                            let (activity, mut summary) = complete_tool_activity(
                                fallback.activity,
                                &fallback.name,
                                &durable_result,
                                1,
                                ProjectedToolProgress::default(),
                            );
                            summary.tool_call_item_id = tool_call_item_id.clone();
                            fallback.activity = activity;
                            fallback.result = Some(summary.clone());
                            tool_calls.insert(result.tool_call_id.0.clone(), fallback);
                            summary
                        };
                        items.push(committed_item(
                            item_id_for_entry(entry, items.len())?,
                            run_id.clone(),
                            durable_id.clone(),
                            ItemPayload::ToolResult(ToolResultSummary {
                                tool_call_item_id,
                                ..semantic_result
                            }),
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
                            documents: Vec::new(),
                            project_files: Vec::new(),
                            delivery: user_delivery,
                            branch_provenance: None,
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
                let arguments =
                    if ygg_serve_backend::validate_json("tool.arguments", &arguments, 256 * 1024)
                        .is_ok()
                    {
                        arguments
                    } else {
                        serde_json::Value::Null
                    };
                let item_id = tool_items
                    .get(&call.id.0)
                    .cloned()
                    .unwrap_or(stable_tool_item_id(&call.id.0)?);
                tool_items.insert(call.id.0.clone(), item_id.clone());
                let projected =
                    tool_calls
                        .entry(call.id.0.clone())
                        .or_insert_with(|| ProjectedToolCall {
                            name: call.name.clone(),
                            arguments: arguments.clone(),
                            activity: semantic_tool_activity(&call.name, &arguments, workspace, 1),
                            result: None,
                            turn_id: TurnId::new("turn-history")
                                .expect("static historical turn ID is valid"),
                        });
                items.push(committed_item(
                    item_id,
                    run_id.clone(),
                    durable_id.clone(),
                    ItemPayload::ToolCall(projected.activity.clone()),
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
                    ItemPayload::RunOutcome {
                        outcome,
                        review: completion_review.cloned().unwrap_or_else(|| {
                            default_completion_review(outcome, 0, message.as_deref())
                        }),
                        message,
                    },
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

fn rehydrate_stored_evidence(
    resources: &ygg_serve_backend::ResourceStore,
    session: &Session,
    session_id: &SessionId,
    result_entry: &Entry,
    active_entry_ids: &std::collections::BTreeSet<&str>,
    tool_items: &HashMap<String, ItemId>,
) -> Option<EvidenceProjection> {
    let durable_result_id = DurableEntryId::new(result_entry.id.0.clone()).ok()?;
    let bytes = resources.record(session_id, &durable_result_id).ok()?;
    let record = serde_json::from_slice::<StoredToolEvidence>(&bytes).ok()?;
    if !matches!(record.version, 1 | STORED_EVIDENCE_VERSION)
        || record.session_id != session_id.as_str()
        || record.result_entry_id != result_entry.id.0
        || !active_entry_ids.contains(record.call_entry_id.as_str())
        || !result_entry_has_successful_tool_result(result_entry, &record.tool_call_id)
    {
        return None;
    }
    let call_entry = session.entry(&EntryId(record.call_entry_id.clone()))?;
    if !entry_has_tool_call(call_entry, &record.tool_call_id) {
        return None;
    }
    project_stored_evidence(
        resources,
        session_id,
        &record,
        None,
        None,
        tool_items.get(&record.tool_call_id).cloned(),
    )
    .ok()
}

fn result_entry_has_successful_tool_result(entry: &Entry, tool_call_id: &str) -> bool {
    matches!(
        &entry.value,
        EntryValue::Message(Message::User(message))
            if message.content.iter().any(|part| {
                matches!(
                    part,
                    UserPart::ToolResult(result)
                        if result.tool_call_id.0 == tool_call_id && !result.is_error
                )
            })
    )
}

fn entry_has_tool_call(entry: &Entry, tool_call_id: &str) -> bool {
    matches!(
        &entry.value,
        EntryValue::Message(Message::Assistant(message))
            if message.content.iter().any(|part| {
                matches!(
                    part,
                    AssistantPart::ToolCall(call) if call.id.0 == tool_call_id
                )
            })
    )
}

struct SessionSeedOptions<'a> {
    workspace: &'a Path,
    project_id: Option<ProjectId>,
    model: ModelSelection,
    authority: AuthorityProfile,
    generation: u64,
    meta: Option<SessionMeta>,
    attachment_store: Option<&'a AttachmentStore>,
    resource_store: Option<&'a ygg_serve_backend::ResourceStore>,
}

fn seed_from_session(
    session: &Session,
    session_id: SessionId,
    options: SessionSeedOptions<'_>,
) -> Result<SessionSeed, ServiceError> {
    let SessionSeedOptions {
        workspace,
        project_id,
        model,
        authority,
        generation,
        meta,
        attachment_store,
        resource_store,
    } = options;
    let mut chain = Vec::new();
    let mut cursor = session.head_ref();
    while let Some(id) = cursor {
        let entry = session.entry(id).ok_or(ServiceError::InvalidSeed)?;
        chain.push(entry);
        cursor = entry.parent.as_ref();
    }
    chain.reverse();
    let active_entry_ids = chain
        .iter()
        .map(|entry| entry.id.0.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let mut items = Vec::new();
    let mut sources = Vec::new();
    let mut artifacts = Vec::new();
    let mut tool_items = HashMap::new();
    let mut tool_calls = HashMap::new();
    let mut attributions = HashMap::<String, Vec<StoredRunItemAttribution>>::new();
    let mut run_ids_by_entry = HashMap::<String, RunId>::new();
    let mut reviews_by_outcome = HashMap::<String, CompletionReview>::new();
    if let Some(resources) = resource_store {
        for entry in &chain {
            if !entry
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.run_outcome.is_some())
            {
                continue;
            }
            let Ok(outcome_entry_id) = DurableEntryId::new(entry.id.0.clone()) else {
                continue;
            };
            let Some(record) = load_stored_run_record(resources, &session_id, &outcome_entry_id)
            else {
                continue;
            };
            let Ok(run_id) = RunId::new(record.run_id.clone()) else {
                continue;
            };
            reviews_by_outcome.insert(entry.id.0.clone(), record.review.clone());
            for item in record.items {
                if !active_entry_ids.contains(item.durable_entry_id.as_str()) {
                    continue;
                }
                run_ids_by_entry.insert(item.durable_entry_id.clone(), run_id.clone());
                attributions
                    .entry(item.durable_entry_id.clone())
                    .or_default()
                    .push(item);
            }
            for tool in record.tools {
                let Ok(item_id) = ItemId::new(tool.item_id.clone()) else {
                    continue;
                };
                let Ok(turn_id) = TurnId::new(tool.turn_id.clone()) else {
                    continue;
                };
                tool_items.insert(tool.tool_call_id.clone(), item_id);
                tool_calls.insert(
                    tool.tool_call_id,
                    ProjectedToolCall {
                        name: tool.activity.raw_tool_name.clone(),
                        arguments: serde_json::Value::Null,
                        activity: tool.activity,
                        result: tool.result,
                        turn_id,
                    },
                );
            }
        }
    }
    for entries in attributions.values_mut() {
        entries.sort_by_key(|item| item.ordinal);
    }
    let mut pending_attachments = VecDeque::new();
    for entry in chain {
        let attachments = attachment_refs_for_entry(
            entry,
            attachment_store,
            &session_id,
            &mut pending_attachments,
        )?;
        let run_id = run_ids_by_entry.get(&entry.id.0).cloned();
        let review = reviews_by_outcome.get(&entry.id.0);
        let mut projected = project_entry(
            entry,
            workspace,
            run_id.clone(),
            None,
            None,
            None,
            &mut tool_items,
            &mut tool_calls,
            review,
            attachments,
        )?;
        if let Some(stored) = attributions.get(&entry.id.0) {
            for (item, attribution) in projected.iter_mut().zip(stored) {
                item.id = ItemId::new(attribution.item_id.clone())
                    .map_err(|_| ServiceError::InvalidSeed)?;
                item.turn_id = Some(
                    TurnId::new(attribution.turn_id.clone())
                        .map_err(|_| ServiceError::InvalidSeed)?,
                );
                item.run_id = run_id.clone();
                if let ItemPayload::UserMessage {
                    delivery,
                    documents,
                    project_files,
                    branch_provenance,
                    ..
                } = &mut item.payload
                {
                    *delivery = attribution.user_delivery;
                    *documents = attribution.documents.clone();
                    *project_files = attribution.project_files.clone();
                    *branch_provenance = attribution.branch_provenance.clone();
                }
            }
        }
        items.extend(projected);
        if let Some(projection) = resource_store.and_then(|store| {
            rehydrate_stored_evidence(
                store,
                session,
                &session_id,
                entry,
                &active_entry_ids,
                &tool_items,
            )
        }) {
            items.extend(projection.items);
            sources.extend(projection.sources);
            artifacts.extend(projection.artifacts);
        }
    }
    // A legacy session may predate semantic run sidecars. Its result entry is
    // encountered after the corresponding call entry, so apply the safe
    // terminal fallback back onto the already-projected call. New sessions
    // take the same path with the exact persisted activity.
    let projected_tools = tool_items
        .iter()
        .filter_map(|(tool_call_id, item_id)| {
            tool_calls
                .get(tool_call_id)
                .map(|tool| (item_id.clone(), tool.activity.clone()))
        })
        .collect::<HashMap<_, _>>();
    for item in &mut items {
        if let ItemPayload::ToolCall(activity) = &mut item.payload {
            if let Some(projected) = projected_tools.get(&item.id) {
                *activity = projected.clone();
            }
        }
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
    let (lifecycle, retention, forked_from) = meta
        .as_ref()
        .map(|meta| session_catalog_metadata(meta, &session_id))
        .transpose()?
        .unwrap_or((SessionCatalogState::Active, None, None));
    let summary = SessionSummary {
        id: session_id.clone(),
        project_id,
        title,
        tags: meta.map(|meta| meta.tags).unwrap_or_default(),
        created_at_ms: modified_at_ms,
        modified_at_ms,
        pinned,
        archived,
        lifecycle,
        retention,
        forked_from,
        provisional: false,
        live_state: SessionLiveState::Idle,
        attention: AttentionState::None,
        pull_request: None,
        owner: ActorOwnerState::Hosted,
        model: model.clone(),
    };
    let branches = branch_graph(session)?;
    let snapshot = SessionSnapshot {
        session_id,
        actor_generation: generation,
        cursor: SessionCursor::zero(generation),
        durable_head: branches.head.clone(),
        branches,
        live_state: SessionLiveState::Idle,
        active_run_id: None,
        model,
        authority,
        context: ContextUsage::default(),
        items,
        pending_requests: Vec::new(),
        sources,
        artifacts,
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
            lifecycle: SessionCatalogState::Active,
            retention: None,
            forked_from: None,
            provisional: true,
            live_state: SessionLiveState::Idle,
            attention: AttentionState::None,
            pull_request: None,
            owner: ActorOwnerState::Hosted,
            model: model.clone(),
        },
        snapshot: SessionSnapshot {
            session_id,
            actor_generation: generation,
            cursor: SessionCursor::zero(generation),
            durable_head: None,
            branches: SessionBranchGraph::default(),
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

fn graphical_input_pricing(pricing: Option<&ygg_ai::Pricing>) -> Option<ModelInputPricing> {
    pricing.map(|pricing| ModelInputPricing {
        base_microdollars_per_million_tokens: pricing.input.0,
        tiers: pricing
            .tiers
            .iter()
            .filter_map(|tier| {
                tier.input.map(|rate| ModelInputPricingTier {
                    min_input_tokens: tier.min_input_tokens,
                    microdollars_per_million_tokens: rate.0,
                })
            })
            .take(MAX_MODEL_INPUT_PRICING_TIERS)
            .collect(),
    })
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
                input_pricing: graphical_input_pricing(model.spec.pricing.as_ref()),
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
    let (lifecycle, retention, forked_from) = session_catalog_metadata(meta, &id)?;
    Ok(SessionSummary {
        id,
        project_id,
        title: bounded_text(&meta.title, 512),
        tags: meta.tags.iter().map(|tag| bounded_text(tag, 64)).collect(),
        created_at_ms: modified_at_ms,
        modified_at_ms,
        pinned: meta.pinned,
        archived: meta.archived,
        lifecycle,
        retention,
        forked_from,
        provisional: false,
        live_state: SessionLiveState::Idle,
        attention: AttentionState::None,
        pull_request: None,
        owner: ActorOwnerState::Inactive,
        model,
    })
}

fn session_catalog_metadata(
    meta: &SessionMeta,
    session_id: &SessionId,
) -> Result<
    (
        SessionCatalogState,
        Option<SessionRetention>,
        Option<ConversationBranchProvenance>,
    ),
    ServiceError,
> {
    let (lifecycle, retention) = match (meta.trashed_at_ms, meta.purge_after_ms) {
        (Some(trashed_at_ms), Some(purge_after_ms)) => (
            SessionCatalogState::Trash,
            Some(SessionRetention {
                trashed_at_ms,
                purge_after_ms,
                permanent_delete_requires_confirmation: true,
            }),
        ),
        (None, None) if meta.archived => (SessionCatalogState::Archived, None),
        (None, None) => (SessionCatalogState::Active, None),
        _ => return Err(ServiceError::InvalidSeed),
    };
    let forked_from = match (
        meta.forked_from_session_id.as_deref(),
        meta.forked_from_entry_id.as_deref(),
    ) {
        (None, None) => None,
        (Some(source_session_id), Some(source_entry_id)) => Some(ConversationBranchProvenance {
            operation: ConversationBranchOperation::ForkSession,
            source_session_id: SessionId::new(source_session_id.to_owned())
                .map_err(|_| ServiceError::InvalidSeed)?,
            source_entry_id: DurableEntryId::new(source_entry_id.to_owned())
                .map_err(|_| ServiceError::InvalidSeed)?,
            originating_user_entry_id: None,
            model_override: None,
            external_effects_preserved: true,
            warning: EXTERNAL_EFFECTS_WARNING.to_owned(),
        }),
        _ => return Err(ServiceError::InvalidSeed),
    };
    let _ = session_id;
    Ok((lifecycle, retention, forked_from))
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
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use ygg_agent::EntryMetadata;
    use ygg_ai::{AssistantMessage, Protocol, UserMessage};
    use ygg_serve_backend::{
        AckDisposition, ActorConfig, ActorError, CatalogCursor, CommandId, DeviceId, HostBootstrap,
        SessionActorCore, SessionCommandEnvelope, SessionSupervisor, SupervisorConfig,
        SupervisorError, PROTOCOL_VERSION,
    };

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

    fn project_test_config(directory: &Path, trusted: bool) -> Config {
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = serve_test_config(&workspace);
        config.session_dir = directory.join("sessions");
        config.workspace_trusted = trusted;
        config
    }

    #[tokio::test]
    async fn host_backfills_durable_provider_usage_once_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = project_test_config(directory.path(), true);
        config.workspace = config.workspace.canonicalize().unwrap();
        config.invocation_cwd = config.workspace.clone();
        let sessions = SessionStore::new(&config.session_dir, &config.workspace);
        std::fs::create_dir_all(sessions.dir()).unwrap();
        let mut session = Session::create(sessions.dir().join("usage-backfill.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("measure usage".into())],
            })))
            .unwrap();
        let assistant = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("done".into())],
                model: ModelId("gpt-4o-mini".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        session
            .record_assistant_usage(
                assistant,
                ygg_ai::EndpointId("openai".into()),
                ModelId("gpt-4o-mini".into()),
                ygg_ai::Usage {
                    input_tokens: 80,
                    cache_read_tokens: 10,
                    cache_write_tokens: 5,
                    cache_write_1h_tokens: 0,
                    output_tokens: 20,
                    reasoning_tokens: 4,
                    total_tokens: 115,
                },
                None,
            )
            .unwrap();
        drop(session);

        let host = YggHost::new(config.clone()).unwrap();
        let lifetime = host.usage_lifetime().await.unwrap();
        assert_eq!(lifetime.prompt_tokens, 80);
        assert_eq!(lifetime.completion_tokens, 20);
        assert_eq!(lifetime.cache_read_tokens, 10);
        assert_eq!(lifetime.cache_write_tokens, 5);
        assert_eq!(lifetime.cache_write_1h_tokens, 0);
        assert_eq!(lifetime.reasoning_tokens, 4);
        assert_eq!(lifetime.total_tokens, 115);
        assert_eq!(lifetime.request_count, 1);
        assert_eq!(
            host.usage_stats(UsagePeriod::Daily)
                .await
                .unwrap()
                .request_count,
            1
        );
        drop(host);

        let reopened = YggHost::new(config).unwrap();
        assert_eq!(reopened.usage_lifetime().await.unwrap().request_count, 1);
        assert_eq!(reopened.usage_lifetime().await.unwrap().total_tokens, 115);
    }

    #[tokio::test]
    async fn real_project_trust_is_required_and_session_binding_survives_restart() {
        let fixture = tempfile::tempdir().unwrap();
        let config = project_test_config(fixture.path(), false);
        let host = YggHost::new(config.clone()).unwrap();
        let launch_project = host.launch_project_id.clone();
        let projects = host.list_projects().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, launch_project);
        assert!(!projects[0].trusted);
        assert!(projects[0].available);
        assert!(projects[0].is_default);

        let request = CreateSessionRequest {
            project_id: Some(launch_project.clone()),
            provisional: true,
            authority: AuthorityProfile::FullAccess,
            model: None,
        };
        assert!(matches!(
            host.create_session(request.clone()).await,
            Err(ServiceError::Unauthorized)
        ));
        let trusted = host.set_project_trust(&launch_project, true).await.unwrap();
        assert!(trusted.trusted);
        let driver = host.create_session(request.clone()).await.unwrap();
        let session_id = driver.seed().summary.id;
        assert_eq!(
            driver.seed().summary.project_id,
            Some(launch_project.clone())
        );
        assert_eq!(
            host.projects
                .lock()
                .unwrap()
                .project_for_session(session_id.as_str())
                .unwrap()
                .as_str(),
            launch_project.as_str()
        );
        drop(driver);
        drop(host);

        let reopened = YggHost::new(config).unwrap();
        assert!(
            reopened
                .list_projects()
                .await
                .unwrap()
                .into_iter()
                .find(|project| project.id == launch_project)
                .unwrap()
                .trusted,
            "a durable explicit trust grant must not depend on the next CLI flag"
        );
        assert_eq!(
            reopened
                .projects
                .lock()
                .unwrap()
                .project_for_session(session_id.as_str())
                .unwrap()
                .as_str(),
            launch_project.as_str()
        );
        reopened
            .set_project_trust(&launch_project, false)
            .await
            .unwrap();
        assert!(matches!(
            reopened.create_session(request).await,
            Err(ServiceError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn imported_project_lifecycle_never_exposes_or_bypasses_its_root_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let config = project_test_config(fixture.path(), true);
        let imported_root = fixture.path().join("private-imported-root");
        std::fs::create_dir(&imported_root).unwrap();
        let first_host = YggHost::new(config.clone()).unwrap();
        assert_eq!(
            first_host
                .import_project("browser-authored-candidate", Some("Rejected"))
                .await
                .unwrap_err(),
            ServiceError::Unavailable,
            "the browser cannot mint or submit filesystem authority"
        );
        drop(first_host);

        let mut imported_launch = config;
        imported_launch.workspace = imported_root.clone();
        imported_launch.invocation_cwd = imported_root.clone();
        imported_launch.workspace_trusted = false;
        let host = YggHost::new(imported_launch).unwrap();
        let imported = host
            .list_projects()
            .await
            .unwrap()
            .into_iter()
            .find(|project| project.name == "private-imported-root")
            .unwrap();
        host.set_default_project(&imported.id).await.unwrap();
        assert_eq!(
            host.project_context(None).unwrap().config.workspace,
            fixture.path().join("workspace").canonicalize().unwrap(),
            "a cold launch must skip an untrusted default when another trusted project exists"
        );
        assert!(!imported.trusted);
        assert!(imported.available);
        assert!(!imported.archived);
        let public_json = serde_json::to_string(&imported).unwrap();
        assert!(!public_json.contains(imported_root.to_str().unwrap()));
        assert!(matches!(
            host.create_session(CreateSessionRequest {
                project_id: Some(imported.id.clone()),
                provisional: true,
                authority: AuthorityProfile::FullAccess,
                model: None,
            })
            .await,
            Err(ServiceError::Unauthorized)
        ));

        host.set_project_trust(&imported.id, true).await.unwrap();
        let context = host.project_context(Some(&imported.id)).unwrap();
        assert_eq!(
            context.config.workspace,
            imported_root.canonicalize().unwrap()
        );
        assert_eq!(context.config.invocation_cwd, context.config.workspace);
        assert!(context.config.workspace_trusted);

        let renamed = host.rename_project(&imported.id, "Renamed").await.unwrap();
        assert_eq!(renamed.name, "Renamed");
        assert!(
            host.set_default_project(&imported.id)
                .await
                .unwrap()
                .is_default
        );
        let archived = host.archive_project(&imported.id).await.unwrap();
        assert!(archived.archived);
        assert!(!archived.trusted);
        assert!(!archived.is_default);
        assert!(matches!(
            host.project_context(Some(&imported.id)),
            Err(ServiceError::InvalidBoundary)
        ));
    }

    fn worker_checkout_fixture(
        directory: &Path,
        session_name: &str,
    ) -> (YggHost, SessionId, DurableEntryId, DurableEntryId, PathBuf) {
        let mut config = serve_test_config(directory);
        let workspace = directory.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        config.workspace = workspace.clone();
        config.invocation_cwd = workspace;
        config.model = Some(ModelId("gpt-4o-mini".into()));
        config.model_explicit = true;
        let host = YggHost::new(config).unwrap();
        let session_id = SessionId::new(session_name).unwrap();
        let context = host.project_context(Some(&host.launch_project_id)).unwrap();
        std::fs::create_dir_all(context.sessions.dir()).unwrap();
        let path = context.sessions.dir().join(format!("{session_name}.jsonl"));
        host.projects
            .lock()
            .unwrap()
            .bind_session(
                session_name,
                &registry_project_id(&host.launch_project_id).unwrap(),
            )
            .unwrap();
        let mut session = Session::create(&path).unwrap();
        let root = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("root prompt".into())],
            })))
            .unwrap();
        let old_head = session
            .append(EntryValue::Config {
                model: Some("gpt-4o-mini".into()),
                reasoning: Some("off".into()),
                reasoning_mode: Some("standard".into()),
            })
            .unwrap();
        session.checkout(root).unwrap();
        let target = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("alternate prompt".into())],
            })))
            .unwrap();
        session.checkout(old_head.clone()).unwrap();
        (
            host,
            session_id,
            DurableEntryId::new(old_head.0).unwrap(),
            DurableEntryId::new(target.0).unwrap(),
            path,
        )
    }

    fn checkout_envelope(
        host: &YggHost,
        session_id: &SessionId,
        generation: u64,
        command_id: &str,
        target: DurableEntryId,
    ) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            host.descriptor.id.clone(),
            DeviceId::new("device-worker-test").unwrap(),
            session_id.clone(),
            CommandId::new(command_id).unwrap(),
            1,
            Some(generation),
            SessionCommand::Checkout { entry_id: target },
        )
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

    fn session_with_successful_tool_result(
        path: &Path,
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
        output: &str,
    ) -> Session {
        let mut session = Session::create(path).unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ygg_ai::ToolCall {
                    id: ToolCallId(call_id.to_owned()),
                    name: name.to_owned(),
                    arguments_json: serde_json::to_string(&arguments).unwrap(),
                })],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ygg_ai::ToolResult {
                    tool_call_id: ToolCallId(call_id.to_owned()),
                    content: vec![ToolResultPart::Text(output.to_owned())],
                    is_error: false,
                })],
            })))
            .unwrap();
        session
    }

    fn projected_tool(
        workspace: &Path,
        name: &str,
        arguments: serde_json::Value,
    ) -> ProjectedToolCall {
        ProjectedToolCall {
            name: name.into(),
            activity: semantic_tool_activity(name, &arguments, workspace, 1),
            arguments,
            result: None,
            turn_id: TurnId::new("turn-test").unwrap(),
        }
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
        let content = b"pub fn ygg() {}\n";
        std::fs::write(workspace.path().join("src/lib.rs"), content).unwrap();
        let registry = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let session_id = SessionId::new("session-evidence").unwrap();
        let run_id = RunId::new("run-evidence").unwrap();
        let turn_id = TurnId::new("turn-evidence").unwrap();
        let tool_item_id = ItemId::new("item-tool-read").unwrap();
        let tool = projected_tool(
            workspace.path(),
            "read",
            serde_json::json!({"path": "src/lib.rs"}),
        );
        let output = format!(
            "src/lib.rs:1-1/1 hash={}\n1: pub fn ygg() {{}}\ntruncated=false",
            stable_hash(content)
        );
        let session = session_with_successful_tool_result(
            &workspace.path().join("read-session.jsonl"),
            "call-read",
            "read",
            tool.arguments.clone(),
            &output,
        );

        let events = project_tool_evidence(
            &session,
            workspace.path(),
            &registry,
            &session_id,
            &run_id,
            &turn_id,
            "call-read",
            &tool_item_id,
            &tool,
            &ToolOutput::new(output),
        );
        assert_eq!(events.len(), 2);
        let EventPayload::SourceUpserted { source } = &events[0] else {
            panic!("first event was not source evidence");
        };
        assert_eq!(source.title, "src/lib.rs");
        assert_eq!(source.kind, SourceKind::File);
        assert_eq!(source.origin_item_id.as_ref(), Some(&tool_item_id));
        assert_eq!(
            registry.content(&session_id, &source.handle).unwrap().bytes,
            bytes::Bytes::from_static(b"pub fn ygg() {}\n")
        );
        assert!(!source.handle.contains("src"));
    }

    #[test]
    fn durable_evidence_rehydrates_only_on_the_active_branch() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("notes.txt"), b"durable evidence\n").unwrap();
        let session_path = workspace.path().join("branch-evidence.jsonl");
        let session_id = SessionId::new("branch-evidence").unwrap();
        let tool = projected_tool(
            workspace.path(),
            "read",
            serde_json::json!({"path": "notes.txt"}),
        );
        let output = format!(
            "notes.txt:1-1/1 hash={}\n1: durable evidence\ntruncated=false",
            stable_hash(b"durable evidence\n")
        );
        let mut session = session_with_successful_tool_result(
            &session_path,
            "call-branch-read",
            "read",
            tool.arguments.clone(),
            &output,
        );
        let call_entry = session.entries()[0].id.clone();
        let result_entry = session.entries()[1].id.clone();
        let store = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let events = project_tool_evidence(
            &session,
            workspace.path(),
            &store,
            &session_id,
            &RunId::new("run-branch-evidence").unwrap(),
            &TurnId::new("turn-branch-evidence").unwrap(),
            "call-branch-read",
            &ItemId::new("item-call-branch-read").unwrap(),
            &tool,
            &ToolOutput::new(output),
        );
        assert_eq!(events.len(), 2);
        drop(store);

        let store = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let seed_for = |session: &Session| {
            seed_from_session(
                session,
                session_id.clone(),
                SessionSeedOptions {
                    workspace: workspace.path(),
                    project_id: None,
                    model: ModelSelection {
                        provider: "test".into(),
                        model: "test-model".into(),
                        reasoning: "off".into(),
                    },
                    authority: AuthorityProfile::FullAccess,
                    generation: 1,
                    meta: None,
                    attachment_store: None,
                    resource_store: Some(&store),
                },
            )
            .unwrap()
        };
        assert_eq!(seed_for(&session).snapshot.sources.len(), 1);

        session.checkout(call_entry).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("alternate branch".into())],
            })))
            .unwrap();
        assert!(seed_for(&session).snapshot.sources.is_empty());

        session.checkout(result_entry).unwrap();
        let restored = seed_for(&session);
        assert_eq!(restored.snapshot.sources.len(), 1);
        assert_eq!(
            restored.snapshot.sources[0]
                .origin_item_id
                .as_ref()
                .map(ItemId::as_str),
            Some("item-call-branch-read")
        );
        assert!(restored
            .snapshot
            .items
            .iter()
            .any(|item| matches!(item.payload, ItemPayload::Source(_))));
    }

    #[test]
    fn resource_projection_rejects_outside_workspace_and_snapshots_successful_edits() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let registry = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let session_id = SessionId::new("session-evidence").unwrap();
        let run_id = RunId::new("run-evidence").unwrap();
        let turn_id = TurnId::new("turn-evidence").unwrap();
        let tool_item_id = ItemId::new("item-tool").unwrap();
        let outside_tool = projected_tool(
            workspace.path(),
            "read",
            serde_json::json!({"path": outside.path()}),
        );
        let outside_session = session_with_successful_tool_result(
            &workspace.path().join("outside-session.jsonl"),
            "call-outside",
            "read",
            outside_tool.arguments.clone(),
            "secret",
        );
        assert!(project_tool_evidence(
            &outside_session,
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
        let edit = projected_tool(
            workspace.path(),
            "edit",
            serde_json::json!({
                "path": "notes.md",
                "old": "before\n",
                "new": "after\nsecond\n"
            }),
        );
        let output = format!(
            "ok modified=1\nnotes.md  +2 -1 hash={}\n--- a/notes.md\n+++ b/notes.md\n@@ -1,1 +1,2 @@\n-before\n+after\n+second\n",
            stable_hash(b"after\nsecond\n")
        );
        let edit_session = session_with_successful_tool_result(
            &workspace.path().join("edit-session.jsonl"),
            "call-edit",
            "edit",
            edit.arguments.clone(),
            &output,
        );
        let events = project_tool_evidence(
            &edit_session,
            workspace.path(),
            &registry,
            &session_id,
            &run_id,
            &turn_id,
            "call-edit",
            &tool_item_id,
            &edit,
            &ToolOutput::new(output),
        );
        assert_eq!(events.len(), 1);
        let EventPayload::ItemCommitted { item } = &events[0] else {
            panic!("first edit event was not a file change");
        };
        let ItemPayload::FileChange(change) = &item.payload else {
            panic!("edit item was not a file change");
        };
        assert_eq!(change.display_path, "notes.md");
        assert_eq!(change.origin_item_id.as_ref(), Some(&tool_item_id));
        assert_eq!((change.additions, change.deletions), (2, 1));
        assert!(change.result_handle.is_some());
        assert_eq!(
            registry.content(&session_id, &change.handle).unwrap().bytes,
            bytes::Bytes::from_static(
                b"--- a/notes.md\n+++ b/notes.md\n@@ -1,1 +1,2 @@\n-before\n+after\n+second\n"
            )
        );
        assert_eq!(
            registry
                .content(&session_id, change.result_handle.as_deref().unwrap())
                .unwrap()
                .bytes,
            bytes::Bytes::from_static(b"after\nsecond\n")
        );
    }

    #[test]
    fn evidence_projection_rolls_back_when_the_second_resource_cannot_stage() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("empty.txt"), b"").unwrap();
        let store = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let session_id = SessionId::new("session-partial-evidence").unwrap();
        let tool = projected_tool(
            workspace.path(),
            "write",
            serde_json::json!({"path": "empty.txt", "content": ""}),
        );
        let output = format!(
            "ok\nempty.txt  created hash={}\n--- /dev/null\n+++ b/empty.txt\n@@ -0,0 +1,0 @@\n",
            stable_hash(b"")
        );
        let session = session_with_successful_tool_result(
            &workspace.path().join("partial-evidence.jsonl"),
            "call-partial-write",
            "write",
            tool.arguments.clone(),
            &output,
        );

        assert!(project_tool_evidence(
            &session,
            workspace.path(),
            &store,
            &session_id,
            &RunId::new("run-partial-evidence").unwrap(),
            &TurnId::new("turn-partial-evidence").unwrap(),
            "call-partial-write",
            &ItemId::new("item-partial-evidence").unwrap(),
            &tool,
            &ToolOutput::new(output),
        )
        .is_empty());

        let replacement = store
            .register(
                &session_id,
                "call-partial-write",
                "diff",
                "replacement.diff",
                "text/plain",
                bytes::Bytes::from_static(b"rollback freed this binding"),
            )
            .unwrap();
        assert_eq!(
            store
                .content(&session_id, &replacement.handle)
                .unwrap()
                .bytes,
            bytes::Bytes::from_static(b"rollback freed this binding")
        );
    }

    #[test]
    fn only_created_deliverables_are_promoted_to_artifacts() {
        let workspace = tempfile::tempdir().unwrap();
        let store = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let session_id = SessionId::new("session-artifact-semantics").unwrap();
        let run_id = RunId::new("run-artifact-semantics").unwrap();
        let turn_id = TurnId::new("turn-artifact-semantics").unwrap();
        let tool_item_id = ItemId::new("item-tool-write").unwrap();

        std::fs::write(workspace.path().join("report.md"), b"# Report\n").unwrap();
        let created = projected_tool(
            workspace.path(),
            "write",
            serde_json::json!({"path": "report.md", "content": "# Report\n"}),
        );
        let created_output = format!(
            "ok\nreport.md  created hash={}\n--- /dev/null\n+++ b/report.md\n@@ -0,0 +1,1 @@\n+# Report\n",
            stable_hash(b"# Report\n")
        );
        let mut created_session = session_with_successful_tool_result(
            &workspace.path().join("created-artifact.jsonl"),
            "call-write-created",
            "write",
            created.arguments.clone(),
            &created_output,
        );
        let created_events = project_tool_evidence(
            &created_session,
            workspace.path(),
            &store,
            &session_id,
            &run_id,
            &turn_id,
            "call-write-created",
            &tool_item_id,
            &created,
            &ToolOutput::new(created_output),
        );
        assert!(created_events
            .iter()
            .any(|event| matches!(event, EventPayload::ArtifactUpserted { artifact } if artifact.kind == ArtifactKind::Document)));
        assert!(created_events.iter().any(|event| {
            matches!(
                event,
                EventPayload::ArtifactUpserted { artifact }
                    if artifact.origin_item_id.as_ref() == Some(&tool_item_id)
            )
        }));

        std::fs::write(workspace.path().join("report.md"), b"# Revised\n").unwrap();
        let replaced = projected_tool(
            workspace.path(),
            "write",
            serde_json::json!({"path": "report.md", "content": "# Revised\n"}),
        );
        let replaced_output = format!(
            "ok\nreport.md  replaced hash={}\n--- a/report.md\n+++ b/report.md\n@@ -1,1 +1,1 @@\n-# Report\n+# Revised\n",
            stable_hash(b"# Revised\n")
        );
        created_session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ygg_ai::ToolCall {
                    id: ToolCallId("call-write-replaced".into()),
                    name: "write".into(),
                    arguments_json: serde_json::to_string(&replaced.arguments).unwrap(),
                })],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        created_session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ygg_ai::ToolResult {
                    tool_call_id: ToolCallId("call-write-replaced".into()),
                    content: vec![ToolResultPart::Text(replaced_output.clone())],
                    is_error: false,
                })],
            })))
            .unwrap();
        let replaced_events = project_tool_evidence(
            &created_session,
            workspace.path(),
            &store,
            &session_id,
            &run_id,
            &turn_id,
            "call-write-replaced",
            &tool_item_id,
            &replaced,
            &ToolOutput::new(replaced_output),
        );
        assert!(replaced_events
            .iter()
            .all(|event| !matches!(event, EventPayload::ArtifactUpserted { .. })));
        assert!(replaced_events.iter().any(|event| {
            matches!(
                event,
                EventPayload::ItemCommitted {
                    item: SessionItem {
                        payload: ItemPayload::FileChange(_),
                        ..
                    }
                }
            )
        }));
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
            project_id: None,
            attachments: None,
            documents: None,
            projects: Arc::new(Mutex::new(
                ProjectRegistry::open(directory.path().join("selection-projects")).unwrap(),
            )),
            trusted_files: Arc::new(Mutex::new(HashMap::new())),
            search_index: Arc::new(Mutex::new(TranscriptSearchIndex::new())),
            resources: None,
            usage: Arc::new(Mutex::new(
                InferenceRequestStore::open(directory.path()).unwrap(),
            )),
            checkout_hooks: CheckoutTestHooks::default(),
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
        assert!(matches!(
            first.events.as_slice(),
            [
                TimestampedEvent {
                    payload: EventPayload::SessionSettingsChanged {
                        model,
                        authority: AuthorityProfile::FullAccess,
                    },
                    ..
                },
                TimestampedEvent {
                    payload: EventPayload::SessionBranchEntriesAppended { entries },
                    ..
                },
                TimestampedEvent {
                    payload: EventPayload::SessionDurableHeadChanged {
                        durable_entry_id: Some(head),
                    },
                    ..
                },
            ] if model.provider == "test"
                && model.model == "selected-model"
                && model.reasoning == "high"
                && entries.len() == 1
                && entries[0].kind == SessionBranchEntryKind::Internal
                && !entries[0].checkoutable
                && entries[0].entry_id == *head
        ));
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

    #[tokio::test]
    async fn rejected_guarded_checkout_restores_durable_head_and_reopened_projection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkout-rollback.jsonl");
        let session_id = SessionId::new("checkout-rollback").unwrap();
        let model = ModelSelection {
            provider: "test".into(),
            model: "test-model".into(),
            reasoning: "off".into(),
        };
        let mut session = Session::create(&path).unwrap();
        let root = session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("question".into())],
            })))
            .unwrap();
        let previous_head = session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("answer".into())],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        let actor_seed = seed_from_session(
            &session,
            session_id.clone(),
            SessionSeedOptions {
                workspace: directory.path(),
                project_id: None,
                model: model.clone(),
                authority: AuthorityProfile::FullAccess,
                generation: 1,
                meta: None,
                attachment_store: None,
                resource_store: None,
            },
        )
        .unwrap();
        session.checkout(root.clone()).unwrap();
        drop(session);

        let mut invalid_replacement = actor_seed.clone();
        let wrong_session = SessionId::new("checkout-rollback-wrong").unwrap();
        invalid_replacement.summary.id = wrong_session.clone();
        invalid_replacement.snapshot.session_id = wrong_session;
        let (outcome, mut finalizer) = DriverCommandOutcome::guarded_replace(invalid_replacement);
        let restored_before_rejection = Arc::new(AtomicBool::new(false));
        let worker_restored = Arc::clone(&restored_before_rejection);
        let worker_path = path.clone();
        let worker_head = previous_head.clone();
        let worker = tokio::spawn(async move {
            assert_eq!(
                finalizer.decision().await.unwrap(),
                FinalizeDecision::Rollback
            );
            restore_session_head(&worker_path, worker_head.clone()).unwrap();
            let reopened = Session::open_read_only(&worker_path).unwrap();
            assert_eq!(reopened.head(), Some(worker_head));
            worker_restored.store(true, AtomicOrdering::Release);
            finalizer
                .complete(Ok(FinalizeCompletion::RolledBack))
                .unwrap();
        });

        let host_id = HostId::new("host-test").unwrap();
        let mut actor =
            SessionActorCore::new(host_id.clone(), actor_seed.clone(), ActorConfig::default())
                .unwrap();
        let command = SessionCommandEnvelope::new(
            host_id,
            DeviceId::new("device-test").unwrap(),
            session_id.clone(),
            CommandId::new("command-checkout-rollback").unwrap(),
            1,
            Some(1),
            SessionCommand::Checkout {
                entry_id: DurableEntryId::new(root.0).unwrap(),
            },
        );
        let admission = actor
            .admit_command(command, 10, |_| async { Ok(outcome) })
            .await
            .unwrap();
        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Rejected { .. }
        ));
        assert!(restored_before_rejection.load(AtomicOrdering::Acquire));
        assert_eq!(actor.snapshot(), actor_seed.snapshot);
        worker.await.unwrap();

        let reopened = Session::open_read_only(&path).unwrap();
        assert_eq!(reopened.head(), Some(previous_head.clone()));
        let restored = seed_from_session(
            &reopened,
            session_id,
            SessionSeedOptions {
                workspace: directory.path(),
                project_id: None,
                model,
                authority: AuthorityProfile::FullAccess,
                generation: 1,
                meta: None,
                attachment_store: None,
                resource_store: None,
            },
        )
        .unwrap();
        assert_eq!(
            restored.snapshot.durable_head,
            Some(DurableEntryId::new(previous_head.0).unwrap())
        );
        assert!(restored.snapshot.items.iter().any(|item| {
            matches!(
                &item.payload,
                ItemPayload::AssistantMessage { text } if text == "answer"
            )
        }));
    }

    #[tokio::test]
    async fn production_worker_quarantines_reopen_until_late_rollback_settles() {
        let directory = tempfile::tempdir().unwrap();
        let (host, session_id, old_head, target, path) =
            worker_checkout_fixture(directory.path(), "worker-quarantine");
        let gate = CheckoutRollbackGate {
            entered: Arc::new(tokio::sync::Barrier::new(2)),
            release: Arc::new(tokio::sync::Barrier::new(2)),
        };
        host.checkout_hooks
            .lock()
            .unwrap()
            .push_back(CheckoutTestHooks {
                rollback_gate: Some(gate.clone()),
                corrupt_replacement_identity: true,
                ..CheckoutTestHooks::default()
            });
        let host = Arc::new(host);
        let supervisor = Arc::new(SessionSupervisor::new(
            Arc::clone(&host),
            SupervisorConfig {
                actor: ActorConfig {
                    finalize_timeout: std::time::Duration::from_millis(20),
                    ..ActorConfig::default()
                },
                ..SupervisorConfig::default()
            },
        ));
        let original = supervisor.open_session(&session_id).await.unwrap();
        assert_eq!(host.open_count.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(
            original.view().snapshot.durable_head,
            Some(old_head.clone())
        );
        let envelope = checkout_envelope(
            &host,
            &session_id,
            original.view().snapshot.actor_generation,
            "command-worker-quarantine",
            target,
        );
        let command = {
            let supervisor = Arc::clone(&supervisor);
            tokio::spawn(async move { supervisor.command(envelope, 10).await })
        };
        gate.entered.wait().await;
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), command)
                .await
                .expect("actor finalization timeout")
                .unwrap(),
            Err(SupervisorError::Actor(ActorError::Closed))
        ));

        let mut first_reopen = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.open_session(&session_id).await })
        };
        let mut second_reopen = {
            let supervisor = Arc::clone(&supervisor);
            let session_id = session_id.clone();
            tokio::spawn(async move { supervisor.open_session(&session_id).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(60), &mut first_reopen,)
                .await
                .is_err(),
            "the old durable writer must fence reopen"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(60), &mut second_reopen,)
                .await
                .is_err(),
            "concurrent reopen must join the same ownership quarantine"
        );
        assert_eq!(host.open_count.load(AtomicOrdering::Relaxed), 1);

        gate.release.wait().await;
        let (first_reopen, second_reopen) = tokio::join!(first_reopen, second_reopen);
        let first_reopen = first_reopen.unwrap().unwrap();
        let second_reopen = second_reopen.unwrap().unwrap();
        assert_eq!(host.open_count.load(AtomicOrdering::Relaxed), 2);
        assert_eq!(
            first_reopen.view().snapshot.durable_head,
            Some(old_head.clone())
        );
        assert_eq!(
            second_reopen.view().snapshot.durable_head,
            Some(old_head.clone())
        );
        assert_eq!(
            Session::open_read_only(path).unwrap().head(),
            Some(EntryId(old_head.as_str().to_owned()))
        );
    }

    #[tokio::test]
    async fn production_worker_rolls_back_injected_seed_failure_before_rejection() {
        let directory = tempfile::tempdir().unwrap();
        let (host, session_id, old_head, target, path) =
            worker_checkout_fixture(directory.path(), "worker-seed-rollback");
        host.checkout_hooks
            .lock()
            .unwrap()
            .push_back(CheckoutTestHooks {
                fail_seed_after_checkout: true,
                ..CheckoutTestHooks::default()
            });
        let host = Arc::new(host);
        let supervisor = SessionSupervisor::new(Arc::clone(&host), SupervisorConfig::default());
        let handle = supervisor.open_session(&session_id).await.unwrap();
        let envelope = checkout_envelope(
            &host,
            &session_id,
            handle.view().snapshot.actor_generation,
            "command-worker-seed-rollback",
            target,
        );
        let admission = supervisor.command(envelope, 10).await.unwrap();
        assert!(matches!(
            admission.ack.disposition,
            AckDisposition::Rejected { .. }
        ));
        assert_eq!(handle.view().snapshot.durable_head, Some(old_head.clone()));
        assert_eq!(
            Session::open_read_only(path).unwrap().head(),
            Some(EntryId(old_head.as_str().to_owned()))
        );
    }

    #[tokio::test]
    async fn production_worker_rollback_failure_retires_owner_without_ack() {
        let directory = tempfile::tempdir().unwrap();
        let (host, session_id, old_head, target, path) =
            worker_checkout_fixture(directory.path(), "worker-rollback-loss");
        host.checkout_hooks
            .lock()
            .unwrap()
            .push_back(CheckoutTestHooks {
                fail_seed_after_checkout: true,
                fail_rollback: true,
                ..CheckoutTestHooks::default()
            });
        let host = Arc::new(host);
        let supervisor = SessionSupervisor::new(Arc::clone(&host), SupervisorConfig::default());
        let handle = supervisor.open_session(&session_id).await.unwrap();
        let envelope = checkout_envelope(
            &host,
            &session_id,
            handle.view().snapshot.actor_generation,
            "command-worker-rollback-loss",
            target.clone(),
        );
        assert!(matches!(
            supervisor.command(envelope, 10).await,
            Err(SupervisorError::Actor(ActorError::Closed))
        ));

        let reopened = supervisor.open_session(&session_id).await.unwrap();
        let final_disk_head = Session::open_read_only(path).unwrap().head().unwrap();
        let final_durable_head = DurableEntryId::new(final_disk_head.0).unwrap();
        assert_eq!(host.open_count.load(AtomicOrdering::Relaxed), 2);
        assert_eq!(
            reopened.view().snapshot.durable_head,
            Some(final_durable_head.clone())
        );
        assert_ne!(final_durable_head, old_head);
    }

    #[test]
    fn failed_pre_guard_checkout_rollback_is_fatal_owner_loss() {
        let directory = tempfile::tempdir().unwrap();
        let rollback = restore_session_head(
            &directory.path().join("missing-session.jsonl"),
            EntryId("previous-head".into()),
        );
        assert_eq!(
            checkout_rejection_after_rollback(rollback, ServiceError::InvalidSeed),
            Err(ServiceError::OwnerLost)
        );
    }

    #[test]
    fn branch_projection_is_bounded_and_always_preserves_the_selected_head() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bounded-branches.jsonl");
        let mut session = Session::create(&path).unwrap();
        let mut first = None;
        for index in 0..(MAX_PROJECTED_BRANCH_ENTRIES + 2) {
            let entry = session
                .append(EntryValue::Message(Message::User(UserMessage {
                    content: vec![UserPart::Text(format!("message {index}"))],
                })))
                .unwrap();
            first.get_or_insert(entry);
        }
        let first = first.unwrap();
        session.checkout(first.clone()).unwrap();

        let graph = branch_graph(&session).unwrap();
        assert!(graph.truncated);
        assert_eq!(graph.entries.len(), MAX_PROJECTED_BRANCH_ENTRIES);
        assert_eq!(
            graph.head,
            Some(DurableEntryId::new(first.0.clone()).unwrap())
        );
        assert!(graph
            .entries
            .iter()
            .any(|entry| entry.entry_id.as_str() == first.0));
        graph.validate().unwrap();
    }

    #[test]
    fn graphical_session_export_is_redacted_bounded_and_cleans_temporary_files() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let session_dir = directory.path().join("sessions");
        let sessions = SessionStore::new(&session_dir, workspace.path());
        std::fs::create_dir_all(sessions.dir()).unwrap();
        let session_path = sessions.dir().join("safe-export.jsonl");
        let mut session = Session::create(&session_path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("sk-1234567890123456".into())],
            })))
            .unwrap();
        drop(session);
        let session_id = SessionId::new("safe-export").unwrap();
        let serve_state_dir = directory.path().join("serve-state");
        std::fs::create_dir(&serve_state_dir).unwrap();

        let exported = export_session_bytes(
            &sessions,
            &session_id,
            &serve_state_dir,
            MAX_GRAPHICAL_SESSION_EXPORT_BYTES,
        )
        .unwrap();
        let exported: serde_json::Value = serde_json::from_slice(&exported).unwrap();
        assert_eq!(exported["format"], "ygg-session-export");
        assert_eq!(exported["redacted"], true);
        let serialized = exported.to_string();
        assert!(serialized.contains("[REDACTED]"));
        assert!(!serialized.contains("sk-1234567890123456"));
        assert_eq!(std::fs::read_dir(&serve_state_dir).unwrap().count(), 0);

        assert_eq!(
            export_session_bytes(&sessions, &session_id, &serve_state_dir, 16),
            Err(ServiceError::PayloadTooLarge)
        );
        assert_eq!(std::fs::read_dir(&serve_state_dir).unwrap().count(), 0);
        assert_eq!(
            export_session_bytes(
                &sessions,
                &SessionId::new("missing-export").unwrap(),
                &serve_state_dir,
                MAX_GRAPHICAL_SESSION_EXPORT_BYTES,
            ),
            Err(ServiceError::NotFound)
        );
        assert_eq!(std::fs::read_dir(&serve_state_dir).unwrap().count(), 0);
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
            project_id: None,
            attachments: None,
            documents: None,
            projects: Arc::new(Mutex::new(
                ProjectRegistry::open(directory.path().join("metadata-projects")).unwrap(),
            )),
            trusted_files: Arc::new(Mutex::new(HashMap::new())),
            search_index: Arc::new(Mutex::new(TranscriptSearchIndex::new())),
            resources: None,
            usage: Arc::new(Mutex::new(
                InferenceRequestStore::open(directory.path()).unwrap(),
            )),
            checkout_hooks: CheckoutTestHooks::default(),
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
        projection.pending_user_items.push_back(PendingUserItem {
            id: ItemId::new("live-original").unwrap(),
            delivery: UserMessageDelivery::Submit,
            turn_id: TurnId::new("turn-live-original").unwrap(),
            documents: Vec::new(),
            project_files: Vec::new(),
            branch_provenance: None,
        });
        projection.pending_user_items.push_back(PendingUserItem {
            id: ItemId::new("live-steer").unwrap(),
            delivery: UserMessageDelivery::Steer,
            turn_id: TurnId::new("turn-live-steer").unwrap(),
            documents: Vec::new(),
            project_files: Vec::new(),
            branch_provenance: None,
        });
        let live = project_new_entries(
            &session,
            directory.path(),
            &mut projection,
            Some(&RunId::new("run-1-1").unwrap()),
            None,
            None,
            &session_id,
        )
        .unwrap();
        let live_users = live
            .iter()
            .filter_map(|item| match &item.payload {
                ItemPayload::UserMessage { text, delivery, .. } => {
                    Some((item.id.as_str(), text.as_str(), *delivery))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            live_users,
            vec![
                (
                    "live-original",
                    "original prompt",
                    Some(UserMessageDelivery::Submit)
                ),
                (
                    "live-steer",
                    "steer exact text",
                    Some(UserMessageDelivery::Steer)
                )
            ]
        );
        drop(session);

        let reopened = Session::open_read_only(&path).unwrap();
        let seed = seed_from_session(
            &reopened,
            session_id,
            SessionSeedOptions {
                workspace: directory.path(),
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
                resource_store: None,
            },
        )
        .unwrap();
        let replayed_users = seed
            .snapshot
            .items
            .iter()
            .filter_map(|item| match &item.payload {
                ItemPayload::UserMessage { text, delivery, .. } => {
                    assert_eq!(*delivery, None);
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(replayed_users, vec!["original prompt", "steer exact text"]);
    }

    #[tokio::test]
    async fn accepted_controls_have_exact_live_delivery_and_identity() {
        let run_id = RunId::new("run-1-1").unwrap();
        let mut projection = ProjectionState::new(0);
        projection.begin_run();
        let (sender, mut receiver) = mpsc::channel(2);

        for (text, delivery) in [
            ("steer exact text", UserMessageDelivery::Steer),
            ("follow up exact text", UserMessageDelivery::FollowUp),
        ] {
            publish_control_user_item(
                &run_id,
                ResolvedPromptInput {
                    display_text: text.into(),
                    model_text: text.into(),
                    attachments: Vec::new(),
                    documents: Vec::new(),
                    project_files: Vec::new(),
                },
                delivery,
                &mut projection,
                &sender,
            )
            .await
            .unwrap();
        }

        for (index, (text, delivery)) in [
            ("steer exact text", UserMessageDelivery::Steer),
            ("follow up exact text", UserMessageDelivery::FollowUp),
        ]
        .into_iter()
        .enumerate()
        {
            let started = receiver.recv().await.expect("live control event");
            let EventPayload::ItemStarted { item } = started.payload else {
                panic!("accepted control did not start a visible item");
            };
            assert_eq!(
                item.id.as_str(),
                format!("item-run-1-1-user-1-{}", index + 1)
            );
            assert!(matches!(
                item.payload,
                ItemPayload::UserMessage {
                    text: ref actual,
                    ref attachments,
                    delivery: Some(actual_delivery),
                    ..
                } if actual == text
                    && attachments.is_empty()
                    && actual_delivery == delivery
            ));
        }
        assert_eq!(
            projection
                .pending_user_items
                .iter()
                .map(|pending| pending.delivery)
                .collect::<Vec<_>>(),
            [UserMessageDelivery::Steer, UserMessageDelivery::FollowUp]
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
            directory.path(),
            &mut projection,
            Some(&RunId::new("run-1-1").unwrap()),
            None,
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
                workspace: directory.path(),
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
                resource_store: None,
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
                ..
            }
        ));
    }

    #[test]
    fn semantic_tool_projection_redacts_canaries_and_freezes_exit_timing() {
        let workspace = tempfile::tempdir().unwrap();
        let argument_canary = "sk-live-ARGUMENT-CANARY-123456";
        let output_canary = "ghp_OUTPUTCANARY123456789";
        let arguments = serde_json::json!({
            "command": format!("cargo test token={argument_canary}"),
            "cwd": "crates/ygg"
        });
        let activity = semantic_tool_activity("bash", &arguments, workspace.path(), 10_000);
        assert_eq!(activity.kind, ToolKind::Command);
        assert_eq!(activity.phase, ActivityPhase::Verified);
        assert_eq!(activity.command_preview.as_deref(), Some("cargo test"));
        assert_eq!(activity.cwd.as_deref(), Some("crates/ygg"));

        let raw = ToolOutput::new(format!("exit=7 duration=1.25s\nstderr:\n{output_canary}"));
        let (activity, mut result) = complete_tool_activity(
            activity,
            "bash",
            &Ok(raw),
            20_000,
            ProjectedToolProgress {
                observed_output_bytes: 99,
                dropped_output_bytes: 23,
            },
        );
        result.tool_call_item_id = ItemId::new("item-redaction-test").unwrap();
        assert_eq!(activity.status, ToolActivityStatus::Failed);
        assert_eq!(activity.exit_code, Some(7));
        assert_eq!(activity.duration_ms, Some(1_250));
        assert_eq!(result.duration_ms, 1_250);
        assert_eq!(result.dropped_output_bytes, 23);

        let public = serde_json::to_string(&(activity, result)).unwrap();
        for secret in [argument_canary, output_canary] {
            assert!(
                !public.contains(secret),
                "secret canary crossed the public projection: {public}"
            );
        }
        for forbidden_field in ["arguments", "content", "progress", "stdout", "stderr"] {
            assert!(
                !public.contains(&format!("\"{forbidden_field}\"")),
                "raw field crossed the public projection: {public}"
            );
        }
    }

    #[test]
    fn verified_test_command_projects_only_parser_proven_counts() {
        let workspace = tempfile::tempdir().unwrap();
        let item_id = ItemId::new("item-test-command").unwrap();
        let activity = semantic_tool_activity(
            "bash",
            &serde_json::json!({"command": "cargo test"}),
            workspace.path(),
            10,
        );
        let output = ToolOutput::new(format!(
            "exit=0 duration=0.10s\nstdout:\n{}",
            String::from_utf8_lossy(include_bytes!(
                "../../../../extensions/ygg-serve/fixtures/test-results/cargo-libtest.txt"
            ))
        ));
        let (activity, _) = complete_tool_activity(
            activity,
            "bash",
            &Ok(output.clone()),
            110,
            ProjectedToolProgress::default(),
        );
        let projected =
            project_test_results(&item_id, &activity, &output).expect("supported test output");
        assert_eq!(projected.origin_item_id, item_id);
        assert_eq!(projected.framework, TestFramework::CargoLibtest);
        assert_eq!(projected.reported.total, None);
        assert_eq!(projected.reported.passed, None);
        assert_eq!(projected.suites[0].reported.passed, Some(2));
        assert_eq!(
            projected.verification,
            ygg_serve_backend::TestVerificationOutcome::Passed
        );

        let unsupported = ToolOutput::new("exit=0 duration=0.01s\nstdout:\nbuild completed");
        assert!(project_test_results(&item_id, &activity, &unsupported).is_none());
    }

    #[test]
    fn semantic_search_metadata_is_safe_bounded_and_workspace_relative() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("src");
        std::fs::create_dir(&source_dir).unwrap();

        let local_search = semantic_tool_activity(
            "search",
            &serde_json::json!({
                "query": "focus trap",
                "path": "src",
                "cwd": source_dir,
            }),
            workspace.path(),
            1,
        );
        assert_eq!(local_search.kind, ToolKind::Search);
        assert_eq!(local_search.target.as_deref(), Some("focus trap in src"));
        assert_eq!(local_search.cwd.as_deref(), Some("src"));

        let web_search = semantic_tool_activity(
            "web_search",
            &serde_json::json!({
                "query": "Claude app local web search",
                "url": "https://example.test/docs?token=query-secret#private",
            }),
            workspace.path(),
            2,
        );
        assert_eq!(web_search.kind, ToolKind::Web);
        assert_eq!(
            web_search.target.as_deref(),
            Some("https://example.test/docs")
        );

        let query_canary = "sk-live-QUERY-CANARY-123456";
        let redacted_search = semantic_tool_activity(
            "web_search",
            &serde_json::json!({
                "query": format!("find onboarding notes with {query_canary}"),
            }),
            workspace.path(),
            3,
        );
        assert_eq!(redacted_search.target.as_deref(), Some("[redacted query]"));
        let public = serde_json::to_string(&redacted_search).unwrap();
        assert!(!public.contains(query_canary));
        assert!(redacted_search
            .target
            .as_deref()
            .is_some_and(|target| target.len() <= 512));

        let outside = tempfile::tempdir().unwrap();
        let outside_cwd = semantic_tool_activity(
            "bash",
            &serde_json::json!({
                "command": "cargo test",
                "cwd": outside.path(),
            }),
            workspace.path(),
            4,
        );
        assert_eq!(outside_cwd.cwd, None);
        let remote_cwd = semantic_tool_activity(
            "bash",
            &serde_json::json!({
                "command": "cargo test",
                "cwd": "https://example.test/private",
            }),
            workspace.path(),
            5,
        );
        assert_eq!(remote_cwd.cwd, None);
    }

    #[tokio::test]
    async fn live_tool_progress_is_count_only_and_never_forwards_status_or_output_text() {
        let workspace = tempfile::tempdir().unwrap();
        let run_id = RunId::new("run-progress-redaction").unwrap();
        let call_id = "call-progress-redaction";
        let item_id = stable_tool_item_id(call_id).unwrap();
        let mut projection = ProjectionState::new(0);
        projection
            .tool_items
            .insert(call_id.into(), item_id.clone());
        projection.tool_calls.insert(
            call_id.into(),
            projected_tool(
                workspace.path(),
                "bash",
                serde_json::json!({"command": "cargo test"}),
            ),
        );
        let (events, mut receiver) = mpsc::channel(4);
        let canary = "xoxb-LIVE-PROGRESS-CANARY-123456";
        project_tool_progress(
            ToolCallId(call_id.into()),
            ToolProgress::Output {
                stream: ygg_agent::OutputStream::Stdout,
                bytes: bytes::Bytes::copy_from_slice(canary.as_bytes()),
            },
            &run_id,
            &mut projection,
            &events,
        )
        .await
        .unwrap();
        let output_event = receiver.recv().await.unwrap();
        let serialized = serde_json::to_string(&output_event.payload).unwrap();
        assert!(!serialized.contains(canary));
        assert!(matches!(
            output_event.payload,
            EventPayload::ItemDelta {
                item_id: actual_item_id,
                delta: ItemDelta::ToolActivity {
                    activity: ToolActivity {
                        observed_output_bytes,
                        ..
                    }
                }
            } if actual_item_id == item_id && observed_output_bytes == canary.len() as u64
        ));

        project_tool_progress(
            ToolCallId(call_id.into()),
            ToolProgress::Status("token=STATUS-CANARY-SECRET".into()),
            &run_id,
            &mut projection,
            &events,
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), receiver.recv())
                .await
                .is_err(),
            "raw status text unexpectedly produced a public event"
        );
    }

    #[test]
    fn completion_review_links_changes_verification_failures_warnings_and_outputs() {
        let workspace = tempfile::tempdir().unwrap();
        let mut projection = ProjectionState::new(0);
        let command_item = ItemId::new("item-review-command").unwrap();
        let edit_item = ItemId::new("item-review-edit").unwrap();

        let command_args = serde_json::json!({"command": "cargo test"});
        let command = semantic_tool_activity("bash", &command_args, workspace.path(), 10);
        let (command, mut command_result) = complete_tool_activity(
            command,
            "bash",
            &Ok(ToolOutput::new("exit=1 duration=0.20s\nstderr:\nfailed")),
            210,
            ProjectedToolProgress::default(),
        );
        command_result.tool_call_item_id = command_item.clone();
        projection
            .tool_items
            .insert("call-review-command".into(), command_item.clone());
        projection.tool_calls.insert(
            "call-review-command".into(),
            ProjectedToolCall {
                name: "bash".into(),
                arguments: command_args,
                activity: command,
                result: Some(command_result),
                turn_id: TurnId::new("turn-review-command").unwrap(),
            },
        );

        let edit_args = serde_json::json!({"path": "src/lib.rs"});
        let mut edit = semantic_tool_activity("edit", &edit_args, workspace.path(), 20);
        edit.status = ToolActivityStatus::Succeeded;
        edit.summary = Some("Completed".into());
        edit.completed_at_ms = Some(30);
        edit.duration_ms = Some(10);
        edit.output_summary = Some("File updated".into());
        edit.changed_paths = vec!["src/lib.rs".into()];
        projection
            .tool_items
            .insert("call-review-edit".into(), edit_item);
        projection.tool_calls.insert(
            "call-review-edit".into(),
            ProjectedToolCall {
                name: "edit".into(),
                arguments: edit_args,
                activity: edit,
                result: None,
                turn_id: TurnId::new("turn-review-edit").unwrap(),
            },
        );
        let terminal = TerminalProjection::completed();
        let changed_item = ItemId::new("item-review-change").unwrap();
        let output_id = ArtifactId::new("artifact-review").unwrap();
        let review = build_completion_review(
            &terminal,
            1,
            1_001,
            &projection,
            BTreeSet::from([changed_item.clone()]),
            BTreeSet::new(),
            BTreeSet::from([output_id.clone()]),
        );
        assert_eq!(review.duration_ms, 1_000);
        assert_eq!(review.action_count, 2);
        assert_eq!(review.changed_file_item_ids, vec![changed_item]);
        assert_eq!(
            review.verification_action_item_ids,
            vec![command_item.clone()]
        );
        assert_eq!(review.failed_action_item_ids, vec![command_item.clone()]);
        assert_eq!(review.warning_action_item_ids, vec![command_item]);
        assert_eq!(review.output_ids, vec![output_id]);
        assert_eq!(review.evidence_coverage, EvidenceCoverage::Partial);
        assert!(review
            .phases
            .iter()
            .any(|phase| phase.phase == ActivityPhase::Verified && phase.failed_count == 1));
        review.validate().unwrap();
    }

    #[test]
    fn legacy_session_without_run_record_rehydrates_a_terminal_safe_tool_call() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("legacy-semantic.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("verify this".into())],
            })))
            .unwrap();
        let argument_canary = "sk-live-LEGACY-ARGUMENT-CANARY-123456";
        let output_canary = "ghp_LEGACYOUTPUTCANARY123456789";
        let arguments =
            serde_json::json!({"command": format!("cargo test token={argument_canary}")});
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ygg_ai::ToolCall {
                    id: ToolCallId("call-legacy-semantic".into()),
                    name: "bash".into(),
                    arguments_json: serde_json::to_string(&arguments).unwrap(),
                })],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ygg_ai::ToolResult {
                    tool_call_id: ToolCallId("call-legacy-semantic".into()),
                    content: vec![ToolResultPart::Text(format!(
                        "exit=0 duration=0.05s\n{output_canary}"
                    ))],
                    is_error: false,
                })],
            })))
            .unwrap();

        let seed = seed_from_session(
            &session,
            SessionId::new("legacy-semantic").unwrap(),
            SessionSeedOptions {
                workspace: workspace.path(),
                project_id: None,
                model: ModelSelection {
                    provider: "test".into(),
                    model: "test-model".into(),
                    reasoning: "off".into(),
                },
                authority: AuthorityProfile::FullAccess,
                generation: 1,
                meta: None,
                attachment_store: None,
                resource_store: None,
            },
        )
        .unwrap();
        let activity = seed
            .snapshot
            .items
            .iter()
            .find_map(|item| match &item.payload {
                ItemPayload::ToolCall(activity) => Some(activity),
                _ => None,
            })
            .unwrap();
        assert_eq!(activity.status, ToolActivityStatus::Succeeded);
        assert_eq!(activity.command_preview.as_deref(), Some("cargo test"));
        assert_eq!(activity.duration_ms, Some(50));
        let public = serde_json::to_string(&seed.snapshot).unwrap();
        for secret in [argument_canary, output_canary] {
            assert!(!public.contains(secret));
        }
        assert!(!public.contains("\"arguments\""));
    }

    #[test]
    fn semantic_run_record_rehydrates_live_ids_timestamps_results_and_review_exactly() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("semantic-replay.jsonl");
        let mut session = Session::create(&path).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("verify this".into())],
            })))
            .unwrap();
        let arguments = serde_json::json!({"command": "cargo test"});
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ygg_ai::ToolCall {
                    id: ToolCallId("call-semantic-replay".into()),
                    name: "bash".into(),
                    arguments_json: serde_json::to_string(&arguments).unwrap(),
                })],
                model: ModelId("test-model".into()),
                protocol: Protocol::AnthropicMessages,
            })))
            .unwrap();
        let raw_result = "exit=0 duration=0.25s\n(no output)";
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ygg_ai::ToolResult {
                    tool_call_id: ToolCallId("call-semantic-replay".into()),
                    content: vec![ToolResultPart::Text(raw_result.into())],
                    is_error: false,
                })],
            })))
            .unwrap();
        session
            .append_run_outcome(SessionRunOutcome {
                status: SessionRunOutcomeStatus::Completed,
                message: None,
            })
            .unwrap();

        let session_id = SessionId::new("semantic-replay").unwrap();
        let run_id = RunId::new("run-stable-semantic").unwrap();
        let turn_id = TurnId::new("turn-stable-semantic").unwrap();
        let tool_item_id = stable_tool_item_id("call-semantic-replay").unwrap();
        let mut projection = ProjectionState::new(0);
        projection.run_started_at_ms = 1_000;
        projection.pending_user_items.push_back(PendingUserItem {
            id: ItemId::new("item-stable-user").unwrap(),
            delivery: UserMessageDelivery::Submit,
            turn_id: TurnId::new("turn-stable-user").unwrap(),
            documents: Vec::new(),
            project_files: Vec::new(),
            branch_provenance: None,
        });
        projection
            .tool_items
            .insert("call-semantic-replay".into(), tool_item_id.clone());
        projection
            .item_turns
            .insert(tool_item_id.clone(), turn_id.clone());
        let activity = semantic_tool_activity("bash", &arguments, workspace.path(), 1_100);
        let (activity, mut result) = complete_tool_activity(
            activity,
            "bash",
            &Ok(ToolOutput::new(raw_result)),
            1_350,
            ProjectedToolProgress::default(),
        );
        result.tool_call_item_id = tool_item_id;
        projection.tool_calls.insert(
            "call-semantic-replay".into(),
            ProjectedToolCall {
                name: "bash".into(),
                arguments,
                activity,
                result: Some(result),
                turn_id,
            },
        );
        let review = build_completion_review(
            &TerminalProjection::completed(),
            1_000,
            1_500,
            &projection,
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        let live = project_new_entries(
            &session,
            workspace.path(),
            &mut projection,
            Some(&run_id),
            Some(&review),
            None,
            &session_id,
        )
        .unwrap();
        let resources = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        persist_run_projection(
            &resources,
            &session_id,
            &run_id,
            1_000,
            1_500,
            &projection,
            &live,
            &review,
        )
        .unwrap();
        drop(session);
        drop(resources);

        let reopened = Session::open_read_only(&path).unwrap();
        let resources = ygg_serve_backend::ResourceStore::open(workspace.path()).unwrap();
        let seed = seed_from_session(
            &reopened,
            session_id,
            SessionSeedOptions {
                workspace: workspace.path(),
                project_id: None,
                model: ModelSelection {
                    provider: "test".into(),
                    model: "test-model".into(),
                    reasoning: "off".into(),
                },
                authority: AuthorityProfile::FullAccess,
                generation: 99,
                meta: None,
                attachment_store: None,
                resource_store: Some(&resources),
            },
        )
        .unwrap();
        assert_eq!(seed.snapshot.items, live);
        let outcome = seed
            .snapshot
            .items
            .iter()
            .find_map(|item| match &item.payload {
                ItemPayload::RunOutcome { review, .. } => Some(review),
                _ => None,
            })
            .unwrap();
        assert_eq!(outcome, &review);
        assert_eq!(outcome.duration_ms, 500);
        assert_eq!(outcome.evidence_coverage, EvidenceCoverage::Partial);
    }

    #[test]
    fn graphical_pricing_projects_tiered_input_rates() {
        let pricing = ygg_ai::Pricing {
            input: ygg_ai::TokenRate(3_000_000),
            output: ygg_ai::TokenRate(15_000_000),
            cache_read: ygg_ai::TokenRate(300_000),
            cache_write_5m: ygg_ai::TokenRate(3_750_000),
            cache_write_1h: None,
            reasoning: None,
            tiers: vec![
                ygg_ai::PricingTier {
                    min_input_tokens: 100_000,
                    input: None,
                    output: Some(ygg_ai::TokenRate(20_000_000)),
                    cache_read: None,
                    cache_write_5m: None,
                    cache_write_1h: None,
                    reasoning: None,
                },
                ygg_ai::PricingTier {
                    min_input_tokens: 200_000,
                    input: Some(ygg_ai::TokenRate(6_000_000)),
                    output: None,
                    cache_read: None,
                    cache_write_5m: None,
                    cache_write_1h: None,
                    reasoning: None,
                },
            ],
        };

        assert_eq!(
            graphical_input_pricing(Some(&pricing)),
            Some(ModelInputPricing {
                base_microdollars_per_million_tokens: 3_000_000,
                tiers: vec![ModelInputPricingTier {
                    min_input_tokens: 200_000,
                    microdollars_per_million_tokens: 6_000_000,
                }],
            })
        );
        assert_eq!(graphical_input_pricing(None), None);
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
            input_pricing: None,
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
