//! Bounded idempotent session commands and acknowledgements.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::bounds::{
    validate_public_text, validate_serialized_size, ProtocolValidation, ValidationError,
    MAX_COMMAND_BYTES, MAX_PROMPT_BYTES,
};
use crate::{
    AuthorityProfile, CatalogCursor, CommandId, DeviceId, DocumentId, DurableEntryId, ErrorCode,
    FileEntryId, HostId, ModelSelection, ProjectId, RequestId, RunId, SanitizedError,
    SessionCatalogState, SessionCursor, SessionId, MAX_GOAL_OBJECTIVE_BYTES, MAX_GOAL_TURN_BUDGET,
    PROTOCOL_VERSION,
};

/// Host-scoped commands that do not target an existing session actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum HostCommand {
    /// Create a fresh provisional session.
    #[serde(rename = "host.createSession")]
    CreateSession {
        /// Optional project context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_id: Option<ProjectId>,
        /// Requested initial authority.
        authority: AuthorityProfile,
        /// Explicit model/reasoning selection, or the host default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelSelection>,
    },
    /// Import a host-selected directory as an initially untrusted project.
    ///
    /// The browser can only return an opaque, short-lived candidate minted by
    /// a host-native folder picker. It never submits or receives a host path.
    #[serde(rename = "project.import")]
    ImportProject {
        /// Opaque host-owned folder-selection capability.
        candidate_id: String,
        /// Optional bounded display label.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
    },
    /// Rename one project without changing filesystem authority.
    #[serde(rename = "project.rename")]
    RenameProject {
        /// Opaque project identity.
        project_id: ProjectId,
        /// New bounded public label.
        display_name: String,
    },
    /// Select the default project for future create commands.
    #[serde(rename = "project.setDefault")]
    SetDefaultProject {
        /// Opaque project identity.
        project_id: ProjectId,
    },
    /// Clear the current project default.
    #[serde(rename = "project.clearDefault")]
    ClearDefaultProject,
    /// Explicitly grant or revoke execution trust.
    #[serde(rename = "project.setTrust")]
    SetProjectTrust {
        /// Opaque project identity.
        project_id: ProjectId,
        /// True grants trust; false immediately fences active owners.
        trusted: bool,
    },
    /// Archive a project and immediately revoke its execution trust.
    #[serde(rename = "project.archive")]
    ArchiveProject {
        /// Opaque project identity.
        project_id: ProjectId,
    },
    /// Move a durable session between active, archive, and recoverable trash.
    #[serde(rename = "session.setLifecycle")]
    SetSessionLifecycle {
        /// Opaque session identity.
        session_id: SessionId,
        /// Requested catalog state.
        lifecycle: SessionCatalogState,
    },
    /// Permanently delete a trashed session after an exact fresh confirmation.
    #[serde(rename = "session.deletePermanently")]
    DeleteSessionPermanently {
        /// Opaque session identity.
        session_id: SessionId,
        /// Exact confirmation bound to the current trash generation.
        confirmation: PermanentDeleteConfirmation,
    },
}

/// Exact confirmation required for permanent session deletion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermanentDeleteConfirmation {
    /// Session identity repeated to prevent cross-row confirmation reuse.
    pub session_id: SessionId,
    /// Exact host trash timestamp currently shown to the user.
    pub trashed_at_ms: u64,
    /// Must equal `permanently delete <session-id>`.
    pub phrase: String,
}

/// Device-scoped idempotent host command envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostCommandEnvelope {
    /// Protocol major.
    pub protocol: u16,
    /// Target host.
    pub host_id: HostId,
    /// Authenticated paired-device identity.
    pub device_id: DeviceId,
    /// Stable idempotency key within the device.
    pub command_id: CommandId,
    /// Device timestamp for display/audit only.
    pub issued_at_ms: u64,
    /// Typed command.
    pub command: HostCommand,
}

impl HostCommandEnvelope {
    /// Creates a protocol-v1 host command.
    pub fn new(
        host_id: HostId,
        device_id: DeviceId,
        command_id: CommandId,
        issued_at_ms: u64,
        command: HostCommand,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            host_id,
            device_id,
            command_id,
            issued_at_ms,
            command,
        }
    }
}

