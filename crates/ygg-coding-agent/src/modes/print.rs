#![allow(missing_docs)]

use std::io::Write;

use ygg_agent::{AgentEvent, OutputChannel};

use crate::app::bootstrap::{build_app, resolve_launch_print, Bootstrap};
use crate::modes::{timestamp, RunEnded};

const BASE_SYSTEM: &str = "You are ygg, a careful coding agent. Work directly in the workspace, explain important changes concisely, and use tools when they improve accuracy.";

/// Convert an explicit terminal run result to process success or an actionable
/// nonzero error. A started run must always yield `RunFinished`.
pub fn classify_finish(finished: Option<RunEnded>) -> anyhow::Result<()> {
    match finished {
        Some(RunEnded::Completed) => Ok(()),
        Some(RunEnded::MaxTurns) => anyhow::bail!("run hit max turns before completing"),
        Some(RunEnded::Aborted) => anyhow::bail!("run aborted before completing"),
        Some(RunEnded::Failed(error)) => anyhow::bail!("run failed: {error}"),
        None => anyhow::bail!("run stream ended without RunFinished (invariant violation)"),
    }
}

/// Stream a persistent Agent session to standard output without constructing a
/// terminal UI.
pub async fn run_print(boot: Bootstrap, prompt: String) -> anyhow::Result<()> {
    let launch = resolve_launch_print(&boot, &timestamp())?;
    let mut app = build_app(boot, launch, BASE_SYSTEM.to_owned())?;

    // M9 inserts the shared pre-request compaction gate here.
    let show_reasoning = app.config.show_reasoning_in_print;
    let mut run = app.agent.prompt(prompt).await?;
    let mut output = std::io::stdout().lock();
    let mut finished = None;

    while let Some(event) = run.next().await {
        match event {
            AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text,
            } => write!(output, "{text}")?,
            AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text,
            } if show_reasoning => write!(output, "{text}")?,
            AgentEvent::RunFinished { reason, .. } => finished = Some(RunEnded::from(reason)),
            _ => {}
        }
    }
    drop(run);
    output.flush()?;
    classify_finish(finished)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_finish_has_explicit_success_and_failures() {
        assert!(classify_finish(Some(RunEnded::Completed)).is_ok());
        assert!(classify_finish(Some(RunEnded::MaxTurns)).is_err());
        assert!(classify_finish(Some(RunEnded::Aborted)).is_err());
        assert!(classify_finish(Some(RunEnded::Failed("nope".into()))).is_err());
        assert!(classify_finish(None).is_err());
    }
}
