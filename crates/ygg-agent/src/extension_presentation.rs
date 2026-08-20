//! Frontend-neutral presentation data published by executable extensions.
//!
//! The wire carries bounded semantic state only. Frontends retain ownership of
//! layout, focus, selection, accessibility, sanitization, and action routing.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Maximum activities retained in one extension presentation snapshot.
pub const MAX_EXTENSION_PRESENTATION_ACTIVITIES: usize = 128;
/// Maximum collection nodes retained in one extension presentation snapshot.
pub const MAX_EXTENSION_PRESENTATION_NODES: usize = 256;
/// Maximum declared actions retained in one extension presentation snapshot.
pub const MAX_EXTENSION_PRESENTATION_ACTIONS: usize = 64;
/// Maximum references attached to one semantic presentation item.
pub const MAX_EXTENSION_PRESENTATION_REFERENCES: usize = 8;
/// Maximum tree nesting accepted from an extension.
pub const MAX_EXTENSION_PRESENTATION_DEPTH: usize = 16;
/// Maximum UTF-8 bytes in a compact label or identifier.
pub const MAX_EXTENSION_PRESENTATION_LABEL_BYTES: usize = 1_024;
/// Maximum UTF-8 bytes in a detail document.
pub const MAX_EXTENSION_PRESENTATION_DETAIL_BYTES: usize = 64 * 1_024;
/// Maximum revision exactly representable by every supported JSON frontend.
pub const MAX_EXTENSION_PRESENTATION_REVISION: u64 = (1_u64 << 53) - 1;
/// Maximum encoded bytes in one complete presentation snapshot.
pub const MAX_EXTENSION_PRESENTATION_BYTES: usize = 256 * 1_024;

/// Generic lifecycle/health state rendered consistently by every frontend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPresentationState {
    /// No product-owned items currently exist.
    Empty,
    /// Initial state is being loaded.
    Loading,
    /// Work has been admitted but has not started.
    Pending,
    /// A stateful capability is available or selected.
    Active,
    /// Work is currently running.
    Running,
    /// Work completed successfully.
    Succeeded,
    /// Work failed.
    Failed,
    /// Work was cancelled before completion.
    Cancelled,
    /// The capability remains usable with reduced functionality.
    Degraded,
    /// Work or a resource was deliberately stopped.
    Stopped,
    /// A required external resource is unavailable.
    Unavailable,
}

/// A compact extension status value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationStatus {
    /// Generic state used for host-owned styling and accessibility labels.
    pub state: ExtensionPresentationState,
    /// Compact plain-text label.
    pub label: String,
    /// Optional bounded plain-text explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Type of an opaque host- or extension-owned reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPresentationReferenceKind {
    /// Durable Ygg session reference.
    Session,
    /// Host-ingested artifact reference.
    Artifact,
    /// Extension-owned opaque resource reference.
    Resource,
    /// Sanitized user-clicked HTTP(S) source link.
    Url,
}

/// A vetted link or opaque reference routed by a host-owned frontend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationReference {
    /// Reference class.
    pub kind: ExtensionPresentationReferenceKind,
    /// Opaque stable identifier; never interpreted as rendering code.
    pub id: String,
    /// Optional safe host-rendered label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One bounded operational activity note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationActivity {
    /// Stable extension-scoped activity identifier.
    pub id: String,
    /// Capability-owned semantic kind, such as `search` or `memory_read`.
    pub kind: String,
    /// Generic lifecycle state.
    pub state: ExtensionPresentationState,
    /// Compact plain-text summary with no raw terminal markup.
    pub summary: String,
    /// Optional bounded, content-free provenance label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// Optional Unix timestamp in milliseconds for start ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Optional Unix timestamp in milliseconds for terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    /// Opaque references associated with this activity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ExtensionPresentationReference>,
}

/// A host-rendered collection layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPresentationCollectionKind {
    /// Flat ordered collection.
    List,
    /// Parent-linked tree.
    Tree,
}

