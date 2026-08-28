//! The extension spine: one trait, one registration boundary, two hooks.
//!
//! Deliberately small. There are no context providers, commands, UI widgets,
//! service containers, or lifecycle callbacks — those can be added as new
//! `ExtensionHost` methods later without breaking the [`Extension`] trait.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use crate::events::AgentEvent;
use crate::input::UserInput;
use crate::tool::{Tool, ToolContext, ToolError};
use tokio::sync::Notify;
use ygg_ai::{Model, ToolDef};

type ToolPolicy = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Implemented by any extension the agent loads.
///
/// The core tools register through this same boundary (see
/// [`CoreTools`](crate::tools::CoreTools)); extensions are first-class from
/// day one.
pub trait Extension: Send + Sync {
    /// Registers the extension's tools and observers with the host.
    fn register(&self, host: &mut ExtensionHost);
}

/// Observes agent events without modifying them.
///
/// Observers are invoked synchronously, in registration order, immediately
/// before each event is delivered to the run's consumer.
pub trait EventObserver: Send + Sync {
    /// Called immediately after a user prompt is durably appended and before
    /// the first provider turn. Implementations should hash or summarize the
    /// input rather than retaining its contents when collecting diagnostics.
    ///
    /// This lifecycle hook is intentionally separate from [`on_event`]: it
    /// gives optional telemetry a stable run identity and model binding without
    /// adding a user-visible event to the frontend stream.
    fn on_run_started_for_owner(
        &self,
        _run_id: &str,
        _input: &UserInput,
        _model: &Model,
        _resource_owner: &str,
    ) {
    }

    /// Called for every [`AgentEvent`] the run emits.
    fn on_event(&self, event: &AgentEvent);

    /// Called with the host-derived durable resource owner of the Agent that
    /// emitted the event. Implementations that do not own session-scoped
    /// state retain the original unscoped callback by default.
    fn on_event_for_owner(&self, event: &AgentEvent, _resource_owner: &str) {
        self.on_event(event);
    }
}

/// Typed interception point around every broker-admitted, successfully resolved
/// tool call. Hooks receive semantic values only and cannot reach private agent
/// state. They are observers and secondary deny inputs, not the authority
/// boundary: the deterministic effect broker always runs first.
#[async_trait::async_trait]
pub trait ToolCallHook: Send + Sync {
    /// Runs after argument validation and effect admission, before the tool receives control.
    /// Returning an error denies the call and produces a normal tool error for
    /// the model; no side effect has occurred at this boundary.
    async fn before_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        context: &ToolContext<'_>,
    ) -> Result<(), ToolError>;

    /// Runs after the tool has resolved. Failures here are diagnostic only:
    /// an observer cannot erase or relabel an already completed side effect.
    async fn after_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        output: &str,
        is_error: bool,
        context: &ToolContext<'_>,
    );
}

#[derive(Default)]
struct DynamicToolRegistry {
    static_names: BTreeSet<String>,
    groups: Vec<DynamicToolGroup>,
    reservations: Vec<DynamicToolReservationEntry>,
    next_reservation: u64,
    ready: bool,
    ready_changed: Arc<Notify>,
    policy: Option<ToolPolicy>,
    revision: u64,
}

struct DynamicToolGroup {
    owner: String,
    tools: Vec<Arc<dyn Tool>>,
}

struct DynamicToolReservationEntry {
    id: u64,
    owner: String,
    names: BTreeSet<String>,
}

/// An extension-owned live tool registration.
///
/// Replacements are transactional: a conflicting catalog leaves the
/// previously published tools untouched, while policy-denied entries are
/// omitted from both publication and the acknowledged active catalog. Clones
/// share the same host registry, so an already-constructed
/// [`Agent`](crate::Agent) observes accepted changes at its next model-turn
/// boundary.
#[derive(Clone)]
pub(crate) struct DynamicToolRegistration {
    owner: String,
    registry: Weak<RwLock<DynamicToolRegistry>>,
}

/// An unpublished catalog replacement whose names are reserved against other
/// extension owners. This lets an async process reload finish its drain and
/// lifecycle cutover before one synchronous, infallible publication step.
pub(crate) struct DynamicToolReservation {
    id: u64,
    owner: String,
    registry: Weak<RwLock<DynamicToolRegistry>>,
    tools: Option<Vec<Arc<dyn Tool>>>,
}

