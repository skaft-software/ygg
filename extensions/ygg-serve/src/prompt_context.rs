//! Shared composition boundary for visible auxiliary prompt context.
//!
//! Document and trusted-project file stores both produce ordinary UTF-8 text.
//! This module is the final fail-closed join: user text, both auxiliary
//! sources, and inserted separators must fit the existing protocol prompt cap.

#![forbid(unsafe_code)]

use std::fmt;

use thiserror::Error;

use crate::bounds::{validate_public_text, MAX_PROMPT_BYTES};

const SECTION_SEPARATOR: &str = "\n\n";

/// Maximum extracted uploaded-document text accepted before prompt composition.
pub const MAX_DOCUMENT_CONTEXT_BYTES: usize = 96 * 1024;
/// Maximum trusted project-file text accepted before prompt composition.
pub const MAX_PROJECT_FILE_CONTEXT_BYTES: usize = 96 * 1024;
/// Maximum combined auxiliary text, including the separator between sources.
pub const MAX_AUXILIARY_PROMPT_CONTEXT_BYTES: usize = 192 * 1024;

/// A composed text-only prompt that is guaranteed to satisfy `MAX_PROMPT_BYTES`.
#[derive(Clone, PartialEq, Eq)]
pub struct ComposedPromptText {
    text: String,
    user_text_bytes: usize,
    document_context_bytes: usize,
    project_file_context_bytes: usize,
}

impl ComposedPromptText {
    /// Returns the fully composed, protocol-bounded prompt text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Consumes the composition and returns its text.
    pub fn into_string(self) -> String {
        self.text
    }

    /// Returns the exact user-authored byte count.
    pub fn user_text_bytes(&self) -> usize {
        self.user_text_bytes
    }

    /// Returns the exact uploaded-document context byte count.
    pub fn document_context_bytes(&self) -> usize {
        self.document_context_bytes
    }

    /// Returns the exact trusted project-file context byte count.
    pub fn project_file_context_bytes(&self) -> usize {
        self.project_file_context_bytes
    }

    /// Returns the final composed UTF-8 byte count.
    pub fn total_bytes(&self) -> usize {
        self.text.len()
    }
}

impl fmt::Debug for ComposedPromptText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ComposedPromptText")
            .field("text", &"<redacted>")
            .field("user_text_bytes", &self.user_text_bytes)
            .field("document_context_bytes", &self.document_context_bytes)
            .field(
                "project_file_context_bytes",
                &self.project_file_context_bytes,
            )
            .field("total_bytes", &self.total_bytes())
            .finish()
    }
}

/// Prompt-context composition failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum PromptContextError {
    /// User-authored text violates the existing public prompt boundary.
    #[error("the user prompt text is invalid or too large")]
    InvalidUserText,
    /// Uploaded-document context exceeded its reserved source budget.
    #[error("uploaded-document context exceeds its prompt budget")]
    DocumentContextTooLarge,
    /// Uploaded-document context contains a disallowed public-text character.
    #[error("uploaded-document context is invalid")]
    InvalidDocumentContext,
    /// Trusted project-file context exceeded its reserved source budget.
    #[error("trusted project-file context exceeds its prompt budget")]
    ProjectFileContextTooLarge,
    /// Trusted project-file context contains a disallowed public-text character.
    #[error("trusted project-file context is invalid")]
    InvalidProjectFileContext,
    /// Both auxiliary sources plus their separator exceeded the shared budget.
    #[error("combined auxiliary prompt context exceeds its budget")]
    AuxiliaryContextTooLarge,
    /// User and auxiliary text together exceeded the protocol prompt boundary.
    #[error("the composed prompt exceeds the protocol prompt boundary")]
    PromptTooLarge,
}

/// Composes user text and optional visible context without truncation.
///
/// Both context arguments must already contain any source labels/delimiters
/// they need. Empty context is treated as absent. The function validates the
/// user-authored text with the same public-text rule as `PromptInput`, inserts
/// separators only between non-empty sections, and guarantees the returned
/// string is no larger than [`MAX_PROMPT_BYTES`].
pub fn compose_prompt_text(
    user_text: &str,
    document_context: Option<&str>,
    project_file_context: Option<&str>,
) -> Result<ComposedPromptText, PromptContextError> {
    validate_public_text("prompt.user_text", user_text, MAX_PROMPT_BYTES, true)
        .map_err(|_| PromptContextError::InvalidUserText)?;
    let document_context = document_context.filter(|context| !context.is_empty());
    let project_file_context = project_file_context.filter(|context| !context.is_empty());
    let document_context_bytes = document_context.map(str::len).unwrap_or_default();
    let project_file_context_bytes = project_file_context.map(str::len).unwrap_or_default();
    if document_context_bytes > MAX_DOCUMENT_CONTEXT_BYTES {
        return Err(PromptContextError::DocumentContextTooLarge);
    }
    if project_file_context_bytes > MAX_PROJECT_FILE_CONTEXT_BYTES {
        return Err(PromptContextError::ProjectFileContextTooLarge);
    }
    if let Some(context) = document_context {
        validate_public_text(
            "prompt.document_context",
            context,
            MAX_DOCUMENT_CONTEXT_BYTES,
            true,
        )
        .map_err(|_| PromptContextError::InvalidDocumentContext)?;
    }
    if let Some(context) = project_file_context {
        validate_public_text(
            "prompt.project_file_context",
            context,
            MAX_PROJECT_FILE_CONTEXT_BYTES,
            true,
        )
        .map_err(|_| PromptContextError::InvalidProjectFileContext)?;
    }

    let auxiliary_separator_bytes =
        usize::from(document_context.is_some() && project_file_context.is_some())
            * SECTION_SEPARATOR.len();
    let auxiliary_bytes = document_context_bytes
        .checked_add(project_file_context_bytes)
        .and_then(|bytes| bytes.checked_add(auxiliary_separator_bytes))
        .ok_or(PromptContextError::AuxiliaryContextTooLarge)?;
    if auxiliary_bytes > MAX_AUXILIARY_PROMPT_CONTEXT_BYTES {
        return Err(PromptContextError::AuxiliaryContextTooLarge);
    }

    let section_count = usize::from(!user_text.is_empty())
        + usize::from(document_context.is_some())
        + usize::from(project_file_context.is_some());
    let total_separator_bytes = section_count.saturating_sub(1) * SECTION_SEPARATOR.len();
    let total_bytes = user_text
        .len()
        .checked_add(document_context_bytes)
        .and_then(|bytes| bytes.checked_add(project_file_context_bytes))
        .and_then(|bytes| bytes.checked_add(total_separator_bytes))
        .ok_or(PromptContextError::PromptTooLarge)?;
    if total_bytes > MAX_PROMPT_BYTES {
        return Err(PromptContextError::PromptTooLarge);
    }

    let mut text = String::with_capacity(total_bytes);
    push_section(&mut text, user_text);
    if let Some(context) = document_context {
        push_section(&mut text, context);
    }
    if let Some(context) = project_file_context {
        push_section(&mut text, context);
    }
    debug_assert_eq!(text.len(), total_bytes);
    Ok(ComposedPromptText {
        text,
        user_text_bytes: user_text.len(),
        document_context_bytes,
        project_file_context_bytes,
    })
}

fn push_section(output: &mut String, section: &str) {
    if section.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push_str(SECTION_SEPARATOR);
    }
    output.push_str(section);
}