/// Host-command acknowledgement result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum HostAckDisposition {
    /// Command completed at the host boundary.
    Accepted {
        /// Session allocated by a create command.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_session_id: Option<SessionId>,
        /// Updated path-free project summary for a project mutation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project: Option<crate::ProjectSummary>,
        /// A path-free catalog changed without one natural project result.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        catalog_changed: bool,
    },
    /// Command was rejected before mutation.
    Rejected {
        /// Sanitized public reason.
        error: SanitizedError,
    },
}

/// Exact host acknowledgement cached by device and command identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostCommandAck {
    /// Protocol major.
    pub protocol: u16,
    /// Target host.
    pub host_id: HostId,
    /// Idempotency key.
    pub command_id: CommandId,
    /// Host acknowledgement time.
    pub acknowledged_at_ms: u64,
    /// Catalog revision after the operation.
    pub catalog_cursor: CatalogCursor,
    /// Accepted/rejected disposition.
    pub disposition: HostAckDisposition,
}

impl HostCommandAck {
    /// Accepted create-session acknowledgement.
    pub fn accepted(
        host_id: HostId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        catalog_cursor: CatalogCursor,
        created_session_id: SessionId,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            host_id,
            command_id,
            acknowledged_at_ms,
            catalog_cursor,
            disposition: HostAckDisposition::Accepted {
                created_session_id: Some(created_session_id),
                project: None,
                catalog_changed: false,
            },
        }
    }

    /// Accepted project mutation acknowledgement.
    pub fn accepted_project(
        host_id: HostId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        catalog_cursor: CatalogCursor,
        project: Option<crate::ProjectSummary>,
    ) -> Self {
        let catalog_changed = project.is_none();
        Self {
            protocol: PROTOCOL_VERSION,
            host_id,
            command_id,
            acknowledged_at_ms,
            catalog_cursor,
            disposition: HostAckDisposition::Accepted {
                created_session_id: None,
                project,
                catalog_changed,
            },
        }
    }

    /// Rejected host acknowledgement.
    pub fn rejected(
        host_id: HostId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        catalog_cursor: CatalogCursor,
        error: SanitizedError,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            host_id,
            command_id,
            acknowledged_at_ms,
            catalog_cursor,
            disposition: HostAckDisposition::Rejected { error },
        }
    }
}

/// Host-ingested attachment reference.
///
/// Raw bytes and arbitrary client or host paths travel through a separate
/// authenticated ingest service and never appear in this command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentRef {
    /// Opaque authenticated upload/content handle.
    pub handle: String,
    /// Safe display name.
    pub display_name: String,
    /// Validated media type.
    pub media_type: String,
    /// Expected byte length.
    pub byte_len: u64,
}

/// User input submitted, steered, or queued.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptInput {
    /// Exact model-bound text. The UI adds no hidden prompt instructions.
    pub text: String,
    /// Authenticated host-ingested attachments.
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// Uploaded documents already bound to this project/session by the host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub document_ids: Vec<DocumentId>,
    /// Immutable snapshots selected from the trusted project-file index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_file_ids: Vec<FileEntryId>,
}

/// User-selected slash invocation routed through the session owner rather than the model prompt path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SlashCommandInvocation {
    /// Exact slash-prefixed input typed in the composer.
    pub invocation: String,
}

/// Typed answer to one opaque public request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RequestAnswer {
    /// Approval response.
    Approval {
        /// Explicit allow or deny.
        allowed: bool,
    },
    /// Free-form input.
    Text {
        /// Bounded answer.
        text: String,
    },
    /// One host-defined choice.
    Choice {
        /// Exact selected choice.
        choice: String,
    },
}

