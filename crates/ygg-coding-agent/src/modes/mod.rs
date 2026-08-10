#![allow(missing_docs)]

pub mod interactive;
pub mod plain;
pub mod print;
pub mod rpc;

use std::time::{SystemTime, UNIX_EPOCH};

use ygg_agent::{public_error_diagnostic, FinishReason};

/// Terminal state of a started Agent run, shared by both frontends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnded {
    Completed,
    Aborted,
    MaxTurns,
    Failed(String),
}

pub fn run_ended(reason: &FinishReason, endpoint: &str, model: &str) -> RunEnded {
    match reason {
        FinishReason::Completed => RunEnded::Completed,
        FinishReason::Aborted => RunEnded::Aborted,
        FinishReason::MaxTurns => RunEnded::MaxTurns,
        FinishReason::Failed(error) => {
            RunEnded::Failed(public_error_diagnostic(error, endpoint, model))
        }
    }
}

/// Filesystem-safe timestamp seed for new session filenames.
pub fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}-{:09}-{}",
        elapsed.as_secs(),
        elapsed.subsec_nanos(),
        std::process::id()
    )
}
