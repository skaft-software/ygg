#![allow(missing_docs)]

//! Product integration for language-neutral executable extensions.
//!
//! `ygg-agent` owns the typed JSON-RPC process protocol. This module owns the
//! coding product boundary: shared-resource discovery, explicit activation and
//! trust, startup diagnostics, host-state refresh, slash commands, context
//! composition, semantic status collection, and reload.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::Context as _;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use ygg_agent::extension_process::{
    ConfirmationRequest, ConfirmationResponse, ContextContribution, ContextPlacement,
    DiscoveredExtension, ExtensionEvent, ExtensionHook, ExtensionHookDisposition,
    ExtensionHostState, ExtensionManifest, ExtensionPolicy, ExtensionProcess, ExtensionRequestId,
    ExtensionRuntimeConfig, ExtensionSource, ExtensionTrust, ExtensionUiSurface, ToolRenderRequest,
    ToolRenderSegment, EXTENSION_MANIFEST_FILENAME,
};
use ygg_agent::{ExtensionHost, Session};
use ygg_ai::{AssistantMessage, AssistantPart, Message, Model, ReasoningConfig, ToolCallId};

use crate::config::Config;
use crate::resource_resolver::{
    ResolvedResource, ResourceDiagnosticLevel, ResourceKind, ResourceResolver, ResourceScope,
};
use crate::session_store::SessionStore;

const MAX_CONTEXT_CONTRIBUTION_BYTES: usize = 64 * 1024;
const MAX_EXTENSION_CONTEXT_BYTES: usize = 256 * 1024;
const MAX_CONTEXT_LABEL_BYTES: usize = 1024;
const MAX_PENDING_CONTEXT_ITEMS: usize = 256;
const MAX_DIAGNOSTIC_ENTRY_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
const MAX_DIAGNOSTIC_ENTRIES: usize = 256;
const PROMPT_RPC_DEADLINE: Duration = Duration::from_secs(5);
const AFTER_RESPONSE_RPC_DEADLINE: Duration = Duration::from_secs(2);
const STATUS_RPC_DEADLINE: Duration = Duration::from_millis(250);
const RENDERER_RPC_DEADLINE: Duration = Duration::from_millis(500);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
const BACKGROUND_UPDATE_CAPACITY: usize = 64;
const EVENT_DRAIN_BUDGET: usize = 64;
const EVENT_DRAIN_PER_RECEIVER_BUDGET: usize = 8;
const CONFIRMATION_DENIAL_QUEUE_CAPACITY: usize = 64;
const CONFIRMATION_DENIAL_CONCURRENCY: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ConfiguredTrustGrant {
    Global { name: String },
    Exact { name: String, path: PathBuf },
}

impl ConfiguredTrustGrant {
    fn matches(&self, descriptor: &DiscoveredExtension) -> bool {
        match self {
            Self::Global { name } => {
                descriptor.source == ExtensionSource::Global && descriptor.manifest.name == *name
            }
            Self::Exact { name, path } => {
                descriptor.manifest.name == *name && descriptor.manifest_path == *path
            }
        }
    }

    fn display(&self) -> String {
        match self {
            Self::Global { name } => name.clone(),
            Self::Exact { name, path } => format!("{name}@{}", path.display()),
        }
    }
}

fn extension_policy(
    config: &Config,
    diagnostics: &mut Vec<String>,
) -> (ExtensionPolicy, Vec<ConfiguredTrustGrant>) {
    let mut policy = ExtensionPolicy::default();
    for name in &config.enabled_extensions {
        policy.enable(name.clone());
    }

    let mut grants = Vec::new();
    for grant in &config.trusted_extensions {
        if let Some((name, path)) = grant.split_once('@') {
            match normalize_trusted_manifest_path(Path::new(path)) {
                Ok(path) => {
                    policy.trust_source(name.to_owned(), path.clone());
                    grants.push(ConfiguredTrustGrant::Exact {
                        name: name.to_owned(),
                        path,
                    });
                }
                Err(error) => diagnostics.push(format!(
                    "warning: invalid source-bound extension trust grant {grant:?}: {error}"
                )),
            }
        } else {
            policy.trust(grant.clone());
            grants.push(ConfiguredTrustGrant::Global {
                name: grant.clone(),
            });
        }
    }
    for name in &config.invocation_trusted_extensions {
        policy.trust_for_invocation(name.clone());
    }
    (policy, grants)
}

fn normalize_trusted_manifest_path(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("the manifest path must be absolute");
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("the manifest path has no file name"))?;
    if file_name != EXTENSION_MANIFEST_FILENAME {
        anyhow::bail!("the manifest path must end in {EXTENSION_MANIFEST_FILENAME}");
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("the manifest path has no parent directory"))?
        .canonicalize()
        .with_context(|| format!("cannot normalize manifest parent for {}", path.display()))?;
    Ok(parent.join(file_name))
}

fn persistent_trust_grant(descriptor: &DiscoveredExtension) -> String {
    if descriptor.source == ExtensionSource::Global {
        descriptor.manifest.name.clone()
    } else {
        format!(
            "{}@{}",
            descriptor.manifest.name,
            descriptor.manifest_path.display()
        )
    }
}

fn load_extension_descriptor(
    resolver: &ResourceResolver,
    resource: &ResolvedResource,
    policy: &ExtensionPolicy,
    diagnostics: &mut Vec<String>,
) -> Option<DiscoveredExtension> {
    let manifest = match resolver
        .read_text(resource)
        .and_then(|source| ExtensionManifest::parse(&source).map_err(anyhow::Error::from))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            diagnostics.push(format!("error: {}: {error}", resource.path.display()));
            return None;
        }
    };
    if resource.name != manifest.name {
        diagnostics.push(format!(
            "warning: {}: extension directory name {:?} must match manifest name {:?}; ignored",
            resource.path.display(),
            resource.name,
            manifest.name
        ));
        return None;
    }
    let source = match resource.scope {
        ResourceScope::Global => ExtensionSource::Global,
        ResourceScope::Project => ExtensionSource::Project,
        ResourceScope::Explicit => ExtensionSource::Explicit,
    };
    Some(DiscoveredExtension {
        activation: policy.activation(&manifest.name, &resource.path, source),
        manifest,
        manifest_path: resource.path.clone(),
        source,
    })
}

pub trait ExtensionConfirmationHandler {
    /// Wait until the frontend asks to cancel the in-flight command. Dropping
    /// this future must leave the input source usable by `confirm`.
    fn wait_for_cancel<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        Box::pin(std::future::pending())
    }

    fn confirm<'a>(
        &'a mut self,
        extension: &'a str,
        request: &'a ConfirmationRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + 'a>>;
}

#[derive(Clone, Debug)]
pub struct ExtensionSummary {
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    pub source: ExtensionSource,
    pub enabled: bool,
    pub trusted: bool,
    pub running: bool,
    pub tools: Vec<String>,
    pub commands: Vec<String>,
    pub hooks: Vec<ExtensionHook>,
    pub ui: Vec<ExtensionUiSurface>,
}

#[derive(Default)]
struct BoundedDiagnostics {
    entries: VecDeque<String>,
    retained_bytes: usize,
    dropped: u64,
}