/// Commands routed to the actor that exclusively owns a Ygg session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all_fields = "camelCase")]
pub enum SessionCommand {
    /// Start one run at an idle boundary.
    #[serde(rename = "session.submitPrompt")]
    SubmitPrompt {
        /// User input.
        input: PromptInput,
    },
    /// Inject input at the next supported model-turn boundary.
    #[serde(rename = "session.steer")]
    Steer {
        /// User input.
        input: PromptInput,
    },
    /// Queue input after the active run settles.
    #[serde(rename = "session.followUp")]
    FollowUp {
        /// User input.
        input: PromptInput,
    },
    /// Invoke a host-admitted slash command without turning it into a model prompt.
    #[serde(rename = "session.invokeSlashCommand")]
    InvokeSlashCommand {
        /// Exact slash-prefixed invocation.
        invocation: SlashCommandInvocation,
    },
    /// Invoke one current semantic action by stable extension/action identity.
    #[serde(rename = "extension.invokeAction")]
    InvokeExtensionAction {
        /// Manifest-bound extension name.
        extension: String,
        /// Host-created process-instance fence shown to the user.
        extension_instance_id: String,
        /// Active extension process generation shown to the user.
        generation: u64,
        /// Presentation revision shown to the user.
        revision: u64,
        /// Stable action ID from that presentation revision.
        action: String,
        /// User-approved destructive action, bound to this authenticated command.
        #[serde(default)]
        confirmed: bool,
    },
    /// Replace or create the durable session goal.
    #[serde(rename = "session.goal.set")]
    SetGoal {
        /// Bounded objective.
        objective: String,
        /// Maximum automatic continuation turns, or unlimited when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_budget: Option<u32>,
    },
    /// Pause the durable session goal.
    #[serde(rename = "session.goal.pause")]
    PauseGoal,
    /// Resume the durable session goal.
    #[serde(rename = "session.goal.resume")]
    ResumeGoal,
    /// Clear the durable session goal.
    #[serde(rename = "session.goal.clear")]
    ClearGoal,
    /// Stop the current run.
    #[serde(rename = "session.abort")]
    Abort {
        /// Expected run when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
    },
    /// Resolve one public request.
    #[serde(rename = "session.answerRequest")]
    AnswerRequest {
        /// Opaque request identity.
        request_id: RequestId,
        /// Typed answer.
        answer: RequestAnswer,
    },
    /// Change provider/model at a safe idle boundary.
    #[serde(rename = "session.changeModel")]
    ChangeModel {
        /// Provider identity.
        provider: String,
        /// Canonical model identity.
        model: String,
    },
    /// Change reasoning selection at a safe idle boundary.
    #[serde(rename = "session.changeReasoning")]
    ChangeReasoning {
        /// Product reasoning selection.
        reasoning: String,
    },
    /// Change session agent authority within the host-configured ceiling.
    #[serde(rename = "session.setAuthority")]
    SetAuthority {
        /// Requested authority.
        authority: AuthorityProfile,
    },
    /// Replace the user-owned session title.
    #[serde(rename = "session.rename")]
    Rename {
        /// Trimmed, bounded display title.
        title: String,
    },
    /// Change whether the session is pinned.
    #[serde(rename = "session.pin")]
    SetPinned {
        /// New pinned state.
        pinned: bool,
    },
    /// Change whether the session is archived.
    #[serde(rename = "session.archive")]
    SetArchived {
        /// New archived state.
        archived: bool,
    },
    /// Select an existing durable entry as the branch point for future work.
    #[serde(rename = "session.checkout")]
    Checkout {
        /// Exact preserved Ygg entry identity.
        entry_id: DurableEntryId,
    },
    /// Replace an earlier committed user turn on a sibling durable branch.
    #[serde(rename = "session.editUserTurn")]
    EditUserTurn {
        /// Exact committed user entry being replaced.
        source_user_entry_id: DurableEntryId,
        /// Replacement input.
        input: PromptInput,
    },
    /// Retry an assistant response from its originating user checkpoint.
    #[serde(rename = "session.retryResponse")]
    RetryResponse {
        /// Exact committed assistant entry being retried.
        source_assistant_entry_id: DurableEntryId,
        /// Optional explicit alternate model and reasoning selection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelSelection>,
    },
    /// Copy a committed checkpoint into a genuinely new durable session.
    #[serde(rename = "session.forkConversation")]
    ForkConversation {
        /// Exact committed checkpoint copied into the new session.
        entry_id: DurableEntryId,
    },
}

/// Idempotent session command envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCommandEnvelope {
    /// Protocol major.
    pub protocol: u16,
    /// Target host.
    pub host_id: HostId,
    /// Authenticated paired-device identity.
    pub device_id: DeviceId,
    /// Target session.
    pub session_id: SessionId,
    /// Stable idempotency key.
    pub command_id: CommandId,
    /// Device timestamp for display/audit only.
    pub issued_at_ms: u64,
    /// Expected session-owner generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_actor_generation: Option<u64>,
    /// Typed command.
    pub command: SessionCommand,
}

impl SessionCommandEnvelope {
    /// Creates a protocol-v1 envelope.
    pub fn new(
        host_id: HostId,
        device_id: DeviceId,
        session_id: SessionId,
        command_id: CommandId,
        issued_at_ms: u64,
        expected_actor_generation: Option<u64>,
        command: SessionCommand,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            host_id,
            device_id,
            session_id,
            command_id,
            issued_at_ms,
            expected_actor_generation,
            command,
        }
    }
}

