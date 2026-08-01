//! Aggregate prompt-context boundary coverage.

#[allow(dead_code)]
#[path = "../src/bounds.rs"]
mod bounds;
#[allow(dead_code)]
#[path = "../src/prompt_context.rs"]
mod prompt_context;

use bounds::MAX_PROMPT_BYTES;
use prompt_context::{
    compose_prompt_text, PromptContextError, MAX_AUXILIARY_PROMPT_CONTEXT_BYTES,
    MAX_DOCUMENT_CONTEXT_BYTES, MAX_PROJECT_FILE_CONTEXT_BYTES,
};

#[test]
fn composition_accounts_for_user_context_and_inserted_separators() {
    let composed = compose_prompt_text(
        "user request",
        Some("document context"),
        Some("file context"),
    )
    .unwrap();
    assert_eq!(
        composed.as_str(),
        "user request\n\ndocument context\n\nfile context"
    );
    assert_eq!(composed.user_text_bytes(), "user request".len());
    assert_eq!(composed.document_context_bytes(), "document context".len());
    assert_eq!(composed.project_file_context_bytes(), "file context".len());
    assert_eq!(composed.total_bytes(), composed.as_str().len());
    assert!(composed.total_bytes() <= MAX_PROMPT_BYTES);
    assert!(!format!("{composed:?}").contains("user request"));
    assert_eq!(
        composed.clone().into_string(),
        "user request\n\ndocument context\n\nfile context"
    );
}

#[test]
fn individual_auxiliary_source_budgets_fail_closed() {
    assert_eq!(
        compose_prompt_text(
            "user",
            Some(&"d".repeat(MAX_DOCUMENT_CONTEXT_BYTES + 1)),
            None
        ),
        Err(PromptContextError::DocumentContextTooLarge)
    );
    assert_eq!(
        compose_prompt_text(
            "user",
            None,
            Some(&"f".repeat(MAX_PROJECT_FILE_CONTEXT_BYTES + 1))
        ),
        Err(PromptContextError::ProjectFileContextTooLarge)
    );
}

#[test]
fn combined_context_and_final_prompt_never_cross_the_protocol_cap() {
    let documents = "d".repeat(MAX_DOCUMENT_CONTEXT_BYTES);
    let files = "f".repeat(MAX_PROJECT_FILE_CONTEXT_BYTES);
    assert_eq!(
        compose_prompt_text("", Some(&documents), Some(&files)),
        Err(PromptContextError::AuxiliaryContextTooLarge)
    );
    assert_eq!(
        MAX_AUXILIARY_PROMPT_CONTEXT_BYTES,
        MAX_DOCUMENT_CONTEXT_BYTES + MAX_PROJECT_FILE_CONTEXT_BYTES
    );

    let documents = "d".repeat(80 * 1024);
    let files = "f".repeat(80 * 1024);
    let remaining = MAX_PROMPT_BYTES - documents.len() - files.len() - 4;
    let user = "u".repeat(remaining);
    let exact = compose_prompt_text(&user, Some(&documents), Some(&files)).unwrap();
    assert_eq!(exact.total_bytes(), MAX_PROMPT_BYTES);
    assert_eq!(
        compose_prompt_text(&format!("{user}u"), Some(&documents), Some(&files)),
        Err(PromptContextError::PromptTooLarge)
    );
}

#[test]
fn invalid_user_text_and_empty_sections_are_handled_consistently() {
    assert_eq!(
        compose_prompt_text("unsafe\0text", None, None),
        Err(PromptContextError::InvalidUserText)
    );
    assert_eq!(
        compose_prompt_text("user", Some("unsafe\u{202e}document"), None),
        Err(PromptContextError::InvalidDocumentContext)
    );
    assert_eq!(
        compose_prompt_text("user", None, Some("unsafe\u{202e}file")),
        Err(PromptContextError::InvalidProjectFileContext)
    );
    assert_eq!(
        compose_prompt_text("", Some(""), Some("file"))
            .unwrap()
            .as_str(),
        "file"
    );
    assert_eq!(compose_prompt_text("", None, None).unwrap().as_str(), "");
}