/// One stable node in a generic list or tree snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationNode {
    /// Stable extension-scoped node identifier.
    pub id: String,
    /// Parent identifier for tree nodes; absent for roots and lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Generic state.
    pub state: ExtensionPresentationState,
    /// Primary safe plain-text label.
    pub label: String,
    /// Optional compact secondary metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    /// Declared action IDs made available for this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action_ids: Vec<String>,
    /// Opaque references associated with this node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ExtensionPresentationReference>,
}

/// Selected-node detail shown by a host-owned inspector.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationDetail {
    /// Node this document describes, when collection-owned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Plain-text document title.
    pub title: String,
    /// Plain-text or host-sanitized Markdown-like body; raw ANSI/HTML is data.
    pub body: String,
    /// Opaque references associated with the detail document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<ExtensionPresentationReference>,
}

/// A bounded list/tree and its current selected detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationCollection {
    /// Host-rendered collection kind.
    pub kind: ExtensionPresentationCollectionKind,
    /// Compact collection title.
    pub title: String,
    /// Stable nodes in display order.
    #[serde(default)]
    pub nodes: Vec<ExtensionPresentationNode>,
    /// Current extension-suggested selection; the frontend still owns focus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_node_id: Option<String>,
    /// Optional detail for the selected node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ExtensionPresentationDetail>,
}

/// A safe action routed to an already declared extension slash command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationAction {
    /// Stable extension-scoped action identifier.
    pub id: String,
    /// Host-rendered action label.
    pub label: String,
    /// Existing manifest-declared extension command name.
    pub command: String,
    /// Literal arguments passed through normal command validation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
    /// Whether the frontend should emphasize the existing confirmation boundary.
    #[serde(default)]
    pub destructive: bool,
}

/// Complete process-owned semantic presentation state at one monotonic revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPresentationSnapshot {
    /// Monotonic process-generation revision. Revision zero is valid initially.
    pub revision: u64,
    /// Compact status, when the extension has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ExtensionPresentationStatus>,
    /// Recent bounded operational activity.
    #[serde(default)]
    pub activities: Vec<ExtensionPresentationActivity>,
    /// Optional inspectable list/tree and detail document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<ExtensionPresentationCollection>,
    /// Actions routed to manifest-declared commands.
    #[serde(default)]
    pub actions: Vec<ExtensionPresentationAction>,
}