/// Stable acknowledgement result.
///
/// Identical duplicate commands return the exact cached acknowledgement and
/// are never relabelled or executed twice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AckDisposition {
    /// Command reached the owning driver.
    Accepted {
        /// Run created/admitted by this command when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
        /// New session created by a conversation fork.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_session_id: Option<SessionId>,
    },
    /// Command was rejected before or at the driver boundary.
    Rejected {
        /// Sanitized public reason.
        error: SanitizedError,
    },
}

/// Exact acknowledgement cached by command identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandAck {
    /// Protocol major.
    pub protocol: u16,
    /// Target session.
    pub session_id: SessionId,
    /// Idempotency key.
    pub command_id: CommandId,
    /// Host acknowledgement time.
    pub acknowledged_at_ms: u64,
    /// Latest cursor after immediate projection.
    pub cursor: SessionCursor,
    /// Accepted/rejected disposition.
    pub disposition: AckDisposition,
}

impl CommandAck {
    /// Accepted acknowledgement.
    pub fn accepted(
        session_id: SessionId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        cursor: SessionCursor,
        run_id: Option<RunId>,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            session_id,
            command_id,
            acknowledged_at_ms,
            cursor,
            disposition: AckDisposition::Accepted {
                run_id,
                created_session_id: None,
            },
        }
    }

    /// Accepted acknowledgement for a new-session conversation fork.
    pub fn accepted_fork(
        session_id: SessionId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        cursor: SessionCursor,
        created_session_id: SessionId,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            session_id,
            command_id,
            acknowledged_at_ms,
            cursor,
            disposition: AckDisposition::Accepted {
                run_id: None,
                created_session_id: Some(created_session_id),
            },
        }
    }

    /// Rejected acknowledgement.
    pub fn rejected(
        session_id: SessionId,
        command_id: CommandId,
        acknowledged_at_ms: u64,
        cursor: SessionCursor,
        error: SanitizedError,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            session_id,
            command_id,
            acknowledged_at_ms,
            cursor,
            disposition: AckDisposition::Rejected { error },
        }
    }

    /// Returns the public error for a rejected acknowledgement.
    pub fn error(&self) -> Option<&SanitizedError> {
        match &self.disposition {
            AckDisposition::Accepted { .. } => None,
            AckDisposition::Rejected { error } => Some(error),
        }
    }
}

fn validate_media_type(value: &str) -> Result<(), ValidationError> {
    validate_public_text("attachment.media_type", value, 255, false)?;
    if !value.contains('/')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'+' | b'-' | b'.'))
    {
        return Err(ValidationError::new(
            "attachment.media_type",
            "is not a safe media type",
        ));
    }
    Ok(())
}