impl DynamicToolRegistration {
    pub(crate) async fn wait_until_ready(&self, timeout: Duration) -> Result<(), String> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| "dynamic tool host is no longer available".to_owned())?;
        let ready_changed = Arc::clone(
            &registry
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .ready_changed,
        );
        let notified = ready_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ready
        {
            return Ok(());
        }
        tokio::time::timeout(timeout, notified)
            .await
            .map_err(|_| "dynamic tool host did not finish registration in time".to_owned())?;
        let ready = registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ready;
        if ready {
            Ok(())
        } else {
            Err("dynamic tool host is not ready".to_owned())
        }
    }

    #[cfg(test)]
    pub(crate) fn published_names(&self) -> BTreeSet<String> {
        let Some(registry) = self.registry.upgrade() else {
            return BTreeSet::new();
        };
        let registry = registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .groups
            .iter()
            .find(|group| group.owner == self.owner)
            .into_iter()
            .flat_map(|group| &group.tools)
            .map(|tool| tool.definition().name)
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn replace(
        &self,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<(u64, BTreeSet<String>), String> {
        self.reserve(tools)?.commit()
    }

    pub(crate) fn reserve(
        &self,
        mut tools: Vec<Arc<dyn Tool>>,
    ) -> Result<DynamicToolReservation, String> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| "dynamic tool host is no longer available".to_owned())?;
        let mut state = registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(policy) = &state.policy {
            tools.retain(|tool| policy(&tool.definition().name));
        }
        validate_dynamic_catalog(&state, &self.owner, &tools)?;
        if state
            .reservations
            .iter()
            .any(|reservation| reservation.owner == self.owner)
        {
            return Err(format!(
                "dynamic tool catalog update already in progress for `{}`",
                self.owner
            ));
        }
        state.next_reservation = state.next_reservation.saturating_add(1);
        let id = state.next_reservation;
        let names = tools.iter().map(|tool| tool.definition().name).collect();
        state.reservations.push(DynamicToolReservationEntry {
            id,
            owner: self.owner.clone(),
            names,
        });
        Ok(DynamicToolReservation {
            id,
            owner: self.owner.clone(),
            registry: Arc::downgrade(&registry),
            tools: Some(tools),
        })
    }

    pub(crate) fn remove(&self) -> u64 {
        let Some(registry) = self.registry.upgrade() else {
            return 0;
        };
        let mut registry = registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_len = registry.groups.len();
        registry.groups.retain(|group| group.owner != self.owner);
        registry
            .reservations
            .retain(|reservation| reservation.owner != self.owner);
        if registry.groups.len() != previous_len {
            registry.revision = registry.revision.saturating_add(1);
        }
        registry.revision
    }
}

impl DynamicToolReservation {
    #[cfg(test)]
    pub(crate) fn commit(self) -> Result<(u64, BTreeSet<String>), String> {
        self.commit_with(|_, _| {})
    }

    pub(crate) fn commit_with(
        mut self,
        before_publish: impl FnOnce(u64, &BTreeSet<String>),
    ) -> Result<(u64, BTreeSet<String>), String> {
        let registry = self
            .registry
            .upgrade()
            .ok_or_else(|| "dynamic tool host is no longer available".to_owned())?;
        let mut registry = registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(index) = registry
            .reservations
            .iter()
            .position(|reservation| reservation.id == self.id && reservation.owner == self.owner)
        else {
            return Err("dynamic tool catalog reservation is no longer active".to_owned());
        };
        let mut tools = self
            .tools
            .take()
            .ok_or_else(|| "dynamic tool catalog reservation was already committed".to_owned())?;
        if let Some(policy) = &registry.policy {
            tools.retain(|tool| policy(&tool.definition().name));
        }
        let published = tools
            .iter()
            .map(|tool| tool.definition().name)
            .collect::<BTreeSet<_>>();
        let revision = registry.revision.saturating_add(1);
        before_publish(revision, &published);
        if let Some(group) = registry
            .groups
            .iter_mut()
            .find(|group| group.owner == self.owner)
        {
            group.tools = tools;
        } else {
            registry.groups.push(DynamicToolGroup {
                owner: self.owner.clone(),
                tools,
            });
            registry
                .groups
                .sort_by(|left, right| left.owner.cmp(&right.owner));
        }
        registry.reservations.swap_remove(index);
        registry.revision = revision;
        Ok((registry.revision, published))
    }
}

