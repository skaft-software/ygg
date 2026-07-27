#![allow(missing_docs)]

#[path = "../src/runtime_status.rs"]
mod runtime_status;

use runtime_status::*;
use serde_json::{json, Value};

fn id(value: &str) -> RuntimeId {
    RuntimeId::new(value).unwrap()
}

fn label(value: &str) -> RuntimeLabel {
    RuntimeLabel::new(value).unwrap()
}

fn objective(value: &str) -> AgentObjective {
    AgentObjective::new(value).unwrap()
}

fn summary(value: &str) -> RuntimeSummary {
    RuntimeSummary::new(value).unwrap()
}

fn command(value: &str) -> CommandName {
    CommandName::new(value).unwrap()
}

fn domain(value: &str) -> DomainName {
    DomainName::new(value).unwrap()
}

fn totals(values: &[(ContextCategory, u64)]) -> ContextTotals {
    ContextTotals::try_new(
        values
            .iter()
            .map(|(category, tokens)| ContextCategoryTotal {
                category: *category,
                tokens: *tokens,
            })
            .collect(),
        values.iter().map(|(_, tokens)| tokens).sum(),
    )
    .unwrap()
}

fn contribution(kind: ContributionKind, value: &str) -> CatalogContribution {
    CatalogContribution {
        kind,
        id: id(value),
        label: label(&format!("Contribution {value}")),
    }
}

fn entry(
    value: &str,
    kind: TrustedCatalogKind,
    enabled: bool,
    contributions: Vec<CatalogContribution>,
) -> TrustedCatalogEntry {
    TrustedCatalogEntry {
        id: id(value),
        label: label(&format!("Entry {value}")),
        kind,
        enabled,
        contributions: contributions.try_into().unwrap(),
    }
}

fn enforced_policy(revision: u64, at_ms: u64) -> RuntimePolicyStatus {
    RuntimePolicyStatus {
        revision,
        observed_at_ms: at_ms,
        filesystem: FilesystemPolicy::Enforced {
            access: FilesystemAccess::TrustedProjectRead,
        },
        tools: ToolPolicy::Enforced {
            rules: RuleSet::try_new(
                RuleDefault::Deny,
                vec![id("tool.read")],
                vec![id("tool.danger")],
            )
            .unwrap(),
        },
        commands: CommandPolicy::Enforced {
            rules: RuleSet::try_new(
                RuleDefault::Deny,
                vec![command("cargo")],
                vec![command("curl")],
            )
            .unwrap(),
        },
        remote_read: RemoteReadPolicy::Enforced {
            consequence: RemoteReadConsequence::DomainRules {
                domains: RuleSet::try_new(
                    RuleDefault::Deny,
                    vec![domain("docs.example.com")],
                    vec![domain("blocked.example.com")],
                )
                .unwrap(),
            },
        },
        process_network: ProcessNetworkPolicy::Enforced {
            consequence: ProcessNetworkConsequence::Blocked,
        },
        approvals: ApprovalPolicy::Enforced {
            consequence: ApprovalConsequence::RequiredFor {
                operations: vec![ApprovalOperation::Command].try_into().unwrap(),
            },
        },
        secrets: SecretsPolicy::Enforced {
            consequence: SecretsConsequence::NamedGrants {
                grants: vec![id("secret.build-token")].try_into().unwrap(),
            },
        },
    }
}