impl ProtocolValidation for AttachmentRef {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("attachment.handle", &self.handle, 256, false)?;
        validate_public_text("attachment.display_name", &self.display_name, 512, false)?;
        validate_media_type(&self.media_type)?;
        if self.byte_len == 0 || self.byte_len > crate::MAX_ATTACHMENT_FILE_BYTES as u64 {
            return Err(ValidationError::new(
                "attachment.byte_len",
                format!(
                    "must be within 1..={} bytes",
                    crate::MAX_ATTACHMENT_FILE_BYTES
                ),
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for PromptInput {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text("command.input.text", &self.text, MAX_PROMPT_BYTES, true)?;
        if self.text.trim().is_empty()
            && self.attachments.is_empty()
            && self.document_ids.is_empty()
            && self.project_file_ids.is_empty()
        {
            return Err(ValidationError::new(
                "command.input",
                "requires text, an attachment, an uploaded document, or a project file",
            ));
        }
        if self.attachments.len() > crate::MAX_ATTACHMENT_COUNT {
            return Err(ValidationError::new(
                "command.input.attachments",
                format!(
                    "exceeds the {}-attachment limit",
                    crate::MAX_ATTACHMENT_COUNT
                ),
            ));
        }
        let mut handles = BTreeSet::new();
        let mut total_bytes = 0u64;
        for attachment in &self.attachments {
            attachment.validate()?;
            if !handles.insert(attachment.handle.as_str()) {
                return Err(ValidationError::new(
                    "command.input.attachments",
                    "contains a duplicate attachment handle",
                ));
            }
            total_bytes = total_bytes
                .checked_add(attachment.byte_len)
                .ok_or_else(|| {
                    ValidationError::new(
                        "command.input.attachments",
                        "exceeds the aggregate byte limit",
                    )
                })?;
            if total_bytes > crate::MAX_ATTACHMENT_TOTAL_BYTES as u64 {
                return Err(ValidationError::new(
                    "command.input.attachments",
                    format!(
                        "exceeds the {}-byte aggregate limit",
                        crate::MAX_ATTACHMENT_TOTAL_BYTES
                    ),
                ));
            }
        }
        if self.document_ids.len() > crate::MAX_DOCUMENTS_PER_PROMPT {
            return Err(ValidationError::new(
                "command.input.document_ids",
                format!(
                    "exceeds the {}-document limit",
                    crate::MAX_DOCUMENTS_PER_PROMPT
                ),
            ));
        }
        let mut document_ids = BTreeSet::new();
        for document_id in &self.document_ids {
            if !document_ids.insert(document_id.as_str()) {
                return Err(ValidationError::new(
                    "command.input.document_ids",
                    "contains a duplicate document ID",
                ));
            }
        }
        if self.project_file_ids.len() > crate::MAX_TRUSTED_FILES_PER_CONTEXT {
            return Err(ValidationError::new(
                "command.input.project_file_ids",
                format!(
                    "exceeds the {}-file limit",
                    crate::MAX_TRUSTED_FILES_PER_CONTEXT
                ),
            ));
        }
        let mut project_file_ids = BTreeSet::new();
        for entry_id in &self.project_file_ids {
            if !project_file_ids.insert(entry_id.as_str()) {
                return Err(ValidationError::new(
                    "command.input.project_file_ids",
                    "contains a duplicate project-file ID",
                ));
            }
        }
        Ok(())
    }
}

impl ProtocolValidation for SlashCommandInvocation {
    fn validate(&self) -> Result<(), ValidationError> {
        validate_public_text(
            "command.slash.invocation",
            &self.invocation,
            MAX_PROMPT_BYTES,
            false,
        )?;
        if !self.invocation.starts_with('/') || self.invocation[1..].trim().is_empty() {
            return Err(ValidationError::new(
                "command.slash.invocation",
                "must contain a slash-prefixed command name",
            ));
        }
        Ok(())
    }
}

impl ProtocolValidation for SessionCommandEnvelope {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "command.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        if self.expected_actor_generation.is_none() {
            return Err(ValidationError::new(
                "command.expected_actor_generation",
                "is required for every remote session mutation",
            ));
        }
        match &self.command {
            SessionCommand::SubmitPrompt { input }
            | SessionCommand::Steer { input }
            | SessionCommand::FollowUp { input }
            | SessionCommand::EditUserTurn { input, .. } => input.validate()?,
            SessionCommand::InvokeSlashCommand { invocation } => invocation.validate()?,
            SessionCommand::InvokeExtensionAction {
                extension,
                extension_instance_id,
                generation,
                action,
                ..
            } => {
                if *generation == 0 {
                    return Err(ValidationError::new(
                        "command.extension.generation",
                        "must identify an active extension generation",
                    ));
                }
                for (field, value) in [
                    ("command.extension.extension", extension),
                    (
                        "command.extension.extension_instance_id",
                        extension_instance_id,
                    ),
                    ("command.extension.action", action),
                ] {
                    validate_public_text(field, value, 128, false)?;
                    if value.chars().any(char::is_whitespace) {
                        return Err(ValidationError::new(
                            field,
                            "must be a whitespace-free stable identifier",
                        ));
                    }
                }
            }
            SessionCommand::SetGoal {
                objective,
                turn_budget,
            } => {
                validate_public_text(
                    "command.goal.objective",
                    objective,
                    MAX_GOAL_OBJECTIVE_BYTES,
                    false,
                )?;
                if turn_budget.is_some_and(|budget| budget == 0 || budget > MAX_GOAL_TURN_BUDGET) {
                    return Err(ValidationError::new(
                        "command.goal.turnBudget",
                        "must be absent or within the durable goal budget limit",
                    ));
                }
            }
            SessionCommand::PauseGoal
            | SessionCommand::ResumeGoal
            | SessionCommand::ClearGoal
            | SessionCommand::Abort { .. } => {}
            SessionCommand::AnswerRequest { answer, .. } => match answer {
                RequestAnswer::Approval { .. } => {}
                RequestAnswer::Text { text } => {
                    validate_public_text("command.answer.text", text, 64 * 1024, true)?;
                }
                RequestAnswer::Choice { choice } => {
                    validate_public_text("command.answer.choice", choice, 1024, false)?;
                }
            },
            SessionCommand::ChangeModel { provider, model } => {
                validate_public_text("command.provider", provider, 128, false)?;
                validate_public_text("command.model", model, 256, false)?;
            }
            SessionCommand::ChangeReasoning { reasoning } => {
                validate_public_text("command.reasoning", reasoning, 128, false)?;
            }
            SessionCommand::SetAuthority { .. } => {}
            SessionCommand::Rename { title } => {
                validate_public_text("command.title", title, 512, false)?;
            }
            SessionCommand::SetPinned { .. }
            | SessionCommand::SetArchived { .. }
            | SessionCommand::Checkout { .. }
            | SessionCommand::ForkConversation { .. } => {}
            SessionCommand::RetryResponse { model, .. } => {
                if let Some(model) = model {
                    model.validate()?;
                }
            }
        }
        validate_serialized_size("command", self, MAX_COMMAND_BYTES)
    }
}

impl ProtocolValidation for HostCommandEnvelope {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "host_command.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        match &self.command {
            HostCommand::CreateSession { model, .. } => {
                if let Some(model) = model {
                    model.validate()?;
                }
            }
            HostCommand::ImportProject {
                candidate_id,
                display_name,
            } => {
                validate_public_text("host_command.candidate_id", candidate_id, 256, false)?;
                if candidate_id.is_empty()
                    || !candidate_id.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                    })
                {
                    return Err(ValidationError::new(
                        "host_command.candidate_id",
                        "must be an opaque host-issued identifier",
                    ));
                }
                if let Some(display_name) = display_name {
                    validate_public_text("host_command.display_name", display_name, 160, false)?;
                }
            }
            HostCommand::RenameProject { display_name, .. } => {
                validate_public_text("host_command.display_name", display_name, 160, false)?;
            }
            HostCommand::SetDefaultProject { .. }
            | HostCommand::ClearDefaultProject
            | HostCommand::SetProjectTrust { .. }
            | HostCommand::ArchiveProject { .. }
            | HostCommand::SetSessionLifecycle { .. } => {}
            HostCommand::DeleteSessionPermanently {
                session_id,
                confirmation,
            } => {
                if &confirmation.session_id != session_id
                    || confirmation.trashed_at_ms == 0
                    || confirmation.phrase != format!("permanently delete {}", session_id.as_str())
                {
                    return Err(ValidationError::new(
                        "host_command.confirmation",
                        "must exactly confirm the target session and current trash timestamp",
                    ));
                }
            }
        }
        validate_serialized_size("host_command", self, MAX_COMMAND_BYTES)
    }
}