impl ExtensionPresentationSnapshot {
    /// Validates bounds, references, parentage, and declared action routing.
    pub fn validate(&self, declared_commands: &[String]) -> Result<(), String> {
        if self.revision > MAX_EXTENSION_PRESENTATION_REVISION {
            return Err(format!(
                "presentation revision {} exceeds the portable JSON integer limit {MAX_EXTENSION_PRESENTATION_REVISION}",
                self.revision
            ));
        }
        let encoded = serde_json::to_vec(self)
            .map_err(|error| format!("presentation snapshot cannot be encoded: {error}"))?;
        if encoded.len() > MAX_EXTENSION_PRESENTATION_BYTES {
            return Err(format!(
                "presentation snapshot is {} bytes; limit is {MAX_EXTENSION_PRESENTATION_BYTES}",
                encoded.len()
            ));
        }
        if self.activities.len() > MAX_EXTENSION_PRESENTATION_ACTIVITIES {
            return Err(format!(
                "presentation snapshot has {} activities; limit is {MAX_EXTENSION_PRESENTATION_ACTIVITIES}",
                self.activities.len()
            ));
        }
        if self.actions.len() > MAX_EXTENSION_PRESENTATION_ACTIONS {
            return Err(format!(
                "presentation snapshot has {} actions; limit is {MAX_EXTENSION_PRESENTATION_ACTIONS}",
                self.actions.len()
            ));
        }

        if let Some(status) = &self.status {
            validate_label("status label", &status.label)?;
            validate_optional_label("status detail", status.detail.as_deref())?;
        }

        let mut activity_ids = BTreeSet::new();
        for activity in &self.activities {
            validate_id("activity", &activity.id)?;
            validate_id("activity kind", &activity.kind)?;
            validate_label("activity summary", &activity.summary)?;
            validate_optional_label("activity provenance", activity.provenance.as_deref())?;
            validate_references(&activity.references)?;
            if !activity_ids.insert(activity.id.as_str()) {
                return Err(format!(
                    "duplicate presentation activity id {:?}",
                    activity.id
                ));
            }
            if activity
                .started_at_ms
                .into_iter()
                .chain(activity.completed_at_ms)
                .any(|timestamp| timestamp > MAX_EXTENSION_PRESENTATION_REVISION)
            {
                return Err(format!(
                    "presentation activity {:?} has a timestamp above the portable JSON integer limit",
                    activity.id
                ));
            }
            if let (Some(start), Some(end)) = (activity.started_at_ms, activity.completed_at_ms) {
                if end < start {
                    return Err(format!(
                        "presentation activity {:?} completes before it starts",
                        activity.id
                    ));
                }
            }
        }

        let declared_commands = declared_commands
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let mut action_ids = BTreeSet::new();
        for action in &self.actions {
            validate_id("action", &action.id)?;
            validate_label("action label", &action.label)?;
            validate_id("action command", &action.command)?;
            if !declared_commands.contains(action.command.as_str()) {
                return Err(format!(
                    "presentation action {:?} routes to undeclared command {:?}",
                    action.id, action.command
                ));
            }
            if !action_ids.insert(action.id.as_str()) {
                return Err(format!("duplicate presentation action id {:?}", action.id));
            }
            if action.arguments.len() > 32 {
                return Err(format!(
                    "presentation action {:?} has too many arguments",
                    action.id
                ));
            }
            for argument in &action.arguments {
                validate_text(
                    "action argument",
                    argument,
                    MAX_EXTENSION_PRESENTATION_LABEL_BYTES,
                    false,
                )?;
            }
        }

        if let Some(collection) = &self.collection {
            validate_label("collection title", &collection.title)?;
            validate_collection(collection, &action_ids)?;
        }
        Ok(())
    }
}

fn validate_collection(
    collection: &ExtensionPresentationCollection,
    action_ids: &BTreeSet<&str>,
) -> Result<(), String> {
    if collection.nodes.len() > MAX_EXTENSION_PRESENTATION_NODES {
        return Err(format!(
            "presentation collection has {} nodes; limit is {MAX_EXTENSION_PRESENTATION_NODES}",
            collection.nodes.len()
        ));
    }
    let mut nodes = BTreeMap::new();
    for node in &collection.nodes {
        validate_id("node", &node.id)?;
        validate_label("node label", &node.label)?;
        validate_optional_label("node secondary metadata", node.secondary.as_deref())?;
        validate_references(&node.references)?;
        if nodes
            .insert(node.id.as_str(), node.parent_id.as_deref())
            .is_some()
        {
            return Err(format!("duplicate presentation node id {:?}", node.id));
        }
        let mut seen_actions = BTreeSet::new();
        for action_id in &node.action_ids {
            validate_id("node action", action_id)?;
            if !action_ids.contains(action_id.as_str()) {
                return Err(format!(
                    "presentation node {:?} references unknown action {:?}",
                    node.id, action_id
                ));
            }
            if !seen_actions.insert(action_id.as_str()) {
                return Err(format!(
                    "presentation node {:?} repeats action {:?}",
                    node.id, action_id
                ));
            }
        }
    }
    if collection.kind == ExtensionPresentationCollectionKind::List
        && nodes.values().any(Option::is_some)
    {
        return Err("presentation list nodes cannot declare parents".into());
    }
    for (id, parent) in &nodes {
        if let Some(parent) = parent {
            if parent == id {
                return Err(format!("presentation node {id:?} cannot parent itself"));
            }
            if !nodes.contains_key(parent) {
                return Err(format!(
                    "presentation node {id:?} names missing parent {parent:?}"
                ));
            }
        }
        let mut cursor = Some(*id);
        let mut visited = BTreeSet::new();
        let mut depth = 0usize;
        while let Some(current) = cursor {
            if !visited.insert(current) {
                return Err(format!(
                    "presentation collection contains a cycle at {current:?}"
                ));
            }
            depth += 1;
            if depth > MAX_EXTENSION_PRESENTATION_DEPTH {
                return Err(format!(
                    "presentation node {id:?} exceeds depth limit {MAX_EXTENSION_PRESENTATION_DEPTH}"
                ));
            }
            cursor = nodes.get(current).copied().flatten();
        }
    }

    if let Some(selected) = collection.selected_node_id.as_deref() {
        if !nodes.contains_key(selected) {
            return Err(format!(
                "presentation selection names missing node {selected:?}"
            ));
        }
    }
    if let Some(detail) = &collection.detail {
        validate_label("detail title", &detail.title)?;
        validate_text(
            "detail body",
            &detail.body,
            MAX_EXTENSION_PRESENTATION_DETAIL_BYTES,
            true,
        )?;
        validate_references(&detail.references)?;
        if let Some(node_id) = detail.node_id.as_deref() {
            if !nodes.contains_key(node_id) {
                return Err(format!(
                    "presentation detail names missing node {node_id:?}"
                ));
            }
            if collection.selected_node_id.as_deref() != Some(node_id) {
                return Err("presentation detail must describe the selected node".into());
            }
        }
    }
    Ok(())
}

