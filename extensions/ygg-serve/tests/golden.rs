//! Golden JSON contracts consumed by web and native client implementations.

use std::collections::BTreeMap;
use std::fmt::Debug;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use ygg_serve_backend::{
    ActorOwnerState, AttachmentPolicy, AttentionState, AuthorityProfile, CatalogCursor,
    ColorScheme, CommandId, ContextUsage, DeviceId, DurableEntryId, EventEnvelope, EventPayload,
    HostAckDisposition, HostBootstrap, HostCapabilities, HostCommand, HostCommandAck,
    HostCommandEnvelope, HostDescriptor, HostId, InputModality, ItemDelta, ItemId, ItemLifecycle,
    ItemPayload, ModelSelection, ModelSummary, ProjectId, ProjectSummary, PromptInput,
    ProtocolValidation, SessionCommand, SessionCommandEnvelope, SessionCursor, SessionId,
    SessionItem, SessionLiveState, SessionSnapshot, SessionSummary, ThemeColor, ThemeDensity,
    ThemeDto, ThemeId, ThemeMotion, ThemeOption, ThemeRoleStyle, ThemeSourceClass, ThemeTypography,
    TurnId, UsageSnapshot,
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

fn snapshot() -> SessionSnapshot {
    SessionSnapshot {
        session_id: SessionId::new("session-demo").unwrap(),
        actor_generation: 3,
        cursor: SessionCursor {
            actor_generation: 3,
            sequence: 42,
        },
        durable_head: Some(DurableEntryId::new("entry-42").unwrap()),
        live_state: SessionLiveState::Idle,
        active_run_id: None,
        model: model_selection(),
        authority: AuthorityProfile::FullAccess,
        context: ContextUsage {
            usage: UsageSnapshot {
                input_tokens: 120,
                output_tokens: 45,
                context_tokens: 165,
                context_limit: Some(128_000),
            },
            compactions: 1,
        },
        items: vec![session_item()],
        pending_requests: Vec::new(),
        sources: Vec::new(),
        artifacts: Vec::new(),
    }
}

fn theme() -> ThemeOption {
    ThemeOption {
        id: ThemeId::new("tidepool").unwrap(),
        theme: ThemeDto {
            name: "Tidepool".into(),
            source: ThemeSourceClass::Bundled,
            revision: 3,
            scheme: ColorScheme::Dark,
            density: ThemeDensity::Airy,
            motion: ThemeMotion::Full,
            typography: ThemeTypography {
                body_family: "system-sans".into(),
                mono_family: "system-mono".into(),
                body_size: 17,
                display_ratio_milli: 1235,
            },
            colors: BTreeMap::from([
                (
                    "accent".into(),
                    ThemeColor::Rgb {
                        red: 22,
                        green: 143,
                        blue: 145,
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
            previews: true,
            connected_devices: false,
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
            input_modalities: vec![InputModality::Text, InputModality::Image],
        }],
        authority_profiles: vec![
            AuthorityProfile::ReadOnly,
            AuthorityProfile::Workspace,
            AuthorityProfile::FullAccess,
        ],
        authority_ceiling: AuthorityProfile::FullAccess,
        themes: vec![theme()],
        selected_theme_id: ThemeId::new("tidepool").unwrap(),
        projects: vec![ProjectSummary {
            id: ProjectId::new("project-ygg").unwrap(),
            name: "ygg".into(),
            trusted: true,
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
            provisional: false,
            live_state: SessionLiveState::Idle,
            attention: AttentionState::None,
            owner: ActorOwnerState::Hosted,
            model: model_selection(),
        }],
        selected_session_id: SessionId::new("session-demo").unwrap(),
        selected_session: snapshot(),
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
            created_session_id: SessionId::new("session-created").unwrap(),
        },
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
fn dotted_event_golden_contract() {
    assert_golden(event(), include_str!("../fixtures/event-envelope.json"));
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