impl ProtocolValidation for CommandAck {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "ack.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        match &self.disposition {
            AckDisposition::Accepted {
                run_id,
                created_session_id,
            } => {
                if run_id.is_some() && created_session_id.is_some() {
                    return Err(ValidationError::new(
                        "ack.disposition",
                        "cannot contain both a run and a created session",
                    ));
                }
            }
            AckDisposition::Rejected { error } => error.validate()?,
        }
        validate_serialized_size("ack", self, MAX_COMMAND_BYTES)
    }
}

impl ProtocolValidation for HostCommandAck {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(ValidationError::new(
                "host_ack.protocol",
                format!("must equal protocol major {PROTOCOL_VERSION}"),
            ));
        }
        match &self.disposition {
            HostAckDisposition::Accepted {
                created_session_id,
                project,
                catalog_changed,
            } => {
                let result_count = usize::from(created_session_id.is_some())
                    + usize::from(project.is_some())
                    + usize::from(*catalog_changed);
                if result_count != 1 {
                    return Err(ValidationError::new(
                        "host_ack.disposition",
                        "must contain exactly one created session, updated project, or catalog change",
                    ));
                }
                if let Some(project) = project {
                    project.validate()?;
                }
            }
            HostAckDisposition::Rejected { error } => error.validate()?,
        }
        validate_serialized_size("host_ack", self, MAX_COMMAND_BYTES)
    }
}

impl From<ValidationError> for SanitizedError {
    fn from(error: ValidationError) -> Self {
        Self::public(ErrorCode::InvalidCommand, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_command(command_id: &str, command: SessionCommand) -> SessionCommandEnvelope {
        SessionCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            SessionId::new("session-test").unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            Some(1),
            command,
        )
    }

    fn host_command(command_id: &str, command: HostCommand) -> HostCommandEnvelope {
        HostCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            CommandId::new(command_id).unwrap(),
            1,
            command,
        )
    }