fn validate_references(references: &[ExtensionPresentationReference]) -> Result<(), String> {
    if references.len() > MAX_EXTENSION_PRESENTATION_REFERENCES {
        return Err(format!(
            "presentation item has {} references; limit is {MAX_EXTENSION_PRESENTATION_REFERENCES}",
            references.len()
        ));
    }
    let mut unique = BTreeSet::new();
    for reference in references {
        validate_text(
            "reference id",
            &reference.id,
            MAX_EXTENSION_PRESENTATION_LABEL_BYTES,
            false,
        )?;
        if reference.kind == ExtensionPresentationReferenceKind::Url {
            validate_url_reference(&reference.id)?;
        }
        validate_optional_label("reference label", reference.label.as_deref())?;
        if !unique.insert((reference.kind, reference.id.as_str())) {
            return Err(format!(
                "duplicate presentation reference {:?}",
                reference.id
            ));
        }
    }
    Ok(())
}

fn validate_url_reference(value: &str) -> Result<(), String> {
    let parsed = url::Url::parse(value)
        .map_err(|_| "presentation URL reference is not a valid absolute URL".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(
            "presentation URL reference must use HTTP(S) without embedded credentials".into(),
        );
    }
    let Some(host) = parsed.host() else {
        return Err("presentation URL reference requires a host".into());
    };
    let unsafe_host = match host {
        url::Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            domain == "localhost" || domain.ends_with(".localhost") || domain.ends_with(".local")
        }
        url::Host::Ipv4(address) => unsafe_ipv4_address(address),
        url::Host::Ipv6(address) => {
            if let Some(mapped) = address.to_ipv4() {
                unsafe_ipv4_address(mapped)
            } else {
                address.is_loopback()
                    || address.is_unspecified()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
                    || address.is_multicast()
            }
        }
    };
    if unsafe_host {
        return Err("presentation URL reference cannot target a local or private host".into());
    }
    Ok(())
}

fn unsafe_ipv4_address(address: std::net::Ipv4Addr) -> bool {
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || address.is_multicast()
}

fn validate_id(kind: &str, value: &str) -> Result<(), String> {
    validate_text(kind, value, MAX_EXTENSION_PRESENTATION_LABEL_BYTES, false)?;
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
    }) {
        return Err(format!(
            "presentation {kind} {value:?} contains unsupported characters"
        ));
    }
    Ok(())
}

fn validate_label(kind: &str, value: &str) -> Result<(), String> {
    validate_text(kind, value, MAX_EXTENSION_PRESENTATION_LABEL_BYTES, false)
}

fn validate_optional_label(kind: &str, value: Option<&str>) -> Result<(), String> {
    value.map_or(Ok(()), |value| validate_label(kind, value))
}

