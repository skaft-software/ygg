#![allow(missing_docs)]

use std::path::PathBuf;

use ygg_agent::{
    Agent, AgentConfig, CoreTools, EditTool, EntryValue, ExecTool, ExtensionHost, ReadTool,
    SearchTool, Session, Tool,
};
use ygg_ai::{AiClient, Model, ModelCatalog, ModelId, ReasoningConfig, ToolDef};

use crate::app::App;
use crate::config::{Config, ResumeSelector};
use crate::session_store::SessionStore;

/// Inputs needed to resolve a launch without constructing an Agent or a TUI.
pub struct Bootstrap {
    pub config: Config,
    pub catalog: ModelCatalog,
    pub sessions: SessionStore,
    pub client: AiClient,
}

/// Selected persistent session operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionSelection {
    OpenExisting(PathBuf),
    CreateNew(PathBuf),
}

/// Resolved model and session for one launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSelection {
    pub model: ModelId,
    pub session: SessionSelection,
}

/// Build bootstrap state from resolved configuration.
pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap> {
    let catalog = ModelCatalog::builtin()?;
    let sessions = SessionStore::new(&config.session_dir, &config.workspace);
    let client = AiClient::new();
    Ok(Bootstrap {
        config,
        catalog,
        sessions,
        client,
    })
}

/// Resolve a print launch without opening an interactive picker.
pub fn resolve_launch_print(boot: &Bootstrap, stamp: &str) -> anyhow::Result<LaunchSelection> {
    let model = boot.config.model.clone().ok_or_else(|| {
        let mut models = boot
            .catalog
            .models()
            .map(|model| model.id.0.clone())
            .collect::<Vec<_>>();
        models.sort();
        anyhow::anyhow!(
            "no model configured: pass --model <id> or set model in .ygg/config.toml (available: {})",
            models.join(", ")
        )
    })?;

    let session = match &boot.config.resume {
        ResumeSelector::New => SessionSelection::CreateNew(boot.sessions.new_path(stamp)),
        ResumeSelector::Continue => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => {
            SessionSelection::OpenExisting(boot.sessions.by_id(id)?.path)
        }
        ResumeSelector::Resume(None) => {
            anyhow::bail!("--resume needs a session id in print mode")
        }
    };

    Ok(LaunchSelection { model, session })
}

/// Conservative character-based token estimate used for capacity reserves.
pub fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64 + 3) / 4
}

fn estimate_tool_definition(definition: &ToolDef) -> u64 {
    serde_json::to_string(definition)
        .map(|json| estimate_text_tokens(&json))
        .unwrap_or_default()
}

/// Estimate the reserved serialized size of the four frozen core tool schemas.
/// The schemas themselves remain owned by the concrete tool implementations.
pub fn tool_schema_reserve() -> u64 {
    let definitions: Vec<ToolDef> = vec![
        ReadTool.definition(),
        SearchTool.definition(),
        EditTool.definition(),
        ExecTool.definition(),
    ];
    estimate_text_tokens(&serde_json::to_string(&definitions).unwrap_or_default())
}

/// Construct the owning Agent only after model and session selection complete.
pub fn build_app(boot: Bootstrap, launch: LaunchSelection, system: String) -> anyhow::Result<App> {
    let Bootstrap {
        config,
        catalog,
        sessions,
        client,
    } = boot;
    let model = catalog.resolve(&launch.model)?;
    let session = match launch.session {
        SessionSelection::CreateNew(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Session::create(path)?
        }
        SessionSelection::OpenExisting(path) => Session::open(path)?,
    };

    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let reasoning = config.reasoning.clone();
    let system_tokens = estimate_text_tokens(&system);
    let tool_schema_tokens = tool_schema_reserve();
    let agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
    })?;

    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens,
    })
}

