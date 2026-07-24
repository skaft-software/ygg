#![allow(missing_docs)]

use crate::app::{reasoning_label, App, Reconfig};
use crate::compaction::{context_window, estimate_next_request_tokens};
use crate::presentation::{format_token_rate, ModelDisplayMetadata};
use crate::session_store::active_branch_title;
use ygg_agent::{
    analyze_session_cache, analyze_session_cache_stats, CacheStats, EntryValue, Session,
    UsageRecordKind,
};
use ygg_ai::{AssistantPart, Cost, Message, Model, Usage};

/// Parsed in-TUI command. Commands are deliberately separate from shell CLI
/// options: only editor text beginning with `/` enters this grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Login(Option<String>),
    Logout(Option<String>),
    Model(Option<String>),
    CycleModel,
    Thinking(Option<String>),
    Theme(Option<String>),
    /// Toggle details for one tool call, or the latest call when omitted.
    Tool(Option<String>),
    Verbose(Option<bool>),
    Compact,
    AutoCompact(Option<AutoCompactSetting>),
    Reload,
    New,
    Resume(Option<String>),
    Tree,
    Checkout(String),
    Status,
    Context,
    Cost,
    Cache,
    Update,
    Name(Option<String>),
    Sessions,
    Export(Option<String>),
    Quit,
    /// List or invoke named prompt templates. The optional string preserves
    /// the template name and raw arguments for deterministic expansion.
    Prompt(Option<String>),
    Skills(SkillsSubcommand),
    /// Inspect or reload explicitly enabled executable extensions.
    Extensions(ExtensionsSubcommand),
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoCompactSetting {
    Enabled(bool),
    ThresholdPercent(u8),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionsSubcommand {
    List,
    Reload,
}

/// Subcommands for the `/skills` slash command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillsSubcommand {
    /// List all discovered skills.
    List,
    /// Show details about a specific skill.
    Show(String),
    /// List all currently active skills.
    Active,
    /// Search discovered skill metadata.
    Search(String),
    /// Explicitly load and activate a skill.
    Load(String),
    /// Rescan all configured skill roots.
    Reload,
    /// Explicitly deactivate a skill.
    Off(String),
}

/// One command shown in the prompt's live slash-command suggestions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashCommandSuggestion {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
    pub(crate) accepts_argument: bool,
}

macro_rules! slash {
    ($name:literal, $usage:literal, $description:literal, $accepts:literal) => {
        SlashCommandSuggestion {
            name: $name,
            usage: $usage,
            description: $description,
            accepts_argument: $accepts,
        }
    };
}

// The popup is the command-discovery surface, so this stays a single flat list:
// no one-off `Session` header and no `/help` entry that merely repeats it.
const SLASH_COMMANDS: &[SlashCommandSuggestion] = &[
    slash!("new", "/new", "start a fresh conversation", false),
    slash!(
        "resume",
        "/resume [id]",
        "re-open or list recent sessions",
        true
    ),
    slash!("tree", "/tree", "show the conversation branch tree", false),
    slash!(
        "checkout",
        "/checkout <id>",
        "switch to a different branch",
        true
    ),
    slash!("model", "/model [id]", "select or change the model", true),
    slash!(
        "cycle-model",
        "/cycle-model",
        "switch to the next available model",
        false
    ),
    slash!(
        "thinking",
        "/thinking [level]",
        "set reasoning effort",
        true
    ),
    slash!("compact", "/compact", "compact conversation context", false),
    slash!(
        "auto-compact",
        "/auto-compact [on|off|85%]",
        "show or configure automatic compaction",
        true
    ),
    slash!(
        "theme",
        "/theme [name|list|reload]",
        "select, list, or reload themes",
        true
    ),
    slash!(
        "verbose",
        "/verbose [on|off]",
        "show or hide raw tool details",
        true
    ),
    slash!(
        "tool",
        "/tool [call-id]",
        "toggle details for one tool call",
        true
    ),
    slash!(
        "reload",
        "/reload",
        "reload instructions, themes, prompts, and skills",
        false
    ),
    slash!("login", "/login [provider]", "sign in to a provider", true),
    slash!(
        "logout",
        "/logout [provider]",
        "remove stored credentials",
        true
    ),
    slash!("status", "/status", "show model and diagnostics", false),
    slash!(
        "context",
        "/context",
        "show what occupies the model context",
        false
    ),
    slash!("cost", "/cost", "show turn and session cost", false),
    slash!("cache", "/cache", "show prompt-cache diagnostics", false),
    slash!("update", "/update", "check for a newer Ygg release", false),
    slash!("name", "/name [name]", "show or rename this session", true),
    slash!("sessions", "/sessions", "list local sessions", false),
    slash!(
        "export",
        "/export [path]",
        "export this session with secret redaction",
        true
    ),
    slash!(
        "prompt",
        "/prompt [name] [arguments]",
        "list or expand prompt templates",
        true
    ),
    slash!(
        "skills",
        "/skills [subcommand]",
        "manage and view agent skills",
        true
    ),
    slash!(
        "extensions",
        "/extensions [reload]",
        "inspect or reload executable extensions",
        true
    ),
    slash!("quit", "/quit", "exit Ygg", false),
];