impl BoundedDiagnostics {
    fn push(&mut self, message: impl Into<String>) {
        let message = truncate_diagnostic(message.into());
        while !self.entries.is_empty()
            && (self.entries.len() >= MAX_DIAGNOSTIC_ENTRIES
                || self.retained_bytes.saturating_add(message.len()) > MAX_DIAGNOSTIC_BYTES)
        {
            if let Some(removed) = self.entries.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(removed.len());
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.retained_bytes = self.retained_bytes.saturating_add(message.len());
        self.entries.push_back(message);
    }

    fn extend(&mut self, messages: impl IntoIterator<Item = String>) {
        for message in messages {
            self.push(message);
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.dropped == 0
    }

    fn iter(&self) -> impl Iterator<Item = &String> {
        self.entries.iter()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

fn truncate_diagnostic(message: String) -> String {
    const MARKER: &str = "\n[… diagnostic truncated …]";
    if message.len() <= MAX_DIAGNOSTIC_ENTRY_BYTES {
        return message.into_boxed_str().into_string();
    }
    let mut keep = MAX_DIAGNOSTIC_ENTRY_BYTES.saturating_sub(MARKER.len());
    while !message.is_char_boundary(keep) {
        keep = keep.saturating_sub(1);
    }
    let mut bounded = String::with_capacity(MAX_DIAGNOSTIC_ENTRY_BYTES);
    bounded.push_str(&message[..keep]);
    bounded.push_str(MARKER);
    bounded
}

#[derive(Default)]
struct PendingContext {
    entries: VecDeque<ContextContribution>,
    retained_bytes: usize,
}

impl PendingContext {
    fn try_push(&mut self, mut contribution: ContextContribution) -> Result<(), String> {
        let contribution_bytes = context_contribution_bytes(&contribution)?;
        if self.entries.len() >= MAX_PENDING_CONTEXT_ITEMS {
            return Err(format!(
                "pending context exceeds the {MAX_PENDING_CONTEXT_ITEMS} contribution limit"
            ));
        }
        if self.retained_bytes.saturating_add(contribution_bytes) > MAX_EXTENSION_CONTEXT_BYTES {
            return Err(format!(
                "pending context exceeds the {MAX_EXTENSION_CONTEXT_BYTES} byte aggregate limit"
            ));
        }
        contribution.label = contribution.label.into_boxed_str().into_string();
        contribution.content = contribution.content.into_boxed_str().into_string();
        self.retained_bytes = self.retained_bytes.saturating_add(contribution_bytes);
        self.entries.push_back(contribution);
        Ok(())
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn iter(&self) -> impl Iterator<Item = &ContextContribution> {
        self.entries.iter()
    }

    #[cfg(test)]
    fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn commit(&mut self, count: usize) {
        for _ in 0..count.min(self.entries.len()) {
            if let Some(contribution) = self.entries.pop_front() {
                let bytes = context_contribution_bytes(&contribution).unwrap_or_default();
                self.retained_bytes = self.retained_bytes.saturating_sub(bytes);
            }
        }
    }

    fn into_vec(self) -> Vec<ContextContribution> {
        self.entries.into_iter().collect()
    }
}

fn admit_context(
    pending_context: &mut PendingContext,
    diagnostics: &mut BoundedDiagnostics,
    source: &str,
    contribution: ContextContribution,
) -> bool {
    let label = contribution.label.clone();
    match pending_context.try_push(contribution) {
        Ok(()) => true,
        Err(error) => {
            diagnostics.push(format!(
                "warning: {source}: dropped extension context {label:?}: {error}"
            ));
            false
        }
    }
}

pub struct ExecutableExtensions {
    processes: Vec<ExtensionProcess>,
    receivers: Vec<broadcast::Receiver<ExtensionEvent>>,
    summaries: Vec<ExtensionSummary>,
    diagnostics: BoundedDiagnostics,
    pending_context: PendingContext,
    background_tx: mpsc::Sender<ExtensionBackgroundUpdate>,
    background_rx: mpsc::Receiver<ExtensionBackgroundUpdate>,
    status_generation: u64,
    status_task: Option<JoinHandle<()>>,
    renderer_tasks: Vec<JoinHandle<()>>,
    event_drain_cursor: usize,
    confirmation_denials: VecDeque<PendingConfirmationDenial>,
    confirmation_tasks: Vec<JoinHandle<()>>,
}

struct PendingConfirmationDenial {
    process: ExtensionProcess,
    request_id: ExtensionRequestId,
    generation: u64,
}

impl Default for ExecutableExtensions {
    fn default() -> Self {
        let (background_tx, background_rx) = mpsc::channel(BACKGROUND_UPDATE_CAPACITY);
        Self {
            processes: Vec::new(),
            receivers: Vec::new(),
            summaries: Vec::new(),
            diagnostics: BoundedDiagnostics::default(),
            pending_context: PendingContext::default(),
            background_tx,
            background_rx,
            status_generation: 0,
            status_task: None,
            renderer_tasks: Vec::new(),
            event_drain_cursor: 0,
            confirmation_denials: VecDeque::new(),
            confirmation_tasks: Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct ExtensionUiSnapshot {
    pub header: Option<(String, Option<String>)>,
    pub status: Option<(String, Option<String>)>,
    pub footer: Option<(String, Option<String>)>,
}

pub struct ExtensionToolRenderUpdate {
    pub id: ToolCallId,
    pub segments: Vec<ToolRenderSegment>,
}

#[derive(Default)]
pub struct ExtensionBackgroundUpdates {
    pub ui: Option<ExtensionUiSnapshot>,
    pub rendered_tools: Vec<ExtensionToolRenderUpdate>,
}

enum ExtensionBackgroundUpdate {
    Status {
        generation: u64,
        snapshot: ExtensionUiSnapshot,
        diagnostics: Vec<String>,
    },
    Renderer {
        update: Option<ExtensionToolRenderUpdate>,
        diagnostic: Option<String>,
    },
}

impl ExecutableExtensions {
    pub fn discover_and_start(
        config: &Config,
        session: &Session,
        model: &Model,
        reasoning: &ReasoningConfig,
        sessions: &SessionStore,
        host: &mut ExtensionHost,
    ) -> Self {
        let resolver = ResourceResolver::new(config.workspace.clone(), config.workspace_trusted);
        let snapshot = resolver.discover(ResourceKind::Extension, &config.extension_paths);
        let mut diagnostics = snapshot
            .diagnostics()
            .iter()
            .map(|diagnostic| {
                format!(
                    "{}: {}: {}",
                    match diagnostic.level {
                        ResourceDiagnosticLevel::Info => "info",
                        ResourceDiagnosticLevel::Warning => "warning",
                    },
                    diagnostic.path.display(),
                    diagnostic.message
                )
            })
            .collect::<Vec<_>>();

        let (policy, trust_grants) = extension_policy(config, &mut diagnostics);

        // Read through the shared no-follow, bounded boundary, then construct
        // the protocol catalog from validated values. A second filename that
        // declares the same manifest name is retained only as a diagnostic.
        let mut by_name = BTreeMap::<String, DiscoveredExtension>::new();
        for resource in snapshot.resources() {
            let Some(descriptor) =
                load_extension_descriptor(&resolver, resource, &policy, &mut diagnostics)
            else {
                continue;
            };
            if let Some(first) = by_name.get(&descriptor.manifest.name) {
                diagnostics.push(format!(
                    "warning: {}: extension {:?} duplicates {}; the first manifest wins",
                    descriptor.manifest_path.display(),
                    descriptor.manifest.name,
                    first.manifest_path.display()
                ));
            } else {
                by_name.insert(descriptor.manifest.name.clone(), descriptor);
            }
        }

        let discovered_names = by_name.keys().cloned().collect::<BTreeSet<_>>();
        for name in &config.enabled_extensions {
            if !discovered_names.contains(name) {
                diagnostics.push(format!(
                    "warning: enabled extension {name:?} was not discovered"
                ));
            }
        }
        for grant in &trust_grants {
            if !by_name.values().any(|descriptor| grant.matches(descriptor)) {
                diagnostics.push(format!(
                    "info: trust grant {:?} has no matching discovered extension source",
                    grant.display()
                ));
            }
        }
        for name in &config.invocation_trusted_extensions {
            if !discovered_names.contains(name) {
                diagnostics.push(format!(
                    "info: one-shot trust grant {name:?} has no discovered extension"
                ));
            }
        }

        let descriptors = by_name.into_values().collect::<Vec<_>>();
        for descriptor in &descriptors {
            if descriptor.activation.enabled
                && descriptor.activation.trust == ExtensionTrust::Untrusted
            {
                diagnostics.push(format!(
                    "warning: {}: extension {:?} is enabled but untrusted; add trusted_extensions = [{:?}] to the user config or pass --trust-extension {} for this invocation",
                    descriptor.manifest_path.display(),
                    descriptor.manifest.name,
                    persistent_trust_grant(descriptor),
                    descriptor.manifest.name
                ));
            }
        }
        let host_state = host_state(session, model, reasoning, sessions);
        // Executable extensions are child processes, not merely optional
        // tools. Do not start even trusted extensions when process authority
        // is disabled; discovery remains available for actionable diagnostics.
        if !config.sandbox.process_execution_allowed()
            && descriptors.iter().any(|descriptor| {
                descriptor.activation.enabled
                    && descriptor.activation.trust == ExtensionTrust::Trusted
            })
        {
            diagnostics.push(
                "executable extensions were not started: process execution is disabled by --no-process/--no-shell".to_owned(),
            );
        }
        let startable = descriptors
            .iter()
            .filter(|descriptor| {
                config.sandbox.process_execution_allowed()
                    && descriptor.activation.enabled
                    && descriptor.activation.trust == ExtensionTrust::Trusted
            })
            .cloned()
            .collect::<Vec<_>>();

        let starts = if startable.is_empty() {
            Vec::new()
        } else {
            let workspace = config.workspace.clone();
            let state = host_state.clone();
            match block_on_runtime(async move {
                futures_util::future::join_all(startable.into_iter().map(|descriptor| {
                    let mut runtime = ExtensionRuntimeConfig::new(workspace.clone());
                    runtime.host_state = state.clone();
                    ExtensionProcess::start(descriptor, runtime)
                }))
                .await
            }) {
                Ok(starts) => starts,
                Err(error) => {
                    diagnostics.push(format!(
                        "error: executable extensions could not start: {error}"
                    ));
                    Vec::new()
                }
            }
        };

        let mut processes = Vec::new();
        let mut receivers = Vec::new();
        let mut running = BTreeSet::new();
        for start in starts {
            match start {
                Ok(process) => {
                    let name = process.descriptor().manifest.name.clone();
                    receivers.push(process.subscribe());
                    host.load(&process);
                    running.insert(name);
                    processes.push(process);
                }
                Err(error) => diagnostics.push(format!("error: extension launch failed: {error}")),
            }
        }

        let summaries = descriptors
            .into_iter()
            .map(|descriptor| {
                let process = processes
                    .iter()
                    .find(|process| process.descriptor().manifest.name == descriptor.manifest.name);
                let contributions = process.map(ExtensionProcess::contributions);
                ExtensionSummary {
                    name: descriptor.manifest.name.clone(),
                    version: descriptor.manifest.version,
                    manifest_path: descriptor.manifest_path,
                    source: descriptor.source,
                    enabled: descriptor.activation.enabled,
                    trusted: descriptor.activation.trust == ExtensionTrust::Trusted,
                    running: running.contains(&descriptor.manifest.name),
                    tools: contributions
                        .map(|value| value.tools.iter().map(|tool| tool.name.clone()).collect())
                        .unwrap_or_else(|| descriptor.manifest.contributes.tools.clone()),
                    commands: contributions
                        .map(|value| {
                            value
                                .commands
                                .iter()
                                .map(|command| command.name.clone())
                                .collect()
                        })
                        .unwrap_or_else(|| descriptor.manifest.contributes.commands.clone()),
                    hooks: descriptor.manifest.contributes.hooks,
                    ui: descriptor.manifest.contributes.ui,
                }
            })
            .collect();

        let mut extensions = Self::default();
        extensions.processes = processes;
        extensions.receivers = receivers;
        extensions.summaries = summaries;
        extensions.diagnostics.extend(diagnostics);
        extensions
    }

    pub fn command_suggestions(&self) -> Vec<(String, String)> {
        self.processes
            .iter()
            .flat_map(|process| {
                process
                    .contributions()
                    .commands
                    .iter()
                    .map(|command| (command.name.clone(), command.description.clone()))
            })
            .collect()
    }

    pub fn status_summary(&self) -> String {
        let running = self
            .summaries
            .iter()
            .filter(|extension| extension.running)
            .map(|extension| extension.name.as_str())
            .collect::<Vec<_>>();
        if self.summaries.is_empty() {
            "0 running / 0 discovered".to_owned()
        } else if running.is_empty() {
            format!("0 running / {} discovered", self.summaries.len())
        } else {
            format!(
                "{} running / {} discovered ({})",
                running.len(),
                self.summaries.len(),
                running.join(", ")
            )
        }
    }

    pub fn inspect_text(&mut self) -> String {
        self.drain_events();
        let mut lines = Vec::new();
        if self.summaries.is_empty() {
            lines.push("No executable extensions discovered.".to_owned());
        } else {
            lines.push("Executable extensions".to_owned());
            for extension in &self.summaries {
                let state = match (extension.enabled, extension.trusted, extension.running) {
                    (_, _, true) => "running",
                    (true, false, false) => "enabled, untrusted",
                    (false, true, false) => "trusted, disabled",
                    (true, true, false) => "launch failed",
                    (false, false, false) => "disabled, untrusted",
                };
                lines.push(format!(
                    "- {} {} · {} · {:?} · {}",
                    extension.name,
                    extension.version,
                    state,
                    extension.source,
                    extension.manifest_path.display()
                ));
                if !extension.tools.is_empty() {
                    lines.push(format!("  tools: {}", extension.tools.join(", ")));
                }
                if !extension.commands.is_empty() {
                    lines.push(format!("  commands: /{}", extension.commands.join(", /")));
                }
                if !extension.hooks.is_empty() {
                    lines.push(format!("  hooks: {:?}", extension.hooks));
                }
                if !extension.ui.is_empty() {
                    lines.push(format!("  ui: {:?}", extension.ui));
                }
            }
        }
        if !self.diagnostics.is_empty() {
            lines.push(String::new());
            lines.push("Diagnostics".to_owned());
            if self.diagnostics.dropped > 0 {
                lines.push(format!(
                    "- warning: {} older extension diagnostic(s) were dropped to enforce the {} byte / {} entry history limit",
                    self.diagnostics.dropped, MAX_DIAGNOSTIC_BYTES, MAX_DIAGNOSTIC_ENTRIES
                ));
            }
            lines.extend(
                self.diagnostics
                    .iter()
                    .map(|diagnostic| format!("- {diagnostic}")),
            );
        }
        lines.join("\n")
    }

    pub fn refresh_host_state(
        &self,
        session: &Session,
        model: &Model,
        reasoning: &ReasoningConfig,
        sessions: &SessionStore,
    ) {
        let state = host_state(session, model, reasoning, sessions);
        for process in &self.processes {
            process.set_host_state(state.clone());
        }
    }

    fn enqueue_context(&mut self, source: &str, contribution: ContextContribution) -> bool {
        admit_context(
            &mut self.pending_context,
            &mut self.diagnostics,
            source,
            contribution,
        )
    }

    fn enqueue_contexts(
        &mut self,
        source: &str,
        contributions: impl IntoIterator<Item = ContextContribution>,
    ) {
        let mut dropped = 0usize;
        let mut last_error = None;
        for contribution in contributions {
            if let Err(error) = self.pending_context.try_push(contribution) {
                dropped = dropped.saturating_add(1);
                last_error = Some(error);
            }
        }
        if dropped > 0 {
            self.diagnostics.push(format!(
                "warning: {source}: dropped {dropped} extension context contribution(s): {}",
                last_error.unwrap_or_else(|| "context admission failed".into())
            ));
        }
    }

    pub async fn compose_prompt(
        &mut self,
        base_system: &str,
        prompt: String,
    ) -> anyhow::Result<ExtensionPromptComposition> {
        let mut notifications = self.drain_events();
        // Composition is transactional. Context already queued by an
        // extension remains pending until the complete composed prompt has
        // passed validation and can be submitted durably.
        let pending_count = self.pending_context.len();
        let mut context = PendingContext::default();
        for contribution in self.pending_context.iter().take(pending_count).cloned() {
            context
                .try_push(contribution)
                .expect("admitted pending extension context must remain valid");
        }
        let mut rejected_context = Vec::new();

        for process in &self.processes {
            let execution = process.current_context();
            if process
                .contributions()
                .hooks
                .contains(&ExtensionHook::BeforePrompt)
            {
                let output = tokio::time::timeout(
                    PROMPT_RPC_DEADLINE,
                    process.run_hook(
                        ExtensionHook::BeforePrompt,
                        serde_json::json!({ "prompt": prompt }),
                        execution.clone(),
                    ),
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "extension {:?} before_prompt hook exceeded {:?}",
                        process.descriptor().manifest.name,
                        PROMPT_RPC_DEADLINE,
                    )
                })?
                .with_context(|| {
                    format!(
                        "extension {:?} before_prompt hook failed",
                        process.descriptor().manifest.name
                    )
                })?;
                if let ExtensionHookDisposition::Deny { reason } = output.disposition {
                    anyhow::bail!(
                        "extension {:?} denied the prompt: {reason}",
                        process.descriptor().manifest.name
                    );
                }
                let mut dropped = 0usize;
                let mut last_error = None;
                for contribution in output.context {
                    if let Err(error) = context.try_push(contribution) {
                        dropped = dropped.saturating_add(1);
                        last_error = Some(error);
                    }
                }
                if dropped > 0 {
                    rejected_context.push(format!(
                        "warning: extension {:?} dropped {dropped} before_prompt context contribution(s): {}",
                        process.descriptor().manifest.name,
                        last_error.unwrap_or_else(|| "context admission failed".into())
                    ));
                }
                notifications.extend(output.notifications.into_iter().map(|notification| {
                    format_notification(&process.descriptor().manifest.name, &notification)
                }));
            }
            if process.contributions().context {
                let collected = tokio::time::timeout(
                    PROMPT_RPC_DEADLINE,
                    process.collect_context(Some(prompt.clone()), execution),
                )
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "extension {:?} context collection exceeded {:?}",
                        process.descriptor().manifest.name,
                        PROMPT_RPC_DEADLINE,
                    )
                })?
                .with_context(|| {
                    format!(
                        "extension {:?} context collection failed",
                        process.descriptor().manifest.name
                    )
                })?;
                let mut dropped = 0usize;
                let mut last_error = None;
                for contribution in collected {
                    if let Err(error) = context.try_push(contribution) {
                        dropped = dropped.saturating_add(1);
                        last_error = Some(error);
                    }
                }
                if dropped > 0 {
                    rejected_context.push(format!(
                        "warning: extension {:?} dropped {dropped} collected context contribution(s): {}",
                        process.descriptor().manifest.name,
                        last_error.unwrap_or_else(|| "context admission failed".into())
                    ));
                }
            }
        }

        notifications.extend(rejected_context.iter().cloned());
        self.diagnostics.extend(rejected_context);
        let (system, prompt) = compose_context(base_system, prompt, context.into_vec())?;
        notifications.extend(self.drain_events());
        Ok(ExtensionPromptComposition {
            system,
            prompt,
            notifications,
            pending_context_count: pending_count,
        })
    }

    /// Commit the one-shot context captured by a successful prompt
    /// composition. Frontends call this only after the user message has been
    /// appended durably; preflight/append failures leave the context available
    /// for the restored draft's retry.
    pub fn commit_prompt_context(&mut self, pending_context_count: usize) {
        self.pending_context.commit(pending_context_count);
    }

    pub async fn after_response(&mut self, response: &str) -> Vec<String> {
        let mut messages = Vec::new();
        let mut queued_context = Vec::new();
        for process in &self.processes {
            if !process
                .contributions()
                .hooks
                .contains(&ExtensionHook::AfterResponse)
            {
                continue;
            }
            match tokio::time::timeout(
                AFTER_RESPONSE_RPC_DEADLINE,
                process.run_hook(
                    ExtensionHook::AfterResponse,
                    serde_json::json!({ "response": response }),
                    process.current_context(),
                ),
            )
            .await
            {
                Err(_) => messages.push(format!(
                    "extension {:?} after_response hook exceeded {:?}",
                    process.descriptor().manifest.name,
                    AFTER_RESPONSE_RPC_DEADLINE,
                )),
                Ok(Ok(output)) => {
                    let extension_name = process.descriptor().manifest.name.clone();
                    messages.extend(
                        output.notifications.into_iter().map(|notification| {
                            format_notification(&extension_name, &notification)
                        }),
                    );
                    queued_context.extend(
                        output
                            .context
                            .into_iter()
                            .map(|contribution| (extension_name.clone(), contribution)),
                    );
                }
                Ok(Err(error)) => messages.push(format!(
                    "extension {:?} after_response hook failed: {error}",
                    process.descriptor().manifest.name
                )),
            }
        }
        for (extension_name, contribution) in queued_context {
            self.enqueue_context(&extension_name, contribution);
        }
        messages.extend(self.drain_events());
        messages
    }

    pub async fn execute_command_with_confirmation<H>(
        &mut self,
        name: &str,
        arguments: Vec<String>,
        confirmations: &mut H,
    ) -> anyhow::Result<Option<String>>
    where
        H: ExtensionConfirmationHandler + ?Sized,
    {
        let Some(process) = self
            .processes
            .iter()
            .find(|process| {
                process
                    .contributions()
                    .commands
                    .iter()
                    .any(|command| command.name == name)
            })
            .cloned()
        else {
            return Ok(None);
        };
        let extension_name = process.descriptor().manifest.name.clone();
        let mut events = process.subscribe();
        let output = {
            let mut execution = Box::pin(process.execute_command(
                name.to_owned(),
                arguments,
                process.current_context(),
            ));
            let mut events_open = true;
            loop {
                // The cancellation future and confirmation UI borrow the same
                // frontend. Keep the select in its own scope so cancellation
                // is dropped before a confirmation prompt borrows it again.
                let event = {
                    let cancellation = confirmations.wait_for_cancel();
                    tokio::pin!(cancellation);
                    tokio::select! {
                        result = &mut execution => break result?,
                        event = events.recv(), if events_open => Some(event),
                        cancelled = &mut cancellation => {
                            cancelled.with_context(|| format!(
                                "cancellation UI failed for extension {extension_name:?}"
                            ))?;
                            anyhow::bail!("extension command {name:?} cancelled");
                        }
                    }
                };
                let Some(event) = event else {
                    continue;
                };
                match event {
                    Ok(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        request,
                    }) => {
                        if process.confirmation_answered(&request_id, generation) {
                            continue;
                        }
                        let confirmed = confirmations
                            .confirm(&extension_name, &request)
                            .await
                            .with_context(|| {
                                format!("confirmation UI failed for extension {extension_name:?}")
                            })?;
                        process
                            .respond_to_confirmation(
                                request_id,
                                generation,
                                ConfirmationResponse { confirmed },
                            )
                            .await?;
                    }
                    Ok(_) => {
                        // The product's persistent receiver owns ordinary
                        // notifications, status, context, and diagnostics.
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        self.diagnostics.push(format!(
                                "warning: {extension_name}: confirmation listener lagged by {count} events"
                            ));
                    }
                    Err(broadcast::error::RecvError::Closed) => events_open = false,
                }
            }
        };
        self.enqueue_contexts(&extension_name, output.context);
        let mut blocks = Vec::new();
        if !output.text.trim().is_empty() {
            blocks.push(output.text);
        }
        blocks.extend(
            output
                .notifications
                .iter()
                .map(|notification| format_notification(name, notification)),
        );
        blocks.extend(self.drain_events());
        Ok(Some(blocks.join("\n")))
    }

    /// Start a fresh semantic UI refresh without awaiting extension RPC on the
    /// input/render path. A newer request cancels the older generation.
    pub fn request_status_refresh(&mut self) {
        if let Some(task) = self.status_task.take() {
            task.abort();
        }
        self.status_generation = self.status_generation.saturating_add(1);
        let generation = self.status_generation;
        let processes = self.processes.clone();
        let sender = self.background_tx.clone();
        self.status_task = Some(tokio::spawn(async move {
            let (snapshot, diagnostics) = collect_ui_snapshot(processes).await;
            let _ = sender
                .send(ExtensionBackgroundUpdate::Status {
                    generation,
                    snapshot,
                    diagnostics,
                })
                .await;
        }));
    }

    /// Start a semantic tool renderer without stalling Agent events or input.
    /// Returns whether a matching renderer was registered.
    pub fn request_tool_render(
        &mut self,
        id: ToolCallId,
        name: &str,
        arguments: serde_json::Value,
        output: Option<String>,
        is_error: bool,
    ) -> bool {
        let Some(process) = self
            .processes
            .iter()
            .find(|process| {
                process
                    .contributions()
                    .tool_renderers
                    .iter()
                    .any(|tool| tool == name)
            })
            .cloned()
        else {
            return false;
        };
        self.renderer_tasks.retain(|task| !task.is_finished());
        let sender = self.background_tx.clone();
        let name = name.to_owned();
        let request = ToolRenderRequest {
            name: name.clone(),
            arguments,
            output,
            is_error,
            context: process.current_context(),
        };
        self.renderer_tasks.push(tokio::spawn(async move {
            let (update, diagnostic) =
                match tokio::time::timeout(RENDERER_RPC_DEADLINE, process.render_tool(request))
                    .await
                {
                    Err(_) => (
                        None,
                        Some(format!(
                            "warning: renderer for {name:?} exceeded {RENDERER_RPC_DEADLINE:?}"
                        )),
                    ),
                    Ok(Err(error)) => (
                        None,
                        Some(format!("warning: renderer for {name:?} failed: {error}")),
                    ),
                    Ok(Ok(rendered)) => (
                        Some(ExtensionToolRenderUpdate {
                            id,
                            segments: rendered.segments,
                        }),
                        None,
                    ),
                };
            let _ = sender
                .send(ExtensionBackgroundUpdate::Renderer { update, diagnostic })
                .await;
        }));
        true
    }

    /// Drain completed background work without waiting. Stale status
    /// generations are ignored; tool renderer results retain completion order.
    pub fn drain_background_updates(&mut self) -> ExtensionBackgroundUpdates {
        let mut updates = ExtensionBackgroundUpdates::default();
        loop {
            match self.background_rx.try_recv() {
                Ok(ExtensionBackgroundUpdate::Status {
                    generation,
                    snapshot,
                    diagnostics,
                }) if generation == self.status_generation => {
                    self.diagnostics.extend(diagnostics);
                    updates.ui = Some(snapshot);
                }
                Ok(ExtensionBackgroundUpdate::Status { .. }) => {}
                Ok(ExtensionBackgroundUpdate::Renderer { update, diagnostic }) => {
                    self.diagnostics.extend(diagnostic);
                    updates.rendered_tools.extend(update);
                }
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
        updates
    }

    fn cancel_background_work(&mut self) {
        if let Some(task) = self.status_task.take() {
            task.abort();
        }
        for task in self.renderer_tasks.drain(..) {
            task.abort();
        }
        for task in self.confirmation_tasks.drain(..) {
            task.abort();
        }
        self.confirmation_denials.clear();
        while self.background_rx.try_recv().is_ok() {}
    }

    /// Gracefully stop every extension. Each protocol shutdown has its own
    /// hard timeout in `ExtensionProcess`; the outer timeout prevents a
    /// broken extension from delaying terminal restoration indefinitely.
    pub async fn shutdown(&mut self) {
        self.cancel_background_work();
        let processes = self.processes.clone();
        let shutdowns =
            futures_util::future::join_all(processes.iter().map(ExtensionProcess::shutdown));
        let _ = tokio::time::timeout(SHUTDOWN_DEADLINE, shutdowns).await;
        self.processes.clear();
        self.receivers.clear();
        for summary in &mut self.summaries {
            summary.running = false;
        }
    }

    /// Synchronous rebuild boundary used by the app constructor. Interactive
    /// rebuilds run on Ygg's multi-thread runtime, so this retains graceful
    /// protocol shutdown instead of dropping extension children.
    pub fn shutdown_blocking(&mut self) {
        let _ = block_on_runtime(self.shutdown());
    }

    pub async fn reload(&mut self) -> Vec<String> {
        self.cancel_background_work();
        let reloads = self.processes.iter().cloned().map(|process| async move {
            let name = process.descriptor().manifest.name.clone();
            (name, process.reload().await)
        });
        // `join_all` polls every reload concurrently and preserves the input
        // order in its output, so one hung child cannot serialize every
        // extension and completion timing cannot reorder user-visible lines.
        let mut messages = futures_util::future::join_all(reloads)
            .await
            .into_iter()
            .map(|(name, result)| match result {
                Ok(report) => format!(
                    "reloaded {name} (generation {}, previous shutdown {})",
                    report.generation,
                    if report.previous_shutdown_graceful {
                        "clean"
                    } else {
                        "forced"
                    }
                ),
                Err(error) => format!("unable to reload {name}: {error}"),
            })
            .collect::<Vec<_>>();
        messages.extend(self.drain_events());
        messages
    }

    fn schedule_confirmation_denials(&mut self) {
        self.confirmation_tasks.retain(|task| !task.is_finished());
        if tokio::runtime::Handle::try_current().is_err() {
            if !self.confirmation_denials.is_empty() {
                self.diagnostics
                    .push("warning: extension confirmation denials require the Ygg Tokio runtime");
            }
            return;
        }
        while self.confirmation_tasks.len() < CONFIRMATION_DENIAL_CONCURRENCY {
            let Some(pending) = self.confirmation_denials.pop_front() else {
                break;
            };
            self.confirmation_tasks.push(tokio::spawn(async move {
                let _ = pending
                    .process
                    .respond_to_confirmation(
                        pending.request_id,
                        pending.generation,
                        ConfirmationResponse { confirmed: false },
                    )
                    .await;
            }));
        }
    }

    fn queue_confirmation_denial(&mut self, pending: PendingConfirmationDenial) {
        if self.confirmation_denials.len() >= CONFIRMATION_DENIAL_QUEUE_CAPACITY {
            self.diagnostics.push(format!(
                "warning: extension confirmation denial queue reached its {CONFIRMATION_DENIAL_QUEUE_CAPACITY}-request limit; newest request was dropped"
            ));
            return;
        }
        self.confirmation_denials.push_back(pending);
        self.schedule_confirmation_denials();
    }

    /// Drain a fixed amount of extension work without letting a continuously
    /// ready process monopolize the input/render task. The start receiver
    /// rotates between calls and each receiver has a smaller per-call quota.
    pub fn drain_events(&mut self) -> Vec<String> {
        self.schedule_confirmation_denials();
        let receiver_count = self.receivers.len();
        if receiver_count == 0 {
            return Vec::new();
        }

        let start = self.event_drain_cursor % receiver_count;
        let mut remaining = EVENT_DRAIN_BUDGET;
        let mut visited = 0usize;
        let mut messages = Vec::new();
        while visited < receiver_count && remaining > 0 {
            let index = (start + visited) % receiver_count;
            let name = self
                .processes
                .get(index)
                .map(|process| process.descriptor().manifest.name.clone())
                .unwrap_or_else(|| "extension".to_owned());
            let process = self.processes.get(index).cloned();
            let mut receiver_budget = EVENT_DRAIN_PER_RECEIVER_BUDGET.min(remaining);
            while receiver_budget > 0 {
                let event = self.receivers[index].try_recv();
                match event {
                    Ok(ExtensionEvent::Notification { notification }) => {
                        messages.push(format_notification(&name, &notification));
                    }
                    Ok(ExtensionEvent::ContextContributed { contribution }) => {
                        admit_context(
                            &mut self.pending_context,
                            &mut self.diagnostics,
                            &name,
                            contribution,
                        );
                    }
                    Ok(ExtensionEvent::StatusContributed { contribution }) => {
                        messages.push(format!("[{name}] {}", contribution.text));
                    }
                    Ok(ExtensionEvent::Diagnostic { message }) => {
                        self.diagnostics.push(format!("warning: {name}: {message}"));
                    }
                    Ok(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        request,
                    }) => {
                        if process.as_ref().is_some_and(|process| {
                            process.confirmation_answered(&request_id, generation)
                        }) {
                            remaining -= 1;
                            receiver_budget -= 1;
                            continue;
                        }
                        // A request outside a frontend-controlled confirmation
                        // boundary is denied through a bounded tracked queue.
                        messages.push(format!(
                            "[{name}] confirmation denied (no active confirmation UI): {}",
                            request.prompt
                        ));
                        if let Some(process) = process.clone() {
                            self.queue_confirmation_denial(PendingConfirmationDenial {
                                process,
                                request_id,
                                generation,
                            });
                        } else {
                            self.diagnostics.push(format!(
                                "warning: {name}: confirmation could not be denied because its process is unavailable"
                            ));
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty)
                    | Err(broadcast::error::TryRecvError::Closed) => break,
                    Err(broadcast::error::TryRecvError::Lagged(count)) => {
                        messages.push(format!(
                            "[{name}] dropped {count} extension events because the consumer lagged"
                        ));
                    }
                }
                remaining -= 1;
                receiver_budget -= 1;
            }
            visited += 1;
        }
        self.event_drain_cursor = (start + visited.max(1)) % receiver_count;
        self.schedule_confirmation_denials();
        messages
    }
}

impl Drop for ExecutableExtensions {
    fn drop(&mut self) {
        self.cancel_background_work();
        if !self.processes.is_empty() {
            // Mode error paths still pass through this boundary. In the normal
            // multi-thread runtime, request graceful shutdown before Arc
            // teardown falls back to the process-group kill guard.
            self.shutdown_blocking();
        }
    }
}

async fn collect_ui_snapshot(
    processes: Vec<ExtensionProcess>,
) -> (ExtensionUiSnapshot, Vec<String>) {
    let mut requests = Vec::new();
    for process in processes {
        for surface in [
            ExtensionUiSurface::Header,
            ExtensionUiSurface::Status,
            ExtensionUiSurface::Footer,
        ] {
            if !process.contributions().ui.contains(&surface) {
                continue;
            }
            let process = process.clone();
            let name = process.descriptor().manifest.name.clone();
            requests.push(async move {
                let result = tokio::time::timeout(
                    STATUS_RPC_DEADLINE,
                    process.collect_status(surface, process.current_context()),
                )
                .await;
                (name, surface, result)
            });
        }
    }

    let mut contributions = Vec::new();
    let mut diagnostics = Vec::new();
    for (name, surface, result) in futures_util::future::join_all(requests).await {
        match result {
            Err(_) => diagnostics.push(format!(
                "warning: extension {name:?} {surface:?} status exceeded {STATUS_RPC_DEADLINE:?}"
            )),
            Ok(Err(error)) => diagnostics.push(format!(
                "warning: extension {name:?} {surface:?} status failed: {error}"
            )),
            Ok(Ok(Some(contribution))) => contributions.push(contribution),
            Ok(Ok(None)) => {}
        }
    }
    contributions.sort_by_key(|contribution| Reverse(contribution.priority));

    let mut snapshot = ExtensionUiSnapshot::default();
    for contribution in contributions {
        let value = (contribution.text, contribution.style_role);
        match contribution.surface {
            ExtensionUiSurface::Header if snapshot.header.is_none() => {
                snapshot.header = Some(value);
            }
            ExtensionUiSurface::Status if snapshot.status.is_none() => {
                snapshot.status = Some(value);
            }
            ExtensionUiSurface::Footer if snapshot.footer.is_none() => {
                snapshot.footer = Some(value);
            }
            ExtensionUiSurface::Header
            | ExtensionUiSurface::Status
            | ExtensionUiSurface::Footer => {}
        }
    }
    (snapshot, diagnostics)
}

pub struct ExtensionPromptComposition {
    pub system: String,
    pub prompt: String,
    pub notifications: Vec<String>,
    pub pending_context_count: usize,
}

pub fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            AssistantPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn latest_assistant_text(session: &Session) -> String {
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        if let ygg_agent::EntryValue::Message(Message::Assistant(message)) = &entry.value {
            return assistant_text(message);
        }
        cursor = entry.parent.clone();
    }
    String::new()
}

fn context_contribution_bytes(contribution: &ContextContribution) -> Result<usize, String> {
    if contribution.label.len() > MAX_CONTEXT_LABEL_BYTES {
        return Err(format!(
            "label exceeds the {MAX_CONTEXT_LABEL_BYTES} byte limit"
        ));
    }
    if contribution.content.len() > MAX_CONTEXT_CONTRIBUTION_BYTES {
        return Err(format!(
            "content exceeds the {MAX_CONTEXT_CONTRIBUTION_BYTES} byte limit"
        ));
    }
    if contribution.label.contains('\0') || contribution.content.contains('\0') {
        return Err("label or content contains NUL".into());
    }
    let quoted_label = format!("{:?}", contribution.label);
    Ok("<ygg-extension-context label="
        .len()
        .saturating_add(quoted_label.len())
        .saturating_add(">\n".len())
        .saturating_add(contribution.content.len())
        .saturating_add("\n</ygg-extension-context>".len())
        // `join_around` separates every non-empty block from its neighbor.
        .saturating_add("\n\n".len()))
}

fn compose_context(
    base_system: &str,
    prompt: String,
    contributions: Vec<ContextContribution>,
) -> anyhow::Result<(String, String)> {
    if contributions.len() > MAX_PENDING_CONTEXT_ITEMS {
        anyhow::bail!(
            "extension context exceeds the {} contribution limit",
            MAX_PENDING_CONTEXT_ITEMS
        );
    }
    let mut total = 0usize;
    let mut system_prefix = Vec::new();
    let mut system_suffix = Vec::new();
    let mut prompt_prefix = Vec::new();
    let mut prompt_suffix = Vec::new();
    for contribution in contributions {
        let contribution_bytes = context_contribution_bytes(&contribution).map_err(|error| {
            anyhow::anyhow!("extension context {:?}: {error}", contribution.label)
        })?;
        total = total.saturating_add(contribution_bytes);
        if total > MAX_EXTENSION_CONTEXT_BYTES {
            anyhow::bail!(
                "extension context exceeds the {} byte aggregate limit",
                MAX_EXTENSION_CONTEXT_BYTES
            );
        }
        let block = format!(
            "<ygg-extension-context label={:?}>\n{}\n</ygg-extension-context>",
            contribution.label, contribution.content
        );
        match contribution.placement {
            ContextPlacement::SystemPrefix => system_prefix.push(block),
            ContextPlacement::SystemSuffix => system_suffix.push(block),
            ContextPlacement::PromptPrefix => prompt_prefix.push(block),
            ContextPlacement::PromptSuffix => prompt_suffix.push(block),
        }
    }
    let system = join_around(system_prefix, base_system.to_owned(), system_suffix);
    let prompt = join_around(prompt_prefix, prompt, prompt_suffix);
    Ok((system, prompt))
}

fn join_around(prefix: Vec<String>, center: String, suffix: Vec<String>) -> String {
    prefix
        .into_iter()
        .chain(std::iter::once(center))
        .chain(suffix)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn format_notification(
    extension: &str,
    notification: &ygg_agent::extension_process::ExtensionNotification,
) -> String {
    let title = notification
        .title
        .as_deref()
        .map(|title| format!(" {title}:"))
        .unwrap_or_default();
    format!(
        "[{extension} {:?}]{title} {}",
        notification.level, notification.message
    )
}

fn host_state(
    session: &Session,
    model: &Model,
    reasoning: &ReasoningConfig,
    sessions: &SessionStore,
) -> ExtensionHostState {
    let session_id = session
        .path()
        .file_stem()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let session_name = session_id
        .as_deref()
        .and_then(|id| sessions.load_metadata(id).ok())
        .and_then(|metadata| metadata.name);
    let active_skills = session
        .head()
        .and_then(|head| session.resolve_active_skills(&head).ok())
        .map(|state| {
            state
                .active_skills
                .into_iter()
                .map(|skill| ygg_agent::extension_process::ExtensionActiveSkill {
                    id: skill.descriptor.id,
                    name: skill.descriptor.name,
                    version: skill.descriptor.version,
                })
                .collect()
        })
        .unwrap_or_default();
    ExtensionHostState {
        session_id,
        session_name,
        model: Some(model.spec.id.0.clone()),
        reasoning: Some(serde_json::Value::String(format!("{reasoning:?}"))),
        active_skills,
    }
}

fn block_on_runtime<F>(future: F) -> anyhow::Result<F::Output>
where
    F: Future + Send,
    F::Output: Send,
{
    let handle = Handle::try_current()
        .map_err(|_| anyhow::anyhow!("executable extensions require the Ygg Tokio runtime"))?;
    if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
        anyhow::bail!("executable extensions require Ygg's multi-thread runtime");
    }
    Ok(tokio::task::block_in_place(|| handle.block_on(future)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_extension_manifest(directory: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join(EXTENSION_MANIFEST_FILENAME),
            format!(
                r#"name = {name:?}
version = "0.1.0"
api_version = "0.1"
description = {description:?}

[entrypoint]
command = "does-not-exist"
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn global_name_trust_never_transfers_to_project_shadow() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let global_ygg = temp.path().join("global/.ygg");
        write_extension_manifest(
            &global_ygg.join("extensions/git-tools"),
            "git-tools",
            "global",
        );
        write_extension_manifest(
            &workspace.join(".ygg/extensions/git-tools"),
            "git-tools",
            "project",
        );
        let resolver = ResourceResolver::with_global_ygg_dir(workspace, true, global_ygg);
        let snapshot = resolver.discover(ResourceKind::Extension, &[]);
        let resource = snapshot.get("git-tools").unwrap();
        assert_eq!(resource.scope, ResourceScope::Project);

        let mut policy = ExtensionPolicy::default();
        policy.enable("git-tools");
        policy.trust("git-tools");
        let mut diagnostics = Vec::new();
        let descriptor =
            load_extension_descriptor(&resolver, resource, &policy, &mut diagnostics).unwrap();

        assert_eq!(descriptor.source, ExtensionSource::Project);
        assert_eq!(
            descriptor.activation.trust,
            ExtensionTrust::Untrusted,
            "a bare global grant must not launch the shadowing project process"
        );
        assert_eq!(descriptor.manifest.description.as_deref(), Some("project"));
        assert!(diagnostics.is_empty());

        policy.trust_source("git-tools", resource.path.clone());
        let descriptor =
            load_extension_descriptor(&resolver, resource, &policy, &mut diagnostics).unwrap();
        assert_eq!(descriptor.activation.trust, ExtensionTrust::Trusted);
    }

    #[test]
    fn directory_and_manifest_names_must_match() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let global_ygg = temp.path().join("global/.ygg");
        write_extension_manifest(
            &global_ygg.join("extensions/alias"),
            "actual-name",
            "mismatch",
        );
        let resolver = ResourceResolver::with_global_ygg_dir(workspace, false, global_ygg);
        let snapshot = resolver.discover(ResourceKind::Extension, &[]);
        let resource = snapshot.get("alias").unwrap();
        let mut diagnostics = Vec::new();

        let descriptor = load_extension_descriptor(
            &resolver,
            resource,
            &ExtensionPolicy::default(),
            &mut diagnostics,
        );

        assert!(descriptor.is_none());
        assert!(diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("must match manifest name")));
    }

    #[test]
    fn exact_trust_paths_normalize_only_the_parent() {
        let temp = tempfile::tempdir().unwrap();
        let extension = temp.path().join("git-tools");
        std::fs::create_dir_all(&extension).unwrap();
        let manifest = extension.join(EXTENSION_MANIFEST_FILENAME);

        assert_eq!(
            normalize_trusted_manifest_path(&manifest).unwrap(),
            extension
                .canonicalize()
                .unwrap()
                .join(EXTENSION_MANIFEST_FILENAME)
        );
        assert!(
            normalize_trusted_manifest_path(Path::new("relative/git-tools/extension.toml"))
                .is_err()
        );
        assert!(normalize_trusted_manifest_path(&extension.join("other.toml")).is_err());
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingConfirmationHandler {
        calls: Vec<(String, String)>,
    }

    #[cfg(unix)]
    impl ExtensionConfirmationHandler for RecordingConfirmationHandler {
        fn confirm<'a>(
            &'a mut self,
            extension: &'a str,
            request: &'a ConfirmationRequest,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + 'a>> {
            Box::pin(async move {
                self.calls
                    .push((extension.to_owned(), request.prompt.clone()));
                Ok(true)
            })
        }
    }

    #[cfg(unix)]
    struct ImmediateCancellationHandler;

    #[cfg(unix)]
    impl ExtensionConfirmationHandler for ImmediateCancellationHandler {
        fn wait_for_cancel<'a>(
            &'a mut self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn confirm<'a>(
            &'a mut self,
            _extension: &'a str,
            _request: &'a ConfirmationRequest,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + 'a>> {
            Box::pin(async { anyhow::bail!("unexpected confirmation") })
        }
    }

    #[test]
    fn context_composition_is_typed_deterministic_and_bounded() {
        let contributions = vec![
            ContextContribution {
                label: "rules".into(),
                content: "stay local".into(),
                placement: ContextPlacement::SystemPrefix,
            },
            ContextContribution {
                label: "repo".into(),
                content: "workspace facts".into(),
                placement: ContextPlacement::PromptSuffix,
            },
        ];
        let (system, prompt) = compose_context("system", "prompt".into(), contributions).unwrap();
        assert!(system.starts_with("<ygg-extension-context label=\"rules\">"));
        assert!(system.ends_with("system"));
        assert!(prompt.starts_with("prompt"));
        assert!(prompt.ends_with("</ygg-extension-context>"));
    }

    #[test]
    fn oversized_context_is_rejected_before_prompting() {
        let result = compose_context(
            "",
            String::new(),
            vec![ContextContribution {
                label: "large".into(),
                content: "x".repeat(MAX_CONTEXT_CONTRIBUTION_BYTES + 1),
                placement: ContextPlacement::PromptSuffix,
            }],
        );
        assert!(result.is_err());
    }

    #[test]
    fn diagnostics_history_is_byte_and_count_bounded() {
        let mut diagnostics = BoundedDiagnostics::default();
        for index in 0..(MAX_DIAGNOSTIC_ENTRIES * 2) {
            diagnostics.push(format!(
                "diagnostic-{index}:{}",
                "x".repeat(MAX_DIAGNOSTIC_ENTRY_BYTES * 2)
            ));
        }

        assert!(diagnostics.len() <= MAX_DIAGNOSTIC_ENTRIES);
        assert!(diagnostics.retained_bytes() <= MAX_DIAGNOSTIC_BYTES);
        assert!(diagnostics
            .iter()
            .all(|message| message.len() <= MAX_DIAGNOSTIC_ENTRY_BYTES));
        assert!(diagnostics
            .iter()
            .all(|message| message.ends_with("[… diagnostic truncated …]")));
        assert!(diagnostics.dropped > 0);
    }

    #[tokio::test]
    async fn event_drain_obeys_the_per_extension_frame_budget() {
        let (sender, receiver) = broadcast::channel(128);
        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(receiver);
        for index in 0..100 {
            sender
                .send(ExtensionEvent::Diagnostic {
                    message: format!("diagnostic-{index}"),
                })
                .unwrap();
        }

        extensions.drain_events();

        assert_eq!(
            extensions.diagnostics.len(),
            EVENT_DRAIN_PER_RECEIVER_BUDGET
        );
        assert_eq!(
            extensions.receivers[0].len(),
            100 - EVENT_DRAIN_PER_RECEIVER_BUDGET
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_confirmation_is_subscribed_answered_and_unblocks_the_extension() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("confirmation-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[{"name":"guarded","description":"Wait for an explicit confirmation","usage":"/guarded"}]}}'
IFS= read -r command
printf '%s\n' '{"jsonrpc":"2.0","id":"fixture-confirmation","method":"confirmation/request","params":{"prompt":"Allow fixture command?","detail":"The fixture will not finish until Ygg answers.","destructive":false,"default":false}}'
IFS= read -r confirmation
case "$confirmation" in
  *'"id":"fixture-confirmation"'*'"confirmed":true'*) ;;
  *) exit 41 ;;
esac
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"text":"fixture completed","notifications":[],"context":[]}}'
IFS= read -r shutdown
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{}}'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let manifest = ExtensionManifest::parse(
            r#"
name = "confirmation-fixture"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "confirmation-fixture.sh"

[contributes]
commands = ["guarded"]
confirmations = true
"#,
        )
        .unwrap();
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join("extension.toml"),
                source: ExtensionSource::Explicit,
                activation: ygg_agent::extension_process::ExtensionActivation {
                    enabled: true,
                    trust: ExtensionTrust::Trusted,
                },
            },
            ExtensionRuntimeConfig::new(temp.path()),
        )
        .await
        .unwrap();

        // Mirror product startup: retain the process's startup-buffered event
        // receiver before command-specific confirmation routing subscribes.
        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        let mut confirmations = RecordingConfirmationHandler::default();

        let output = tokio::time::timeout(
            Duration::from_secs(5),
            extensions.execute_command_with_confirmation("guarded", Vec::new(), &mut confirmations),
        )
        .await
        .expect("command remained blocked waiting for confirmation")
        .unwrap()
        .expect("fixture command was not registered");

        assert_eq!(output, "fixture completed");
        assert_eq!(
            confirmations.calls,
            vec![(
                "confirmation-fixture".to_owned(),
                "Allow fixture command?".to_owned()
            )]
        );
        assert!(process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_status_and_renderer_rpc_never_block_the_interactive_boundary() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::{Duration, Instant};

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("hung-ui-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
id=$(printf '%s\n' "$initialize" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[{"name":"hang","description":"Never responds"}]}}\n' "$id"
while IFS= read -r request; do
  case "$request" in
    *'"method":"shutdown"'*)
      id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      exit 0
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let manifest = ExtensionManifest::parse(
            r#"
name = "hung-ui-fixture"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "hung-ui-fixture.sh"

[contributes]
commands = ["hang"]
ui = ["status"]
tool_renderers = ["slow"]
"#,
        )
        .unwrap();
        let mut runtime = ExtensionRuntimeConfig::new(temp.path());
        runtime.request_timeout = Duration::from_secs(5);
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join("extension.toml"),
                source: ExtensionSource::Explicit,
                activation: ygg_agent::extension_process::ExtensionActivation {
                    enabled: true,
                    trust: ExtensionTrust::Trusted,
                },
            },
            runtime,
        )
        .await
        .unwrap();

        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        let started = Instant::now();
        extensions.request_status_refresh();
        assert!(extensions.request_tool_render(
            ToolCallId("call-1".into()),
            "slow",
            serde_json::json!({}),
            Some("output".into()),
            false,
        ));
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "scheduling extension work blocked the caller"
        );

        tokio::time::sleep(Duration::from_millis(700)).await;
        let updates = extensions.drain_background_updates();
        assert!(updates.ui.is_some());
        assert!(updates.rendered_tools.is_empty());
        assert!(extensions
            .diagnostics
            .iter()
            .any(|message| message.contains("status exceeded")));
        assert!(extensions
            .diagnostics
            .iter()
            .any(|message| message.contains("renderer") && message.contains("exceeded")));

        let command_started = Instant::now();
        let error = extensions
            .execute_command_with_confirmation(
                "hang",
                Vec::new(),
                &mut ImmediateCancellationHandler,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(command_started.elapsed() < Duration::from_millis(100));

        tokio::time::timeout(Duration::from_secs(2), extensions.shutdown())
            .await
            .expect("extension shutdown exceeded its global bound");
        assert!(!process.is_running());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reloads_run_concurrently_and_report_in_stable_process_order() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture_source = r#"#!/bin/sh
count_file="$YGG_WORKSPACE/$YGG_EXTENSION_NAME.count"
count=0
if [ -f "$count_file" ]; then IFS= read -r count < "$count_file"; fi
count=$((count + 1))
printf '%s\n' "$count" > "$count_file"
IFS= read -r initialize
if [ "$count" -gt 1 ]; then
  : > "$YGG_WORKSPACE/$YGG_EXTENSION_NAME.ready"
  attempts=0
  while [ ! -f "$YGG_WORKSPACE/alpha.ready" ] || [ ! -f "$YGG_WORKSPACE/beta.ready" ]; do
    attempts=$((attempts + 1))
    if [ "$attempts" -gt 500 ]; then exit 41; fi
    sleep 0.01
  done
fi
id=$(printf '%s\n' "$initialize" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[]}}\n' "$id"
while IFS= read -r request; do
  case "$request" in
    *'"method":"shutdown"'*)
      id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      exit 0
      ;;
  esac
