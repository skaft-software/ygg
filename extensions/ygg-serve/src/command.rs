//! Bounded idempotent session commands and acknowledgements.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::bounds::{
    validate_public_text, validate_serialized_size, ProtocolValidation, ValidationError,
    MAX_COMMAND_BYTES, MAX_PROMPT_BYTES,
};
use crate::{
    AuthorityProfile, CatalogCursor, CommandId, DeviceId, DurableEntryId, ErrorCode, HostId,
    ModelSelection, ProjectId, RequestId, RunId, SanitizedError, SessionCursor, SessionId,
    PROTOCOL_VERSION,
};

/// Host-scoped commands that do not target an existing session actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all_fields = "camelCase")]
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
        created_session_id: SessionId,
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
            disposition: HostAckDisposition::Accepted { created_session_id },
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
            disposition: AckDisposition::Accepted { run_id },
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
        if self.text.trim().is_empty() && self.attachments.is_empty() {
            return Err(ValidationError::new(
                "command.input",
                "requires text or at least one attachment",
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
            | SessionCommand::FollowUp { input } => input.validate()?,
            SessionCommand::Abort { .. } => {}
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
            | SessionCommand::Checkout { .. } => {}
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
        if let AckDisposition::Rejected { error } = &self.disposition {
            error.validate()?;
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
        if let HostAckDisposition::Rejected { error } = &self.disposition {
            error.validate()?;
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
        };
        eight.validate().unwrap();
        let exact_total = PromptInput {
            text: "four full images".into(),
            attachments: (0..4)
                .map(|index| attachment(index, crate::MAX_ATTACHMENT_FILE_BYTES as u64))
                .collect(),
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
        };
        assert!(over_count.validate().is_err());

        let zero = PromptInput {
            text: "zero".into(),
            attachments: vec![attachment(0, 0)],
        };
        assert!(zero.validate().is_err());

        let over_file = PromptInput {
            text: "large".into(),
            attachments: vec![attachment(0, crate::MAX_ATTACHMENT_FILE_BYTES as u64 + 1)],
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
        };
        assert!(over_total.validate().is_err());

        let duplicate = attachment(0, 1);
        let duplicates = PromptInput {
            text: "duplicate".into(),
            attachments: vec![duplicate.clone(), duplicate],
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
}