#[test]
fn public_scalar_types_reject_paths_commands_urls_controls_and_excess() {
    assert!(RuntimeId::new("../private").is_err());
    assert!(RuntimeId::new("has space").is_err());
    assert!(RuntimeId::new("x".repeat(MAX_RUNTIME_ID_BYTES + 1)).is_err());
    assert!(RuntimeLabel::new("/Users/alice/private/project").is_err());
    assert!(RuntimeSummary::new("failed in C:\\private\\repo").is_err());
    assert!(RuntimeSummary::new("line one\nline two").is_err());
    assert!(RuntimeSummary::new("x".repeat(MAX_RUNTIME_SUMMARY_BYTES + 1)).is_err());
    assert!(CommandName::new("cargo test").is_err());
    assert!(CommandName::new("/usr/bin/cargo").is_err());
    assert!(CommandName::new("cargo;curl").is_err());
    assert!(DomainName::new("https://example.com").is_err());
    assert!(DomainName::new("example.com:443").is_err());
    assert!(DomainName::new("*.example.com").is_err());
    assert!(DomainName::new("EXAMPLE.com").is_err());
    assert!(DomainName::new("-bad.example").is_err());
    assert_eq!(id("runtime.agent").as_str(), "runtime.agent");
    assert_eq!(command("cargo").as_str(), "cargo");
    assert_eq!(domain("api.example.com").as_str(), "api.example.com");
}

#[test]
fn bounded_vector_deserializer_rejects_the_first_excess_item() {
    type Two = BoundedVec<u64, 2>;
    assert_eq!(
        serde_json::from_value::<Two>(json!([1, 2]))
            .unwrap()
            .as_slice(),
        &[1, 2]
    );
    assert!(serde_json::from_value::<Two>(json!([1, 2, 3])).is_err());
    assert!(Two::try_new(vec![1, 2, 3]).is_err());
}

#[test]
fn child_agents_preserve_parentage_timing_outcomes_and_exact_replay() {
    let mut state = RuntimeStatusState::default();
    let root = RuntimeEvent::ChildAgentSpawned {
        id: id("agent.root"),
        parent_id: None,
        objective: objective("Coordinate bounded implementation work"),
        queued_at_ms: 10,
    };
    assert_eq!(state.apply(root.clone()).unwrap(), ApplyOutcome::Applied);
    assert_eq!(state.apply(root).unwrap(), ApplyOutcome::Replay);
    state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("agent.root"),
            state: ChildAgentState::Running,
            at_ms: 11,
            outcome: None,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::ChildAgentSpawned {
            id: id("agent.child"),
            parent_id: Some(id("agent.root")),
            objective: objective("Verify runtime projections"),
            queued_at_ms: 12,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("agent.child"),
            state: ChildAgentState::Running,
            at_ms: 13,
            outcome: None,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("agent.child"),
            state: ChildAgentState::Waiting,
            at_ms: 14,
            outcome: None,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("agent.child"),
            state: ChildAgentState::Succeeded,
            at_ms: 15,
            outcome: Some(summary("Focused validation passed")),
        })
        .unwrap();

    let snapshot = state.snapshot();
    let child = snapshot
        .child_agents
        .as_slice()
        .iter()
        .find(|agent| agent.id == id("agent.child"))
        .unwrap();
    assert_eq!(child.parent_id, Some(id("agent.root")));
    assert_eq!(child.started_at_ms, Some(13));
    assert_eq!(child.finished_at_ms, Some(15));
    assert_eq!(
        child.outcome.as_ref().unwrap().as_str(),
        "Focused validation passed"
    );
}

#[test]
fn child_agent_state_machine_rejects_orphans_cycles_regressions_and_fake_outcomes() {
    let mut state = RuntimeStatusState::default();
    assert!(state
        .apply(RuntimeEvent::ChildAgentSpawned {
            id: id("orphan"),
            parent_id: Some(id("missing")),
            objective: objective("Should fail"),
            queued_at_ms: 1,
        })
        .is_err());
    assert!(state
        .apply(RuntimeEvent::ChildAgentSpawned {
            id: id("self"),
            parent_id: Some(id("self")),
            objective: objective("Should fail"),
            queued_at_ms: 1,
        })
        .is_err());
    state
        .apply(RuntimeEvent::ChildAgentSpawned {
            id: id("valid"),
            parent_id: None,
            objective: objective("Valid work"),
            queued_at_ms: 10,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("valid"),
            state: ChildAgentState::Succeeded,
            at_ms: 11,
            outcome: None,
        })
        .is_err());
    assert!(state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("valid"),
            state: ChildAgentState::Running,
            at_ms: 11,
            outcome: Some(summary("not terminal")),
        })
        .is_err());
    state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("valid"),
            state: ChildAgentState::Running,
            at_ms: 11,
            outcome: None,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::ChildAgentTransitioned {
            id: id("valid"),
            state: ChildAgentState::Waiting,
            at_ms: 9,
            outcome: None,
        })
        .is_err());

    let mut hostile = serde_json::to_value(state.snapshot()).unwrap();
    hostile["childAgents"][0]["parentId"] = json!("valid");
    assert!(serde_json::from_value::<RuntimeSnapshot>(hostile).is_err());
}

