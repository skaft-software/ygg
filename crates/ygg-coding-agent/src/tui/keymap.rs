#![allow(missing_docs)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};

/// Actions produced by the pure terminal-event translator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputAction {
    Abort,
    Steer(String),
    FollowUp(String),
    Submit(String),
    Command(String),
    CompleteSlashCommand,
    CompleteMention,
    Edit(EditAction),
    Resize(u16, u16),
    /// Page-based transcript navigation from PageUp/PageDown.
    Scroll(i16),
    /// Small incremental movement from a mouse wheel or trackpad.
    ScrollLines(i16),
    Close,
    Closed,
    Ignore,
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

/// Translate one crossterm event according to the product keymap.
pub fn translate(event: Option<Event>, active: bool, editor_text: &str) -> InputAction {
    let Some(event) = event else {
        return InputAction::Closed;
    };

    match event {
        Event::Resize(columns, rows) => InputAction::Resize(columns, rows),
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => InputAction::ScrollLines(-3),
            MouseEventKind::ScrollDown => InputAction::ScrollLines(3),
            _ => InputAction::Ignore,
        },
        Event::Paste(text) => InputAction::Edit(EditAction::Paste(text)),
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return InputAction::Ignore;
            }

            let control_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            let control_d =
                key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL);
            if control_c || control_d {
                return if active {
                    InputAction::Abort
                } else {
                    InputAction::Closed
                };
            }

            if key.code == KeyCode::Enter
                && (key.modifiers == KeyModifiers::ALT
                    || key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return InputAction::Edit(EditAction::Newline);
            }

            if key.code == KeyCode::PageUp && key.modifiers.is_empty() {
                return InputAction::Scroll(-1);
            }
            if key.code == KeyCode::PageDown && key.modifiers.is_empty() {
                return InputAction::Scroll(1);
            }
            if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                return if active {
                    InputAction::Abort
                } else {
                    InputAction::Close
                };
            }

            if is_command_submission(&key) && editor_text.starts_with('/') {
                return InputAction::Command(editor_text.to_owned());
            }
            if key.code == KeyCode::Tab && key.modifiers.is_empty() && editor_text.starts_with('/')
            {
                return InputAction::CompleteSlashCommand;
            }
            if key.code == KeyCode::Tab
                && key.modifiers.is_empty()
                && crate::tui::composer::active_mention(editor_text).is_some()
            {
                return InputAction::CompleteMention;
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
                        InputAction::FollowUp(editor_text.to_owned())
                    }
                }
                (true, KeyCode::Char('s'), KeyModifiers::CONTROL) => {
                    if editor_text.is_empty() {
                        InputAction::Ignore
                    } else {
                        InputAction::Steer(editor_text.to_owned())
                    }
                }
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
                (_, KeyCode::Char(character), modifiers)
                    if !modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) && !character.is_control() =>
                {
                    InputAction::Edit(EditAction::Char(character))
                }
                _ => InputAction::Ignore,
            }
        }
        _ => InputAction::Ignore,
    }
}

/// Encode a crossterm key the way sexy-tui's private terminal encoder does.
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
            InputAction::FollowUp("hello".into())
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
    fn slash_text_is_always_an_application_command_on_submit() {
        for active in [false, true] {
            assert_eq!(
                translate(
                    Some(key(KeyCode::Enter, KeyModifiers::NONE)),
                    active,
                    "/status"
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
            translate(Some(key(KeyCode::Esc, KeyModifiers::NONE)), false, ""),
            InputAction::Close
        );
        assert_eq!(
            translate(Some(key(KeyCode::Esc, KeyModifiers::NONE)), true, ""),
            InputAction::Abort
        );
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