impl Drop for DynamicToolReservation {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry
            .reservations
            .retain(|reservation| reservation.id != self.id || reservation.owner != self.owner);
    }
}

fn validate_dynamic_catalog(
    registry: &DynamicToolRegistry,
    owner: &str,
    tools: &[Arc<dyn Tool>],
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for tool in tools {
        let name = tool.definition().name;
        if !names.insert(name.clone()) {
            return Err(format!("dynamic tool catalog contains duplicate `{name}`"));
        }
        if registry.static_names.contains(&name) {
            return Err(format!("dynamic tool `{name}` conflicts with a host tool"));
        }
        if registry
            .groups
            .iter()
            .filter(|group| group.owner != owner)
            .flat_map(|group| &group.tools)
            .any(|registered| registered.definition().name == name)
        {
            return Err(format!(
                "dynamic tool `{name}` conflicts with another extension"
            ));
        }
        if registry
            .reservations
            .iter()
            .filter(|reservation| reservation.owner != owner)
            .any(|reservation| reservation.names.contains(&name))
        {
            return Err(format!(
                "dynamic tool `{name}` is reserved by another extension update"
            ));
        }
    }
    Ok(())
}

/// Registry of tools and event observers, filled by extensions and consumed
/// by [`Agent::new`](crate::Agent::new).
#[derive(Clone)]
pub struct ExtensionHost {
    pub(crate) tools: Vec<Arc<dyn Tool>>,
    pub(crate) observers: Vec<Arc<dyn EventObserver>>,
    pub(crate) tool_call_hooks: Vec<Arc<dyn ToolCallHook>>,
    pub(crate) duplicate_tools: Vec<String>,
    dynamic_tools: Arc<RwLock<DynamicToolRegistry>>,
}

impl Default for ExtensionHost {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            observers: Vec::new(),
            tool_call_hooks: Vec::new(),
            duplicate_tools: Vec::new(),
            dynamic_tools: Arc::new(RwLock::new(DynamicToolRegistry::default())),
        }
    }
}

impl ExtensionHost {
    /// Creates an empty host.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool. Core tools use this same method.
    ///
    /// Duplicate tool names are rejected deterministically: the first
    /// registration wins, and [`Agent::new`](crate::Agent::new) fails with
    /// [`AgentError::DuplicateTool`](crate::AgentError::DuplicateTool) if any
    /// duplicate was registered.
    pub fn tool(&mut self, tool: impl Tool + 'static) {
        self.tool_arc(Arc::new(tool));
    }

    pub(crate) fn tool_arc(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name;
        let mut dynamic = self
            .dynamic_tools
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.tools.iter().any(|t| t.definition().name == name)
            || dynamic
                .groups
                .iter()
                .flat_map(|group| &group.tools)
                .any(|registered| registered.definition().name == name)
            || dynamic
                .reservations
                .iter()
                .any(|reservation| reservation.names.contains(&name))
        {
            self.duplicate_tools.push(name);
        } else {
            dynamic.static_names.insert(name);
            self.tools.push(tool);
        }
    }