/// Recreate the Agent at an idle boundary. Taking `App` by value guarantees the
/// old Agent and its session file are dropped before a session is reopened.
pub fn rebuild_app(
    app: App,
    new_model: Option<Model>,
    new_reasoning: Option<ReasoningConfig>,
    selection: Option<SessionSelection>,
) -> anyhow::Result<App> {
    let App {
        agent,
        model,
        client,
        mut config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens,
    } = app;
    let current_path = agent.session().path().to_owned();
    drop(agent);

    let model = new_model.unwrap_or(model);
    let reasoning = new_reasoning.unwrap_or(reasoning);
    let mut session = match selection {
        Some(SessionSelection::CreateNew(path)) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            Session::create(path)?
        }
        Some(SessionSelection::OpenExisting(path)) => Session::open(path)?,
        None => Session::open(current_path)?,
    };
    session.append(EntryValue::Config {
        model: Some(model.spec.id.0.clone()),
        reasoning: Some(crate::app::reasoning_label(&reasoning)),
    })?;

    config.model = Some(model.spec.id.clone());
    config.reasoning = reasoning.clone();
    let mut extensions = ExtensionHost::new();
    extensions.load(&CoreTools);
    let agent = Agent::new(AgentConfig {
        client: client.clone(),
        model: model.clone(),
        session,
        system: system.clone(),
        sandbox: config.sandbox.to_sandbox_config(&config.workspace),
        extensions,
        max_turns: config.max_turns,
        reasoning: reasoning.clone(),
    })?;

    Ok(App {
        agent,
        model,
        client,
        config,
        catalog,
        sessions,
        reasoning,
        system,
        system_tokens,
        tool_schema_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompactionPolicy, Mode, ResumeSelector, SandboxPolicy};

    fn config(directory: &std::path::Path, model: Option<&str>) -> Config {
        Config {
            workspace: directory.to_path_buf(),
            invocation_cwd: directory.to_path_buf(),
            model: model.map(|model| ModelId(model.to_owned())),
            reasoning: ReasoningConfig::Off,
            sandbox: SandboxPolicy::default(),
            theme: None,
            session_dir: directory.join("sessions"),
            compaction: CompactionPolicy::default(),
            max_turns: 40,
            show_reasoning_in_print: false,
            initial_prompt: None,
            mode: Mode::Print {
                prompt: "hi".to_owned(),
            },
            resume: ResumeSelector::New,
        }
    }

    #[test]
    fn print_launch_errors_without_model() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), None)).unwrap();
        let error = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap_err();
        assert!(error.to_string().contains("no model configured"));
    }

    #[test]
    fn print_launch_creates_new_session_path_with_model() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), Some("gpt-4o-mini"))).unwrap();
        let launch = resolve_launch_print(&boot, "2026-07-12T00-00-00Z").unwrap();
        assert_eq!(launch.model.0, "gpt-4o-mini");
        assert!(matches!(launch.session, SessionSelection::CreateNew(_)));
    }

    #[test]
    fn tool_schema_reserve_is_positive_and_deterministic() {
        assert!(tool_schema_reserve() > 0);
        assert_eq!(tool_schema_reserve(), tool_schema_reserve());
    }

    fn fresh_app(directory: &std::path::Path) -> App {
        let boot = bootstrap(config(directory, Some("gpt-4o-mini"))).unwrap();
        let launch = resolve_launch_print(&boot, "test-session").unwrap();
        build_app(boot, launch, "system".into()).unwrap()
    }

    #[test]
    fn rebuild_same_session_preserves_history_and_records_provenance() {
        use ygg_ai::{Message, UserMessage, UserPart};

        let directory = tempfile::tempdir().unwrap();
        let mut app = fresh_app(directory.path());
        let entry = app
            .agent
            .session_mut()
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("keep me".into())],
            })))
            .unwrap();
        let app = rebuild_app(app, None, None, None).unwrap();
        assert!(app.agent.session().entry(&entry).is_some());
        assert!(matches!(
            app.agent
                .session()
                .entries()
                .last()
                .map(|entry| &entry.value),
            Some(EntryValue::Config { .. })
        ));
    }

    #[test]
    fn rebuild_new_session_has_empty_context_and_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let app = fresh_app(directory.path());
        let new_path = directory.path().join("new.jsonl");
        let app =
            rebuild_app(app, None, None, Some(SessionSelection::CreateNew(new_path))).unwrap();
        assert!(app.agent.session().context().unwrap().is_empty());
        assert_eq!(app.agent.session().entries().len(), 1);
        assert!(matches!(
            app.agent.session().entries()[0].value,
            EntryValue::Config { .. }
        ));
    }
}
