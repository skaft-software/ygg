#![allow(missing_docs)]

pub mod interactive;
pub mod print;

use std::time::{SystemTime, UNIX_EPOCH};

use ygg_agent::FinishReason;

/// Terminal state of a started Agent run, shared by both frontends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEnded {
    Completed,
    Aborted,
    MaxTurns,
    Failed(String),
}

impl From<FinishReason> for RunEnded {
    fn from(reason: FinishReason) -> Self {
        match reason {
            FinishReason::Completed => Self::Completed,
            FinishReason::Aborted => Self::Aborted,
            FinishReason::MaxTurns => Self::MaxTurns,
            FinishReason::Failed(error) => Self::Failed(error.to_string()),
        }
    }
}

/// Filesystem-safe timestamp seed for new session filenames.
pub fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{seconds}")
}
