//! Read-only session documents rendered through the interactive transcript pipeline.

use ygg_agent::Session;

use super::transcript_hydration::append_hydrated_items;
use super::transcript_render::render_block_planned;
use super::{fit_line, ShellState};
use crate::hydrate::hydrate_transcript_tail;
use crate::tui::theme::YggTheme;

const MAX_TRANSCRIPT_ENTRIES: usize = 64;
const MAX_RENDERED_BLOCK_BYTES: usize = 16 * 1024;
const MAX_DOCUMENT_BYTES: usize = 128 * 1024;

fn joined_len(lines: &[String]) -> usize {
    lines
        .iter()
        .map(String::len)
        .sum::<usize>()
        .saturating_add(lines.len().saturating_sub(1))
}

/// Keep both the semantic lead and live tail when one unusually large block
/// would otherwise consume the complete read-only document. Rich transcript
/// rows close their own ANSI state, so retaining whole rows is terminal-safe.
fn bounded_rendered_block(lines: Vec<String>, theme: &YggTheme, width: u16) -> String {
    if joined_len(&lines) <= MAX_RENDERED_BLOCK_BYTES {
        return lines.join("\n");
    }

    let marker = fit_line(
        &theme.dim("[middle of oversized transcript entry omitted]"),
        width,
    );
    let available = MAX_RENDERED_BLOCK_BYTES.saturating_sub(marker.len().saturating_add(2));
    let head_budget = available / 2;
    let tail_budget = available.saturating_sub(head_budget);

    let mut head_end = 0usize;
    let mut head_bytes = 0usize;
    for line in &lines {
        let additional = line.len().saturating_add(usize::from(head_end > 0));
        if head_bytes.saturating_add(additional) > head_budget {
            break;
        }
        head_bytes += additional;
        head_end += 1;
    }

    let mut tail_start = lines.len();
    let mut tail_bytes = 0usize;
    while tail_start > head_end {
        let index = tail_start - 1;
        let additional = lines[index]
            .len()
            .saturating_add(usize::from(tail_start < lines.len()));
        if tail_bytes.saturating_add(additional) > tail_budget {
            break;
        }
        tail_bytes += additional;
        tail_start = index;
    }

    let mut bounded = Vec::with_capacity(
        head_end
            .saturating_add(lines.len().saturating_sub(tail_start))
            .saturating_add(1),
    );
    bounded.extend(lines[..head_end].iter().cloned());
    bounded.push(marker);
    bounded.extend(lines[tail_start..].iter().cloned());
    let bounded = bounded.join("\n");
    debug_assert!(bounded.len() <= MAX_RENDERED_BLOCK_BYTES);
    bounded
}

/// Render a delegated session with the same semantic hydration, tool cards,
/// Markdown, syntax highlighting, prompt surfaces, and transition geometry as
/// the main interactive transcript. Only the outer read-only panel is distinct.
pub(crate) fn delegated_session_document(
    session: &Session,
    theme: &YggTheme,
    width: u16,
    verbose_tools: bool,
) -> anyhow::Result<String> {
    let width = width.max(1);
    let (items, history_truncated) = hydrate_transcript_tail(session, MAX_TRANSCRIPT_ENTRIES)?;
    let mut state = ShellState {
        theme: theme.clone(),
        verbose_tools,
        ..ShellState::default()
    };
    append_hydrated_items(&mut state, items);

    let rich_renderer = theme.rich_renderer();
    let reasoning_renderer = theme.reasoning_renderer();
    let blocks = state
        .transcript
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            let rendered = render_block_planned(
                index
                    .checked_sub(1)
                    .and_then(|previous| state.transcript.get(previous)),
                block,
                theme,
                &rich_renderer,
                &reasoning_renderer,
                width,
                verbose_tools,
                0,
            )
            .lines;
            (!rendered.is_empty()).then(|| bounded_rendered_block(rendered, theme, width))
        })
        .collect::<Vec<_>>();

    if blocks.is_empty() {
        return Ok(theme.dim("No durable transcript entries are available yet."));
    }

    let omitted_marker = theme.dim("[older transcript entries omitted]");
    let mut selected_start = blocks.len();
    let mut selected_bytes = 0usize;
    for index in (0..blocks.len()).rev() {
        let separator = usize::from(selected_start < blocks.len());
        let older_would_be_omitted = history_truncated || index > 0;
        let marker_bytes = if older_would_be_omitted {
            omitted_marker.len().saturating_add(1)
        } else {
            0
        };
        if selected_bytes
            .saturating_add(separator)
            .saturating_add(blocks[index].len())
            .saturating_add(marker_bytes)
            > MAX_DOCUMENT_BYTES
        {
            break;
        }
        selected_start = index;
        selected_bytes = selected_bytes
            .saturating_add(separator)
            .saturating_add(blocks[index].len());
    }

    let omitted = history_truncated || selected_start > 0;
    let body = blocks[selected_start..].join("\n");
    let output = if omitted {
        format!("{omitted_marker}\n{body}")
    } else {
        body
    };
    debug_assert!(output.len() <= MAX_DOCUMENT_BYTES);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::InteractiveShell;
    use ygg_agent::EntryValue;
    use ygg_ai::{
        AssistantMessage, AssistantPart, Message, ModelId, Protocol, UserMessage, UserPart,
    };

    #[test]
    fn delegated_document_matches_the_main_hydrated_transcript_rows() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = Session::create(directory.path().join("child.jsonl")).unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("Inspect the renderer.".into())],
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text(
                    "# Result\n\nA **styled** worker response with `code`.".into(),
                )],
                model: ModelId("worker-test".into()),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();

        let theme = crate::tui::theme::test_theme();
        let width = 76;
        let mut shell = InteractiveShell::test_shell();
        shell.set_theme(theme.clone());
        shell.set_size(width, 20);
        shell.hydrate(&session).unwrap();
        let expected = shell.state.borrow().rendered_transcript(width).join("\n");
        let actual = delegated_session_document(&session, &theme, width, false).unwrap();

        assert_eq!(actual, expected);
    }
}
