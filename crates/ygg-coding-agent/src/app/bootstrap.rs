#![allow(missing_docs)]

use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::EventStream;
use ygg_agent::{
    Agent, AgentConfig, CoreTools, EditTool, EntryValue, ExecTool, ExtensionHost, ReadTool,
    SearchTool, Session, Tool,
};
use ygg_ai::{
    AiClient, Auth, Capabilities, Endpoint, EndpointId, ModalitySet, Model, ModelCatalog, ModelId,
    ModelLimits, ModelSpec, OpenAiChatReasoningMode, Protocol, ReasoningCapability,
    ReasoningConfig, ReasoningControl, ToolDef,
};

use crate::app::{level_from_reasoning, normalize_reasoning_for_model, thinking_to_reasoning, App};
use crate::config::{Config, ResumeSelector};
use crate::session_store::SessionStore;
use crate::tui::pickers::{model_picker, session_picker};
use crate::tui::view::InteractiveShell;

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

const DEEPSEEK_ENDPOINT_ID: &str = "deepseek";
const DEEPSEEK_MODEL_ID: &str = "deepseek-v4-pro";
const DEEPSEEK_DEFAULT_BASE_URL: &str = "https://api.deepseek.com/v1/";
const DEEPSEEK_DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
// Only a local capacity reserve; it never sends a provider max-output value.
const DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS: u64 = 32_768;

fn deepseek_base_url() -> anyhow::Result<url::Url> {
    let configured = std::env::var("YGG_DEEPSEEK_BASE_URL")
        .unwrap_or_else(|_| DEEPSEEK_DEFAULT_BASE_URL.to_owned());
    let normalized = if configured.ends_with('/') {
        configured
    } else {
        format!("{configured}/")
    };
    url::Url::parse(&normalized)
        .map_err(|error| anyhow::anyhow!("invalid YGG_DEEPSEEK_BASE_URL: {error}"))
}

fn deepseek_limit(name: &str, default: u64) -> anyhow::Result<u64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|error| anyhow::anyhow!("invalid {name}={value:?}: {error}")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(anyhow::anyhow!("could not read {name}: {error}")),
    }
}

fn register_deepseek_v4_pro(catalog: &mut ModelCatalog) -> anyhow::Result<()> {
    let endpoint = Endpoint {
        id: EndpointId(DEEPSEEK_ENDPOINT_ID.into()),
        base_url: deepseek_base_url()?,
        auth: Auth::bearer_env("DEEPSEEK_API_KEY"),
        default_headers: http::HeaderMap::new(),
        timeout: Duration::from_secs(120),
    };
    catalog.register_endpoint(endpoint)?;
    let api_name =
        std::env::var("YGG_DEEPSEEK_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL_ID.to_owned());
    let context_window = deepseek_limit(
        "YGG_DEEPSEEK_CONTEXT_WINDOW",
        DEEPSEEK_DEFAULT_CONTEXT_WINDOW,
    )?;
    let max_output_tokens = deepseek_limit(
        "YGG_DEEPSEEK_MAX_OUTPUT_TOKENS",
        DEEPSEEK_DEFAULT_MAX_OUTPUT_TOKENS,
    )?;
    if max_output_tokens > context_window {
        anyhow::bail!("YGG_DEEPSEEK_MAX_OUTPUT_TOKENS must not exceed YGG_DEEPSEEK_CONTEXT_WINDOW");
    }
    catalog.register_model(ModelSpec {
        id: ModelId(DEEPSEEK_MODEL_ID.into()),
        endpoint: EndpointId(DEEPSEEK_ENDPOINT_ID.into()),
        api_name,
        protocol: Protocol::OpenAiChat,
        capabilities: Capabilities {
            input_modalities: ModalitySet::none(),
            output_modalities: ModalitySet::none(),
            tools: true,
            parallel_tool_calls: false,
            // DeepSeek v4 Pro's OpenAI-compatible API supports an explicit
            // `thinking` toggle plus `reasoning_effort`; the mode also tells the
            // Chat codec to replay `reasoning_content` after tool calls.
            reasoning: Some(ReasoningCapability {
                control: ReasoningControl::Effort,
                exposes_text: true,
                preserves_state: false,
                effort_budgets: None,
                openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
            }),
            structured_output: false,
        },
        // DeepSeek v4 Pro advertises a 1M-token context window. These values
        // drive only Ygg's local capacity gate and are configurable by env.
        limits: ModelLimits {
            context_window,
            max_output_tokens,
        },
        pricing: None,
        cache: ygg_ai::CacheCompatibility::default(),
    })?;
    Ok(())
}

// Codex models advertise a 272k context window with 128k max output (per the
// GPT-5.4/5.5 family). These bound only Ygg's local capacity gate.
const CODEX_CONTEXT_WINDOW: u64 = 272_000;
const CODEX_MAX_OUTPUT_TOKENS: u64 = 128_000;

fn codex_user_agent() -> String {
    format!(
        "ygg/{} ({})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS
    )
}

/// Register the OpenAI Codex (Sign in with ChatGPT) endpoint and the models
/// supported by Ygg's SSE transport, but only for a validated subscription
/// credential. The Codex-specific headers are injected via static endpoint
/// headers plus the resolver's dynamic `extra_headers`.
fn register_openai_codex(
    catalog: &mut ModelCatalog,
    store: crate::auth::codex::CredentialStore,
) -> anyhow::Result<()> {
    use crate::auth::codex;

    if !codex::has_usable_credential(&store)? {
        return Ok(());
    }
    let resolver = std::sync::Arc::new(codex::CodexResolver::new(store));

    let mut default_headers = http::HeaderMap::new();
    default_headers.insert(
        http::HeaderName::from_static("openai-beta"),
        http::HeaderValue::from_static("responses=experimental"),
    );
    default_headers.insert(
        http::HeaderName::from_static("originator"),
        http::HeaderValue::from_static(codex::ORIGINATOR),
    );
    default_headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_str(&codex_user_agent())?,
    );

    catalog.register_endpoint(Endpoint {
        id: EndpointId(codex::ENDPOINT_ID.into()),
        base_url: url::Url::parse(codex::BACKEND_BASE_URL)?,
        auth: Auth::dynamic(resolver),
        default_headers,
        timeout: Duration::from_secs(120),
    })?;

    for &model_id in codex::MODELS {
        catalog.register_model(ModelSpec {
            id: ModelId(model_id.into()),
            endpoint: EndpointId(codex::ENDPOINT_ID.into()),
            api_name: model_id.into(),
            protocol: Protocol::OpenAiResponses,
            capabilities: Capabilities {
                input_modalities: ModalitySet::none(),
                output_modalities: ModalitySet::none(),
                tools: true,
                parallel_tool_calls: true,
                reasoning: Some(ReasoningCapability {
                    control: ReasoningControl::Effort,
                    exposes_text: true,
                    preserves_state: true,
                    effort_budgets: None,
                    openai_chat_mode: OpenAiChatReasoningMode::Standard,
                }),
                structured_output: false,
            },
            limits: ModelLimits {
                context_window: CODEX_CONTEXT_WINDOW,
                max_output_tokens: CODEX_MAX_OUTPUT_TOKENS,
            },
            pricing: None,
            cache: ygg_ai::CacheCompatibility::default(),
        })?;
    }
    Ok(())
}

