//! The agent: configuration, the procedural run loop, and run control.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_core::Stream;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use ygg_ai::{
    AiClient, AiError, CompatibilityMode, Message, Model, OutputFormat, OutputModalities,
    ReasoningConfig, Request, StreamEvent, ToolCall, ToolChoice, ToolDef, ToolResult,
    ToolResultPart, Usage, UserMessage, UserPart,
};

use crate::events::{AgentEvent, Control, FinishReason, OutputChannel};
use crate::extension::{EventObserver, ExtensionHost};
use crate::sandbox::SandboxConfig;
use crate::session::{EntryId, EntryValue, Session, SessionError};
use crate::tool::{
    Tool, ToolContext, ToolError, ToolOutput, ToolProgress, ToolProgressSink,
    PROGRESS_CHANNEL_CAPACITY,
};

/// Errors surfaced by [`Agent`] APIs.
///
/// Before a run starts these are returned directly (from [`Agent::new`],
/// [`Agent::prompt`], [`RunControl::steer`]…). Once a run has started, every
/// failure is delivered as the single terminal
/// [`AgentEvent::RunFinished`]`{ reason: FinishReason::Failed(..) }` event —
/// there is no second asynchronous error channel.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// Session persistence failed.
    #[error("session error: {0}")]
    Session(#[from] SessionError),
    /// The inference layer failed.
    #[error("ai error: {0}")]
    Ai(#[from] AiError),
    /// Two tools were registered under the same name.
    #[error("duplicate tool name registered: {0}")]
    DuplicateTool(String),
    /// The configured workspace root is unusable.
    #[error("invalid workspace: {0}")]
    Workspace(String),
    /// A control message was sent after the run finished.
    #[error("the run has already finished")]
    RunEnded,
}

/// Configuration for [`Agent::new`].
pub struct AgentConfig {
    /// The inference client.
    pub client: AiClient,
    /// The resolved model to converse with.
    pub model: Model,
    /// The session holding (and persisting) conversation history.
    pub session: Session,
    /// The system prompt (empty string for none).
    pub system: String,
    /// Capability gates and limits for tool execution.
    pub sandbox: SandboxConfig,
    /// Registered tools and event observers. Register [`CoreTools`](crate::tools::CoreTools)
    /// here for the built-in `read`/`search`/`edit`/`exec` tools.
    pub extensions: ExtensionHost,
    /// Maximum model turns per run; exceeding it finishes the run with
    /// [`FinishReason::MaxTurns`].
    pub max_turns: u64,
    /// Reasoning configuration applied to every model request in this agent's
    /// runs. Use [`ReasoningConfig::Off`] to disable reasoning (the historical
    /// default). Unsupported configurations are rejected by `ygg-ai`'s
    /// validation when the run opens its stream, surfacing as
    /// [`FinishReason::Failed`].
    pub reasoning: ReasoningConfig,
}

/// A stateful agent: one session, one model, one authoritative head.
///
/// The agent owns its [`Session`]; runs borrow the agent mutably
/// (`&mut self`), so there is exactly one mutable head and no cloned or
/// detached conversation state.
pub struct Agent {
    client: AiClient,
    model: Model,
    session: Session,
    extensions: ExtensionHost,
    sandbox: SandboxConfig,
    system: String,
    max_turns: u64,
    reasoning: ReasoningConfig,
}

/// Aggregate result of [`Agent::complete`].
#[derive(Debug)]
pub struct RunOutput {
    /// Concatenated visible text from all turns.
    pub text: String,
    /// Total token usage across the run.
    pub usage: Usage,
    /// Session entry ID after the run.
    pub head: EntryId,
    /// How the run ended (never [`FinishReason::Failed`]; failures are
    /// returned as `Err` instead).
    pub reason: FinishReason,
}

/// A streaming agent run: the event stream plus a clonable control handle.
///
/// The run is driven by the caller — poll it with [`Run::next`] (or as a
/// [`Stream`]), typically inside `tokio::select!` alongside user input.
/// Dropping the run cancels the in-flight model stream and any running tool
/// (child processes included).
pub struct Run<'a> {
    stream: Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>,
    control: RunControl,
}

impl<'a> Run<'a> {
    /// Returns a clonable handle for sending control messages while the run's
    /// event stream is being consumed.
    pub fn control(&self) -> RunControl {
        self.control.clone()
    }

    /// Returns the next event, or `None` after the terminal
    /// [`AgentEvent::RunFinished`] has been delivered.
    pub async fn next(&mut self) -> Option<AgentEvent> {
        self.stream.next().await
    }
}

