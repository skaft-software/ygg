#![allow(missing_docs)]

pub mod interactive;
pub mod plain;
pub mod print;
pub mod rpc;

use std::time::{SystemTime, UNIX_EPOCH};

use ygg_agent::{public_error_diagnostic, AgentEvent, FinishReason};

pub const RUN_STREAM_LOST_MESSAGE: &str = "run stream ended without RunFinished";
pub const RUN_SHUTDOWN_MESSAGE: &str = "host shutdown requested";

/// Host-owned terminal outcome of a started Agent run.
///
/// `RunFinished` remains the authoritative normal boundary. Frontends also
/// settle explicitly when that event cannot arrive because the run stream was
/// lost or coordinated process shutdown took ownership of cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRunOutcome {
    Completed,
    Aborted,
    MaxTurns,
    Failed(String),
    StreamLost,
    Shutdown,
}

impl HostRunOutcome {
    pub fn from_event(event: &AgentEvent, endpoint: &str, model: &str) -> Option<Self> {
        let AgentEvent::RunFinished { reason, .. } = event else {
            return None;
        };
        Some(Self::from_finish_reason(reason, endpoint, model))
    }

    pub fn from_finish_reason(reason: &FinishReason, endpoint: &str, model: &str) -> Self {
        match reason {
            FinishReason::Completed => Self::Completed,
            FinishReason::Aborted => Self::Aborted,
            FinishReason::MaxTurns => Self::MaxTurns,
            FinishReason::Failed(error) => {
                Self::Failed(public_error_diagnostic(error, endpoint, model))
            }
        }
    }

    pub fn stream_lost() -> Self {
        Self::StreamLost
    }

    pub fn shutdown() -> Self {
        Self::Shutdown
    }

    pub fn completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// API 0.1's compatibility hook is success-only; terminal failures,
    /// cancellation, stream loss, and shutdown must never invoke it.
    pub fn allows_after_response(&self) -> bool {
        self.completed()
    }

    pub fn shutdown_requested(&self) -> bool {
        matches!(self, Self::Shutdown)
    }

    pub fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Completed => None,
            Self::Aborted => Some("run aborted before completing"),
            Self::MaxTurns => Some("run hit max turns before completing"),
            Self::Failed(error) => Some(error),
            Self::StreamLost => Some(RUN_STREAM_LOST_MESSAGE),
            Self::Shutdown => Some(RUN_SHUTDOWN_MESSAGE),
        }
    }

    /// Maps the shared host terminal boundary onto API 0.2's observational
    /// lifecycle outcome without changing frontend exit semantics.
    pub fn extension_lifecycle_outcome(&self) -> ygg_agent::ExtensionLifecycleOutcome {
        match self {
            Self::Completed => ygg_agent::ExtensionLifecycleOutcome::Completed,
            Self::Aborted => ygg_agent::ExtensionLifecycleOutcome::Cancelled,
            Self::MaxTurns => ygg_agent::ExtensionLifecycleOutcome::LimitReached,
            Self::Failed(_) => ygg_agent::ExtensionLifecycleOutcome::Failed,
            Self::StreamLost => ygg_agent::ExtensionLifecycleOutcome::FrontendDisconnected,
            Self::Shutdown => ygg_agent::ExtensionLifecycleOutcome::Shutdown,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(reason: FinishReason) -> AgentEvent {
        AgentEvent::RunFinished {
            head: ygg_agent::EntryId("head".into()),
            reason,
        }
    }

    #[test]
    fn run_finished_reasons_map_to_one_shared_host_outcome() {
        assert_eq!(
            HostRunOutcome::from_event(&finished(FinishReason::Completed), "provider", "model"),
            Some(HostRunOutcome::Completed)
        );
        assert_eq!(
            HostRunOutcome::from_event(&finished(FinishReason::Aborted), "provider", "model"),
            Some(HostRunOutcome::Aborted)
        );
        assert_eq!(
            HostRunOutcome::from_event(&finished(FinishReason::MaxTurns), "provider", "model"),
            Some(HostRunOutcome::MaxTurns)
        );

        let failed = HostRunOutcome::from_event(
            &finished(FinishReason::Failed(ygg_agent::AgentError::RunEnded)),
            "provider",
            "model",
        )
        .expect("RunFinished is terminal");
        assert!(matches!(failed, HostRunOutcome::Failed(_)));
    }

    #[test]
    fn stream_loss_and_shutdown_are_explicit_non_success_outcomes() {
        let stream_lost = HostRunOutcome::stream_lost();
        assert_eq!(stream_lost.failure_message(), Some(RUN_STREAM_LOST_MESSAGE));
        assert!(!stream_lost.completed());

        let shutdown = HostRunOutcome::shutdown();
        assert_eq!(shutdown.failure_message(), Some(RUN_SHUTDOWN_MESSAGE));
        assert!(shutdown.shutdown_requested());
        assert!(!shutdown.completed());
    }

    #[test]
    fn after_response_remains_success_only() {
        let outcomes = [
            HostRunOutcome::Completed,
            HostRunOutcome::Aborted,
            HostRunOutcome::MaxTurns,
            HostRunOutcome::Failed("failed".into()),
            HostRunOutcome::StreamLost,
            HostRunOutcome::Shutdown,
        ];
        let eligible = outcomes
            .iter()
            .filter(|outcome| outcome.allows_after_response())
            .collect::<Vec<_>>();
        assert_eq!(eligible, vec![&HostRunOutcome::Completed]);
    }

    #[test]
    fn every_host_terminal_outcome_has_an_api_0_2_lifecycle_outcome() {
        use ygg_agent::ExtensionLifecycleOutcome as Lifecycle;

        let cases = [
            (HostRunOutcome::Completed, Lifecycle::Completed),
            (HostRunOutcome::Aborted, Lifecycle::Cancelled),
            (HostRunOutcome::MaxTurns, Lifecycle::LimitReached),
            (HostRunOutcome::Failed("failed".into()), Lifecycle::Failed),
            (HostRunOutcome::StreamLost, Lifecycle::FrontendDisconnected),
            (HostRunOutcome::Shutdown, Lifecycle::Shutdown),
        ];
        for (outcome, expected) in cases {
            assert_eq!(outcome.extension_lifecycle_outcome(), expected);
        }
    }

    #[test]
    fn nonterminal_events_do_not_settle_the_host_boundary() {
        let event = AgentEvent::OutputDelta {
            channel: ygg_agent::OutputChannel::Text,
            text: "partial".into(),
        };
        assert_eq!(
            HostRunOutcome::from_event(&event, "provider", "model"),
            None
        );
    }

    #[test]
    fn a_normal_event_sequence_produces_one_terminal_outcome() {
        let events = [
            AgentEvent::OutputDelta {
                channel: ygg_agent::OutputChannel::Text,
                text: "partial".into(),
            },
            finished(FinishReason::Completed),
        ];
        let outcomes = events
            .iter()
            .filter_map(|event| HostRunOutcome::from_event(event, "provider", "model"))
            .collect::<Vec<_>>();
        assert_eq!(outcomes, vec![HostRunOutcome::Completed]);
    }
}