/// Suggestions for an editor value while its first token is a slash command.
pub fn slash_suggestions(input: &str) -> Vec<&'static SlashCommandSuggestion> {
    let Some(query) = input.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) || query.contains('\n') {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(query))
        .collect()
}

/// Complete a unique command-name prefix. Argument-taking commands receive a
/// trailing space so the next keystroke naturally begins their argument.
#[cfg(test)]
pub fn complete_slash_command(input: &str) -> Option<String> {
    let suggestions = slash_suggestions(input);
    let [command] = suggestions.as_slice() else {
        return None;
    };
    Some(format!(
        "/{}{}",
        command.name,
        if command.accepts_argument { " " } else { "" }
    ))
}

/// Parse a slash command without interpreting models, paths, or capabilities.
pub fn parse(input: &str) -> Command {
    let input = input.trim();
    let Some(body) = input.strip_prefix('/') else {
        return Command::Unknown(input.to_owned());
    };
    let mut parts = body.split_whitespace();
    let name = parts.next().unwrap_or_default();

    let matches: Vec<_> = SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(name))
        .collect();
    let full_name = if SLASH_COMMANDS.iter().any(|command| command.name == name) {
        name
    } else if let [command] = matches.as_slice() {
        command.name
    } else {
        return Command::Unknown(input.to_owned());
    };

    // Resolve the command name before parsing the variable-arity `/skills`
    // arguments, so a future command sharing its prefix remains ambiguous.
    if full_name == "skills" {
        let args: Vec<&str> = parts.collect();
        let sub = match args.as_slice() {
            [] => SkillsSubcommand::List,
            ["active"] => SkillsSubcommand::Active,
            ["show", id] => SkillsSubcommand::Show(id.to_string()),
            ["search", query @ ..] if !query.is_empty() => {
                SkillsSubcommand::Search(query.join(" "))
            }
            ["load", id] | ["reload", id] => SkillsSubcommand::Load(id.to_string()),
            ["reload"] => SkillsSubcommand::Reload,
            ["off", id] | ["unload", id] => SkillsSubcommand::Off(id.to_string()),
            _ => return Command::Unknown(input.to_owned()),
        };
        return Command::Skills(sub);
    }

    if full_name == "prompt" {
        let argument = body[name.len()..].trim();
        return Command::Prompt((!argument.is_empty()).then(|| argument.to_owned()));
    }

    if full_name == "name" || full_name == "export" {
        let argument = body[name.len()..].trim();
        let argument = (!argument.is_empty()).then(|| argument.to_owned());
        return if full_name == "name" {
            Command::Name(argument)
        } else {
            Command::Export(argument)
        };
    }

    if full_name == "extensions" {
        let args = parts.collect::<Vec<_>>();
        return match args.as_slice() {
            [] => Command::Extensions(ExtensionsSubcommand::List),
            ["reload"] => Command::Extensions(ExtensionsSubcommand::Reload),
            _ => Command::Unknown(input.to_owned()),
        };
    }

    let argument = parts.next().map(str::to_owned);
    if parts.next().is_some() {
        return Command::Unknown(input.to_owned());
    }

    match full_name {
        "login" => Command::Login(argument),
        "logout" => Command::Logout(argument),
        "model" => Command::Model(argument),
        "cycle-model" if argument.is_none() => Command::CycleModel,
        "thinking" => Command::Thinking(argument),
        "theme" => Command::Theme(argument),
        "tool" => Command::Tool(argument),
        "verbose" => match argument.as_deref() {
            None => Command::Verbose(None),
            Some("on" | "true" | "yes") => Command::Verbose(Some(true)),
            Some("off" | "false" | "no") => Command::Verbose(Some(false)),
            Some(_) => Command::Unknown(input.to_owned()),
        },
        "compact" if argument.is_none() => Command::Compact,
        "auto-compact" => match argument.as_deref() {
            None => Command::AutoCompact(None),
            Some("on" | "true" | "yes") => {
                Command::AutoCompact(Some(AutoCompactSetting::Enabled(true)))
            }
            Some("off" | "false" | "no") => {
                Command::AutoCompact(Some(AutoCompactSetting::Enabled(false)))
            }
            Some(value) => value
                .strip_suffix('%')
                .and_then(|percent| percent.parse::<u8>().ok())
                .filter(|percent| (1..=100).contains(percent))
                .map(|percent| {
                    Command::AutoCompact(Some(AutoCompactSetting::ThresholdPercent(percent)))
                })
                .unwrap_or_else(|| Command::Unknown(input.to_owned())),
        },
        "reload" if argument.is_none() => Command::Reload,
        "new" if argument.is_none() => Command::New,
        "resume" => Command::Resume(argument),
        "tree" if argument.is_none() => Command::Tree,
        "checkout" => match argument {
            Some(id) => Command::Checkout(id),
            None => Command::Unknown(input.to_owned()),
        },
        "status" if argument.is_none() => Command::Status,
        "context" if argument.is_none() => Command::Context,
        "cost" if argument.is_none() => Command::Cost,
        "cache" if argument.is_none() => Command::Cache,
        "update" if argument.is_none() => Command::Update,
        "sessions" if argument.is_none() => Command::Sessions,
        "quit" if argument.is_none() => Command::Quit,
        _ => Command::Unknown(input.to_owned()),
    }
}