    fn attachment(index: usize, byte_len: u64) -> AttachmentRef {
        AttachmentRef {
            handle: format!("handle-{index}"),
            display_name: format!("image-{index}.png"),
            media_type: "image/png".into(),
            byte_len,
        }
    }

    #[test]
    fn attachment_only_and_exact_policy_boundaries_are_valid() {
        let eight = PromptInput {
            text: String::new(),
            attachments: (0..crate::MAX_ATTACHMENT_COUNT)
                .map(|index| attachment(index, 1))
                .collect(),
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        eight.validate().unwrap();
        let exact_total = PromptInput {
            text: "four full images".into(),
            attachments: (0..4)
                .map(|index| attachment(index, crate::MAX_ATTACHMENT_FILE_BYTES as u64))
                .collect(),
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        exact_total.validate().unwrap();
    }

    #[test]
    fn count_file_total_zero_and_duplicate_bounds_are_rejected() {
        let over_count = PromptInput {
            text: "too many".into(),
            attachments: (0..=crate::MAX_ATTACHMENT_COUNT)
                .map(|index| attachment(index, 1))
                .collect(),
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        assert!(over_count.validate().is_err());

        let zero = PromptInput {
            text: "zero".into(),
            attachments: vec![attachment(0, 0)],
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        assert!(zero.validate().is_err());

        let over_file = PromptInput {
            text: "large".into(),
            attachments: vec![attachment(0, crate::MAX_ATTACHMENT_FILE_BYTES as u64 + 1)],
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        assert!(over_file.validate().is_err());

        let over_total = PromptInput {
            text: "aggregate".into(),
            attachments: vec![
                attachment(0, crate::MAX_ATTACHMENT_FILE_BYTES as u64),
                attachment(1, crate::MAX_ATTACHMENT_FILE_BYTES as u64),
                attachment(2, crate::MAX_ATTACHMENT_FILE_BYTES as u64),
                attachment(3, crate::MAX_ATTACHMENT_FILE_BYTES as u64),
                attachment(4, 1),
            ],
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        assert!(over_total.validate().is_err());

        let duplicate = attachment(0, 1);
        let duplicates = PromptInput {
            text: "duplicate".into(),
            attachments: vec![duplicate.clone(), duplicate],
            document_ids: Vec::new(),
            project_file_ids: Vec::new(),
        };
        assert!(duplicates.validate().is_err());
    }

    #[test]
    fn session_metadata_commands_have_exact_validated_wire_names() {
        let commands = [
            (
                session_command(
                    "command-rename",
                    SessionCommand::Rename {
                        title: "Renamed session".into(),
                    },
                ),
                "session.rename",
            ),
            (
                session_command("command-pin", SessionCommand::SetPinned { pinned: true }),
                "session.pin",
            ),
            (
                session_command(
                    "command-archive",
                    SessionCommand::SetArchived { archived: true },
                ),
                "session.archive",
            ),
            (
                session_command(
                    "command-checkout",
                    SessionCommand::Checkout {
                        entry_id: DurableEntryId::new("entry-17").unwrap(),
                    },
                ),
                "session.checkout",
            ),
        ];

        for (envelope, expected_type) in commands {
            envelope.validate().unwrap();
            let value = serde_json::to_value(&envelope).unwrap();
            assert_eq!(value["command"]["type"], expected_type);
            assert_eq!(
                serde_json::from_value::<SessionCommandEnvelope>(value).unwrap(),
                envelope
            );
        }

        assert!(session_command(
            "command-blank-title",
            SessionCommand::Rename {
                title: " \n ".into(),
            },
        )
        .validate()
        .is_err());
    }

    #[test]
    fn conversation_branch_commands_have_exact_validated_wire_contracts() {
        let commands = [
            (
                session_command(
                    "command-edit",
                    SessionCommand::EditUserTurn {
                        source_user_entry_id: DurableEntryId::new("entry-user").unwrap(),
                        input: PromptInput {
                            text: "replacement".into(),
                            attachments: Vec::new(),
                            document_ids: Vec::new(),
                            project_file_ids: Vec::new(),
                        },
                    },
                ),
                "session.editUserTurn",
            ),
            (
                session_command(
                    "command-retry",
                    SessionCommand::RetryResponse {
                        source_assistant_entry_id: DurableEntryId::new("entry-assistant").unwrap(),
                        model: Some(ModelSelection {
                            provider: "openai".into(),
                            model: "gpt-test".into(),
                            reasoning: "high".into(),
                        }),
                    },
                ),
                "session.retryResponse",
            ),
            (
                session_command(
                    "command-fork",
                    SessionCommand::ForkConversation {
                        entry_id: DurableEntryId::new("entry-checkpoint").unwrap(),
                    },
                ),
                "session.forkConversation",
            ),
        ];

        for (envelope, expected_type) in commands {
            envelope.validate().unwrap();
            let value = serde_json::to_value(&envelope).unwrap();
            assert_eq!(value["command"]["type"], expected_type);
            assert_eq!(
                serde_json::from_value::<SessionCommandEnvelope>(value).unwrap(),
                envelope
            );
        }

        let ack = CommandAck::accepted_fork(
            SessionId::new("source-session").unwrap(),
            CommandId::new("command-fork").unwrap(),
            7,
            SessionCursor {
                actor_generation: 1,
                sequence: 11,
            },
            SessionId::new("created-session").unwrap(),
        );
        ack.validate().unwrap();
        let value = serde_json::to_value(&ack).unwrap();
        assert_eq!(value["disposition"]["createdSessionId"], "created-session");
        assert!(value["disposition"].get("runId").is_none());
    }

    #[test]
    fn trash_and_permanent_delete_are_bound_to_exact_session_generation() {
        let lifecycle = host_command(
            "command-trash",
            HostCommand::SetSessionLifecycle {
                session_id: SessionId::new("session-trash").unwrap(),
                lifecycle: SessionCatalogState::Trash,
            },
        );
        lifecycle.validate().unwrap();
        assert_eq!(
            serde_json::to_value(&lifecycle).unwrap()["command"],
            serde_json::json!({
                "type": "session.setLifecycle",
                "data": {
                    "sessionId": "session-trash",
                    "lifecycle": "trash"
                }
            })
        );

        let valid = host_command(
            "command-delete",
            HostCommand::DeleteSessionPermanently {
                session_id: SessionId::new("session-trash").unwrap(),
                confirmation: PermanentDeleteConfirmation {
                    session_id: SessionId::new("session-trash").unwrap(),
                    trashed_at_ms: 42,
                    phrase: "permanently delete session-trash".into(),
                },
            },
        );
        valid.validate().unwrap();

        for confirmation in [
            PermanentDeleteConfirmation {
                session_id: SessionId::new("other-session").unwrap(),
                trashed_at_ms: 42,
                phrase: "permanently delete session-trash".into(),
            },
            PermanentDeleteConfirmation {
                session_id: SessionId::new("session-trash").unwrap(),
                trashed_at_ms: 0,
                phrase: "permanently delete session-trash".into(),
            },
            PermanentDeleteConfirmation {
                session_id: SessionId::new("session-trash").unwrap(),
                trashed_at_ms: 42,
                phrase: "delete session-trash".into(),
            },
        ] {
            assert!(host_command(
                "command-invalid-delete",
                HostCommand::DeleteSessionPermanently {
                    session_id: SessionId::new("session-trash").unwrap(),
                    confirmation,
                },
            )
            .validate()
            .is_err());
        }
    }

    #[test]
    fn project_import_accepts_only_an_opaque_host_candidate() {
        let envelope = HostCommandEnvelope::new(
            HostId::new("host-test").unwrap(),
            DeviceId::new("device-test").unwrap(),
            CommandId::new("command-import").unwrap(),
            1,
            HostCommand::ImportProject {
                candidate_id: "candidate-opaque-1".into(),
                display_name: Some("Project".into()),
            },
        );
        envelope.validate().unwrap();
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            value["command"],
            serde_json::json!({
                "type": "project.import",
                "data": {
                    "candidateId": "candidate-opaque-1",
                    "displayName": "Project"
                }
            })
        );
        assert!(
            serde_json::from_value::<HostCommandEnvelope>(serde_json::json!({
                "protocol": 1,
                "hostId": "host-test",
                "deviceId": "device-test",
                "commandId": "command-import",
                "issuedAtMs": 1,
                "command": {
                    "type": "project.import",
                    "data": {
                        "candidateId": "candidate-opaque-1",
                        "rootPath": "/private/host/path"
                    }
                }
            }))
            .is_err(),
            "an arbitrary browser-authored host path must be rejected, not ignored"
        );
    }
}