#[test]
fn mcp_lifecycle_requires_failure_facts_and_explicit_restart() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::McpConfigured {
            id: id("mcp.github"),
            label: label("GitHub"),
            at_ms: 10,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::McpTransitioned {
            id: id("mcp.github"),
            state: McpServerState::Starting,
            at_ms: 11,
            failure: None,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::McpTransitioned {
            id: id("mcp.github"),
            state: McpServerState::Failed,
            at_ms: 12,
            failure: None,
        })
        .is_err());
    state
        .apply(RuntimeEvent::McpTransitioned {
            id: id("mcp.github"),
            state: McpServerState::Failed,
            at_ms: 12,
            failure: Some(summary("Handshake failed after redaction")),
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::McpTransitioned {
            id: id("mcp.github"),
            state: McpServerState::Starting,
            at_ms: 13,
            failure: None,
        })
        .is_err());
    state
        .apply(RuntimeEvent::McpRestarted {
            id: id("mcp.github"),
            at_ms: 13,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::McpTransitioned {
            id: id("mcp.github"),
            state: McpServerState::Ready,
            at_ms: 14,
            failure: None,
        })
        .unwrap();
    let snapshot = state.snapshot();
    let server = &snapshot.mcp_servers.as_slice()[0];
    assert_eq!(server.state, McpServerState::Ready);
    assert_eq!(server.restart_count, 1);
    assert!(server.failure.is_none());
}

#[test]
fn catalog_reload_is_atomic_and_failed_reload_retains_prior_generation() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload.1"),
            at_ms: 10,
        })
        .unwrap();
    let first = vec![
        entry(
            "skill.review",
            TrustedCatalogKind::Skill,
            true,
            vec![contribution(ContributionKind::Skill, "review")],
        ),
        entry(
            "extension.git",
            TrustedCatalogKind::Extension,
            false,
            vec![contribution(ContributionKind::Tool, "git.status")],
        ),
    ];
    state
        .apply(RuntimeEvent::CatalogReloadSucceeded {
            reload_id: id("reload.1"),
            entries: first.clone().try_into().unwrap(),
            at_ms: 11,
        })
        .unwrap();
    let committed = state.snapshot().catalog;
    assert_eq!(committed.generation, 1);

    state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload.2"),
            at_ms: 12,
        })
        .unwrap();
    assert_eq!(
        state.snapshot().catalog.entries.as_slice(),
        first.as_slice()
    );
    state
        .apply(RuntimeEvent::CatalogReloadFailed {
            reload_id: id("reload.2"),
            failure: summary("Catalog validation failed"),
            at_ms: 13,
        })
        .unwrap();
    let failed = state.snapshot().catalog;
    assert_eq!(failed.generation, 1);
    assert_eq!(failed.entries.as_slice(), first.as_slice());
    assert!(matches!(
        failed.reload,
        CatalogReloadStatus::Failed {
            retained_generation: 1,
            ..
        }
    ));
    assert!(state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload.stale"),
            at_ms: 12,
        })
        .is_err());
}