/// Render a capability gate as an explicit enabled/disabled word rather than a
/// bare boolean, so `/status` reads as a security report.
fn gate(enabled: bool) -> &'static str {
    if enabled {
        "enabled"
    } else {
        "disabled"
    }
}

fn path_access(allow_external_paths: bool) -> &'static str {
    if allow_external_paths {
        "current-user paths (absolute, ~/ and relative)"
    } else {
        "workspace-only guard"
    }
}

fn session_activity_counts(session: &ygg_agent::Session) -> (usize, usize) {
    let mut model_turns = 0usize;
    let mut tool_calls = 0usize;
    let mut cursor = session.head();
    while let Some(id) = cursor {
        let Some(entry) = session.entry(&id) else {
            break;
        };
        if let EntryValue::Message(Message::Assistant(message)) = &entry.value {
            model_turns = model_turns.saturating_add(1);
            tool_calls = tool_calls.saturating_add(
                message
                    .content
                    .iter()
                    .filter(|part| matches!(part, AssistantPart::ToolCall(_)))
                    .count(),
            );
        }
        cursor = entry.parent.clone();
    }
    (model_turns, tool_calls)
}

fn token_count(value: u64) -> String {
    if value >= 1_000 {
        let thousands = value as f64 / 1_000.0;
        format!("{thousands:.1}k")
    } else {
        value.to_string()
    }
}

/// Render an exact microdollar amount with enough precision to make small
/// requests visible in a report.
pub fn format_microdollars(value: u64) -> String {
    format!("${}.{:06}", value / 1_000_000, value % 1_000_000)
}