impl Stream for Run<'_> {
    type Item = AgentEvent;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

/// Clonable control handle for an active [`Run`].
#[derive(Clone)]
pub struct RunControl {
    tx: mpsc::Sender<Control>,
    abort: Arc<AbortFlag>,
}

impl RunControl {
    /// Injects text into the conversation at the next model-turn boundary of
    /// the active run (persisted to the session when applied).
    pub async fn steer(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.tx
            .send(Control::Steer(text.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Queues text for after the current run settles: when the model completes
    /// a turn without tool calls, the run continues with this input instead of
    /// finishing.
    pub async fn follow_up(&self, text: impl Into<String>) -> Result<(), AgentError> {
        self.tx
            .send(Control::FollowUp(text.into()))
            .await
            .map_err(|_| AgentError::RunEnded)
    }

    /// Aborts the run at the next safe boundary: the in-flight model stream is
    /// dropped (cancelling the request) or the running tool is cancelled (child
    /// processes killed). All already-completed session entries are preserved
    /// and the run finishes with exactly one
    /// [`AgentEvent::RunFinished`]`{ reason: FinishReason::Aborted }`.
    pub fn abort(&self) {
        self.abort.set();
    }
}

/// Level-triggered abort signal: reliable regardless of channel capacity and
/// observable both by polling (`is_set`) and awaiting (`wait`).
#[derive(Default)]
struct AbortFlag {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}

impl AbortFlag {
    fn set(&self) {
        self.set.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_set() {
                return;
            }
            notified.await;
        }
    }
}

fn user_message(text: String) -> EntryValue {
    EntryValue::Message(Message::User(UserMessage {
        content: vec![UserPart::Text(text)],
    }))
}

fn notify_observers(observers: &[Arc<dyn EventObserver>], event: &AgentEvent) {
    for observer in observers {
        observer.on_event(event);
    }
}

fn add_usage(total: &mut Usage, turn: &Usage) {
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

impl Agent {
    /// Creates a new agent: canonicalizes the sandbox workspace and validates
    /// the registered extensions (duplicate tool names are rejected).
    pub fn new(mut config: AgentConfig) -> Result<Self, AgentError> {
        if let Some(duplicate) = config.extensions.duplicate_tools.first() {
            return Err(AgentError::DuplicateTool(duplicate.clone()));
        }
        let workspace = config.sandbox.workspace.canonicalize().map_err(|e| {
            AgentError::Workspace(format!("{}: {e}", config.sandbox.workspace.display()))
        })?;
        if !workspace.is_dir() {
            return Err(AgentError::Workspace(format!(
                "{}: not a directory",
                workspace.display()
            )));
        }
        config.sandbox.workspace = workspace;
        Ok(Self {
            client: config.client,
            model: config.model,
            session: config.session,
            extensions: config.extensions,
            sandbox: config.sandbox,
            system: config.system,
            max_turns: config.max_turns,
            reasoning: config.reasoning,
        })
    }

    /// Read-only access to the agent's session (its entries and head).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Mutable access to the session for history operations between runs
    /// (checkout, manual compaction, config entries).
    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Begins a run: appends the user message to the session and returns the
    /// caller-driven event stream plus its control handle.
    ///
    /// Pre-flight failures (e.g. the session append) are returned here; once
    /// the run has started every terminal outcome — completed, aborted,
    /// failed, or max-turns — is reported by exactly one
    /// [`AgentEvent::RunFinished`].
    pub async fn prompt(&mut self, input: impl Into<String>) -> Result<Run<'_>, AgentError> {
        let first_entry = self.session.append(user_message(input.into()))?;

        let (control_tx, mut control_rx) = mpsc::channel::<Control>(8);
        let abort = Arc::new(AbortFlag::default());
        let control = RunControl {
            tx: control_tx,
            abort: abort.clone(),
        };

        // Disjoint borrows: the run stream owns clones of everything except
        // the session, which it borrows mutably for the run's lifetime —
        // preserving one authoritative head.
        let client = self.client.clone();
        let model = self.model.clone();
        let system = self.system.clone();
        let sandbox = self.sandbox.clone();
        let tools = self.extensions.tools.clone();
        let observers = self.extensions.observers.clone();
        let max_turns = self.max_turns;
        let reasoning = self.reasoning.clone();
        let session = &mut self.session;

        let stream = async_stream::stream! {
            let mut tool_map: HashMap<String, Arc<dyn Tool>> =
                HashMap::with_capacity(tools.len());
            for tool in &tools {
                let definition = tool.definition();
                tool_map.insert(definition.name, Arc::clone(tool));
            }
            let tool_defs: Vec<ToolDef> = tools.iter().map(|t| t.definition()).collect();

            let mut pending_steer: Vec<String> = Vec::new();
            let mut followups: VecDeque<String> = VecDeque::new();
            let mut control_open = true;
            let mut completed_turns: u64 = 0;
            let mut run_usage = Usage::default();

            let reason: FinishReason = 'run: loop {
                // ── Drain control at the turn boundary ─────────────────────
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(text)) => pending_steer.push(text),
                        Ok(Control::FollowUp(text)) => followups.push_back(text),
                        Ok(Control::Abort) => break 'run FinishReason::Aborted,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }
                if abort.is_set() {
                    break 'run FinishReason::Aborted;
                }

                // ── Steering enters here, at the model-turn boundary ───────
                // Drain the whole steering queue before opening the next
                // request. This is the "all at once" steering mode: every
                // message queued since the previous boundary is visible to
                // the same model turn, after all of that turn's tools have
                // finished.
                if !pending_steer.is_empty() {
                    let queued = std::mem::take(&mut pending_steer);
                    let mut delivered = Vec::with_capacity(queued.len());
                    for text in queued {
                        if let Err(e) = session.append(user_message(text.clone())) {
                            if !delivered.is_empty() {
                                let ev = AgentEvent::SteeringDelivered {
                                    messages: delivered,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            break 'run FinishReason::Failed(e.into());
                        }
                        delivered.push(text);
                    }
                    if !delivered.is_empty() {
                        let ev = AgentEvent::SteeringDelivered {
                            messages: delivered,
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }
                }

                // ── Turn guard ─────────────────────────────────────────────
                if completed_turns >= max_turns {
                    break 'run FinishReason::MaxTurns;
                }
                completed_turns += 1;

                // ── Reconstruct context from the session head ──────────────
                let messages = match session.context() {
                    Ok(m) => m,
                    Err(e) => break 'run FinishReason::Failed(e.into()),
                };
                let request = Request {
                    system: if system.is_empty() { None } else { Some(system.clone()) },
                    messages,
                    tools: tool_defs.clone(),
                    tool_choice: ToolChoice::Auto,
                    max_output_tokens: None,
                    temperature: None,
                    stop: vec![],
                    reasoning: reasoning.clone(),
                    output_format: OutputFormat::Text,
                    output_modalities: OutputModalities::Text,
                    compatibility: CompatibilityMode::Strict,
                };

                // ── Open the provider stream (abortable) ───────────────────
                let opened = tokio::select! {
                    r = client.stream(&model, request) => Some(r),
                    _ = abort.wait() => None,
                };
                let mut response_stream = match opened {
                    None => break 'run FinishReason::Aborted,
                    Some(Err(e)) => break 'run FinishReason::Failed(e.into()),
                    Some(Ok(s)) => s,
                };

                // ── Consume the stream, staying responsive to control ──────
                enum Next {
                    Event(Option<Result<StreamEvent, AiError>>),
                    Ctl(Option<Control>),
                    Abort,
                }
                let turn = loop {
                    let next = tokio::select! {
                        ev = response_stream.next() => Next::Event(ev),
                        c = control_rx.recv(), if control_open => Next::Ctl(c),
                        _ = abort.wait() => Next::Abort,
                    };
                    match next {
                        Next::Abort | Next::Ctl(Some(Control::Abort)) => {
                            break Err(FinishReason::Aborted);
                        }
                        Next::Ctl(Some(Control::Steer(text))) => pending_steer.push(text),
                        Next::Ctl(Some(Control::FollowUp(text))) => followups.push_back(text),
                        Next::Ctl(None) => control_open = false,
                        Next::Event(None) => {
                            break Err(FinishReason::Failed(
                                AiError::StreamProtocol(
                                    ygg_ai::StreamProtocolError::MissingFinish,
                                )
                                .into(),
                            ));
                        }
                        Next::Event(Some(Err(e))) => break Err(FinishReason::Failed(e.into())),
                        Next::Event(Some(Ok(event))) => match event {
                            StreamEvent::TextDelta { delta, .. } => {
                                let ev = AgentEvent::OutputDelta {
                                    channel: OutputChannel::Text,
                                    text: delta,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            StreamEvent::ReasoningDelta { delta, .. } => {
                                let ev = AgentEvent::OutputDelta {
                                    channel: OutputChannel::Reasoning,
                                    text: delta,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            // `ygg-ai`'s ResponseBuilder assembles the message
                            // and validates the stream; the agent does not
                            // duplicate that parser. Raw tool-argument deltas
                            // are deliberately not exposed.
                            StreamEvent::Finished(response) => break Ok(response),
                            _ => {}
                        },
                    }
                };
                let response = match turn {
                    Ok(r) => r,
                    Err(reason) => break 'run reason,
                };
                drop(response_stream);

                // ── Persist the completed assistant message ────────────────
                let assistant = response.message;
                if let Err(e) =
                    session.append(EntryValue::Message(Message::Assistant(assistant.clone())))
                {
                    break 'run FinishReason::Failed(e.into());
                }
                add_usage(&mut run_usage, &response.usage);
                let ev = AgentEvent::TurnFinished {
                    message: assistant.clone(),
                    usage: run_usage,
                };
                notify_observers(&observers, &ev);
                yield ev;

                // Drain any steer/follow-up that arrived while TurnFinished
                // was being processed by the caller. Without this, a steer
                // that lands between TurnFinished and the calls.is_empty()
                // check below is left stranded in the channel buffer and
                // lost when the run completes without tool calls.
                while control_open {
                    match control_rx.try_recv() {
                        Ok(Control::Steer(text)) => pending_steer.push(text),
                        Ok(Control::FollowUp(text)) => followups.push_back(text),
                        Ok(Control::Abort) => break 'run FinishReason::Aborted,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => control_open = false,
                    }
                }

                // ── Extract tool calls ─────────────────────────────────────
                let calls: Vec<ToolCall> = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ygg_ai::AssistantPart::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();

                if calls.is_empty() {
                    // A pending steer re-enters the loop so the model sees it;
                    // otherwise a queued follow-up begins now that the run has
                    // settled; otherwise the run is complete.
                    if !pending_steer.is_empty() {
                        continue;
                    }
                    if let Some(text) = followups.pop_front() {
                        if let Err(e) = session.append(user_message(text)) {
                            break 'run FinishReason::Failed(e.into());
                        }
                        continue;
                    }
                    break 'run FinishReason::Completed;
                }

                // ── Execute tools sequentially, in emitted order ───────────
                for call in calls {
                    if abort.is_set() {
                        break 'run FinishReason::Aborted;
                    }
                    let parsed = call.arguments_value();
                    let ev = AgentEvent::ToolStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        args: parsed.as_ref().cloned().unwrap_or(serde_json::Value::Null),
                    };
                    notify_observers(&observers, &ev);
                    yield ev;

                    // Create a fresh progress channel for every tool
                    // call. Non‑streaming tools (unknown, bad args)
                    // simply never push into it; the drain below is a
                    // no‑op.
                    let (progress_tx, mut progress_rx) =
                        mpsc::channel::<ToolProgress>(PROGRESS_CHANNEL_CAPACITY);
                    let progress_sink = ToolProgressSink::live(progress_tx);

                    let result: Result<ToolOutput, ToolError> =
                        match (tool_map.get(&call.name), parsed) {
                            (None, _) => {
                                Err(ToolError::new(format!("unknown tool: {}", call.name)))
                            }
                            (Some(_), Err(e)) => {
                                Err(ToolError::new(format!("invalid tool arguments: {e}")))
                            }
                            (Some(tool), Ok(args)) => {
                                let tool_ctx = ToolContext {
                                    workspace: &sandbox.workspace,
                                    sandbox: &sandbox,
                                    progress: progress_sink.clone(),
                                };
                                let execute = tool.execute(args, &tool_ctx);
                                tokio::pin!(execute);
                                // Cancellation drops the pinned future, which
                                // kills any child process tree it spawned.
                                let outcome = loop {
                                    tokio::select! {
                                        biased;
                                        _ = abort.wait() => break None,
                                        r = &mut execute => break Some(r),
                                        c = control_rx.recv(), if control_open => match c {
                                            Some(Control::Steer(text)) => pending_steer.push(text),
                                            Some(Control::FollowUp(text)) => followups.push_back(text),
                                            Some(Control::Abort) => break None,
                                            None => control_open = false,
                                        },
                                        progress = progress_rx.recv() => {
                                            if let Some(p) = progress {
                                                let ev = AgentEvent::ToolProgress {
                                                    id: call.id.clone(),
                                                    progress: p,
                                                };
                                                notify_observers(&observers, &ev);
                                                yield ev;
                                            }
                                        },
                                    }
                                };
                                match outcome {
                                    Some(r) => r,
                                    None => break 'run FinishReason::Aborted,
                                }
                            }
                        };

                    // ── COMMIT BOUNDARY ──────────────────────────────────
                    // Tool::execute resolved (or an immediate error was
                    // produced). Persist the result immediately before
                    // draining progress or checking abort. An abort
                    // received after this point cannot erase an already-
                    // committed result.
                    let (text, is_error) = match &result {
                        Ok(output) => (output.text.clone(), false),
                        Err(error) => (error.message.clone(), true),
                    };
                    let tool_result = ToolResult {
                        tool_call_id: call.id.clone(),
                        content: vec![ToolResultPart::Text(text)],
                        is_error,
                    };
                    if let Err(e) = session.append(EntryValue::Message(Message::User(
                        UserMessage {
                            content: vec![UserPart::ToolResult(tool_result)],
                        },
                    ))) {
                        break 'run FinishReason::Failed(e.into());
                    }

                    // ── Drain accepted progress before ToolFinished ───────
                    loop {
                        if abort.is_set() {
                            break; // stop draining, but result is committed
                        }
                        match progress_rx.try_recv() {
                            Ok(p) => {
                                let ev = AgentEvent::ToolProgress {
                                    id: call.id.clone(),
                                    progress: p,
                                };
                                notify_observers(&observers, &ev);
                                yield ev;
                            }
                            Err(_) => break,
                        }
                    }
                    // Report dropped bytes if any.
                    let dropped = progress_sink.take_dropped();
                    if dropped > 0 {
                        let ev = AgentEvent::ToolProgress {
                            id: call.id.clone(),
                            progress: ToolProgress::Dropped { bytes: dropped },
                        };
                        notify_observers(&observers, &ev);
                        yield ev;
                    }

                    let ev = AgentEvent::ToolFinished {
                        id: call.id.clone(),
                        result,
                    };
                    notify_observers(&observers, &ev);
                    yield ev;

                    // Abort after ToolFinished: stop further tool/model
                    // turns, but the committed result is preserved.
                    if abort.is_set() {
                        break 'run FinishReason::Aborted;
                    }
                }
                // Context reconstruction coalesces the consecutive tool-result
                // entries into the provider-required single user message.
            };

            let head = session.head().unwrap_or(first_entry);
            let ev = AgentEvent::RunFinished { head, reason };
            notify_observers(&observers, &ev);
            yield ev;
        };

        Ok(Run {
            stream: Box::pin(stream),
            control,
        })
    }

    /// Runs to completion, returning the aggregate output.
    ///
    /// A run that ends with [`FinishReason::Failed`] is returned as `Err`;
    /// aborted and max-turns runs return `Ok` with their reason.
    pub async fn complete(&mut self, input: impl Into<String>) -> Result<RunOutput, AgentError> {
        let mut run = self.prompt(input).await?;
        let mut text = String::new();
        let mut usage = Usage::default();
        while let Some(event) = run.next().await {
            match event {
                AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: delta,
                } => text.push_str(&delta),
                AgentEvent::SteeringDelivered { .. } => {}
                AgentEvent::TurnFinished { usage: total, .. } => usage = total,
                AgentEvent::RunFinished { head, reason } => {
                    return match reason {
                        FinishReason::Failed(e) => Err(e),
                        reason => Ok(RunOutput {
                            text,
                            usage,
                            head,
                            reason,
                        }),
                    };
                }
                AgentEvent::ToolProgress { .. } => {}
                _ => {}
            }
        }
        // Unreachable for a started run: the stream always ends with RunFinished.
        Err(AgentError::RunEnded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates_across_turns() {
        let mut total = Usage::default();
        let turn = Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 2,
            total_tokens: 15,
            ..Usage::default()
        };
        add_usage(&mut total, &turn);
        add_usage(&mut total, &turn);
        assert_eq!(total.input_tokens, 20);
        assert_eq!(total.output_tokens, 10);
        assert_eq!(total.reasoning_tokens, 4);
        assert_eq!(total.total_tokens, 30);
    }

    #[tokio::test]
    async fn abort_flag_wakes_waiters_and_stays_set() {
        let flag = Arc::new(AbortFlag::default());
        let waiter = {
            let flag = flag.clone();
            tokio::spawn(async move { flag.wait().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flag.set();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter must wake")
            .unwrap();
        // Late waiters return immediately.
        tokio::time::timeout(std::time::Duration::from_secs(1), flag.wait())
            .await
            .expect("level-triggered wait");
        assert!(flag.is_set());
    }
}
