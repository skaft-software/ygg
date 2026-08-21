#![allow(missing_docs)]

//! Product integration for language-neutral executable extensions.
//!
//! `ygg-agent` owns the typed JSON-RPC process protocol. This module owns the
//! coding product boundary: shared-resource discovery, explicit activation and
//! trust, startup diagnostics, host-state refresh, slash commands, context
//! composition, semantic status collection, and reload.

#[cfg(feature = "serve")]
pub mod serve;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures_util::stream::{FuturesUnordered, StreamExt as _};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use ygg_agent::extension_process::{
    ConfirmationRequest, ConfirmationResponse, ContextContribution, ContextPlacement,
    DiscoveredExtension, ExtensionEvent, ExtensionHealthSnapshot, ExtensionHealthState,
    ExtensionHook, ExtensionHookDisposition, ExtensionHostState, ExtensionInputRequest,
    ExtensionInputResponse, ExtensionLifecycleEvent, ExtensionLifecycleOutcome, ExtensionManifest,
    ExtensionPolicy, ExtensionPolicyEvaluationResponse, ExtensionProcess, ExtensionRequestId,
    ExtensionRuntimeConfig, ExtensionSource, ExtensionTrust, ExtensionUiSurface, ToolRenderRequest,
    ToolRenderSegment, DELEGATION_TELEMETRY_SCHEMA, EXTENSION_API_VERSION_0_1,
    EXTENSION_FEATURE_AGENT_SESSIONS, EXTENSION_FEATURE_DELEGATION_TELEMETRY,
    EXTENSION_FEATURE_DYNAMIC_TOOLS, EXTENSION_MANIFEST_FILENAME,
};
use ygg_agent::{
    Agent, ExtensionHost, ExtensionPolicyDecision, ExtensionPresentationSnapshot, Session,
};
use ygg_ai::{AssistantMessage, AssistantPart, Message, Model, ReasoningConfig, ToolCallId};

use crate::config::Config;
use crate::resource_resolver::{
    ResolvedResource, ResourceDiagnosticLevel, ResourceKind, ResourceResolver, ResourceScope,
};
use crate::session_store::SessionStore;

/// The first-party extension that owns every in-harness child-session surface.
pub const SUBAGENTS_EXTENSION_NAME: &str = "ygg-subagents";
const MAX_CONTEXT_CONTRIBUTION_BYTES: usize = 64 * 1024;
/// Returns whether configuration is eligible to launch the trusted observer.
///
/// The live process/handshake check remains authoritative; this conservative
/// preflight is used only to avoid advertising Ultra in a frontend before its
/// worker app has been built.
#[cfg(feature = "serve")]
pub fn subagents_extension_activation_configured(config: &Config) -> bool {
    config.effect_policy == ygg_agent::EffectPolicy::UnsafeHost
        && config.sandbox.process_execution_allowed()
        && config
            .enabled_extensions
            .iter()
            .any(|name| name == SUBAGENTS_EXTENSION_NAME)
        && (config
            .invocation_trusted_extensions
            .iter()
            .any(|name| name == SUBAGENTS_EXTENSION_NAME)
            || config.trusted_extensions.iter().any(|grant| {
                grant == SUBAGENTS_EXTENSION_NAME
                    || grant.starts_with(&format!("{SUBAGENTS_EXTENSION_NAME}@"))
            }))
}

const MAX_EXTENSION_CONTEXT_BYTES: usize = 256 * 1024;
const MAX_CONTEXT_LABEL_BYTES: usize = 1024;
const MAX_PENDING_CONTEXT_ITEMS: usize = 256;
const MAX_DIAGNOSTIC_ENTRY_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
const MAX_DIAGNOSTIC_ENTRIES: usize = 256;
const PROMPT_RPC_DEADLINE: Duration = Duration::from_secs(5);
const AFTER_RESPONSE_RPC_DEADLINE: Duration = Duration::from_secs(2);
const RENDERER_RPC_DEADLINE: Duration = Duration::from_millis(500);
const LIFECYCLE_NOTIFY_DEADLINE: Duration = Duration::from_millis(250);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
const BACKGROUND_UPDATE_CAPACITY: usize = 64;
const EVENT_DRAIN_BUDGET: usize = 64;
const EVENT_DRAIN_PER_RECEIVER_BUDGET: usize = 8;
const CONFIRMATION_DENIAL_QUEUE_CAPACITY: usize = 64;
const CONFIRMATION_DENIAL_CONCURRENCY: usize = 8;
const INPUT_CANCELLATION_QUEUE_CAPACITY: usize = 64;
const INPUT_CANCELLATION_CONCURRENCY: usize = 8;
const CONTROLLED_EXTENSION_START_DIAGNOSTIC: &str = "executable extensions were not started: safe mode denies extension process startup; rerun without --safe-mode only inside OS-level isolation";
static NEXT_EXTENSION_RUN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(serde::Serialize)]
struct BeforePromptHookPayload<'a> {
    prompt: &'a str,
}

#[derive(serde::Serialize)]
struct AfterResponseHookPayload<'a> {
    response: &'a str,
}

fn before_prompt_hook_payload(prompt: &str) -> serde_json::Value {
    serde_json::json!(BeforePromptHookPayload { prompt })
}

fn after_response_hook_payload(response: &str) -> serde_json::Value {
    serde_json::json!(AfterResponseHookPayload { response })
}

fn denied_policy_response() -> ExtensionPolicyEvaluationResponse {
    ExtensionPolicyEvaluationResponse {
        decision: ExtensionPolicyDecision::Deny,
        approval_token: None,
    }
}

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

fn sha256_manifest(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => format!("{:x}", Sha256::digest(&bytes)),
        Err(_) => "unavailable".to_owned(),
    }
}

fn installed_bundle_digest(manifest_path: &Path) -> Option<String> {
    let install_path = manifest_path.parent()?.join("install.json");
    let bytes = std::fs::read(&install_path).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let digest = value.get("archive_sha256")?.as_str()?;
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
}

fn extension_compatibility(
    name: &str,
    running: bool,
    features: &[String],
    health: Option<&ExtensionHealthSnapshot>,
) -> (Option<String>, String) {
    if name != SUBAGENTS_EXTENSION_NAME {
        return (None, "not_applicable".to_owned());
    }
    if features
        .iter()
        .any(|feature| feature == EXTENSION_FEATURE_DELEGATION_TELEMETRY)
    {
        return (
            Some(DELEGATION_TELEMETRY_SCHEMA.to_owned()),
            "compatible".to_owned(),
        );
    }
    if running {
        return (
            None,
            "incompatible: delegation telemetry was not negotiated; rebuild/reinstall the current workspace bundle"
                .to_owned(),
        );
    }
    let error = health
        .and_then(|health| health.last_error.as_deref())
        .unwrap_or_default();
    if error.contains(EXTENSION_FEATURE_DELEGATION_TELEMETRY)
        || error.contains("required API 0.2 features")
    {
        (
            None,
            "incompatible: rebuild/reinstall the current workspace bundle".to_owned(),
        )
    } else {
        (None, "unavailable".to_owned())
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

    fn input<'a>(
        &'a mut self,
        _extension: &'a str,
        _request: &'a ExtensionInputRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + 'a>> {
        Box::pin(std::future::ready(Ok(None)))
    }
}

struct PreapprovedExtensionConfirmation<'a, H: ?Sized> {
    // One action-level approval may satisfy only the first confirmation emitted
    // by that same manifest-scoped command; later prompts still reach the UI.
    inner: &'a mut H,
    remaining: usize,
}