/// Render a spend limit in ordinary dollars, rounded to cents.
pub fn format_microdollars_cents(value: u64) -> String {
    let cents = value.saturating_add(5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Present the active model's base input/output/cache-read rates.
pub fn model_pricing_text(model: &Model) -> String {
    match model.spec.pricing.as_ref() {
        Some(pricing) => format!(
            "{}/{}/{} (input/output/cache-read; cache-write {})",
            format_token_rate(pricing.input),
            format_token_rate(pricing.output),
            format_token_rate(pricing.cache_read),
            format_token_rate(pricing.cache_write_5m),
        ),
        None => "unavailable (no configured rates)".to_owned(),
    }
}

/// Complete model comparison row used by `/model` and the model picker.
pub fn model_selection_text(model: &Model) -> String {
    format!(
        "{} — {} — {} ctx",
        model.spec.id.0,
        model_pricing_text(model),
        token_count(model.spec.limits.context_window),
    )
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(character);
    }
    output
}

fn usage_cost_cell(tokens: u64, cost: Option<u64>) -> String {
    format!(
        "{}/{}",
        grouped(tokens),
        cost.map(|cost| cost.to_string())
            .unwrap_or_else(|| "—".to_owned())
    )
}

fn add_usage(total: &mut Usage, turn: Usage) {
    total.input_tokens = total.input_tokens.saturating_add(turn.input_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(turn.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(turn.cache_write_tokens);
    total.cache_write_1h_tokens = total
        .cache_write_1h_tokens
        .saturating_add(turn.cache_write_1h_tokens);
    total.output_tokens = total.output_tokens.saturating_add(turn.output_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(turn.reasoning_tokens);
    total.total_tokens = total.total_tokens.saturating_add(turn.total_tokens);
}

/// Detailed cumulative spend report. Usage records are durable and therefore
/// this formatter works identically for a live or replayed session.
pub fn cost_text(session: &Session, model: &Model) -> String {
    let records = session.usage_records();
    let turn_count = records
        .iter()
        .filter(|record| matches!(record.kind, UsageRecordKind::AssistantTurn { .. }))
        .count();
    let mut lines = vec![format!(
        "Session cost · {} across {turn_count} turn{}",
        format_microdollars(session.total_cost_microdollars()),
        if turn_count == 1 { "" } else { "s" }
    )];
    if records.is_empty() {
        lines.push("".to_owned());
        lines.push("No completed priced model calls yet.".to_owned());
    } else {
        lines.extend([
            "".to_owned(),
            "  Turn  Model             Input tok/µ$  CacheR tok/µ$  CacheW tok/µ$  Output tok/µ$  Reason tok/µ$  Total µ$"
                .to_owned(),
            "  ────  ────────────────  ───────────  ─────────────  ─────────────  ─────────────  ─────────────  ────────"
                .to_owned(),
        ]);
    }

    let mut assistant_turn = 0usize;
    let mut totals = ygg_ai::Usage::default();
    let mut cost_totals = Cost::default();
    let mut has_priced_record = false;
    for record in records {
        add_usage(&mut totals, record.usage);
        if let Some(cost) = record.cost {
            has_priced_record = true;
            cost_totals.input = cost_totals.input.saturating_add(cost.input);
            cost_totals.cache_read = cost_totals.cache_read.saturating_add(cost.cache_read);
            cost_totals.cache_write = cost_totals.cache_write.saturating_add(cost.cache_write);
            cost_totals.output = cost_totals.output.saturating_add(cost.output);
            cost_totals.reasoning = cost_totals.reasoning.saturating_add(cost.reasoning);
        }
        let turn = match record.kind {
            UsageRecordKind::AssistantTurn { .. } => {
                assistant_turn += 1;
                assistant_turn.to_string()
            }
            UsageRecordKind::Compaction => "cmp".to_owned(),
            UsageRecordKind::TerminalGate { returned } => match returned {
                Some(true) => "gate:R".to_owned(),
                Some(false) => "gate:C".to_owned(),
                None => "gate:?".to_owned(),
            },
        };
        let model_name = record
            .model
            .as_ref()
            .map(|model| model.0.as_str())
            .unwrap_or("unknown");
        let model_name = model_name
            .chars()
            .rev()
            .take(16)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        let output = record
            .usage
            .output_tokens
            .saturating_sub(record.usage.reasoning_tokens);
        let cost = record.cost;
        let total_cost = record
            .cost_microdollars
            .map(|cost| cost.to_string())
            .unwrap_or_else(|| "—".to_owned());
        lines.push(format!(
            "  {turn:<4}  {model_name:<16}  {:>12}  {:>14}  {:>14}  {:>14}  {:>14}  {:>8}",
            usage_cost_cell(record.usage.input_tokens, cost.map(|cost| cost.input)),
            usage_cost_cell(
                record.usage.cache_read_tokens,
                cost.map(|cost| cost.cache_read)
            ),
            usage_cost_cell(
                record.usage.cache_write_tokens,
                cost.map(|cost| cost.cache_write)
            ),
            usage_cost_cell(output, cost.map(|cost| cost.output)),
            usage_cost_cell(
                record.usage.reasoning_tokens,
                cost.map(|cost| cost.reasoning)
            ),
            total_cost,
        ));
    }
    if !records.is_empty() {
        let output = totals.output_tokens.saturating_sub(totals.reasoning_tokens);
        lines.extend([
            "  ────  ────────────────  ──────  ──────  ──────  ──────  ──────  ────────".to_owned(),
            format!(
                "  Total                   {:>12}  {:>14}  {:>14}  {:>14}  {:>14}  {:>8}",
                usage_cost_cell(
                    totals.input_tokens,
                    has_priced_record.then_some(cost_totals.input),
                ),
                usage_cost_cell(
                    totals.cache_read_tokens,
                    has_priced_record.then_some(cost_totals.cache_read),
                ),
                usage_cost_cell(
                    totals.cache_write_tokens,
                    has_priced_record.then_some(cost_totals.cache_write),
                ),
                usage_cost_cell(output, has_priced_record.then_some(cost_totals.output)),
                usage_cost_cell(
                    totals.reasoning_tokens,
                    has_priced_record.then_some(cost_totals.reasoning),
                ),
                format_microdollars(session.total_cost_microdollars()),
            ),
        ]);
    }
    lines.extend([
        "".to_owned(),
        format!("Model: {} · {}", model.spec.id.0, model_pricing_text(model)),
    ]);
    lines.join("\n")
}

/// Prompt-cache effectiveness and material miss report for the active branch.
pub fn cache_text(session: &Session) -> String {
    let (stats, misses) = analyze_session_cache(session);
    let hit_rate = stats.hit_rate_basis_points();
    let model_changes = misses.iter().filter(|miss| miss.model_changed).count();
    let idle_timeouts = misses
        .iter()
        .filter(|miss| miss.idle_past_short_ttl)
        .count();
    let hit_summary = hit_rate
        .map(|basis_points| {
            format!(
                "{basis_points} bp ({:.1}%) across {} analyzable turns",
                f64::from(basis_points) / 100.0,
                stats.assistant_turns
            )
        })
        .unwrap_or_else(|| {
            format!(
                "unavailable (no cache activity observed) across {} turns",
                stats.assistant_turns
            )
        });
    let mut lines = vec![
        format!("Cache effectiveness · {hit_summary}"),
        format!(
            "Reusable prefix: {} tokens · missed: {} tokens · estimated waste: {}",
            grouped(stats.reusable_prefix_tokens),
            grouped(stats.missed_reusable_tokens),
            format_microdollars(stats.missed_cost_microdollars)
        ),
        "".to_owned(),
    ];
    if !misses.is_empty() {
        lines.extend([
            "  Assistant entry    Expected  Cached  Missed  Waste      Cause".to_owned(),
            "  ────────────────  ────────  ──────  ──────  ─────────  ─────────────────".to_owned(),
        ]);
        for miss in misses {
            let mut causes = Vec::new();
            if miss.model_changed {
                causes.push("model changed");
            }
            if miss.idle_past_short_ttl {
                causes.push("idle timeout");
            }
            if causes.is_empty() {
                causes.push("cache miss");
            }
            lines.push(format!(
                "  {:<16}  {:>8}  {:>6}  {:>6}  {:>9}  {}",
                miss.assistant.0,
                grouped(miss.expected_reusable_tokens),
                grouped(miss.cache_read_tokens),
                grouped(miss.missed_reusable_tokens),
                miss.missed_cost_microdollars
                    .map(format_microdollars)
                    .unwrap_or_else(|| "—".to_owned()),
                causes.join(", "),
            ));
        }
        lines.push("".to_owned());
    }
    lines.extend([
        format!(
            "Cumulative reusable prefix: {} tokens",
            grouped(stats.reusable_prefix_tokens)
        ),
        format!(
            "Total cache reads:          {} tokens",
            grouped(stats.cache_read_tokens)
        ),
        format!(
            "Material misses:            {} ({} tokens wasted)",
            stats.miss_count,
            grouped(stats.missed_reusable_tokens)
        ),
        format!(
            "Estimated waste cost:       {}",
            format_microdollars(stats.missed_cost_microdollars)
        ),
        "".to_owned(),
        format!(
            "Diagnostics: {model_changes} model-change misses, {idle_timeouts} idle-timeout misses, {} priced misses",
            stats.priced_miss_count
        ),
    ]);
    lines.join("\n")
}

/// Detailed status text suitable for the `/status` overlay.
///
/// The security block states Ygg's model plainly: it is a trusted local agent,
/// not an OS sandbox. Built-in tools default to the current user's local files;
/// a host can opt into a workspace-only accidental-path guard, but neither mode
/// confines spawned processes.
pub fn status_text(app: &App, queued: Option<&Reconfig>) -> String {
    let context_estimate = estimate_next_request_tokens(app, &[]);
    let cache_stats = analyze_session_cache_stats(app.agent.session());
    status_text_with_metrics(app, queued, context_estimate, &cache_stats)
}

pub(crate) fn status_text_with_metrics(
    app: &App,
    queued: Option<&Reconfig>,
    context_estimate: u64,
    cache_stats: &CacheStats,
) -> String {
    let session = app.agent.session();
    let session_id = session
        .path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("(unknown)");
    let queue = queued
        .map(|item| format!("{item:?}"))
        .unwrap_or_else(|| "none".to_owned());
    let sandbox = &app.config.sandbox;
    let (model_turns, tool_calls) = session_activity_counts(session);
    let active_skills = session
        .head_ref()
        .and_then(|head| session.resolve_active_skills(head).ok())
        .map(|state| state.active_skills.len())
        .unwrap_or(0);
    let discovered_skills = app.skills.descriptors().len();
    let context = token_count(context_estimate);
    let context_window = token_count(context_window(&app.model));
    let display = ModelDisplayMetadata::resolve(&app.model.spec);
    let pricing = model_pricing_text(&app.model);
    let cache_rate = cache_stats
        .hit_rate_basis_points()
        .map(|basis_points| format!("{basis_points} bp"))
        .unwrap_or_else(|| "unavailable".to_owned());
    let cost_limit = app
        .config
        .max_cost_microdollars
        .map(format_microdollars_cents)
        .unwrap_or_else(|| "disabled".to_owned());
    let reasoning = match app.reasoning_mode {
        ygg_ai::ReasoningMode::Standard => reasoning_label(&app.reasoning),
        ygg_ai::ReasoningMode::Pro => format!("pro · {}", reasoning_label(&app.reasoning)),
    };
    let cost_warning = app
        .config
        .cost_warning_microdollars
        .map(format_microdollars_cents)
        .unwrap_or_else(|| "disabled".to_owned());
    format!(
        "Provider       {}\nModel          {}\nDisplay model  {}\nAPI model      {}\nEndpoint       {}\nProtocol       {:?}\nTransport      {:?}\nReasoning      {}\nPricing        {}\nContext        ~{} / {} (estimated)\n\
         Workspace      {}\nSession        {} — {}\nSession cost   {} ({})\nCost guardrails limit {} · turn warning {}\nCache hit rate  {}\nModel turns    {}\nTool calls     {}\nSkills         {} active / {} discovered\n\n\
         Extensions     {}\n\n\
         Security model: local agent with workspace trust gates\nBuilt-in file paths: {}\nFile edits: {}\nFile write: {}\n\
         Process execution: {}\nShell execution: {}\nOS isolation: none\n\
         Process privileges: current user\nRepository trust: {}\nQueued reconfiguration: {}",
        app.model.endpoint.id.0,
        app.model.spec.id.0,
        display.name,
        app.model.spec.api_name,
        app.model.endpoint.base_url,
        app.model.spec.protocol,
        app.model.endpoint.transport,
        reasoning,
        pricing,
        context,
        context_window,
        app.config.workspace.display(),
        session_id,
        active_branch_title(session),
        format_microdollars(session.total_cost_microdollars()),
                session.total_cost_microdollars(),
        cost_limit,
        cost_warning,
        cache_rate,
        model_turns,
        tool_calls,
        active_skills,
        discovered_skills,
        app.executable_extensions.status_summary(),
        path_access(sandbox.allow_external_paths),
        gate(sandbox.allow_edit),
        gate(sandbox.allow_write),
        gate(sandbox.allow_process && sandbox.allow_shell),
        gate(sandbox.allow_process && sandbox.allow_shell),
        if app.config.workspace_trusted {
            "trusted (project config/context/skills enabled)"
        } else {
            "untrusted (project config/context/skills ignored)"
        },
        queue,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_complete_v1_command_grammar() {
        assert_eq!(parse("/login"), Command::Login(None));
        assert_eq!(
            parse("/logout openai-codex"),
            Command::Logout(Some("openai-codex".into()))
        );
        assert_eq!(
            parse("/model gpt-4o-mini"),
            Command::Model(Some("gpt-4o-mini".into()))
        );
        assert_eq!(parse("/cycle-model"), Command::CycleModel);
        assert_eq!(parse("/thinking"), Command::Thinking(None));
        assert_eq!(parse("/theme dusk"), Command::Theme(Some("dusk".into())));
        assert_eq!(parse("/verbose on"), Command::Verbose(Some(true)));
        assert_eq!(parse("/verbose off"), Command::Verbose(Some(false)));
        assert_eq!(parse("/compact"), Command::Compact);
        assert_eq!(parse("/auto-compact"), Command::AutoCompact(None));
        assert_eq!(
            parse("/auto-compact off"),
            Command::AutoCompact(Some(AutoCompactSetting::Enabled(false)))
        );
        assert_eq!(
            parse("/auto-compact 85%"),
            Command::AutoCompact(Some(AutoCompactSetting::ThresholdPercent(85)))
        );
        assert_eq!(parse("/reload"), Command::Reload);
        assert_eq!(parse("/new"), Command::New);
        assert_eq!(parse("/resume id"), Command::Resume(Some("id".into())));
        assert_eq!(parse("/tree"), Command::Tree);
        assert_eq!(parse("/checkout 001"), Command::Checkout("001".into()));
        assert_eq!(parse("/status"), Command::Status);
        assert_eq!(parse("/context"), Command::Context);
        assert_eq!(parse("/cost"), Command::Cost);
        assert_eq!(parse("/cache"), Command::Cache);
        assert_eq!(parse("/update"), Command::Update);
        assert_eq!(parse("/prompt"), Command::Prompt(None));
        assert_eq!(
            parse("/prompt review staged changes"),
            Command::Prompt(Some("review staged changes".into()))
        );
        assert_eq!(parse("/help"), Command::Unknown("/help".into()));
        assert_eq!(parse("/quit"), Command::Quit);
        assert_eq!(parse("/skills"), Command::Skills(SkillsSubcommand::List));
        assert_eq!(
            parse("/sk active"),
            Command::Skills(SkillsSubcommand::Active)
        );
        assert_eq!(
            parse("/skills search rust review"),
            Command::Skills(SkillsSubcommand::Search("rust review".into()))
        );
        assert_eq!(
            parse("/skills load audit"),
            Command::Skills(SkillsSubcommand::Load("audit".into()))
        );
        assert_eq!(
            parse("/skills reload"),
            Command::Skills(SkillsSubcommand::Reload)
        );
        assert_eq!(
            parse("/skills off audit"),
            Command::Skills(SkillsSubcommand::Off("audit".into()))
        );
    }

    #[test]
    fn slash_suggestions_filter_and_tab_complete_unique_prefixes() {
        assert_eq!(slash_suggestions("/").len(), SLASH_COMMANDS.len());
        assert_eq!(slash_suggestions("/mod")[0].usage, "/model [id]");
        assert_eq!(slash_suggestions("/th").len(), 2);
        assert!(slash_suggestions("/model ").is_empty());
        assert_eq!(complete_slash_command("/mod"), Some("/model ".to_owned()));
        assert_eq!(complete_slash_command("/th"), None);
        assert_eq!(
            complete_slash_command("/status"),
            Some("/status".to_owned())
        );
    }

    #[test]
    fn popup_registry_is_flat_and_help_is_not_a_duplicate_command() {
        assert!(SLASH_COMMANDS.iter().all(|command| command.name != "help"));
        assert!(SLASH_COMMANDS
            .iter()
            .all(|command| command.name != "Session"));
    }

    #[test]
    fn every_discovered_builtin_has_an_executable_parser_route() {
        for command in SLASH_COMMANDS {
            let invocation = match command.name {
                "checkout" => "/checkout entry-id".to_owned(),
                "name" => "/name release audit".to_owned(),
                "export" => "/export audit.md".to_owned(),
                name => format!("/{name}"),
            };
            assert!(
                !matches!(parse(&invocation), Command::Unknown(_)),
                "popup advertises /{} but its representative invocation {invocation:?} has no parser route",
                command.name
            );
        }
    }

    #[test]
    fn parses_unambiguous_command_prefixes() {
        assert_eq!(parse("/mod"), Command::Model(None));
        assert_eq!(
            parse("/mo gpt-4o-mini"),
            Command::Model(Some("gpt-4o-mini".into()))
        );
        assert_eq!(parse("/comp"), Command::Compact);
        // /c and /t each match multiple commands, so they remain unknown.
        assert!(matches!(parse("/c"), Command::Unknown(_)));
        assert!(matches!(parse("/t"), Command::Unknown(_)));
    }

    #[test]
    fn rejects_unknown_or_malformed_commands() {
        assert!(matches!(parse("hello"), Command::Unknown(_)));
        assert!(matches!(parse("/new extra"), Command::Unknown(_)));
        assert!(matches!(parse("/checkout"), Command::Unknown(_)));
        assert!(matches!(parse("/auto-compact 0%"), Command::Unknown(_)));
        assert!(matches!(parse("/auto-compact 101%"), Command::Unknown(_)));
    }

    fn app_for_status() -> (tempfile::TempDir, App) {
        use crate::app::bootstrap::{bootstrap, build_app, LaunchSelection, SessionSelection};
        use crate::config::{CompactionPolicy, Config, Mode, ResumeSelector, SandboxPolicy};
        use ygg_ai::{ModelId, ReasoningConfig};

        let directory = tempfile::tempdir().unwrap();
        let config = Config {
            workspace: directory.path().to_owned(),
            invocation_cwd: directory.path().to_owned(),
            model: Some(ModelId("gpt-4o-mini".into())),
            model_explicit: false,
            reasoning: ReasoningConfig::Off,
            reasoning_explicit: false,
            reasoning_mode: ygg_ai::ReasoningMode::Standard,
            reasoning_mode_explicit: false,
            cache_retention: ygg_ai::CacheRetention::Short,
            sandbox: SandboxPolicy::default(),
            theme: None,
            theme_paths: vec![],
            color: crate::config::ColorMode::Auto,
            mouse: crate::config::MouseMode::Auto,
            plain: false,
            session_dir: directory.path().join("sessions"),
            compaction: CompactionPolicy::default(),
            max_cost_microdollars: None,
            cost_warning_microdollars: None,
            show_turn_cost: false,
            max_turns: Some(40),
            show_reasoning_in_print: false,
            initial_prompt: None,
            prompt_template: None,
            debug_prompt: false,
            prompt_paths: vec![],
            mode: Mode::Interactive,
            resume: ResumeSelector::New,
            skill_paths: vec![],
            extension_paths: vec![],
            enabled_extensions: vec![],
            trusted_extensions: vec![],
            invocation_trusted_extensions: vec![],
            tools: crate::config::ToolPolicy::default(),
            context_files: true,
            offline: true,
            workspace_trusted: true,
        };
        let boot = bootstrap(config).unwrap();
        let app = build_app(
            boot,
            LaunchSelection {
                model: ModelId("gpt-4o-mini".into()),
                session: SessionSelection::CreateNew(directory.path().join("session.jsonl")),
                reasoning: ReasoningConfig::Off,
                reasoning_mode: ygg_ai::ReasoningMode::Standard,
            },
            "system".into(),
        )
        .unwrap();
        (directory, app)
    }

    #[test]
    fn status_references_real_runtime_features() {
        let (_directory, app) = app_for_status();
        let queued = Reconfig::NewSession;
        let status = status_text(&app, Some(&queued));
        for expected in [
            "Provider       openai",
            "Model          gpt-4o-mini",
            "Reasoning      off",
            "Workspace",
            "Session",
            "Context",
            "Model turns",
            "Tool calls",
            "Skills         0 active / 0 discovered",
            "Security model: local agent with workspace trust gates",
            "Built-in file paths: current-user paths (absolute, ~/ and relative)",
            "File edits: enabled",
            "Process execution: enabled",
            "Shell execution: enabled",
            "OS isolation: none",
            "Process privileges: current user",
            "Repository trust: trusted (project config/context/skills enabled)",
            "NewSession",
        ] {
            assert!(
                status.contains(expected),
                "missing {expected:?} in {status}"
            );
        }
    }
}