fn base_model_catalog() -> anyhow::Result<ModelCatalog> {
    let mut catalog = ModelCatalog::builtin()?;
    register_deepseek_v4_pro(&mut catalog)?;
    Ok(catalog)
}

/// Build the runtime model catalog, exposing ChatGPT subscription models only
/// when Ygg owns a usable OAuth credential.
pub fn model_catalog() -> anyhow::Result<ModelCatalog> {
    let mut catalog = base_model_catalog()?;
    let store = crate::auth::codex::CredentialStore::new(crate::auth::codex::default_path());
    // Non-fatal: a stale or malformed OAuth file must never block Ygg startup.
    if let Err(error) = register_openai_codex(&mut catalog, store) {
        eprintln!("warning: OpenAI Codex models unavailable: {error}");
    }
    Ok(catalog)
}

/// Build the catalog without subscription models, used to make `/logout`
/// atomic when the active model itself belongs to ChatGPT.
pub fn model_catalog_without_codex() -> anyhow::Result<ModelCatalog> {
    base_model_catalog()
}

/// Build bootstrap state from resolved configuration.
pub fn bootstrap(config: Config) -> anyhow::Result<Bootstrap> {
    let catalog = model_catalog()?;
    let sessions = SessionStore::new(&config.session_dir, &config.workspace);
    let client = AiClient::try_new()?;
    Ok(Bootstrap {
        config,
        catalog,
        sessions,
        client,
    })
}

/// Resolve model configuration precedence. The caller supplies values from
/// distinct configuration layers; explicit CLI selection always wins.
pub fn resolve_model_id(
    cli: Option<ModelId>,
    project: Option<ModelId>,
    global: Option<ModelId>,
) -> Option<ModelId> {
    cli.or(project).or(global)
}