done
"#;

        let mut extensions = ExecutableExtensions::default();
        for name in ["alpha", "beta"] {
            let directory = temp.path().join(name);
            std::fs::create_dir_all(&directory).unwrap();
            let fixture = directory.join("probe.sh");
            std::fs::write(&fixture, fixture_source).unwrap();
            let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&fixture, permissions).unwrap();
            let manifest = ExtensionManifest::parse(&format!(
                r#"
name = {name:?}
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "probe.sh"
"#
            ))
            .unwrap();
            let mut runtime = ExtensionRuntimeConfig::new(temp.path());
            runtime.request_timeout = Duration::from_secs(2);
            let process = ExtensionProcess::start(
                DiscoveredExtension {
                    manifest,
                    manifest_path: directory.join("extension.toml"),
                    source: ExtensionSource::Explicit,
                    activation: ygg_agent::extension_process::ExtensionActivation {
                        enabled: true,
                        trust: ExtensionTrust::Trusted,
                    },
                },
                runtime,
            )
            .await
            .unwrap();
            extensions.receivers.push(process.subscribe());
            extensions.processes.push(process);
        }

        let messages = tokio::time::timeout(Duration::from_secs(3), extensions.reload())
            .await
            .expect("concurrent extension reload exceeded its bound");

        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(messages[0].starts_with("reloaded alpha"), "{messages:?}");
        assert!(messages[1].starts_with("reloaded beta"), "{messages:?}");
        extensions.shutdown().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_event_flood_keeps_context_and_diagnostics_bounded() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("bounded-events-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
id=$(printf '%s\n' "$initialize" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[]}}\n' "$id"
oversized=$(printf '%*s' 65537 '' | tr ' ' x)
chunk=$(printf '%*s' 8192 '' | tr ' ' y)
long_method=$(printf '%*s' 20000 '' | tr ' ' z)
round=0
while IFS= read -r request; do
  case "$request" in
    *'"method":"context/collect"'*)
      id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","method":"context/contribution","params":{"label":"oversized","content":"%s","placement":"prompt_suffix"}}\n' "$oversized"
      if [ "$round" -eq 0 ]; then
        printf '{"jsonrpc":"2.0","method":"%s","params":{}}\n' "$long_method"
      fi
      index=0
      while [ "$index" -lt 40 ]; do
        printf '{"jsonrpc":"2.0","method":"context/contribution","params":{"label":"context-%s-%s","content":"%s","placement":"prompt_suffix"}}\n' "$round" "$index" "$chunk"
        printf '%s\n' '{"jsonrpc":"2.0","method":"flood/unknown","params":{}}'
        index=$((index + 1))
      done
      printf '{"jsonrpc":"2.0","id":%s,"result":[{"label":"collected-oversized","content":"%s","placement":"prompt_suffix"}]}\n' "$id" "$oversized"
      round=$((round + 1))
      ;;
    *'"method":"shutdown"'*)
      id=$(printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      exit 0
      ;;
  esac
done
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let manifest = ExtensionManifest::parse(
            r#"
name = "bounded-events-fixture"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "bounded-events-fixture.sh"

[contributes]
context = true
"#,
        )
        .unwrap();
        let mut runtime = ExtensionRuntimeConfig::new(temp.path());
        runtime.request_timeout = Duration::from_secs(5);
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join("extension.toml"),
                source: ExtensionSource::Explicit,
                activation: ygg_agent::extension_process::ExtensionActivation {
                    enabled: true,
                    trust: ExtensionTrust::Trusted,
                },
            },
            runtime,
        )
        .await
        .unwrap();

        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        for _ in 0..10 {
            tokio::time::timeout(
                Duration::from_secs(5),
                process.collect_context(None, process.current_context()),
            )
            .await
            .expect("fixture context flood timed out")
            .expect("fixture context request failed");
            extensions.drain_events();
        }
        // Continue at the same bounded per-frame rate after the producer has
        // stopped so the diagnostic-ring eviction path is exercised too.
        for _ in 0..128 {
            extensions.drain_events();
        }

        assert!(extensions.pending_context.len() <= MAX_PENDING_CONTEXT_ITEMS);
        assert!(extensions.pending_context.retained_bytes() <= MAX_EXTENSION_CONTEXT_BYTES);
        assert!(extensions.diagnostics.len() <= MAX_DIAGNOSTIC_ENTRIES);
        assert!(extensions.diagnostics.retained_bytes() <= MAX_DIAGNOSTIC_BYTES);
        assert!(extensions
            .diagnostics
            .iter()
            .all(|message| message.len() <= MAX_DIAGNOSTIC_ENTRY_BYTES));
        assert!(extensions
            .compose_prompt("system", "still usable".into())
            .await
            .is_ok());
        assert!(process.shutdown().await);
    }

    #[tokio::test]
    async fn invalid_and_overflow_context_are_dropped_without_bricking_prompting() {
        let mut extensions = ExecutableExtensions::default();
        assert!(!extensions.enqueue_context(
            "fixture",
            ContextContribution {
                label: "too-large".into(),
                content: "x".repeat(MAX_CONTEXT_CONTRIBUTION_BYTES + 1),
                placement: ContextPlacement::PromptSuffix,
            }
        ));
        for index in 0..(MAX_PENDING_CONTEXT_ITEMS * 2) {
            extensions.enqueue_context(
                "fixture",
                ContextContribution {
                    label: format!("context-{index}"),
                    content: "bounded".repeat(1024),
                    placement: ContextPlacement::PromptSuffix,
                },
            );
        }

        assert!(extensions.pending_context.len() <= MAX_PENDING_CONTEXT_ITEMS);
        assert!(extensions.pending_context.retained_bytes() <= MAX_EXTENSION_CONTEXT_BYTES);
        assert!(extensions.diagnostics.len() <= MAX_DIAGNOSTIC_ENTRIES);
        assert!(extensions.diagnostics.retained_bytes() <= MAX_DIAGNOSTIC_BYTES);
        assert!(extensions
            .diagnostics
            .iter()
            .any(|message| message.contains("dropped extension context")));

        let composition = extensions
            .compose_prompt("system", "prompt".into())
            .await
            .expect("rejected asynchronous context must not poison later prompts");
        assert!(composition.prompt.starts_with("prompt"));
    }

    #[tokio::test]
    async fn pending_context_is_committed_only_after_the_prompt_append_succeeds() {
        use std::io::Write as _;

        use ygg_agent::{Agent, AgentConfig, ExtensionHost, SandboxConfig};
        use ygg_ai::{AiClient, CacheRetention, ModelCatalog, ModelId};

        let temp = tempfile::tempdir().unwrap();
        let session_path = temp.path().join("session.jsonl");
        let session = Session::create(&session_path).unwrap();
        let catalog = ModelCatalog::builtin().unwrap();
        let model = catalog
            .resolve(&ModelId("gpt-5.4-mini-responses".into()))
            .unwrap();
        let mut agent = Agent::new(AgentConfig {
            client: AiClient::new(),
            model,
            session,
            system: "base system".into(),
            sandbox: SandboxConfig::new(temp.path()),
            extensions: ExtensionHost::new(),
            max_turns: None,
            reasoning: ReasoningConfig::Off,
            cache_retention: CacheRetention::Short,
            session_id: None,
        })
        .unwrap();
        let mut extensions = ExecutableExtensions::default();
        assert!(extensions.enqueue_context(
            "fixture",
            ContextContribution {
                label: "one-shot".into(),
                content: "retained for retry".into(),
                placement: ContextPlacement::PromptSuffix,
            }
        ));
        let composition = extensions
            .compose_prompt("base system", "first attempt".into())
            .await
            .unwrap();

        // Make the Agent's open session handle stale so its durable append
        // fails before mutating the in-memory session.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&session_path)
            .unwrap()
            .write_all(b" ")
            .unwrap();
        agent.set_system_prompt(composition.system);
        let failed = agent.prompt(composition.prompt).await;
        assert!(failed.is_err());
        drop(failed);

        assert_eq!(extensions.pending_context.len(), 1);
        let retry = extensions
            .compose_prompt("base system", "second attempt".into())
            .await
            .unwrap();
        assert!(retry.prompt.contains("retained for retry"));
        extensions.commit_prompt_context(retry.pending_context_count);
        assert!(extensions.pending_context.is_empty());
    }

    #[test]
    fn assistant_text_excludes_reasoning_and_tool_calls() {
        let message = AssistantMessage {
            content: vec![
                AssistantPart::Reasoning(ygg_ai::ReasoningPart {
                    text: Some("private reasoning".into()),
                    state: None,
                }),
                AssistantPart::Text("final ".into()),
                AssistantPart::ToolCall(ygg_ai::ToolCall {
                    id: ToolCallId("call-1".into()),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                }),
                AssistantPart::Text("answer".into()),
            ],
            model: ygg_ai::ModelId("test".into()),
            protocol: ygg_ai::Protocol::OpenAiResponses,
        };

        assert_eq!(assistant_text(&message), "final answer");
    }
}