#[test]
fn catalog_rejects_duplicate_entries_and_contributions_before_commit() {
    let duplicate_entry = entry(
        "same",
        TrustedCatalogKind::Skill,
        true,
        vec![contribution(ContributionKind::Skill, "one")],
    );
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload"),
            at_ms: 1,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::CatalogReloadSucceeded {
            reload_id: id("reload"),
            entries: vec![duplicate_entry.clone(), duplicate_entry]
                .try_into()
                .unwrap(),
            at_ms: 2,
        })
        .is_err());
    assert_eq!(state.snapshot().catalog.generation, 0);
    assert!(matches!(
        state.snapshot().catalog.reload,
        CatalogReloadStatus::Running { .. }
    ));

    let duplicate_contribution = contribution(ContributionKind::Tool, "same-tool");
    assert!(state
        .apply(RuntimeEvent::CatalogReloadSucceeded {
            reload_id: id("reload"),
            entries: vec![entry(
                "entry",
                TrustedCatalogKind::Extension,
                true,
                vec![duplicate_contribution.clone(), duplicate_contribution,],
            )]
            .try_into()
            .unwrap(),
            at_ms: 2,
        })
        .is_err());
}

#[test]
fn catalog_enabled_changes_are_generation_guarded_and_blocked_during_reload() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload.initial"),
            at_ms: 1,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::CatalogReloadSucceeded {
            reload_id: id("reload.initial"),
            entries: vec![entry("skill.one", TrustedCatalogKind::Skill, false, vec![])]
                .try_into()
                .unwrap(),
            at_ms: 2,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::CatalogEntryEnabled {
            entry_id: id("skill.one"),
            enabled: true,
            expected_generation: 0,
            at_ms: 3,
        })
        .is_err());
    state
        .apply(RuntimeEvent::CatalogEntryEnabled {
            entry_id: id("skill.one"),
            enabled: true,
            expected_generation: 1,
            at_ms: 3,
        })
        .unwrap();
    assert_eq!(state.snapshot().catalog.generation, 2);
    assert!(state.snapshot().catalog.entries.as_slice()[0].enabled);
    let round_trip: RuntimeSnapshot =
        serde_json::from_value(serde_json::to_value(state.snapshot()).unwrap()).unwrap();
    assert_eq!(round_trip.catalog.generation, 2);
    state
        .apply(RuntimeEvent::CatalogReloadStarted {
            reload_id: id("reload.next"),
            at_ms: 4,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::CatalogEntryEnabled {
            entry_id: id("skill.one"),
            enabled: false,
            expected_generation: 2,
            at_ms: 5,
        })
        .is_err());
}

#[test]
fn lsp_lifecycle_tracks_project_language_diagnostics_and_restart() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::LspConfigured {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            at_ms: 10,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            state: LspServerState::Starting,
            at_ms: 11,
            failure: None,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::LspDiagnosticsPublished {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            revision: 1,
            counts: DiagnosticCounts {
                errors: 1,
                ..DiagnosticCounts::default()
            },
            at_ms: 12,
        })
        .is_err());
    state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            state: LspServerState::Ready,
            at_ms: 12,
            failure: None,
        })
        .unwrap();
    let counts = DiagnosticCounts {
        errors: 2,
        warnings: 3,
        information: 4,
        hints: 5,
    };
    state
        .apply(RuntimeEvent::LspDiagnosticsPublished {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            revision: 1,
            counts,
            at_ms: 13,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::LspDiagnosticsPublished {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            revision: 1,
            counts: DiagnosticCounts::default(),
            at_ms: 14,
        })
        .is_err());
    state
        .apply(RuntimeEvent::LspRestarted {
            project_id: id("project.ygg"),
            language_id: id("rust"),
            at_ms: 14,
        })
        .unwrap();
    let snapshot = state.snapshot();
    let server = &snapshot.lsp_servers.as_slice()[0];
    assert_eq!(server.state, LspServerState::Starting);
    assert_eq!(server.restart_count, 1);
    assert_eq!(server.diagnostics, DiagnosticCounts::default());
}