/// Resolve an interactive launch and open pickers only while no Agent exists.
pub async fn resolve_launch_interactive(
    boot: &Bootstrap,
    shell: &mut InteractiveShell,
    input: &mut EventStream,
) -> anyhow::Result<LaunchSelection> {
    let model = match boot.config.model.clone() {
        Some(model) => model,
        None => model_picker(shell, input, &boot.catalog).await?,
    };
    let session = match &boot.config.resume {
        ResumeSelector::New => {
            SessionSelection::CreateNew(boot.sessions.new_path(&crate::modes::timestamp()))
        }
        ResumeSelector::Continue => SessionSelection::OpenExisting(boot.sessions.latest()?.path),
        ResumeSelector::Resume(Some(id)) => {
            SessionSelection::OpenExisting(boot.sessions.by_id(id)?.path)
        }
        ResumeSelector::Resume(None) => session_picker(shell, input, &boot.sessions)
            .await?
            .map(SessionSelection::OpenExisting)
            .ok_or_else(|| anyhow::anyhow!("session selection cancelled"))?,
    };
    Ok(LaunchSelection { model, session })
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
    (text.len() as u64).div_ceil(4)
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
        mut config,
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
    let reasoning = normalize_reasoning_for_model(&config.reasoning, &model)?;
    config.reasoning = reasoning.clone();
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
        cache_retention: ygg_ai::CacheRetention::default(),
        session_id: None,
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

    let changing_model = new_model.is_some();
    let old_model = model;
    let model = new_model.unwrap_or_else(|| old_model.clone());
    let reasoning = match new_reasoning {
        Some(reasoning) => normalize_reasoning_for_model(&reasoning, &model)?,
        None if changing_model => {
            let level = level_from_reasoning(&reasoning, &old_model)?;
            thinking_to_reasoning(level, &model)?
        }
        None => normalize_reasoning_for_model(&reasoning, &model)?,
    };
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
        cache_retention: ygg_ai::CacheRetention::default(),
        session_id: None,
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
    fn model_resolution_has_cli_project_global_precedence() {
        let id = |value: &str| Some(ModelId(value.into()));
        assert_eq!(
            resolve_model_id(id("cli"), id("project"), id("global")),
            id("cli")
        );
        assert_eq!(
            resolve_model_id(None, id("project"), id("global")),
            id("project")
        );
        assert_eq!(resolve_model_id(None, None, id("global")), id("global"));
        assert_eq!(resolve_model_id(None, None, None), None);
    }

    fn write_codex_credential(path: &std::path::Path, localhost: bool) {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let payload = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct_test",
                "localhost": localhost
            }
        });
        let access = format!(
            "h.{}.s",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(
            path,
            serde_json::to_vec(&serde_json::json!({
                "tokens": {
                    "access_token": access,
                    "refresh_token": "refresh",
                    "account_id": "acct_test"
                },
                "expires_at": u64::MAX
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn codex_models_require_a_usable_credential_and_match_sse_support() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("codex.json");
        let store = crate::auth::codex::CredentialStore::new(&path);

        let mut catalog = base_model_catalog().unwrap();
        register_openai_codex(&mut catalog, store.clone()).unwrap();
        assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

        write_codex_credential(&path, true);
        let mut catalog = base_model_catalog().unwrap();
        let error = register_openai_codex(&mut catalog, store.clone()).unwrap_err();
        assert!(error.to_string().contains("localhost-only"));
        assert!(catalog.resolve(&ModelId("gpt-5.6-sol".into())).is_err());

        write_codex_credential(&path, false);
        let mut catalog = base_model_catalog().unwrap();
        register_openai_codex(&mut catalog, store).unwrap();
        for model_id in crate::auth::codex::MODELS {
            let model = catalog.resolve(&ModelId((*model_id).into())).unwrap();
            assert_eq!(model.endpoint.id.0, crate::auth::codex::ENDPOINT_ID);
            assert_eq!(model.spec.protocol, Protocol::OpenAiResponses);
        }
        // These were the two misleading entries in the original integration:
        // Pro is not in the subscription catalog and Luna currently needs WS.
        assert!(catalog.resolve(&ModelId("gpt-5.5-pro".into())).is_err());
        assert!(catalog.resolve(&ModelId("gpt-5.6-luna".into())).is_err());
    }

    #[test]
    fn deepseek_v4_pro_is_registered_as_openai_chat_with_env_auth() {
        let directory = tempfile::tempdir().unwrap();
        let boot = bootstrap(config(directory.path(), Some(DEEPSEEK_MODEL_ID))).unwrap();
        let model = boot
            .catalog
            .resolve(&ModelId(DEEPSEEK_MODEL_ID.into()))
            .unwrap();
        assert_eq!(model.spec.protocol, Protocol::OpenAiChat);
        assert_eq!(model.endpoint.id.0, DEEPSEEK_ENDPOINT_ID);
        assert_eq!(
            model.spec.api_name,
            std::env::var("YGG_DEEPSEEK_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL_ID.into())
        );
        assert!(model.spec.capabilities.tools);
        assert!(matches!(
            model.spec.capabilities.reasoning.as_ref(),
            Some(ReasoningCapability {
                control: ReasoningControl::Effort,
                exposes_text: true,
                openai_chat_mode: OpenAiChatReasoningMode::DeepSeekThinking,
                ..
            })
        ));
        assert_eq!(
            model.spec.limits.context_window,
            std::env::var("YGG_DEEPSEEK_CONTEXT_WINDOW")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEEPSEEK_DEFAULT_CONTEXT_WINDOW)
        );
    }

    #[test]
    fn deepseek_v4_pro_accepts_high_reasoning_at_startup() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = config(directory.path(), Some(DEEPSEEK_MODEL_ID));
        config.reasoning = ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High);
        let boot = bootstrap(config).unwrap();
        let launch = resolve_launch_print(&boot, "test-session").unwrap();
        let app = build_app(boot, launch, "system".into()).unwrap();
        assert_eq!(
            app.reasoning,
            ReasoningConfig::Effort(ygg_ai::ReasoningEffort::High)
        );
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
