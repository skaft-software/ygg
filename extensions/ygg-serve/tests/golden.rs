//! Golden JSON contracts consumed by web and native client implementations.

use std::collections::BTreeMap;
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use ygg_serve_backend::{
    ActivityPhase, ActivityPhaseSummary, ActorOwnerState, AttachmentPolicy, AttentionState,
    AuthorityProfile, CatalogCursor, ColorScheme, CommandId, CompletionReview, ContextCategory,
    ContextCategoryTotal, ContextStatus, ContextTotals, ContextUsage, DeviceId, DurableEntryId,
    EventEnvelope, EventPayload, EvidenceCoverage, HostAckDisposition, HostBootstrap,
    HostCapabilities, HostCommand, HostCommandAck, HostCommandEnvelope, HostDescriptor, HostId,
    InputModality, ItemDelta, ItemId, ItemLifecycle, ItemPayload, ModelInputPricing,
    ModelInputPricingTier, ModelSelection, ModelSummary, ProjectCatalog, ProjectId, ProjectSummary,
    PromptInput, ProtocolValidation, PullRequestState, PullRequestSummary, RunId, RunOutcome,
    SessionBranchEntry, SessionBranchEntryKind, SessionBranchGraph, SessionCommand,
    SessionCommandEnvelope, SessionCursor, SessionId, SessionItem, SessionLiveState,
    SessionSnapshot, SessionSummary, ThemeColor, ThemeDensity, ThemeDto, ThemeId, ThemeMotion,
    ThemeOption, ThemeRoleStyle, ThemeSourceClass, ThemeTypography, ToolActivity,
    ToolActivityStatus, ToolKind, TurnId, UsageSnapshot, UserMessageDelivery,
};

fn model_selection() -> ModelSelection {
    ModelSelection {
        provider: "openai".into(),
        model: "gpt-5.6".into(),
        reasoning: "high".into(),
    }
}

fn session_item() -> SessionItem {
    SessionItem {
        id: ItemId::new("item-committed").unwrap(),
        run_id: None,
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        provider_attempt: None,
        lifecycle: ItemLifecycle::Committed,
        durable_entry_id: Some(DurableEntryId::new("entry-42").unwrap()),
        payload: ItemPayload::AssistantMessage {
            text: "Ready.".into(),
        },
    }
}

fn live_control_item() -> SessionItem {
    SessionItem {
        id: ItemId::new("item-live-steer").unwrap(),
        run_id: Some(RunId::new("run-3-1").unwrap()),
        turn_id: Some(TurnId::new("turn-1").unwrap()),
        provider_attempt: None,
        lifecycle: ItemLifecycle::Provisional,
        durable_entry_id: None,
        payload: ItemPayload::UserMessage {
            text: "Change direction".into(),
            attachments: Vec::new(),
            documents: Vec::new(),
            project_files: Vec::new(),
            delivery: Some(UserMessageDelivery::Steer),
            branch_provenance: None,
        },
    }
}

fn snapshot() -> SessionSnapshot {
    SessionSnapshot {
        session_id: SessionId::new("session-demo").unwrap(),
        delegated_parent_session_id: None,
        actor_generation: 3,
        cursor: SessionCursor {
            actor_generation: 3,
            sequence: 42,
        },
        durable_head: Some(DurableEntryId::new("entry-42").unwrap()),
        branches: SessionBranchGraph {
            head: Some(DurableEntryId::new("entry-42").unwrap()),
            entries: vec![SessionBranchEntry {
                entry_id: DurableEntryId::new("entry-42").unwrap(),
                parent_entry_id: None,
                kind: SessionBranchEntryKind::AssistantMessage,
                checkoutable: true,
                label: "Ready.".into(),
            }],
            truncated: false,
        },
        live_state: SessionLiveState::Idle,
        active_run_id: None,
        model: model_selection(),
        authority: AuthorityProfile::Workspace,
        context: ContextUsage {
            usage: UsageSnapshot {
                input_tokens: 120,
                output_tokens: 45,
                context_tokens: 165,
                context_limit: Some(128_000),
            },
            compactions: 1,
            status: ContextStatus {
                current: ContextTotals::try_new(
                    vec![ContextCategoryTotal {
                        category: ContextCategory::Other,
                        tokens: 165,
                    }],
                    165,
                )
                .unwrap(),
                updated_at_ms: 1_721_000_000_042,
                active_compaction: None,
                last_compaction: None,
            },
            run: None,
        },
        items: vec![session_item()],
        extension_presentations: Vec::new(),
        pending_requests: Vec::new(),
        sources: Vec::new(),
        artifacts: Vec::new(),
    }
}