#[test]
fn lsp_rejects_unbounded_diagnostics_failures_without_summaries_and_wrong_keys() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::LspConfigured {
            project_id: id("project"),
            language_id: id("typescript"),
            at_ms: 1,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("project"),
            language_id: id("typescript"),
            state: LspServerState::Failed,
            at_ms: 2,
            failure: None,
        })
        .is_err());
    assert!(state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("other"),
            language_id: id("typescript"),
            state: LspServerState::Starting,
            at_ms: 2,
            failure: None,
        })
        .is_err());
    state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("project"),
            language_id: id("typescript"),
            state: LspServerState::Starting,
            at_ms: 2,
            failure: None,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::LspTransitioned {
            project_id: id("project"),
            language_id: id("typescript"),
            state: LspServerState::Ready,
            at_ms: 3,
            failure: None,
        })
        .unwrap();
    assert!(state
        .apply(RuntimeEvent::LspDiagnosticsPublished {
            project_id: id("project"),
            language_id: id("typescript"),
            revision: 1,
            counts: DiagnosticCounts {
                errors: MAX_DIAGNOSTICS_PER_SEVERITY + 1,
                ..DiagnosticCounts::default()
            },
            at_ms: 4,
        })
        .is_err());
}

#[test]
fn context_totals_deserialize_only_when_categories_are_unique_and_sum_exactly() {
    let valid: ContextTotals = serde_json::from_value(json!({
        "categories": [
            {"category": "system", "tokens": 10},
            {"category": "conversation", "tokens": 20}
        ],
        "totalTokens": 30
    }))
    .unwrap();
    assert_eq!(valid.total_tokens, 30);
    assert!(serde_json::from_value::<ContextTotals>(json!({
        "categories": [
            {"category": "system", "tokens": 10},
            {"category": "system", "tokens": 20}
        ],
        "totalTokens": 30
    }))
    .is_err());
    assert!(serde_json::from_value::<ContextTotals>(json!({
        "categories": [{"category": "system", "tokens": 10}],
        "totalTokens": 9
    }))
    .is_err());
    assert!(serde_json::from_value::<ContextTotals>(json!({
        "categories": [
            {"category": "system", "tokens": 18446744073709551615_u64},
            {"category": "conversation", "tokens": 1}
        ],
        "totalTokens": 18446744073709551615_u64
    }))
    .is_err());
}

#[test]
fn compaction_start_finish_replays_and_reconciles_totals_exactly() {
    let mut state = RuntimeStatusState::default();
    let before = totals(&[
        (ContextCategory::System, 100),
        (ContextCategory::Conversation, 900),
        (ContextCategory::ToolResults, 200),
    ]);
    state
        .apply(RuntimeEvent::ContextUpdated {
            totals: before.clone(),
            at_ms: 10,
        })
        .unwrap();
    let start = RuntimeEvent::CompactionStarted {
        id: id("compact.1"),
        before: before.clone(),
        at_ms: 11,
    };
    assert_eq!(state.apply(start.clone()).unwrap(), ApplyOutcome::Applied);
    assert_eq!(state.apply(start).unwrap(), ApplyOutcome::Replay);
    assert!(state
        .apply(RuntimeEvent::ContextUpdated {
            totals: before.clone(),
            at_ms: 12,
        })
        .is_err());
    let after = totals(&[
        (ContextCategory::System, 100),
        (ContextCategory::Conversation, 250),
        (ContextCategory::CompactionSummaries, 50),
    ]);
    assert!(state
        .apply(RuntimeEvent::CompactionFinished {
            id: id("compact.1"),
            after: after.clone(),
            reclaimed_tokens: 799,
            at_ms: 12,
        })
        .is_err());
    let finish = RuntimeEvent::CompactionFinished {
        id: id("compact.1"),
        after: after.clone(),
        reclaimed_tokens: 800,
        at_ms: 12,
    };
    assert_eq!(state.apply(finish.clone()).unwrap(), ApplyOutcome::Applied);
    assert_eq!(state.apply(finish).unwrap(), ApplyOutcome::Replay);
    assert_eq!(
        state
            .apply(RuntimeEvent::CompactionStarted {
                id: id("compact.1"),
                before,
                at_ms: 11,
            })
            .unwrap(),
        ApplyOutcome::Replay
    );
    let context = state.snapshot().context;
    assert_eq!(context.current, after);
    assert_eq!(context.last_compaction.unwrap().reclaimed_tokens, 800);
}