    /// Reserves names for host tools installed after extension discovery.
    /// Reserved names participate in dynamic-catalog conflict checks without
    /// creating provider-visible schemas until the real tools are installed.
    pub fn reserve_tool_names<'a>(&mut self, names: impl IntoIterator<Item = &'a str>) {
        let mut dynamic = self
            .dynamic_tools
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for name in names {
            if dynamic
                .groups
                .iter()
                .flat_map(|group| &group.tools)
                .any(|tool| tool.definition().name == name)
                || dynamic
                    .reservations
                    .iter()
                    .any(|reservation| reservation.names.contains(name))
            {
                self.duplicate_tools.push(name.to_owned());
            } else {
                dynamic.static_names.insert(name.to_owned());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn dynamic_tools(
        &mut self,
        owner: impl Into<String>,
        tools: Vec<Arc<dyn Tool>>,
    ) -> Result<DynamicToolRegistration, String> {
        self.dynamic_tools_with(owner, tools, |_, _| {})
    }

    pub(crate) fn dynamic_tools_with(
        &mut self,
        owner: impl Into<String>,
        tools: Vec<Arc<dyn Tool>>,
        before_publish: impl FnOnce(u64, &BTreeSet<String>),
    ) -> Result<DynamicToolRegistration, String> {
        let registration = DynamicToolRegistration {
            owner: owner.into(),
            registry: Arc::downgrade(&self.dynamic_tools),
        };
        registration.reserve(tools)?.commit_with(before_publish)?;
        Ok(registration)
    }

    /// Registers an event observer.
    pub fn observe(&mut self, observer: impl EventObserver + 'static) {
        self.observers.push(Arc::new(observer));
    }

    /// Register a typed before/after tool-call hook.
    pub fn tool_call_hook(&mut self, hook: impl ToolCallHook + 'static) {
        self.tool_call_hooks.push(Arc::new(hook));
    }

    /// Loads an extension by letting it register against this host.
    pub fn load(&mut self, extension: &dyn Extension) {
        extension.register(self);
    }

    /// Keep only tools accepted by one authoritative product policy. Filtering
    /// the execution registry here also filters the provider schemas returned
    /// by [`tool_definitions`](Self::tool_definitions).
    pub fn retain_tools(&mut self, mut keep: impl FnMut(&str) -> bool) {
        let mut dynamic = self
            .dynamic_tools
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.tools.retain(|tool| {
            let name = tool.definition().name;
            let retained = keep(&name);
            if !retained {
                dynamic.static_names.remove(&name);
            }
            retained
        });
        for group in &mut dynamic.groups {
            group.tools.retain(|tool| keep(&tool.definition().name));
        }
        dynamic.revision = dynamic.revision.saturating_add(1);
    }

    /// Applies the authoritative product tool policy to current and future
    /// live extension catalogs.
    pub fn set_tool_policy(&mut self, keep: impl Fn(&str) -> bool + Send + Sync + 'static) {
        let keep = Arc::new(keep);
        self.tools.retain(|tool| keep(&tool.definition().name));
        let mut dynamic = self
            .dynamic_tools
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        dynamic.static_names.retain(|name| {
            self.tools
                .iter()
                .any(|tool| tool.definition().name == *name)
        });
        for group in &mut dynamic.groups {
            group.tools.retain(|tool| keep(&tool.definition().name));
        }
        dynamic.policy = Some(keep);
        dynamic.revision = dynamic.revision.saturating_add(1);
    }

    /// Builds a detached child host whose provider and execution surfaces are
    /// both restricted to the requested upper-bound allowlist.
    ///
    /// The dynamic registry is intentionally not shared with `self`: mutating a
    /// cloned host through `retain_tools` would otherwise prune the root agent's
    /// live registry as well. The child receives one frozen snapshot, while
    /// observers and hooks keep the parent's inherited policy behavior.
    pub(crate) fn scoped_tool_snapshot(
        &self,
        allowed: &BTreeSet<String>,
    ) -> Result<(Self, Vec<String>), String> {
        let (_, available) = self.tool_snapshot();
        let mut scoped = Self::new();
        scoped.observers = self.observers.clone();
        scoped.tool_call_hooks = self.tool_call_hooks.clone();
        let mut effective = Vec::new();
        for tool in available {
            let name = tool.definition().name;
            if allowed.contains(&name) {
                effective.push(name);
                scoped.tool_arc(tool);
            }
        }
        effective.sort();
        effective.dedup();
        if effective.is_empty() {
            return Err(
                "requested child tool scope has no tools available after parent policy".into(),
            );
        }
        scoped.finalize_tool_surface();
        Ok((scoped, effective))
    }

    pub(crate) fn tool_snapshot(&self) -> (u64, Vec<Arc<dyn Tool>>) {
        let dynamic = self
            .dynamic_tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dynamic_len: usize = dynamic.groups.iter().map(|group| group.tools.len()).sum();
        let mut tools = Vec::with_capacity(self.tools.len() + dynamic_len);
        tools.extend(self.tools.iter().cloned());
        tools.extend(
            dynamic
                .groups
                .iter()
                .flat_map(|group| group.tools.iter().cloned()),
        );
        (dynamic.revision, tools)
    }

    /// Opens live extension catalog publication after all host tool names have
    /// either been installed or reserved.
    pub fn finalize_tool_surface(&self) {
        let ready_changed = {
            let mut dynamic = self
                .dynamic_tools
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if dynamic.ready {
                return;
            }
            dynamic.ready = true;
            Arc::clone(&dynamic.ready_changed)
        };
        ready_changed.notify_waiters();
    }

    /// Returns the exact provider schemas currently registered, in wire order.
    pub fn tool_definitions(&self) -> Vec<ToolDef> {
        self.tool_snapshot()
            .1
            .iter()
            .map(|tool| tool.definition())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::ToolEffect;
    use crate::tool::{ToolContext, ToolError, ToolOutput};
    use ygg_ai::ToolDef;

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: self.0.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        fn effect(
            &self,
            _args: &serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolEffect, ToolError> {
            Ok(ToolEffect::Pure)
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::new("ok"))
        }
    }

    #[test]
    fn filtering_removes_both_schema_and_implementation() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("read"));
        host.tool(NamedTool("write"));
        host.retain_tools(|name| name == "read");
        assert_eq!(host.tool_definitions().len(), 1);
        assert_eq!(host.tool_definitions()[0].name, "read");
    }

    #[test]
    fn scoped_child_snapshot_is_exact_and_does_not_mutate_shared_dynamic_registry() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("read"));
        host.tool(NamedTool("write"));
        host.dynamic_tools("search-provider", vec![named_tool("search")])
            .unwrap();
        let allowed = BTreeSet::from(["read".to_owned(), "search".to_owned()]);

        let (mut child, effective) = host.scoped_tool_snapshot(&allowed).unwrap();
        assert_eq!(effective, vec!["read", "search"]);
        assert_eq!(
            child
                .tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<BTreeSet<_>>(),
            allowed
        );
        child.retain_tools(|name| name == "read");

        assert_eq!(
            host.tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["read".to_owned(), "search".to_owned(), "write".to_owned()])
        );
    }

    #[test]
    fn duplicate_tool_names_are_recorded() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("read"));
        host.tool(NamedTool("search"));
        host.tool(NamedTool("read"));
        assert_eq!(host.tools.len(), 2);
        assert_eq!(host.duplicate_tools, vec!["read".to_string()]);
    }

    fn named_tool(name: &'static str) -> Arc<dyn Tool> {
        Arc::new(NamedTool(name))
    }

    #[test]
    fn dynamic_catalog_conflict_preserves_the_published_group() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("core"));
        let registration = host
            .dynamic_tools("alpha", vec![named_tool("alpha_old")])
            .unwrap();
        let before = host.tool_snapshot().0;

        assert!(registration.replace(vec![named_tool("core")]).is_err());
        assert_eq!(host.tool_snapshot().0, before);
        assert_eq!(
            registration.published_names(),
            BTreeSet::from(["alpha_old".to_owned()])
        );
    }

    #[test]
    fn dynamic_catalog_acknowledges_only_policy_accepted_names() {
        let mut host = ExtensionHost::new();
        host.set_tool_policy(|name| name != "denied");
        let registration = host
            .dynamic_tools("alpha", vec![named_tool("allowed"), named_tool("denied")])
            .unwrap();

        assert_eq!(
            registration.published_names(),
            BTreeSet::from(["allowed".to_owned()])
        );
        assert_eq!(
            host.tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>(),
            vec!["allowed"]
        );
    }

    #[test]
    fn dynamic_groups_have_stable_owner_order_independent_of_startup_timing() {
        let mut host = ExtensionHost::new();
        host.tool(NamedTool("core"));
        host.dynamic_tools("beta", vec![named_tool("beta_tool")])
            .unwrap();
        host.dynamic_tools("alpha", vec![named_tool("alpha_tool")])
            .unwrap();

        assert_eq!(
            host.tool_definitions()
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>(),
            vec!["core", "alpha_tool", "beta_tool"]
        );
    }

    #[test]
    fn reserved_replacement_is_invisible_and_blocks_cross_extension_claims() {
        let mut host = ExtensionHost::new();
        let alpha = host
            .dynamic_tools("alpha", vec![named_tool("alpha_old")])
            .unwrap();
        let beta = host
            .dynamic_tools("beta", vec![named_tool("beta")])
            .unwrap();
        let reservation = alpha.reserve(vec![named_tool("alpha_new")]).unwrap();

        let visible = host
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            visible,
            BTreeSet::from(["alpha_old".to_owned(), "beta".to_owned()])
        );
        assert!(beta.replace(vec![named_tool("alpha_new")]).is_err());

        let (_, published) = reservation.commit().unwrap();
        assert_eq!(published, BTreeSet::from(["alpha_new".to_owned()]));
        assert_eq!(
            alpha.published_names(),
            BTreeSet::from(["alpha_new".to_owned()])
        );
    }
}
