#![allow(missing_docs)]

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use sexy_tui_rs::key_text;

/// Actions produced by the pure terminal-event translator.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InputAction {
    Abort,
    Steer(String),
    Submit(String),
    Command(String),
    CompleteSlashCommand,
    SlashMenu(SlashMenuAction),
    CompleteMention,
    ShowCompactionSummary,
    /// Toggle verbose transcript mode for all expandable blocks (ctrl+o).
    ExpandFocusedTool,
    /// Cycle to the next thinking level supported by the active model.
    CycleThinking,
    Edit(EditAction),
    Resize(u16, u16),
    /// Page-based transcript navigation from PageUp/PageDown.
    Scroll(i16),
    /// Small incremental movement from a mouse wheel or trackpad.
    ScrollLines(i16),
    /// Explicit return to the newest transcript output (Ctrl+End).
    JumpToTail,
    /// Application-owned transcript selection actions. Ctrl+C remains
    /// interruption/close and is never repurposed.
    SelectAllTranscript,
    CopyTranscriptSelection,
    /// A pointer gesture that began in the transcript. Coordinates remain
    /// terminal cells; the shell converts them to durable document anchors.
    TranscriptPointer(PointerGesture),
    Close,
    Closed,
    Ignore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashMenuAction {
    Previous,
    Next,
    First,
    Last,
    PageUp,
    PageDown,
    Select,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PointerGesture {
    Begin { row: u16, col: u16, extend: bool },
    Extend { row: u16, col: u16 },
    End { row: u16, col: u16 },
}

/// Editor mutations understood by the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditAction {
    Char(char),
    /// A bracketed-paste payload. It is inserted verbatim by the editor rather
    /// than being interpreted as key presses or command submission.
    Paste(String),
    Backspace,
    Delete,
    Newline,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
}

fn is_command_submission(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter && key.modifiers.is_empty())
        || (key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL)
}

/// Key repeats are useful for text editing and navigation, but must never
/// replay one-shot actions such as submit, close, abort, confirm, or toggle.
pub(crate) fn accepts_key_event(key: &KeyEvent) -> bool {
    match key.kind {
        KeyEventKind::Press => true,
        KeyEventKind::Release => false,
        KeyEventKind::Repeat => match key.code {
            KeyCode::Char(_) => key_text(key).is_some(),
            KeyCode::Backspace => !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER),
            KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => key.modifiers.is_empty(),
            _ => false,
        },
    }
}

/// Translate one crossterm event according to the product keymap.
#[cfg(test)]
pub fn translate(event: Option<Event>, active: bool, editor_text: &str) -> InputAction {
    translate_with_popup(event, active, editor_text, true)
}