#[test]
fn durable_event_replay_reconstructs_identical_context_state() {
    let events = vec![
        RuntimeEvent::ContextUpdated {
            totals: totals(&[(ContextCategory::Conversation, 1_000)]),
            at_ms: 1,
        },
        RuntimeEvent::CompactionStarted {
            id: id("compact"),
            before: totals(&[(ContextCategory::Conversation, 1_000)]),
            at_ms: 2,
        },
        RuntimeEvent::CompactionFinished {
            id: id("compact"),
            after: totals(&[
                (ContextCategory::Conversation, 250),
                (ContextCategory::CompactionSummaries, 50),
            ]),
            reclaimed_tokens: 700,
            at_ms: 3,
        },
    ];
    let encoded = events
        .iter()
        .map(|event| serde_json::to_string(event).unwrap())
        .collect::<Vec<_>>();
    let mut first = RuntimeStatusState::default();
    let mut replayed = RuntimeStatusState::default();
    for event in events {
        first.apply(event).unwrap();
    }
    for line in encoded {
        replayed
            .apply(serde_json::from_str::<RuntimeEvent>(&line).unwrap())
            .unwrap();
    }
    assert_eq!(first.snapshot(), replayed.snapshot());

    let restored: RuntimeSnapshot =
        serde_json::from_str(&serde_json::to_string(&first.snapshot()).unwrap()).unwrap();
    assert_eq!(
        RuntimeStatusState::from_snapshot(restored)
            .unwrap()
            .snapshot(),
        first.snapshot()
    );
}

#[test]
fn every_policy_reports_enforced_or_unavailable_with_explicit_consequences() {
    let policy = enforced_policy(1, 10);
    assert_eq!(
        policy
            .filesystem
            .evaluate(FilesystemAccess::TrustedProjectRead),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy
            .filesystem
            .evaluate(FilesystemAccess::TrustedProjectReadWrite),
        PolicyEvaluation::Blocked
    );
    assert_eq!(
        policy.tools.evaluate(&id("tool.read")),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy.tools.evaluate(&id("tool.danger")),
        PolicyEvaluation::Blocked
    );
    assert_eq!(
        policy.commands.evaluate(&command("cargo")),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy.commands.evaluate(&command("curl")),
        PolicyEvaluation::Blocked
    );
    assert_eq!(
        policy.remote_read.evaluate(&domain("docs.example.com")),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy.remote_read.evaluate(&domain("blocked.example.com")),
        PolicyEvaluation::Blocked
    );
    assert_eq!(
        policy.process_network.evaluate(&domain("docs.example.com")),
        PolicyEvaluation::Blocked
    );
    assert_eq!(
        policy.approvals.evaluate(ApprovalOperation::Command),
        PolicyEvaluation::ApprovalRequired
    );
    assert_eq!(
        policy.approvals.evaluate(ApprovalOperation::Tool),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy.secrets.evaluate(&id("secret.build-token")),
        PolicyEvaluation::Allowed
    );
    assert_eq!(
        policy.secrets.evaluate(&id("secret.other")),
        PolicyEvaluation::Blocked
    );

    let unavailable = ToolPolicy::Unavailable {
        reason: summary("Isolation engine is unavailable"),
        consequence: UnavailableConsequence::FeatureBlocked,
    };
    assert_eq!(
        unavailable.evaluate(&id("tool.read")),
        PolicyEvaluation::Unavailable(UnavailableConsequence::FeatureBlocked)
    );
}