fn theme() -> ThemeOption {
    ThemeOption {
        id: ThemeId::new("theme-37a8eec1ce19687d132fe290").unwrap(),
        theme: ThemeDto {
            name: "Ygg Default".into(),
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
            colors: BTreeMap::from([
                (
                    "accent".into(),
                    ThemeColor::Rgb {
                        red: 22,
                        green: 135,
                        blue: 109,
                    },
                ),
                ("canvas".into(), ThemeColor::Default),
            ]),
            roles: BTreeMap::from([(
                ygg_serve_backend::SemanticRole::new("surface.user").unwrap(),
                ThemeRoleStyle {
                    foreground: Some("accent".into()),
                    background: Some("canvas".into()),
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                },
            )]),
        },
    }
}

fn bootstrap() -> HostBootstrap {
    HostBootstrap {
        protocol: 1,
        host: HostDescriptor {
            id: HostId::new("host-demo").unwrap(),
            name: "Achu's Mac".into(),
        },
        capabilities: HostCapabilities {
            concurrent_sessions: true,
            opaque_resources: true,
            attachments: true,
            attachment_policy: Some(AttachmentPolicy::image_defaults()),
            documents: false,
            trusted_project_files: false,
            project_file_browser: false,
            project_file_write: false,
            transcript_search: false,
            previews: true,
            connected_devices: false,
            session_metadata: true,
            session_branches: true,
            conversation_branching: true,
            session_trash: true,
            session_export: true,
            lan_clients: false,
            terminal: false,
            child_agents: false,
        },
        catalog_cursor: CatalogCursor(7),
        models: vec![ModelSummary {
            id: "gpt-5.6".into(),
            name: "GPT-5.6".into(),
            provider: "openai".into(),
            local: false,
            available: true,
            reasoning: vec!["low".into(), "medium".into(), "high".into()],
            default_reasoning: Some("high".into()),
            input_pricing: Some(ModelInputPricing {
                base_microdollars_per_million_tokens: 2_500_000,
                tiers: vec![ModelInputPricingTier {
                    min_input_tokens: 200_000,
                    microdollars_per_million_tokens: 5_000_000,
                }],
            }),
            input_modalities: vec![InputModality::Text, InputModality::Image],
        }],
        authority_profiles: vec![AuthorityProfile::ReadOnly, AuthorityProfile::Workspace],
        authority_ceiling: AuthorityProfile::Workspace,
        themes: vec![theme()],
        selected_theme_id: ThemeId::new("theme-37a8eec1ce19687d132fe290").unwrap(),
        projects: vec![ProjectSummary {
            id: ProjectId::new("project-ygg").unwrap(),
            name: "ygg".into(),
            trusted: true,
            archived: false,
            available: true,
            is_default: true,
            session_count: 1,
            live_session_count: 1,
        }],
        sessions: vec![SessionSummary {
            id: SessionId::new("session-demo").unwrap(),
            project_id: Some(ProjectId::new("project-ygg").unwrap()),
            title: "New session".into(),
            tags: Vec::new(),
            created_at_ms: 1_721_000_000_000,
            modified_at_ms: 1_721_000_000_042,
            pinned: false,
            archived: false,
            lifecycle: ygg_serve_backend::SessionCatalogState::Active,
            retention: None,
            forked_from: None,
            provisional: false,
            live_state: SessionLiveState::Idle,
            attention: AttentionState::None,
            pull_request: Some(PullRequestSummary {
                state: PullRequestState::Ready,
            }),
            owner: ActorOwnerState::Hosted,
            model: model_selection(),
        }],
        selected_session_id: Some(SessionId::new("session-demo").unwrap()),
        selected_session: Some(snapshot()),
    }
}