fn validate_text(kind: &str, value: &str, limit: usize, allow_newline: bool) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("presentation {kind} must not be empty"));
    }
    if value.len() > limit {
        return Err(format!(
            "presentation {kind} is {} bytes; limit is {limit}",
            value.len()
        ));
    }
    if value.contains('\u{1b}')
        || value.chars().any(|character| {
            character.is_control() && !(allow_newline && matches!(character, '\n' | '\r' | '\t'))
                || matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{feff}'
                )
        })
    {
        return Err(format!(
            "presentation {kind} contains terminal, control, or invisible formatting characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ExtensionPresentationSnapshot {
        ExtensionPresentationSnapshot {
            revision: 1,
            status: Some(ExtensionPresentationStatus {
                state: ExtensionPresentationState::Active,
                label: "2 workers".into(),
                detail: None,
            }),
            activities: vec![ExtensionPresentationActivity {
                id: "worker:1".into(),
                kind: "delegation".into(),
                state: ExtensionPresentationState::Running,
                summary: "Reviewing tests".into(),
                provenance: Some("local child".into()),
                started_at_ms: Some(10),
                completed_at_ms: None,
                references: vec![],
            }],
            collection: Some(ExtensionPresentationCollection {
                kind: ExtensionPresentationCollectionKind::Tree,
                title: "Workers".into(),
                nodes: vec![ExtensionPresentationNode {
                    id: "worker:1".into(),
                    parent_id: None,
                    state: ExtensionPresentationState::Running,
                    label: "test-review".into(),
                    secondary: Some("running".into()),
                    action_ids: vec!["stop".into()],
                    references: vec![],
                }],
                selected_node_id: Some("worker:1".into()),
                detail: Some(ExtensionPresentationDetail {
                    node_id: Some("worker:1".into()),
                    title: "test-review".into(),
                    body: "Running in a bounded child session.".into(),
                    references: vec![],
                }),
            }),
            actions: vec![ExtensionPresentationAction {
                id: "stop".into(),
                label: "Stop worker".into(),
                command: "workers".into(),
                arguments: vec!["stop".into(), "worker:1".into()],
                destructive: true,
            }],
        }
    }

    #[test]
    fn accepts_bounded_semantic_snapshot() {
        snapshot().validate(&["workers".into()]).unwrap();
    }

    #[test]
    fn rejects_undeclared_action_and_cycles() {
        assert!(snapshot().validate(&[]).unwrap_err().contains("undeclared"));
        let mut value = snapshot();
        value.collection.as_mut().unwrap().nodes[0].parent_id = Some("worker:1".into());
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("parent itself"));
    }

    #[test]
    fn url_references_are_user_clicked_and_reject_local_targets() {
        let mut value = snapshot();
        value.activities[0].references = vec![ExtensionPresentationReference {
            kind: ExtensionPresentationReferenceKind::Url,
            id: "https://example.com/docs/source".into(),
            label: Some("Example source".into()),
        }];
        value.validate(&["workers".into()]).unwrap();

        value.activities[0].references[0].id = "http://127.0.0.1/private".into();
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("local or private"));

        value.activities[0].references[0].id = "http://[::ffff:127.0.0.1]/mapped-private".into();
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("local or private"));
    }

    #[test]
    fn rejects_nonportable_revision() {
        let mut value = snapshot();
        value.revision = MAX_EXTENSION_PRESENTATION_REVISION + 1;
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("portable JSON integer"));

        value.revision = 1;
        value.activities[0].started_at_ms = Some(MAX_EXTENSION_PRESENTATION_REVISION + 1);
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("timestamp above the portable JSON integer"));
    }

    #[test]
    fn rejects_terminal_escapes_and_stale_detail_selection() {
        let mut value = snapshot();
        value.status.as_mut().unwrap().label = "ready\u{1b}[31m".into();
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("control"));

        value.status.as_mut().unwrap().label = "ready\u{202e}txt".into();
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("invisible formatting"));

        let mut value = snapshot();
        value.actions[0].arguments[0] = "stop\nspoof".into();
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("control"));

        let mut value = snapshot();
        value.collection.as_mut().unwrap().selected_node_id = None;
        assert!(value
            .validate(&["workers".into()])
            .unwrap_err()
            .contains("selected node"));
    }
}