#[test]
fn hostile_policy_json_cannot_mix_unavailable_labels_with_enforcement_facts() {
    let mixed = json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "hostBehaviorUnknown",
        "rules": {"default": "allow", "allow": [], "deny": []}
    });
    assert!(serde_json::from_value::<ToolPolicy>(mixed.clone()).is_err());
    assert!(serde_json::from_value::<CommandPolicy>(mixed).is_err());

    assert!(serde_json::from_value::<FilesystemPolicy>(json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "hostBehaviorUnknown",
        "access": "trustedProjectReadWrite"
    }))
    .is_err());
    assert!(serde_json::from_value::<RemoteReadPolicy>(json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "featureBlocked",
        "domains": {"default": "allow", "allow": [], "deny": []}
    }))
    .is_err());
    assert!(serde_json::from_value::<ProcessNetworkPolicy>(json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "featureBlocked",
        "mode": "blocked"
    }))
    .is_err());
    assert!(serde_json::from_value::<ApprovalPolicy>(json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "featureBlocked",
        "operations": ["command"]
    }))
    .is_err());
    assert!(serde_json::from_value::<SecretsPolicy>(json!({
        "status": "unavailable",
        "reason": "No authoritative observer",
        "consequence": "featureBlocked",
        "grants": ["secret.raw"]
    }))
    .is_err());
}

#[test]
fn policy_rules_reject_overlap_duplicates_empty_contradictions_and_injection() {
    assert!(RuleSet::try_new(RuleDefault::Deny, vec![id("same")], vec![id("same")]).is_err());
    assert!(serde_json::from_value::<RuleSet<RuntimeId>>(json!({
        "default": "deny",
        "allow": ["same", "same"],
        "deny": []
    }))
    .is_err());
    assert!(serde_json::from_value::<RuleSet<CommandName>>(json!({
        "default": "deny",
        "allow": ["cargo test"],
        "deny": []
    }))
    .is_err());
    assert!(serde_json::from_value::<RuleSet<DomainName>>(json!({
        "default": "deny",
        "allow": ["https://example.com/private"],
        "deny": []
    }))
    .is_err());
    assert!(serde_json::from_value::<ApprovalPolicy>(json!({
        "status": "enforced",
        "consequence": {"mode": "requiredFor", "operations": []}
    }))
    .is_err());
    assert!(serde_json::from_value::<SecretsPolicy>(json!({
        "status": "enforced",
        "consequence": {"mode": "namedGrants", "grants": []}
    }))
    .is_err());
}

#[test]
fn policy_publication_is_revisioned_and_exactly_replayable() {
    let mut state = RuntimeStatusState::default();
    let first = RuntimeEvent::PolicyPublished {
        policy: Box::new(enforced_policy(1, 10)),
    };
    assert_eq!(state.apply(first.clone()).unwrap(), ApplyOutcome::Applied);
    assert_eq!(state.apply(first).unwrap(), ApplyOutcome::Replay);
    assert!(state
        .apply(RuntimeEvent::PolicyPublished {
            policy: Box::new(enforced_policy(1, 11)),
        })
        .is_err());
    assert!(state
        .apply(RuntimeEvent::PolicyPublished {
            policy: Box::new(enforced_policy(2, 9)),
        })
        .is_err());
    state
        .apply(RuntimeEvent::PolicyPublished {
            policy: Box::new(enforced_policy(2, 12)),
        })
        .unwrap();
    assert_eq!(state.snapshot().policy.unwrap().revision, 2);
}