fn event() -> EventEnvelope {
    EventEnvelope::new(
        SessionId::new("session-demo").unwrap(),
        SessionCursor {
            actor_generation: 3,
            sequence: 43,
        },
        1_721_000_000_043,
        EventPayload::ItemDelta {
            item_id: ItemId::new("item-stream").unwrap(),
            delta: ItemDelta::AssistantText {
                append: " world".into(),
            },
        },
    )
}

fn semantic_tool_event() -> EventEnvelope {
    EventEnvelope::new(
        SessionId::new("session-demo").unwrap(),
        SessionCursor {
            actor_generation: 3,
            sequence: 44,
        },
        1_721_000_000_350,
        EventPayload::ItemDelta {
            item_id: ItemId::new("item-tool-cargo-test").unwrap(),
            delta: ItemDelta::ToolActivity {
                activity: ToolActivity {
                    raw_tool_name: "bash".into(),
                    kind: ToolKind::Command,
                    phase: ActivityPhase::Verified,
                    status: ToolActivityStatus::Succeeded,
                    title: "Run cargo test".into(),
                    summary: Some("Completed".into()),
                    target: None,
                    cwd: Some(".".into()),
                    command_preview: Some("cargo test".into()),
                    exit_code: Some(0),
                    signal: None,
                    started_at_ms: 1_721_000_000_100,
                    completed_at_ms: Some(1_721_000_000_350),
                    duration_ms: Some(250),
                    output_summary: Some("Verification completed".into()),
                    output_handle: None,
                    observed_output_bytes: 4_096,
                    dropped_output_bytes: 128,
                    changed_paths: Vec::new(),
                    source_ids: Vec::new(),
                    artifact_ids: Vec::new(),
                },
            },
        },
    )
}

fn completion_review_item() -> SessionItem {
    SessionItem {
        id: ItemId::new("item-run-outcome").unwrap(),
        run_id: Some(RunId::new("run-stable-1").unwrap()),
        turn_id: Some(TurnId::new("turn-stable-2").unwrap()),
        provider_attempt: None,
        lifecycle: ItemLifecycle::Committed,
        durable_entry_id: Some(DurableEntryId::new("entry-run-outcome").unwrap()),
        payload: ItemPayload::RunOutcome {
            outcome: RunOutcome::Completed,
            message: None,
            review: CompletionReview {
                summary:
                    "Completed 2 actions, 1 changed file, 1 verification, 0 failures, 0 warnings, and 1 output."
                        .into(),
                duration_ms: 1_250,
                action_count: 2,
                phases: vec![
                    ActivityPhaseSummary {
                        phase: ActivityPhase::Changed,
                        action_count: 1,
                        succeeded_count: 1,
                        failed_count: 0,
                        stopped_count: 0,
                    },
                    ActivityPhaseSummary {
                        phase: ActivityPhase::Verified,
                        action_count: 1,
                        succeeded_count: 1,
                        failed_count: 0,
                        stopped_count: 0,
                    },
                ],
                changed_file_item_ids: vec![ItemId::new("item-file-change").unwrap()],
                verification_action_item_ids: vec![
                    ItemId::new("item-tool-cargo-test").unwrap(),
                ],
                failed_action_item_ids: Vec::new(),
                warning_action_item_ids: Vec::new(),
                source_ids: Vec::new(),
                output_ids: vec![
                    ygg_serve_backend::ArtifactId::new("artifact-report").unwrap(),
                ],
                test_results: Vec::new(),
                evidence_coverage: EvidenceCoverage::Partial,
                open_questions: Vec::new(),
            },
        },
    }
}