/// Translate an event while respecting application-owned popup dismissal.
pub fn translate_with_popup(
    event: Option<Event>,
    active: bool,
    editor_text: &str,
    slash_popup_open: bool,
) -> InputAction {
    let Some(event) = event else {
        return InputAction::Closed;
    };

    match event {
        Event::Resize(columns, rows) => InputAction::Resize(columns, rows),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => InputAction::ScrollLines(-3),
            MouseEventKind::ScrollDown => InputAction::ScrollLines(3),
            MouseEventKind::Down(MouseButton::Left) => {
                InputAction::TranscriptPointer(PointerGesture::Begin {
                    row: mouse.row,
                    col: mouse.column,
                    extend: mouse.modifiers.contains(KeyModifiers::SHIFT),
                })
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                InputAction::TranscriptPointer(PointerGesture::Extend {
                    row: mouse.row,
                    col: mouse.column,
                })
            }
            MouseEventKind::Moved => InputAction::TranscriptPointer(PointerGesture::Extend {
                row: mouse.row,
                col: mouse.column,
            }),
            MouseEventKind::Up(MouseButton::Left) => {
                InputAction::TranscriptPointer(PointerGesture::End {
                    row: mouse.row,
                    col: mouse.column,
                })
            }
            _ => InputAction::Ignore,
        },
        Event::Paste(text) => InputAction::Edit(EditAction::Paste(text)),
        Event::Key(key) => {
            if !accepts_key_event(&key) {
                return InputAction::Ignore;
            }

            let control_c =
                key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL;
            let control_d =
                key.code == KeyCode::Char('d') && key.modifiers == KeyModifiers::CONTROL;
            if control_c || control_d {
                return if active {
                    InputAction::Abort
                } else {
                    InputAction::Closed
                };
            }

            let modified_enter = (key.code == KeyCode::Enter
                || matches!(key.code, KeyCode::Char('\n' | '\r')))
                && (key.modifiers == KeyModifiers::SHIFT
                    || key.modifiers == KeyModifiers::ALT
                    || key.modifiers.contains(KeyModifiers::CONTROL));
            if modified_enter {
                return InputAction::Edit(EditAction::Newline);
            }

            let slash_menu = slash_popup_open
                && editor_text.starts_with('/')
                && !editor_text.chars().any(char::is_whitespace)
                && (editor_text == "/"
                    || !crate::tui::composer::looks_like_absolute_path(editor_text));
            if slash_menu && key.modifiers.is_empty() {
                let action = match key.code {
                    KeyCode::Up => Some(SlashMenuAction::Previous),
                    KeyCode::Down => Some(SlashMenuAction::Next),
                    KeyCode::Home => Some(SlashMenuAction::First),
                    KeyCode::End => Some(SlashMenuAction::Last),
                    KeyCode::PageUp => Some(SlashMenuAction::PageUp),
                    KeyCode::PageDown => Some(SlashMenuAction::PageDown),
                    KeyCode::Enter => Some(SlashMenuAction::Select),
                    KeyCode::Esc => Some(SlashMenuAction::Close),
                    _ => None,
                };
                if let Some(action) = action {
                    return InputAction::SlashMenu(action);
                }
            }

            if key.code == KeyCode::PageUp && key.modifiers.is_empty() {
                return InputAction::Scroll(-1);
            }
            if key.code == KeyCode::PageDown && key.modifiers.is_empty() {
                return InputAction::Scroll(1);
            }
            if key.code == KeyCode::End && key.modifiers.contains(KeyModifiers::CONTROL) {
                return InputAction::JumpToTail;
            }
            if key.code == KeyCode::Char('a')
                && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                return InputAction::SelectAllTranscript;
            }
            if key.code == KeyCode::Char('c')
                && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            {
                return InputAction::CopyTranscriptSelection;
            }
            if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                return if active {
                    InputAction::Abort
                } else {
                    InputAction::Close
                };
            }

            if is_command_submission(&key)
                && editor_text.starts_with('/')
                && !crate::tui::composer::looks_like_absolute_path(editor_text)
            {
                return InputAction::Command(editor_text.to_owned());
            }
            if key.code == KeyCode::Tab
                && key.modifiers.is_empty()
                && editor_text.starts_with('/')
                && !crate::tui::composer::looks_like_absolute_path(editor_text)
            {
                return InputAction::CompleteSlashCommand;
            }
            if key.code == KeyCode::Tab
                && key.modifiers.is_empty()
                && crate::tui::composer::active_mention(editor_text).is_some()
            {
                return InputAction::CompleteMention;
            }

            if key.code == KeyCode::BackTab
                || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
            {
                return InputAction::CycleThinking;
            }

            match (active, key.code, key.modifiers) {
                (false, KeyCode::Enter, modifiers) if modifiers.is_empty() => {
                    if editor_text.is_empty() {
                        InputAction::Ignore
                    } else {
                        InputAction::Submit(editor_text.to_owned())
                    }
                }
                (true, KeyCode::Enter, modifiers) if modifiers.is_empty() => {
                    if editor_text.is_empty() {
                        InputAction::Ignore
                    } else {
                        InputAction::Steer(editor_text.to_owned())
                    }
                }
                (true, KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                    if editor_text.is_empty() {
                        InputAction::Ignore
                    } else {
                        InputAction::Steer(editor_text.to_owned())
                    }
                }
                (_, KeyCode::Char('o'), KeyModifiers::CONTROL) => InputAction::ExpandFocusedTool,
                (_, KeyCode::Backspace, modifiers)
                    if !modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    InputAction::Edit(EditAction::Backspace)
                }
                (_, KeyCode::Delete, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Delete)
                }
                (_, KeyCode::Left, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Left)
                }
                (_, KeyCode::Right, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Right)
                }
                (_, KeyCode::Up, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Up)
                }
                (_, KeyCode::Down, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Down)
                }
                (_, KeyCode::Home, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::Home)
                }
                (_, KeyCode::End, modifiers) if modifiers.is_empty() => {
                    InputAction::Edit(EditAction::End)
                }
                (_, KeyCode::Char(_), _) => key_text(&key)
                    .map(|character| InputAction::Edit(EditAction::Char(character)))
                    .unwrap_or(InputAction::Ignore),
                _ => InputAction::Ignore,
            }
        }
        _ => InputAction::Ignore,
    }
}