impl<H> ExtensionConfirmationHandler for PreapprovedExtensionConfirmation<'_, H>
where
    H: ExtensionConfirmationHandler + ?Sized,
{
    fn wait_for_cancel<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
        self.inner.wait_for_cancel()
    }

    fn confirm<'a>(
        &'a mut self,
        extension: &'a str,
        request: &'a ConfirmationRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<bool>> + 'a>> {
        if self.remaining > 0 {
            self.remaining -= 1;
            Box::pin(std::future::ready(Ok(true)))
        } else {
            self.inner.confirm(extension, request)
        }
    }

    fn input<'a>(
        &'a mut self,
        extension: &'a str,
        request: &'a ExtensionInputRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + 'a>> {
        self.inner.input(extension, request)
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ExtensionSummary {
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    pub manifest_digest: String,
    pub bundle_digest: Option<String>,
    pub source: ExtensionSource,
    pub enabled: bool,
    pub trusted: bool,
    pub running: bool,
    pub api_version: String,
    pub negotiated_features: Vec<String>,
    pub telemetry_schema: Option<String>,
    pub compatibility: String,
    pub health: Option<ExtensionHealthSnapshot>,
    pub tools: Vec<String>,
    pub commands: Vec<String>,
    pub hooks: Vec<ExtensionHook>,
    pub ui: Vec<ExtensionUiSurface>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExtensionPresentationView {
    /// Manifest-bound extension that owns this state.
    pub extension: String,
    /// Active process generation; stale generations are discarded.
    pub generation: u64,
    /// Host-created process-instance fence. This prevents a replacement
    /// process whose generation counter restarted from accepting stale actions.
    pub extension_instance_id: String,
    /// Host-derived durable session owner for frontend isolation.
    pub resource_owner: Option<String>,
    /// Complete monotonic semantic snapshot.
    pub snapshot: ExtensionPresentationSnapshot,
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

fn admit_presentation_owner(
    published: Option<ygg_agent::extension_process::ExtensionResourceOwner>,
    active_owner: Option<&str>,
) -> Result<Option<String>, String> {
    let published = published.map(|owner| owner.session_id);
    if published.is_some() && published.as_deref() != active_owner {
        return Err("discarded semantic presentation for another resource owner".into());
    }
    Ok(published)
}

fn reduce_presentation_update(
    presentations: &mut BTreeMap<String, ExtensionPresentationView>,
    extension: String,
    active_generation: Option<u64>,
    extension_instance_id: String,
    resource_owner: Option<String>,
    generation: u64,
    snapshot: ExtensionPresentationSnapshot,
) -> Result<String, String> {
    if active_generation != Some(generation) {
        return Err(format!(
            "discarded semantic presentation from stale generation {generation}"
        ));
    }
    if presentations
        .get(&extension)
        .is_some_and(|view| view.extension_instance_id != extension_instance_id)
    {
        presentations.remove(&extension);
    }
    if presentations.get(&extension).is_some_and(|view| {
        view.generation == generation && view.resource_owner.is_some() && resource_owner.is_none()
    }) {
        return Err(
            "discarded process-scoped semantic presentation while owner-scoped state is active"
                .into(),
        );
    }
    if presentations
        .get(&extension)
        .is_some_and(|view| view.resource_owner != resource_owner)
    {
        presentations.remove(&extension);
    }
    if presentations.get(&extension).is_some_and(|view| {
        view.generation > generation
            || (view.generation == generation && view.snapshot.revision >= snapshot.revision)
    }) {
        return Err(format!(
            "discarded stale semantic presentation revision {} for generation {generation}",
            snapshot.revision
        ));
    }
    let compact = snapshot
        .status
        .as_ref()
        .map(|status| status.label.clone())
        .or_else(|| {
            snapshot
                .activities
                .last()
                .map(|activity| activity.summary.clone())
        })
        .unwrap_or_else(|| "presentation updated".to_owned());
    presentations.insert(
        extension.clone(),
        ExtensionPresentationView {
            extension,
            generation,
            extension_instance_id,
            resource_owner,
            snapshot,
        },
    );
    Ok(compact)
}

pub struct ExecutableExtensions {
    processes: Vec<ExtensionProcess>,
    receivers: Vec<broadcast::Receiver<ExtensionEvent>>,
    summaries: Vec<ExtensionSummary>,
    diagnostics: BoundedDiagnostics,
    pending_context: PendingContext,
    presentations: BTreeMap<String, ExtensionPresentationView>,
    background_tx: mpsc::Sender<ExtensionBackgroundUpdate>,
    background_rx: mpsc::Receiver<ExtensionBackgroundUpdate>,
    renderer_tasks: Vec<JoinHandle<()>>,
    event_drain_cursor: usize,
    confirmation_denials: VecDeque<PendingConfirmationDenial>,
    confirmation_tasks: Vec<JoinHandle<()>>,
    input_cancellations: VecDeque<PendingInputCancellation>,
    input_tasks: Vec<JoinHandle<()>>,
    policy_supervisors: Vec<JoinHandle<()>>,
    session_id: Option<String>,
    resource_owner: Option<String>,
    session_started_at: Instant,
    session_lifecycle_started: bool,
    last_lifecycle_outcome: Option<ExtensionLifecycleOutcome>,
    #[cfg(test)]
    lifecycle_delivery_test_control: Option<std::sync::Arc<LifecycleDeliveryTestControl>>,
}

pub struct ExtensionTurnLifecycle {
    processes: Vec<ExtensionProcess>,
    resource_owner: String,
    session_id: String,
    run_id: String,
    turn_id: String,
    started_at: Instant,
    started_delivery: watch::Receiver<bool>,
    settled: bool,
    #[cfg(test)]
    lifecycle_delivery_test_control: Option<std::sync::Arc<LifecycleDeliveryTestControl>>,
}

/// Per-instance cancellation barrier used only by lifecycle ownership tests.
/// Keeping this on the owning `ExecutableExtensions` avoids global hooks and
/// lets the rest of the test suite continue to run in parallel.
#[cfg(test)]
#[derive(Default)]
struct LifecycleDeliveryTestControl {
    gate_turn_started: std::sync::atomic::AtomicBool,
    turn_started_entered: tokio::sync::Notify,
    turn_started_release: tokio::sync::Notify,
    gate_turn_settled: std::sync::atomic::AtomicBool,
    turn_settled_entered: tokio::sync::Notify,
    turn_settled_release: tokio::sync::Notify,
}

#[cfg(test)]
impl LifecycleDeliveryTestControl {
    fn gate_turn_started(&self) {
        self.gate_turn_started.store(true, Ordering::Release);
    }

    fn release_turn_started(&self) {
        self.turn_started_release.notify_one();
    }

    async fn turn_started_entered(&self) {
        self.turn_started_entered.notified().await;
    }

    fn gate_turn_settled(&self) {
        self.gate_turn_settled.store(true, Ordering::Release);
    }

    fn release_turn_settled(&self) {
        self.turn_settled_release.notify_one();
    }

    async fn turn_settled_entered(&self) {
        self.turn_settled_entered.notified().await;
    }

    async fn wait_before_delivery(&self, event: &ExtensionLifecycleEvent) {
        match event {
            ExtensionLifecycleEvent::TurnStarted { .. }
                if self.gate_turn_started.load(Ordering::Acquire) =>
            {
                self.turn_started_entered.notify_one();
                self.turn_started_release.notified().await;
            }
            ExtensionLifecycleEvent::TurnSettled { .. }
                if self.gate_turn_settled.load(Ordering::Acquire) =>
            {
                self.turn_settled_entered.notify_one();
                self.turn_settled_release.notified().await;
            }
            _ => {}
        }
    }
}

impl ExtensionTurnLifecycle {
    async fn settle(
        mut self,
        outcome: ExtensionLifecycleOutcome,
        reason: Option<String>,
    ) -> Vec<String> {
        let processes = self.processes.clone();
        let resource_owner = self.resource_owner.clone();
        let turn_id = self.turn_id.clone();
        let event = ExtensionLifecycleEvent::TurnSettled {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_id: self.turn_id.clone(),
            outcome,
            duration_ms: duration_millis(self.started_at.elapsed()),
            reason,
        };
        #[cfg(test)]
        let lifecycle_delivery_test_control = self.lifecycle_delivery_test_control.clone();
        // Transfer terminal delivery to an owned task before this future can
        // be cancelled. Dropping the JoinHandle detaches rather than aborts
        // the task, so every admitted turn retains one terminal owner.
        let delivery = tokio::spawn(async move {
            #[cfg(test)]
            if let Some(control) = lifecycle_delivery_test_control {
                control.wait_before_delivery(&event).await;
            }
            let diagnostics = notify_lifecycle_all(&processes, event).await;
            for process in &processes {
                process.clear_active_lifecycle_turn(&resource_owner, &turn_id);
            }
            diagnostics
        });
        self.settled = true;
        match delivery.await {
            Ok(diagnostics) => diagnostics,
            Err(error) => vec![format!(
                "warning: extension turn lifecycle task failed: {error}"
            )],
        }
    }
}

impl Drop for ExtensionTurnLifecycle {
    fn drop(&mut self) {
        if self.settled || self.processes.is_empty() {
            return;
        }
        let Ok(handle) = Handle::try_current() else {
            for process in &self.processes {
                process.clear_active_lifecycle_turn(&self.resource_owner, &self.turn_id);
            }
            return;
        };
        let processes = self.processes.clone();
        let resource_owner = self.resource_owner.clone();
        let turn_id = self.turn_id.clone();
        let mut started_delivery = self.started_delivery.clone();
        let event = ExtensionLifecycleEvent::TurnSettled {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_id: self.turn_id.clone(),
            outcome: ExtensionLifecycleOutcome::FrontendDisconnected,
            duration_ms: duration_millis(self.started_at.elapsed()),
            reason: Some("turn owner dropped before explicit settlement".into()),
        };
        #[cfg(test)]
        let lifecycle_delivery_test_control = self.lifecycle_delivery_test_control.clone();
        handle.spawn(async move {
            let _ = started_delivery.wait_for(|started| *started).await;
            #[cfg(test)]
            if let Some(control) = lifecycle_delivery_test_control {
                control.wait_before_delivery(&event).await;
            }
            let _ = notify_lifecycle_all(&processes, event).await;
            for process in &processes {
                process.clear_active_lifecycle_turn(&resource_owner, &turn_id);
            }
        });
    }
}

struct PendingConfirmationDenial {
    process: ExtensionProcess,
    request_id: ExtensionRequestId,
    generation: u64,
}

struct PendingInputCancellation {
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
            presentations: BTreeMap::new(),
            background_tx,
            background_rx,
            renderer_tasks: Vec::new(),
            event_drain_cursor: 0,
            confirmation_denials: VecDeque::new(),
            confirmation_tasks: Vec::new(),
            input_cancellations: VecDeque::new(),
            input_tasks: Vec::new(),
            policy_supervisors: Vec::new(),
            session_id: None,
            resource_owner: None,
            session_started_at: Instant::now(),
            session_lifecycle_started: false,
            last_lifecycle_outcome: None,
            #[cfg(test)]
            lifecycle_delivery_test_control: None,
        }
    }
}

pub struct ExtensionToolRenderUpdate {
    pub id: ToolCallId,
    pub segments: Vec<ToolRenderSegment>,
}

#[derive(Default)]
pub struct ExtensionBackgroundUpdates {
    pub rendered_tools: Vec<ExtensionToolRenderUpdate>,
}

enum ExtensionBackgroundUpdate {
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
        let has_enabled_trusted = descriptors.iter().any(|descriptor| {
            descriptor.activation.enabled && descriptor.activation.trust == ExtensionTrust::Trusted
        });
        // Executable extensions are ambient-authority child processes, not
        // merely optional tools. Controlled must prevent startup itself; later
        // broker checks cannot contain an already-running process.
        if config.effect_policy != ygg_agent::EffectPolicy::UnsafeHost && has_enabled_trusted {
            diagnostics.push(CONTROLLED_EXTENSION_START_DIAGNOSTIC.to_owned());
        }
        // Keep the independent product process gate as an additional
        // prerequisite. Discovery remains available for actionable diagnostics.
        if !config.sandbox.process_execution_allowed() && has_enabled_trusted {
            diagnostics.push(
                "executable extensions were not started: process execution is disabled by --no-process/--no-shell".to_owned(),
            );
        }
        let startable = descriptors
            .iter()
            .filter(|descriptor| {
                config.effect_policy == ygg_agent::EffectPolicy::UnsafeHost
                    && config.sandbox.process_execution_allowed()
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
            let subagents_tool_available =
                model.spec.capabilities.tools && config.tool_available("subagent_spawn");
            match block_on_runtime(async move {
                let starts = FuturesUnordered::new();
                for (index, descriptor) in startable.into_iter().enumerate() {
                    let name = descriptor.manifest.name.clone();
                    let mut runtime = ExtensionRuntimeConfig::new(workspace.clone());
                    runtime.host_state = state.clone();
                    runtime.agent_sessions =
                        name == SUBAGENTS_EXTENSION_NAME && subagents_tool_available;
                    starts.push(async move {
                        (
                            index,
                            name,
                            ExtensionProcess::start(descriptor, runtime).await,
                        )
                    });
                }
                let mut completed = Vec::new();
                tokio::pin!(starts);
                while let Some((index, name, start)) = starts.next().await {
                    if let Ok(process) = &start {
                        // Attach only the live catalog immediately. A fast
                        // dynamic child must not wait behind a slow sibling's
                        // independent initialization deadline, while observer
                        // and hook registration still follows discovery order.
                        process.register_dynamic_tool_catalog(host);
                    }
                    completed.push((index, name, start));
                }
                completed.sort_by_key(|(index, _, _)| *index);
                for (_, _, start) in &completed {
                    if let Ok(process) = start {
                        host.load(process);
                    }
                }
                completed
                    .into_iter()
                    .map(|(_, name, start)| (name, start))
                    .collect::<Vec<_>>()
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
        let mut start_failures = BTreeMap::new();
        for (name, start) in starts {
            match start {
                Ok(process) => {
                    receivers.push(process.subscribe());
                    running.insert(name);
                    processes.push(process);
                }
                Err(error) => {
                    let error = error.to_string();
                    diagnostics.push(format!("error: extension {name:?} launch failed: {error}"));
                    start_failures.insert(name, clip_lifecycle_reason(&error, 4 * 1024));
                }
            }
        }

        let summaries = descriptors
            .into_iter()
            .map(|descriptor| {
                let process = processes
                    .iter()
                    .find(|process| process.descriptor().manifest.name == descriptor.manifest.name);
                let contributions = process.map(ExtensionProcess::contributions);
                let health = process.map(ExtensionProcess::health_snapshot).or_else(|| {
                    start_failures.get(&descriptor.manifest.name).map(|error| {
                        ExtensionHealthSnapshot {
                            state: ExtensionHealthState::Parked,
                            generation: 0,
                            pending_requests: 0,
                            last_error: Some(error.clone()),
                        }
                    })
                });
                let negotiated_features: Vec<String> = process
                    .map(|process| process.negotiated_features().iter().cloned().collect())
                    .unwrap_or_default();
                let (telemetry_schema, compatibility) = extension_compatibility(
                    &descriptor.manifest.name,
                    running.contains(&descriptor.manifest.name),
                    &negotiated_features,
                    health.as_ref(),
                );
                let manifest_path = descriptor.manifest_path;
                let manifest_digest = sha256_manifest(&manifest_path);
                let bundle_digest = installed_bundle_digest(&manifest_path);
                ExtensionSummary {
                    name: descriptor.manifest.name.clone(),
                    version: descriptor.manifest.version,
                    manifest_path,
                    manifest_digest,
                    bundle_digest,
                    source: descriptor.source,
                    enabled: descriptor.activation.enabled,
                    trusted: descriptor.activation.trust == ExtensionTrust::Trusted,
                    running: running.contains(&descriptor.manifest.name),
                    api_version: process
                        .map(|process| process.api_version().to_owned())
                        .unwrap_or_else(|| descriptor.manifest.api_version.clone()),
                    negotiated_features,
                    telemetry_schema,
                    compatibility,
                    health,
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
        extensions.session_id = host_state.session_id.clone();
        extensions.resource_owner = Some(session.resource_owner_key());
        extensions.start_policy_supervisors();
        extensions.start_session_lifecycle();
        extensions
    }

    pub fn bind_agent_sessions(&self, agent: &Agent) -> anyhow::Result<usize> {
        let mut bound = 0;
        for process in &self.processes {
            if agent
                .bind_extension_agent_sessions(process)
                .with_context(|| {
                    format!(
                        "could not bind agent_sessions for extension {:?}",
                        process.descriptor().manifest.name
                    )
                })?
            {
                bound += 1;
            }
        }
        Ok(bound)
    }

    pub fn has_dynamic_tool_provider(&self) -> bool {
        self.processes.iter().any(|process| {
            process
                .negotiated_features()
                .contains(EXTENSION_FEATURE_DYNAMIC_TOOLS)
        })
    }

    fn start_policy_supervisors(&mut self) {
        let Ok(handle) = Handle::try_current() else {
            if !self.processes.is_empty() {
                self.diagnostics
                    .push("warning: extension policy supervision requires the Ygg Tokio runtime");
            }
            return;
        };
        self.policy_supervisors
            .extend(self.processes.iter().cloned().map(|process| {
                let mut events = process.subscribe();
                handle.spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(ExtensionEvent::PolicyEvaluationRequested {
                                request_id,
                                generation,
                                ..
                            }) => {
                                let _ = process
                                    .respond_to_policy_evaluation(
                                        request_id,
                                        generation,
                                        denied_policy_response(),
                                    )
                                    .await;
                            }
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
            }));
    }

    fn start_session_lifecycle(&mut self) {
        let Some(session_id) = self.session_id.clone() else {
            return;
        };
        if self.processes.is_empty() || self.session_lifecycle_started {
            return;
        }
        let processes = self.processes.clone();
        let event = ExtensionLifecycleEvent::SessionStarted {
            session_id,
            run_id: None,
        };
        match block_on_runtime(async move { notify_lifecycle_all(&processes, event).await }) {
            Ok(messages) => self.diagnostics.extend(messages),
            Err(error) => self.diagnostics.push(format!(
                "warning: extension session lifecycle could not start: {error}"
            )),
        }
        self.session_lifecycle_started = true;
        self.session_started_at = Instant::now();
    }

    pub async fn begin_turn(&self) -> ExtensionTurnLifecycle {
        let sequence = NEXT_EXTENSION_RUN_ID.fetch_add(1, Ordering::Relaxed);
        let session_id = self
            .processes
            .first()
            .and_then(|process| process.current_context().host.session_id)
            .or_else(|| self.session_id.clone())
            .unwrap_or_else(|| "unknown-session".into());
        let run_id = format!("extension-run-{sequence}");
        let turn_id = format!("extension-turn-{sequence}");
        let resource_owner = self
            .resource_owner
            .clone()
            .unwrap_or_else(|| session_id.clone());
        let started_at = Instant::now();
        let processes = self.processes.clone();
        for process in &processes {
            process.set_active_lifecycle_turn(
                resource_owner.clone(),
                session_id.clone(),
                run_id.clone(),
                turn_id.clone(),
            );
        }
        let (started_tx, started_delivery) = watch::channel(false);
        let start_processes = processes.clone();
        let start_event = ExtensionLifecycleEvent::TurnStarted {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
        };
        #[cfg(test)]
        let lifecycle_delivery_test_control = self.lifecycle_delivery_test_control.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            if let Some(control) = lifecycle_delivery_test_control {
                control.wait_before_delivery(&start_event).await;
            }
            let _ = notify_lifecycle_all(&start_processes, start_event).await;
            let _ = started_tx.send(true);
        });
        let mut turn = ExtensionTurnLifecycle {
            processes,
            resource_owner,
            session_id,
            run_id,
            turn_id,
            started_at,
            started_delivery,
            settled: false,
            #[cfg(test)]
            lifecycle_delivery_test_control: self.lifecycle_delivery_test_control.clone(),
        };
        let _ = turn.started_delivery.wait_for(|started| *started).await;
        turn
    }

    pub async fn settle_turn(
        &mut self,
        turn: ExtensionTurnLifecycle,
        outcome: &crate::modes::HostRunOutcome,
    ) {
        let lifecycle_outcome = outcome.extension_lifecycle_outcome();
        let reason = outcome
            .failure_message()
            .map(|reason| clip_lifecycle_reason(reason, 4 * 1024));
        let diagnostics = turn.settle(lifecycle_outcome, reason).await;
        self.diagnostics.extend(diagnostics);
        self.last_lifecycle_outcome = Some(lifecycle_outcome);
    }

    pub fn command_suggestions(&self) -> Vec<(String, String)> {
        self.command_suggestions_with_usage()
            .into_iter()
            .map(|(name, description, _)| (name, description))
            .collect()
    }

    /// Returns the executable command metadata needed by non-TUI discovery surfaces.
    pub fn command_suggestions_with_usage(&self) -> Vec<(String, String, Option<String>)> {
        self.processes
            .iter()
            .flat_map(|process| {
                process.contributions().commands.iter().map(|command| {
                    (
                        command.name.clone(),
                        command.description.clone(),
                        command.usage.clone(),
                    )
                })
            })
            .collect()
    }

    /// Returns the manifest identity that owns one registered slash command.
    pub fn command_owner(&self, command: &str) -> Option<String> {
        self.processes.iter().find_map(|process| {
            process
                .contributions()
                .commands
                .iter()
                .any(|definition| definition.name == command)
                .then(|| process.descriptor().manifest.name.clone())
        })
    }

    /// Renders one extension's latest host-owned presentation fallback.
    pub fn presentation_text_for(&mut self, extension: &str) -> Option<String> {
        let _ = self.drain_events();
        let view = self
            .presentation_views()
            .into_iter()
            .find(|view| view.extension == extension)?;
        Some(format_presentation_views(&[view]))
    }

    pub fn status_summary(&self) -> String {
        let ready = self
            .summaries()
            .into_iter()
            .filter(|extension| {
                extension
                    .health
                    .as_ref()
                    .is_some_and(|health| health.state == ExtensionHealthState::Ready)
            })
            .map(|extension| extension.name.clone())
            .collect::<Vec<_>>();
        if self.summaries.is_empty() {
            "0 ready / 0 discovered".to_owned()
        } else if ready.is_empty() {
            format!("0 ready / {} discovered", self.summaries.len())
        } else {
            format!(
                "{} ready / {} discovered ({})",
                ready.len(),
                self.summaries.len(),
                ready.join(", ")
            )
        }
    }

    /// Returns discovery metadata with live protocol-health snapshots overlaid.
    pub fn summaries(&self) -> Vec<ExtensionSummary> {
        let mut summaries = self.summaries.clone();
        for summary in &mut summaries {
            let Some(process) = self
                .processes
                .iter()
                .find(|process| process.descriptor().manifest.name == summary.name)
            else {
                continue;
            };
            let health = process.health_snapshot();
            summary.running = process.is_running();
            summary.health = Some(health);
            summary.api_version = process.api_version().to_owned();
            summary.negotiated_features = process.negotiated_features().iter().cloned().collect();
            let (telemetry_schema, compatibility) = extension_compatibility(
                &summary.name,
                summary.running,
                &summary.negotiated_features,
                summary.health.as_ref(),
            );
            summary.telemetry_schema = telemetry_schema;
            summary.compatibility = compatibility;
            summary.tools = process
                .tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect();
        }
        summaries
    }

    /// Returns whether the observing first-party subagents extension has a
    /// live, negotiated child-session service.
    pub fn has_agent_session_service(&self) -> bool {
        self.processes.iter().any(|process| {
            process.descriptor().manifest.name == SUBAGENTS_EXTENSION_NAME
                && process.is_running()
                && process
                    .negotiated_features()
                    .contains(EXTENSION_FEATURE_AGENT_SESSIONS)
                && process
                    .negotiated_features()
                    .contains(EXTENSION_FEATURE_DELEGATION_TELEMETRY)
        })
    }

    /// Returns the latest accepted semantic state for each running extension.
    pub fn presentation_views(&self) -> Vec<ExtensionPresentationView> {
        self.presentations
            .values()
            .filter(|view| {
                (view.resource_owner.is_none()
                    || view.resource_owner.as_deref() == self.resource_owner.as_deref())
                    && self.processes.iter().any(|process| {
                        process.descriptor().manifest.name == view.extension
                            && process.is_running()
                            && process.health_snapshot().generation == view.generation
                            && process.extension_instance_id() == view.extension_instance_id
                    })
            })
            .cloned()
            .collect()
    }

    /// Returns the exact path-free extension principal that issued a current
    /// owner-scoped delegated-session reference. Resolution remains separately
    /// parent- and principal-bound in `Agent`.
    pub fn presentation_session_reference_principal(&self, reference: &str) -> Option<String> {
        let matches = |references: &[ygg_agent::ExtensionPresentationReference]| {
            references.iter().any(|candidate| {
                candidate.kind == ygg_agent::ExtensionPresentationReferenceKind::Session
                    && candidate.id == reference
            })
        };
        let view = self.presentation_views().into_iter().find(|view| {
            view.snapshot
                .activities
                .iter()
                .any(|activity| matches(&activity.references))
                || view.snapshot.collection.as_ref().is_some_and(|collection| {
                    collection
                        .nodes
                        .iter()
                        .any(|node| matches(&node.references))
                        || collection
                            .detail
                            .as_ref()
                            .is_some_and(|detail| matches(&detail.references))
                })
        })?;
        self.processes
            .iter()
            .find(|process| {
                process.descriptor().manifest.name == view.extension
                    && process.extension_instance_id() == view.extension_instance_id
                    && process.health_snapshot().generation == view.generation
            })
            .map(ExtensionProcess::agent_session_principal)
    }

    /// Drains pending extension events and renders the generic headless fallback.
    pub fn presentation_text(&mut self) -> String {
        let _ = self.drain_events();
        format_presentation_views(&self.presentation_views())
    }

    pub fn inspect_text(&mut self) -> String {
        self.drain_events();
        let mut lines = Vec::new();
        if self.summaries.is_empty() {
            lines.push("No executable extensions discovered.".to_owned());
        } else {
            lines.push("Executable extensions".to_owned());
            for extension in self.summaries() {
                let state = match (extension.enabled, extension.trusted, extension.running) {
                    (_, _, true) => "running",
                    (true, false, false) => "enabled, untrusted",
                    (false, true, false) => "trusted, disabled",
                    (true, true, false) => "launch failed",
                    (false, false, false) => "disabled, untrusted",
                };
                lines.push(format!(
                    "- {} {} · API {} · {} · {:?} · {}",
                    extension.name,
                    extension.version,
                    extension.api_version,
                    state,
                    extension.source,
                    extension.manifest_path.display()
                ));
                lines.push(format!(
                    "  manifest sha256: {} · bundle sha256: {}",
                    extension.manifest_digest,
                    extension.bundle_digest.as_deref().unwrap_or("unpackaged"),
                ));
                lines.push(format!(
                    "  compatibility: {} · telemetry schema: {}",
                    extension.compatibility,
                    extension
                        .telemetry_schema
                        .as_deref()
                        .unwrap_or("not negotiated"),
                ));
                if let Some(health) = &extension.health {
                    lines.push(format!(
                        "  health: {:?} · generation {} · {} pending{}",
                        health.state,
                        health.generation,
                        health.pending_requests,
                        health
                            .last_error
                            .as_ref()
                            .map(|error| format!(" · last error: {error}"))
                            .unwrap_or_default()
                    ));
                }
                if !extension.negotiated_features.is_empty() {
                    lines.push(format!(
                        "  features: {}",
                        extension.negotiated_features.join(", ")
                    ));
                }
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
        let presentation = self.presentation_text();
        if !presentation.is_empty() {
            lines.push(String::new());
            lines.push("Extension activity".to_owned());
            lines.push(presentation);
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
            let execution = extension_execution_context(process, self.resource_owner.as_deref());
            if process
                .contributions()
                .hooks
                .contains(&ExtensionHook::BeforePrompt)
            {
                let output = tokio::time::timeout(
                    PROMPT_RPC_DEADLINE,
                    process.run_hook(
                        ExtensionHook::BeforePrompt,
                        before_prompt_hook_payload(&prompt),
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
            let execution = extension_execution_context(process, self.resource_owner.as_deref());
            match tokio::time::timeout(
                AFTER_RESPONSE_RPC_DEADLINE,
                process.run_hook(
                    ExtensionHook::AfterResponse,
                    after_response_hook_payload(response),
                    execution,
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

    pub async fn execute_presentation_action_with_confirmation<H>(
        &mut self,
        extension: &str,
        action_id: &str,
        confirmations: &mut H,
    ) -> anyhow::Result<String>
    where
        H: ExtensionConfirmationHandler + ?Sized,
    {
        let action = self
            .presentation_views()
            .into_iter()
            .find(|view| view.extension == extension)
            .and_then(|view| {
                view.snapshot
                    .actions
                    .into_iter()
                    .find(|action| action.id == action_id)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                "extension presentation action {extension:?}/{action_id:?} is unavailable or stale"
            )
            })?;
        let mut approval_budget = 0;
        if action.destructive {
            let request = ConfirmationRequest {
                parent_request_id: None,
                prompt: format!("Run {}?", action.label),
                detail: Some(format!("Declared by extension {extension:?}")),
                destructive: true,
                default: false,
            };
            if !confirmations.confirm(extension, &request).await? {
                anyhow::bail!("extension presentation action was denied");
            }
            approval_budget = 1;
        }
        let mut command_confirmations = PreapprovedExtensionConfirmation {
            inner: confirmations,
            remaining: approval_budget,
        };
        self.execute_command_with_confirmation_scoped(
            Some(extension),
            &action.command,
            action.arguments,
            &mut command_confirmations,
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("extension presentation action routed to an unavailable command")
        })
    }

    /// Executes an authenticated Serve action with one command-scoped approval.
    #[cfg(feature = "serve")]
    pub async fn execute_presentation_action_for_serve(
        &mut self,
        extension: &str,
        expected_extension_instance_id: &str,
        expected_generation: u64,
        expected_revision: u64,
        action_id: &str,
        confirmed: bool,
    ) -> anyhow::Result<String> {
        let action = self
            .presentation_views()
            .into_iter()
            .find(|view| {
                view.extension == extension
                    && view.extension_instance_id == expected_extension_instance_id
                    && view.generation == expected_generation
                    && view.snapshot.revision == expected_revision
            })
            .and_then(|view| {
                view.snapshot
                    .actions
                    .into_iter()
                    .find(|action| action.id == action_id)
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                "extension presentation action {extension:?}/{action_id:?} is unavailable or stale"
            )
            })?;
        if confirmed && !action.destructive {
            anyhow::bail!("non-destructive extension action cannot carry approval");
        }
        if action.destructive && !confirmed {
            anyhow::bail!("extension presentation action requires explicit confirmation");
        }
        self.execute_command_headless_scoped(
            Some(extension),
            &action.command,
            action.arguments,
            usize::from(action.destructive && confirmed),
        )
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("extension presentation action routed to an unavailable command")
        })
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
        self.execute_command_with_confirmation_scoped(None, name, arguments, confirmations)
            .await
    }

    async fn execute_command_with_confirmation_scoped<H>(
        &mut self,
        extension: Option<&str>,
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
                extension.is_none_or(|extension| process.descriptor().manifest.name == extension)
                    && process
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
        let execution_context =
            extension_execution_context(&process, self.resource_owner.as_deref());
        let mut events = process.subscribe();
        let output = {
            let legacy_uncorrelated = process.api_version() == EXTENSION_API_VERSION_0_1;
            let (request_started, started) = tokio::sync::oneshot::channel();
            let mut started = Box::pin(started);
            let mut operation = None;
            let mut execution = Box::pin(process.execute_command_controlled(
                name.to_owned(),
                arguments,
                execution_context,
                request_started,
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
                        started = &mut started, if operation.is_none() => match started {
                            Ok(started) => {
                                operation = Some(started);
                                None
                            }
                            Err(_) => break execution.await?,
                        },
                        event = events.recv(), if events_open && operation.is_some() => Some(event),
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
                        parent_request_id,
                        request,
                    }) if parent_request_id.is_some_and(|parent| {
                        operation.is_some_and(|operation| operation.owns(generation, parent))
                    }) || (legacy_uncorrelated
                        && parent_request_id.is_none()
                        && operation
                            .is_some_and(|operation| operation.generation == generation)) =>
                    {
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
                    Ok(ExtensionEvent::PolicyEvaluationRequested { .. }) => {}
                    Ok(ExtensionEvent::InputRequested {
                        request_id,
                        generation,
                        parent_request_id,
                        request,
                    }) if operation
                        .is_some_and(|operation| operation.owns(generation, parent_request_id)) =>
                    {
                        if process.input_answered(&request_id, generation) {
                            continue;
                        }
                        let value = confirmations
                            .input(&extension_name, &request)
                            .await
                            .with_context(|| {
                                format!("input UI failed for extension {extension_name:?}")
                            })?;
                        process
                            .respond_to_input(
                                request_id,
                                generation,
                                ExtensionInputResponse { value },
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

    /// Executes an extension command at a non-interactive boundary.
    ///
    /// Extension confirmation requests are explicitly denied and reported as a
    /// failed invocation because no trusted confirmation surface is available to
    /// the caller. Commands that do not request confirmation retain ordinary
    /// output and queued-context handling.
    pub async fn execute_command_without_confirmation(
        &mut self,
        name: &str,
        arguments: Vec<String>,
    ) -> anyhow::Result<Option<String>> {
        self.execute_command_headless_scoped(None, name, arguments, 0)
            .await
    }

    async fn execute_command_headless_scoped(
        &mut self,
        extension: Option<&str>,
        name: &str,
        arguments: Vec<String>,
        mut approval_budget: usize,
    ) -> anyhow::Result<Option<String>> {
        let Some(process) = self
            .processes
            .iter()
            .find(|process| {
                extension.is_none_or(|extension| process.descriptor().manifest.name == extension)
                    && process
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
        let execution_context =
            extension_execution_context(&process, self.resource_owner.as_deref());
        let mut events = process.subscribe();
        let (output, confirmation_denied) = {
            let mut confirmation_denied = false;
            let output = {
                let legacy_uncorrelated = process.api_version() == EXTENSION_API_VERSION_0_1;
                let (request_started, started) = tokio::sync::oneshot::channel();
                let mut started = Box::pin(started);
                let mut operation = None;
                let mut execution = Box::pin(process.execute_command_controlled(
                    name.to_owned(),
                    arguments,
                    execution_context,
                    request_started,
                ));
                let mut events_open = true;
                loop {
                    tokio::select! {
                        result = &mut execution => break result?,
                        started = &mut started, if operation.is_none() => match started {
                            Ok(started) => operation = Some(started),
                            Err(_) => break execution.await?,
                        },
                        event = events.recv(), if events_open && operation.is_some() => match event {
                            Ok(ExtensionEvent::ConfirmationRequested {
                                request_id,
                                generation,
                                parent_request_id,
                                ..
                            }) if parent_request_id.is_some_and(|parent| {
                                operation.is_some_and(|operation| operation.owns(generation, parent))
                            }) || (legacy_uncorrelated
                                && parent_request_id.is_none()
                                && operation.is_some_and(|operation| operation.generation == generation)) => {
                                if !process.confirmation_answered(&request_id, generation) {
                                    let confirmed = approval_budget > 0;
                                    if confirmed {
                                        approval_budget -= 1;
                                    } else {
                                        confirmation_denied = true;
                                    }
                                    process
                                        .respond_to_confirmation(
                                            request_id,
                                            generation,
                                            ConfirmationResponse { confirmed },
                                        )
                                        .await?;
                                }
                            }
                            Ok(ExtensionEvent::PolicyEvaluationRequested { .. }) => {}
                            Ok(ExtensionEvent::InputRequested {
                                request_id,
                                generation,
                                parent_request_id,
                                ..
                            }) if operation.is_some_and(|operation| {
                                operation.owns(generation, parent_request_id)
                            }) => {
                                process
                                    .respond_to_input(
                                        request_id,
                                        generation,
                                        ExtensionInputResponse { value: None },
                                    )
                                    .await?;
                            }
                            Ok(_) => {
                                // The persistent receiver owns ordinary notifications,
                                // status, context, and diagnostics.
                            }
                            Err(broadcast::error::RecvError::Lagged(count)) => {
                                self.diagnostics.push(format!(
                                    "warning: {extension_name}: confirmation listener lagged by {count} events"
                                ));
                            }
                            Err(broadcast::error::RecvError::Closed) => events_open = false,
                        },
                    }
                }
            };
            (output, confirmation_denied)
        };
        if confirmation_denied {
            anyhow::bail!(
                "extension command {name:?} requires an interactive confirmation surface"
            );
        }
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

    /// Drain completed renderer work without waiting, retaining completion order.
    pub fn drain_background_updates(&mut self) -> ExtensionBackgroundUpdates {
        let mut updates = ExtensionBackgroundUpdates::default();
        while let Ok(update) = self.background_rx.try_recv() {
            match update {
                ExtensionBackgroundUpdate::Renderer { update, diagnostic } => {
                    self.diagnostics.extend(diagnostic);
                    updates.rendered_tools.extend(update);
                }
            }
        }
        updates
    }

    fn cancel_background_work(&mut self) {
        for task in self.renderer_tasks.drain(..) {
            task.abort();
        }
        for task in self.confirmation_tasks.drain(..) {
            task.abort();
        }
        for task in self.input_tasks.drain(..) {
            task.abort();
        }
        for task in self.policy_supervisors.drain(..) {
            task.abort();
        }
        self.confirmation_denials.clear();
        self.input_cancellations.clear();
        while self.background_rx.try_recv().is_ok() {}
    }

    /// Gracefully stop every extension. Each protocol shutdown has its own
    /// hard timeout in `ExtensionProcess`; the outer timeout prevents a
    /// broken extension from delaying terminal restoration indefinitely.
    pub async fn shutdown(&mut self) {
        self.cancel_background_work();
        if self.session_lifecycle_started {
            if let Some(session_id) = self.session_id.clone() {
                let diagnostics = notify_lifecycle_all(
                    &self.processes,
                    ExtensionLifecycleEvent::SessionSettled {
                        session_id,
                        run_id: None,
                        outcome: self
                            .last_lifecycle_outcome
                            .unwrap_or(ExtensionLifecycleOutcome::Shutdown),
                        duration_ms: duration_millis(self.session_started_at.elapsed()),
                        reason: None,
                    },
                )
                .await;
                self.diagnostics.extend(diagnostics);
            }
            self.session_lifecycle_started = false;
        }
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
        let results = futures_util::future::join_all(reloads).await;
        let mut messages = Vec::with_capacity(results.len());
        for (name, result) in results {
            match result {
                Ok(report) => {
                    messages.push(format!(
                        "reloaded {name} (generation {}, previous shutdown {})",
                        report.generation,
                        if report.previous_shutdown_graceful {
                            "clean"
                        } else {
                            "forced"
                        }
                    ));
                }
                Err(error) => messages.push(format!("unable to reload {name}: {error}")),
            }
        }
        self.start_policy_supervisors();
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

    fn schedule_input_cancellations(&mut self) {
        self.input_tasks.retain(|task| !task.is_finished());
        if tokio::runtime::Handle::try_current().is_err() {
            if !self.input_cancellations.is_empty() {
                self.diagnostics
                    .push("warning: extension input cancellation requires the Ygg Tokio runtime");
            }
            return;
        }
        while self.input_tasks.len() < INPUT_CANCELLATION_CONCURRENCY {
            let Some(pending) = self.input_cancellations.pop_front() else {
                break;
            };
            self.input_tasks.push(tokio::spawn(async move {
                let _ = pending
                    .process
                    .respond_to_input(
                        pending.request_id,
                        pending.generation,
                        ExtensionInputResponse { value: None },
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

    fn queue_input_cancellation(&mut self, pending: PendingInputCancellation) {
        if self.input_cancellations.len() >= INPUT_CANCELLATION_QUEUE_CAPACITY {
            self.diagnostics.push(format!(
                "warning: extension input cancellation queue reached its {INPUT_CANCELLATION_QUEUE_CAPACITY}-request limit; newest request was dropped"
            ));
            return;
        }
        self.input_cancellations.push_back(pending);
        self.schedule_input_cancellations();
    }

    /// Drain a fixed amount of extension work without letting a continuously
    /// ready process monopolize the input/render task. The start receiver
    /// rotates between calls and each receiver has a smaller per-call quota.
    pub fn drain_events(&mut self) -> Vec<String> {
        self.schedule_confirmation_denials();
        self.schedule_input_cancellations();
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
                    Ok(ExtensionEvent::StatusContributed { .. }) => {
                        // Header/status/footer contributions remain available as
                        // protocol data, but the coding TUI never turns them
                        // into ambient transcript or composer chrome.
                    }
                    Ok(ExtensionEvent::PresentationUpdated {
                        generation,
                        resource_owner,
                        snapshot,
                    }) => {
                        let published_owner = match admit_presentation_owner(
                            resource_owner,
                            self.resource_owner.as_deref(),
                        ) {
                            Ok(owner) => owner,
                            Err(error) => {
                                self.diagnostics.push(format!("warning: {name}: {error}"));
                                remaining -= 1;
                                receiver_budget -= 1;
                                continue;
                            }
                        };
                        let active_generation = process
                            .as_ref()
                            .map(ExtensionProcess::health_snapshot)
                            .map(|health| health.generation);
                        let extension_instance_id = process
                            .as_ref()
                            .expect("extension event receivers align with processes")
                            .extension_instance_id()
                            .to_owned();
                        if let Err(error) = reduce_presentation_update(
                            &mut self.presentations,
                            name.clone(),
                            active_generation,
                            extension_instance_id,
                            published_owner,
                            generation,
                            snapshot,
                        ) {
                            self.diagnostics.push(format!("warning: {name}: {error}"));
                        }
                    }
                    Ok(ExtensionEvent::Diagnostic { message }) => {
                        self.diagnostics.push(format!("warning: {name}: {message}"));
                    }
                    Ok(ExtensionEvent::ConfirmationRequested {
                        request_id,
                        generation,
                        request,
                        ..
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
                    Ok(ExtensionEvent::PolicyEvaluationRequested { intent, .. }) => {
                        messages.push(format!(
                            "[{name}] policy intent denied (no host-managed adapter): {}",
                            intent.operation
                        ));
                    }
                    Ok(ExtensionEvent::InputRequested {
                        request_id,
                        generation,
                        request,
                        ..
                    }) => {
                        if process
                            .as_ref()
                            .is_some_and(|process| process.input_answered(&request_id, generation))
                        {
                            remaining -= 1;
                            receiver_budget -= 1;
                            continue;
                        }
                        messages.push(format!(
                            "[{name}] input cancelled (no active input owner): {}",
                            request.prompt
                        ));
                        if let Some(process) = process.clone() {
                            self.queue_input_cancellation(PendingInputCancellation {
                                process,
                                request_id,
                                generation,
                            });
                        } else {
                            self.diagnostics.push(format!(
                                "warning: {name}: input could not be cancelled because its process is unavailable"
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
        let active_owner = self.resource_owner.as_deref();
        self.presentations.retain(|name, view| {
            (view.resource_owner.is_none() || view.resource_owner.as_deref() == active_owner)
                && self.processes.iter().any(|process| {
                    process.descriptor().manifest.name == *name
                        && process.is_running()
                        && process.health_snapshot().generation == view.generation
                })
        });
        self.schedule_confirmation_denials();
        self.schedule_input_cancellations();
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

async fn notify_lifecycle_all(
    processes: &[ExtensionProcess],
    event: ExtensionLifecycleEvent,
) -> Vec<String> {
    futures_util::future::join_all(processes.iter().map(|process| {
        let process = process.clone();
        let event = event.clone();
        async move {
            match tokio::time::timeout(
                LIFECYCLE_NOTIFY_DEADLINE,
                process.notify_lifecycle(&event),
            )
            .await
            {
                Err(_) => Some(format!(
                    "warning: extension {:?} lifecycle notification exceeded {LIFECYCLE_NOTIFY_DEADLINE:?}",
                    process.descriptor().manifest.name
                )),
                Ok(Err(error)) => Some(format!(
                    "warning: extension {:?} lifecycle notification failed: {error}",
                    process.descriptor().manifest.name
                )),
                Ok(Ok(())) => None,
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect()
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn clip_lifecycle_reason(reason: &str, limit: usize) -> String {
    if reason.len() <= limit {
        return reason.to_owned();
    }
    let marker = "[… truncated …]";
    let mut end = limit.saturating_sub(marker.len());
    while !reason.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{marker}", &reason[..end])
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

fn format_presentation_views(views: &[ExtensionPresentationView]) -> String {
    let mut lines = Vec::new();
    for view in views {
        lines.push(format!(
            "[{}] generation {} · revision {}",
            view.extension, view.generation, view.snapshot.revision
        ));
        if let Some(status) = &view.snapshot.status {
            let state = format!("{:?}", status.state).to_lowercase();
            lines.push(format!("  status: {state} · {}", status.label));
            if let Some(detail) = &status.detail {
                lines.push(format!("    {detail}"));
            }
        }
        for activity in &view.snapshot.activities {
            let state = format!("{:?}", activity.state).to_lowercase();
            let provenance = activity
                .provenance
                .as_deref()
                .map(|value| format!(" · {value}"))
                .unwrap_or_default();
            lines.push(format!(
                "  activity {}: {state} · {}{provenance}",
                activity.kind, activity.summary
            ));
            lines.extend(
                activity
                    .references
                    .iter()
                    .map(|reference| format_presentation_reference("    ", reference)),
            );
        }
        if let Some(collection) = &view.snapshot.collection {
            lines.push(format!("  {}:", collection.title));
            let parents = collection
                .nodes
                .iter()
                .map(|node| (node.id.as_str(), node.parent_id.as_deref()))
                .collect::<BTreeMap<_, _>>();
            for node in &collection.nodes {
                let mut depth = 0usize;
                let mut parent = node.parent_id.as_deref();
                while let Some(id) = parent {
                    depth = depth.saturating_add(1);
                    parent = parents.get(id).copied().flatten();
                }
                let state = format!("{:?}", node.state).to_lowercase();
                let secondary = node
                    .secondary
                    .as_deref()
                    .map(|value| format!(" · {value}"))
                    .unwrap_or_default();
                lines.push(format!(
                    "    {}- {} · {state}{secondary}",
                    "  ".repeat(depth),
                    node.label
                ));
                lines.extend(
                    node.references
                        .iter()
                        .map(|reference| format_presentation_reference("      ", reference)),
                );
            }
            if let Some(detail) = &collection.detail {
                lines.push(format!("  detail: {}", detail.title));
                lines.extend(detail.body.lines().map(|line| format!("    {line}")));
                lines.extend(
                    detail
                        .references
                        .iter()
                        .map(|reference| format_presentation_reference("    ", reference)),
                );
            }
        }
        for action in &view.snapshot.actions {
            lines.push(format!(
                "  action {}: /{}{}{}",
                action.label,
                action.command,
                if action.arguments.is_empty() { "" } else { " " },
                action.arguments.join(" ")
            ));
        }
    }
    lines.join("\n")
}

fn format_presentation_reference(
    indent: &str,
    reference: &ygg_agent::ExtensionPresentationReference,
) -> String {
    let kind = match reference.kind {
        ygg_agent::ExtensionPresentationReferenceKind::Session => "session",
        ygg_agent::ExtensionPresentationReferenceKind::Artifact => "artifact",
        ygg_agent::ExtensionPresentationReferenceKind::Resource => "resource",
        ygg_agent::ExtensionPresentationReferenceKind::Url => "source",
    };
    let label = reference
        .label
        .as_deref()
        .map(|label| format!("{label} · "))
        .unwrap_or_default();
    let value = format!("{indent}{kind}: {label}{}", reference.id);
    if reference.kind == ygg_agent::ExtensionPresentationReferenceKind::Session {
        format!(
            "{value}\n{indent}  inspect: /extensions inspect {}",
            reference.id
        )
    } else {
        value
    }
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

fn extension_execution_context(
    process: &ExtensionProcess,
    resource_owner: Option<&str>,
) -> ygg_agent::extension_process::ExtensionExecutionContext {
    resource_owner.map_or_else(
        || process.current_context(),
        |owner| process.current_context_for_resource_owner(owner.to_owned()),
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

    #[test]
    fn first_party_status_marks_missing_delegation_telemetry_incompatible() {
        let (schema, compatibility) = extension_compatibility(
            SUBAGENTS_EXTENSION_NAME,
            true,
            &[EXTENSION_FEATURE_AGENT_SESSIONS.to_owned()],
            None,
        );
        assert_eq!(schema, None);
        assert!(compatibility.contains("rebuild/reinstall"));

        let (schema, compatibility) = extension_compatibility(
            SUBAGENTS_EXTENSION_NAME,
            true,
            &[
                EXTENSION_FEATURE_AGENT_SESSIONS.to_owned(),
                EXTENSION_FEATURE_DELEGATION_TELEMETRY.to_owned(),
            ],
            None,
        );
        assert_eq!(schema.as_deref(), Some(DELEGATION_TELEMETRY_SCHEMA));
        assert_eq!(compatibility, "compatible");
    }

    #[test]
    fn status_contributions_never_become_ambient_messages() {
        let (sender, receiver) = broadcast::channel(4);
        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(receiver);
        sender
            .send(ExtensionEvent::StatusContributed {
                contribution: ygg_agent::extension_process::ExtensionStatusContribution {
                    surface: ExtensionUiSurface::Footer,
                    text: "persistent extension status".into(),
                    style_role: None,
                    priority: 0,
                },
            })
            .unwrap();

        assert!(extensions.drain_events().is_empty());
    }

    #[test]
    fn semantic_presentation_reducer_fences_revision_generation_and_owner() {
        let owner = ygg_agent::extension_process::ExtensionResourceOwner {
            session_id: "owner-a".into(),
            extension_instance_id: "instance-a".into(),
            process_generation: 3,
        };
        assert_eq!(
            admit_presentation_owner(Some(owner.clone()), Some("owner-a")).unwrap(),
            Some("owner-a".into())
        );
        assert!(admit_presentation_owner(Some(owner), Some("owner-b"))
            .unwrap_err()
            .contains("another resource owner"));
        assert_eq!(
            admit_presentation_owner(None, Some("owner-a")).unwrap(),
            None
        );

        let snapshot: ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap();
        snapshot.validate(&["workers".into()]).unwrap();
        let mut presentations = BTreeMap::new();
        assert_eq!(
            reduce_presentation_update(
                &mut presentations,
                "fixture-extension".into(),
                Some(3),
                "instance-a".into(),
                Some("owner-a".into()),
                3,
                snapshot.clone(),
            )
            .unwrap(),
            "1 worker"
        );
        let rendered =
            format_presentation_views(&presentations.values().cloned().collect::<Vec<_>>());
        assert!(rendered.contains("Reviewing tests"));
        assert!(rendered.contains("https://example.com/docs/extensions"));
        assert!(rendered.contains("session: Worker transcript · session-worker-1"));

        let mut process_scoped = snapshot.clone();
        process_scoped.revision += 1;
        assert!(reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(3),
            "instance-a".into(),
            None,
            3,
            process_scoped,
        )
        .unwrap_err()
        .contains("owner-scoped state is active"));
        assert_eq!(
            presentations["fixture-extension"].resource_owner.as_deref(),
            Some("owner-a")
        );

        assert!(reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(3),
            "instance-a".into(),
            Some("owner-a".into()),
            3,
            snapshot.clone(),
        )
        .unwrap_err()
        .contains("stale semantic presentation revision"));

        let mut restarted = snapshot;
        restarted.revision = 0;
        reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(4),
            "instance-a".into(),
            Some("owner-a".into()),
            4,
            restarted,
        )
        .unwrap();
        assert_eq!(presentations["fixture-extension"].generation, 4);
        assert_eq!(presentations["fixture-extension"].snapshot.revision, 0);

        let owner_reset: ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap();
        reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(4),
            "instance-a".into(),
            Some("owner-b".into()),
            4,
            owner_reset,
        )
        .unwrap();
        assert_eq!(
            presentations["fixture-extension"].resource_owner.as_deref(),
            Some("owner-b")
        );

        let error = reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(4),
            "instance-a".into(),
            Some("owner-b".into()),
            3,
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap(),
        )
        .unwrap_err();
        assert!(error.contains("stale generation"));

        let mut replacement: ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap();
        replacement.revision = 0;
        reduce_presentation_update(
            &mut presentations,
            "fixture-extension".into(),
            Some(4),
            "instance-b".into(),
            Some("owner-b".into()),
            4,
            replacement,
        )
        .unwrap();
        assert_eq!(
            presentations["fixture-extension"].extension_instance_id,
            "instance-b"
        );
        assert_eq!(presentations["fixture-extension"].snapshot.revision, 0);
    }

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

    #[cfg(unix)]
    fn executable_extension_config(workspace: &Path, extension_root: &Path, name: &str) -> Config {
        Config {
            workspace: workspace.to_owned(),
            invocation_cwd: workspace.to_owned(),
            model: Some(ygg_ai::ModelId("gpt-4o-mini".into())),
            model_explicit: false,
            reasoning: ReasoningConfig::Off,
            reasoning_explicit: false,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
            reasoning_mode_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            effect_policy: ygg_agent::EffectPolicy::Controlled,
            sandbox: crate::config::SandboxPolicy {
                allow_external_paths: false,
                ..crate::config::SandboxPolicy::default()
            },
            theme: None,
            system_prompt: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: workspace.join("sessions"),
            compaction: crate::config::CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(1),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: crate::config::Mode::Interactive,
            resume: crate::config::ResumeSelector::New,
            skill_paths: vec![],
            extension_paths: vec![extension_root.to_owned()],
            enabled_extensions: vec![name.to_owned()],
            extension_activation_overridden: false,
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![name.to_owned()],
            tools: crate::config::ToolPolicy::default(),
            context_files: false,
            offline: true,
            workspace_trusted: false,
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn controlled_policy_never_launches_enabled_trusted_executable_extension() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let extension_root = temp.path().join("extensions");
        let extension_dir = extension_root.join("controlled-launch-probe");
        std::fs::create_dir_all(&extension_dir).unwrap();
        std::fs::write(
            extension_dir.join(EXTENSION_MANIFEST_FILENAME),
            r#"name = "controlled-launch-probe"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "launch-probe.sh"
"#,
        )
        .unwrap();
        let executable = extension_dir.join("launch-probe.sh");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf launched > \"$YGG_WORKSPACE/controlled-extension-launched\"\nexit 1\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let config =
            executable_extension_config(temp.path(), &extension_root, "controlled-launch-probe");
        assert!(config.sandbox.allow_process);
        assert!(config.sandbox.allow_shell);
        let session = Session::create(temp.path().join("session.jsonl")).unwrap();
        let model = ygg_ai::ModelCatalog::builtin()
            .unwrap()
            .resolve(&ygg_ai::ModelId("gpt-4o-mini".into()))
            .unwrap();
        let sessions = SessionStore::new(&config.session_dir, temp.path());
        let mut host = ExtensionHost::new();

        let extensions = ExecutableExtensions::discover_and_start(
            &config,
            &session,
            &model,
            &ReasoningConfig::Off,
            &sessions,
            &mut host,
        );

        assert!(!temp.path().join("controlled-extension-launched").exists());
        assert!(extensions.processes.is_empty());
        assert!(extensions.summaries.iter().any(|extension| {
            extension.name == "controlled-launch-probe"
                && extension.enabled
                && extension.trusted
                && !extension.running
        }));
        assert!(extensions
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic == CONTROLLED_EXTENSION_START_DIAGNOSTIC));
    }

    #[cfg(unix)]
    async fn lifecycle_fixture(temp: &tempfile::TempDir) -> (ExecutableExtensions, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = temp.path().join("lifecycle-fixture.sh");
        let wire_log = temp.path().join("lifecycle-wire.jsonl");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
id=$(printf '%s\n' "$initialize" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.2","tools":[],"commands":[],"protocol":{"version":"0.2","features":["request_cancellation","content_parts","lifecycle_events"],"limits":{"max_concurrent_requests":1},"lifecycle_events":["turn/started","turn/settled"]}}}\n' "$id"
while IFS= read -r request; do
  printf '%s\n' "$request" >> "$YGG_WORKSPACE/lifecycle-wire.jsonl"
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
name = "lifecycle-fixture"
version = "0.2.0"
api_version = "0.2"

[entrypoint]
command = "lifecycle-fixture.sh"
"#,
        )
        .unwrap();
        let mut runtime = ExtensionRuntimeConfig::new(temp.path());
        runtime.host_state.session_id = Some("lifecycle-test-session".into());
        runtime.request_timeout = Duration::from_secs(2);
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
        extensions.processes.push(process);
        extensions.session_id = Some("lifecycle-test-session".into());
        (extensions, wire_log)
    }

    #[cfg(unix)]
    fn recorded_turn_lifecycle(path: &Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|message| {
                matches!(
                    message.get("method").and_then(serde_json::Value::as_str),
                    Some("turn/started" | "turn/settled")
                )
            })
            .collect()
    }

    #[cfg(unix)]
    async fn wait_for_recorded_turn_lifecycle(
        path: &Path,
        minimum: usize,
    ) -> Vec<serde_json::Value> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let events = recorded_turn_lifecycle(path);
                if events.len() >= minimum {
                    return events;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("lifecycle fixture did not receive its terminal boundary")
    }

    #[cfg(unix)]
    fn assert_one_ordered_turn_with_reason(
        events: &[serde_json::Value],
        outcome: &str,
        reason: Option<&str>,
    ) {
        assert_eq!(events.len(), 2, "unexpected lifecycle wire: {events:#?}");
        assert_eq!(events[0]["method"], "turn/started");
        assert_eq!(events[1]["method"], "turn/settled");
        assert_eq!(events[1]["params"]["outcome"], outcome);
        if let Some(reason) = reason {
            assert_eq!(events[1]["params"]["reason"], reason);
        } else {
            assert!(events[1]["params"]["reason"].is_null());
        }
        for id in ["session_id", "run_id", "turn_id"] {
            assert_eq!(
                events[0]["params"][id], events[1]["params"][id],
                "{id} changed across the lifecycle pair"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_begin_turn_during_started_delivery_preserves_ordered_terminal_owner() {
        let temp = tempfile::tempdir().unwrap();
        let (mut extensions, wire_log) = lifecycle_fixture(&temp).await;
        let control = std::sync::Arc::new(LifecycleDeliveryTestControl::default());
        control.gate_turn_started();
        extensions.lifecycle_delivery_test_control = Some(control.clone());

        let mut beginning = Box::pin(extensions.begin_turn());
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                () = control.turn_started_entered() => {}
                _ = &mut beginning => panic!("begin_turn completed while start delivery was gated"),
            }
        })
        .await
        .expect("begin_turn never transferred start delivery to its owned task");

        // Cancelling the caller here drops the already-constructed guard. Its
        // Drop owner must wait for TurnStarted and then emit one TurnSettled.
        drop(beginning);
        control.release_turn_started();
        let events = wait_for_recorded_turn_lifecycle(&wire_log, 2).await;
        assert_one_ordered_turn_with_reason(
            &events,
            "frontend_disconnected",
            Some("turn owner dropped before explicit settlement"),
        );

        extensions.shutdown().await;
        assert_one_ordered_turn_with_reason(
            &recorded_turn_lifecycle(&wire_log),
            "frontend_disconnected",
            Some("turn owner dropped before explicit settlement"),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_settle_future_keeps_owned_terminal_delivery_alive() {
        let temp = tempfile::tempdir().unwrap();
        let (mut extensions, wire_log) = lifecycle_fixture(&temp).await;
        let control = std::sync::Arc::new(LifecycleDeliveryTestControl::default());
        extensions.lifecycle_delivery_test_control = Some(control.clone());
        let turn = extensions.begin_turn().await;

        control.gate_turn_settled();
        let outcome = crate::modes::HostRunOutcome::Failed("fixture failure".into());
        let mut settling = Box::pin(extensions.settle_turn(turn, &outcome));
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                () = control.turn_settled_entered() => {}
                () = &mut settling => panic!("settle_turn completed while terminal delivery was gated"),
            }
        })
        .await
        .expect("settle_turn never transferred terminal delivery to its owned task");

        // The detached notification task, not this cancellable caller future,
        // owns terminal delivery after the boundary above.
        drop(settling);
        control.release_turn_settled();
        let events = wait_for_recorded_turn_lifecycle(&wire_log, 2).await;
        assert_one_ordered_turn_with_reason(&events, "failed", Some("fixture failure"));

        extensions.shutdown().await;
        assert_one_ordered_turn_with_reason(
            &recorded_turn_lifecycle(&wire_log),
            "failed",
            Some("fixture failure"),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn settle_turn_reports_non_completion_outcomes_with_matching_extension_reason() {
        let temp = tempfile::tempdir().unwrap();
        let (mut extensions, wire_log) = lifecycle_fixture(&temp).await;

        let cases = [
            (
                crate::modes::HostRunOutcome::Aborted,
                "cancelled",
                Some("run aborted before completing"),
            ),
            (
                crate::modes::HostRunOutcome::Failed("fixture failure".into()),
                "failed",
                Some("fixture failure"),
            ),
            (
                crate::modes::HostRunOutcome::MaxTurns,
                "limit_reached",
                Some("run hit max turns before completing"),
            ),
            (
                crate::modes::HostRunOutcome::stream_lost(),
                "frontend_disconnected",
                Some(crate::modes::RUN_STREAM_LOST_MESSAGE),
            ),
            (
                crate::modes::HostRunOutcome::Shutdown,
                "shutdown",
                Some(crate::modes::RUN_SHUTDOWN_MESSAGE),
            ),
        ];

        for case in cases.iter() {
            let (outcome, expected_outcome, expected_reason) = case;
            let before = recorded_turn_lifecycle(&wire_log).len();
            let turn = extensions.begin_turn().await;
            extensions.settle_turn(turn, outcome).await;
            let events = wait_for_recorded_turn_lifecycle(&wire_log, before + 2).await;
            assert_one_ordered_turn_with_reason(
                &events[before..],
                expected_outcome,
                *expected_reason,
            );
        }

        extensions.shutdown().await;
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
        input_calls: Vec<(String, String, bool)>,
        input_value: Option<String>,
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

        fn input<'a>(
            &'a mut self,
            extension: &'a str,
            request: &'a ExtensionInputRequest,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<String>>> + 'a>> {
            Box::pin(async move {
                self.input_calls.push((
                    extension.to_owned(),
                    request.prompt.clone(),
                    request.secret,
                ));
                Ok(self.input_value.take())
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

    #[cfg(unix)]
    #[tokio::test]
    async fn prompt_hooks_use_the_production_wire_payloads() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("prompt-hooks.jsonl");
        let fixture = temp.path().join("prompt-hooks.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
set -eu
log=$1

read_request() {
  IFS= read -r request
  printf '%s\n' "$request" >> "$log"
}

request_id() {
  printf '%s\n' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'
}

read_request
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[]}}\n' "$id"

read_request
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"

read_request
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"

read_request
id=$(request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let mut manifest = ExtensionManifest::parse(
            r#"
name = "prompt-hooks"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "prompt-hooks.sh"

[contributes]
hooks = ["before_prompt", "after_response"]
"#,
        )
        .unwrap();
        manifest.entrypoint.args = vec![log_path.to_string_lossy().into_owned()];
        let manifest_path = temp.path().join(EXTENSION_MANIFEST_FILENAME);
        let mut runtime = ExtensionRuntimeConfig::new(temp.path());
        runtime.request_timeout = Duration::from_secs(2);
        runtime.shutdown_timeout = Duration::from_secs(2);
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path,
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
        let composition = extensions
            .compose_prompt("system", "Explain the contract".into())
            .await
            .unwrap();
        assert_eq!(composition.system, "system");
        assert_eq!(composition.prompt, "Explain the contract");
        assert!(composition.notifications.is_empty());
        assert!(extensions
            .after_response("The contract is bounded.")
            .await
            .is_empty());
        extensions.shutdown().await;
        assert!(!process.is_running());

        let frames = std::fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 4, "unexpected wire transcript: {frames:#?}");
        let context = serde_json::json!({
            "workspace": temp.path(),
            "execution_scope": null,
            "host": {
                "session_id": null,
                "session_name": null,
                "model": null,
                "reasoning": null,
                "active_skills": [],
            },
        });
        assert_eq!(
            frames[1],
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "hook/run",
                "params": {
                    "hook": "before_prompt",
                    "payload": {"prompt": "Explain the contract"},
                    "context": context,
                },
            })
        );
        assert_eq!(
            frames[2],
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "hook/run",
                "params": {
                    "hook": "after_response",
                    "payload": {"response": "The contract is bounded."},
                    "context": context,
                },
            })
        );
        assert_eq!(
            frames[3],
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "shutdown",
                "params": {},
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_0_2_prompt_hooks_receive_owner_and_success_response_content() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let log_path = temp.path().join("hook-v02.jsonl");
        let fixture = temp.path().join("hook-v02.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
log=$1
request_id() { sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
read_request() { IFS= read -r line || exit 91; printf '%s\n' "$line" >> "$log"; }
read_request
id=$(printf '%s' "$line" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.2","tools":[],"commands":[],"protocol":{"version":"0.2","features":["request_cancellation","content_parts"],"limits":{"max_concurrent_requests":1}}}}\n' "$id"
read_request
id=$(printf '%s' "$line" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"
read_request
id=$(printf '%s' "$line" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"disposition":{"action":"continue"},"context":[],"notifications":[]}}\n' "$id"
read_request
id=$(printf '%s' "$line" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();
        let mut manifest = ExtensionManifest::parse(
            r#"
name = "hook-v02"
version = "0.1.0"
api_version = "0.2"
[entrypoint]
command = "hook-v02.sh"
[contributes]
hooks = ["before_prompt", "after_response"]
"#,
        )
        .unwrap();
        manifest.entrypoint.args = vec![log_path.to_string_lossy().into_owned()];
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join(EXTENSION_MANIFEST_FILENAME),
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
        let mut extensions = ExecutableExtensions::default();
        extensions.resource_owner = Some("owner-v02".into());
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        extensions
            .compose_prompt("system", "user text".into())
            .await
            .unwrap();
        extensions.after_response("assistant text").await;
        extensions.shutdown().await;

        let frames = std::fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames[1]["params"]["hook"], "before_prompt");
        assert_eq!(frames[1]["params"]["payload"]["prompt"], "user text");
        assert_eq!(frames[2]["params"]["hook"], "after_response");
        assert_eq!(frames[2]["params"]["payload"]["response"], "assistant text");
        for frame in &frames[1..=2] {
            let owner = &frame["params"]["context"]["resource_owner"];
            assert_eq!(owner["session_id"], "owner-v02");
            assert_eq!(owner["process_generation"], 1);
            assert!(owner["extension_instance_id"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn api_0_2_command_parent_binds_owner_for_reverse_services() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("command-owner-v02.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
request_id() { sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
IFS= read -r initialize
id=$(printf '%s' "$initialize" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.2","tools":[],"commands":[{"name":"owned","description":"Exercise an owner-bound reverse service","usage":"/owned"}],"protocol":{"version":"0.2","features":["request_cancellation","content_parts","artifacts"],"limits":{"max_concurrent_requests":1}}}}\n' "$id"
IFS= read -r command
parent=$(printf '%s' "$command" | request_id)
printf '{"jsonrpc":"2.0","id":"artifact-child","method":"artifact/publish","params":{"parent_request_id":%s,"mime_type":"image/png","size":21,"sha256":"8c423f0980ff76637fe84cd6f9f8c63922b665c90e5608f37d995dca965f5fee","data":{"encoding":"base64","data":"iVBORw0KGgpvd25lci1maXh0dXJl"}}}\n' "$parent"
IFS= read -r artifact
case "$artifact" in
  *'"id":"artifact-child"'*'"artifact_id"'*) ;;
  *) exit 42 ;;
esac
printf '{"jsonrpc":"2.0","id":%s,"result":{"text":"owner-bound artifact published","notifications":[],"context":[]}}\n' "$parent"
IFS= read -r shutdown
id=$(printf '%s' "$shutdown" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();
        let manifest = ExtensionManifest::parse(
            r#"
name = "command-owner-v02"
version = "0.1.0"
api_version = "0.2"
[entrypoint]
command = "command-owner-v02.sh"
[contributes]
commands = ["owned"]
"#,
        )
        .unwrap();
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join(EXTENSION_MANIFEST_FILENAME),
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
        let mut extensions = ExecutableExtensions::default();
        extensions.resource_owner = Some("owner-command-v02".into());
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        let mut confirmations = RecordingConfirmationHandler::default();
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            extensions.execute_command_with_confirmation("owned", Vec::new(), &mut confirmations),
        )
        .await
        .expect("owner-bound artifact publication timed out")
        .unwrap()
        .unwrap();
        assert_eq!(output, "owner-bound artifact published");
        assert!(confirmations.calls.is_empty());
        assert!(process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn presentation_action_routes_duplicate_command_name_to_owning_process() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("duplicate-command.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
label=$1
request_id() { sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
IFS= read -r initialize
id=$(printf '%s' "$initialize" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[{"name":"shared","description":"Shared fixture command","usage":"/shared"}]}}\n' "$id"
IFS= read -r command
id=$(printf '%s' "$command" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"text":"%s","notifications":[],"context":[]}}\n' "$id" "$label"
IFS= read -r shutdown
id=$(printf '%s' "$shutdown" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let mut processes = Vec::new();
        for name in ["first-owner", "second-owner"] {
            let mut manifest = ExtensionManifest::parse(&format!(
                r#"
name = {name:?}
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "duplicate-command.sh"
[contributes]
commands = ["shared"]
"#
            ))
            .unwrap();
            manifest.entrypoint.args = vec![name.into()];
            processes.push(
                ExtensionProcess::start(
                    DiscoveredExtension {
                        manifest,
                        manifest_path: temp.path().join(format!("{name}.toml")),
                        source: ExtensionSource::Explicit,
                        activation: ygg_agent::extension_process::ExtensionActivation {
                            enabled: true,
                            trust: ExtensionTrust::Trusted,
                        },
                    },
                    ExtensionRuntimeConfig::new(temp.path()),
                )
                .await
                .unwrap(),
            );
        }

        let mut snapshot: ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap();
        snapshot.actions.truncate(1);
        snapshot.actions[0].id = "route".into();
        snapshot.actions[0].command = "shared".into();
        snapshot.actions[0].arguments.clear();
        snapshot.actions[0].destructive = false;
        let generation = processes[1].health_snapshot().generation;
        let extension_instance_id = processes[1].extension_instance_id().to_owned();
        let mut extensions = ExecutableExtensions::default();
        extensions.processes = processes;
        extensions.presentations.insert(
            "second-owner".into(),
            ExtensionPresentationView {
                extension: "second-owner".into(),
                generation,
                extension_instance_id,
                resource_owner: None,
                snapshot,
            },
        );
        let mut confirmations = RecordingConfirmationHandler::default();
        let output = extensions
            .execute_presentation_action_with_confirmation(
                "second-owner",
                "route",
                &mut confirmations,
            )
            .await
            .unwrap();
        assert_eq!(output, "second-owner");
        assert!(confirmations.calls.is_empty());
        extensions.shutdown().await;
    }

    #[cfg(all(unix, feature = "serve"))]
    #[tokio::test]
    async fn confirmed_presentation_action_funds_one_command_requested_confirmation() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("serve-confirmation-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
request_id() { sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p'; }
IFS= read -r initialize
id=$(printf '%s' "$initialize" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{"api_version":"0.1","tools":[],"commands":[{"name":"setup","description":"Confirmed setup","usage":"/setup"}]}}\n' "$id"
count=0
while [ "$count" -lt 2 ]; do
  IFS= read -r command
  id=$(printf '%s' "$command" | request_id)
  confirmation_id="setup-confirmation-$count"
  printf '{"jsonrpc":"2.0","id":"%s","method":"confirmation/request","params":{"prompt":"Install runtime?","destructive":false,"default":false}}\n' "$confirmation_id"
  IFS= read -r confirmation
  case "$confirmation" in
    *\"id\":\"$confirmation_id\"*'"confirmed":true'*) ;;
    *) exit 41 ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":{"text":"setup started","notifications":[],"context":[]}}\n' "$id"
  count=$((count + 1))
done
IFS= read -r shutdown
id=$(printf '%s' "$shutdown" | request_id)
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();
        let manifest = ExtensionManifest::parse(
            r#"
name = "serve-confirmation-fixture"
version = "0.1.0"
api_version = "0.1"
[entrypoint]
command = "serve-confirmation-fixture.sh"
[contributes]
commands = ["setup"]
confirmations = true
"#,
        )
        .unwrap();
        let process = ExtensionProcess::start(
            DiscoveredExtension {
                manifest,
                manifest_path: temp.path().join(EXTENSION_MANIFEST_FILENAME),
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
        let mut snapshot: ExtensionPresentationSnapshot =
            serde_json::from_str(include_str!("../fixtures/extension-presentation.json")).unwrap();
        snapshot.actions.truncate(1);
        snapshot.actions[0].id = "setup".into();
        snapshot.actions[0].label = "Set up browser".into();
        snapshot.actions[0].command = "setup".into();
        snapshot.actions[0].arguments.clear();
        snapshot.actions[0].destructive = true;
        let generation = process.health_snapshot().generation;
        let extension_instance_id = process.extension_instance_id().to_owned();
        let revision = snapshot.revision;
        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        extensions.presentations.insert(
            "serve-confirmation-fixture".into(),
            ExtensionPresentationView {
                extension: "serve-confirmation-fixture".into(),
                generation,
                extension_instance_id: extension_instance_id.clone(),
                resource_owner: None,
                snapshot,
            },
        );

        let mut confirmations = RecordingConfirmationHandler::default();
        let interactive = extensions
            .execute_presentation_action_with_confirmation(
                "serve-confirmation-fixture",
                "setup",
                &mut confirmations,
            )
            .await
            .unwrap();
        assert_eq!(interactive, "setup started");
        assert_eq!(
            confirmations.calls,
            vec![(
                "serve-confirmation-fixture".to_owned(),
                "Run Set up browser?".to_owned(),
            )]
        );

        let denied = extensions
            .execute_presentation_action_for_serve(
                "serve-confirmation-fixture",
                &extension_instance_id,
                generation,
                revision,
                "setup",
                false,
            )
            .await
            .unwrap_err();
        assert!(denied
            .to_string()
            .contains("requires explicit confirmation"));
        let output = extensions
            .execute_presentation_action_for_serve(
                "serve-confirmation-fixture",
                &extension_instance_id,
                generation,
                revision,
                "setup",
                true,
            )
            .await
            .unwrap();
        assert_eq!(output, "setup started");
        assert!(process.shutdown().await);
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
    async fn command_input_is_routed_through_the_interactive_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("input-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.2","tools":[],"commands":[{"name":"configure","description":"Collect one secret","usage":"/configure"}],"protocol":{"version":"0.2","features":["request_cancellation","content_parts"],"limits":{"max_concurrent_requests":1}}}}'
IFS= read -r command
id=$(printf '%s\n' "$command" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":"fixture-input","method":"input/request","params":{"parent_request_id":%s,"prompt":"API key:","secret":true}}\n' "$id"
IFS= read -r answer
case "$answer" in
  *'fixture-secret'*) ;;
  *) exit 42 ;;
esac
printf '{"jsonrpc":"2.0","id":%s,"result":{"text":"configured","notifications":[],"context":[]}}\n' "$id"
IFS= read -r shutdown
id=$(printf '%s\n' "$shutdown" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fixture).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fixture, permissions).unwrap();

        let manifest = ExtensionManifest::parse(
            r#"
name = "input-fixture"
version = "0.1.0"
api_version = "0.2"

[entrypoint]
command = "input-fixture.sh"

[contributes]
commands = ["configure"]
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

        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        let mut interaction = RecordingConfirmationHandler {
            input_value: Some("fixture-secret".to_owned()),
            ..RecordingConfirmationHandler::default()
        };
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            extensions.execute_command_with_confirmation("configure", Vec::new(), &mut interaction),
        )
        .await
        .expect("command remained blocked waiting for input")
        .unwrap()
        .expect("fixture command was not registered");

        assert_eq!(output, "configured");
        assert_eq!(
            interaction.input_calls,
            vec![("input-fixture".to_owned(), "API key:".to_owned(), true)]
        );
        assert!(process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn noninteractive_command_confirmation_is_denied() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("noninteractive-confirmation-fixture.sh");
        std::fs::write(
            &fixture,
            r#"#!/bin/sh
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"api_version":"0.1","tools":[],"commands":[{"name":"guarded","description":"Wait for an explicit confirmation","usage":"/guarded"}]}}'
IFS= read -r command
printf '%s\n' '{"jsonrpc":"2.0","id":"fixture-confirmation","method":"confirmation/request","params":{"prompt":"Allow fixture command?","detail":"The fixture will not finish until Ygg answers.","destructive":false,"default":false}}'
IFS= read -r confirmation
case "$confirmation" in
  *'"id":"fixture-confirmation"'*'"confirmed":false'*) ;;
  *) exit 41 ;;
esac
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"text":"fixture denied","notifications":[],"context":[]}}'
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
name = "noninteractive-confirmation-fixture"
version = "0.1.0"
api_version = "0.1"

[entrypoint]
command = "noninteractive-confirmation-fixture.sh"

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

        let mut extensions = ExecutableExtensions::default();
        extensions.receivers.push(process.subscribe());
        extensions.processes.push(process.clone());
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            extensions.execute_command_without_confirmation("guarded", Vec::new()),
        )
        .await
        .expect("command remained blocked waiting for confirmation")
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("requires an interactive confirmation surface"));
        assert!(process.shutdown().await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hung_renderer_rpc_never_blocks_the_interactive_boundary() {
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
        assert!(updates.rendered_tools.is_empty());
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

        use ygg_agent::{Agent, AgentConfig, EffectBroker, ExtensionHost, SandboxConfig};
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
            effect_broker: EffectBroker::default(),
            extensions: ExtensionHost::new(),
            max_turns: None,
            reasoning: ReasoningConfig::Off,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
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
