//! Sanitized public errors.

use serde::{Deserialize, Serialize};

use crate::bounds::{
    sanitize_public_text, validate_public_text, ProtocolValidation, ValidationError,
    MAX_DIAGNOSTIC_BYTES,
};

/// Stable frontend-safe error categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// Protocol major mismatch.
    IncompatibleProtocol,
    /// Invalid command or DTO.
    InvalidCommand,
    /// Goal objective, budget, or lifecycle transition is invalid.
    InvalidGoal,
    /// Command ID was reused with different content.
    CommandIdConflict,
    /// Session ownership generation changed.
    StaleGeneration,
    /// Requested session or resource was not found.
    NotFound,
    /// One-shot request already resolved.
    AlreadyResolved,
    /// Replay cursor is outside retained history.
    ReplayGap,
    /// Public payload exceeded a bound.
    PayloadTooLarge,
    /// Device is not authorized.
    Unauthorized,
    /// Command is invalid at the current run boundary.
    InvalidBoundary,
    /// Session has another mutable owner.
    Locked,
    /// Requested capability is unavailable.
    Unavailable,
    /// Internal failure with private detail removed.
    Internal,
}

/// A public error containing no source chain, path, credential, or secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SanitizedError {
    /// Stable error category.
    pub code: ErrorCode,
    /// Bounded user-safe explanation.
    pub message: String,
    /// Whether retrying at the same semantic boundary can succeed.
    #[serde(default)]
    pub retryable: bool,
    /// Current actor generation for stale-generation recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<u64>,
}

impl SanitizedError {
    /// Builds an explicitly public error and strips unsafe controls.
    pub fn public(code: ErrorCode, message: impl AsRef<str>) -> Self {
        Self {
            code,
            message: sanitize_public_text(message.as_ref(), MAX_DIAGNOSTIC_BYTES, true),
            retryable: false,
            current_generation: None,
        }
    }

    /// Builds an opaque internal error.
    pub fn internal() -> Self {
        Self::public(
            ErrorCode::Internal,
            "The session host could not complete the request.",
        )
    }

    /// Marks retryability.
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    /// Includes the current generation.
    pub fn with_current_generation(mut self, generation: u64) -> Self {
        self.current_generation = Some(generation);
        self
    }
}

impl ProtocolValidation for SanitizedError {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("error.message", &self.message, MAX_DIAGNOSTIC_BYTES, true)
    }
}