/// Encode a crossterm key the way sexy-tui's private terminal encoder does.
#[allow(dead_code)]
pub fn encode(key: &KeyEvent) -> String {
    let has_modifier = !key.modifiers.is_empty();
    if has_modifier {
        let mut modifier = 1u8;
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            modifier += 1;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            modifier += 2;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            modifier += 4;
        }
        if key.modifiers.contains(KeyModifiers::SUPER) {
            modifier += 8;
        }

        let codepoint = match key.code {
            KeyCode::Char(character) => Some(character as u32),
            KeyCode::Enter => Some(13),
            KeyCode::Tab => Some(9),
            KeyCode::Backspace => Some(127),
            KeyCode::Esc => Some(27),
            _ => None,
        };
        if let Some(codepoint) = codepoint {
            return format!("\x1b[{codepoint};{modifier}u");
        }
    }

    match key.code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Enter => "\r".to_string(),
        KeyCode::Tab => "\t".to_string(),
        KeyCode::Backspace => "\x7f".to_string(),
        KeyCode::Esc => "\x1b".to_string(),
        KeyCode::Up => "\x1b[A".to_string(),
        KeyCode::Down => "\x1b[B".to_string(),
        KeyCode::Left => "\x1b[D".to_string(),
        KeyCode::Right => "\x1b[C".to_string(),
        KeyCode::Home => "\x1b[H".to_string(),
        KeyCode::End => "\x1b[F".to_string(),
        KeyCode::Delete => "\x1b[3~".to_string(),
        KeyCode::Insert => "\x1b[2~".to_string(),
        KeyCode::PageUp => "\x1b[5~".to_string(),
        KeyCode::PageDown => "\x1b[6~".to_string(),
        KeyCode::F(number @ 1..=12) => {
            let code = match number {
                1 => "11",
                2 => "12",
                3 => "13",
                4 => "14",
                5 => "15",
                6 => "17",
                7 => "18",
                8 => "19",
                9 => "20",
                10 => "21",
                11 => "23",
                12 => "24",
                _ => unreachable!(),
            };
            format!("\x1b[{code}~")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn control_c_aborts_active_and_closes_idle() {
        let event = Some(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(translate(event.clone(), true, ""), InputAction::Abort);
        assert_eq!(translate(event, false, ""), InputAction::Closed);
    }

    #[test]
    fn control_d_aborts_active_and_closes_idle() {
        let event = Some(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(translate(event.clone(), true, ""), InputAction::Abort);
        assert_eq!(translate(event, false, ""), InputAction::Closed);
    }

    #[test]
    fn submit_and_active_controls_use_editor_text() {
        assert_eq!(
            translate(
                Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                false,
                "hello"
            ),
            InputAction::Submit("hello".into())
        );
        assert_eq!(
            translate(Some(key(KeyCode::Enter, KeyModifiers::NONE)), true, "hello"),
            InputAction::Steer("hello".into())
        );
        assert_eq!(
            translate(
                Some(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                true,
                "steer"
            ),
            InputAction::Steer("steer".into())
        );
        assert_eq!(
            translate(
                Some(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                false,
                "normal"
            ),
            InputAction::Ignore
        );
    }

    #[test]
    fn slash_enter_selects_the_popup_then_submits_after_it_closes() {
        for active in [false, true] {
            assert_eq!(
                translate(
                    Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                    active,
                    "/status"
                ),
                InputAction::SlashMenu(SlashMenuAction::Select)
            );
            assert_eq!(
                translate_with_popup(
                    Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                    active,
                    "/status",
                    false,
                ),
                InputAction::Command("/status".into())
            );
            assert_eq!(
                translate(
                    Some(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                    active,
                    "/model"
                ),
                InputAction::Command("/model".into())
            );
        }
    }

    #[test]
    fn absolute_paths_are_submitted_as_prompts_not_slash_commands() {
        // The path need not exist: users commonly paste a destination that
        // will be created by the agent. Its second separator is enough to
        // distinguish it from a command token.
        assert_eq!(
            translate(
                Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                false,
                "/Users/example/project/new-file.txt"
            ),
            InputAction::Submit("/Users/example/project/new-file.txt".into())
        );
        // Recognized commands remain commands even when an argument contains
        // a slash.
        assert_eq!(
            translate(
                Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                false,
                "/model provider/foo"
            ),
            InputAction::Command("/model provider/foo".into())
        );
        assert_eq!(
            translate(
                Some(key(KeyCode::Tab, KeyModifiers::NONE)),
                false,
                "/Users/example/project/new-file.txt"
            ),
            InputAction::Ignore
        );
    }

    #[test]
    fn slash_tab_requests_completion() {
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "/mod"),
            InputAction::CompleteSlashCommand
        );
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "hello"),
            InputAction::Ignore
        );
    }

    #[test]
    fn tab_on_trailing_at_token_requests_mention_completion() {
        assert_eq!(
            translate(
                Some(key(KeyCode::Tab, KeyModifiers::NONE)),
                false,
                "see @sr"
            ),
            InputAction::CompleteMention
        );
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "plain"),
            InputAction::Ignore
        );
        assert_eq!(
            translate(Some(key(KeyCode::Tab, KeyModifiers::NONE)), false, "/mod"),
            InputAction::CompleteSlashCommand
        );
    }

    #[test]
    fn enhanced_keyboard_text_uses_the_terminal_character_not_a_us_shift_map() {
        // Legacy terminals send the text itself.
        assert_eq!(
            translate(Some(key(KeyCode::Char('A'), KeyModifiers::NONE)), false, ""),
            InputAction::Edit(EditAction::Char('A'))
        );
        assert_eq!(
            translate(Some(key(KeyCode::Char('!'), KeyModifiers::NONE)), false, ""),
            InputAction::Edit(EditAction::Char('!'))
        );

        // With REPORT_ALTERNATE_KEYS, crossterm resolves Kitty's
        // `97:65;2u` and `49:33;2u` payloads to these text events. Shift has
        // intentionally been consumed by the parser: the character is the
        // keyboard-layout-correct text, not a physical US key.
        assert_eq!(
            translate(Some(key(KeyCode::Char('A'), KeyModifiers::NONE)), false, ""),
            InputAction::Edit(EditAction::Char('A'))
        );
        assert_eq!(
            translate(Some(key(KeyCode::Char('!'), KeyModifiers::NONE)), false, ""),
            InputAction::Edit(EditAction::Char('!'))
        );

        let caps = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('Q'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::CAPS_LOCK,
        );
        assert_eq!(
            translate(Some(Event::Key(caps)), false, ""),
            InputAction::Edit(EditAction::Char('Q'))
        );
    }

    #[test]
    fn option_and_altgr_text_are_not_mistaken_for_shortcuts() {
        assert_eq!(
            translate(Some(key(KeyCode::Char('é'), KeyModifiers::ALT)), false, ""),
            InputAction::Edit(EditAction::Char('é'))
        );
        // Several terminal/platform combinations expose AltGr as Ctrl+Alt.
        assert_eq!(
            translate(
                Some(key(
                    KeyCode::Char('€'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT,
                )),
                false,
                "",
            ),
            InputAction::Edit(EditAction::Char('€'))
        );
        // Plain Ctrl remains reserved for controls rather than text insertion.
        assert_eq!(
            translate(
                Some(key(KeyCode::Char('x'), KeyModifiers::CONTROL)),
                false,
                ""
            ),
            InputAction::Ignore
        );
        assert_eq!(
            translate(
                Some(key(KeyCode::Char('x'), KeyModifiers::SUPER)),
                false,
                "",
            ),
            InputAction::Ignore
        );
    }

    #[test]
    fn enhanced_text_repeats_but_releases_do_not() {
        let repeated =
            KeyEvent::new_with_kind(KeyCode::Char('é'), KeyModifiers::ALT, KeyEventKind::Repeat);
        assert_eq!(
            translate(Some(Event::Key(repeated)), false, ""),
            InputAction::Edit(EditAction::Char('é'))
        );
        let released = KeyEvent::new_with_kind(
            KeyCode::Char('€'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyEventKind::Release,
        );
        assert_eq!(
            translate(Some(Event::Key(released)), false, ""),
            InputAction::Ignore
        );
    }

    #[test]
    fn editor_mutations_and_stream_end_are_translated() {
        assert_eq!(
            translate(Some(key(KeyCode::Char('x'), KeyModifiers::NONE)), false, ""),
            InputAction::Edit(EditAction::Char('x'))
        );
        assert_eq!(
            translate(
                Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
                false,
                "x"
            ),
            InputAction::Edit(EditAction::Backspace)
        );
        assert_eq!(
            translate(Some(key(KeyCode::Enter, KeyModifiers::ALT)), false, "x"),
            InputAction::Edit(EditAction::Newline)
        );
        assert_eq!(
            translate(Some(key(KeyCode::Enter, KeyModifiers::CONTROL)), false, "x"),
            InputAction::Edit(EditAction::Newline)
        );
        // A few terminal integrations preserve the modified control byte as a
        // character instead of normalizing it to KeyCode::Enter.
        assert_eq!(
            translate(
                Some(key(KeyCode::Char('\n'), KeyModifiers::CONTROL)),
                false,
                "x"
            ),
            InputAction::Edit(EditAction::Newline)
        );
        assert_eq!(
            translate(Some(key(KeyCode::Enter, KeyModifiers::SHIFT)), false, "x"),
            InputAction::Edit(EditAction::Newline)
        );
        assert_eq!(
            translate(Some(Event::Paste("first\nsecond".into())), false, ""),
            InputAction::Edit(EditAction::Paste("first\nsecond".into()))
        );
        assert_eq!(
            translate(Some(key(KeyCode::Enter, KeyModifiers::NONE)), false, ""),
            InputAction::Ignore
        );
        assert_eq!(translate(None, false, ""), InputAction::Closed);
    }

    #[test]
    fn held_keys_repeat_editor_actions_while_release_is_ignored() {
        let repeated =
            KeyEvent::new_with_kind(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            translate(Some(Event::Key(repeated)), false, "text"),
            InputAction::Edit(EditAction::Backspace)
        );
        let released = KeyEvent::new_with_kind(
            KeyCode::Backspace,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(
            translate(Some(Event::Key(released)), false, "text"),
            InputAction::Ignore
        );

        for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Tab] {
            let repeated = KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Repeat);
            assert_eq!(
                translate(Some(Event::Key(repeated)), false, "text"),
                InputAction::Ignore,
                "one-shot {code:?} must not auto-repeat"
            );
        }
        let repeated_toggle = KeyEvent::new_with_kind(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
            KeyEventKind::Repeat,
        );
        assert_eq!(
            translate(Some(Event::Key(repeated_toggle)), false, "text"),
            InputAction::Ignore
        );

        let repeated_character =
            KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Repeat);
        assert_eq!(
            translate(Some(Event::Key(repeated_character)), false, "text"),
            InputAction::Edit(EditAction::Char('x'))
        );
    }

    #[test]
    fn shift_tab_cycles_thinking_as_a_one_shot_action() {
        assert_eq!(
            translate(
                Some(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
                false,
                "draft",
            ),
            InputAction::CycleThinking
        );
        assert_eq!(
            translate(
                Some(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::BackTab,
                    KeyModifiers::SHIFT,
                    KeyEventKind::Repeat,
                ))),
                false,
                "draft",
            ),
            InputAction::Ignore
        );
    }

    #[test]
    fn ctrl_o_is_a_one_shot_tool_output_toggle_while_idle_or_running() {
        let ctrl_o = Some(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert_eq!(
            translate(ctrl_o.clone(), false, "draft prompt"),
            InputAction::ExpandFocusedTool
        );
        assert_eq!(
            translate(ctrl_o, true, "queued steering"),
            InputAction::ExpandFocusedTool
        );
    }

    #[test]
    fn editor_navigation_and_mouse_scroll_are_available_to_the_shell() {
        assert_eq!(
            translate(Some(key(KeyCode::Left, KeyModifiers::NONE)), false, "x"),
            InputAction::Edit(EditAction::Left)
        );
        assert_eq!(
            translate(Some(key(KeyCode::Delete, KeyModifiers::NONE)), false, "x"),
            InputAction::Edit(EditAction::Delete)
        );
        assert_eq!(
            translate(
                Some(Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::ScrollUp,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                })),
                false,
                ""
            ),
            InputAction::ScrollLines(-3)
        );
        assert_eq!(
            translate(
                Some(Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 11,
                    row: 3,
                    modifiers: KeyModifiers::NONE,
                })),
                false,
                ""
            ),
            InputAction::TranscriptPointer(PointerGesture::Extend { row: 3, col: 11 })
        );
    }

    #[test]
    fn resize_scroll_and_escape_are_available_to_the_shell() {
        assert_eq!(
            translate(Some(Event::Resize(100, 30)), false, ""),
            InputAction::Resize(100, 30)
        );
        assert_eq!(
            translate(Some(key(KeyCode::PageUp, KeyModifiers::NONE)), false, ""),
            InputAction::Scroll(-1)
        );
        assert_eq!(
            translate(Some(key(KeyCode::End, KeyModifiers::CONTROL)), false, ""),
            InputAction::JumpToTail
        );
        assert_eq!(
            translate(
                Some(key(
                    KeyCode::Char('a'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )),
                false,
                ""
            ),
            InputAction::SelectAllTranscript
        );
        assert_eq!(
            translate(
                Some(key(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )),
                false,
                ""
            ),
            InputAction::CopyTranscriptSelection
        );
        assert_eq!(
            translate(Some(key(KeyCode::Esc, KeyModifiers::NONE)), false, ""),
            InputAction::Close
        );
        assert_eq!(
            translate(Some(key(KeyCode::Esc, KeyModifiers::NONE)), true, ""),
            InputAction::Abort
        );
    }

    #[test]
    fn slash_popup_owns_all_navigation_keys_and_escape() {
        let cases = [
            (KeyCode::Up, SlashMenuAction::Previous),
            (KeyCode::Down, SlashMenuAction::Next),
            (KeyCode::Home, SlashMenuAction::First),
            (KeyCode::End, SlashMenuAction::Last),
            (KeyCode::PageUp, SlashMenuAction::PageUp),
            (KeyCode::PageDown, SlashMenuAction::PageDown),
            (KeyCode::Enter, SlashMenuAction::Select),
            (KeyCode::Esc, SlashMenuAction::Close),
        ];
        for (key_code, expected) in cases {
            assert_eq!(
                translate(Some(key(key_code, KeyModifiers::NONE)), false, "/"),
                InputAction::SlashMenu(expected)
            );
        }
    }

    #[test]
    fn key_encoder_matches_sexy_tui_sequences() {
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            "\x1b[A"
        );
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            "\r"
        );
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            "\x1b"
        );
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            "x"
        );
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            "\x1b[97;5u"
        );
        assert_eq!(
            encode(&KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            "\x1b[13;3u"
        );
    }
}