#[test]
fn snapshot_and_events_are_camel_case_path_free_and_reject_unknown_fields() {
    let mut state = RuntimeStatusState::default();
    state
        .apply(RuntimeEvent::McpConfigured {
            id: id("mcp.one"),
            label: label("One"),
            at_ms: 1,
        })
        .unwrap();
    state
        .apply(RuntimeEvent::PolicyPublished {
            policy: Box::new(enforced_policy(1, 1)),
        })
        .unwrap();
    let value = serde_json::to_value(state.snapshot()).unwrap();
    assert!(value.get("childAgents").is_some());
    assert!(value.get("mcpServers").is_some());
    let serialized = serde_json::to_string(&value).unwrap();
    for forbidden in [
        "hostPath",
        "cwd",
        "rawConfig",
        "environment",
        "secretValue",
        "commandArguments",
        "/Users/",
        "/home/",
    ] {
        assert!(!serialized.contains(forbidden));
    }

    let hostile_event = json!({
        "type": "mcpConfigured",
        "id": "mcp.one",
        "label": "One",
        "atMs": 1,
        "rawConfig": {"command": "secret"}
    });
    assert!(serde_json::from_value::<RuntimeEvent>(hostile_event).is_err());
    let mut hostile_snapshot = value;
    hostile_snapshot["hostPath"] = json!("/private");
    assert!(serde_json::from_value::<RuntimeSnapshot>(hostile_snapshot).is_err());
}

#[test]
fn snapshot_deserializer_rejects_contradictory_runtime_facts() {
    let base = serde_json::to_value(RuntimeStatusState::default().snapshot()).unwrap();

    let mut bad_context = base.clone();
    bad_context["context"]["current"]["totalTokens"] = json!(1);
    assert!(serde_json::from_value::<RuntimeSnapshot>(bad_context).is_err());

    let mut bad_mcp = base.clone();
    bad_mcp["mcpServers"] = json!([{
        "id": "mcp.bad",
        "label": "Bad",
        "state": "ready",
        "restartCount": 0,
        "configuredAtMs": 10,
        "updatedAtMs": 9,
        "failure": "Should not exist"
    }]);
    assert!(serde_json::from_value::<RuntimeSnapshot>(bad_mcp).is_err());

    let mut bad_lsp = base.clone();
    bad_lsp["lspServers"] = json!([{
        "projectId": "project",
        "languageId": "rust",
        "state": "stopped",
        "restartCount": 0,
        "configuredAtMs": 1,
        "updatedAtMs": 2,
        "diagnosticRevision": 1,
        "diagnostics": {"errors": 1, "warnings": 0, "information": 0, "hints": 0}
    }]);
    assert!(serde_json::from_value::<RuntimeSnapshot>(bad_lsp).is_err());

    let mut bad_catalog = base;
    bad_catalog["catalog"] = json!({
        "generation": 2,
        "updatedAtMs": 2,
        "reload": {"state": "idle"},
        "entries": []
    });
    assert!(serde_json::from_value::<RuntimeSnapshot>(bad_catalog).is_err());
}

#[test]
fn hostile_collection_limits_apply_to_snapshot_catalog_rules_and_contributions() {
    let mut snapshot = serde_json::to_value(RuntimeStatusState::default().snapshot()).unwrap();
    snapshot["childAgents"] = Value::Array(
        (0..=MAX_CHILD_AGENTS)
            .map(|index| {
                json!({
                    "id": format!("agent.{index}"),
                    "objective": "bounded",
                    "state": "queued",
                    "queuedAtMs": 1,
                    "updatedAtMs": 1
                })
            })
            .collect(),
    );
    assert!(serde_json::from_value::<RuntimeSnapshot>(snapshot).is_err());

    let too_many_rules = Value::Array(
        (0..=MAX_POLICY_RULES)
            .map(|index| json!(format!("tool.{index}")))
            .collect(),
    );
    assert!(serde_json::from_value::<RuleSet<RuntimeId>>(json!({
        "default": "deny",
        "allow": too_many_rules,
        "deny": []
    }))
    .is_err());

    let too_many_contributions = (0..=MAX_ENTRY_CONTRIBUTIONS)
        .map(|index| {
            json!({
                "kind": "tool",
                "id": format!("tool.{index}"),
                "label": "Tool"
            })
        })
        .collect::<Vec<_>>();
    assert!(serde_json::from_value::<TrustedCatalogEntry>(json!({
        "id": "extension",
        "label": "Extension",
        "kind": "extension",
        "enabled": true,
        "contributions": too_many_contributions
    }))
    .is_err());
}