fn session_command() -> SessionCommandEnvelope {
    SessionCommandEnvelope::new(
        HostId::new("host-demo").unwrap(),
        DeviceId::new("device-browser").unwrap(),
        SessionId::new("session-demo").unwrap(),
        CommandId::new("command-submit").unwrap(),
        1_721_000_000_050,
        Some(3),
        SessionCommand::SubmitPrompt {
            input: PromptInput {
                text: "Review this image".into(),
                attachments: vec![ygg_serve_backend::AttachmentRef {
                    handle: "upload:image-1".into(),
                    display_name: "alignment.png".into(),
                    media_type: "image/png".into(),
                    byte_len: 98_765,
                }],
                document_ids: Vec::new(),
                project_file_ids: Vec::new(),
            },
        },
    )
}

fn host_command() -> HostCommandEnvelope {
    HostCommandEnvelope::new(
        HostId::new("host-demo").unwrap(),
        DeviceId::new("device-browser").unwrap(),
        CommandId::new("command-create").unwrap(),
        1_721_000_000_060,
        HostCommand::CreateSession {
            project_id: Some(ProjectId::new("project-ygg").unwrap()),
            authority: AuthorityProfile::FullAccess,
            model: Some(model_selection()),
        },
    )
}

fn host_ack() -> HostCommandAck {
    HostCommandAck {
        protocol: 1,
        host_id: HostId::new("host-demo").unwrap(),
        command_id: CommandId::new("command-create").unwrap(),
        acknowledged_at_ms: 1_721_000_000_061,
        catalog_cursor: CatalogCursor(8),
        disposition: HostAckDisposition::Accepted {
            created_session_id: Some(SessionId::new("session-created").unwrap()),
            project: None,
            catalog_changed: false,
        },
    }
}

fn project_catalog() -> ProjectCatalog {
    ProjectCatalog {
        protocol: 1,
        host: HostDescriptor {
            id: HostId::new("host-demo").unwrap(),
            name: "Local Ygg".into(),
        },
        catalog_cursor: CatalogCursor(9),
        lifecycle_mutations_supported: true,
        import_supported: false,
        projects: vec![ProjectSummary {
            id: ProjectId::new("project-ygg").unwrap(),
            name: "ygg".into(),
            trusted: true,
            archived: false,
            available: true,
            is_default: true,
            session_count: 1,
            live_session_count: 1,
        }],
    }
}

fn assert_golden<T>(value: T, source: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug + ProtocolValidation,
{
    value.validate().unwrap();
    let expected_value: Value = serde_json::from_str(source).unwrap();
    assert_eq!(serde_json::to_value(&value).unwrap(), expected_value);
    let decoded: T = serde_json::from_value(expected_value).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn host_bootstrap_golden_contract() {
    assert_golden(bootstrap(), include_str!("../fixtures/host-bootstrap.json"));
}

#[test]
fn session_snapshot_golden_contract() {
    assert_golden(
        snapshot(),
        include_str!("../fixtures/session-snapshot.json"),
    );
}

#[test]
fn live_user_delivery_golden_contract() {
    assert_golden(
        live_control_item(),
        include_str!("../fixtures/live-user-delivery.json"),
    );
}

#[test]
fn dotted_event_golden_contract() {
    assert_golden(event(), include_str!("../fixtures/event-envelope.json"));
}

#[test]
fn semantic_tool_event_golden_contract() {
    assert_golden(
        semantic_tool_event(),
        include_str!("../fixtures/semantic-tool-event.json"),
    );
}

#[test]
fn completion_review_golden_contract() {
    assert_golden(
        completion_review_item(),
        include_str!("../fixtures/completion-review-item.json"),
    );
}

#[test]
fn session_command_golden_contract() {
    assert_golden(
        session_command(),
        include_str!("../fixtures/session-command.json"),
    );
}

#[test]
fn host_command_and_ack_golden_contract() {
    assert_golden(
        host_command(),
        include_str!("../fixtures/host-command.json"),
    );
    assert_golden(
        host_ack(),
        include_str!("../fixtures/host-command-ack.json"),
    );
}

#[test]
fn project_catalog_golden_contract() {
    assert_golden(
        project_catalog(),
        include_str!("../fixtures/project-catalog.json"),
    );
}
