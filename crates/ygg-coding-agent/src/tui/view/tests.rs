use std::collections::HashSet;

use sexy_tui_rs::{Block, Inline};

use super::bash_render::render_compact_bash_output;
use super::surface_layout::compile_surface_plan;
use super::tool_render::{
    tool_grid_label, tool_value_indent, tool_value_indent_width, without_redundant_tool_lead,
};
use super::transcript_commit::{
    transcript_commit_cursor, transcript_commit_position, FINAL_COMMIT_SEGMENT,
};
use super::*;
use crate::commands;
use crate::presentation::RunPhase;
use crate::tui::theme::ThemeSurfaceHeading;
use sexy_tui_rs::CURSOR_MARKER;

struct EmulatedTerminal {
    size: Arc<Mutex<(u16, u16)>>,
    bytes: Arc<Mutex<Vec<u8>>>,
    synchronized_output: bool,
}

impl EmulatedTerminal {
    fn push(&self, bytes: &[u8]) {
        self.bytes
            .lock()
            .expect("emulated terminal output mutex poisoned")
            .extend_from_slice(bytes);
    }
}

impl sexy_tui_rs::Terminal for EmulatedTerminal {
    fn start_events(
        &mut self,
        _on_input: Box<dyn FnMut(sexy_tui_rs::TerminalInput)>,
        _on_resize: Box<dyn FnMut()>,
    ) {
    }

    fn stop(&mut self) {}

    fn write(&mut self, data: &str) {
        // The production primary-screen terminal uses the normal output
        // post-processing convention where LF returns to column zero.
        // vt100 deliberately models raw bytes, so make that convention
        // explicit in the test backend.
        let mut previous = None;
        for byte in data.bytes() {
            if byte == b'\n' && previous != Some(b'\r') {
                self.push(b"\r");
            }
            self.push(&[byte]);
            previous = Some(byte);
        }
    }

    fn columns(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").0
    }

    fn rows(&self) -> u16 {
        self.size.lock().expect("terminal size mutex poisoned").1
    }

    fn move_by(&mut self, lines: i16) {
        match lines.cmp(&0) {
            std::cmp::Ordering::Less => {
                self.push(format!("\x1b[{}A", lines.unsigned_abs()).as_bytes());
            }
            std::cmp::Ordering::Greater => {
                self.push(format!("\x1b[{}B", lines.unsigned_abs()).as_bytes());
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    fn hide_cursor(&mut self) {
        self.push(b"\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.push(b"\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.push(b"\x1b[0m\x1b[2K");
    }

    fn clear_from_cursor(&mut self) {
        self.push(b"\x1b[0m\x1b[0J");
    }

    fn clear_screen(&mut self) {
        self.push(b"\x1b[0m\x1b[2J");
    }

    fn capabilities(&self) -> sexy_tui_rs::TerminalCapabilities {
        let mut capabilities = sexy_tui_rs::TerminalCapabilities::interactive(
            sexy_tui_rs::ColorDepth::TrueColor,
            true,
        );
        capabilities.synchronized_output = self.synchronized_output;
        capabilities.sync_output = self.synchronized_output;
        capabilities
    }
}

fn test_effective_tool_policy() -> ygg_agent::EffectiveToolPolicy {
    ygg_agent::SandboxConfig::new(".").effective_tool_policy(ygg_agent::EffectPolicy::Controlled)
}

fn inherited_delegation_provenance() -> ygg_agent::DelegationOrchestrationProvenance {
    ygg_agent::DelegationOrchestrationProvenance::all(
        ygg_agent::DelegationPolicySource::ParentInherited,
    )
}

fn emulated_shell(
    theme: YggTheme,
    width: u16,
    height: u16,
) -> (InteractiveShell, Arc<Mutex<Vec<u8>>>) {
    emulated_shell_with_sync(theme, width, height, false)
}

fn emulated_shell_with_sync(
    theme: YggTheme,
    width: u16,
    height: u16,
    synchronized_output: bool,
) -> (InteractiveShell, Arc<Mutex<Vec<u8>>>) {
    emulated_shell_with_mode(theme, width, height, synchronized_output, false)
}

fn emulated_shell_with_mode(
    theme: YggTheme,
    width: u16,
    height: u16,
    synchronized_output: bool,
    application_viewport: bool,
) -> (InteractiveShell, Arc<Mutex<Vec<u8>>>) {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let size = Arc::new(Mutex::new((width, height)));
    let state = SharedState::new(ShellState {
        theme,
        size: (width, height),
        follow_tail: true,
        application_viewport_requested: application_viewport,
        ..ShellState::default()
    });
    let mut tui = TUI::new(Box::new(EmulatedTerminal {
        size: size.clone(),
        bytes: bytes.clone(),
        synchronized_output,
    }));
    tui.add_child(Box::new(ShellComponent::new(
        state.clone(),
        application_viewport,
    )));
    tui.start();
    (
        InteractiveShell {
            tui: Some(tui),
            state,
            size,
            render_tx: None,
            render_thread: None,
            capture_mouse: application_viewport,
        },
        bytes,
    )
}

fn lazy_history_test_shell() -> InteractiveShell {
    let mut shell = InteractiveShell::test_shell();
    shell.capture_mouse = true;
    shell.state.borrow_mut().application_viewport_requested = true;
    shell
}

fn session_with_user_prompts(path: &std::path::Path, prefix: &str, count: usize) -> Session {
    use ygg_ai::{Message, UserMessage, UserPart};

    let mut session = Session::create(path).unwrap();
    for index in 0..count {
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(format!("{prefix} {index}"))],
            })))
            .unwrap();
    }
    session
}

fn emulate_rows(lines: &[String], width: u16) -> vt100::Parser {
    let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX).max(1);
    let mut terminal = vt100::Parser::new(rows, width, 0);
    for (index, line) in lines.iter().enumerate() {
        terminal.process(line.as_bytes());
        if index + 1 < lines.len() {
            terminal.process(b"\r\n");
        }
    }
    terminal
}

fn assert_input_suggestions_replace_status_footer(
    shell: &mut InteractiveShell,
    expected_hint: &str,
) {
    const WIDTH: u16 = 120;
    shell.set_size(WIDTH, 24);
    shell.set_context_estimate(80, 272_000);

    let state = shell.state.borrow();
    let now = Instant::now();
    let standalone_composer =
        crate::tui::composer_surface::render_composer_surface(&state, WIDTH, now);
    assert!(strip_terminal_sequences(
        standalone_composer
            .last()
            .expect("standalone composer should include its status footer")
    )
    .contains("context 0%/272K"));

    let chrome = shell_chrome(&state, WIDTH, now);
    assert!(!chrome.suggestions.is_empty());
    assert!(chrome
        .suggestions
        .iter()
        .any(|line| { strip_terminal_sequences(line).contains(expected_hint) }));
    assert!(
        chrome
            .composer
            .iter()
            .all(|line| !strip_terminal_sequences(line).contains("context 0%")),
        "autocomplete must replace the model and token status row"
    );

    let expected_tail = chrome
        .composer
        .iter()
        .chain(&chrome.suggestions)
        .cloned()
        .collect::<Vec<_>>();
    for (mode, rendered) in [
        ("terminal-owned", render_shell_at(&state, WIDTH, now)),
        (
            "application-owned",
            render_shell_viewport_at(&state, WIDTH, now),
        ),
    ] {
        assert!(
            rendered.ends_with(&expected_tail),
            "{mode} suggestions must replace the status row below the composer"
        );
    }
}

/// `vt100` 0.15 ignores ED 3. Recreate its grid at the final saved-line
/// clear so protocol tests can model the destructive reset before replay.
/// This deliberately does not pretend to model modern terminal reflow.
fn process_vt100_with_saved_line_clear(
    terminal: &mut vt100::Parser,
    output: &[u8],
    rows: u16,
    columns: u16,
    scrollback_len: usize,
) {
    const CLEAR_SAVED_LINES: &[u8] = b"\x1b[3J";
    if let Some(clear_at) = output
        .windows(CLEAR_SAVED_LINES.len())
        .rposition(|window| window == CLEAR_SAVED_LINES)
    {
        *terminal = vt100::Parser::new(rows, columns, scrollback_len);
        terminal.process(&output[clear_at + CLEAR_SAVED_LINES.len()..]);
    } else {
        terminal.process(output);
    }
}

fn find_ascii_cell(screen: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
    screen
        .rows(0, screen.size().1)
        .enumerate()
        .find_map(|(row, contents)| {
            contents.find(needle).map(|byte| {
                (
                    row as u16,
                    u16::try_from(visible_width(&contents[..byte])).unwrap_or(u16::MAX),
                )
            })
        })
}

fn assert_ascii_foreground(terminal: &vt100::Parser, needle: &str, expected: vt100::Color) {
    let (row, column) = find_ascii_cell(terminal.screen(), needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in {:?}", terminal.screen().contents()));
    for offset in 0..needle.len() as u16 {
        let cell = terminal
            .screen()
            .cell(row, column + offset)
            .expect("text cell inside terminal bounds");
        assert_eq!(
            cell.fgcolor(),
            expected,
            "foreground mismatch for {needle:?} at ({row}, {})",
            column + offset
        );
    }
}

fn assert_ascii_bold(terminal: &vt100::Parser, needle: &str) {
    let (row, column) = find_ascii_cell(terminal.screen(), needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in {:?}", terminal.screen().contents()));
    for offset in 0..needle.len() as u16 {
        assert!(
            terminal
                .screen()
                .cell(row, column + offset)
                .expect("text cell inside terminal bounds")
                .bold(),
            "{needle:?} was not bold at offset {offset}"
        );
    }
}

fn assert_ascii_default_rendition(terminal: &vt100::Parser, needle: &str) {
    let (row, column) = find_ascii_cell(terminal.screen(), needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in {:?}", terminal.screen().contents()));
    for offset in 0..needle.len() as u16 {
        let cell = terminal
            .screen()
            .cell(row, column + offset)
            .expect("text cell inside terminal bounds");
        assert_eq!(cell.fgcolor(), vt100::Color::Default);
        assert_eq!(cell.bgcolor(), vt100::Color::Default);
        assert!(!cell.bold(), "{needle:?} retained bold at offset {offset}");
        assert!(
            !cell.italic(),
            "{needle:?} retained italic at offset {offset}"
        );
        assert!(
            !cell.underline(),
            "{needle:?} retained underline at offset {offset}"
        );
        assert!(
            !cell.inverse(),
            "{needle:?} retained inverse at offset {offset}"
        );
    }
}

fn role_rgb_color(theme: &YggTheme, role: &str) -> vt100::Color {
    let (red, green, blue) = theme
        .role_rgb(role)
        .unwrap_or_else(|| panic!("test theme role {role:?} did not resolve to RGB"));
    vt100::Color::Rgb(red, green, blue)
}

/// Build a key-press event for panel input tests.
fn panel_key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
}

fn panel_key_with_modifiers(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new(code, modifiers))
}

fn picker_session(
    id: &str,
    title: &str,
    message_count: usize,
    modified_seconds: u64,
) -> SessionMeta {
    SessionMeta {
        id: id.to_owned(),
        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        title: title.to_owned(),
        name: None,
        tags: Vec::new(),
        pinned: false,
        archived: false,
        trashed_at_ms: None,
        purge_after_ms: None,
        forked_from_session_id: None,
        forked_from_entry_id: None,
        message_count,
        modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified_seconds),
        workspace: Some(PathBuf::from("/work")),
    }
}

fn panel_key_kind(
    code: crossterm::event::KeyCode,
    kind: crossterm::event::KeyEventKind,
) -> crossterm::event::Event {
    crossterm::event::Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        crossterm::event::KeyModifiers::NONE,
        kind,
    ))
}

/// Open a select-list panel with no descriptions.
fn open_select_panel(shell: &mut InteractiveShell, items: &[&str]) {
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        items: items.iter().map(|item| item.to_string()).collect(),
        descriptions: vec![None; items.len()],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(vec![]),
    });
}

fn panel_state(shell: &InteractiveShell) -> (Vec<String>, usize, String) {
    let state = shell.state.borrow();
    let Some(Panel::SelectList {
        items,
        selected,
        filter,
        ..
    }) = state.panel.as_ref()
    else {
        panic!("panel should be open");
    };
    (items.clone(), *selected, filter.clone())
}

#[test]
fn session_picker_ordering_filters_and_cycles_sorts() {
    let mut named = picker_session("named", "Zebra", 2, 30);
    named.name = Some("Release notes".into());
    named.tags = vec!["rust".into()];
    let rows = vec![
        picker_session("recent", "Beta", 1, 40),
        named,
        picker_session("long", "Alpha", 9, 20),
    ];
    let mut picker = PickerState::new(rows, None);

    assert_eq!(session_picker_ordering(&picker), vec![0, 1, 2]);
    picker.sort = PickerSort::Name;
    assert_eq!(session_picker_ordering(&picker), vec![2, 0, 1]);
    picker.sort = PickerSort::Messages;
    assert_eq!(session_picker_ordering(&picker), vec![2, 1, 0]);

    picker.named_only = true;
    assert_eq!(session_picker_ordering(&picker), vec![1]);
    picker.named_only = false;
    picker.filter = "rse".into();
    assert_eq!(session_picker_ordering(&picker), vec![1]);
    picker.filter = "re:beta".into();
    assert_eq!(session_picker_ordering(&picker), vec![0]);
}

#[test]
fn session_picker_panel_handles_scope_filter_and_selection_outbox() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(100, 24);
    let mut first = picker_session("one", "First", 1, 1);
    first.name = Some("First name".into());
    let rows = vec![first, picker_session("two", "Second", 2, 2)];
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(rows.clone(), Some(rows[0].path.clone())),
    });

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    shell.panel_input(&panel_key_with_modifiers(
        crossterm::event::KeyCode::Char('n'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    let state = shell.state.borrow();
    let Some(Panel::SessionPicker { picker }) = state.panel.as_ref() else {
        panic!("session picker should be open");
    };
    assert_eq!(picker.selected, 0, "named-filter changes reset selection");
    drop(state);

    // Restore the complete list; the current row is protected from deletion.
    shell.panel_input(&panel_key_with_modifiers(
        crossterm::event::KeyCode::Char('n'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Delete));
    let state = shell.state.borrow();
    let Some(Panel::SessionPicker { picker }) = state.panel.as_ref() else {
        panic!("session picker should be open");
    };
    assert!(!picker.confirming_delete);
    let OrdinarySurfaceLifecycle::RecoverableError(status) = &picker.surface.lifecycle else {
        panic!("current-session delete should set a recoverable-error lifecycle");
    };
    assert_eq!(status.text, "cannot delete the currently active session");
    drop(state);

    // Clear the named/filter state and select the second row.
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('x')));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    let (result, action) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("session selection should close the panel");
    assert_eq!(result, PanelResult::Select("two".into()));
    assert!(matches!(action, PanelAction::SessionPicker));
    assert_eq!(
        shell.take_picker_selection(),
        Some(("two".into(), PathBuf::from("/tmp/two.jsonl")))
    );
    assert!(!shell.has_panel());
}

#[test]
fn session_picker_rename_and_delete_emit_driver_requests() {
    let mut shell = InteractiveShell::test_shell();
    let row = picker_session("one", "First", 1, 1);
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(vec![row], None),
    });

    shell.panel_input(&panel_key_with_modifiers(
        crossterm::event::KeyCode::Char('r'),
        crossterm::event::KeyModifiers::CONTROL,
    ));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('X')));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Enter));
    assert!(matches!(
        shell.drain_panel_requests().as_slice(),
        [PanelRequest::RenameSession { id, name, .. }] if id == "one" && name == "X"
    ));

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Delete));
    assert!(shell.has_panel());
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Enter));
    assert!(matches!(
        shell.drain_panel_requests().as_slice(),
        [PanelRequest::TrashSession { id, .. }] if id == "one"
    ));
}

#[test]
fn message_picker_returns_selected_text_through_outbox() {
    let mut shell = InteractiveShell::test_shell();
    shell.open_panel(Panel::MessagePicker {
        picker: MessagePicker::new(vec![
            ForkMessage {
                entry_id: "entry-a".into(),
                text: "first prompt".into(),
                whole_conversation: false,
            },
            ForkMessage {
                entry_id: "entry-head".into(),
                text: String::new(),
                whole_conversation: true,
            },
        ]),
    });
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Up));
    let (result, action) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("message selection should close the panel");
    assert_eq!(result, PanelResult::Select("entry-a".into()));
    assert!(matches!(action, PanelAction::MessagePicker));
    assert_eq!(
        shell.take_message_picker_selection(),
        Some(("entry-a".into(), "first prompt".into()))
    );
}

#[test]
fn session_picker_render_shows_scope_markers_and_fork_metadata() {
    let mut shell = InteractiveShell::test_shell();
    let mut fork = picker_session("fork", "Forked", 3, 1);
    fork.pinned = true;
    fork.forked_from_session_id = Some("source".into());
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(vec![fork], None),
    });
    let raw = render_panel(&shell.state.borrow(), 100);
    let plain = raw
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    let rendered = plain.join("\n");
    assert!(rendered.contains("Resume Session (Current Folder)"));
    assert!(rendered.contains("Forked (fork)"));
    assert!(!rendered.contains("(current)"));
    assert!(rendered.contains("^s sort"));
    assert_eq!(raw.join("\n").matches(CURSOR_MARKER).count(), 1);
    assert!(plain[1].starts_with("Resume Session"), "{plain:?}");
    assert!(plain[2].starts_with("Filter"), "{plain:?}");
    let selected = plain
        .iter()
        .find(|line| line.contains("Forked (fork)"))
        .expect("selected session title");
    assert!(
        selected.starts_with("› ") || selected.starts_with("> "),
        "{plain:?}"
    );
}

#[test]
fn model_and_resume_pickers_use_the_active_model_accent() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    let (model_accent, ui_accent) = {
        let state = shell.state.borrow();
        let sequence = |role| {
            let (red, green, blue) = state
                .theme
                .role_rgb(role)
                .unwrap_or_else(|| panic!("missing {role} colour"));
            format!("\x1b[38;2;{red};{green};{blue}m")
        };
        (sequence("model_accent"), sequence("accent"))
    };
    assert_ne!(model_accent, ui_accent);

    open_select_panel(&mut shell, &["Claude Sonnet 4", "GPT-5"]);
    let model_rows = render_panel(&shell.state.borrow(), 80);
    let selected_model = model_rows
        .iter()
        .find(|line| strip_terminal_sequences(line).contains("Claude Sonnet 4"))
        .expect("selected model row");
    assert!(selected_model.contains(&model_accent), "{model_rows:?}");
    assert!(!selected_model.contains(&ui_accent), "{model_rows:?}");

    shell.close_panel();
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(vec![picker_session("one", "First session", 1, 1)], None),
    });
    let resume_rows = render_panel(&shell.state.borrow(), 100);
    let selected_session = resume_rows
        .iter()
        .find(|line| strip_terminal_sequences(line).contains("First session"))
        .expect("selected resume row");
    assert!(selected_session.contains(&model_accent), "{resume_rows:?}");
    assert!(!selected_session.contains(&ui_accent), "{resume_rows:?}");
    let active_scope = resume_rows
        .iter()
        .find(|line| strip_terminal_sequences(line).contains("Current Folder"))
        .expect("active resume scope");
    assert!(active_scope.contains(&model_accent), "{resume_rows:?}");
    assert!(!active_scope.contains(&ui_accent), "{resume_rows:?}");
}

#[test]
fn wide_session_picker_gives_titles_and_metadata_separate_rows() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(100, 24);
    let title =
        "please perform a thorough audit of the resume picker layout without truncating the title";
    let mut unreadable = picker_session("2026-08-27-session-00ff", "(unreadable session)", 0, 2);
    unreadable.modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(9);
    let mut readable = picker_session("readable", title, 12, 1);
    readable.modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
    shell.open_panel(Panel::SessionPicker {
        picker: PickerState::new(vec![readable, unreadable], None),
    });

    let lines = render_panel(&shell.state.borrow(), 100)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let title_row = lines
        .iter()
        .position(|line| line.contains(title))
        .expect("the wide title should not be truncated");
    assert!(lines[title_row + 1].contains("12 msgs"));
    assert!(lines
        .iter()
        .any(|line| { line.contains("(unreadable session ·") && line.contains("00ff)") }));
    assert!(lines.iter().all(|line| visible_width(line) <= 100));
}

fn plain_composer_surface(shell: &InteractiveShell, width: u16, now: Instant) -> Vec<String> {
    crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), width, now)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect()
}

fn plain_footer(shell: &InteractiveShell, width: u16, now: Instant) -> String {
    plain_composer_surface(shell, width, now)
        .pop()
        .expect("composer always has a status row at useful widths")
}

fn default_composer_rule(width: u16) -> String {
    let theme = crate::tui::theme::test_theme();
    theme.glyph("horizontal").repeat(usize::from(width))
}

#[test]
fn confirmation_panel_shows_shared_detail_and_unfiltered_actions() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(100, 24);
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Approve one exact `bash` tool effect?"),
        items: vec!["Deny".into(), "Approve".into()],
        descriptions: vec![
            Some("effect: host_process sha256: 85bc9fe8cfaf7c550880d65882f7c4142c8374875c976c58d1dd724a7f16e609".into()),
            Some("effect: host_process sha256: 85bc9fe8cfaf7c550880d65882f7c4142c8374875c976c58d1dd724a7f16e609".into()),
        ],
        selected: 0,
        filter: String::new(),
        action: PanelAction::Confirmation,
    });

    let lines = render_panel(&shell.state.borrow(), 100)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let rendered = lines.join("\n");
    assert_eq!(lines.len(), 5);
    assert!(rendered.contains("Approve one exact `bash` tool effect?"));
    assert!(rendered.contains("Deny"));
    assert!(rendered.contains("Approve"));
    assert!(rendered.contains("Detail"));
    assert_eq!(rendered.matches("85bc9fe8").count(), 1);
    assert!(!rendered.contains("Filter"));
    assert!(!rendered.contains("1/2"));

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('x')));
    assert!(panel_state(&shell).2.is_empty());
}

#[test]
fn confirmation_requires_its_selected_action_to_be_visible() {
    for (selected, label) in [(0, "Deny"), (1, "Approve")] {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 5);
        shell.open_panel(Panel::SelectList {
            surface: OrdinarySurfaceMetadata::new("Approve?"),
            items: vec!["Deny".into(), "Approve".into()],
            descriptions: vec![Some("writes src/lib.rs".into()); 2],
            selected,
            filter: String::new(),
            action: PanelAction::Confirmation,
        });

        let hidden = shell_chrome(&shell.state.borrow(), 80, Instant::now()).panel;
        assert_eq!(hidden.len(), 1, "{hidden:?}");
        assert_eq!(strip_terminal_sequences(&hidden[0]).trim(), "Approve?");
        assert!(
            shell
                .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
                .is_none(),
            "Enter must not choose an action that is not rendered"
        );
        assert!(shell.has_panel());

        shell.set_size(80, 6);
        let visible = shell_chrome(&shell.state.borrow(), 80, Instant::now()).panel;
        assert_eq!(visible.len(), 2, "{visible:?}");
        assert!(visible.iter().any(|line| line.contains(label)));
        let (result, action) = shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
            .expect("visible selected action can be chosen");
        assert_eq!(result, PanelResult::Confirm(selected));
        assert!(matches!(action, PanelAction::Confirmation));
    }
}

#[test]
fn confirmation_allows_a_visible_action_when_the_title_is_clipped() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(46, 8);
    let title = "Approve one exact workspace mutation with a deliberately long identity?";
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new(title),
        items: vec!["Deny".into(), "Approve".into()],
        descriptions: vec![None, None],
        selected: 1,
        filter: String::new(),
        action: PanelAction::Confirmation,
    });

    let visible = shell_chrome(&shell.state.borrow(), 46, Instant::now()).panel;
    assert!(
        visible.iter().any(|line| line.contains("Approve")),
        "{visible:?}"
    );
    assert!(
        visible
            .iter()
            .all(|line| !strip_terminal_sequences(line).contains(title)),
        "the test requires a clipped title: {visible:?}"
    );
    let (result, action) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("a visible action remains actionable when the title is clipped");
    assert_eq!(result, PanelResult::Confirm(1));
    assert!(matches!(action, PanelAction::Confirmation));
}

#[test]
fn select_list_filter_narrows_items_and_confirm_returns_original_index() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

    for c in "amm".chars() {
        assert!(
            shell
                .panel_input(&panel_key(crossterm::event::KeyCode::Char(c)))
                .is_none(),
            "typing must keep the panel open"
        );
    }

    let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
    assert!(rendered.contains("gamma"), "matching item must render");
    assert!(
        !rendered.contains("alpha"),
        "filtered-out item must not render"
    );
    assert!(
        !rendered.contains("beta"),
        "filtered-out item must not render"
    );

    let (result, _) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("enter should confirm the sole match");
    // "gamma" is index 2 in the original list.
    assert_eq!(result, PanelResult::Confirm(2));
    assert!(!shell.has_panel());
}

#[test]
fn live_subagent_refresh_preserves_selection_by_stable_node_id() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Subagents"),
        items: vec!["alpha".into(), "beta".into()],
        descriptions: vec![Some("running".into()), Some("done".into())],
        selected: 1,
        filter: String::new(),
        action: PanelAction::SelectSubagent(vec!["node-a".into(), "node-b".into()]),
    });

    shell.refresh_subagent_panel(
        "Subagents · refreshed".into(),
        vec!["beta".into(), "gamma".into()],
        vec![Some("done".into()), Some("running".into())],
        vec!["node-b".into(), "node-c".into()],
    );

    let (result, action) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("enter should confirm the stable refreshed selection");
    assert_eq!(result, PanelResult::Confirm(0));
    assert!(matches!(
        action,
        PanelAction::SelectSubagent(ids) if ids == ["node-b", "node-c"]
    ));
}

#[test]
fn select_list_filter_is_case_insensitive_and_matches_descriptions() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        items: vec!["gpt-4o".into(), "claude-sonnet".into()],
        descriptions: vec![
            Some("openai · 128k context".into()),
            Some("anthropic · 200k context".into()),
        ],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(vec![]),
    });

    // Multi-term uppercase query must match across label + description.
    for c in "CLAUDE ANTHROPIC".chars() {
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(c)));
    }
    let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
    assert!(rendered.contains("claude-sonnet"));
    assert!(!rendered.contains("gpt-4o"));

    let (result, _) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("enter should confirm the description match");
    assert_eq!(result, PanelResult::Confirm(1));
}

#[test]
fn select_list_filter_resets_cursor_and_bounds_navigation() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    open_select_panel(&mut shell, &["apple", "banana", "cherry"]);

    // Move to the last row, then filter: the cursor must restart at the
    // first match.
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('a')));
    let (_, selected, filter) = panel_state(&shell);
    assert_eq!(filter, "a");
    assert_eq!(
        selected, 0,
        "typing must reset the cursor to the first match"
    );

    // 'a' matches "apple" and "banana" only; one Down moves to the second
    // match, and a further Down is out of bounds.
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Down));
    let (_, selected, _) = panel_state(&shell);
    assert_eq!(selected, 1, "navigation must stop at the last match");

    let (result, _) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
        .expect("enter should confirm the second match");
    assert_eq!(result, PanelResult::Confirm(1));
}

#[test]
fn select_list_accepts_held_key_repeats_but_ignores_release() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

    shell.panel_input(&panel_key_kind(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyEventKind::Repeat,
    ));
    assert_eq!(panel_state(&shell).1, 1);
    shell.panel_input(&panel_key_kind(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyEventKind::Release,
    ));
    assert_eq!(panel_state(&shell).1, 1);

    assert!(
        shell
            .panel_input(&panel_key_kind(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyEventKind::Repeat,
            ))
            .is_none(),
        "a held Enter key must not confirm a panel twice"
    );
    assert!(shell.has_panel());
}

#[test]
fn select_list_filter_without_matches_keeps_panel_open_on_enter() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    open_select_panel(&mut shell, &["apple", "banana", "cherry"]);

    for c in "zzz".chars() {
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(c)));
    }
    let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
    assert!(
        rendered.contains("no matches") && rendered.contains("zzz"),
        "{rendered}"
    );

    // Enter is a no-op while nothing matches; Esc still cancels.
    assert!(
        shell
            .panel_input(&panel_key(crossterm::event::KeyCode::Enter))
            .is_none(),
        "enter must not confirm when no item matches"
    );
    assert!(shell.has_panel());

    // Deleting the filter restores the full list.
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Backspace));
    let rendered = render_shell(&shell.state.borrow(), 80).join("\n");
    assert!(rendered.contains("apple"));
    assert!(rendered.contains("cherry"));

    let (result, _) = shell
        .panel_input(&panel_key(crossterm::event::KeyCode::Esc))
        .expect("esc should cancel the panel");
    assert_eq!(result, PanelResult::Cancel);
}

#[test]
fn select_list_has_a_stable_filter_row_and_owns_the_only_cursor() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

    let empty_panel_rows = render_panel(&shell.state.borrow(), 80).len();
    let empty = render_shell(&shell.state.borrow(), 80).join("\n");
    let empty_plain = strip_terminal_sequences(&empty);
    assert!(empty_plain.contains("Filter"));
    assert!(empty_plain.contains("type to filter"));
    assert!(empty_plain.contains("1/3"));
    assert_eq!(empty.matches(CURSOR_MARKER).count(), 1);

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Char('a')));
    let filtered_panel_rows = render_panel(&shell.state.borrow(), 80).len();
    let filtered = render_shell(&shell.state.borrow(), 80).join("\n");
    let filtered_plain = strip_terminal_sequences(&filtered);
    assert_eq!(filtered_panel_rows, empty_panel_rows);
    assert!(filtered_plain.contains("Filter  a"));
    assert!(filtered_plain.contains("1/3"));
    assert_eq!(filtered.matches(CURSOR_MARKER).count(), 1);

    shell.close_panel();
    let composer = render_shell(&shell.state.borrow(), 80).join("\n");
    assert_eq!(composer.matches(CURSOR_MARKER).count(), 1);
}

#[test]
fn composer_keeps_its_cursor_marker_at_extreme_narrow_widths() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    let model_rgb = shell
        .state
        .borrow()
        .theme
        .model_rgb(Some(ModelLab::Anthropic))
        .expect("Anthropic model accent");
    let encoded_model_rgb = format!("38;2;{};{};{}", model_rgb.0, model_rgb.1, model_rgb.2);
    for width in [1, 2] {
        let rendered = crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            width,
            Instant::now(),
        )
        .join("\n");
        assert_eq!(
            rendered.matches(CURSOR_MARKER).count(),
            1,
            "width {width}: {rendered:?}"
        );
        assert!(
            rendered.contains(&encoded_model_rgb),
            "width {width} lost next-model provenance: {rendered:?}"
        );
    }

    open_select_panel(&mut shell, &["alpha"]);
    let rendered = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        2,
        Instant::now(),
    )
    .join("\n");
    assert_eq!(rendered.matches(CURSOR_MARKER).count(), 0);
}

#[test]
fn select_list_long_filter_keeps_its_tail_and_cursor_in_narrow_panes() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(24, 12);
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);
    for character in "abcdefghijklmnopqrstuvwxyz".chars() {
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Char(character)));
    }

    let rendered = render_shell(&shell.state.borrow(), 24).join("\n");
    assert_eq!(rendered.matches(CURSOR_MARKER).count(), 1);
    let cursor_line = rendered
        .lines()
        .find(|line| line.contains(CURSOR_MARKER))
        .expect("the active filter must own the cursor");
    let plain = strip_terminal_sequences(cursor_line).replace(CURSOR_MARKER, "");
    assert!(plain.contains("wxyz"), "{plain:?}");
    assert!(!plain.contains("abcdef"), "{plain:?}");
    assert!(visible_width(&plain) <= 24, "{plain:?}");
}

#[test]
fn select_list_keeps_a_focused_filter_row_in_a_tiny_busy_terminal() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(20, 5);
    shell.error("a wrapped background error that would otherwise consume the picker row".into());
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

    let rendered = render_shell(&shell.state.borrow(), 20);
    assert_eq!(rendered.len(), 5, "{rendered:?}");
    assert_eq!(rendered.join("\n").matches(CURSOR_MARKER).count(), 1);
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Filter") && line.contains(CURSOR_MARKER)),
        "{rendered:?}"
    );
}

#[test]
fn select_list_separates_model_metadata_and_drops_it_before_narrow_labels() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(100, 24);
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        items: vec![
            "GPT-5.6".into(),
            "Claude Opus 4.8".into(),
            "Qwen3.6 35B A3B".into(),
        ],
        descriptions: vec![
            Some("openai · 400k context".into()),
            Some("anthropic · 1M context".into()),
            Some("openrouter · 256k context".into()),
        ],
        selected: 1,
        filter: String::new(),
        action: PanelAction::SelectModel(vec![]),
    });

    let wide = render_panel(&shell.state.borrow(), 100)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    assert!(wide[1].starts_with("Select model"), "{wide:?}");
    assert!(wide[2].starts_with("Filter"), "{wide:?}");
    let description_columns = ["openai", "anthropic", "openrouter"]
        .iter()
        .map(|provider| {
            wide.iter()
                .find_map(|line| line.find(provider).map(|byte| visible_width(&line[..byte])))
                .expect("provider metadata should be visible")
        })
        .collect::<Vec<_>>();
    assert!(
        description_columns
            .windows(2)
            .all(|columns| columns[0] == columns[1]),
        "{wide:?}"
    );
    let gpt_row = wide
        .iter()
        .position(|line| line.contains("GPT-5.6"))
        .expect("model label should render");
    assert!(wide[gpt_row + 1].contains("openai"), "{wide:?}");
    assert!(!wide[gpt_row].contains("openai"));
    let selected = wide
        .iter()
        .find(|line| line.contains("Claude Opus"))
        .expect("selected model should render");
    assert!(selected.trim_start().starts_with('›') || selected.trim_start().starts_with('>'));
    assert!(
        selected.starts_with("› ") || selected.starts_with("> "),
        "{wide:?}"
    );
    assert!(wide[gpt_row + 1].starts_with("  "), "{wide:?}");

    let narrow = render_panel(&shell.state.borrow(), 30)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    assert!(narrow.iter().any(|line| line.contains("Claude Opus")));
    assert!(!narrow.iter().any(|line| line.contains("openrouter")));
    assert!(narrow.iter().all(|line| visible_width(line) <= 30));
}

#[test]
fn select_list_home_end_and_page_navigation_stay_bounded() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(52, 12);
    let items = (0..60)
        .map(|index| format!("Model {index:02}"))
        .collect::<Vec<_>>();
    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Select model"),
        descriptions: vec![Some("provider · context".into()); items.len()],
        items,
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(vec![]),
    });

    shell.panel_input(&panel_key(crossterm::event::KeyCode::End));
    assert_eq!(panel_state(&shell).1, 59);
    let at_end = render_panel(&shell.state.borrow(), 52)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    assert!(at_end.iter().any(|line| line.contains("60/60")));
    assert!(at_end.iter().any(|line| line.contains("Model 59")));
    assert!(at_end.iter().all(|line| visible_width(line) <= 52));

    shell.panel_input(&panel_key(crossterm::event::KeyCode::PageUp));
    assert_eq!(panel_state(&shell).1, 55);
    shell.panel_input(&panel_key(crossterm::event::KeyCode::PageDown));
    assert_eq!(panel_state(&shell).1, 59);
    shell.panel_input(&panel_key(crossterm::event::KeyCode::Home));
    assert_eq!(panel_state(&shell).1, 0);
}

#[test]
fn secret_tool_prompt_temporarily_owns_composer_without_touching_the_editor() {
    let mut shell = InteractiveShell::test_shell();
    for character in "ordinary draft".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.set_tool_input_prompt(Some("Password:".into()));
    let secret_surface = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        Instant::now(),
    )
    .iter()
    .map(|line| strip_terminal_sequences(line))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(secret_surface.contains("Password:"), "{secret_surface}");
    assert!(
        !secret_surface.contains("ordinary draft"),
        "{secret_surface}"
    );
    assert_eq!(shell.pending(), "ordinary draft");

    shell.set_tool_input_prompt(None);
    let restored = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        Instant::now(),
    )
    .iter()
    .map(|line| strip_terminal_sequences(line))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(restored.contains("ordinary draft"), "{restored}");
}

#[test]
fn plain_wrapping_is_nonempty_for_empty_text() {
    assert_eq!(wrap_plain("", 10), vec![String::new()]);
}

#[test]
fn wrapped_truecolor_never_reopens_rgb_components_as_backgrounds() {
    let mut theme = crate::tui::theme::test_theme();
    theme.override_token("accent", "#16846b");
    let styled = theme.fg("accent", "alpha beta gamma");
    assert!(styled.contains(";107m"));

    let wrapped = wrap_text_with_ansi(&styled, 6);
    assert!(wrapped.len() > 1);
    assert!(!wrapped.iter().any(|line| line.contains("\x1b[107m")));
    assert!(!wrapped.iter().any(|line| line.contains("\x1b[38;2m")));
    assert!(wrapped.join("").contains("\x1b[38;2;22;132;107m"));
}

#[test]
fn styled_overlay_wraps_by_visible_width_without_splitting_ansi() {
    let theme = sexy_tui_rs::theme::Theme::load(
        None,
        sexy_tui_rs::theme::capability::CapabilityTier::Baseline,
    );
    let selected = format!(
        "{} — {}",
        theme.bold(&theme.fg("accent", "gpt-audio-1.5")),
        theme.fg("muted", "gpt-audio-1.5")
    );
    // This is 29 visible cells but 82 raw characters. At an 80-column
    // terminal the old raw-character wrapper split off the final reset as
    // a literal `[39m` line.
    assert_eq!(visible_width(&selected), 29);
    let wrapped = wrap_text_with_ansi(&selected, 78);
    assert_eq!(wrapped, vec![selected.clone()]);

    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 20);
    shell.show_styled_overlay_text(selected);
    let rendered = render_shell(&shell.state.borrow(), 80);
    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains("gpt-audio-1.5"))
            .count(),
        1,
        "one styled item must occupy one overlay row at 80 columns"
    );
    assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
    assert!(!rendered.iter().any(|line| line == "[39m"));
}

#[test]
fn markdown_transcript_renders_common_headings_lists_code_and_rules() {
    let theme = crate::tui::theme::test_theme();
    let rendered = markdown_lines(
        "### 🔍 **Read & Search**\n- **`read`** — inspect a file\n\n---",
        &theme,
        80,
    )
    .join("\n");
    for marker in ["###", "**", "`", "---"] {
        assert!(!rendered.contains(marker), "marker {marker:?} leaked");
    }
    assert!(rendered.contains("Read & Search"));
    assert!(rendered.contains("read"));
    assert!(rendered.contains('—'));
    assert!(rendered.contains('─'));
}

#[test]
fn rich_text_renders_gfm_tables_tasks_links_and_fenced_code() {
    let theme = crate::tui::theme::test_theme();
    let rendered = markdown_lines(
            "- [x] migrated\n\n| Name | State |\n| --- | --- |\n| TUI | ready |\n\n[docs](https://example.com)\n\n```rust\nfn main() {}\n```",
            &theme,
            80,
        )
        .join("\n");
    assert!(rendered.contains("[x]"), "{rendered}");
    assert!(rendered.contains("migrated"), "{rendered}");
    assert!(rendered.contains("Name"));
    assert!(rendered.contains("ready"));
    assert!(rendered.contains("https://example.com"));
    assert!(rendered.contains("fn"));
    assert!(!rendered.contains("```"));
}

#[test]
fn slash_popup_event_path_keeps_arrow_navigation_active() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Char('/'));
    let total = commands::slash_suggestions("/").len();
    for expected in 1..total {
        let pending = shell.pending();
        let action = crate::tui::keymap::translate_with_popup(
            Some(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
            )),
            false,
            &pending,
            shell.slash_popup_open(),
        );
        assert_eq!(
            action,
            crate::tui::keymap::InputAction::SlashMenu(SlashMenuAction::Next)
        );
        shell.slash_menu(SlashMenuAction::Next);
        assert_eq!(shell.state.borrow().slash_selection, expected);
    }
}

#[test]
fn refreshing_unchanged_slash_catalog_preserves_selection() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Char('/'));

    let commands: Arc<[(String, String)]> = Arc::from(vec![
        ("extension-one".to_owned(), "first".to_owned()),
        ("extension-two".to_owned(), "second".to_owned()),
    ]);
    shell.set_extension_commands(commands.clone());
    shell.slash_menu(SlashMenuAction::Next);
    assert_eq!(shell.state.borrow().slash_selection, 1);

    // The extension polling path republishes an equivalent Arc on every tick.
    // That refresh must not move the highlighted command back to the first row.
    shell.set_extension_commands(commands);
    assert_eq!(shell.state.borrow().slash_selection, 1);
}

#[test]
fn slash_command_menu_lists_commands_and_tab_completes_a_unique_prefix() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Char('/'));
    let rendered = render_slash_suggestions(&shell.state.borrow(), 120, 100);
    for command in ["/new", "/model", "/login", "/cost"] {
        assert!(rendered.iter().any(|line| line.contains(command)));
    }
    let popup = rendered.join("\n");
    assert!(popup.contains("commands"));
    assert!(!popup.contains("Session"));
    assert!(!popup.contains("opens picker"));
    assert!(popup.contains("/help"));
    for removed in ["/tool", "/docs", "/sessions", "/cycle-model"] {
        assert!(!popup.contains(removed), "{removed} remained in {popup}");
    }
    assert!(popup.contains("› /new"));
    assert_input_suggestions_replace_status_footer(&mut shell, "commands");

    shell.slash_menu(SlashMenuAction::Last);
    let scrolled = render_slash_suggestions(&shell.state.borrow(), 80, 7).join("\n");
    assert!(scrolled.contains("/quit"), "{scrolled}");
    assert!(scrolled.contains('/'), "{scrolled}");

    shell.slash_menu(SlashMenuAction::First);
    shell.slash_menu(SlashMenuAction::Next);
    shell.slash_menu(SlashMenuAction::Select);
    assert_eq!(shell.pending(), "/resume ");
    assert!(!shell.slash_popup_open());
    let restored = shell_chrome(&shell.state.borrow(), 120, Instant::now());
    assert!(restored.suggestions.is_empty());
    assert!(restored
        .composer
        .iter()
        .any(|line| strip_terminal_sequences(line).contains("context 0%/272K")));

    shell.drain_editor();
    shell.apply_edit(EditAction::Char('/'));
    for character in "mod".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.complete_slash_command();
    assert_eq!(shell.pending(), "/model ");
}

#[test]
fn inline_autocomplete_uses_compact_footers_and_the_model_accent() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::Anthropic);
        state.model_lab = Some(ModelLab::Anthropic);
    }
    let model_accent = {
        let state = shell.state.borrow();
        let (red, green, blue) = state
            .theme
            .role_rgb("model_accent")
            .expect("active model accent");
        format!("\x1b[38;2;{red};{green};{blue}m")
    };
    let ui_accent = {
        let state = shell.state.borrow();
        let (red, green, blue) = state.theme.role_rgb("accent").expect("UI accent");
        format!("\x1b[38;2;{red};{green};{blue}m")
    };
    assert_ne!(model_accent, ui_accent);

    shell.apply_edit(EditAction::Char('/'));
    let slash = render_slash_suggestions(&shell.state.borrow(), 120, 6);
    assert_eq!(slash.len(), 6, "{slash:?}");
    let selected = slash.first().expect("selected slash suggestion");
    assert!(
        strip_terminal_sequences(selected).starts_with("› /new"),
        "{slash:?}"
    );
    assert!(selected.contains(&model_accent), "{selected:?}");
    assert!(!selected.contains(&ui_accent), "{selected:?}");
    let unselected = slash
        .iter()
        .find(|line| line.contains("/resume"))
        .expect("unselected slash suggestion");
    assert!(!unselected.contains(&model_accent), "{unselected:?}");
    let footer = slash.last().expect("slash suggestion footer");
    let plain_footer = strip_terminal_sequences(footer);
    assert!(
        plain_footer.contains("commands 1–5/")
            && plain_footer.contains("↑↓ navigate · ↵ select · esc close"),
        "{plain_footer:?}"
    );
    assert!(footer.contains(&model_accent), "{footer:?}");
    assert!(!footer.contains(&ui_accent), "{footer:?}");

    shell.drain_editor();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "see @main".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let paths = shell_chrome(&shell.state.borrow(), 120, Instant::now()).suggestions;
    let selected = paths
        .iter()
        .find(|line| line.contains("src/main.rs"))
        .expect("selected mention suggestion");
    assert_eq!(strip_terminal_sequences(selected).trim(), "› src/main.rs");
    assert!(selected.contains(&model_accent), "{selected:?}");
    assert!(!selected.contains(&ui_accent), "{selected:?}");
    let footer = paths.last().expect("mention suggestion footer");
    assert_eq!(
        strip_terminal_sequences(footer).trim(),
        "project files · tab complete"
    );
    assert!(footer.contains(&model_accent), "{footer:?}");
    assert!(!footer.contains(&ui_accent), "{footer:?}");
}

#[test]
fn slash_palette_shares_the_composer_grid_at_narrow_and_wide_widths() {
    for width in [32_u16, 80, 120] {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(width, 20);
        shell.apply_edit(EditAction::Char('/'));

        let state = shell.state.borrow();
        let plan = crate::tui::layout::PresentationLayout::new(&state.theme, width);
        let slash = render_slash_suggestions(&state, width, 6);
        let selected = slash.first().expect("selected slash command");
        let selected = strip_terminal_sequences(selected);
        let composer =
            crate::tui::composer_surface::render_composer_surface(&state, width, Instant::now());
        let composer_prompt = composer
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .find(|line| line.contains("› /"))
            .expect("composer prompt row");

        assert_eq!(
            plan.inset, 0,
            "default surfaces must reach the terminal edge at width {width}"
        );
        assert_eq!(
            selected.find('›'),
            Some(usize::from(plan.inset)),
            "slash palette width {width}: {selected:?}"
        );
        assert_eq!(
            composer_prompt.find('›'),
            Some(usize::from(plan.inset)),
            "composer width {width}: {composer_prompt:?}"
        );
        let command_byte = selected.find('/').expect("slash command name");
        assert_eq!(
            visible_width(&selected[..command_byte]),
            2,
            "slash command names belong on the shared primary text column: {selected:?}"
        );
        assert!(
            visible_width(&selected) <= usize::from(plan.inset + plan.content_width),
            "slash palette exceeded composer right edge at width {width}: {selected:?}"
        );
    }
}

#[test]
fn composer_always_uses_the_model_selected_for_the_next_prompt() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    let now = Instant::now();
    let idle =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, now);
    shell.apply_edit(EditAction::Char('x'));
    let focused =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, now);
    let run_id = shell.begin_run("anthropic");
    let active =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, now);

    let accent = shell
        .state
        .borrow()
        .theme
        .model_rgb(Some(ModelLab::Anthropic))
        .expect("Anthropic model accent");
    let encoded = format!("38;2;{};{};{}", accent.0, accent.1, accent.2);
    for surface in [&idle, &focused, &active] {
        assert!(surface.join("\n").contains(&encoded), "{surface:?}");
    }
    assert_eq!(idle[0], focused[0]);
    assert_eq!(focused[0], active[0]);
    shell.interrupt_run(run_id);
}

#[test]
fn model_switch_recolors_only_the_composer_and_future_prompt() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    shell.on_prompt_submitted("prompt for Claude");
    shell.set_identity("local", "qwen3.6-27b", "high");
    shell.on_prompt_submitted("prompt for Qwen");

    let before_switch = shell
        .state
        .borrow()
        .transcript
        .iter()
        .filter_map(|block| match block {
            TranscriptBlock::User {
                text, prompt_color, ..
            } => Some((text.clone(), prompt_color.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_ne!(before_switch[0].1, before_switch[1].1);

    shell.set_identity("openai", "gpt-5.6", "high");
    let after_switch = shell
        .state
        .borrow()
        .transcript
        .iter()
        .filter_map(|block| match block {
            TranscriptBlock::User {
                text, prompt_color, ..
            } => Some((text.clone(), prompt_color.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(after_switch, before_switch);

    let rendered = shell.state.borrow().rendered_transcript(80).join("\n");
    for (_, color) in &before_switch {
        let color = color.as_deref().expect("model prompt colour");
        let red = u8::from_str_radix(&color[1..3], 16).unwrap();
        let green = u8::from_str_radix(&color[3..5], 16).unwrap();
        let blue = u8::from_str_radix(&color[5..7], 16).unwrap();
        assert!(
            rendered.contains(&format!("48;2;{red};{green};{blue}")),
            "stored prompt card lost {color}: {rendered:?}"
        );
    }

    let state = shell.state.borrow();
    assert_eq!(state.model_lab, Some(ModelLab::OpenAi));
    let openai = state
        .theme
        .model_rgb(Some(ModelLab::OpenAi))
        .expect("OpenAI model accent");
    let composer =
        crate::tui::composer_surface::render_composer_surface(&state, 80, Instant::now())
            .join("\n");
    assert!(composer.contains(&format!("38;2;{};{};{}", openai.0, openai.1, openai.2)));
}

#[test]
fn discovered_prompt_templates_join_slash_autocomplete() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_prompt_templates(Arc::from(vec![crate::prompts::PromptTemplateDescriptor {
        name: "local-review".into(),
        description: "Focused local review".into(),
        argument_hint: Some("[focus]".into()),
        path: PathBuf::from("/tmp/local-review.md"),
        trust: crate::prompts::PromptTrust::UserInstalled,
        content_hash: "hash".into(),
    }]));
    for character in "/loc".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let rendered = render_slash_suggestions(&shell.state.borrow(), 100, 10).join("\n");
    assert!(rendered.contains("/local-review [focus]"), "{rendered}");
    assert!(
        rendered.contains("prompt · Focused local review"),
        "{rendered}"
    );
    let narrow = render_slash_suggestions(&shell.state.borrow(), 32, 10).join("\n");
    assert!(narrow.contains("/local-review [focus]"), "{narrow}");
    shell.complete_slash_command();
    assert_eq!(shell.pending(), "/local-review ");
}

#[test]
fn dynamic_slash_discovery_contains_only_registered_executable_names() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_prompt_templates(Arc::from(vec![crate::prompts::PromptTemplateDescriptor {
        name: "local-review".into(),
        description: "Focused local review".into(),
        argument_hint: None,
        path: PathBuf::from("/tmp/local-review.md"),
        trust: crate::prompts::PromptTrust::UserInstalled,
        content_hash: "hash".into(),
    }]));
    shell.set_skill_commands(Arc::from(vec![(
        "workspace-review".into(),
        "Review workspace changes".into(),
    )]));
    shell.set_extension_commands(Arc::from(vec![
        ("checkpoint".into(), "Save checkpoint".into()),
        // A dynamic command cannot shadow a working built-in.
        ("status".into(), "Shadow status".into()),
    ]));
    shell.apply_edit(EditAction::Char('/'));

    let state = shell.state.borrow();
    let suggestions = input_slash_suggestions(&state);
    let prompt_names = state
        .prompt_templates
        .iter()
        .map(|template| template.name.as_str())
        .collect::<HashSet<_>>();
    let skill_names = state
        .skill_commands
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<HashSet<_>>();
    let extension_names = state
        .extension_commands
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<HashSet<_>>();
    for suggestion in suggestions.iter().filter(|suggestion| {
        matches!(
            suggestion.provenance,
            super::input_overlays::SlashSuggestionProvenance::Prompt
                | super::input_overlays::SlashSuggestionProvenance::Skill
                | super::input_overlays::SlashSuggestionProvenance::Extension
        )
    }) {
        let registered = match suggestion.provenance {
            super::input_overlays::SlashSuggestionProvenance::Prompt => {
                prompt_names.contains(suggestion.name.as_str())
            }
            super::input_overlays::SlashSuggestionProvenance::Skill => {
                skill_names.contains(suggestion.name.as_str())
            }
            super::input_overlays::SlashSuggestionProvenance::Extension => {
                extension_names.contains(suggestion.name.as_str())
            }
            super::input_overlays::SlashSuggestionProvenance::Builtin => {
                unreachable!("only dynamic slash suggestions should reach this registration check")
            }
        };
        assert!(registered, "unregistered suggestion: {suggestion:?}");
    }
    assert_eq!(
        suggestions
            .iter()
            .filter(|suggestion| suggestion.name == "status")
            .count(),
        1,
        "dynamic command shadowed the built-in route"
    );
    assert!(suggestions.iter().any(|suggestion| {
        suggestion.name == "local-review"
            && suggestion.provenance == super::input_overlays::SlashSuggestionProvenance::Prompt
    }));
    assert!(suggestions.iter().any(|suggestion| {
        suggestion.name == "workspace-review"
            && suggestion.provenance == super::input_overlays::SlashSuggestionProvenance::Skill
    }));
    assert!(suggestions.iter().any(|suggestion| {
        suggestion.name == "checkpoint"
            && suggestion.provenance == super::input_overlays::SlashSuggestionProvenance::Extension
    }));
}

#[test]
fn mention_completion_inserts_path_reference_for_text_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "see @main".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let rendered = render_shell(&shell.state.borrow(), 120);
    assert!(rendered
        .iter()
        .any(|line| { strip_terminal_sequences(line).contains("project files · tab complete") }));
    assert!(rendered.iter().any(|line| line.contains("src/main.rs")));
    assert_input_suggestions_replace_status_footer(&mut shell, "project files");
    shell.complete_path();
    assert_eq!(shell.pending(), "see @src/main.rs ");
}

#[test]
fn literal_path_completion_descends_through_directories() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "inspect ./sr".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    let rendered = render_shell(&shell.state.borrow(), 120);
    assert!(rendered
        .iter()
        .any(|line| strip_terminal_sequences(line).contains("paths · tab complete")));
    assert!(rendered.iter().any(|line| line.contains("./src/")));
    assert_input_suggestions_replace_status_footer(&mut shell, "paths");

    shell.complete_path();
    assert_eq!(shell.pending(), "inspect ./src/");
    shell.complete_path();
    assert_eq!(shell.pending(), "inspect ./src/main.rs ");
}

#[test]
fn literal_path_completion_escapes_spaces_and_stays_active() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("My Folder")).unwrap();
    std::fs::write(dir.path().join("My Folder/draft note.md"), b"text").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "inspect ./My".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    shell.complete_path();
    assert_eq!(shell.pending(), r"inspect ./My\ Folder/");
    shell.complete_path();
    assert_eq!(shell.pending(), r"inspect ./My\ Folder/draft\ note.md ");
}

#[test]
fn mention_path_completion_keeps_directories_active() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "@./sr".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    shell.complete_path();
    assert_eq!(shell.pending(), "@./src/");
    shell.complete_path();
    assert_eq!(shell.pending(), "@./src/lib.rs ");
}

#[test]
fn mention_completion_attaches_media_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), b"png").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
    for character in "@shot".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.complete_path();
    assert_eq!(shell.pending(), "[Image #1]");
    let composed = shell.drain_composed();
    assert!(composed
        .parts
        .iter()
        .any(|part| matches!(part, ygg_agent::InputPart::Media(_))));
}

#[test]
fn set_workspace_keeps_file_index_and_layout_when_the_root_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "@a".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let generation = {
        let state = shell.state.borrow();
        drop(state.rendered_transcript(80));
        let cache = state.transcript_cache.borrow();
        assert_eq!(cache.width, Some(80));
        assert!(!cache.dirty);
        cache.generation
    };
    assert!(shell.state.borrow().file_index.is_some());

    // Re-asserting the same root (update_status runs after every turn) must
    // preserve both the lazily built mention index and historic layout.
    shell.set_workspace(dir.path().to_path_buf());
    let state = shell.state.borrow();
    assert!(state.file_index.is_some());
    let cache = state.transcript_cache.borrow();
    assert_eq!(cache.width, Some(80));
    assert!(!cache.dirty);
    assert_eq!(cache.generation, generation);
    drop(cache);
    drop(state);

    // A genuinely different root invalidates both workspace-derived caches.
    let other = tempfile::tempdir().unwrap();
    shell.set_workspace(other.path().to_path_buf());
    let state = shell.state.borrow();
    assert!(state.file_index.is_none());
    let cache = state.transcript_cache.borrow();
    assert_eq!(cache.width, None);
    assert!(cache.dirty);
}

#[test]
fn invalidate_file_index_forces_a_fresh_walk_for_new_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "@a".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    assert!(shell.state.borrow().file_index.is_some());

    // A run may have created files; invalidation makes the next mention
    // pick them up.
    std::fs::write(dir.path().join("brand_new.rs"), b"x").unwrap();
    shell.invalidate_file_index();
    assert!(shell.state.borrow().file_index.is_none());
    shell.apply_edit(EditAction::Char('_'));
    let state = shell.state.borrow();
    let files = state.file_index.as_ref().unwrap();
    assert!(files.iter().any(|file| file == "brand_new.rs"));
}

#[test]
fn unsupported_media_mention_falls_back_to_a_path_and_notice() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.png"), b"png").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "@shot".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.complete_path();

    assert_eq!(shell.pending(), "@shot.png ");
    assert!(shell
        .debug_snapshot()
        .contains("does not accept image input"));
}

#[test]
fn context_uses_single_turn_provider_total_not_cumulative_run_usage() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5", "high");
    shell.set_context_estimate(80, 272_000);
    shell.begin_run("openai");
    let turn_usage = Usage {
        input_tokens: 10_000,
        cache_read_tokens: 200_000,
        output_tokens: 10_000,
        total_tokens: 220_000,
        ..Usage::default()
    };
    shell.on_agent_event(&AgentEvent::TurnFinished {
        message: ygg_ai::AssistantMessage {
            content: vec![ygg_ai::AssistantPart::Text("done".into())],
            model: ModelId("gpt-5".into()),
            protocol: ygg_ai::Protocol::OpenAiResponses,
        },
        stop_reason: ygg_ai::StopReason::EndTurn,
        turn_usage,
        usage: Usage {
            input_tokens: 20_000,
            cache_read_tokens: 370_000,
            output_tokens: 20_000,
            total_tokens: 410_000,
            ..Usage::default()
        },
        session_cost_microdollars: None,
        run_cost_microdollars: 0,
    });

    let state = shell.state.borrow();
    assert_eq!(state.last_turn_usage, Some(turn_usage));
    assert_eq!(state.run_context_estimate, Some((220_000, 272_000)));
    assert_eq!(state.context_estimate, Some((220_000, 272_000)));
}

#[test]
fn submitted_prompts_render_immediately_with_real_context_budget() {
    let mut shell = InteractiveShell::test_shell();
    shell.on_prompt_submitted("second prompt");
    shell.set_identity("deepseek", "deepseek-v4-pro", "high");
    shell.set_context_estimate(900_000, 967_232);
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("second prompt"));
    let rendered = render_shell(&shell.state.borrow(), 120);
    let footer = rendered.last().expect("single composer footer");
    assert!(
        strip_terminal_sequences(footer).contains("context 93%/967K"),
        "footer was {footer:?}"
    );
}

#[test]
fn running_local_shell_repaints_the_latest_output_tail_before_exit() {
    let mut shell = InteractiveShell::test_shell();
    let id = shell.append_shell_in_progress("long command".into());
    shell.update_shell_output(
        &id,
        (1..=8)
            .map(|line| format!("LIVE OUTPUT {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let rows = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    let output = rows
        .iter()
        .find(|line| line.contains("LIVE OUTPUT 5"))
        .expect("first retained local-shell output");
    let nested = rows
        .iter()
        .find(|line| line.contains("4 earlier visual rows hidden"))
        .expect("local-shell output metadata");
    let elbow_byte = nested.find('└').expect("local-shell output elbow");
    let text_byte = output
        .find("LIVE OUTPUT 5")
        .expect("local-shell output text");
    assert_eq!(visible_width(&nested[..elbow_byte]), 2, "{rows:?}");
    assert_eq!(visible_width(&output[..text_byte]), 4, "{rows:?}");
    let rendered = rows.join("\n");
    assert!(!rendered.contains("LIVE OUTPUT 1"), "{rendered}");
    assert!(!rendered.contains("LIVE OUTPUT 4"), "{rendered}");
    assert!(rendered.contains("LIVE OUTPUT 5"), "{rendered}");
    assert!(
        rendered.contains("4 earlier visual rows hidden"),
        "{rendered}"
    );
    assert!(rendered.contains("LIVE OUTPUT 8"), "{rendered}");
}

#[test]
fn local_shell_commands_do_not_claim_a_model_prompt_color() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5.6", "high");
    shell.on_local_command_submitted("!git status");
    let state = shell.state.borrow();
    let TranscriptBlock::User { prompt_color, .. } = &state.transcript[0] else {
        panic!("local command transcript row expected");
    };
    assert_eq!(prompt_color, &None);
    let rendered = render_block(
        None,
        &state.transcript[0],
        &state.theme,
        &state.theme.rich_renderer(),
        &state.theme.reasoning_renderer(),
        80,
        false,
    )
    .join("\n");
    assert!(!rendered.contains("\x1b[48;"), "{rendered:?}");
}

#[test]
fn steering_messages_are_queued_above_prompt_and_delivered_as_a_batch() {
    let mut shell = InteractiveShell::test_shell();
    shell.queue_steering(&ComposedInput::from_text("check the docs".into()));
    shell.queue_steering(&ComposedInput::from_text("then run the tests".into()));

    let rendered = render_shell(&shell.state.borrow(), 120);
    let prompt = rendered
        .iter()
        .position(|line| line.contains(CURSOR_MARKER))
        .expect("prompt line");
    let plain = rendered
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    let queue = plain
        .iter()
        .position(|line| line.contains("Steering · 2 queued"))
        .expect("steering queue");
    assert!(queue < prompt);
    assert!(plain
        .iter()
        .any(|line| line.starts_with("  └ check the docs") && line.contains("+1 more")));
    assert!(!plain.iter().any(|line| line.contains("then run the tests")));

    shell.on_agent_event(&AgentEvent::SteeringDelivered {
        messages: vec!["check the docs".into(), "then run the tests".into()],
    });
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("check the docs"));
    assert!(snapshot.contains("then run the tests"));
    assert!(!render_shell(&shell.state.borrow(), 120)
        .iter()
        .any(|line| line.contains("Steering ·")));
}

#[test]
fn queued_steering_uses_active_model_color() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    shell.queue_steering(&ComposedInput::from_text("check the docs".into()));

    let state = shell.state.borrow();
    let model_color = state
        .theme
        .model_rgb(state.model_lab)
        .expect("active model color");
    let ui_color = state.theme.role_rgb("accent").expect("UI accent");
    assert_ne!(model_color, ui_color);
    let model_sequence = format!(
        "38;2;{};{};{}m",
        model_color.0, model_color.1, model_color.2
    );

    let rendered = input_overlays::render_pending_steering(&state, 80, 2);
    assert!(rendered[0].contains(&model_sequence), "{rendered:?}");
    assert!(rendered[1].contains(&model_sequence), "{rendered:?}");
}

#[test]
fn steering_preview_stays_bounded_to_one_clipped_content_row() {
    let mut shell = InteractiveShell::test_shell();
    shell.queue_steering(&ComposedInput::from_text(
        "i'm sending you a longer steering prompt just because i want to see how ygg's tui handles showing this in the queued prompts area".into(),
    ));

    let rendered = input_overlays::render_pending_steering(&shell.state.borrow(), 71, 8)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();

    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(rendered[1].starts_with("  └ i'm sending"), "{rendered:?}");
    assert!(rendered[1].ends_with('…'), "{rendered:?}");
    assert!(rendered.iter().all(|line| visible_width(line) <= 71));
}

#[test]
fn steering_messages_preserve_explicit_newlines() {
    let mut shell = InteractiveShell::test_shell();
    shell.queue_steering(&ComposedInput::from_text(
        "first line\nsecond 👩‍💻 line".into(),
    ));

    let rendered = input_overlays::render_pending_steering(&shell.state.borrow(), 40, 8)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();

    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(
        rendered[1].contains("first line ↵ second 👩‍💻 line"),
        "{rendered:?}"
    );
    assert!(rendered.iter().all(|line| visible_width(line) <= 40));
}

#[test]
fn steering_overflow_previews_first_prompt_and_counts_the_rest() {
    let mut shell = InteractiveShell::test_shell();
    shell.queue_steering(&ComposedInput::from_text(
        "first prompt has enough words to require several wrapped display rows".into(),
    ));
    shell.queue_steering(&ComposedInput::from_text(
        "second prompt also has enough words to require several wrapped rows".into(),
    ));

    let rendered = input_overlays::render_pending_steering(&shell.state.borrow(), 30, 5)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let joined = rendered.join("\n");

    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(joined.contains("└ first prompt"), "{rendered:?}");
    assert!(!joined.contains("second prompt"), "{rendered:?}");
    assert!(joined.contains("+1 more"), "{rendered:?}");
    assert!(rendered.iter().all(|line| visible_width(line) <= 30));
}

#[test]
fn steering_overflow_reports_entirely_hidden_prompts() {
    let mut shell = InteractiveShell::test_shell();
    for index in 1..=5 {
        shell.queue_steering(&ComposedInput::from_text(format!("prompt {index}")));
    }

    let rendered = input_overlays::render_pending_steering(&shell.state.borrow(), 40, 4)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let joined = rendered.join("\n");

    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(joined.contains("└ prompt 1"), "{rendered:?}");
    assert!(!joined.contains("prompt 2"), "{rendered:?}");
    assert!(joined.contains("+4 more"), "{rendered:?}");
}

#[test]
fn terminal_native_prompt_stays_within_every_viewport() {
    for (width, height) in [
        (1, 5),
        (2, 5),
        (3, 5),
        (4, 5),
        (8, 5),
        (12, 7),
        (24, 10),
        (40, 12),
        (60, 18),
        (80, 24),
        (120, 30),
        (160, 40),
    ] {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(width, height);
        for character in "a long prompt that must wrap cleanly at every width".chars() {
            shell.apply_edit(EditAction::Char(character));
        }

        let rendered = render_shell(&shell.state.borrow(), width);
        assert!(rendered.len() <= usize::from(height));
        assert!(
            rendered
                .iter()
                .all(|line| visible_width(line) <= usize::from(width)),
            "{width}x{height}: {rendered:?}"
        );
        assert!(!rendered.iter().any(|line| {
            line.chars()
                .any(|character| matches!(character, '┏' | '┓' | '┗' | '┛'))
        }));
        if width >= 4 {
            assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
        }
    }
}

#[test]
fn vertical_editor_navigation_snaps_to_document_boundaries_in_one_step() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(40, 12);
    for character in "first\nsecond\nthird".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    shell.state.borrow_mut().editor_cursor = 3;
    shell.apply_edit(EditAction::Up);
    assert_eq!(shell.state.borrow().editor_cursor, 0);

    let editor_len = shell.state.borrow().editor.len();
    shell.state.borrow_mut().editor_cursor = editor_len - 2;
    shell.apply_edit(EditAction::Down);
    assert_eq!(shell.state.borrow().editor_cursor, editor_len);
}

#[test]
fn vertical_editor_navigation_snaps_at_soft_wrapped_boundaries() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(8, 12);
    for character in "abcdefghijklm".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    shell.state.borrow_mut().editor_cursor = 3;
    shell.apply_edit(EditAction::Up);
    assert_eq!(shell.state.borrow().editor_cursor, 0);

    let editor_len = shell.state.borrow().editor.len();
    shell.state.borrow_mut().editor_cursor = editor_len - 2;
    let (editor, cursor) = {
        let state = shell.state.borrow();
        (state.editor.clone(), state.editor_cursor)
    };
    assert_eq!(
        editor_layout(&editor, cursor, 8).cursor_row,
        2,
        "fixture cursor must begin on the bottom soft-wrapped row"
    );
    shell.apply_edit(EditAction::Down);
    assert_eq!(shell.state.borrow().editor_cursor, editor_len);
}

#[test]
fn clear_editor_discards_attachments_and_resets_composer_navigation() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Paste("discarded\n".repeat(20)));
    assert!(!shell.state.borrow().ledger.is_empty());
    {
        let mut state = shell.state.borrow_mut();
        state.editor_cursor = 3;
        state.slash_selection = 4;
        state.slash_scroll = 2;
        state.slash_popup_dismissed = true;
    }

    shell.clear_editor();

    {
        let state = shell.state.borrow();
        assert!(state.editor.is_empty());
        assert_eq!(state.editor_cursor, 0);
        assert!(state.ledger.is_empty());
        assert_eq!(state.slash_selection, 0);
        assert_eq!(state.slash_scroll, 0);
        assert!(!state.slash_popup_dismissed);
    }

    shell.apply_edit(EditAction::Paste("kept\n".repeat(20)));
    assert!(
        shell.pending().starts_with("[Pasted text #2:"),
        "clearing a draft must not reuse an attachment ID"
    );
    let composed = shell.drain_composed();
    assert!(matches!(
        composed.parts.as_slice(),
        [ygg_agent::InputPart::Text(text)]
            if text.contains("kept") && !text.contains("discarded")
    ));
}

#[test]
fn bracketed_paste_preserves_multiline_editor_text_without_submitting() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Char('a'));
    shell.apply_edit(EditAction::Paste("b\r\nc\rd".into()));
    assert_eq!(shell.pending(), "ab\nc\nd");
    assert_eq!(shell.state.borrow().editor_cursor, "ab\nc\nd".len());
    let rendered = render_shell(&shell.state.borrow(), 120);
    assert!(rendered.iter().any(|line| line.contains("ab")));
    assert!(rendered.iter().any(|line| line.contains("c")));
}

#[test]
fn media_path_paste_attaches_a_chip_and_composes_media_parts() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    std::fs::write(&image, b"png").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_input_modalities(
        ygg_ai::ModalitySet::none()
            .with(ygg_ai::Modality::Image)
            .with(ygg_ai::Modality::Audio),
    );
    for character in "see ".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.apply_edit(EditAction::Paste(image.display().to_string()));

    let composed = shell.drain_composed();
    assert_eq!(composed.display_text, "see [Image #1]");
    assert!(composed
        .parts
        .iter()
        .any(|part| matches!(part, ygg_agent::InputPart::Media(_))));
}

#[test]
fn raw_key_drop_with_surrounding_prompt_still_attaches_media() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("screen shot.png");
    std::fs::write(&image, b"png").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_input_modalities(ygg_ai::ModalitySet::none().with(ygg_ai::Modality::Image));
    let escaped = image.display().to_string().replace(' ', "\\ ");
    for character in format!("{escaped} diagnose this UI").chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    let composed = shell.drain_composed();
    assert!(composed
        .display_text
        .contains("[Image #1] diagnose this UI"));
    assert!(composed
        .parts
        .iter()
        .any(|part| matches!(part, ygg_agent::InputPart::Media(ygg_ai::Media::Image(_)))));
    assert!(composed.parts.iter().any(
        |part| matches!(part, ygg_agent::InputPart::Text(text) if text.contains("diagnose this UI"))
    ));
}

#[test]
fn media_paste_without_capability_inserts_plain_path_and_notice() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    std::fs::write(&image, b"png").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_input_modalities(ygg_ai::ModalitySet::none());
    shell.apply_edit(EditAction::Paste(image.display().to_string()));

    let composed = shell.drain_composed();
    assert_eq!(composed.display_text, image.display().to_string());
    assert!(composed
        .parts
        .iter()
        .all(|part| matches!(part, ygg_agent::InputPart::Text(_))));
    assert!(shell
        .debug_snapshot()
        .contains("does not accept image input"));
}

#[test]
fn large_paste_collapses_to_chip_and_splices_back_on_drain() {
    let mut shell = InteractiveShell::test_shell();
    let large = "line\n".repeat(20);
    shell.apply_edit(EditAction::Paste(large.clone()));

    let state_text = shell.pending();
    assert!(state_text.starts_with("[Pasted text #1: 20 lines]"));

    let composed = shell.drain_composed();
    assert!(matches!(
        composed.parts.as_slice(),
        [ygg_agent::InputPart::Text(text)] if text.matches("line").count() == 20
    ));
}

#[test]
fn small_paste_still_inserts_verbatim() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Paste("first\nsecond".into()));
    assert_eq!(shell.pending(), "first\nsecond");
}

#[test]
fn steering_restore_returns_chips_and_attachments() {
    let mut shell = InteractiveShell::test_shell();
    let large = "line\n".repeat(20);
    shell.apply_edit(EditAction::Paste(large));
    let composed = shell.drain_composed();
    shell.queue_steering(&composed);

    shell.restore_queued_steering();
    assert!(shell.pending().contains("[Pasted text #1: 20 lines]"));
    // The ledger got its entry back: draining resolves the chip again.
    let recomposed = shell.drain_composed();
    assert!(matches!(
        recomposed.parts.as_slice(),
        [ygg_agent::InputPart::Text(text)] if text.matches("line").count() == 20
    ));
}

#[test]
fn aborted_final_frame_shows_interruption_and_restored_steering() {
    use ygg_agent::{EntryId, FinishReason};

    const WIDTH: u16 = 72;
    const HEIGHT: u16 = 18;
    for synchronized_output in [false, true] {
        let (mut shell, bytes) = emulated_shell_with_sync(
            crate::tui::theme::test_theme(),
            WIDTH,
            HEIGHT,
            synchronized_output,
        );
        let run_id = shell.begin_run("temper");
        shell.queue_steering(&ComposedInput::from_text("inspect renderer".into()));
        shell.queue_steering(&ComposedInput::from_text("then run tests".into()));

        // This is the production ordering at the terminal run boundary:
        // settle the outcome, restore any undelivered queue, then publish
        // one complete frame.
        shell.on_run_event(
            run_id,
            &AgentEvent::RunFinished {
                head: EntryId("aborted-head".into()),
                reason: FinishReason::Aborted,
            },
        );
        shell.restore_queued_steering();
        shell.render();

        let output = bytes.lock().unwrap().clone();
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
        terminal.process(&output);
        let physical = terminal.screen().contents();
        assert_eq!(physical.matches("interrupted").count(), 1, "{physical}");
        assert!(physical.contains("inspect renderer"), "{physical}");
        assert!(physical.contains("then run tests"), "{physical}");
        assert!(!physical.contains("Steering prompt"), "{physical}");
    }
}

#[test]
fn steering_delivery_is_positional_fifo() {
    let mut shell = InteractiveShell::test_shell();
    shell.apply_edit(EditAction::Paste("go left".into()));
    let first = shell.drain_composed();
    shell.queue_steering(&first);
    shell.apply_edit(EditAction::Paste("go right".into()));
    let second = shell.drain_composed();
    shell.queue_steering(&second);

    shell.on_agent_event(&AgentEvent::SteeringDelivered {
        messages: vec!["go left".into()],
    });
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("go left"));
    // Second message still pending.
    assert!(render_shell(&shell.state.borrow(), 120)
        .iter()
        .any(|line| line.contains("go right")));
}

#[test]
fn prompt_bar_cursor_tracks_insertions_and_cursor_motion() {
    let mut shell = InteractiveShell::test_shell();
    for character in "abcdef".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.apply_edit(EditAction::Left);
    shell.apply_edit(EditAction::Left);
    shell.apply_edit(EditAction::Char('X'));
    assert_eq!(shell.state.borrow().editor, "abcdXef");

    let rendered = render_shell(&shell.state.borrow(), 120);
    let line = rendered
        .iter()
        .find(|line| line.contains(CURSOR_MARKER))
        .unwrap();
    assert!(line.find("abcdX").unwrap() < line.find(CURSOR_MARKER).unwrap());
    assert!(line.find(CURSOR_MARKER).unwrap() < line.find("ef").unwrap());
}

#[test]
fn scrolling_reuses_the_cached_transcript_layout() {
    let mut shell = InteractiveShell::test_shell();
    for number in 0..200 {
        shell.notice(format!("notice {number}"));
    }
    let _ = render_shell(&shell.state.borrow(), 120);
    let first_generation = shell.state.borrow().transcript_cache.borrow().generation;

    shell.scroll_lines(-3);
    let _ = render_shell(&shell.state.borrow(), 120);
    assert_eq!(
        shell.state.borrow().transcript_cache.borrow().generation,
        first_generation,
        "scrolling must only slice the existing layout"
    );

    shell.notice("new transcript block");
    let _ = render_shell(&shell.state.borrow(), 120);
    assert_eq!(
        shell.state.borrow().transcript_cache.borrow().generation,
        first_generation + 1
    );
}

#[test]
fn transcript_cache_reflows_when_width_changes_without_content_changes() {
    let shell = InteractiveShell::test_shell();
    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized(
                "Width-sensitive transcript caching must rebuild this line when the terminal shrinks while preserving TAIL-MARKER.".into(),
            ),
        )));

    let wide = shell.state.borrow().rendered_transcript(120).clone();
    let first_generation = shell.state.borrow().transcript_cache.borrow().generation;
    let narrow = shell.state.borrow().rendered_transcript(36).clone();
    let state = shell.state.borrow();
    let cache = state.transcript_cache.borrow();

    assert_eq!(cache.width, Some(36));
    assert_eq!(cache.generation, first_generation + 1);
    assert!(narrow.len() > wide.len(), "wide={wide:?} narrow={narrow:?}");
    assert!(narrow.iter().all(|line| visible_width(line) <= 36));
    assert!(strip_terminal_sequences(&narrow.join("\n")).contains("TAIL-MARKER"));
}

#[test]
fn new_output_does_not_move_a_scrolled_reader_viewport() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 18);
    for number in 0..100 {
        shell.notice(format!("anchor notice {number}"));
    }
    let _ = render_shell(&shell.state.borrow(), 80);
    shell.scroll_lines(-6);
    let before = render_shell(&shell.state.borrow(), 80)
        .into_iter()
        .filter(|line| line.contains("anchor notice"))
        .collect::<Vec<_>>();

    shell.notice("new output while reading");
    let after = render_shell(&shell.state.borrow(), 80)
        .into_iter()
        .filter(|line| line.contains("anchor notice"))
        .collect::<Vec<_>>();
    assert_eq!(after, before);
}

#[test]
fn terminal_native_resume_materializes_complete_history_for_pi_scrollback() {
    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("native-complete-session.jsonl"),
        "native prompt",
        100,
    );
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("native prompt 0\n"));
    assert!(snapshot.contains("native prompt 99"));
    assert!(shell.state.borrow().deferred_session_history.is_none());
}

#[test]
fn application_viewport_resume_is_tail_first_and_materializes_when_scrolling_past_it() {
    use ygg_agent::EntryValue;
    use ygg_ai::{Message, UserMessage, UserPart};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    let mut session = Session::create(&path).unwrap();
    for index in 0..100 {
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(format!("prompt {index}"))],
            })))
            .unwrap();
    }

    let mut shell = lazy_history_test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    assert!(shell.debug_snapshot().contains("prompt 99"));
    assert!(!shell.debug_snapshot().contains("prompt 0\n"));
    assert!(shell.state.borrow().deferred_session_history.is_some());
    shell.on_local_command_submitted("!local-only command");
    let retained_tail_cursor = {
        let state = shell.state.borrow();
        transcript_commit_cursor(
            &state,
            state.transcript.len().saturating_sub(1),
            FINAL_COMMIT_SEGMENT,
        )
    };

    let page = usize::from(shell.state.borrow().size.1.max(4) / 2);
    let mut crossing_scroll = None;
    for _ in 0..100 {
        let before = shell.state.borrow().scroll_from_bottom.get();
        shell.scroll(-1);
        if shell.state.borrow().deferred_session_history.is_none() {
            crossing_scroll = Some((before, shell.state.borrow().scroll_from_bottom.get()));
            break;
        }
    }
    assert!(shell.state.borrow().deferred_session_history.is_none());
    let (before, after) = crossing_scroll.expect("deferred history crossing");
    assert!(
        after <= before.saturating_add(page),
        "prepending history must advance one page, not jump to oldest: {before} -> {after}"
    );
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("prompt 0\n"));
    assert_eq!(snapshot.matches("!local-only command").count(), 1);
    let remapped_tail_cursor = {
        let state = shell.state.borrow();
        transcript_commit_cursor(
            &state,
            state.transcript.len().saturating_sub(1),
            FINAL_COMMIT_SEGMENT,
        )
    };
    assert_eq!(
        remapped_tail_cursor, retained_tail_cursor,
        "prepending deferred history must preserve retained block identity"
    );
}

#[test]
fn deferred_history_keeps_local_outcome_before_a_later_persisted_prompt() {
    use ygg_agent::EntryValue;
    use ygg_ai::{Message, UserMessage, UserPart};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("interleaved-session.jsonl");
    let mut session = Session::create(&path).unwrap();
    for index in 0..100 {
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text(format!("persisted prompt {index}"))],
            })))
            .unwrap();
    }

    let mut shell = lazy_history_test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    assert!(shell.state.borrow().deferred_session_history.is_some());

    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Outcome(OutcomeBlock::new(
            RunOutcome::Completed {
                elapsed: Duration::from_secs(1),
                summary: crate::presentation::RunSummary {
                    files_changed: 0,
                    tool_calls: 0,
                    warnings: 0,
                },
            },
            None,
        )));
    session
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("persisted after local outcome".into())],
        })))
        .unwrap();
    shell.on_prompt_submitted("persisted after local outcome");
    shell.mark_prompt_persisted();
    drop(session);

    assert!(shell.materialize_deferred_history().unwrap());
    let state = shell.state.borrow();
    assert!(
        state
            .transcript_commit_ids
            .windows(2)
            .all(|ids| ids[0] < ids[1]),
        "materialized commit identities must remain strictly ordered: {:?}",
        state.transcript_commit_ids
    );
    let outcome = state
        .transcript
        .iter()
        .position(|block| matches!(block, TranscriptBlock::Outcome(_)))
        .expect("local outcome retained");
    let later_prompt = state
        .transcript
        .iter()
        .position(|block| {
            matches!(
                block,
                TranscriptBlock::User { text, .. } if text == "persisted after local outcome"
            )
        })
        .expect("later persisted prompt hydrated");
    assert!(outcome < later_prompt);
}

#[test]
fn resize_keeps_deferred_history_lazy_during_an_active_stream() {
    const WIDTH: u16 = 80;
    const RESIZED_WIDTH: u16 = 96;
    const HEIGHT: u16 = 12;

    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("active-resize-session.jsonl"),
        "active resize prompt",
        100,
    );
    let (mut shell, bytes) =
        emulated_shell_with_mode(crate::tui::theme::test_theme(), WIDTH, HEIGHT, false, true);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    shell.hydrate(&session).unwrap();
    assert!(shell.state.borrow().deferred_session_history.is_some());

    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "active-stream-before-resize".into(),
        },
    );
    shell.render();
    let _ = drain(&bytes);
    let (active_index_before, active_commit_id) = {
        let state = shell.state.borrow();
        let index = state.active_text.expect("active assistant stream");
        (index, state.transcript_commit_ids[index])
    };

    shell.set_size(RESIZED_WIDTH, HEIGHT);
    let active_index_after = {
        let state = shell.state.borrow();
        assert!(state.deferred_session_history.is_some());
        assert!(state.run.is_active());
        assert!(
            !state.transcript.iter().any(|block| matches!(
                block,
                TranscriptBlock::User { text, .. } if text == "active resize prompt 0"
            )),
            "resize must not materialize deferred history"
        );
        assert!(state
            .transcript_commit_ids
            .windows(2)
            .all(|ids| ids[0] < ids[1]));
        let index = state.active_text.expect("retained assistant stream");
        assert_eq!(index, active_index_before);
        assert_eq!(state.transcript_commit_ids[index], active_commit_id);
        index
    };

    shell.render();
    let resize = String::from_utf8_lossy(&drain(&bytes)).into_owned();
    assert!(resize.contains("\x1b[3J"), "{resize:?}");
    assert!(!resize.contains("active resize prompt 0"), "{resize:?}");
    assert!(resize.contains("active-stream-before-resize"), "{resize:?}");

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "-and-after".into(),
        },
    );
    let state = shell.state.borrow();
    assert_eq!(state.active_text, Some(active_index_after));
    let TranscriptBlock::Assistant(assistant) = &state.transcript[active_index_after] else {
        panic!("active stream must remain an assistant block");
    };
    assert_eq!(
        assistant.text, "active-stream-before-resize-and-after",
        "post-resize deltas must continue the retained live block"
    );
}

#[test]
fn delayed_resize_reconciliation_keeps_deferred_history_lazy() {
    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("reconciled-resize-session.jsonl"),
        "reconciled resize prompt",
        100,
    );
    let mut shell = lazy_history_test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    assert!(shell.state.borrow().deferred_session_history.is_some());
    shell.notice("live block before reconciled resize");
    let live_commit_id = *shell
        .state
        .borrow()
        .transcript_commit_ids
        .last()
        .expect("live block identity");

    assert!(reconcile_terminal_size(&shell.state, &shell.size, (91, 17)));
    let state = shell.state.borrow();
    assert_eq!(state.size, (91, 17));
    assert!(state.deferred_session_history.is_some());
    assert!(!state.transcript.iter().any(|block| matches!(
        block,
        TranscriptBlock::User { text, .. } if text == "reconciled resize prompt 0"
    )));
    assert!(matches!(
        state.transcript.last(),
        Some(TranscriptBlock::Notice(text)) if text == "live block before reconciled resize"
    ));
    assert_eq!(state.transcript_commit_ids.last(), Some(&live_commit_id));
}

#[test]
fn deferred_history_identity_failure_is_transactional() {
    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("transactional-history-session.jsonl"),
        "transactional prompt",
        100,
    );
    let mut shell = lazy_history_test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    assert!(shell.state.borrow().deferred_session_history.is_some());
    let run_id = shell.begin_run("openai");
    let tool_id = ToolCallId("transactional-live-tool".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: tool_id.clone(),
            name: "read".into(),
            args: serde_json::json!({"path": "live.rs"}),
        },
    );
    shell.notice("live block survives failed materialization");
    {
        let mut state = shell.state.borrow_mut();
        state
            .deferred_session_history
            .as_mut()
            .expect("deferred history")
            .retained_id_end = 0;
    }
    let before_snapshot = shell.debug_snapshot();
    let (before_len, before_ids, before_revisions, before_tools, before_deferred, before_next_id) = {
        let state = shell.state.borrow();
        (
            state.transcript.len(),
            state.transcript_commit_ids.clone(),
            state.block_revisions.clone(),
            state.tool_panels.clone(),
            state.deferred_session_history.clone(),
            state.next_transcript_commit_id.0,
        )
    };
    assert_eq!(before_tools.get(&tool_id).copied(), Some(before_len - 2));

    let error = shell.materialize_deferred_history().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("deferred history exhausted commit identity space"),
        "{error:#}"
    );
    let state = shell.state.borrow();
    assert_eq!(state.transcript.len(), before_len);
    assert_eq!(state.transcript_commit_ids, before_ids);
    assert_eq!(state.block_revisions, before_revisions);
    assert_eq!(state.tool_panels, before_tools);
    assert_eq!(state.deferred_session_history, before_deferred);
    assert_eq!(state.next_transcript_commit_id.0, before_next_id);
    drop(state);
    assert_eq!(shell.debug_snapshot(), before_snapshot);
}

#[test]
fn resumed_session_restores_every_write_as_a_diff_panel() {
    use ygg_agent::EntryValue;
    use ygg_ai::{
        AssistantMessage, AssistantPart, Message, Protocol, ToolCall, ToolResult, ToolResultPart,
        UserMessage, UserPart,
    };

    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
    session
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::Text("write both files".into())],
        })))
        .unwrap();

    let writes = [
        (
            "write-current",
            "new.rs",
            "ok\nnew.rs  created hash=x\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+current format\n",
        ),
        (
            "write-legacy",
            "legacy.rs",
            "ok\nlegacy.rs  created hash=y\n--- /dev/null\n+++ b/legacy.rs\n+legacy format\n",
        ),
    ];
    for (id, path, result) in writes {
        session
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId(id.into()),
                    name: "write".into(),
                    arguments_json: serde_json::json!({
                        "path": path,
                        "content": format!("{path} contents\n"),
                    })
                    .to_string(),
                    argument_error: None,
                })],
                model: ModelId("gpt-5.6-sol".into()),
                protocol: Protocol::OpenAiResponses,
            })))
            .unwrap();
        session
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::ToolResult(ToolResult {
                    tool_call_id: ToolCallId(id.into()),
                    content: vec![ToolResultPart::Text(result.into())],
                    is_error: false,
                    added_tool_names: None,
                })],
            })))
            .unwrap();
    }

    let mut shell = InteractiveShell::test_shell();
    shell.set_size(120, 40);
    shell.hydrate(&session).unwrap();
    let rendered = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 120).join("\n"));

    assert!(rendered.contains("current format"), "{rendered}");
    assert!(rendered.contains("legacy format"), "{rendered}");
    assert!(rendered.matches("/dev/null").count() >= 2, "{rendered}");
}

#[test]
fn duplicate_hydrated_tool_call_ids_never_leave_a_running_card() {
    use ygg_ai::{
        AssistantMessage, AssistantPart, Message, Protocol, ToolCall, ToolResult, ToolResultPart,
        UserMessage, UserPart,
    };

    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
    session
        .append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("duplicate".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"first"}"#.into(),
                    argument_error: None,
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("duplicate".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"second"}"#.into(),
                    argument_error: None,
                }),
            ],
            model: ModelId("test".into()),
            protocol: Protocol::OpenAiChat,
        })))
        .unwrap();
    session
        .append(EntryValue::Message(Message::User(UserMessage {
            content: vec![UserPart::ToolResult(ToolResult {
                tool_call_id: ToolCallId("duplicate".into()),
                content: vec![ToolResultPart::Text("durable result".into())],
                is_error: false,
                added_tool_names: None,
            })],
        })))
        .unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.hydrate(&session).unwrap();
    let state = shell.state.borrow();
    let panels = state
        .transcript
        .iter()
        .filter_map(|block| match block {
            TranscriptBlock::Tool(panel) => Some(panel.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(panels.len(), 2);
    assert!(
        panels.iter().all(|panel| panel.finished),
        "duplicate recovered IDs must never revive a running card: {panels:?}"
    );
    assert!(panels.iter().any(|panel| panel.is_error));
    assert!(panels.iter().any(|panel| !panel.is_error));
}

#[test]
fn streaming_assistant_cache_replaces_only_the_mutable_block_suffix() {
    const WIDTH: u16 = 80;
    let shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::streaming("# Stable heading\n\nmutable"),
        )));
        let _ = state.rendered_transcript(WIDTH);
    }

    let (block_start, old_lines) = {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        (cache.block_starts[0], cache.lines.clone())
    };
    {
        let mut state = shell.state.borrow_mut();
        let TranscriptBlock::Assistant(assistant) = &mut state.transcript[0] else {
            unreachable!()
        };
        assistant.append(" tail");
        state.touch_block(0);
        let _ = state.rendered_transcript(WIDTH);
    }

    let state = shell.state.borrow();
    let cache = state.transcript_cache.borrow();
    assert!(cache.last_update_start > block_start);
    assert_eq!(
        &cache.lines[..cache.last_update_start],
        &old_lines[..cache.last_update_start],
        "parser-committed rows were rebuilt"
    );
    let expected = render_block(
        None,
        &TranscriptBlock::Assistant(Box::new(AssistantBlock::streaming(
            "# Stable heading\n\nmutable tail",
        ))),
        &state.theme,
        &state.theme.rich_renderer(),
        &state.theme.reasoning_renderer(),
        WIDTH,
        false,
    );
    let start = cache.block_starts[0];
    let end = start + cache.block_lengths[0];
    assert_eq!(&cache.lines[start..end], expected);
}

#[test]
fn streamed_delta_marks_only_its_changed_cached_block() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_theme(crate::tui::theme::test_theme_from_source(
        SURFACE_TEST_THEME,
    ));
    for number in 0..500 {
        shell.notice(format!("historic {number}"));
    }
    shell.on_agent_event(&AgentEvent::OutputDelta {
        channel: OutputChannel::Text,
        text: "first".into(),
    });
    let _ = render_shell(&shell.state.borrow(), 120);
    let assistant_index = shell
        .state
        .borrow()
        .active_text
        .expect("active assistant block");

    // Keep a later block in the layout so this exercises the splice/start
    // adjustments as well as the no-history-scan dirty path.
    shell.notice("later block");
    shell.on_agent_event(&AgentEvent::OutputDelta {
        channel: OutputChannel::Text,
        text: " second".into(),
    });
    {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        assert_eq!(cache.dirty_blocks, vec![assistant_index]);
    }

    let rendered = render_shell(&shell.state.borrow(), 120).join("\n");
    assert!(rendered.contains("first second"));
    assert!(rendered.contains("later block"));
    assert!(shell
        .state
        .borrow()
        .transcript_cache
        .borrow()
        .dirty_blocks
        .is_empty());
    {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        assert_eq!(cache.block_geometries.len(), state.transcript.len());
        assert_eq!(cache.block_geometries[assistant_index].leading_rows, 1);
        assert_eq!(cache.block_geometries[assistant_index].trailing_rows, 1);
        let later = assistant_index + 1;
        assert_eq!(
            cache.block_starts[later],
            cache.block_starts[assistant_index] + cache.block_lengths[assistant_index]
        );
    }
}

#[test]
fn hidden_reasoning_stream_does_not_grow_native_scrollback() {
    const WIDTH: u16 = 64;
    const HEIGHT: u16 = 10;
    let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    terminal.process(&drain(&bytes));
    terminal.set_scrollback(usize::MAX);
    let baseline_scrollback = terminal.screen().scrollback();
    terminal.set_scrollback(0);

    let run_id = shell.begin_run("openai");
    for index in 0..160 {
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Reasoning,
                text: format!("private sentinel {index}\n"),
            },
        );
        shell.render();
    }
    terminal.process(&drain(&bytes));
    terminal.set_scrollback(usize::MAX);
    assert_eq!(
        terminal.screen().scrollback(),
        baseline_scrollback,
        "collapsed streaming reasoning must not commit mutable rows"
    );
    terminal.set_scrollback(0);
    let visible = terminal.screen().contents();
    assert!(visible.contains("Thinking"), "{visible:?}");
    assert!(!visible.contains("Working"), "{visible:?}");
    assert!(visible.contains("(ctrl+o to expand)"), "{visible:?}");
    assert!(!visible.contains("private sentinel"), "{visible:?}");
    let state = shell.state.borrow();
    let TranscriptBlock::Reasoning(reasoning) = state.transcript.last().unwrap() else {
        panic!("reasoning block expected");
    };
    assert!(reasoning.text.contains("private sentinel 159"));
}

#[test]
fn streamed_assistant_rows_enter_native_scrollback_once() {
    const WIDTH: u16 = 96;
    const HEIGHT: u16 = 48;
    let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    terminal.process(&drain(&bytes));

    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "private reasoning sentinel".into(),
        },
    );
    shell.render();
    terminal.process(&drain(&bytes));
    let mut response = String::from("# Stream report\n\n## Findings\n\n");
    for index in 0..48 {
        response.push_str(&format!(
            "- **stream-sentinel-{index:02}**: detailed finding for row {index}\n"
        ));
        if index == 15 {
            response.push_str("\n## Nested concerns\n\n");
        } else if index == 31 {
            response.push_str("\n## Final checks\n\n");
        }
    }
    let response_chars = response.chars().collect::<Vec<_>>();
    for chunk in response_chars.chunks(7) {
        shell.state.borrow_mut().advance_event_dot_animation();
        shell.render();
        terminal.process(&drain(&bytes));
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: chunk.iter().collect(),
            },
        );
        shell.render();
        terminal.process(&drain(&bytes));
    }

    // Grow the parser's viewport before looking back so its public
    // contents API can expose the complete retained history at once.
    terminal.set_size(512, WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();

    for index in 0..48 {
        let sentinel = format!("stream-sentinel-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel} was duplicated in native scrollback:\n{physical}"
        );
    }
}

#[test]
fn pi_renderer_keeps_streamed_transcript_complete_while_a_draft_is_open() {
    const WIDTH: u16 = 72;
    const HEIGHT: u16 = 12;
    let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    process_vt100_with_saved_line_clear(&mut terminal, &drain(&bytes), HEIGHT, WIDTH, 512);

    for index in 0..12 {
        shell.notice(format!("DRAFT-HISTORY-{index:02}"));
    }
    shell.apply_edit(EditAction::Char('x'));
    shell.render();
    process_vt100_with_saved_line_clear(&mut terminal, &drain(&bytes), HEIGHT, WIDTH, 512);

    let run_id = shell.begin_run("openai");
    for index in 0..32 {
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: format!(
                    "DRAFT-STREAM-{index:02} stays present while the mutable composer remains open.\n\n"
                ),
            },
        );
        shell.render();
        process_vt100_with_saved_line_clear(&mut terminal, &drain(&bytes), HEIGHT, WIDTH, 512);
    }

    terminal.set_size(256, WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();
    for index in 0..12 {
        let sentinel = format!("DRAFT-HISTORY-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel}:\n{physical}"
        );
    }
    for index in 0..32 {
        let sentinel = format!("DRAFT-STREAM-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel}:\n{physical}"
        );
    }
}

#[test]
fn ctrl_o_expands_and_collapses_the_inline_compaction_summary() {
    let mut shell = InteractiveShell::test_shell();
    shell.compaction_marker(
        "Context compacted · 12,000 input tokens summarized",
        "# Grounded summary\n\n- kept decision\n- **summary sentinel**",
    );
    let plain = |shell: &InteractiveShell| {
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"))
    };

    let collapsed = plain(&shell);
    assert!(
        collapsed.contains("12,000 input tokens summarized"),
        "{collapsed}"
    );
    assert!(collapsed.contains("ctrl+o to view"), "{collapsed}");
    assert!(!collapsed.contains("summary sentinel"), "{collapsed}");

    shell.toggle_disclosure();
    let expanded = plain(&shell);
    assert!(expanded.contains("Grounded summary"), "{expanded}");
    assert!(expanded.contains("summary sentinel"), "{expanded}");
    assert!(expanded.contains("ctrl+o to collapse"), "{expanded}");
    assert!(!shell.has_overlay(), "compaction must expand inline");

    shell.toggle_disclosure();
    let collapsed_again = plain(&shell);
    assert!(
        !collapsed_again.contains("summary sentinel"),
        "{collapsed_again}"
    );
    assert!(
        collapsed_again.contains("ctrl+o to view"),
        "{collapsed_again}"
    );
}

#[test]
fn autonomous_compaction_events_show_work_success_and_failure_inline() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5.6", "high");
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::CompactionStarted {
            reason: ygg_agent::CompactionReason::Threshold,
        },
    );
    let footer = strip_terminal_sequences(
        &crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            80,
            Instant::now() + Duration::from_secs(1),
        )
        .join("\n"),
    );
    assert!(!footer.contains("Working"), "{footer}");
    assert!(!footer.contains("compacting"), "{footer}");
    let compacting =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
    assert!(compacting.contains("• Compacting context"), "{compacting}");
    assert!(!compacting.contains("ctrl+o"), "{compacting}");

    shell.on_run_event(
        run_id,
        &AgentEvent::CompactionFinished {
            reason: ygg_agent::CompactionReason::Threshold,
            result: Ok(ygg_agent::CompactionInfo {
                kind: ygg_agent::CompactionKind::Local,
                summary: "# Automatic summary\n\nauto-summary sentinel".into(),
                first_kept: ygg_agent::EntryId("kept".into()),
                usage: ygg_ai::Usage::default(),
                elapsed: Duration::ZERO,
                cost_microdollars: None,
            }),
        },
    );
    let collapsed =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
    assert!(
        collapsed.contains("Context compacted automatically"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("Compacting context"), "{collapsed}");
    assert!(!collapsed.contains("auto-summary sentinel"), "{collapsed}");
    shell.toggle_disclosure();
    let expanded =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
    assert!(expanded.contains("auto-summary sentinel"), "{expanded}");

    let mut native_shell = InteractiveShell::test_shell();
    let native_run = native_shell.begin_run("openai");
    native_shell.on_run_event(
        native_run,
        &AgentEvent::CompactionFinished {
            reason: ygg_agent::CompactionReason::Threshold,
            result: Ok(ygg_agent::CompactionInfo {
                kind: ygg_agent::CompactionKind::NativeResponses {
                    checkpoint: ygg_agent::EntryId("checkpoint".into()),
                    covered_through: ygg_agent::EntryId("covered".into()),
                },
                summary: String::new(),
                first_kept: ygg_agent::EntryId("covered".into()),
                usage: ygg_ai::Usage::default(),
                elapsed: Duration::ZERO,
                cost_microdollars: None,
            }),
        },
    );
    let native = strip_terminal_sequences(
        &native_shell
            .state
            .borrow()
            .rendered_transcript(80)
            .join("\n"),
    );
    assert!(native.contains("Context compacted natively"), "{native}");
    assert!(native.contains("opaque Responses state"), "{native}");
    assert!(native.contains("retained"), "{native}");
    assert!(!native.contains("checkpoint"), "{native}");

    let mut failed_shell = InteractiveShell::test_shell();
    let failed_run = failed_shell.begin_run("openai");
    failed_shell.on_run_event(
        failed_run,
        &AgentEvent::CompactionStarted {
            reason: ygg_agent::CompactionReason::Overflow,
        },
    );
    failed_shell.on_run_event(
        failed_run,
        &AgentEvent::CompactionFinished {
            reason: ygg_agent::CompactionReason::Overflow,
            result: Err("cold endpoint timed out".into()),
        },
    );
    assert_eq!(
        failed_shell.debug_error().as_deref(),
        Some("automatic compaction failed: cold endpoint timed out")
    );
    assert!(failed_shell.state.borrow().run_label.is_empty());
}

#[test]
fn resumed_compaction_summary_remains_expandable_after_theme_switch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.jsonl");
    let mut session = Session::create(&path).unwrap();
    let first_kept = session
        .append(EntryValue::Config {
            model: Some("gpt-5.6".into()),
            reasoning: Some("high".into()),
            reasoning_mode: None,
        })
        .unwrap();
    session
        .append(EntryValue::Compaction {
            summary: "# Resumed summary\n\nresume-only sentinel".into(),
            first_kept,
            active_skills: Vec::new(),
            skill_resources: Vec::new(),
            details: Default::default(),
        })
        .unwrap();
    drop(session);

    let resumed = Session::open(path).unwrap();
    let mut shell = InteractiveShell::test_shell();
    shell.show_overlay_text("stale session overlay".into());
    shell.hydrate(&resumed).unwrap();
    assert!(
        !shell.has_overlay(),
        "resume must close session-local overlays"
    );
    let render = |shell: &InteractiveShell| {
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(72).join("\n"))
    };
    assert!(!render(&shell).contains("resume-only sentinel"));

    shell.toggle_disclosure();
    assert!(render(&shell).contains("resume-only sentinel"));
    shell.set_theme(crate::tui::theme::test_theme());
    let restyled = render(&shell);
    assert!(restyled.contains("resume-only sentinel"), "{restyled}");
    assert!(restyled.contains("ctrl+o to collapse"), "{restyled}");
}

#[test]
fn compaction_disclosure_preserves_native_presentation() {
    const WIDTH: u16 = 88;
    const HEIGHT: u16 = 18;
    for synchronized_output in [false, true] {
        let (mut shell, bytes) = emulated_shell_with_sync(
            crate::tui::theme::test_theme(),
            WIDTH,
            HEIGHT,
            synchronized_output,
        );
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        terminal.process(&drain(&bytes));

        for index in 0..24 {
            shell.notice(format!("compaction-history-{index:02}"));
        }
        let summary = format!(
            "# Summary\n\n{}",
            (0..40)
                .map(|index| format!("- compaction-detail-{index:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        shell.compaction_marker("Context compacted", summary);
        shell.render();
        terminal.process(&drain(&bytes));

        shell.toggle_disclosure();
        shell.render();
        let expansion = drain(&bytes);
        let expansion_text = String::from_utf8_lossy(&expansion);
        assert!(
            !expansion
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "Pi can repaint a disclosure that begins inside the visible viewport: {expansion_text:?}"
        );
        assert!(
            !expansion_text.contains("compaction-history-00"),
            "visible-tail differential update replayed off-screen history: {expansion_text:?}"
        );

        terminal.process(&expansion);
        terminal.set_scrollback(0);
        let visible = terminal.screen().contents();
        assert!(visible.contains("compaction-detail-39"), "{visible}");
        assert!(
            visible
                .lines()
                .any(|line| line == default_composer_rule(WIDTH)),
            "composer disappeared: {visible}"
        );

        shell.toggle_disclosure();
        shell.render();
        let collapse = drain(&bytes);
        let collapse_text = String::from_utf8_lossy(&collapse);
        assert!(
            collapse
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "Pi parity requires contraction above the viewport to clear and replay: {collapse_text:?}"
        );
        assert!(collapse_text.contains("compaction-history-00"));
        process_vt100_with_saved_line_clear(&mut terminal, &collapse, HEIGHT, WIDTH, 512);
        terminal.set_scrollback(0);
        let collapsed = terminal.screen().contents();
        assert!(collapsed.contains("ctrl+o to view"), "{collapsed}");
        assert!(!collapsed.contains("compaction-detail-"), "{collapsed}");
        assert!(
            collapsed
                .lines()
                .any(|line| line == default_composer_rule(WIDTH)),
            "composer disappeared: {collapsed}"
        );

        terminal.set_size(512, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();
        for index in 0..24 {
            let sentinel = format!("compaction-history-{index:02}");
            assert_eq!(
                physical.matches(&sentinel).count(),
                1,
                "{sentinel} was lost or duplicated with synchronized_output={synchronized_output}:\n{physical}"
            );
        }
    }
}

#[test]
fn removed_streaming_tail_keeps_a_tombstoned_commit_seam() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(64, 10);
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "first finalized paragraph\n\nreplacement remains mutable".into(),
        },
    );
    let _ = render_shell(&shell.state.borrow(), 64);

    let (active_index, retained_cursor) = {
        let state = shell.state.borrow();
        let active_index = state.active_text.expect("streaming assistant block");
        let TranscriptBlock::Assistant(assistant) = &state.transcript[active_index] else {
            panic!("active text must be an assistant block");
        };
        assert!(!assistant.layout.borrow().committed_block_ends().is_empty());
        (
            active_index,
            transcript_commit_cursor(&state, active_index, 0),
        )
    };

    shell.state.borrow_mut().discard_streaming_blocks();
    let _ = render_shell(&shell.state.borrow(), 64);
    let tombstone = transcript_commit_position(&shell.state.borrow(), retained_cursor)
        .expect("removed commit cursor should map to its insertion seam");
    assert_eq!(tombstone.cursor, retained_cursor);

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "retry output\n\nnext block".into(),
        },
    );
    let state = shell.state.borrow();
    assert_eq!(state.active_text, Some(active_index));
    let retry_cursor = transcript_commit_cursor(&state, active_index, 0);
    assert!(retry_cursor > retained_cursor);
}

#[test]
fn resize_matches_pi_clear_and_complete_replay_semantics() {
    const WIDTH: u16 = 48;
    const RESIZED_WIDTH: u16 = 64;
    const HEIGHT: u16 = 10;
    const SHELL_SENTINEL: &str = "PRE-YGG-SHELL-HISTORY";

    let (mut shell, bytes) =
        emulated_shell_with_sync(crate::tui::theme::test_theme(), WIDTH, HEIGHT, true);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    terminal.process(format!("{SHELL_SENTINEL}\r\n").as_bytes());
    terminal.process(&drain(&bytes));

    for index in 0..18 {
        shell.notice(format!("YGG-OWNED-RESIZE-{index:02}"));
    }
    shell.render();
    terminal.process(&drain(&bytes));

    terminal.set_size(256, WIDTH);
    terminal.set_scrollback(usize::MAX);
    assert!(terminal.screen().contents().contains(SHELL_SENTINEL));
    terminal.set_size(HEIGHT, WIDTH);
    terminal.set_scrollback(0);

    terminal.set_size(HEIGHT, RESIZED_WIDTH);
    shell.set_size(RESIZED_WIDTH, HEIGHT);
    shell.render();
    let resize = drain(&bytes);
    let resize_text = String::from_utf8_lossy(&resize);
    assert!(resize_text.contains("\x1b[?2026h"), "{resize_text:?}");
    assert!(
        resize_text.contains("\x1b[2J\x1b[H\x1b[3J"),
        "Pi resize must clear saved lines before replay: {resize_text:?}"
    );
    assert!(
        resize_text.contains("YGG-OWNED-RESIZE-00"),
        "Pi resize omitted off-screen logical history: {resize_text:?}"
    );
    assert!(
        resize_text.contains("YGG-OWNED-RESIZE-17"),
        "{resize_text:?}"
    );
    assert!(resize_text.contains("\x1b[?2026l"), "{resize_text:?}");
    process_vt100_with_saved_line_clear(&mut terminal, &resize, HEIGHT, RESIZED_WIDTH, 512);

    terminal.set_size(256, RESIZED_WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();
    assert!(
        !physical.contains(SHELL_SENTINEL),
        "Pi saved-line reset retained pre-application history: {physical}"
    );
    for index in 0..18 {
        let sentinel = format!("YGG-OWNED-RESIZE-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel} was lost or duplicated after resize:\n{physical}"
        );
    }
}

#[test]
fn resize_while_overlayed_replays_the_pi_composited_frame() {
    const WIDTH: u16 = 48;
    const RESIZED_WIDTH: u16 = 64;
    const HEIGHT: u16 = 10;

    let (mut shell, bytes) =
        emulated_shell_with_sync(crate::tui::theme::test_theme(), WIDTH, HEIGHT, true);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    terminal.process(&drain(&bytes));

    for index in 0..18 {
        shell.notice(format!("YGG-OVERLAY-RESIZE-{index:02}"));
    }
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "OVERLAY-ACTIVE-STREAM-BEFORE".into(),
        },
    );
    shell.render();
    terminal.process(&drain(&bytes));

    shell.show_overlay_text("ACTIVE-OVERLAY-SENTINEL".into());
    shell.render();
    terminal.process(&drain(&bytes));
    assert!(
        terminal
            .screen()
            .contents()
            .contains("ACTIVE-OVERLAY-SENTINEL"),
        "{}",
        terminal.screen().contents()
    );

    terminal.set_size(HEIGHT, RESIZED_WIDTH);
    shell.set_size(RESIZED_WIDTH, HEIGHT);
    shell.render();
    let resize = drain(&bytes);
    let resize_text = String::from_utf8_lossy(&resize);
    assert!(
        resize_text.contains("\x1b[2J\x1b[H\x1b[3J"),
        "{resize_text:?}"
    );
    assert!(
        resize_text.contains("ACTIVE-OVERLAY-SENTINEL"),
        "{resize_text:?}"
    );
    assert!(
        !resize_text.contains("OVERLAY-ACTIVE-STREAM-BEFORE"),
        "Pi replays the current composited frame, not rows hidden by its overlay: {resize_text:?}"
    );
    // The public response keeps a trailing Working row. The resize still
    // replays only the composited overlay frame, never its hidden live tail.
    for index in 0..17 {
        let sentinel = format!("YGG-OVERLAY-RESIZE-{index:02}");
        assert!(
            resize_text.contains(&sentinel),
            "{sentinel} was not replayed with the composited overlay:\n{resize_text:?}"
        );
    }
    assert!(
        !resize_text.contains("YGG-OVERLAY-RESIZE-17"),
        "unexpected mutable notice in resize replay: {resize_text:?}"
    );

    process_vt100_with_saved_line_clear(&mut terminal, &resize, HEIGHT, RESIZED_WIDTH, 512);
    assert!(
        terminal
            .screen()
            .contents()
            .contains("ACTIVE-OVERLAY-SENTINEL"),
        "{}",
        terminal.screen().contents()
    );

    terminal.set_size(256, RESIZED_WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();
    assert!(physical.contains("YGG-OVERLAY-RESIZE-00"), "{physical}");
    assert!(physical.contains("ACTIVE-OVERLAY-SENTINEL"), "{physical}");
    terminal.set_size(HEIGHT, RESIZED_WIDTH);
    terminal.set_scrollback(0);

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: " OVERLAY-ACTIVE-STREAM-AFTER".into(),
        },
    );
    shell.render();
    terminal.process(&drain(&bytes));
    assert!(
        terminal
            .screen()
            .contents()
            .contains("ACTIVE-OVERLAY-SENTINEL"),
        "{}",
        terminal.screen().contents()
    );

    shell.close_overlay();
    shell.render();
    let close = drain(&bytes);
    let close_text = String::from_utf8_lossy(&close);
    assert!(
        !close_text.contains("\x1b[3J"),
        "Pi can restore an overlay whose first changed row remains visible: {close_text:?}"
    );
    terminal.process(&close);
    terminal.set_size(256, RESIZED_WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();
    assert!(
        physical.contains("OVERLAY-ACTIVE-STREAM-BEFORE"),
        "{physical}"
    );
    assert!(
        physical.contains("OVERLAY-ACTIVE-STREAM-AFTER"),
        "{physical}"
    );
    for index in 0..18 {
        let sentinel = format!("YGG-OVERLAY-RESIZE-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel} was not retained exactly once after closing the overlay:\n{physical}"
        );
    }
}

#[test]
fn slash_popup_then_context_overlay_uses_pi_full_frame_replay() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 16;

    for synchronized_output in [false, true] {
        let (mut shell, bytes) = emulated_shell_with_sync(
            crate::tui::theme::test_theme(),
            WIDTH,
            HEIGHT,
            synchronized_output,
        );
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        terminal.process(&drain(&bytes));

        for index in 0..40 {
            shell.notice(format!("CONTEXT-OVERLAY-HISTORY-{index:02}"));
        }
        shell.render();
        terminal.process(&drain(&bytes));

        // Paint the tallest slash-command surface before completing the
        // command. This is the transition that used to advance the native
        // history seam by nine rows in a 16-row terminal.
        shell.apply_edit(EditAction::Char('/'));
        shell.render();
        terminal.process(&drain(&bytes));
        assert!(terminal.screen().contents().contains("commands 1–9/"));

        let (_directory, app) = crate::compaction::tests::app_for_estimate();
        shell.clear_editor();
        shell.show_context_report(crate::tui::context::ContextReport::capture(&app, &[]));
        shell.render();
        let context_frame = drain(&bytes);
        assert!(
            context_frame
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "Pi must clear and replay when the overlay changes above its viewport"
        );
        process_vt100_with_saved_line_clear(&mut terminal, &context_frame, HEIGHT, WIDTH, 512);
        terminal.set_scrollback(0);

        let visible = terminal.screen().contents();
        assert!(
            visible
                .lines()
                .next()
                .is_some_and(|line| line.contains("Context Usage")),
            "context heading was clipped with synchronized_output={synchronized_output}:\n{visible}"
        );
        assert!(visible.contains("Estimated usage by category"), "{visible}");
        assert!(visible.contains("Runtime framing and tools"), "{visible}");
        assert!(visible.contains("System instructions"), "{visible}");
        assert!(visible.contains("tokens"), "{visible}");
        assert!(
            visible.contains('⛀'),
            "context grid was clipped:\n{visible}"
        );
        assert!(
            visible
                .lines()
                .any(|line| line == default_composer_rule(WIDTH)),
            "composer disappeared:\n{visible}"
        );

        shell.close_overlay();
        shell.render();
        let close = drain(&bytes);
        assert!(
            !close
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "Pi can restore the context overlay from its visible first change"
        );
        terminal.process(&close);
        terminal.set_size(512, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();
        assert!(
            !physical.contains("Context Usage"),
            "overlay entered history:\n{physical}"
        );
        for index in 0..40 {
            let sentinel = format!("CONTEXT-OVERLAY-HISTORY-{index:02}");
            assert_eq!(
                physical.matches(&sentinel).count(),
                1,
                "{sentinel} was lost or duplicated with synchronized_output={synchronized_output}:\n{physical}"
            );
        }
    }
}

#[test]
fn native_scrollback_keeps_finalized_tool_stable_while_streaming_scrolled_away() {
    use ygg_agent::ToolOutput;

    const WIDTH: u16 = 72;
    const HEIGHT: u16 = 12;

    for synchronized_output in [false, true] {
        let (mut shell, bytes) = emulated_shell_with_sync(
            crate::tui::theme::test_theme(),
            WIDTH,
            HEIGHT,
            synchronized_output,
        );
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        terminal.process(&drain(&bytes));

        for index in 0..18 {
            shell.notice(format!("TOOL-SCROLLBACK-HISTORY-{index:02}"));
        }
        let run_id = shell.begin_run("openai");
        let tool_id = ToolCallId("tool-scrollback-regression".into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: tool_id.clone(),
                name: "bash".into(),
                args: serde_json::json!({"command": "TOOL-CARD-SENTINEL"}),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id: tool_id,
                result: Ok(ToolOutput::new("tool completed")),
                duration: Duration::from_millis(10),
            },
        );
        shell.render();
        terminal.process(&drain(&bytes));

        for index in 0..4 {
            shell.on_run_event(
                run_id,
                &AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: format!(
                        "STREAM-BEFORE-{index:02} has enough words to occupy a physical row.\n\n"
                    ),
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));
        }

        let offset = (1..=usize::from(HEIGHT))
            .find(|offset| {
                terminal.set_scrollback(*offset);
                terminal
                    .screen()
                    .contents()
                    .lines()
                    .take(terminal.screen().scrollback())
                    .any(|line| line.contains("TOOL-CARD-SENTINEL"))
            })
            .expect("finalized tool should be retained in native scrollback");
        terminal.set_scrollback(offset);
        let historical_row_count = terminal.screen().scrollback();
        let historical_view = terminal
            .screen()
            .contents()
            .lines()
            .take(historical_row_count)
            .map(str::to_owned)
            .collect::<Vec<_>>();

        for index in 0..12 {
            if index == 2 {
                // vt100 0.15 cannot materialize a viewport whose preserved
                // scrollback offset exceeds the screen height. Two streamed
                // frames are enough to exercise read-while-streaming; process
                // the remaining chronology from the live tail.
                terminal.set_scrollback(0);
            }
            shell.on_run_event(
                run_id,
                &AgentEvent::OutputDelta {
                    channel: OutputChannel::Text,
                    text: format!(
                        "STREAM-AFTER-{index:02} has enough words to occupy a physical row.\n\n"
                    ),
                },
            );
            shell.render();
            terminal.process(&drain(&bytes));
            if index < 2 {
                let viewed_history = terminal
                    .screen()
                    .contents()
                    .lines()
                    .take(historical_row_count)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                assert_eq!(
                    viewed_history, historical_view,
                    "historical tool surface changed during token {index} with synchronized_output={synchronized_output}"
                );
            }
        }

        terminal.set_size(256, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();
        assert_eq!(
            physical.matches("TOOL-CARD-SENTINEL").count(),
            1,
            "finalized tool was lost or duplicated:\n{physical}"
        );
        for index in 0..12 {
            let sentinel = format!("STREAM-AFTER-{index:02}");
            assert_eq!(
                physical.matches(&sentinel).count(),
                1,
                "{sentinel} was lost or duplicated:\n{physical}"
            );
        }
    }
}

#[test]
fn streamed_table_and_wrapped_lists_survive_shrink_scroll_and_resize() {
    const WIDTH: u16 = 96;
    const HEIGHT: u16 = 22;
    let (mut shell, bytes) =
        emulated_shell_with_sync(crate::tui::theme::test_theme(), WIDTH, HEIGHT, true);
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
    let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
        std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
    };
    terminal.process(&drain(&bytes));

    shell.on_prompt_submitted(
        "TABLE-PROMPT-SENTINEL: stream a long table while mutable chrome changes height",
    );
    shell.render();
    terminal.process(&drain(&bytes));

    // Submission is painted while idle. Beginning the run changes the
    // composer/status height before any model text arrives.
    let run_id = shell.begin_run("openai");
    shell.render();
    terminal.process(&drain(&bytes));
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "constructing a long boundary regression table".into(),
        },
    );
    shell.state.borrow_mut().advance_event_dot_animation();
    shell.render();
    terminal.process(&drain(&bytes));

    let mut update_probe = ShellFrameState::default();
    let initial_probe = render_shell_update(
        &shell.state.borrow(),
        WIDTH,
        Instant::now(),
        &mut update_probe,
    );
    assert!(!initial_probe.reanchor_viewport);
    let mut saw_streaming_row_shrink = false;
    let mut current_width = WIDTH;

    let mut response = String::from(
        "# TABLE HEADING SENTINEL\n\n\
             | Marker | Boundary condition details | Regression scenario details | Expected behavior details |\n\
             |---|---|---|---|\n",
    );
    for index in 0..12 {
        response.push_str(&format!(
                "| ROW{index:02}SENTINEL | boundary condition {index} contains enough distinct words to wrap | streaming markdown reparses this row as tokens arrive | retain exactly one final physical copy in terminal history |\n"
            ));
    }
    response.push_str(
            "\n## LIST HEADING SENTINEL\n\n\
             - **WRAPPED-LIST-SENTINEL** remains unique while this deliberately long list item wraps across several terminal cells and rows.\n",
        );
    for (chunk_index, chunk) in response.as_bytes().chunks(5).enumerate() {
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: String::from_utf8(chunk.to_vec()).unwrap(),
            },
        );

        // Change the emulator dimensions before delivering the resize event,
        // matching terminal event order. `vt100` does not model modern
        // soft-wrap reflow; the ED 3 helper below models only the reset.
        // Keep a nonzero scrollback offset while more tokens arrive, then
        // widen again during the same generation.
        let resized = match chunk_index {
            180 => Some(61),
            360 => Some(WIDTH),
            _ => None,
        };
        if let Some(width) = resized {
            terminal.set_scrollback(6);
            terminal.set_size(HEIGHT, width);
            shell.set_size(width, HEIGHT);
            current_width = width;
        }

        let previous_rows = update_probe.transcript_len;
        let probe = render_shell_update(
            &shell.state.borrow(),
            current_width,
            Instant::now(),
            &mut update_probe,
        );
        saw_streaming_row_shrink |= update_probe.transcript_len < previous_rows;
        assert_eq!(
            probe.reanchor_viewport,
            resized.is_some(),
            "only width reflow should reanchor the streaming viewport"
        );
        shell.render();
        let output = drain(&bytes);
        process_vt100_with_saved_line_clear(&mut terminal, &output, HEIGHT, current_width, 512);
        if chunk_index == 360 {
            terminal.set_scrollback(0);
        }
    }
    assert!(
        saw_streaming_row_shrink,
        "fixture did not exercise a shrinking streamed layout"
    );

    terminal.set_size(256, WIDTH);
    terminal.set_scrollback(usize::MAX);
    let physical = terminal.screen().contents();
    let rule = default_composer_rule(WIDTH);
    assert_eq!(
        physical.lines().filter(|line| line == &rule).count(),
        2,
        "mutable composer was committed to scrollback:\n{physical}"
    );
    for sentinel in [
        "TABLE-PROMPT-SENTINEL",
        "TABLE HEADING SENTINEL",
        "ROW00SENTINEL",
        "ROW05SENTINEL",
        "ROW11SENTINEL",
        "LIST HEADING SENTINEL",
        "WRAPPED-LIST-SENTINEL",
    ] {
        assert_eq!(
            physical.matches(sentinel).count(),
            1,
            "{sentinel:?} was duplicated in native scrollback:\n{physical}"
        );
    }
}

#[test]
fn closing_overlay_reanchors_without_replaying_native_scrollback() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 16;
    for synchronized_output in [false, true] {
        let (mut shell, bytes) = emulated_shell_with_sync(
            crate::tui::theme::test_theme(),
            WIDTH,
            HEIGHT,
            synchronized_output,
        );
        let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 512);
        let drain = |bytes: &Arc<Mutex<Vec<u8>>>| {
            std::mem::take(&mut *bytes.lock().expect("emulated terminal bytes"))
        };
        terminal.process(&drain(&bytes));

        for index in 0..32 {
            shell.notice(format!("overlay-history-{index:02}"));
        }
        shell.render();
        terminal.process(&drain(&bytes));

        shell.show_overlay_text("status overlay\nclose me".into());
        shell.render();
        terminal.process(&drain(&bytes));

        shell.close_overlay();
        shell.render();
        let close_frame = drain(&bytes);
        // Closing a one-viewport overlay must repaint only the visible
        // tail. Replaying the retained transcript here is what created
        // duplicated lines in terminal-owned scrollback.
        assert!(
            close_frame.len() < 8 * 1024,
            "overlay close replayed an unbounded frame ({} bytes)",
            close_frame.len()
        );
        terminal.process(&close_frame);

        terminal.set_size(512, WIDTH);
        terminal.set_scrollback(usize::MAX);
        let physical = terminal.screen().contents();
        for index in 0..32 {
            let sentinel = format!("overlay-history-{index:02}");
            assert_eq!(
                physical.matches(&sentinel).count(),
                1,
                "{sentinel} replayed with synchronized_output={synchronized_output}:\n{physical}"
            );
        }
    }
}

#[test]
fn tool_progress_repaints_the_bounded_rendered_tail() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    let id = ToolCallId("long-bash".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "bash".into(),
            args: serde_json::json!({"command": "long-running-audit"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolProgress {
            id: id.clone(),
            progress: ToolProgress::Output {
                stream: ygg_agent::OutputStream::Stdout,
                bytes: bytes::Bytes::from_static(b"private live output"),
            },
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolProgress {
            id: id.clone(),
            progress: ToolProgress::Status("private status detail".into()),
        },
    );
    let rendered = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 96).join("\n"));
    assert!(rendered.contains("Bash  long-running-audit"), "{rendered}");
    assert!(rendered.contains("private live output"), "{rendered}");
    assert!(rendered.contains("private status detail"), "{rendered}");
    assert!(shell.debug_tool_output(&id).is_some());
}

#[test]
fn short_transcript_chrome_follows_content_without_viewport_padding() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 40);
    shell.set_identity("codex", "gpt-5.6", "high");
    let run_id = shell.begin_run("codex");
    let now = Instant::now();

    let composer_row = |lines: &[String]| {
        lines
            .iter()
            .position(|line| line.contains(CURSOR_MARKER))
            .expect("composer cursor row")
    };
    let initial = render_shell_at(&shell.state.borrow(), 80, now);
    let initial_composer = composer_row(&initial);
    assert!(
        initial.len() < 40,
        "native mode must not pad a short frame to the terminal height"
    );

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "I’ll inspect the tree.".into(),
        },
    );
    let streamed = render_shell_at(&shell.state.borrow(), 80, now);
    // The response retains one trailing Working row while the run is active.
    // The response row plus its transition therefore grow the short frame by
    // two rows without padding it to the terminal height.
    assert_eq!(composer_row(&streamed), initial_composer + 2);
    assert!(streamed.len() < 40);

    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: ToolCallId("read-1".into()),
            name: "read".into(),
            args: serde_json::json!({"path": "src/main.rs"}),
        },
    );
    let tool = render_shell_at(&shell.state.borrow(), 80, now);
    assert_eq!(
        composer_row(&tool),
        composer_row(&streamed),
        "the active tool should replace, not duplicate, the trailing Working row"
    );
    assert!(!tool
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .any(|line| line.contains("Working")));
    assert!(tool.len() < 40);

    shell.queue_steering(&ComposedInput::from_text("also inspect tests".into()));
    let steering = render_shell_at(&shell.state.borrow(), 80, now);
    assert!(composer_row(&steering) > composer_row(&tool));
    assert!(steering.len() < 40);
    let steering_plain = steering
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(steering_plain.contains("Steering · queued"));
    assert!(steering_plain.contains("  └ also inspect tests"));

    // The native-scrollback renderer retains committed transcript rows and
    // returns only the mutable suffix after the first frame.
    let mut frame = ShellFrameState::default();
    let first = render_shell_update(&shell.state.borrow(), 80, now, &mut frame);
    assert_eq!(first.stable_prefix, 0);
    assert_eq!(first.replacement, steering);
    assert!(!first.rebuild_scrollback);
    let next = render_shell_update(&shell.state.borrow(), 80, now, &mut frame);
    assert!(next.stable_prefix > 0);
    assert!(!next.rebuild_scrollback);
    assert!(next.stable_prefix + next.replacement.len() < 40);
    assert!(next
        .replacement
        .iter()
        .any(|line| line.contains(CURSOR_MARKER)));
}

#[test]
fn emulated_native_short_frame_does_not_pin_composer_to_terminal_bottom() {
    const WIDTH: u16 = 80;
    const HEIGHT: u16 = 40;
    let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
    shell.set_identity("codex", "gpt-5.6", "high");
    shell.notice("recent transcript row");
    shell.render();

    let output = bytes.lock().unwrap().clone();
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 0);
    terminal.process(&output);
    assert!(
        terminal
            .screen()
            .contents()
            .contains("recent transcript row"),
        "transcript was not painted: {:?}",
        terminal.screen().contents()
    );
    let (cursor_row, _) = terminal.screen().cursor_position();
    assert!(
        cursor_row < HEIGHT / 2,
        "short native frame pinned the composer at terminal row {cursor_row}"
    );
}

#[test]
fn slash_popup_height_changes_use_differential_repaint() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 20);
    for character in "/res".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    let mut frame = ShellFrameState::default();
    let initial = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(!initial.reanchor_viewport);
    let composer_row = |update: &sexy_tui_rs::FrameUpdate| {
        update
            .replacement
            .iter()
            .position(|line| line.contains(CURSOR_MARKER))
            .expect("composer cursor row")
    };
    let initial_composer_row = composer_row(&initial);

    for _ in 0..3 {
        shell.apply_edit(EditAction::Backspace);
    }
    assert_eq!(shell.pending(), "/");
    let expanded = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(
        !expanded.reanchor_viewport,
        "growing mutable chrome must not replay the viewport"
    );
    assert_eq!(
        composer_row(&expanded),
        initial_composer_row,
        "suggestion growth must expand below the composer"
    );

    for character in "res".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let collapsed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(
        !collapsed.reanchor_viewport,
        "shrinking mutable chrome must clear only its changed tail"
    );
    assert_eq!(
        composer_row(&collapsed),
        initial_composer_row,
        "suggestion shrinkage must leave the composer in place"
    );
}

#[test]
fn native_scrollback_frame_exposes_committed_rows_and_reuses_stable_history() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    for number in 0..80 {
        shell.notice(format!("older {number}"));
    }
    shell.notice("live streamed result");

    let full = render_shell(&shell.state.borrow(), 80);
    let full_text = full.join("\n");
    assert!(full.len() > 14);
    assert!(full_text.contains("older 0"));
    assert!(full_text.contains("live streamed result"));

    let mut frame = ShellFrameState::default();
    let initial = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert_eq!(initial.stable_prefix, 0);
    assert!(!initial.rebuild_scrollback);
    let committed = frame.transcript_len;

    shell.notice("new native row");
    let appended = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert_eq!(appended.stable_prefix, committed);
    assert!(!appended.rebuild_scrollback);
    let appended_text = appended.replacement.join("\n");
    assert!(appended_text.contains("new native row"));
    assert!(!appended_text.contains("older 0"));
}

#[test]
fn native_ctrl_o_rebuilds_offscreen_compaction_and_contracts_the_complete_frame() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 12);
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
            label: "Context compacted".into(),
            summary: "COMPLETE COMPACTION SUMMARY".into(),
            expanded: false,
        })));
        for number in 0..40 {
            state.push_block(TranscriptBlock::Notice(format!("later event {number}")));
        }
    }

    let mut frame = ShellFrameState::default();
    let initial = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(!initial.rebuild_scrollback);
    assert!(initial.pinned.is_some());
    assert!(initial.stable_prefix + initial.replacement.len() > 12);
    assert!(!initial
        .replacement
        .iter()
        .any(|line| line.contains("COMPLETE COMPACTION SUMMARY")));

    shell.toggle_disclosure();
    let expanded = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert_eq!(expanded.stable_prefix, 0);
    assert!(expanded.rebuild_scrollback);
    assert!(expanded.pinned.is_some());
    assert!(expanded
        .replacement
        .iter()
        .any(|line| line.contains("COMPLETE COMPACTION SUMMARY")));

    shell.toggle_disclosure();
    let collapsed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert_eq!(collapsed.stable_prefix, 0);
    assert!(collapsed.rebuild_scrollback);
    assert!(collapsed.pinned.is_some());
    assert!(!collapsed
        .replacement
        .iter()
        .any(|line| line.contains("COMPLETE COMPACTION SUMMARY")));
    assert!(collapsed
        .replacement
        .iter()
        .any(|line| line.contains(CURSOR_MARKER)));
}

#[test]
fn theme_swap_matches_pi_clear_and_complete_replay() {
    const WIDTH: u16 = 32;
    const HEIGHT: u16 = 10;
    let theme_source = |name: &str, foreground: &str| {
        crate::tui::theme::test_theme_from_source(&format!(
            r##"
                    [metadata]
                    name = "{name}"

                    [colors]
                    foreground = "{foreground}"
                "##
        ))
    };
    let first_theme = theme_source("Viewport red", "#b01020");
    let old_foreground = role_rgb_color(&first_theme, "foreground");
    let (mut shell, bytes) = emulated_shell(first_theme, WIDTH, HEIGHT);
    {
        let mut state = shell.state.borrow_mut();
        for number in 0..12 {
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized(format!("historic-{number}")),
            )));
        }
    }
    shell.render();

    let before = bytes
        .lock()
        .expect("emulated terminal output mutex poisoned")
        .clone();
    let mut before_terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
    before_terminal.process(&before);
    let blank_row = before_terminal
        .screen()
        .rows(0, WIDTH)
        .enumerate()
        .find_map(|(row, contents)| contents.trim().is_empty().then_some(row as u16))
        .expect("fixture should leave a visible semantic separator row");

    // Put a cell into a row that is byte-identical across the two logical
    // frames. A changed-row diff would leave this corruption behind; the
    // required full visible repaint must erase it.
    bytes
        .lock()
        .expect("emulated terminal output mutex poisoned")
        .extend_from_slice(
            format!("\x1b[{};{}H\x1b[48;2;1;2;3mX\x1b[0m", blank_row + 1, WIDTH).as_bytes(),
        );

    let second_theme = theme_source("Viewport blue", "#2040c0");
    let new_foreground = role_rgb_color(&second_theme, "foreground");
    shell.set_theme(second_theme);
    shell.render();

    let complete = bytes
        .lock()
        .expect("emulated terminal output mutex poisoned")
        .clone();
    assert!(
        complete
            .windows(b"\x1b[3J".len())
            .any(|window| window == b"\x1b[3J"),
        "Pi theme changes above the viewport must clear and replay"
    );
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
    process_vt100_with_saved_line_clear(&mut terminal, &complete, HEIGHT, WIDTH, 128);
    assert!(
        find_ascii_cell(terminal.screen(), "historic-").is_some(),
        "visible tail lost after theme replay: {:?}",
        terminal.screen().contents()
    );
    assert_ascii_foreground(&terminal, "historic-11", new_foreground);
    assert!(
        find_ascii_cell(terminal.screen(), "X").is_none(),
        "full replay left a stale cell: {:?}",
        terminal.screen().contents()
    );

    terminal.set_size(128, WIDTH);
    terminal.set_scrollback(usize::MAX);
    for number in 0..12 {
        assert_ascii_foreground(&terminal, &format!("historic-{number}"), new_foreground);
    }
    assert_ne!(old_foreground, new_foreground);
}

#[test]
fn application_viewport_theme_swap_repaints_without_clearing_shell_scrollback() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    let mut frame = ShellFrameState::default();
    let now = Instant::now();

    let initial = render_shell_viewport_update(&shell.state.borrow(), 80, now, &mut frame);
    assert!(!initial.reanchor_viewport);
    assert!(!initial.rebuild_scrollback);

    shell.set_theme(crate::tui::theme::test_theme_from_source(
        r##"
                [metadata]
                name = "Application viewport theme"

                [colors]
                foreground = "#2040c0"
            "##,
    ));
    let repainted = render_shell_viewport_update(&shell.state.borrow(), 80, now, &mut frame);
    assert!(repainted.reanchor_viewport);
    assert!(!repainted.rebuild_scrollback);
}

#[test]
fn switching_back_to_default_clears_custom_theme_attributes() {
    const WIDTH: u16 = 48;
    const HEIGHT: u16 = 10;
    let custom = crate::tui::theme::test_theme_from_source(SURFACE_TEST_THEME);
    let (mut shell, bytes) = emulated_shell(custom, WIDTH, HEIGHT);
    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("plain-default-prose".into()),
        )));
    shell.render();

    // Custom surface rendering terminates every row with a full rendition
    // reset. Switching themes must clear and replay the complete frame.
    shell.set_theme(crate::tui::theme::test_theme());
    shell.render();

    let complete = bytes
        .lock()
        .expect("emulated terminal output mutex poisoned")
        .clone();
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
    terminal.process(&complete);
    assert_ascii_default_rendition(&terminal, "plain-default-prose");
}

#[test]
fn new_session_shrink_reanchors_but_picker_growth_does_not() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    for number in 0..80 {
        shell.notice(format!("resumed history {number}"));
    }

    let mut frame = ShellFrameState::default();
    let resumed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(!resumed.reanchor_viewport);
    assert!(!resumed.rebuild_scrollback);
    assert!(frame.transcript_len > 14);

    {
        let mut state = shell.state.borrow_mut();
        state.transcript_epoch = state.transcript_epoch.wrapping_add(1);
        state.transcript.clear();
        state.transcript_commit_ids.clear();
        state.block_revisions.clear();
        state.invalidate_transcript_layout();
        state.push_block(TranscriptBlock::Notice("new session created".into()));
    }
    let fresh = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(fresh.reanchor_viewport);
    assert!(!fresh.rebuild_scrollback);
    assert!(fresh.stable_prefix + fresh.replacement.len() < 14);

    shell.open_panel(Panel::SelectList {
        surface: OrdinarySurfaceMetadata::new("Models"),
        items: vec!["model-a".into(), "model-b".into()],
        descriptions: vec![None, None],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectModel(vec![
            ModelId("model-a".into()),
            ModelId("model-b".into()),
        ]),
    });
    let picker = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(
        !picker.reanchor_viewport,
        "inserting picker rows must use a differential tail repaint"
    );
    assert!(!picker.rebuild_scrollback);
    assert!(picker.stable_prefix + picker.replacement.len() <= 14);
    assert!(picker
        .replacement
        .iter()
        .any(|line| line.contains("Models")));
    assert!(picker
        .replacement
        .iter()
        .any(|line| line.contains(CURSOR_MARKER)));
}

#[test]
fn keyboard_page_navigation_claims_the_semantic_viewport_without_mouse_capture() {
    const WIDTH: u16 = 80;
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(WIDTH, 14);
    for number in 0..80 {
        shell.notice(format!("keyboard viewport {number:02}"));
    }
    assert!(!shell.state.borrow().application_viewport_requested);

    let component = ShellComponent::new(shell.state.clone(), false);
    let native = sexy_tui_rs::Component::render_update(&component, WIDTH).expect("native frame");
    assert!(native.pinned.is_some());

    shell.scroll(-1);
    assert!(shell.state.borrow().application_viewport_requested);
    let scrolled =
        sexy_tui_rs::Component::render_update(&component, WIDTH).expect("semantic viewport frame");
    assert!(scrolled.pinned.is_none());
    assert!(scrolled.reanchor_viewport);
    assert!(
        scrolled
            .replacement
            .iter()
            .any(|line| line.contains("PageDown returns to live")),
        "{:?}",
        scrolled.replacement
    );
    assert!(scrolled.replacement.len() <= 14);

    // Returning to live keeps semantic viewport ownership; mouse reporting was
    // never enabled and therefore remains an independent policy decision.
    shell.jump_to_tail();
    let live =
        sexy_tui_rs::Component::render_update(&component, WIDTH).expect("semantic live frame");
    assert!(live.pinned.is_none());
    assert!(live.replacement.len() <= 14);
    assert!(!live
        .replacement
        .iter()
        .any(|line| line.contains("PageDown returns to live")));
}

#[test]
fn explicit_application_viewport_bounds_history_and_keeps_old_rows_reachable() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    for number in 0..80 {
        shell.notice(format!("older {number}"));
    }
    shell.notice("live streamed result");

    let live = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
    let live_text = live.join("\n");
    assert_eq!(live.len(), 14);
    assert!(!live_text.contains("older 0"));
    assert!(live_text.contains("older 79"));
    assert!(live_text.contains("live streamed result"));

    shell.scroll_lines(-10_000);
    let oldest = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
    let oldest_text = oldest.join("\n");
    assert_eq!(oldest.len(), 14);
    assert!(oldest_text.contains("older 0"), "{oldest_text}");
    assert!(oldest_text.contains("PageDown returns to live"));
    assert!(!oldest_text.contains("live streamed result"));

    shell.scroll_lines(10_000);
    let returned = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now()).join("\n");
    assert!(returned.contains("live streamed result"));
    assert!(!returned.contains("PageDown returns to live"));

    shell.select_all_transcript();
    let copied = shell.copy_selected_plain_text().expect("semantic copy");
    assert!(copied.contains("older 0"));
    assert!(copied.contains("live streamed result"));
}

#[test]
fn application_viewport_stays_anchored_while_one_markdown_block_streams() {
    const WIDTH: u16 = 80;
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(WIDTH, 16);
    for number in 0..80 {
        shell.notice(format!("reading anchor {number:02}"));
    }

    let _ = render_shell_viewport_at(&shell.state.borrow(), WIDTH, Instant::now());
    shell.scroll_lines(-8);
    let anchor_rows = |shell: &InteractiveShell| {
        render_shell_viewport_at(&shell.state.borrow(), WIDTH, Instant::now())
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .filter(|line| line.contains("reading anchor"))
            .collect::<Vec<_>>()
    };
    let before = anchor_rows(&shell);
    assert!(!before.is_empty());

    let run_id = shell.begin_run("openai");
    for number in 0..24 {
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: format!(
                    "\n\n### streamed section {number}\n\nA growing Markdown paragraph whose wrapping changes while the reader remains above the live tail."
                ),
            },
        );
        assert_eq!(
            anchor_rows(&shell),
            before,
            "viewport moved on token batch {number}"
        );
    }

    let scrolled =
        render_shell_viewport_at(&shell.state.borrow(), WIDTH, Instant::now()).join("\n");
    assert!(scrolled.contains("new"), "{scrolled}");
    assert!(scrolled.contains("PageDown returns to live"), "{scrolled}");
}

#[test]
fn semantic_viewport_anchor_survives_wrapped_markdown_resize() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 16);
    let markdown = (0..80)
        .map(|number| {
            format!(
                "paragraph-{number:02} carries enough stable prose to wrap differently after a narrow resize"
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized(markdown),
        )));
        state.push_block(TranscriptBlock::Notice("live tail".into()));
    }

    let _ = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
    shell.scroll_lines(-40);
    let _ = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
    let before = shell
        .state
        .borrow()
        .viewport_anchor
        .get()
        .expect("scrolled semantic anchor");
    assert!(before.semantic);

    shell.set_size(42, 16);
    let _ = render_shell_viewport_at(&shell.state.borrow(), 42, Instant::now());
    let state = shell.state.borrow();
    let after = state
        .viewport_anchor
        .get()
        .expect("anchor retained after resize");
    assert_eq!(after.commit_id, before.commit_id);
    assert_eq!(after.text_offset, before.text_offset);

    let chrome = shell_chrome(&state, 42, Instant::now());
    let transcript = state.rendered_transcript(42);
    let maximum = max_scroll_for_available(transcript.len(), chrome.transcript_rows);
    let scroll = state.scroll_from_bottom.get().min(maximum);
    let capacity = transcript_viewport_capacity(chrome.transcript_rows, scroll > 0);
    let end = transcript.len().saturating_sub(scroll);
    let start = end.saturating_sub(capacity);
    drop(transcript);
    let anchored = selection_position_for_visual_cell(
        &state,
        start + after.desired_screen_row.min(capacity.saturating_sub(1)),
        0,
    )
    .expect("anchored semantic row after resize");
    assert_eq!(state.transcript_commit_ids[anchored.block], after.commit_id);
    assert!(anchored.offset <= after.text_offset);
    let next = selection_position_for_visual_cell(
        &state,
        start + after.desired_screen_row.min(capacity.saturating_sub(1)) + 1,
        0,
    )
    .expect("row following semantic anchor");
    assert!(
        after.text_offset <= next.offset,
        "semantic point {after:?} escaped anchored rows {anchored:?}..{next:?}"
    );
}

#[test]
fn semantic_viewport_anchor_survives_disclosure_contraction_above_it() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 16);
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
            label: "Context compacted".into(),
            summary: (0..60)
                .map(|number| format!("expanded summary row {number}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
            expanded: false,
        })));
        for number in 0..80 {
            state.push_block(TranscriptBlock::Notice(format!(
                "stable event after compaction {number:02}"
            )));
        }
    }
    shell.toggle_disclosure();
    let _ = render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now());
    shell.scroll_lines(-24);
    let visible_events = |shell: &InteractiveShell| {
        render_shell_viewport_at(&shell.state.borrow(), 80, Instant::now())
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .filter(|line| line.contains("stable event after compaction"))
            .collect::<Vec<_>>()
    };
    let before = visible_events(&shell);
    assert!(!before.is_empty());

    shell.toggle_disclosure();
    let after = visible_events(&shell);
    assert_eq!(after, before);
}

#[test]
fn select_all_copy_is_semantic_and_excludes_pinned_chrome() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-test", "high");
    for number in 0..120 {
        shell.on_prompt_submitted(&format!("user {number}"));
        shell
            .state
            .borrow_mut()
            .push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized(format!(
                    "**assistant {number}**\n\n```rust\nlet n = {number};\n```"
                )),
            )));
    }
    shell.select_all_transcript();
    let copied = shell.copy_selected_plain_text().expect("selection copy");
    assert!(copied.contains("user 0"));
    assert!(copied.contains("assistant 119"));
    assert!(copied.contains("let n = 119;"));
    assert!(!copied.contains(CURSOR_MARKER));
    assert!(!copied.contains("gpt-test"));
    assert!(!copied.contains("\x1b["));
    assert_eq!(shell.copy_buffer().as_deref(), Some(copied.as_str()));
}

#[test]
fn drag_selection_autoscrolls_through_a_transcript_ten_viewports_tall() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    for number in 0..180 {
        shell.on_prompt_submitted(&format!("record {number}"));
    }
    // Establish the cached viewport before mapping mouse rows.
    let _ = render_shell(&shell.state.borrow(), 80);
    let available = shell_chrome(&shell.state.borrow(), 80, Instant::now()).transcript_rows;
    let bottom_row = transcript_viewport_capacity(available, false).saturating_sub(1) as u16;
    // Begin at the physical end of the newest row so the reverse drag
    // includes that complete semantic block as well as the oldest one.
    shell.begin_transcript_selection(bottom_row, 79, false);
    for _ in 0..240 {
        shell.extend_transcript_selection(0, 0);
    }
    shell.end_transcript_selection(0, 0);
    let copied = shell.copy_selected_plain_text().expect("drag copy");
    assert!(copied.contains("record 0"), "{copied}");
    assert!(copied.contains("record 179"), "{copied}");
    assert!(!copied.contains(CURSOR_MARKER));
}

#[test]
fn dragging_into_pinned_chrome_clamps_to_last_semantic_transcript_row() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    for number in 0..40 {
        shell.on_prompt_submitted(&format!("record {number}"));
    }
    let _ = render_shell(&shell.state.borrow(), 80);
    let capacity = transcript_viewport_capacity_for_state(&shell.state.borrow(), 80);
    assert!(capacity > 1);

    shell.begin_transcript_selection(0, 0, false);
    shell.extend_transcript_selection(13, 0);

    let state = shell.state.borrow();
    let expected = InteractiveShell::transcript_position_at_screen_cell(
        &state,
        capacity.saturating_sub(1) as u16,
        0,
    )
    .expect("last transcript row");
    assert_eq!(
        state
            .transcript_selection
            .as_ref()
            .expect("drag selection")
            .focus,
        expected
    );
}

#[test]
fn overscrolled_viewport_clamps_to_available_transcript() {
    let mut shell = InteractiveShell::test_shell();
    shell.on_prompt_submitted("visible prompt");
    shell.state.borrow().scroll_from_bottom.set(9_999);
    let rendered = render_shell(&shell.state.borrow(), 120);
    assert!(rendered.iter().any(|line| line.contains("visible prompt")));
    shell.scroll(1);
    assert_eq!(shell.state.borrow().scroll_from_bottom.get(), 0);
}

#[test]
fn character_accurate_selection_maps_correct_columns() {
    for inset in [0_u16, 2, 4] {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(80, 14);
        shell.set_theme(theme_with_layout(&format!("transcript_inset = {inset}")));
        shell.on_prompt_submitted("hello world");

        // Establish the cached viewport. Prompts share the resolved inset;
        // selection geometry removes both that inset and the marker cells.
        let _ = render_shell(&shell.state.borrow(), 80);
        let resolved = crate::tui::layout::PresentationLayout::new(&shell.state.borrow().theme, 80);
        let start = resolved.inset + 3; // marker (2) + cell index of 'e' (1)
        let end = start + 4;
        shell.begin_transcript_selection(0, start, false);
        shell.extend_transcript_selection(0, end);
        shell.end_transcript_selection(0, end);

        let copied = shell
            .copy_selected_plain_text()
            .expect("character drag copy");
        assert_eq!(copied, "ello", "transcript inset {inset}");
    }
}

fn rendered_phase(phase: RunPhase) -> String {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("relay", "gpt-5.6", "high");
    let now = Instant::now();
    {
        let mut state = shell.state.borrow_mut();
        let id = state.run.begin_at("relay", now).unwrap();
        state.run.set_phase_at(id, phase, now);
    }
    let rendered =
        render_shell_at(&shell.state.borrow(), 80, now + Duration::from_millis(600)).join("\n");
    rendered
}

#[test]
fn renderer_covers_idle_and_every_active_run_phase() {
    let mut idle = InteractiveShell::test_shell();
    idle.set_identity("relay", "gpt-5.6", "high");
    let idle = render_shell(&idle.state.borrow(), 80).join("\n");
    assert!(idle.contains("GPT-5.6"), "{idle}");
    assert!(!idle.contains("relay / "));
    // No newline shortcut hint is shown in idle footer

    let cases = [
        RunPhase::AwaitingProvider {
            provider: "relay".into(),
        },
        RunPhase::Thinking,
        RunPhase::StreamingResponse,
        RunPhase::PreparingToolCall,
        RunPhase::RunningTool {
            summary: "running tests".into(),
        },
        RunPhase::AwaitingApproval {
            prompt: "allow edit".into(),
        },
        RunPhase::Preparing {
            summary: "compacting".into(),
        },
    ];
    for phase in cases {
        let rendered = rendered_phase(phase);
        assert!(rendered.contains("GPT-5.6"), "{rendered}");
        assert!(!rendered.contains("Working"), "{rendered}");
    }
}

#[test]
fn custom_theme_keeps_active_work_out_of_the_footer() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_theme(crate::tui::theme::test_theme_from_source(
        SURFACE_TEST_THEME,
    ));
    let now = Instant::now();
    {
        let mut state = shell.state.borrow_mut();
        let id = state.run.begin_at("relay", now).unwrap();
        state.run.set_phase_at(id, RunPhase::Thinking, now);
    }
    let rendered =
        render_shell_at(&shell.state.borrow(), 80, now + Duration::from_millis(600)).join("\n");
    assert!(!rendered.contains("Working"), "{rendered}");
    assert!(!rendered.contains("0.6s"), "{rendered}");
}

#[test]
fn default_footer_accumulates_work_but_never_shows_the_stopwatch() {
    let shell = InteractiveShell::test_shell();
    let now = Instant::now();
    {
        let mut state = shell.state.borrow_mut();
        let first = state
            .run
            .begin_at("relay", now - Duration::from_secs(4))
            .unwrap();
        let outcome = state.run.interrupt_at(first, now).unwrap();
        InteractiveShell::append_outcome(&mut state, outcome);
        assert_eq!(state.session_work_elapsed, Duration::from_secs(4));

        let second = state.run.begin_at("relay", now).unwrap();
        state.run.set_phase_at(second, RunPhase::Thinking, now);
    }

    let active = strip_terminal_sequences(
        &crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            80,
            now + Duration::from_secs(2),
        )
        .join("\n"),
    );
    assert!(!active.contains("6.0s"), "{active}");

    {
        let mut state = shell.state.borrow_mut();
        let second = state.run.current_id().unwrap();
        let outcome = state
            .run
            .interrupt_at(second, now + Duration::from_secs(2))
            .unwrap();
        InteractiveShell::append_outcome(&mut state, outcome);
        assert_eq!(state.session_work_elapsed, Duration::from_secs(6));
    }
    let idle_later = strip_terminal_sequences(
        &crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            80,
            now + Duration::from_secs(32),
        )
        .join("\n"),
    );
    assert!(!idle_later.contains("6.0s"), "{idle_later}");
    assert!(!idle_later.contains("36.0s"), "{idle_later}");
}

#[test]
fn ctrl_o_keeps_width_cache_and_invalidates_only_disclosure_blocks() {
    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for index in 0..256 {
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized(format!("stable answer {index}")),
            )));
        }
        state.push_block(TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::finalized_reasoning("expand me".into()),
        )));
        let _ = state.rendered_transcript(100);
        assert_eq!(state.transcript_cache.borrow().width, Some(100));
    }

    shell.toggle_disclosure();
    let state = shell.state.borrow();
    let cache = state.transcript_cache.borrow();
    assert_eq!(cache.width, Some(100));
    assert_eq!(cache.dirty_blocks, [256]);
}

#[test]
fn tool_output_uses_one_compact_nested_elbow() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.rich_renderer();
    let args = serde_json::json!({"command": "printf hello"});
    let block = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("aligned-tool-output".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        "exit=0 duration=0.2s\nstdout:\nhello\ncomplete_stdout=true".into(),
        true,
        false,
        None,
        None,
    )));
    let lines = render_block(None, &block, &theme, &renderer, &renderer, 80, false)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let output = lines
        .iter()
        .find(|line| line.contains("└ hello"))
        .expect("tool output should render");
    let command = lines
        .iter()
        .find(|line| line.contains("Bash  printf hello"))
        .expect("tool input should render");
    let label_column = command
        .find("Bash")
        .map(|index| visible_width(&command[..index]))
        .expect("tool label should render");
    let elbow_column = output
        .find('└')
        .map(|index| visible_width(&output[..index]))
        .expect("tool output elbow should render");
    let output_column = output
        .find("hello")
        .map(|index| visible_width(&output[..index]))
        .expect("tool output value should render");
    assert_eq!(
        label_column, 2,
        "tool labels belong on the primary text column"
    );
    assert_eq!(elbow_column, label_column, "{lines:?}");
    assert_eq!(output_column, elbow_column + 2, "{lines:?}");
    assert_eq!(
        lines.iter().filter(|line| line.contains('└')).count(),
        1,
        "one tool output group needs exactly one elbow: {lines:?}"
    );
}

#[test]
fn transcript_events_prompt_and_composer_share_one_grid() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.rich_renderer();
    let prompt = TranscriptBlock::User {
        text: "prompt".into(),
        model_lab: Some(ModelLab::OpenAi),
        prompt_color: Some("#123456".into()),
        persisted: true,
    };
    let assistant =
        TranscriptBlock::Assistant(Box::new(AssistantBlock::finalized("answer".into())));
    let mut working =
        AssistantBlock::streaming_reasoning("").with_model_lab(Some(ModelLab::OpenAi));
    working.reasoning_heading = Some("Working".into());
    working.show_reasoning_hint = false;
    let working = TranscriptBlock::Reasoning(Box::new(working));
    let args = serde_json::json!({"command": "printf hello"});
    let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("shared-grid".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        "exit=0 duration=0.2s\nstdout:\nhello\ncomplete_stdout=true".into(),
        true,
        false,
        None,
        None,
    )));

    let rendered_row = |block: &TranscriptBlock, needle: &str| {
        render_block(None, block, &theme, &renderer, &renderer, 80, false)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("missing {needle:?} transcript row"))
    };
    let prompt_row = rendered_row(&prompt, "prompt");
    let assistant_row = rendered_row(&assistant, "answer");
    let working_row = rendered_row(&working, "Working");
    let tool_row = rendered_row(&tool, "Bash");
    let shell = InteractiveShell::test_shell();
    shell.state.borrow_mut().editor = "draft".into();
    let composer_row = plain_composer_surface(&shell, 80, Instant::now())
        .into_iter()
        .find(|line| line.contains("draft"))
        .expect("composer draft row");
    let column = |line: &str, needle: &str| {
        let byte = line
            .find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}: {line:?}"));
        visible_width(&line[..byte])
    };

    let marker_column = column(&prompt_row, "›");
    assert_eq!(marker_column, 0, "prompt marker must own column zero");
    assert_eq!(
        column(&composer_row, "›"),
        marker_column,
        "{composer_row:?}"
    );
    assert_eq!(
        column(&assistant_row, "•"),
        marker_column,
        "{assistant_row:?}"
    );
    assert_eq!(column(&working_row, "•"), marker_column, "{working_row:?}");
    assert_eq!(column(&tool_row, "•"), marker_column, "{tool_row:?}");

    let text_column = column(&prompt_row, "prompt");
    assert_eq!(text_column, 2, "primary text must begin at column two");
    assert_eq!(
        column(&composer_row, "draft"),
        text_column,
        "{composer_row:?}"
    );
    assert_eq!(
        column(&assistant_row, "answer"),
        text_column,
        "{assistant_row:?}"
    );
    assert_eq!(
        column(&working_row, "Working"),
        text_column,
        "{working_row:?}"
    );
    assert_eq!(column(&tool_row, "Bash"), text_column, "{tool_row:?}");
}

#[test]
fn tool_rendering_shows_concise_failures_but_hides_raw_evidence() {
    use ygg_agent::{ToolError, ToolOutput};
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    let id = ToolCallId("provider-call-secret".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "bash".into(),
            args: serde_json::json!({"command": "cargo test --workspace", "timeout_ms": 1000}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id,
            result: Ok(ToolOutput::new(
                "exit=1 duration=0.2s\nstderr: FAILED 76 passed",
            )),
            duration: Duration::from_millis(200),
        },
    );
    let plain = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 80).join("\n"));
    assert!(plain.contains("Bash  cargo test --workspace"), "{plain:?}");
    assert!(!plain.contains("provider-call-secret"), "{plain:?}");
    assert!(!plain.contains("exit=1"), "{plain:?}");
    assert!(!plain.contains("duration=0.2s"), "{plain:?}");
    assert!(!plain.contains("76 passed"), "{plain:?}");
    assert!(plain.contains("command exited 1"), "{plain:?}");
    let stale = ToolCallId("stale-edit-id".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: stale.clone(),
            name: "edit".into(),
            args: serde_json::json!({"path":"src/lib.rs"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: stale,
            result: Err(ToolError::new(
                "error stale_file\nexpected hash=aaa actual=bbb\nThe file changed",
            )),
            duration: Duration::from_millis(10),
        },
    );
    let plain = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 120).join("\n"));
    // The finished edit keeps its intent; wall time is reserved for bash.
    assert!(plain.contains("Edit"), "{plain:?}");
    assert!(plain.contains("src/lib.rs"), "{plain:?}");
    assert!(!plain.contains("· 10 ms"), "{plain:?}");
    assert!(plain.contains("The file changed"), "{plain:?}");
    assert!(!plain.contains("hash=aaa"), "{plain:?}");
    assert!(!plain.contains("actual=bbb"), "{plain:?}");
}

#[test]
fn successful_media_reads_render_payload_free_capability_indicators() {
    use ygg_agent::{ToolError, ToolOutput};

    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    let image_id = ToolCallId("image-read".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: image_id.clone(),
            name: "read".into(),
            args: serde_json::json!({"path": "capture.png"}),
        },
    );
    let image_output = ToolOutput::new("image summary")
        .with_media(ygg_ai::Media::image_bytes(
            bytes::Bytes::from_static(b"\x89PNG\r\n\x1a\n"),
            mime::IMAGE_PNG,
        ))
        .without_media_payloads();
    assert!(image_output.media().is_empty());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: image_id,
            result: Ok(image_output),
            duration: Duration::from_millis(10),
        },
    );

    for (id, path, result) in [
        (
            "text-read",
            "notes.txt",
            Ok(ToolOutput::new("plain text summary")),
        ),
        (
            "failed-read",
            "broken.png",
            Err(ToolError::new("unsupported image encoding")),
        ),
    ] {
        let id = ToolCallId(id.into());
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: "read".into(),
                args: serde_json::json!({"path": path}),
            },
        );
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id,
                result,
                duration: Duration::from_millis(10),
            },
        );
    }

    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(100)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    let image = rendered
        .iter()
        .find(|line| line.contains("capture.png"))
        .expect("successful image read row");
    assert!(image.contains('◉'), "{rendered:?}");
    assert!(!image.contains('♪'), "{rendered:?}");
    for path in ["notes.txt", "broken.png"] {
        let line = rendered
            .iter()
            .find(|line| line.contains(path))
            .expect("non-media read row");
        assert!(
            !line.contains('◉') && !line.contains('♪'),
            "unsupported or failed reads must not imply media ingestion: {rendered:?}"
        );
    }
}

#[test]
fn responsive_header_drops_metadata_instead_of_truncating_every_field() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("relay", "gpt-5.6", "high");
    let wide = responsive_identity(&shell.state.borrow(), 120);
    assert!(wide.contains("relay / "));
    assert!(wide.contains("GPT-5.6"));
    assert!(wide.contains("high"));

    shell.set_identity(
        "custom-openai",
        "custom/Intel/Qwen3.6-27B-int4-AutoRound",
        "high",
    );
    let custom = strip_terminal_sequences(&responsive_identity(&shell.state.borrow(), 120));
    assert!(custom.contains("custom-openai / Qwen3.6 27B"), "{custom}");
    assert!(!custom.contains("custom/Intel"), "{custom}");

    shell.set_identity(
        "a-very-long-gateway-provider-name",
        "a-very-long-model-name-that-does-not-fit",
        "high",
    );
    let narrow = responsive_identity(&shell.state.borrow(), 40);
    assert!(visible_width(&narrow) <= 40);
    assert!(!narrow.contains("..."));
    assert!(!narrow.contains('…'));
    assert!(narrow.contains("ygg"));
}

#[test]
fn ascii_plain_and_unicode_no_colour_keep_the_same_structure() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

    let ascii_theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
        false,
        false,
        ColorDepth::None,
    ));
    let mut ascii = InteractiveShell::test_shell_with_theme(ascii_theme);
    ascii.set_identity("relay", "gpt-5.6", "off");
    ascii.on_prompt_submitted("fix it");
    {
        let mut state = ascii.state.borrow_mut();
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("# Result\n\n- done".into()),
        )));
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("id".into()),
            "edit".into(),
            "{}".into(),
            summarize_tool("edit", &serde_json::json!({"path":"src/lib.rs"})),
            String::new(),
            true,
            false,
            None,
            None,
        ))));
        state.push_block(TranscriptBlock::Outcome(OutcomeBlock::new(
            RunOutcome::Completed {
                elapsed: Duration::from_secs(1),
                summary: crate::presentation::RunSummary {
                    files_changed: 1,
                    tool_calls: 1,
                    warnings: 0,
                },
            },
            None,
        )));
    }
    ascii.set_size(40, 20);
    let ascii = render_shell(&ascii.state.borrow(), 40)
        .join("\n")
        .replace(CURSOR_MARKER, "");
    assert!(ascii.is_ascii(), "{ascii:?}");
    assert!(!ascii.contains('\x1b'));
    assert!(ascii.contains("> fix it"));
    assert!(ascii.contains("Result"));
    assert!(ascii.contains("- done"));
    assert!(ascii.contains("Edit"));
    assert!(ascii.contains("lib.rs"));
    assert!(ascii.contains("completed - 1.0s"));
    assert!(!ascii.contains("ok completed"));

    let unicode_theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
        true,
        true,
        ColorDepth::None,
    ));
    let mut unicode = InteractiveShell::test_shell_with_theme(unicode_theme);
    unicode.on_prompt_submitted("fix it");
    let unicode = render_shell(&unicode.state.borrow(), 60)
        .join("\n")
        .replace(CURSOR_MARKER, "");
    assert!(unicode.contains("› fix it"));
    assert!(!unicode.contains('\x1b'));
}

#[test]
fn narrow_tool_paths_use_basenames_and_wide_paths_remain_inspectable() {
    let theme = crate::tui::theme::test_theme();
    let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("id".into()),
        "edit".into(),
        serde_json::json!({"path":"crates/ygg-agent/src/session.rs"}).to_string(),
        summarize_tool(
            "edit",
            &serde_json::json!({"path":"crates/ygg-agent/src/session.rs"}),
        ),
        String::new(),
        true,
        false,
        None,
        None,
    )));
    let renderer = theme.rich_renderer();
    let narrow = strip_terminal_sequences(
        &render_block(None, &panel, &theme, &renderer, &renderer, 40, false).join("\n"),
    );
    let wide = strip_terminal_sequences(
        &render_block(None, &panel, &theme, &renderer, &renderer, 120, false).join("\n"),
    );
    assert!(narrow.contains("Edit  session.rs"));
    assert!(!narrow.contains("crates/ygg-agent"));
    assert!(wide.contains("Edit  crates/ygg-agent/src/session.rs"));
}

#[test]
fn edit_status_prefix_does_not_hide_the_unified_diff() {
    let theme = crate::tui::theme::test_theme();
    let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("edit-diff".into()),
        "edit".into(),
        "{}".into(),
        summarize_tool("edit", &serde_json::json!({"path":"src/lib.rs"})),
        concat!(
            "ok modified=1\n",
            "src/lib.rs  +1 -1 hash=abc\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,1 +1,1 @@\n",
            "-old\n",
            "+new\n"
        )
        .into(),
        true,
        false,
        None,
        None,
    )));
    let renderer = theme.rich_renderer();
    let rendered = render_block(None, &panel, &theme, &renderer, &renderer, 100, false).join("\n");
    let plain = strip_terminal_sequences(&rendered);
    assert!(plain.contains("-old"), "{rendered}");
    assert!(plain.contains("+new"), "{rendered}");
    assert!(!plain.contains("hash=abc"), "{rendered}");
}

#[test]
fn layered_write_diff_reports_one_truthful_remainder_per_disclosure_mode() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.rich_renderer();
    let args = serde_json::json!({"path":"large.txt"});
    let preview = (1..=10)
        .map(|line| format!("+line-{line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let block = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("layered-write-diff".into()),
        "write".into(),
        args.to_string(),
        summarize_tool("write", &args),
        format!(
            "ok\nlarge.txt  created hash=abc\n--- /dev/null\n+++ b/large.txt\n@@ -0,0 +1,191 @@\n{preview}\n… 181 more lines\n"
        ),
        true,
        false,
        None,
        None,
    )));

    let collapsed = strip_terminal_sequences(
        &render_block(None, &block, &theme, &renderer, &renderer, 100, false).join("\n"),
    );
    assert!(collapsed.contains("184 lines hidden"), "{collapsed:?}");
    assert!(!collapsed.contains("181 more lines"), "{collapsed:?}");
    assert_eq!(
        collapsed.matches("lines hidden").count(),
        1,
        "{collapsed:?}"
    );

    let expanded = strip_terminal_sequences(
        &render_block(None, &block, &theme, &renderer, &renderer, 100, true).join("\n"),
    );
    assert!(expanded.contains("@@ -0,0 +1,191 @@"), "{expanded:?}");
    assert!(expanded.contains("+line-10"), "{expanded:?}");
    assert_eq!(
        expanded.matches("181 more lines").count(),
        1,
        "{expanded:?}"
    );
    assert!(!expanded.contains("lines hidden"), "{expanded:?}");
}

#[test]
fn tool_values_follow_labels_without_a_wide_dead_column() {
    for (label, expected_column) in [
        ("Read", 6),
        ("Bash", 6),
        ("Write", 7),
        ("Explored", 10),
        ("Delegated", 11),
    ] {
        assert_eq!(tool_value_indent_width(label), expected_column, "{label}");
        assert_eq!(
            visible_width(&tool_value_indent(label)),
            expected_column,
            "{label}"
        );
        assert_eq!(
            expected_column.saturating_sub(visible_width(label)),
            2,
            "{label}"
        );
    }
    assert!(visible_width(&tool_grid_label("an_extremely_long_tool_name")) <= 18);
}

#[test]
fn recognized_assistant_diffs_use_the_pretty_diff_renderer() {
    let theme = crate::tui::theme::test_theme();
    let assistant = AssistantBlock::finalized(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new".into(),
        );
    let rendered_lines = assistant.render(&theme.rich_renderer(), &theme, 80);
    let rendered = rendered_lines.join("\n");
    let plain = strip_terminal_sequences(&rendered);
    assert!(plain.contains("@@ -1 +1 @@"));
    assert!(plain.contains("-old"));
    assert!(plain.contains("+new"));
    assert!(!plain.contains("```"));

    let terminal = emulate_rows(&rendered_lines, 80);
    let (removed_row, removed_col) =
        find_ascii_cell(terminal.screen(), "-old").expect("rendered removal");
    let (added_row, added_col) =
        find_ascii_cell(terminal.screen(), "+new").expect("rendered addition");
    let removed = terminal
        .screen()
        .cell(removed_row, removed_col)
        .expect("removal cell")
        .fgcolor();
    let added = terminal
        .screen()
        .cell(added_row, added_col)
        .expect("addition cell")
        .fgcolor();
    assert_ne!(removed, vt100::Color::Default);
    assert_ne!(added, vt100::Color::Default);
    assert_ne!(removed, added);
}

#[test]
fn fenced_diff_inside_markdown_does_not_hijack_the_whole_answer() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

    let markdown = concat!(
        "## Why this changed\n\n",
        "The cache remains authoritative.\n\n",
        "```diff\n",
        "diff --git a/src/lib.rs b/src/lib.rs\n",
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new\n",
        "```\n",
    );
    assert!(!looks_like_diff(markdown));
    assert!(looks_like_diff(
        "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new"
    ));
    assert!(looks_like_diff(
        "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+fn main() {}"
    ));

    let theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
        true,
        true,
        ColorDepth::None,
    ));
    let rendered = AssistantBlock::finalized(markdown.to_owned())
        .render(&theme.rich_renderer(), &theme, 60)
        .join("\n");
    assert!(rendered.contains("Why this changed"), "{rendered}");
    assert!(
        rendered.contains("cache remains authoritative"),
        "{rendered}"
    );
    assert!(rendered.contains("-old"), "{rendered}");
    assert!(!rendered.contains("```"), "{rendered}");
}

#[test]
fn assistant_markdown_uses_full_rich_pipeline_without_rewriting_source() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

    let theme = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
        true,
        true,
        ColorDepth::None,
    ));
    let source = concat!(
        "# Result\n\n",
        "> Safe presentation projection\n\n",
        "- [x] CommonMark\n",
        "- [ ] cached source\n\n",
        "| Feature | State |\n| --- | --- |\n| tables | on |\n\n",
        "See [the docs](https://example.com/ygg).\n\n",
        "```rust\nlet complete_value = 12345678901234567890;\n```",
    );
    let assistant = AssistantBlock::finalized(source.to_owned());
    let renderer = theme.rich_renderer();
    let rendered = assistant.render(&renderer, &theme, 32).join("\n");

    // Rendering is a view over the exact provider/session payload. It may
    // add terminal structure, but it never normalizes the cached Markdown.
    assert_eq!(assistant.text, source);
    assert_eq!(assistant.markdown.raw_text(), source);
    assert_eq!(
        renderer.options().code_overflow,
        sexy_tui_rs::CodeOverflow::Wrap
    );
    assert!(renderer.options().syntax_highlighting);
    assert!(renderer.options().tables);
    assert!(!renderer.options().code_borders);

    assert!(rendered.contains("Result"), "{rendered}");
    assert!(
        rendered.contains("Safe presentation projection"),
        "{rendered}"
    );
    assert!(rendered.contains("CommonMark"), "{rendered}");
    assert!(rendered.contains("Feature"), "{rendered}");
    assert!(rendered.contains("tables"), "{rendered}");
    assert!(rendered.contains("https://example.com/ygg"), "{rendered}");
    // The end of a long code row remains visible because transcript code
    // wraps instead of being irretrievably clipped.
    assert!(rendered.contains("67890"), "{rendered}");
    assert!(!rendered.contains("```"), "{rendered}");
    assert!(!rendered.contains("\x1b[48;"), "{rendered:?}");
}

#[test]
fn active_run_starts_with_working_until_reasoning_is_observed() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    shell.begin_run("codex");
    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert_eq!(rendered.len(), 1, "{rendered:?}");
    assert!(
        rendered[0].starts_with("• Working (0s • esc to interrupt)"),
        "{rendered:?}"
    );
}

#[test]
fn max_and_ultra_working_rainbow_fades_for_two_seconds_only() {
    assert_eq!(
        status_rainbow_strength_at(Some("max"), Some(Duration::ZERO)),
        100
    );
    assert_eq!(
        status_rainbow_strength_at(Some("ultra"), Some(Duration::from_millis(500))),
        75
    );
    assert_eq!(
        status_rainbow_strength_at(Some("max"), Some(Duration::from_secs(1))),
        50
    );
    assert_eq!(
        status_rainbow_strength_at(Some("max"), Some(Duration::from_millis(1_500))),
        25
    );
    assert_eq!(
        status_rainbow_strength_at(Some("max"), Some(Duration::from_secs(2))),
        0
    );
    assert_eq!(
        status_rainbow_strength_at(Some("high"), Some(Duration::ZERO)),
        0
    );
    assert_eq!(status_rainbow_strength_at(Some("ultra"), None), 0);
}

#[test]
fn collapsed_activity_shimmer_repaints_only_the_status_style() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    let run_id = shell.begin_run("codex");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "private trace".into(),
        },
    );
    let raw = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .find(|line| strip_terminal_sequences(line).contains("Thinking"))
            .cloned()
            .expect("reasoning status row")
    };
    let before = raw(&shell);
    assert!(
        strip_terminal_sequences(&before).starts_with("• Thinking ("),
        "{before:?}"
    );
    let marker_prefix = |line: &str| {
        let marker = line.find('•').expect("reasoning margin marker");
        line[..marker + '•'.len_utf8()].to_owned()
    };
    {
        let mut state = shell.state.borrow_mut();
        assert!(!event_dot_animating(&state));
        assert!(state.has_active_status_shimmer());
        assert_eq!(state.active_event_blocks, vec![0]);
        state.advance_status_shimmer();
    }
    let after = raw(&shell);
    assert!(
        strip_terminal_sequences(&after).starts_with("• Thinking ("),
        "{after:?}"
    );
    assert_ne!(after, before, "the shimmer style must advance");
    assert_ne!(
        marker_prefix(&after),
        marker_prefix(&before),
        "the margin dot must share the status shimmer"
    );
    assert!(
        !after.contains("\x1b[48;"),
        "status shimmer must stay foreground-only"
    );
}

#[test]
fn working_activity_shimmer_repaints_its_margin_dot() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    shell.begin_run("codex");

    let raw = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .find(|line| strip_terminal_sequences(line).contains("Working"))
            .cloned()
            .expect("working status row")
    };
    let marker_prefix = |line: &str| {
        let marker = line.find('•').expect("working margin marker");
        line[..marker + '•'.len_utf8()].to_owned()
    };
    let before = raw(&shell);
    {
        let mut state = shell.state.borrow_mut();
        assert!(state.has_active_status_shimmer());
        state.advance_status_shimmer();
    }
    let after = raw(&shell);

    assert_ne!(marker_prefix(&after), marker_prefix(&before));
    assert!(
        !after.contains("\x1b[48;"),
        "status shimmer must stay foreground-only"
    );
}

#[test]
fn collapsed_reasoning_uses_a_margin_dot_without_an_expanded_content_bullet() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.rich_renderer();
    let args = serde_json::json!({"path":"README.md"});
    let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("read".into()),
        "read".into(),
        args.to_string(),
        summarize_tool("read", &args),
        String::new(),
        true,
        false,
        None,
        None,
    )));
    let reasoning = TranscriptBlock::Reasoning(Box::new(
        AssistantBlock::streaming_reasoning("private detail")
            .with_model_lab(Some(ModelLab::OpenAi)),
    ));
    let plain = |lines: Vec<String>| {
        lines
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
    };
    let tool_lines = plain(render_block(
        None, &tool, &theme, &renderer, &renderer, 80, false,
    ));
    let reasoning_lines = plain(render_block(
        Some(&tool),
        &reasoning,
        &theme,
        &renderer,
        &renderer,
        80,
        false,
    ));
    let expanded_reasoning_lines = plain(render_block(
        Some(&tool),
        &reasoning,
        &theme,
        &renderer,
        &renderer,
        80,
        true,
    ));
    let tool_line = tool_lines
        .iter()
        .find(|line| line.contains("Read"))
        .expect("read row");
    let reasoning_line = reasoning_lines
        .iter()
        .find(|line| line.contains("Thinking"))
        .expect("reasoning row");
    let disclosure_line = reasoning_lines
        .iter()
        .find(|line| line.contains("(ctrl+o to expand)"))
        .expect("reasoning disclosure row");
    let visual_column = |line: &str, needle: &str| {
        line.find(needle)
            .map(|offset| visible_width(&line[..offset]))
    };
    assert!(tool_line.starts_with("• "), "{tool_line:?}");
    assert!(
        reasoning_line.starts_with("• "),
        "collapsed reasoning must carry the blinking event dot: {reasoning_line:?}"
    );
    let expanded_reasoning_line = expanded_reasoning_lines
        .iter()
        .find(|line| line.contains("private detail"))
        .expect("expanded reasoning row");
    assert!(
        expanded_reasoning_line.starts_with("  private detail"),
        "expanded reasoning must retain its gutter without a dot or content bullet: {expanded_reasoning_line:?}"
    );
    assert_eq!(
        visual_column(tool_line, "Read"),
        visual_column(reasoning_line, "Thinking"),
        "reasoning labels must align with tool labels: {tool_line:?} vs {reasoning_line:?}"
    );
    assert_eq!(
        visual_column(reasoning_line, "Thinking"),
        visual_column(disclosure_line, "└"),
        "the disclosure elbow must descend from the first label character: {reasoning_line:?} vs {disclosure_line:?}"
    );
}

#[test]
fn reasoning_heading_moves_below_the_fixed_thinking_header() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "## Verifying reproducibility of evidence package\n\n".into(),
        },
    );

    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert_eq!(
        rendered,
        vec![
            "• Thinking (0s • esc to interrupt)",
            "  └ Verifying reproducibility of evidence package (ctrl+o to expand)",
        ]
    );
}

#[test]
fn reasoning_off_run_uses_a_truthful_non_expandable_working_status() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "off");
    let run_id = shell.begin_run("codex");
    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert_eq!(rendered.len(), 1, "{rendered:?}");
    assert!(
        rendered[0].starts_with("• Working (0s • esc to interrupt)"),
        "{rendered:?}"
    );
    assert!(!rendered[0].contains("ctrl+o"), "{rendered:?}");

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "provider-private detail".into(),
        },
    );
    let promoted = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert_eq!(promoted.len(), 2, "{promoted:?}");
    assert!(promoted[0].contains("Thinking"), "{promoted:?}");
    assert!(promoted[1].contains("(ctrl+o to expand)"), "{promoted:?}");
    assert!(!promoted.join("\n").contains("provider-private detail"));
}

#[test]
fn empty_working_status_leaves_no_ghost_block_when_interrupted() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "off");
    let run_id = shell.begin_run("codex");

    shell.interrupt_run(run_id);

    let state = shell.state.borrow();
    assert!(
        state
            .transcript
            .iter()
            .all(|block| !matches!(block, TranscriptBlock::Reasoning(_))),
        "a display-only status must not become durable transcript history"
    );
    assert!(state.active_event_blocks.is_empty());
}

#[test]
fn public_text_stream_keeps_exactly_one_working_row_while_the_run_is_active() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    let run_id = shell.begin_run("codex");

    for text in ["Ready.", " Still running."] {
        shell.on_run_event(
            run_id,
            &AgentEvent::OutputDelta {
                channel: OutputChannel::Text,
                text: text.into(),
            },
        );
    }

    let state = shell.state.borrow();
    assert!(matches!(
        state.transcript.first(),
        Some(TranscriptBlock::Assistant(_))
    ));
    assert_eq!(state.transcript.len(), 2);
    assert!(state.active_text.is_some());
    assert!(state.active_reasoning.is_some());
    assert!(state.has_active_status_shimmer());
    let rendered = state
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert!(rendered
        .iter()
        .any(|line| line.contains("Ready. Still running.")));
    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.starts_with("• Working ("))
            .count(),
        1,
        "{rendered:?}"
    );
    assert!(rendered
        .last()
        .is_some_and(|line| line.starts_with("• Working (")));
}

#[test]
fn activity_lifecycle_is_working_thinking_streaming_working_then_settled() {
    use ygg_agent::{EntryId, FinishReason};
    use ygg_ai::{AssistantMessage, AssistantPart, ModelId, Protocol, StopReason};

    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    let rendered = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
    };
    assert!(rendered(&shell)
        .iter()
        .any(|line| line.starts_with("• Working (")));

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "real private trace".into(),
        },
    );
    let thinking = rendered(&shell);
    assert!(thinking.iter().any(|line| line.contains("Thinking")));
    assert!(!thinking.iter().any(|line| line.starts_with("• Working (")));

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "Answer".into(),
        },
    );
    let responding = rendered(&shell);
    assert!(responding.iter().any(|line| line.contains("Answer")));
    assert_eq!(
        responding
            .iter()
            .filter(|line| line.starts_with("• Working ("))
            .count(),
        1,
        "{responding:?}"
    );
    assert!(responding
        .last()
        .is_some_and(|line| line.starts_with("• Working (")));
    assert!(shell.state.borrow().has_active_status_shimmer());

    shell.on_run_event(
        run_id,
        &AgentEvent::TurnFinished {
            message: AssistantMessage {
                content: vec![AssistantPart::Text("Answer".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiResponses,
            },
            stop_reason: StopReason::EndTurn,
            turn_usage: Usage::default(),
            usage: Usage::default(),
            session_cost_microdollars: None,
            run_cost_microdollars: 0,
        },
    );
    let finalizing = rendered(&shell);
    assert_eq!(
        finalizing
            .iter()
            .filter(|line| line.starts_with("• Working ("))
            .count(),
        1,
        "a completed turn is not an authoritative run terminal: {finalizing:?}"
    );

    shell.on_run_event(
        run_id,
        &AgentEvent::RunFinished {
            head: EntryId("head".into()),
            reason: FinishReason::Completed,
        },
    );
    let settled = rendered(&shell);
    assert!(!settled.iter().any(|line| line.contains("Working")));
    assert!(shell.state.borrow().active_reasoning.is_none());
}

#[test]
fn removing_a_tail_status_preserves_an_older_semantic_selection() {
    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Notice("older transcript".into()));
        state.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                block: 0,
                offset: 0,
                trailing_affinity: false,
            },
            focus: TranscriptPosition {
                block: 0,
                offset: 5,
                trailing_affinity: false,
            },
        });
    }
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    let run_id = shell.begin_run("codex");

    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "Ready.".into(),
        },
    );

    let state = shell.state.borrow();
    let selection = state
        .transcript_selection
        .as_ref()
        .expect("older selection should survive removal of the empty tail status");
    assert_eq!(selection.anchor.block, 0);
    assert_eq!(selection.focus.block, 0);
}

#[test]
fn reasoning_status_reopens_after_tools_for_the_next_model_turn() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    shell.set_context_estimate(13_000, 128_000);
    let run_id = shell.begin_run("codex");
    let id = ToolCallId("tool-1".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "read".into(),
            args: serde_json::json!({"path": "README.md"}),
        },
    );
    assert!(shell.state.borrow().active_reasoning.is_none());
    assert!(shell
        .state
        .borrow()
        .transcript
        .iter()
        .all(|block| !matches!(block, TranscriptBlock::Reasoning(_))));
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id,
            result: Ok(ygg_agent::ToolOutput::new("x".repeat(4_000))),
            duration: Duration::from_millis(10),
        },
    );
    let state = shell.state.borrow();
    let index = state.active_reasoning.expect("next-turn reasoning status");
    let TranscriptBlock::Reasoning(reasoning) = &state.transcript[index] else {
        panic!("reasoning status expected")
    };
    assert!(reasoning.text.is_empty());
    assert!(!reasoning.finished);
    assert_eq!(state.run_context_estimate, Some((14_008, 128_000)));
    assert_eq!(state.context_estimate, Some((14_008, 128_000)));
}

#[test]
fn streamed_reasoning_shows_one_live_indicator_until_ctrl_o() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "first private sentinel".into(),
        },
    );
    let transcript = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
    };
    let initial = transcript(&shell);
    assert_eq!(initial.len(), 2, "{initial:?}");
    assert!(initial[0].contains("Thinking"), "{initial:?}");
    assert!(initial[1].contains("(ctrl+o to expand)"), "{initial:?}");
    assert!(!initial.join("\n").contains("first private sentinel"));

    let continuation = (0..128)
        .map(|index| format!("\nprivate reasoning row {index}"))
        .collect::<String>();
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: continuation.clone(),
        },
    );
    let after_stream = transcript(&shell);
    assert_eq!(
        after_stream, initial,
        "hidden deltas changed transcript geometry"
    );
    {
        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
            panic!("reasoning block expected");
        };
        assert_eq!(
            reasoning.text,
            format!("first private sentinel{continuation}")
        );
    }

    shell.toggle_disclosure();
    let expanded = transcript(&shell).join("\n");
    assert!(expanded.contains("first private sentinel"), "{expanded}");
    assert!(expanded.contains("private reasoning row 127"), "{expanded}");
    assert!(!expanded.contains("(ctrl+o to expand)"), "{expanded}");

    shell.toggle_disclosure();
    assert_eq!(transcript(&shell), initial);
}

#[test]
fn a_new_reasoning_event_retires_the_previous_ctrl_o_hint() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "first thought".into(),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "answer".into(),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "second thought".into(),
        },
    );
    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rendered.matches("(ctrl+o to expand)").count(),
        1,
        "{rendered}"
    );
    shell.toggle_disclosure();
    let expanded = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(expanded.contains("first thought"), "{expanded}");
    assert!(expanded.contains("second thought"), "{expanded}");
    assert!(!expanded.contains("(ctrl+o to expand)"), "{expanded}");
}

#[test]
fn reasoning_heading_tracks_only_explicit_markdown_headings() {
    let mut reasoning = AssistantBlock::streaming_reasoning(
        "Body sentence must stay private.\n\n## Plan `carefully`\n\nMore private detail.",
    );
    assert_eq!(
        reasoning.reasoning_heading.as_deref(),
        Some("Plan carefully")
    );

    reasoning.append_reasoning("\n\nThis is still ordinary body text.");
    assert_eq!(
        reasoning.reasoning_heading.as_deref(),
        Some("Plan carefully")
    );

    reasoning.append_reasoning("\n\n**Verify results**");
    assert_eq!(
        reasoning.reasoning_heading.as_deref(),
        Some("Verify results")
    );

    reasoning.append_reasoning("\n\nPrefix **bold body text** suffix.");
    assert_eq!(
        reasoning.reasoning_heading.as_deref(),
        Some("Verify results")
    );
}

#[test]
fn reasoning_heading_handles_adjacent_bold_sections_split_across_deltas() {
    let mut reasoning = AssistantBlock::streaming_reasoning("**Plan**");
    assert_eq!(reasoning.reasoning_heading.as_deref(), Some("Plan"));

    reasoning.append_reasoning("**");
    assert_eq!(reasoning.reasoning_heading.as_deref(), Some("Plan"));
    reasoning.append_reasoning("Verify**");
    assert_eq!(reasoning.reasoning_heading.as_deref(), Some("Verify"));
    assert_eq!(reasoning.text, "**Plan****Verify**");
    assert!(!reasoning.markdown.raw_text().contains("****"));
}

#[test]
fn reasoning_heading_is_terminal_sanitized() {
    let heading = reasoning_heading_from_block(&Block::Heading {
        level: 2,
        content: vec![Inline::Text("Safe\x1b[31m heading\x07".into())],
    })
    .expect("heading");
    assert_eq!(heading, "Safe heading␇");
    assert!(!heading.contains('\x1b'));
}

#[test]
fn hydrated_reasoning_is_retained_but_collapsed_until_ctrl_o() {
    use ygg_ai::{AssistantMessage, AssistantPart, Message, ModelId, Protocol, ReasoningPart};

    let directory = tempfile::tempdir().unwrap();
    let mut session = Session::create(directory.path().join("session.jsonl")).unwrap();
    let source = "durable private thought\nwith a second line";
    session
        .append(EntryValue::Message(Message::Assistant(AssistantMessage {
            content: vec![AssistantPart::Reasoning(ReasoningPart {
                text: Some(source.into()),
                state: None,
            })],
            model: ModelId("test".into()),
            protocol: Protocol::OpenAiResponses,
        })))
        .unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.hydrate(&session).unwrap();
    let render = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
    };
    let collapsed = render(&shell);
    assert_eq!(collapsed.len(), 0, "{collapsed:?}");
    assert!(!collapsed.join("\n").contains(source));
    let state = shell.state.borrow();
    let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
        panic!("hydrated reasoning block expected");
    };
    assert_eq!(reasoning.text, source);
    assert!(reasoning.finished);
    drop(state);

    shell.toggle_disclosure();
    let expanded = render(&shell).join("\n");
    assert!(expanded.contains("durable private thought"), "{expanded}");
    assert!(expanded.contains("with a second line"), "{expanded}");

    shell.toggle_disclosure();
    assert_eq!(render(&shell), collapsed);
}

#[test]
fn completed_reasoning_uses_rich_markdown_without_raw_delimiters() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "**Planning validation**".into(),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "**Inspecting `render.rs`**".into(),
        },
    );
    let collapsed = render_shell(&shell.state.borrow(), 80).join("\n");
    let collapsed_plain = strip_terminal_sequences(&collapsed);
    assert!(
        collapsed_plain.contains("Inspecting render.rs"),
        "{collapsed_plain}"
    );
    assert!(
        !collapsed_plain.contains("Planning validation"),
        "{collapsed_plain}"
    );
    {
        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
            panic!("first block must be reasoning Markdown");
        };
        assert!(reasoning.text.contains("****"));
        assert!(!reasoning.markdown.raw_text().contains("****"));
    }
    shell.toggle_disclosure();
    let live = render_shell(&shell.state.borrow(), 80).join("\n");
    assert!(live.contains("Planning validation"), "{live}");
    assert!(live.contains("Inspecting"), "{live}");
    assert!(!live.contains("**"), "{live}");
    assert!(!live.contains("`render.rs`"), "{live}");

    // A tool boundary finalizes both assistant and reasoning streams.
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: ToolCallId("read-1".into()),
            name: "read".into(),
            args: serde_json::json!({"path":"render.rs"}),
        },
    );

    let state = shell.state.borrow();
    let TranscriptBlock::Reasoning(reasoning) = &state.transcript[0] else {
        panic!("first block must be reasoning Markdown");
    };
    assert!(reasoning.markdown.is_finished());
    let rendered = render_shell(&state, 80).join("\n");
    assert!(rendered.contains("Planning validation"), "{rendered}");
    assert!(rendered.contains("Inspecting"), "{rendered}");
    assert!(!rendered.contains("**"), "{rendered}");
    assert!(!rendered.contains("`render.rs`"), "{rendered}");
}

#[test]
fn reasoning_is_subdued_without_losing_inline_code_colour() {
    let theme = crate::tui::theme::test_theme();
    let response = AssistantBlock::finalized("Answer with `Session`".into())
        .render(&theme.rich_renderer(), &theme, 80)
        .join("\n");
    let reasoning = AssistantBlock::finalized_reasoning("Thinking about `Session`".into())
        .render(&theme.reasoning_renderer(), &theme, 80)
        .join("\n");
    let prompt = render_block(
        None,
        &TranscriptBlock::User {
            text: "prompt".into(),
            model_lab: None,
            prompt_color: None,
            persisted: true,
        },
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        80,
        false,
    )
    .into_iter()
    .next()
    .expect("prompt line");
    let reasoning_block = render_block(
        None,
        &TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::streaming_reasoning("Thinking about `Session`")
                .with_model_lab(Some(ModelLab::Unknown)),
        )),
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        80,
        true,
    )
    .into_iter()
    .next()
    .expect("thinking line");
    let reasoning_code = AssistantBlock::finalized_reasoning(
        "Thinking before code:\n\n```rust\nlet answer = 42;\n```".into(),
    )
    .render(&theme.reasoning_renderer(), &theme, 80)
    .join("\n");
    let linked_reasoning =
        AssistantBlock::finalized_reasoning("See [the docs](https://example.com)".into())
            .render(&theme.reasoning_renderer(), &theme, 80)
            .join("\n");
    let conservative_theme =
        crate::tui::theme::test_theme_with(crate::tui::terminal::TerminalCapabilities::test(
            true,
            true,
            crate::tui::terminal::ColorDepth::Ansi16,
        ));
    let conservative_reasoning = AssistantBlock::finalized_reasoning("thinking".into())
        .render(
            &conservative_theme.reasoning_renderer(),
            &conservative_theme,
            80,
        )
        .join("\n");
    let code_line = reasoning_code
        .lines()
        .find(|line| line.contains("answer"))
        .expect("thinking code line");

    assert!(
        response.starts_with("Answer"),
        "responses stay flush: {response:?}"
    );
    assert!(
        strip_terminal_sequences(&prompt).starts_with("› prompt"),
        "prompts should share the presentation inset: {prompt:?}"
    );
    assert!(
        !prompt.contains("\x1b[48;"),
        "prompt identity should use only a restrained foreground marker"
    );
    assert!(
        prompt.contains("\x1b[38;2;"),
        "prompt needs readable text colour"
    );
    assert!(response.contains("Session"));
    assert!(
        response.contains("\x1b[38;2;"),
        "inline code should be coloured"
    );
    assert!(
        strip_terminal_sequences(&reasoning_block).starts_with("  Thinking"),
        "expanded reasoning keeps the transcript inset without a status dot or content bullet: {reasoning_block:?}"
    );
    assert!(reasoning.contains("Session"));
    assert!(
        reasoning.contains("\x1b[38;2;"),
        "reasoning should use a muted foreground"
    );
    assert!(
        !reasoning.contains("\x1b[3m"),
        "reasoning should stay upright"
    );
    assert!(
        !reasoning.contains("\x1b[2m"),
        "reasoning must not use SGR faint"
    );
    assert!(
        !code_line.contains("\x1b[3m"),
        "thinking code blocks must stay upright"
    );
    assert!(
        linked_reasoning.contains("\x1b]8;;https://example.com"),
        "thinking links retain native hyperlink support"
    );
    assert!(
        !conservative_reasoning.contains("\x1b[4m"),
        "unsupported italics must not degrade into underlines"
    );
    assert!(
        !response.contains("\x1b[2m"),
        "ordinary response prose must not inherit reasoning dim"
    );
}

#[test]
fn multiline_prompt_marks_only_the_first_row() {
    let theme = crate::tui::theme::test_theme();
    let block = TranscriptBlock::User {
        text: "help me fix a bug in ygg when the prompt wraps across several rows".into(),
        model_lab: Some(ModelLab::OpenAi),
        prompt_color: None,
        persisted: true,
    };
    let rendered = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        24,
        false,
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>();

    assert!(rendered.len() > 1, "prompt should wrap: {rendered:?}");
    assert!(rendered[0].starts_with("› "), "{rendered:?}");
    assert!(
        rendered[1..]
            .iter()
            .all(|line| line.starts_with("  ") && !line.contains('│') && !line.contains('|')),
        "continuation rows should be indented without rails: {rendered:?}"
    );
}

#[test]
fn prompt_card_keeps_exact_persisted_provenance_across_theme_changes() {
    let mut first_theme = crate::tui::theme::test_theme();
    crate::tui::theme::apply_model_lab(&mut first_theme, ModelLab::OpenAi);
    let block = TranscriptBlock::User {
        text: "safe\u{1b}[31m prompt".into(),
        model_lab: Some(ModelLab::OpenAi),
        prompt_color: Some("#123456".into()),
        persisted: true,
    };
    let first = render_block(
        None,
        &block,
        &first_theme,
        &first_theme.rich_renderer(),
        &first_theme.reasoning_renderer(),
        40,
        false,
    )
    .join("\n");

    let mut second_theme = crate::tui::theme::test_theme();
    crate::tui::theme::apply_model_lab(&mut second_theme, ModelLab::DeepSeek);
    let second = render_block(
        None,
        &block,
        &second_theme,
        &second_theme.rich_renderer(),
        &second_theme.reasoning_renderer(),
        40,
        false,
    )
    .join("\n");

    for rendered in [&first, &second] {
        assert!(
            rendered.contains("48;2;18;52;86m"),
            "persisted prompt background changed: {rendered:?}"
        );
        assert!(!rendered.contains("\x1b[31m"), "{rendered:?}");
        assert!(
            rendered.lines().all(|line| visible_width(line) <= 40),
            "{rendered:?}"
        );
    }
}

#[test]
fn persisted_prompt_background_fills_the_shared_grid() {
    const WIDTH: u16 = 24;
    let theme = crate::tui::theme::test_theme();
    let block = TranscriptBlock::User {
        text: "first line  \nsecond line".into(),
        model_lab: Some(ModelLab::OpenAi),
        prompt_color: Some("#123456".into()),
        persisted: true,
    };
    let rendered = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        WIDTH,
        false,
    );
    let terminal = emulate_rows(&rendered, WIDTH);
    let plan = crate::tui::layout::PresentationLayout::new(&theme, WIDTH);
    assert_eq!(plan.inset, 0, "prompt cards must reach both terminal edges");
    assert_eq!(
        rendered.len(),
        4,
        "highlighted prompts need one breathing row on each side"
    );
    assert!(
        strip_terminal_sequences(&rendered[0]).trim().is_empty(),
        "leading prompt padding should remain visually blank: {rendered:?}"
    );
    assert!(
        strip_terminal_sequences(&rendered[1]).starts_with("› first line"),
        "{rendered:?}"
    );
    assert!(
        strip_terminal_sequences(&rendered[2]).starts_with("  second line"),
        "prompt continuations should keep a blank marker indent: {rendered:?}"
    );
    assert!(
        strip_terminal_sequences(rendered.last().expect("trailing prompt padding"))
            .trim()
            .is_empty()
    );
    assert!(rendered[0].contains("48;2;18;52;86m"), "{rendered:?}");
    let expected = vt100::Color::Rgb(0x12, 0x34, 0x56);
    for row in 0..rendered.len() as u16 {
        for column in 0..WIDTH {
            let inside = column >= plan.inset && column < plan.inset + plan.content_width;
            assert_eq!(
                terminal
                    .screen()
                    .cell(row, column)
                    .expect("prompt row cell inside terminal bounds")
                    .bgcolor(),
                if inside {
                    expected
                } else {
                    vt100::Color::Default
                },
                "prompt card grid mismatch at row {row}, column {column}"
            );
        }
    }

    const CARD_WIDTH: u16 = 80;
    let card_theme = crate::tui::theme::test_theme_from_source(SURFACE_TEST_THEME);
    let card_plan = compile_surface_plan(None, &block, &card_theme, CARD_WIDTH);
    assert_eq!(card_plan.chrome, ThemeSurfaceChrome::Card);
    let card_rendered = render_block(
        None,
        &block,
        &card_theme,
        &card_theme.rich_renderer(),
        &card_theme.reasoning_renderer(),
        CARD_WIDTH,
        false,
    );
    let card_terminal = emulate_rows(&card_rendered, CARD_WIDTH);
    let content_row =
        u16::try_from(card_plan.geometry.transition_rows + card_plan.geometry.leading_rows)
            .expect("card content row fits in terminal coordinates");
    let left_border = card_plan.frame_left;
    let right_border = card_plan
        .frame_left
        .saturating_add(card_plan.frame_width)
        .saturating_sub(1);

    assert_eq!(
        card_terminal
            .screen()
            .cell(content_row, left_border)
            .expect("card left border cell")
            .bgcolor(),
        vt100::Color::Default,
        "structural card border must remain outside the surface"
    );
    let mut card_cells = 0;
    for column in left_border.saturating_add(1)..right_border {
        let color = card_terminal
            .screen()
            .cell(content_row, column)
            .expect("card inner prompt cell")
            .bgcolor();
        assert_eq!(
            color, expected,
            "prompt background did not cover theme padding at {column}"
        );
        card_cells += 1;
    }
    assert!(card_cells > 0, "card should retain its themed interior");
    assert_eq!(
        card_terminal
            .screen()
            .cell(content_row, right_border)
            .expect("card right border cell")
            .bgcolor(),
        vt100::Color::Default,
        "structural card border must remain outside the surface"
    );
}

#[test]
fn tool_lifecycle_styles_are_visible_in_terminal_cells() {
    let theme = crate::tui::theme::test_theme_from_source(
        r##"
                [metadata]
                name = "Tool lifecycle cells"

                [colors]
                foreground = "#f4f4f4"
                muted = "#686868"
                error = "#e43f4f"

                [roles."extension.live"]
                foreground = "#00ff00"
                bold = true
            "##,
    );
    let renderer = theme.rich_renderer();
    let muted = role_rgb_color(&theme, "muted");
    let foreground = role_rgb_color(&theme, "foreground");
    let error = role_rgb_color(&theme, "error");
    let syntax_string = role_rgb_color(&theme, "syntax_string");
    assert_ne!(muted, foreground);
    assert_ne!(error, foreground);

    let args = serde_json::json!({"path":"src/lib.rs"});
    let mut active_panel = ToolPanel::new(
        ToolCallId("active-read".into()),
        "read".into(),
        args.to_string(),
        summarize_tool("read", &args),
        "live raw evidence".into(),
        false,
        false,
        None,
        None,
    );
    active_panel.extension_render_segments =
        vec![ygg_agent::extension_process::ToolRenderSegment {
            text: "live output".into(),
            style_role: Some("extension.live".into()),
        }];
    let active = render_block(
        None,
        &TranscriptBlock::Tool(Box::new(active_panel)),
        &theme,
        &renderer,
        &renderer,
        80,
        true,
    );
    let active = emulate_rows(&active, 80);
    assert_ascii_foreground(&active, "Read", foreground);
    assert_ascii_bold(&active, "Read");
    assert_ascii_foreground(&active, "src/lib.rs", muted);
    assert!(!active.screen().contents().contains("live raw evidence"));
    assert!(!active.screen().contents().contains("live output"));

    let completed = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("completed-read".into()),
        "read".into(),
        args.to_string(),
        summarize_tool("read", &args),
        String::new(),
        true,
        false,
        None,
        None,
    )));
    let completed = render_block(None, &completed, &theme, &renderer, &renderer, 80, false);
    let completed = emulate_rows(&completed, 80);
    assert_ascii_foreground(&completed, "Read", foreground);
    assert_ascii_bold(&completed, "Read");
    assert_ascii_foreground(&completed, "src/lib.rs", muted);

    let failed = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("failed-read".into()),
        "read".into(),
        args.to_string(),
        summarize_tool("read", &args),
        "error\npermission denied".into(),
        true,
        true,
        Some("permission denied".into()),
        None,
    )));
    let failed = render_block(None, &failed, &theme, &renderer, &renderer, 80, false);
    let failed = emulate_rows(&failed, 80);
    assert_ascii_foreground(&failed, "Read", foreground);
    assert_ascii_bold(&failed, "Read");
    assert!(failed.screen().contents().contains("permission denied"));
    assert_ascii_foreground(&failed, "permission denied", error);

    let active_bash_args = serde_json::json!({"command":"echo \"active\""});
    let active_bash = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("active-bash".into()),
        "bash".into(),
        active_bash_args.to_string(),
        summarize_tool("bash", &active_bash_args),
        "private streaming output".into(),
        false,
        false,
        None,
        None,
    )));
    let active_bash = render_block(None, &active_bash, &theme, &renderer, &renderer, 80, true);
    let active_bash = emulate_rows(&active_bash, 80);
    assert_ascii_foreground(&active_bash, "Bash", foreground);
    assert_ascii_bold(&active_bash, "Bash");
    assert_ascii_foreground(&active_bash, "\"active\"", syntax_string);
    assert_ascii_foreground(&active_bash, "private streaming output", muted);
    assert!(active_bash
        .screen()
        .contents()
        .contains("private streaming output"));

    for (command, is_error) in [("echo \"complete\"", false), ("echo \"failed\"", true)] {
        let args = serde_json::json!({"command":command});
        let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId(command.into()),
            "bash".into(),
            args.to_string(),
            summarize_tool("bash", &args),
            String::new(),
            true,
            is_error,
            is_error.then(|| "exit 1".into()),
            None,
        )));
        let rendered = render_block(None, &panel, &theme, &renderer, &renderer, 80, false);
        let terminal = emulate_rows(&rendered, 80);
        assert_ascii_foreground(&terminal, "Bash", foreground);
        let quoted = if is_error {
            "\"failed\""
        } else {
            "\"complete\""
        };
        assert_ascii_foreground(&terminal, quoted, syntax_string);
        let (_, bash_column) =
            find_ascii_cell(terminal.screen(), "Bash").expect("Bash label rendered");
        assert_ne!(
            terminal
                .screen()
                .cell(0, bash_column)
                .expect("Bash label cell")
                .fgcolor(),
            error,
            "tool label must not inherit lifecycle red"
        );
    }
}

#[test]
fn prompt_padding_adds_static_prompt_background_rows() {
    let theme = crate::tui::theme::test_theme_from_source("[layout]\nprompt_padding = true");
    let prompt = TranscriptBlock::User {
        text: "padded prompt".into(),
        model_lab: None,
        prompt_color: None,
        persisted: true,
    };

    let plan = compile_surface_plan(None, &prompt, &theme, 120);

    assert_eq!(plan.geometry.leading_rows, 1);
    assert_eq!(plan.geometry.trailing_rows, 1);
}

#[test]
fn notice_markers_use_neutral_success_and_error_lifecycle_tones() {
    let theme = crate::tui::theme::test_theme();
    let neutral = TranscriptBlock::Notice("model changed".into());
    let approved = TranscriptBlock::NoticeStatus {
        text: "action approved".into(),
        tone: NoticeTone::Success,
    };
    let denied = TranscriptBlock::NoticeStatus {
        text: "action denied".into(),
        tone: NoticeTone::Error,
    };

    assert_eq!(
        event_margin_marker(&neutral, &theme, false, false),
        Some(theme.settled_event_dot("neutral", "•"))
    );
    assert_eq!(
        event_margin_marker(&approved, &theme, false, false),
        Some(theme.settled_event_dot("success", "•"))
    );
    assert_eq!(
        event_margin_marker(&denied, &theme, false, false),
        Some(theme.settled_event_dot("error", "•"))
    );
}

#[test]
fn event_margin_markers_cover_responses_tools_and_collapsed_reasoning() {
    let theme = crate::tui::theme::test_theme();
    let args = serde_json::json!({"path":"src/lib.rs"});
    let panel = |finished, is_error| {
        TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("edit".into()),
            "edit".into(),
            args.to_string(),
            summarize_tool("edit", &args),
            String::new(),
            finished,
            is_error,
            is_error.then(|| "failed".into()),
            None,
        )))
    };

    let active_tool = event_margin_marker(&panel(false, false), &theme, true, false)
        .expect("visible active tool marker");
    let quiet_tool = event_margin_marker(&panel(false, false), &theme, false, false)
        .expect("quiet active tool marker");
    assert_eq!(strip_terminal_sequences(&active_tool), "•");
    assert_eq!(strip_terminal_sequences(&quiet_tool), "•");
    assert_eq!(active_tool, theme.fg("foreground", "•"));
    assert_eq!(quiet_tool, theme.settled_event_dot("neutral", "•"));
    assert!(!active_tool.contains("\x1b[5m"), "{active_tool:?}");
    assert_ne!(
        active_tool, quiet_tool,
        "active tool dots should pulse through tone"
    );

    let successful_tool =
        event_margin_marker(&panel(true, false), &theme, false, false).expect("success marker");
    assert_eq!(successful_tool, theme.settled_event_dot("success", "•"));
    let failed_tool =
        event_margin_marker(&panel(true, true), &theme, false, false).expect("failure marker");
    assert_eq!(failed_tool, theme.settled_event_dot("error", "•"));

    let streaming_response =
        TranscriptBlock::Assistant(Box::new(AssistantBlock::streaming("working")));
    let streaming_visible = event_margin_marker(&streaming_response, &theme, true, false)
        .expect("streaming assistant marker");
    let streaming_quiet = event_margin_marker(&streaming_response, &theme, false, false)
        .expect("quiet streaming assistant marker");
    assert_eq!(streaming_visible, theme.fg("foreground", "•"));
    assert_eq!(
        streaming_quiet, streaming_visible,
        "assistant dots stay solid light instead of pulsing into a dim slot"
    );
    let finished_response =
        TranscriptBlock::Assistant(Box::new(AssistantBlock::finalized("done".into())));
    assert_eq!(
        event_margin_marker(&finished_response, &theme, false, false),
        Some(theme.fg("foreground", "•"))
    );

    let prompt = TranscriptBlock::User {
        text: "prompt".into(),
        model_lab: None,
        prompt_color: None,
        persisted: true,
    };
    assert_eq!(event_margin_marker(&prompt, &theme, true, false), None);
    let reasoning =
        TranscriptBlock::Reasoning(Box::new(AssistantBlock::streaming_reasoning("private")));
    assert_eq!(event_margin_marker(&reasoning, &theme, true, false), None);
    assert_eq!(event_margin_marker(&reasoning, &theme, false, false), None);
    assert_eq!(
        event_margin_marker(&reasoning, &theme, true, true)
            .map(|marker| strip_terminal_sequences(&marker)),
        Some("•".into())
    );
    let reasoning_slot = event_margin_marker(&reasoning, &theme, false, true)
        .expect("steady collapsed reasoning marker");
    assert_eq!(strip_terminal_sequences(&reasoning_slot), "•");
    assert_eq!(reasoning_slot, theme.model_fg(None, "•"));
    let compaction = TranscriptBlock::Compaction(Box::new(CompactionBlock {
        label: "Context compacted".into(),
        summary: "summary".into(),
        expanded: false,
    }));
    assert_eq!(event_margin_marker(&compaction, &theme, true, false), None);
    let outcome = TranscriptBlock::Outcome(OutcomeBlock::new(
        RunOutcome::CompletedWithWarnings {
            elapsed: Duration::from_secs(1),
            warnings: 1,
            summary: crate::presentation::RunSummary {
                files_changed: 0,
                tool_calls: 1,
                warnings: 1,
            },
        },
        None,
    ));
    assert_eq!(event_margin_marker(&outcome, &theme, true, false), None);
}

#[test]
fn event_dot_animation_invalidates_active_tool_rows_in_lockstep() {
    let shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for (id, name) in [("read", "read"), ("edit", "edit")] {
            let args = serde_json::json!({"path":"src/lib.rs"});
            let index = state.transcript.len();
            state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
                ToolCallId(id.into()),
                name.into(),
                args.to_string(),
                summarize_tool(name, &args),
                String::new(),
                false,
                false,
                None,
                None,
            ))));
            state.register_active_event(index);
        }
        state.event_dot_visible = true;
        assert!(event_dot_animating(&state));
    }

    let active_rows = || {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .filter(|line| line.contains("Read") || line.contains("Edit"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let uses_uniform_dot = |lines: &[String]| {
        lines
            .iter()
            .all(|line| strip_terminal_sequences(line).starts_with("• "))
    };
    let visible = active_rows();
    assert_eq!(visible.len(), 2, "{visible:?}");
    assert!(uses_uniform_dot(&visible), "{visible:?}");

    shell.state.borrow_mut().advance_event_dot_animation();
    let quiet = active_rows();
    assert_eq!(quiet.len(), 2, "{quiet:?}");
    assert!(uses_uniform_dot(&quiet), "{quiet:?}");
    assert_ne!(
        quiet, visible,
        "event dots should pulse through colour only"
    );

    shell.state.borrow_mut().advance_event_dot_animation();
    let visible_again = active_rows();
    assert_eq!(visible_again, visible);
}

#[test]
fn streaming_response_dot_stays_solid_light_and_settles_solid() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "Streaming answer".into(),
        },
    );

    let response_row = || {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .find(|line| line.contains("Streaming answer"))
            .cloned()
            .expect("streaming response row")
    };
    let visible = response_row();
    assert!(
        strip_terminal_sequences(&visible).starts_with("• Streaming answer"),
        "{visible:?}"
    );
    {
        let mut state = shell.state.borrow_mut();
        // The assistant keeps its solid provenance dot while the separate
        // Working row carries continuing run liveness.
        assert_eq!(state.active_event_blocks, vec![0, 1]);
        assert!(!event_dot_animating(&state));
        state.advance_event_dot_animation();
    }
    let quiet = response_row();
    assert_eq!(
        quiet, visible,
        "assistant dots should stay solid light while streaming"
    );

    {
        let mut state = shell.state.borrow_mut();
        state.close_streaming_blocks();
        assert!(!event_dot_animating(&state));
        assert!(state.active_event_blocks.is_empty());
        let response = state.transcript.first().expect("finished response");
        assert_eq!(
            event_margin_marker(response, &state.theme, false, false),
            Some(state.theme.fg("foreground", "•"))
        );
    }
}

#[test]
fn event_dot_tracking_stays_bounded_in_long_sessions() {
    let shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for index in 0..10_000 {
            state.push_block(TranscriptBlock::Notice(format!("history {index}")));
        }
    }

    // A running tool drives the pulse; its block revision must move without
    // touching any of the settled history above it.
    let args = serde_json::json!({"path":"src/lib.rs"});
    let active_index = {
        let mut state = shell.state.borrow_mut();
        let index = state.transcript.len();
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("edit".into()),
            "edit".into(),
            args.to_string(),
            summarize_tool("edit", &args),
            String::new(),
            false,
            false,
            None,
            None,
        ))));
        state.register_active_event(index);
        assert!(event_dot_animating(&state));
        index
    };
    shell.state.borrow_mut().advance_event_dot_animation();
    {
        let state = shell.state.borrow();
        assert_eq!(state.block_revisions[active_index], 1);
        assert!(state.block_revisions[..active_index]
            .iter()
            .all(|revision| *revision == 0));
    }

    shell
        .state
        .borrow_mut()
        .unregister_active_event(active_index);
    assert!(shell.state.borrow().active_event_blocks.is_empty());
}

#[test]
fn reasoning_to_working_to_tool_reuses_the_cached_tail_in_long_sessions() {
    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for index in 0..4_096 {
            state.push_block(TranscriptBlock::Notice(format!("history {index}")));
        }
    }
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "## Inspecting the repository\n".into(),
        },
    );
    shell.state.borrow_mut().finish_turn_streaming_blocks();
    let (working_start, history_lines, generation) = {
        let state = shell.state.borrow();
        assert!(matches!(
            state.transcript.last(),
            Some(TranscriptBlock::Reasoning(reasoning)) if reasoning.is_working_activity()
        ));
        assert!(matches!(
            state.transcript.get(state.transcript.len() - 2),
            Some(TranscriptBlock::Reasoning(reasoning)) if reasoning.finished
        ));
        let lines = state.rendered_transcript(80);
        let cache = state.transcript_cache.borrow();
        let working_start = *cache.block_starts.last().expect("Working block start");
        (
            working_start,
            lines[..working_start].to_vec(),
            cache.generation,
        )
    };

    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: ToolCallId("responsive-read".into()),
            name: "read".into(),
            args: serde_json::json!({"path": "README.md"}),
        },
    );
    {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        assert_eq!(
            cache.width,
            Some(80),
            "tool admission must not force reflow"
        );
        assert_eq!(cache.lines, history_lines);
        assert_eq!(cache.block_revisions.len() + 1, state.transcript.len());
    }

    shell.apply_edit(EditAction::Char('x'));
    {
        let state = shell.state.borrow();
        let rendered = state.rendered_transcript(80);
        let cache = state.transcript_cache.borrow();
        assert_eq!(state.editor, "x");
        assert_eq!(cache.generation, generation + 1);
        assert_eq!(cache.last_update_start, working_start);
        assert_eq!(&rendered[..working_start], history_lines.as_slice());
    }
}

#[test]
fn unrendered_working_handoff_preserves_the_long_session_cache() {
    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for index in 0..4_096 {
            state.push_block(TranscriptBlock::Notice(format!("history {index}")));
        }
    }
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "## Inspecting the repository\n".into(),
        },
    );
    let (reasoning_start, history_lines, generation, cached_blocks) = {
        let state = shell.state.borrow();
        let lines = state.rendered_transcript(80);
        let cache = state.transcript_cache.borrow();
        let reasoning_start = *cache.block_starts.last().expect("Thinking block start");
        (
            reasoning_start,
            lines[..reasoning_start].to_vec(),
            cache.generation,
            cache.block_revisions.len(),
        )
    };

    // Provider settlement and tool admission commonly arrive in one event
    // burst, before the intermediate Working row receives a frame.
    shell.state.borrow_mut().finish_turn_streaming_blocks();
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: ToolCallId("immediate-read".into()),
            name: "read".into(),
            args: serde_json::json!({"path": "README.md"}),
        },
    );
    {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        assert_eq!(cache.width, Some(80), "the cache width must be retained");
        assert_eq!(cache.block_revisions.len(), cached_blocks);
        assert_eq!(cache.block_revisions.len() + 1, state.transcript.len());
    }

    shell.apply_edit(EditAction::Char('x'));
    {
        let state = shell.state.borrow();
        let rendered = state.rendered_transcript(80);
        let cache = state.transcript_cache.borrow();
        assert_eq!(state.editor, "x");
        assert_eq!(cache.generation, generation + 1);
        assert_eq!(cache.last_update_start, reasoning_start);
        assert_eq!(&rendered[..reasoning_start], history_lines.as_slice());
    }
}

#[test]
fn tool_summaries_do_not_repeat_the_action_label() {
    assert_eq!(
        without_redundant_tool_lead("read", "read /tmp/src/lib.rs"),
        "/tmp/src/lib.rs"
    );
    assert_eq!(
        without_redundant_tool_lead("search", "searched src for pattern"),
        "src for pattern"
    );
    assert_eq!(
        without_redundant_tool_lead("bash", "running cargo test --workspace"),
        "cargo test --workspace"
    );
    assert_eq!(
        without_redundant_tool_lead("edit", "updated src/lib.rs"),
        "src/lib.rs"
    );
    assert_eq!(
        without_redundant_tool_lead("write", "wrote src/lib.rs"),
        "src/lib.rs"
    );
}

#[test]
fn wrapped_tool_summaries_keep_their_action_indent() {
    let theme = crate::tui::theme::test_theme();
    let args = serde_json::json!({
        "path": "crates/ygg-coding-agent/src/tui/view.rs",
        "query": "a-very-long-search-query-that-must-wrap-without-losing-the-tool-label"
    });
    let panel = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("id".into()),
        "search".into(),
        args.to_string(),
        summarize_tool("search", &args),
        String::new(),
        false,
        false,
        None,
        None,
    )));
    let renderer = theme.rich_renderer();
    let lines = render_block(None, &panel, &theme, &renderer, &renderer, 80, false)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();

    assert!(lines.len() > 1, "the long summary should wrap: {lines:?}");
    assert!(lines[0].starts_with("• Explored"), "{lines:?}");
    let value_byte = lines[0]
        .find("crates/ygg-coding-agent")
        .expect("tool summary value on first row");
    let value_column = visible_width(&lines[0][..value_byte]);
    let continuation_indent = " ".repeat(value_column);
    assert!(
        lines[1..]
            .iter()
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with(&continuation_indent)),
        "continuations must hang under the summary column: {lines:?}"
    );
    assert!(lines.last().is_some_and(|line| !line.is_empty()));
}

#[test]
fn default_rich_markdown_is_copy_safe_on_an_unknown_background() {
    let theme = crate::tui::theme::test_theme();
    let source = concat!(
        "Use `Session` without a painted chip.\n\n",
        "```text\nclone/read-only projection\nterminal rows\n```"
    );
    let rendered =
        AssistantBlock::finalized(source.into()).render(&theme.rich_renderer(), &theme, 160);
    let joined = rendered.join("\n");
    assert!(!joined.contains("\x1b[48;"), "{joined:?}");
    assert!(!joined.contains("```"), "{joined}");
    let copied = strip_terminal_sequences(&joined);
    assert!(copied.contains("clone/read-only projection"));
    assert!(copied.contains("terminal rows"));
    assert!(
        !copied.chars().any(|ch| "┌┐└┘╭╮╰╯│─".contains(ch)),
        "{copied:?}"
    );
}

#[test]
fn compiled_default_code_surfaces_adapt_and_cover_language_padding() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
    for (background, sequence) in [
        (TerminalBackground::Dark, "\x1b[48;2;32;38;48m"),
        (TerminalBackground::Light, "\x1b[48;2;241;245;244m"),
    ] {
        let theme = crate::tui::theme::test_theme_for(background, capabilities);
        let rendered = AssistantBlock::finalized("```rust\nlet answer = 42;\n```".into()).render(
            &theme.rich_renderer(),
            &theme,
            80,
        );
        assert!(rendered.len() >= 2, "{background:?}: {rendered:?}");
        assert!(
            rendered.iter().all(|line| line.contains(sequence)),
            "{background:?}: {rendered:?}"
        );
        let widths = rendered
            .iter()
            .map(|line| visible_width(line))
            .collect::<Vec<_>>();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "{widths:?}"
        );
        let copied = strip_terminal_sequences(&rendered.join("\n"));
        assert!(!copied.chars().any(|ch| "┌┐└┘╭╮╰╯│─".contains(ch)));
    }
}

#[test]
fn streamed_markdown_settles_into_rich_structure() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "## Session recovery\n\n**Changes**\n- preserves ".into(),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "valid records\n- removes invalid bytes".into(),
        },
    );
    // Finalization performs the authoritative CommonMark parse. The live
    // suffix remains deliberately literal until its boundary is proven.
    shell.state.borrow_mut().close_streaming_blocks();
    {
        let state = shell.state.borrow();
        let TranscriptBlock::Assistant(assistant) = &state.transcript[0] else {
            panic!("first block must be assistant Markdown");
        };
        assert!(assistant.markdown.is_finished());
        assert_eq!(
            assistant.markdown.committed(),
            &sexy_tui_rs::parse_markdown(&assistant.text)
        );
    }
    let rendered = render_shell(&shell.state.borrow(), 60).join("\n");
    for raw in ["##", "**", "- preserves"] {
        assert!(!rendered.contains(raw), "raw marker leaked: {rendered}");
    }
    assert!(rendered.contains("Session recovery"));
    assert!(rendered.contains('—'));
}

#[test]
fn compiled_default_composer_keeps_the_terminal_background_unfilled() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
    for background in [
        TerminalBackground::Dark,
        TerminalBackground::Light,
        TerminalBackground::Unknown,
    ] {
        let theme = crate::tui::theme::test_theme_for(background, capabilities);
        let mut shell = InteractiveShell::test_shell_with_theme(theme);
        shell.set_identity("anthropic", "claude-sonnet-4", "high");
        let rendered = crate::tui::composer_surface::render_composer_surface(
            &shell.state.borrow(),
            120,
            Instant::now(),
        )
        .join("\n");
        assert!(rendered.contains("38;2;"), "{background:?}: {rendered:?}");
        assert!(
            !rendered.contains("\x1b[48;2;"),
            "{background:?}: {rendered:?}"
        );
    }
}

#[test]
fn compiled_default_composer_border_is_static_during_work() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
    let theme = crate::tui::theme::test_theme_for(TerminalBackground::Dark, capabilities);
    let mut shell = InteractiveShell::test_shell_with_theme(theme);
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    let now = Instant::now();
    let idle_before =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, now);
    let idle_after = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        now + Duration::from_secs(5),
    );
    assert_eq!(idle_before[0], idle_after[0]);

    let run_id = shell.begin_run("anthropic");
    let accent = {
        let state = shell.state.borrow();
        state
            .theme
            .model_rgb(Some(ModelLab::Anthropic))
            .expect("Anthropic model accent")
    };
    let active_before =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, now);
    let active_after = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        now + Duration::from_secs(5),
    );
    assert_eq!(active_before[0], active_after[0]);
    assert_eq!(idle_before[0], active_before[0]);
    assert!(active_before[0].contains(&format!("38;2;{};{};{}", accent.0, accent.1, accent.2)));
    assert!(active_before[..3]
        .iter()
        .chain(&active_after[..3])
        .all(|line| !line.contains("\x1b[48;2;")));

    shell.interrupt_run(run_id);
    let rest = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        now + Duration::from_secs(10),
    );
    assert_eq!(idle_before[0], rest[0]);
}

#[test]
fn scripted_agent_events_map_to_distinct_transcript_and_tool_state() {
    use ygg_agent::{EntryId, FinishReason, ToolOutput};
    use ygg_ai::{AssistantMessage, AssistantPart, ModelId, Protocol};

    let mut shell = InteractiveShell::test_shell();
    let id = ToolCallId("call-1".into());
    let events = vec![
        AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "considering".into(),
        },
        AgentEvent::OutputDelta {
            channel: OutputChannel::Text,
            text: "answer".into(),
        },
        AgentEvent::ToolStarted {
            id: id.clone(),
            name: "read".into(),
            args: serde_json::json!({"path": "src/lib.rs"}),
        },
        AgentEvent::ToolProgress {
            id: id.clone(),
            progress: ToolProgress::Status("reading".into()),
        },
        AgentEvent::ToolFinished {
            id: id.clone(),
            result: Ok(ToolOutput::new("contents")),
            duration: Duration::from_millis(10),
        },
        AgentEvent::TurnFinished {
            message: AssistantMessage {
                content: vec![AssistantPart::Text("answer".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            },
            stop_reason: ygg_ai::StopReason::EndTurn,
            turn_usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
                total_tokens: 15,
                ..Usage::default()
            },
            usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
                total_tokens: 15,
                ..Usage::default()
            },
            session_cost_microdollars: Some(4200),
            run_cost_microdollars: 4200,
        },
        AgentEvent::RunFinished {
            head: EntryId("003".into()),
            reason: FinishReason::Completed,
        },
    ];
    for event in &events {
        shell.on_agent_event(event);
    }
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("considering"));
    assert!(snapshot.contains("answer"));
    assert!(snapshot.contains("read"));
    assert_eq!(shell.debug_tool_output(&id).as_deref(), Some("contents"));
}

#[test]
fn active_bash_renders_command_and_latest_output_tail() {
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("openai");
    let id = ToolCallId("live-bash".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "bash".into(),
            args: serde_json::json!({"command": "long-running-check"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolProgress {
            id: id.clone(),
            progress: ToolProgress::Output {
                stream: ygg_agent::OutputStream::Stdout,
                bytes: bytes::Bytes::from_static(b"private live output"),
            },
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolProgress {
            id,
            progress: ToolProgress::Status("private status detail".into()),
        },
    );
    let rendered = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 100).join("\n"));
    assert!(rendered.contains("Bash  long-running-check"), "{rendered}");
    assert!(rendered.contains("private live output"), "{rendered}");
    assert!(rendered.contains("private status detail"), "{rendered}");
}

#[test]
fn bash_wraps_command_and_nests_output_under_one_elbow() {
    let theme = crate::tui::theme::test_theme();
    let command = "node --input-type=module --check < ygg/demo.js && git diff --check";
    let args = serde_json::json!({"command":command});
    let block = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("quiet-bash".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        "exit=0 duration=0.2s\n(no output)".into(),
        true,
        false,
        None,
        None,
    )));
    let rendered = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        42,
        false,
    );
    assert!(
        rendered[0].contains(&theme.settled_event_dot("success", "•")),
        "successful Bash dot should use the settled green tone: {rendered:?}"
    );
    let rendered = rendered
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();

    assert!(rendered[0].starts_with("• Bash"), "{rendered:?}");
    let command_byte = rendered[0].find("node").expect("command on Bash row");
    let command_column = visible_width(&rendered[0][..command_byte]);
    assert_eq!(
        command_column, 8,
        "Bash input should begin two cells after its label: {rendered:?}"
    );
    let output_row = rendered
        .iter()
        .position(|line| line.contains("(no output)"))
        .expect("no-output row index");
    let no_output = rendered
        .iter()
        .find(|line| line.contains("(no output)"))
        .expect("no-output metadata");
    let elbow_byte = no_output.find('└').expect("nested output elbow");
    let elbow_column = visible_width(&no_output[..elbow_byte]);
    let output_byte = no_output.find("(no output)").expect("nested output text");
    let output_column = visible_width(&no_output[..output_byte]);
    assert_eq!(elbow_column, 2, "{rendered:?}");
    assert_eq!(
        output_column,
        elbow_column + 2,
        "Bash metadata must begin one level after the elbow: {rendered:?}"
    );
    let wrapped_headers = &rendered[1..output_row];
    assert!(
        !wrapped_headers.is_empty(),
        "fixture must wrap the Bash command: {rendered:?}"
    );
    for continuation in wrapped_headers {
        let stem_byte = continuation
            .find('│')
            .expect("wrapped tool headers need a vertical output stem");
        assert_eq!(
            visible_width(&continuation[..stem_byte]),
            elbow_column,
            "tool stem and output elbow must share the tool-label column: {rendered:?}"
        );
        assert!(
            visible_width(continuation) > command_column,
            "wrapped commands must retain their label-relative value column: {rendered:?}"
        );
    }
    assert_eq!(
        rendered.iter().filter(|line| line.contains('└')).count(),
        1,
        "tool output needs one connector for the whole nested group: {rendered:?}"
    );
    assert!(
        rendered.iter().all(|line| !line
            .chars()
            .any(|character| matches!(character, '✓' | '×' | '…'))),
        "the margin dot is the only lifecycle marker: {rendered:?}"
    );
}

#[test]
fn bash_output_and_hidden_metadata_share_a_terminal_content_gutter() {
    let theme = crate::tui::theme::test_theme();
    let command = "printf result";
    let args = serde_json::json!({"command": command});
    let output = (1..=8)
        .map(|line| format!("result line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let panel = ToolPanel::new(
        ToolCallId("bash-gutter".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        format!("exit=0 duration=0.2s\nstdout: 8 lines\n{output}"),
        true,
        false,
        None,
        None,
    );

    let details = render_compact_bash_output(&panel, &theme, 80, false, &tool_value_indent("Bash"));
    assert!(
        details[0].contains("\x1b[38;2;"),
        "hidden-line metadata should use the muted metadata style: {details:?}"
    );
    assert!(
        details[1].contains("\x1b[38;2;"),
        "raw Bash output should use the muted output style: {details:?}"
    );

    let block = TranscriptBlock::Tool(Box::new(panel));
    let rendered = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        80,
        false,
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>();
    let label_byte = rendered[0].find("Bash").expect("label on Bash row");
    let label_column = visible_width(&rendered[0][..label_byte]);
    let hidden = rendered
        .iter()
        .find(|line| line.contains("4 earlier visual rows hidden"))
        .expect("synthetic hidden-line metadata");
    let output = rendered
        .iter()
        .find(|line| line.contains("result line 5"))
        .expect("first retained output row");
    let elbow_byte = hidden.find('└').expect("nested output elbow");
    let elbow_column = visible_width(&hidden[..elbow_byte]);
    let hidden_byte = hidden.find('…').expect("hidden metadata marker");
    let hidden_column = visible_width(&hidden[..hidden_byte]);
    let output_byte = output.find("result line 5").expect("retained output text");
    let output_column = visible_width(&output[..output_byte]);
    assert_eq!(elbow_column, label_column, "{rendered:?}");
    assert_eq!(hidden_column, elbow_column + 2, "{rendered:?}");
    assert_eq!(output_column, elbow_column + 2, "{rendered:?}");
    let TranscriptBlock::Tool(panel) = &block else {
        unreachable!("fixture is a Bash tool panel");
    };
    assert!(
        !panel.output.contains("lines hidden"),
        "synthetic UI metadata must not enter the raw tool payload"
    );
}

#[test]
fn compact_bash_window_is_capped_at_five_physical_rows_after_wrapping() {
    let theme = crate::tui::theme::test_theme();
    let args = serde_json::json!({"command": "printf wrapped"});
    let panel = ToolPanel::new(
        ToolCallId("bash-physical-window".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        format!(
            "exit=0 duration=0.2s\nstdout: 4 lines\n\x1b[31m{}\x1b[0m\n{}\n{}\npartial-tail",
            "界".repeat(30),
            "wrapped-ascii-".repeat(12),
            "e\u{301}".repeat(50),
        ),
        true,
        false,
        None,
        None,
    );

    for width in [18, 24, 42, 80] {
        let rows =
            render_compact_bash_output(&panel, &theme, width, false, &tool_value_indent("Bash"));
        assert_eq!(
            rows.len(),
            COMPACT_EXEC_OUTPUT_ROWS,
            "width {width}: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| visible_width(row) <= usize::from(width)),
            "width {width}: {rows:?}"
        );
    }
}

#[test]
fn carriage_return_bash_progress_replaces_one_visual_row() {
    let theme = crate::tui::theme::test_theme();
    let args = serde_json::json!({"command": "progress"});
    let panel = ToolPanel::new(
        ToolCallId("bash-cr-progress".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        "phase\nprogress 0\rprogress 10\rprogress 100".into(),
        false,
        false,
        None,
        None,
    );
    let rows = render_compact_bash_output(&panel, &theme, 80, false, &tool_value_indent("Bash"));
    let plain = rows
        .iter()
        .map(|row| strip_terminal_sequences(row))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        rows.len(),
        2,
        "short progress output must not reserve blank rows"
    );
    assert!(plain.contains("progress 100"), "{plain}");
    assert!(!plain.contains("progress 10\n"), "{plain}");
    assert!(!plain.contains("progress 0\n"), "{plain}");
}

#[test]
fn adjacent_bash_cards_do_not_reserve_blank_output_rows() {
    let shell = InteractiveShell::test_shell();
    let args = serde_json::json!({"command": "first"});
    let second_args = serde_json::json!({"command": "second"});
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("compact-first".into()),
            "bash".into(),
            args.to_string(),
            summarize_tool("bash", &args),
            String::new(),
            false,
            false,
            None,
            None,
        ))));
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("compact-second".into()),
            "bash".into(),
            second_args.to_string(),
            summarize_tool("bash", &second_args),
            String::new(),
            false,
            false,
            None,
            None,
        ))));
    }

    let card_height = |state: &ShellState| {
        let _ = state.rendered_transcript(32);
        let cache = state.transcript_cache.borrow();
        cache.block_starts[1].saturating_sub(cache.block_starts[0])
    };
    let waiting_height = card_height(&shell.state.borrow());

    let mut heights = Vec::new();
    for (index, output) in [
        "one partial line".to_owned(),
        format!("{}\nlast", "界 wrapped output ".repeat(20)),
        "exit=0 duration=0.1s\nstdout: 2 lines\nfinal one\nfinal two".to_owned(),
    ]
    .into_iter()
    .enumerate()
    {
        let mut state = shell.state.borrow_mut();
        let TranscriptBlock::Tool(panel) = &mut state.transcript[0] else {
            unreachable!()
        };
        panel.output = output;
        panel.finished = index == 2;
        state.touch_block(0);
        heights.push(card_height(&state));
    }

    assert!(
        heights[0] <= waiting_height,
        "one output row should replace the waiting row without reserving blank space: waiting={waiting_height}, rendered={heights:?}"
    );
    assert!(
        heights[1] > heights[0],
        "a full five-row tail may grow the card"
    );
    assert!(
        heights[1] < heights[0] + COMPACT_EXEC_OUTPUT_ROWS,
        "collapsed output exceeded its five-row budget: {heights:?}"
    );
    assert!(
        heights[2] < heights[1],
        "a short final result should release unused output rows: {heights:?}"
    );
}

#[test]
fn final_tool_result_replaces_live_output_without_the_tui_byte_cap() {
    use ygg_agent::ToolOutput;

    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("local");
    let id = ToolCallId("bash-final-replaces-live".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "bash".into(),
            args: serde_json::json!({"command": "large-final"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolProgress {
            id: id.clone(),
            progress: ToolProgress::Output {
                stream: ygg_agent::OutputStream::Stdout,
                bytes: bytes::Bytes::from_static(b"LIVE-ONLY-SENTINEL"),
            },
        },
    );
    let final_output = format!(
        "exit=0 duration=0.1s\nstdout: 1 lines\n{}FINAL-SENTINEL",
        "x".repeat(70 * 1024)
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: id.clone(),
            result: Ok(ToolOutput::new(final_output.clone())),
            duration: Duration::from_millis(10),
        },
    );

    let retained = shell.debug_tool_output(&id).expect("retained final output");
    assert_eq!(retained, final_output);
    assert!(!retained.contains("LIVE-ONLY-SENTINEL"));
    assert!(retained.ends_with("FINAL-SENTINEL"));
}

#[test]
fn failed_bash_output_is_available_in_expanded_rendering() {
    let theme = crate::tui::theme::test_theme();
    let args = serde_json::json!({"command": "failing-command"});
    let output = (1..=8)
        .map(|line| format!("failed output line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let block = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("failed-bash-output".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        format!("error nonzero_exit\nexit=1 duration=0.2s\nstdout: 8 lines\n{output}"),
        true,
        true,
        Some("command exited with code 1".into()),
        None,
    )));

    let collapsed = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        80,
        false,
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(!collapsed.contains("failed output line 1"), "{collapsed}");
    assert!(!collapsed.contains("failed output line 8"), "{collapsed}");
    assert!(collapsed.contains("failed output hidden"), "{collapsed}");

    let expanded = render_block(
        None,
        &block,
        &theme,
        &theme.rich_renderer(),
        &theme.reasoning_renderer(),
        80,
        true,
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(expanded.contains("failed output line 1"), "{expanded}");
    assert!(expanded.contains("failed output line 8"), "{expanded}");
}

#[test]
fn footer_collapses_semantically_and_keeps_one_adjacent_row() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(PathBuf::from("/work/ygg-footer-regression"));
    shell.set_identity(
        "custom-openai",
        "custom/unsloth/Qwen3.6-35B-A3B-MTP-GGUF",
        "high",
    );
    {
        let mut state = shell.state.borrow_mut();
        state.last_turn_usage = Some(Usage {
            input_tokens: 26_800,
            output_tokens: 422,
            total_tokens: 27_222,
            ..Usage::default()
        });
        state.last_turn_tokens_per_second = Some(41.9);
        state.context_estimate = Some((5_600, 246_000));
        state.price_display = PriceDisplay::ExplicitZero;
        state.telemetry_model = Some(state.model.clone());
    }
    let now = Instant::now();
    let wide = plain_footer(&shell, 100, now);
    assert!(wide.starts_with("  Qwen3.6 35B A3B · high"), "{wide:?}");
    assert!(wide.contains("context 2%/246K"), "{wide:?}");
    assert!(wide.ends_with("session $0"), "{wide:?}");
    assert!(!wide.contains('↑') && !wide.contains('↓'), "{wide:?}");

    let medium = plain_footer(&shell, 68, now);
    assert!(medium.contains("Qwen3.6 35B A3B · high"), "{medium:?}");
    assert!(medium.contains("context 2%/246K"), "{medium:?}");
    assert!(medium.ends_with("session $0"), "{medium:?}");

    let compact = plain_footer(&shell, 44, now);
    assert!(compact.contains("Qwen3.6 35B A3B"), "{compact:?}");
    assert!(compact.contains("2%"), "{compact:?}");
    assert!(compact.ends_with("session $0"), "{compact:?}");
    assert!(!compact.contains("high"), "{compact:?}");

    let narrow = plain_footer(&shell, 30, now);
    assert!(narrow.contains("Qwen3.6 35B A3B"), "{narrow:?}");
    assert!(narrow.contains("2%"), "{narrow:?}");
    assert!(!narrow.contains("session"), "{narrow:?}");

    let surface = plain_composer_surface(&shell, 100, now);
    assert_eq!(surface.len(), 4, "one editor row, two rules, one footer");
    assert!(!surface[surface.len() - 2].is_empty());
    assert_eq!(surface.last().unwrap(), &plain_footer(&shell, 100, now));
    assert!(surface.iter().all(|line| visible_width(line) <= 100));
}

#[test]
fn footer_omits_noisy_throughput_but_keeps_final_rate_in_status() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5.6", "high");
    let started = Instant::now();
    {
        let mut state = shell.state.borrow_mut();
        let id = state.run.begin_at("codex", started).unwrap();
        state.run_model = Some(state.model.clone());
        state.telemetry_model = state.run_model.clone();
        state.run_model_display = Some(state.model_display.clone());
        state.run_model_compact_names = state.model_compact_names.clone();
        state.run_reasoning = Some(state.reasoning.clone());
        state.run_price_display = Some(PriceDisplay::Unknown);
        state.run_context_estimate = Some((21_000, 256_000));
        state.run.set_phase_at(
            id,
            RunPhase::AwaitingProvider {
                provider: "codex".into(),
            },
            started,
        );
        state.turn_generation_started_at = Some(started);
        state.turn_streamed_output_bytes = 2_520;
        state.context_estimate = Some((21_000, 256_000));
        state.price_display = PriceDisplay::Unknown;
        state.run_cost_available = false;
    }
    let now = started + Duration::from_millis(8_700);
    let live = plain_footer(&shell, 100, now);
    assert!(!live.contains("Working"), "{live:?}");
    assert!(!live.contains("waiting for API"), "{live:?}");
    assert!(
        !live.contains("tok/s"),
        "noisy live throughput leaked into footer: {live:?}"
    );
    assert!(
        !live.contains('↑') && !live.contains('↓'),
        "token counters leaked into the simplified footer: {live:?}"
    );
    assert!(
        live.contains("context ~8%/256K"),
        "live context pressure missing: {live:?}"
    );
    assert!(
        !live.contains("cost"),
        "unknown price stays quiet: {live:?}"
    );
    assert!(!live.contains('—'), "unknown price stays quiet: {live:?}");
    assert!(
        !live.contains("esc"),
        "implicit controls stay out: {live:?}"
    );
    assert!(
        visible_width(&live) <= 98,
        "status stays inside the right inset"
    );
    let live_diagnostics = status_telemetry(&shell.state.borrow(), now);
    assert!(live_diagnostics.contains("awaiting turn completion"));
    assert!(!live_diagnostics.contains("tok/s"));

    {
        let mut state = shell.state.borrow_mut();
        state.price_display = PriceDisplay::Priced;
        state.run_price_display = Some(PriceDisplay::Priced);
        state.run_cost_available = true;
        state.run_cost_microdollars = 82_000;
        state.session_cost_microdollars = Some(120_000);
    }
    let paid = plain_footer(&shell, 100, now);
    assert!(
        paid.contains("$0.120"),
        "accumulated session cost should be visible: {paid:?}"
    );
    assert!(
        paid.contains("session $0.120"),
        "durable session spend should be labelled: {paid:?}"
    );
    assert!(!paid.contains("Working"), "{paid:?}");

    {
        let mut state = shell.state.borrow_mut();
        state.turn_generation_started_at = None;
        state.turn_streamed_output_bytes = 0;
        state.last_turn_tokens_per_second = Some(72.4);
        state.last_turn_generation_elapsed = Some(Duration::from_secs(2));
        state.last_turn_generated_tokens = Some(145);
        let id = state.run.current_id().unwrap();
        state.run.set_phase_at(
            id,
            RunPhase::RunningTool {
                summary: "running tests".into(),
            },
            started,
        );
    }
    let active_sample = plain_footer(&shell, 100, now);
    assert!(
        !active_sample.contains("tok/s"),
        "final throughput leaked into footer while tools run: {active_sample:?}"
    );
    assert!(
        !active_sample.contains("8.7s"),
        "default timer leaked: {active_sample:?}"
    );
    assert!(!active_sample.contains("tool"));
    let final_diagnostics = status_telemetry(&shell.state.borrow(), now);
    assert!(final_diagnostics.contains("72.4 tok/s final"));

    {
        let mut state = shell.state.borrow_mut();
        let id = state.run.current_id().unwrap();
        state.run.interrupt_at(id, now);
    }
    let completed_sample = plain_footer(&shell, 100, now);
    assert!(
        !completed_sample.contains("tok/s"),
        "final throughput leaked into settled footer: {completed_sample:?}"
    );
    assert!(!completed_sample.contains('~'), "{completed_sample:?}");
}

#[test]
fn footer_distinguishes_explicit_zero_from_unavailable_pricing() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("local", "qwen3.6-35b-a3b", "high");
    let now = Instant::now();

    shell.state.borrow_mut().price_display = PriceDisplay::Unknown;
    let unknown = plain_footer(&shell, 80, now);
    assert!(!unknown.contains('$'));
    assert!(!unknown.contains("cost"));

    {
        let mut state = shell.state.borrow_mut();
        state.price_display = PriceDisplay::Priced;
        state.run_cost_available = true;
        state.run_cost_microdollars = 0;
    }
    let not_yet_charged = plain_footer(&shell, 80, now);
    assert!(!not_yet_charged.contains('$'));

    shell.state.borrow_mut().price_display = PriceDisplay::ExplicitZero;
    let free = plain_footer(&shell, 80, now);
    assert!(free.ends_with("$0"), "{free:?}");

    for width in 1..=120 {
        let surface = plain_composer_surface(&shell, width, now);
        assert!(surface
            .iter()
            .all(|line| visible_width(line) <= usize::from(width)));
    }
}

#[test]
fn idle_footer_shows_accumulated_session_cost_without_opt_in() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5.6-luna", "high");
    {
        let mut state = shell.state.borrow_mut();
        state.price_display = PriceDisplay::Priced;
        state.session_cost_microdollars = Some(91_400);
        state.cache_hit_rate_basis_points = Some(9_240);
        state.context_estimate = Some((102, 272_000));
        state.telemetry_model = Some(state.model.clone());
    }

    let footer = plain_footer(&shell, 120, Instant::now());
    assert!(footer.contains("context 0%/272K"), "{footer:?}");
    assert!(!footer.contains("102/272k"), "{footer:?}");
    assert!(!footer.contains("cache 92.4%"), "{footer:?}");
    assert!(footer.contains("session"), "{footer:?}");
    assert!(
        footer.contains("$0.0914"),
        "accumulated session cost missing: {footer:?}"
    );
    assert!(!footer.contains('~'), "{footer:?}");
}

#[test]
fn subagent_chrome_renders_live_metrics_and_rolls_cost_into_footer_once() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("openai", "gpt-5.6-luna", "high");
    shell.state.borrow_mut().session_cost_microdollars = Some(91_400);
    let snapshot: ygg_agent::ExtensionPresentationSnapshot =
        serde_json::from_value(serde_json::json!({
            "revision": 1,
            "status": {"state": "active", "label": "Subagents"},
            "activities": [{
                "id": "activity:agent-1",
                "kind": "subagent",
                "state": "running",
                "summary": "read-diffs · using read",
                "metrics": {
                    "tool_calls": 16,
                    "input_tokens": 80_000,
                    "cache_read_tokens": 8_200,
                    "cache_write_tokens": 0,
                    "output_tokens": 99,
                    "reasoning_tokens": 20,
                    "cost_microdollars": 208_600
                }
            }],
            "actions": []
        }))
        .unwrap();

    assert!(shell.set_subagent_presentation(Some(&snapshot), true));
    // Delegated workers are transcript events now; the chrome strip stays empty
    // so the same activity is not duplicated below the composer.
    let chrome = shell_chrome(&shell.state.borrow(), 120, Instant::now());
    assert!(chrome.subagents.is_empty(), "{:?}", chrome.subagents);
    let activity = shell
        .state
        .borrow()
        .rendered_transcript(120)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(activity.contains("Subagents"), "{activity}");
    assert!(activity.contains("read-diffs · using read"), "{activity}");
    assert!(activity.contains("16 call"), "{activity}");
    assert!(plain_footer(&shell, 120, Instant::now()).contains("$0.300"));

    assert!(shell.set_subagent_presentation(Some(&snapshot), false));
    let footer = plain_footer(&shell, 120, Instant::now());
    assert!(footer.contains("$0.0914"), "{footer}");
    assert!(!footer.contains("$0.300"), "{footer}");
}

#[test]
fn native_subagent_telemetry_renders_failure_and_hides_generic_spawn_tools() {
    let mut shell = InteractiveShell::test_shell();
    let child = |id: &str, task: &str, state: &str, reason: Option<&str>| {
        ygg_agent::DelegationTelemetryChild {
            child_id: id.into(),
            task_name: task.into(),
            profile: Some("explore".into()),
            model: "cerebras-gemma-4-31b".into(),
            state: state.into(),
            phase: if state == "failed" {
                "failed"
            } else {
                "using_tool"
            }
            .into(),
            current_tool: (state == "running").then(|| "read".into()),
            tool_use_count: 4,
            input_tokens: 12_000,
            cache_read_tokens: 800,
            cache_write_tokens: 0,
            output_tokens: 220,
            reasoning_tokens: 60,
            total_tokens: 13_020,
            cost: None,
            cost_microdollars: Some(7_200),
            elapsed_ms: 42_000,
            failure_class: reason.map(|_| "provider_failure".into()),
            failure_reason: reason.map(str::to_owned),
            effective_tool_policy: test_effective_tool_policy(),
            orchestration_provenance: inherited_delegation_provenance(),
            session: Some("agent-session:opaque".into()),
        }
    };
    let snapshot = ygg_agent::DelegationTelemetrySnapshot {
        revision: 4,
        captured_at_ms: 1_700_000_000_000,
        children: vec![
            child("agent-1", "Read release history", "running", None),
            child(
                "agent-2",
                "Audit release surface",
                "failed",
                Some("provider request failed: upstream unavailable"),
            ),
        ],
        total_cost_microdollars: Some(14_400),
        failure_reason: Some("spawn rejected: worker limit reached".into()),
        failure_class: Some("spawn_rejected".into()),
    };
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated { snapshot });
    let block = shell
        .state
        .borrow()
        .rendered_transcript(120)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(block.contains("Subagents"), "{block}");
    assert!(block.contains("Read release history"), "{block}");
    assert!(block.contains("Audit release surface"), "{block}");
    let heading = block
        .lines()
        .find(|line| line.contains("Subagents"))
        .expect("subagent heading");
    let child = block
        .lines()
        .find(|line| line.contains("Read release history"))
        .expect("subagent child");
    let heading_byte = heading.find("Subagents").expect("heading text");
    let elbow_byte = child
        .find(['├', '└'])
        .expect("subagent hierarchy connector");
    let task_byte = child.find("Read release history").expect("child text");
    let heading_column = visible_width(&heading[..heading_byte]);
    let elbow_column = visible_width(&child[..elbow_byte]);
    let task_column = visible_width(&child[..task_byte]);
    assert_eq!(elbow_column, heading_column, "{block}");
    assert_eq!(task_column, elbow_column + 2, "{block}");
    assert!(block.contains("failed"), "{block}");
    // Live tool-call and token/cost telemetry must render in the transcript
    // event, matching the composer chrome strip.
    assert!(block.contains("4 calls"), "{block}");
    assert!(block.contains("12.8k"), "{block}");
    assert!(
        block.contains("↓220") || block.contains("out 220"),
        "{block}"
    );
    assert!(block.contains("$0.007"), "{block}");
    assert!(
        block.contains("provider request failed: upstream unavailable")
            || shell
                .state
                .borrow()
                .subagent_activity
                .as_ref()
                .and_then(|view| view.failure_reason.as_deref())
                == Some("spawn rejected: worker limit reached"),
        "{block}"
    );
    assert!(!shell
        .state
        .borrow()
        .rendered_transcript(120)
        .join("\n")
        .contains("Used subagent spawn"));

    // An empty cleanup snapshot must not erase the settled transcript event.
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 5,
            captured_at_ms: 1_700_000_000_001,
            children: Vec::new(),
            total_cost_microdollars: None,
            failure_reason: None,
            failure_class: None,
        },
    });
    assert!(shell.state.borrow().subagent_activity.is_some());
    assert!(shell.state.borrow().subagent_activity_block.is_some());

    shell.on_agent_event(&ygg_agent::AgentEvent::ToolStarted {
        id: ygg_ai::ToolCallId("spawn-call".into()),
        name: "subagent_spawn".into(),
        args: serde_json::json!({"name": "worker"}),
    });
    let transcript = shell.state.borrow().rendered_transcript(120).join("\n");
    assert!(!transcript.contains("Used subagent spawn"), "{transcript}");
}

#[test]
fn extension_tools_render_label_and_argument_like_core_tools() {
    let mut shell = InteractiveShell::test_shell();
    shell.on_agent_event(&ygg_agent::AgentEvent::ToolStarted {
        id: ygg_ai::ToolCallId("ext-call".into()),
        name: "web_search".into(),
        args: serde_json::json!({"query": "rust tui transcript"}),
    });
    let transcript =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(120).join("\n"));
    assert!(transcript.contains("Web search"), "{transcript}");
    assert!(transcript.contains("rust tui transcript"), "{transcript}");
    // The tool name must not be duplicated in the value column, and the
    // legacy "Used" lead stays gone.
    assert!(!transcript.contains("Used "), "{transcript}");
    assert!(!transcript.contains("web search: rust tui"), "{transcript}");

    shell.on_agent_event(&ygg_agent::AgentEvent::ToolStarted {
        id: ygg_ai::ToolCallId("ssh-call".into()),
        name: "ssh_exec".into(),
        args: serde_json::json!({"argv": ["cargo", "test"]}),
    });
    let transcript =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(120).join("\n"));
    assert!(transcript.contains("SSH"), "{transcript}");
    assert!(!transcript.contains("Ssh"), "{transcript}");
}

#[test]
fn hydrating_a_replacement_session_clears_subagent_activity() {
    let mut shell = InteractiveShell::test_shell();
    let snapshot = ygg_agent::DelegationTelemetrySnapshot {
        revision: 1,
        captured_at_ms: 1_700_000_000_000,
        children: vec![ygg_agent::DelegationTelemetryChild {
            child_id: "agent-1".into(),
            task_name: "Inspect tests".into(),
            profile: Some("explore".into()),
            model: "test-model".into(),
            state: "running".into(),
            phase: "using_tool".into(),
            current_tool: Some("read".into()),
            tool_use_count: 1,
            input_tokens: 100,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 10,
            reasoning_tokens: 0,
            total_tokens: 110,
            cost: None,
            cost_microdollars: Some(1),
            elapsed_ms: 500,
            failure_class: None,
            failure_reason: None,
            effective_tool_policy: test_effective_tool_policy(),
            orchestration_provenance: inherited_delegation_provenance(),
            session: Some("agent-session:opaque".into()),
        }],
        total_cost_microdollars: Some(1),
        failure_reason: None,
        failure_class: None,
    };
    assert!(shell.set_subagent_telemetry(Some(&snapshot), true));
    assert!(shell.state.borrow().subagent_activity.is_some());
    let directory = tempfile::tempdir().unwrap();
    let session = Session::create(directory.path().join("replacement.jsonl")).unwrap();

    shell.hydrate(&session).unwrap();

    assert!(shell.state.borrow().subagent_activity.is_none());
    assert!(shell.state.borrow().subagent_activity_block.is_none());
}

#[test]
fn terminal_subagent_snapshots_hide_the_activity_strip() {
    let mut shell = InteractiveShell::test_shell();
    let child = |id: &str, state: &str| ygg_agent::DelegationTelemetryChild {
        child_id: id.into(),
        task_name: "Inspect tests".into(),
        profile: Some("explore".into()),
        model: "test-model".into(),
        state: state.into(),
        phase: "using_tool".into(),
        current_tool: Some("read".into()),
        tool_use_count: 1,
        input_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 10,
        reasoning_tokens: 0,
        total_tokens: 110,
        cost: None,
        cost_microdollars: Some(1),
        elapsed_ms: 500,
        failure_class: None,
        failure_reason: None,
        effective_tool_policy: test_effective_tool_policy(),
        orchestration_provenance: inherited_delegation_provenance(),
        session: Some("agent-session:opaque".into()),
    };

    // A live worker keeps the transcript event active...
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 1,
            captured_at_ms: 1_700_000_000_000,
            children: vec![child("agent-1", "running")],
            total_cost_microdollars: Some(1),
            failure_reason: None,
            failure_class: None,
        },
    });
    assert!(shell.state.borrow().subagent_activity.is_some());
    assert!(shell.state.borrow().subagent_activity_block.is_some());
    assert!(shell_chrome(&shell.state.borrow(), 120, Instant::now())
        .subagents
        .is_empty());

    // ...and the final settlement keeps the event in the transcript (with a
    // settled green/red margin) instead of parking it in the chrome strip.
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 2,
            captured_at_ms: 1_700_000_000_001,
            children: vec![child("agent-1", "completed")],
            total_cost_microdollars: Some(1),
            failure_reason: None,
            failure_class: None,
        },
    });
    assert!(shell.state.borrow().subagent_activity.is_some());
    assert!(shell.state.borrow().subagent_activity_block.is_some());
    let settled = shell
        .state
        .borrow()
        .rendered_transcript(120)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(settled.contains("Subagents"), "{settled}");
    assert!(settled.contains("Inspect tests"), "{settled}");
    assert!(settled.contains("completed"), "{settled}");

    // A spawn that fails outright is still transcript material.
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 3,
            captured_at_ms: 1_700_000_000_002,
            children: Vec::new(),
            total_cost_microdollars: None,
            failure_reason: Some("spawn rejected: worker limit reached".into()),
            failure_class: Some("spawn_rejected".into()),
        },
    });
    assert!(shell.state.borrow().subagent_activity.is_some());
    let failed = shell
        .state
        .borrow()
        .rendered_transcript(120)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        failed.contains("spawn rejected: worker limit reached"),
        "{failed}"
    );
}

#[test]
fn subagent_activity_renders_complete_roster_in_both_disclosure_modes() {
    let mut shell = InteractiveShell::test_shell();
    let child = |id: &str, task: &str, state: &str| ygg_agent::DelegationTelemetryChild {
        child_id: id.into(),
        task_name: task.into(),
        profile: Some("explore".into()),
        model: "test-model".into(),
        state: state.into(),
        phase: "using_tool".into(),
        current_tool: (state == "running").then(|| "read".into()),
        tool_use_count: 2,
        input_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 10,
        reasoning_tokens: 0,
        total_tokens: 110,
        cost: None,
        cost_microdollars: Some(1),
        elapsed_ms: 500,
        failure_class: None,
        failure_reason: None,
        effective_tool_policy: test_effective_tool_policy(),
        orchestration_provenance: inherited_delegation_provenance(),
        session: Some("agent-session:opaque".into()),
    };
    let render = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(120)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 1,
            captured_at_ms: 1_700_000_000_000,
            children: vec![
                child("agent-1", "Read release history", "running"),
                child("agent-2", "Audit release surface", "running"),
                child("agent-3", "Scan changelog", "completed"),
                child("agent-4", "Inspect tests", "running"),
                child("agent-5", "Check package map", "running"),
                child("agent-6", "Review docs", "completed"),
                child("agent-7", "Audit extensions", "running"),
                child("agent-8", "Verify release", "completed"),
            ],
            total_cost_microdollars: Some(3),
            failure_reason: None,
            failure_class: None,
        },
    });

    assert!(!shell.verbose_tools());
    let collapsed = render(&shell);
    for task in [
        "Read release history",
        "Audit release surface",
        "Scan changelog",
        "Inspect tests",
        "Check package map",
        "Review docs",
        "Audit extensions",
        "Verify release",
    ] {
        assert!(collapsed.contains(task), "missing {task}: {collapsed}");
    }

    // Ctrl+O still controls ordinary tool disclosure, but a subagents event is
    // already bounded to the complete eight-worker roster in either mode.
    shell.toggle_disclosure();
    assert!(shell.verbose_tools());
    let expanded = render(&shell);
    assert!(expanded.contains("Read release history"), "{expanded}");
    assert!(expanded.contains("Verify release"), "{expanded}");

    shell.toggle_disclosure();
    assert!(!shell.verbose_tools());
    let collapsed_again = render(&shell);
    assert!(
        collapsed_again.contains("Read release history"),
        "{collapsed_again}"
    );

    // Settlement preserves the complete roster; it does not introduce a
    // separate disclosure mode for the subagents event.
    shell.on_agent_event(&ygg_agent::AgentEvent::DelegationUpdated {
        snapshot: ygg_agent::DelegationTelemetrySnapshot {
            revision: 2,
            captured_at_ms: 1_700_000_000_001,
            children: vec![
                child("agent-1", "Read release history", "completed"),
                child("agent-2", "Audit release surface", "completed"),
                child("agent-3", "Scan changelog", "completed"),
            ],
            total_cost_microdollars: Some(3),
            failure_reason: None,
            failure_class: None,
        },
    });
    shell.toggle_disclosure();
    assert!(shell.verbose_tools());
    assert!(render(&shell).contains("Read release history"));
}

#[test]
fn extension_presentation_hides_terminal_subagent_activities() {
    let mut shell = InteractiveShell::test_shell();
    let snapshot = |state: &str| -> ygg_agent::ExtensionPresentationSnapshot {
        serde_json::from_value(serde_json::json!({
            "revision": 1,
            "status": {"state": "active", "label": "Subagents"},
            "activities": [{
                "id": "activity:agent-1",
                "kind": "subagent",
                "state": state,
                "summary": "read-diffs · using read",
                "metrics": {
                    "tool_calls": 16,
                    "input_tokens": 80_000,
                    "cache_read_tokens": 8_200,
                    "cache_write_tokens": 0,
                    "output_tokens": 99,
                    "reasoning_tokens": 20,
                    "cost_microdollars": 208_600
                }
            }],
            "actions": []
        }))
        .unwrap()
    };

    assert!(shell.set_subagent_presentation(Some(&snapshot("running")), true));
    assert!(shell.state.borrow().subagent_activity.is_some());
    assert!(shell.state.borrow().subagent_activity_block.is_some());
    assert!(shell_chrome(&shell.state.borrow(), 120, Instant::now())
        .subagents
        .is_empty());

    // Terminal extension activities settle into the transcript event rather
    // than being cleared from the shell chrome.
    assert!(shell.set_subagent_presentation(Some(&snapshot("succeeded")), true));
    assert!(shell.state.borrow().subagent_activity.is_some());
    assert!(shell.state.borrow().subagent_activity_block.is_some());
    let settled = shell
        .state
        .borrow()
        .rendered_transcript(120)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(settled.contains("Subagents"), "{settled}");
    assert!(settled.contains("read-diffs · using read"), "{settled}");
    assert!(settled.contains("completed"), "{settled}");
}

#[test]
fn semantic_transcript_blocks_have_uniform_transition_spacing() {
    let theme = crate::tui::theme::test_theme();
    let rich_renderer = theme.rich_renderer();
    let reasoning_renderer = theme.reasoning_renderer();
    let transcript = (0..12)
        .map(|step| {
            let mut reasoning = AssistantBlock::finalized_reasoning(format!("Step {step}"));
            reasoning.reasoning_expanded = true;
            TranscriptBlock::Reasoning(Box::new(reasoning))
        })
        .collect::<Vec<_>>();

    let mut visible = Vec::new();
    for (index, block) in transcript.iter().enumerate() {
        visible.extend(render_block(
            index.checked_sub(1).and_then(|index| transcript.get(index)),
            block,
            &theme,
            &rich_renderer,
            &reasoning_renderer,
            80,
            false,
        ));
    }
    let plain = visible
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert_eq!(
        plain.iter().filter(|line| line.contains("Step ")).count(),
        12
    );
    assert!(plain.iter().any(|line| line.contains("Step 0")));
    assert!(!plain.iter().any(|line| line.contains("earlier analysis")));
    assert_eq!(plain.iter().filter(|line| line.is_empty()).count(), 11);
    for step in 1..12 {
        let label = format!("Step {step}");
        let index = plain
            .iter()
            .position(|line| line.contains(&label))
            .expect("every reasoning block is rendered");
        assert_eq!(
            plain.get(index.wrapping_sub(1)).map(String::as_str),
            Some("")
        );
        assert!(index < 2 || !plain[index - 2].is_empty());
    }

    let mut verbose_reasoning = AssistantBlock::finalized_reasoning(
        "First complete thought.\n\nSecond complete thought.".into(),
    );
    verbose_reasoning.reasoning_expanded = true;
    let verbose = TranscriptBlock::Reasoning(Box::new(verbose_reasoning));
    let verbose = render_block(
        None,
        &verbose,
        &theme,
        &rich_renderer,
        &reasoning_renderer,
        80,
        false,
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>()
    .join("\n");
    assert!(verbose.contains("First complete thought."), "{verbose}");
    assert!(verbose.contains("Second complete thought."), "{verbose}");

    let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("read-compact".into()),
        "read".into(),
        serde_json::json!({"path":"src/lib.rs"}).to_string(),
        summarize_tool("read", &serde_json::json!({"path":"src/lib.rs"})),
        String::new(),
        false,
        false,
        None,
        None,
    )));
    let transition = render_block(
        transcript.last(),
        &tool,
        &theme,
        &rich_renderer,
        &reasoning_renderer,
        80,
        false,
    );
    assert_eq!(transition.first().map(String::as_str), Some(""));
    assert!(transition.get(1).is_some_and(|line| !line.is_empty()));
}

#[test]
fn consecutive_tool_calls_have_one_breathing_row_between_them() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.rich_renderer();
    let tool = |id: &str, name: &str, args: serde_json::Value| {
        TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId(id.into()),
            name.into(),
            args.to_string(),
            summarize_tool(name, &args),
            String::new(),
            true,
            false,
            None,
            None,
        )))
    };
    let tools = [
        tool("read", "read", serde_json::json!({"path":"src/lib.rs"})),
        tool(
            "bash",
            "bash",
            serde_json::json!({"command":"cargo test -p ygg-coding-agent"}),
        ),
        tool("edit", "edit", serde_json::json!({"path":"src/lib.rs"})),
    ];

    for (index, block) in tools.iter().enumerate() {
        let rendered = render_block(
            index
                .checked_sub(1)
                .and_then(|previous| tools.get(previous)),
            block,
            &theme,
            &renderer,
            &renderer,
            80,
            false,
        );
        if index == 0 {
            assert!(rendered.first().is_some_and(|line| !line.is_empty()));
        } else {
            assert_eq!(rendered.first().map(String::as_str), Some(""));
            assert!(rendered.get(1).is_some_and(|line| !line.is_empty()));
            assert!(rendered.get(2).is_none_or(|line| !line.is_empty()));
        }
    }
}

#[test]
fn read_results_stay_hidden_in_collapsed_and_expanded_modes() {
    use ygg_agent::ToolOutput;

    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("local");
    let id = ToolCallId("read-hidden-result".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "read".into(),
            args: serde_json::json!({
                "path": "src/private.rs",
                "offset": 41,
                "limit": 7
            }),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: id.clone(),
            result: Ok(ToolOutput::new(
                "READ RESULT SENTINEL\nfn private_implementation() {}",
            )),
            duration: Duration::from_millis(10),
        },
    );
    let transcript = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(100)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let collapsed = transcript(&shell);
    assert!(collapsed.contains("src/private.rs:41-47"), "{collapsed}");
    assert!(!collapsed.contains("READ RESULT SENTINEL"), "{collapsed}");
    assert!(!collapsed.to_ascii_lowercase().contains("evidence"));

    shell.toggle_disclosure();
    let expanded = transcript(&shell);
    assert!(expanded.contains("src/private.rs:41-47"), "{expanded}");
    assert!(!expanded.contains("READ RESULT SENTINEL"), "{expanded}");
    assert!(!expanded.to_ascii_lowercase().contains("evidence"));
    assert_eq!(
        shell.debug_tool_output(&id).as_deref(),
        Some("READ RESULT SENTINEL\nfn private_implementation() {}")
    );
}

#[test]
fn tool_output_tail_expands_with_global_ctrl_o_and_copy_stays_safe() {
    use ygg_agent::ToolOutput;
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("local");
    let id = ToolCallId("bash-roundtrip".into());
    let output_lines = (1..=8)
        .map(|line| format!("private result line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let secret =
        format!("exit=0 duration=0.1s\nstdout: 8 lines\n{output_lines}\ntruncated_stdout=false");
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "bash".into(),
            args: serde_json::json!({"command": "printf private"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: id.clone(),
            result: Ok(ToolOutput::new(secret.clone())),
            duration: Duration::from_millis(10),
        },
    );
    let transcript = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(100)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let collapsed = transcript(&shell);
    assert!(collapsed.contains("private result line 8"), "{collapsed}");
    assert!(!collapsed.contains("private result line 1"), "{collapsed}");
    assert!(
        collapsed.contains("4 earlier visual rows hidden"),
        "{collapsed}"
    );
    assert_eq!(
        shell.debug_tool_output(&id).as_deref(),
        Some(secret.as_str())
    );

    shell.toggle_disclosure();
    assert!(shell.verbose_tools());
    let expanded = transcript(&shell);
    assert!(expanded.contains("private result line 1"), "{expanded}");
    assert!(expanded.contains("private result line 8"), "{expanded}");
    assert!(!expanded.to_ascii_lowercase().contains("evidence"));

    shell.toggle_disclosure();
    assert!(!shell.verbose_tools());
    let collapsed_again = transcript(&shell);
    assert!(
        !collapsed_again.contains("private result line 1"),
        "{collapsed_again}"
    );
    let state = shell.state.borrow();
    let index = *state.tool_panels.get(&id).expect("tool panel index");
    assert!(!block_copy_text(&state.transcript[index]).contains("private result line"));
}

#[test]
fn search_output_and_edit_write_diffs_expand_with_global_ctrl_o() {
    use ygg_agent::ToolOutput;

    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("local");
    let search_id = ToolCallId("expand-search".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: search_id.clone(),
            name: "search".into(),
            args: serde_json::json!({"query": "needle", "path": "src"}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: search_id,
            result: Ok(ToolOutput::new(
                (1..=8)
                    .map(|line| format!("SEARCH MATCH {line}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            duration: Duration::from_millis(10),
        },
    );

    for (tool, final_sentinel) in [
        ("edit", "EDIT DIFF FINAL SENTINEL"),
        ("write", "WRITE DIFF FINAL SENTINEL"),
    ] {
        let id = ToolCallId(format!("expand-{tool}"));
        let path = format!("src/{tool}.rs");
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolStarted {
                id: id.clone(),
                name: tool.into(),
                args: serde_json::json!({"path": path}),
            },
        );
        let mut diff = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,12 +1,12 @@\n"
        );
        for line in 1..=11 {
            diff.push_str(&format!("-old {line}\n+new {line}\n"));
        }
        diff.push_str(&format!("-old final\n+{final_sentinel}\n"));
        shell.on_run_event(
            run_id,
            &AgentEvent::ToolFinished {
                id,
                result: Ok(ToolOutput::new(diff)),
                duration: Duration::from_millis(10),
            },
        );
    }

    let transcript = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(120)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let collapsed = transcript(&shell);
    assert!(!collapsed.contains("SEARCH MATCH 1"), "{collapsed}");
    assert!(collapsed.contains("SEARCH MATCH 8"), "{collapsed}");
    assert!(
        !collapsed.contains("EDIT DIFF FINAL SENTINEL"),
        "{collapsed}"
    );
    assert!(
        !collapsed.contains("WRITE DIFF FINAL SENTINEL"),
        "{collapsed}"
    );

    shell.toggle_disclosure();
    let expanded = transcript(&shell);
    assert!(expanded.contains("SEARCH MATCH 1"), "{expanded}");
    assert!(expanded.contains("SEARCH MATCH 8"), "{expanded}");
    assert!(expanded.contains("EDIT DIFF FINAL SENTINEL"), "{expanded}");
    assert!(expanded.contains("WRITE DIFF FINAL SENTINEL"), "{expanded}");
}

#[test]
fn ctrl_o_toggles_all_expandable_transcript_blocks() {
    let mut shell = InteractiveShell::test_shell();
    let args = serde_json::json!({"command": "printf output"});
    let tool_output = (1..=6)
        .map(|line| format!("tool output {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let shell_output = (1..=6)
        .map(|line| format!("shell output {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::finalized_reasoning("private reasoning body".into()),
        )));
        state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
            ToolCallId("global-tool".into()),
            "bash".into(),
            args.to_string(),
            summarize_tool("bash", &args),
            format!("exit=0 duration=0.1s\nstdout: 6 lines\n{tool_output}"),
            true,
            false,
            None,
            None,
        ))));
        state.push_block(TranscriptBlock::Shell(Box::new(ShellOutput {
            id: "global-shell".into(),
            command: "printf shell".into(),
            output: shell_output,
            exit_code: 0,
            running: false,
        })));
        state.push_block(TranscriptBlock::Compaction(Box::new(CompactionBlock {
            label: "Context compacted".into(),
            summary: "private compaction body".into(),
            expanded: false,
        })));
    }
    let transcript = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(100)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let collapsed = transcript(&shell);
    assert!(!collapsed.contains("private reasoning body"), "{collapsed}");
    assert!(!collapsed.contains("tool output 1"), "{collapsed}");
    assert!(collapsed.contains("tool output 6"), "{collapsed}");
    assert!(!collapsed.contains("shell output 1"), "{collapsed}");
    assert!(collapsed.contains("shell output 6"), "{collapsed}");
    assert!(
        !collapsed.contains("private compaction body"),
        "{collapsed}"
    );

    shell.toggle_disclosure();
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Reasoning(Box::new(
            AssistantBlock::finalized_reasoning("future reasoning body".into()),
        )));
    }
    let expanded = transcript(&shell);
    assert!(expanded.contains("private reasoning body"), "{expanded}");
    assert!(expanded.contains("future reasoning body"), "{expanded}");
    assert!(expanded.contains("tool output 1"), "{expanded}");
    assert!(expanded.contains("shell output 1"), "{expanded}");
    assert!(expanded.contains("private compaction body"), "{expanded}");

    shell.toggle_disclosure();
    let collapsed_again = transcript(&shell);
    assert!(
        !collapsed_again.contains("private reasoning body"),
        "{collapsed_again}"
    );
    assert!(
        !collapsed_again.contains("future reasoning body"),
        "{collapsed_again}"
    );
    assert!(
        !collapsed_again.contains("tool output 1"),
        "{collapsed_again}"
    );
    assert!(
        !collapsed_again.contains("shell output 1"),
        "{collapsed_again}"
    );
    assert!(
        !collapsed_again.contains("private compaction body"),
        "{collapsed_again}"
    );
}

#[test]
fn extension_tool_renderer_stays_internal_to_the_tool_record() {
    use ygg_agent::extension_process::ToolRenderSegment;
    use ygg_agent::ToolOutput;
    let mut shell = InteractiveShell::test_shell();
    let run_id = shell.begin_run("local");
    let id = ToolCallId("extension-render".into());
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolStarted {
            id: id.clone(),
            name: "git_status".into(),
            args: serde_json::json!({"workspace": "."}),
        },
    );
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id: id.clone(),
            result: Ok(ToolOutput::new("RAW EVIDENCE")),
            duration: Duration::from_millis(10),
        },
    );
    shell.apply_extension_tool_renderer(
        &id,
        &[ToolRenderSegment {
            text: "branch: main".into(),
            style_role: Some("extension.test.label".into()),
        }],
    );
    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(100)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains("RAW EVIDENCE"), "{rendered}");
    assert!(!rendered.contains("branch: main"), "{rendered}");
    shell.toggle_disclosure();
    let expanded = shell
        .state
        .borrow()
        .rendered_transcript(100)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!expanded.contains("RAW EVIDENCE"), "{expanded}");
    assert!(!expanded.contains("branch: main"), "{expanded}");
    let state = shell.state.borrow();
    let index = *state.tool_panels.get(&id).expect("tool panel index");
    let TranscriptBlock::Tool(panel) = &state.transcript[index] else {
        panic!("tool panel")
    };
    assert_eq!(panel.output, "RAW EVIDENCE");
    assert_eq!(panel.extension_render_segments[0].text, "branch: main");
}

#[test]
fn active_model_switch_keeps_run_identity_and_clears_stale_idle_telemetry() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(46, 12);
    shell.set_identity("openai", "gpt-5.6", "high");
    {
        let mut state = shell.state.borrow_mut();
        crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::OpenAi);
        state.model_lab = Some(ModelLab::OpenAi);
        state.context_estimate = Some((12_000, 256_000));
        state.price_display = PriceDisplay::Priced;
    }
    shell.on_prompt_submitted("prompt for A");
    let run_id = shell.begin_run("openai");
    let now = Instant::now();
    let before =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 46, now);

    shell.set_identity("deepseek", "deepseek-v4-pro", "medium");
    {
        let mut state = shell.state.borrow_mut();
        crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::DeepSeek);
        state.model_lab = Some(ModelLab::DeepSeek);
        state.context_estimate = Some((2_000, 128_000));
        state.last_turn_tokens_per_second = Some(55.0);
        state.run_cost_microdollars = 3_000;
        state.run_cost_available = true;
    }
    shell.on_run_event(
        run_id,
        &AgentEvent::OutputDelta {
            channel: OutputChannel::Reasoning,
            text: "Checking ownership".into(),
        },
    );
    let active_footer = plain_footer(&shell, 46, now);
    assert!(active_footer.contains("GPT-5.6"), "{active_footer:?}");
    assert!(!active_footer.contains("DeepSeek"), "{active_footer:?}");
    shell.set_size(24, 12);
    let narrow_active = plain_footer(&shell, 24, now);
    assert!(narrow_active.contains("GPT-5.6"), "{narrow_active:?}");
    assert!(!narrow_active.contains("Working"), "{narrow_active:?}");
    assert!(!narrow_active.contains("tool"), "{narrow_active:?}");
    shell.set_size(46, 12);
    {
        let state = shell.state.borrow();
        let TranscriptBlock::Reasoning(reasoning) = state.transcript.last().unwrap() else {
            panic!("streamed reasoning block expected");
        };
        assert_eq!(reasoning.model_lab, Some(ModelLab::OpenAi));
    }
    let after =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 46, now);
    assert_eq!(before.len(), after.len());
    assert_eq!(visible_width(&before[0]), visible_width(&after[0]));

    shell.interrupt_run(run_id);
    let idle_footer = plain_footer(&shell, 46, now);
    assert!(idle_footer.contains("DeepSeek V4 Pro"), "{idle_footer:?}");
    assert!(!idle_footer.contains("55.0 tok/s"), "{idle_footer:?}");
    assert!(!idle_footer.contains("$0.003"), "{idle_footer:?}");
    shell.on_prompt_submitted("prompt for B");
    let state = shell.state.borrow();
    let prompts = state
        .transcript
        .iter()
        .filter_map(|block| match block {
            TranscriptBlock::User {
                text,
                model_lab,
                prompt_color,
                ..
            } => Some((text.as_str(), *model_lab, prompt_color.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        prompts,
        vec![
            (
                "prompt for A",
                Some(ModelLab::OpenAi),
                Some(crate::tui::theme::prompt_color_for_model_id("gpt-5.6")),
            ),
            (
                "prompt for B",
                Some(ModelLab::DeepSeek),
                Some(crate::tui::theme::prompt_color_for_model_id(
                    "deepseek-v4-pro",
                )),
            ),
        ]
    );
}

#[test]
fn custom_theme_composer_tokens_render_framed_ruled_and_shaded_composers() {
    let composer_lines = |source: &str| -> Vec<String> {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(60, 12);
        shell.set_theme(crate::tui::theme::test_theme_from_source(source));
        let rendered = render_shell(&shell.state.borrow(), 60);
        rendered
    };

    let framed = composer_lines(
        r##"
            [colors]
            composer = "framed"
            composer_border = "#6688aa"
            [layout]
            composer_padding = 1
        "##,
    );
    let plain = framed
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    let top = plain
        .iter()
        .position(|line| line.trim_start().starts_with('╭') && line.ends_with('╮'))
        .unwrap_or_else(|| panic!("framed composer lost its top corners: {plain:?}"));
    assert!(
        plain[top + 1..]
            .iter()
            .any(|line| line.trim_start().starts_with('╰') && line.ends_with('╯')),
        "framed composer lost its bottom corners"
    );
    assert!(
        plain[top + 1].trim_start().starts_with('│') && plain[top + 1].ends_with('│'),
        "framed composer content rows lost their side borders: {:?}",
        plain[top + 1]
    );

    let ruled = composer_lines(
        r##"
            [colors]
            composer = "boxed"
            composer_border = "#6688aa"
            [layout]
            composer_padding = 1
        "##,
    )
    .iter()
    .map(|line| strip_terminal_sequences(line))
    .collect::<Vec<_>>();
    assert!(
        ruled
            .iter()
            .filter(|line| {
                !line.trim().is_empty() && line.trim().chars().all(|character| character == '─')
            })
            .count()
            >= 2,
        "ruled composer lost its rules: {ruled:?}"
    );

    let shaded = composer_lines(
        r##"
            [colors]
            composer = "shaded"
            composer_bg = "#323232"
            [layout]
            prompt_padding = true
            composer_padding = 1
        "##,
    );
    let plain = shaded
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert!(
        !plain
            .iter()
            .any(|line| line.contains('─') || line.contains('╭') || line.contains('│')),
        "shaded composer must not draw rules or frames: {plain:?}"
    );
    assert_eq!(
        shaded
            .iter()
            .filter(|line| line.contains("\x1b[48;2;50;50;50m"))
            .count(),
        3,
        "shaded composer should paint exactly three rows"
    );
}

#[test]
fn transcript_and_composer_have_exactly_one_breathing_row() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 5);
    shell.on_prompt_submitted("question");
    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("answer".into()),
        )));
    let lines = render_shell(&shell.state.borrow(), 80)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    let composer = lines
        .iter()
        .position(|line| {
            !line.trim().is_empty() && line.trim().chars().all(|c| c == '─' || c == '-')
        })
        .expect("composer top rule");
    assert!(composer > 0);
    assert!(lines[composer - 1].is_empty());
    assert!(composer < 2 || !lines[composer - 2].is_empty());
}

#[test]
fn resize_defers_exactly_one_transcript_reflow_to_the_render_thread() {
    let mut shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for index in 0..512 {
            state.push_block(TranscriptBlock::Assistant(Box::new(
                AssistantBlock::finalized(format!(
                    "long stable answer {index} with enough words to wrap across widths"
                )),
            )));
        }
        let _ = state.rendered_transcript(100);
    }
    let generation = shell.state.borrow().transcript_cache.borrow().generation;
    shell.set_size(52, 20);
    {
        let state = shell.state.borrow();
        let cache = state.transcript_cache.borrow();
        assert_eq!(cache.generation, generation, "input thread must not reflow");
        assert_eq!(cache.width, None);
    }
    {
        let state = shell.state.borrow();
        let _ = state.rendered_transcript(52);
    }
    let state = shell.state.borrow();
    let cache = state.transcript_cache.borrow();
    assert_eq!(cache.generation, generation + 1);
    assert_eq!(cache.width, Some(52));
}

#[test]
fn slash_popup_keeps_selection_visible_across_paging_filtering_and_resize() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 14);
    shell.apply_edit(EditAction::Char('/'));
    shell.slash_menu(SlashMenuAction::Last);
    let last = commands::slash_suggestions("/").len() - 1;
    assert_eq!(shell.state.borrow().slash_selection, last);

    shell.set_size(34, 9);
    let resized = shell_chrome(&shell.state.borrow(), 34, Instant::now()).suggestions;
    let resized_plain = resized
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert!(resized_plain.iter().any(|line| line.contains("/quit")));
    assert!(resized_plain
        .iter()
        .any(|line| line.contains('›') && line.contains("/quit")));
    assert!(resized_plain.first().is_some_and(|line| line.contains('/')));
    assert!(resized.iter().all(|line| visible_width(line) <= 34));

    let page = resized.len().saturating_sub(1).max(1);
    shell.slash_menu(SlashMenuAction::PageUp);
    assert_eq!(
        shell.state.borrow().slash_selection,
        last.saturating_sub(page)
    );
    shell.slash_menu(SlashMenuAction::First);
    shell.slash_menu(SlashMenuAction::PageDown);
    assert_eq!(shell.state.borrow().slash_selection, page.min(last));

    shell.slash_menu(SlashMenuAction::Last);
    shell.apply_edit(EditAction::Char('m'));
    let state = shell.state.borrow();
    assert_eq!(state.editor, "/m");
    assert_eq!(state.slash_selection, 0);
    assert_eq!(state.slash_scroll, 0);
    drop(state);

    shell.set_size(1, 9);
    let narrow = render_slash_suggestions(&shell.state.borrow(), 1, 5);
    assert!(narrow.iter().all(|line| visible_width(line) <= 1));
}

#[test]
fn composer_border_stays_stable_when_draft_content_changes() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("anthropic", "claude-sonnet-4", "high");
    {
        let mut state = shell.state.borrow_mut();
        crate::tui::theme::apply_model_lab(&mut state.theme, ModelLab::Anthropic);
        state.model_lab = Some(ModelLab::Anthropic);
    }
    let now = Instant::now();
    let idle =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 60, now);
    shell.apply_edit(EditAction::Char('x'));
    let focused =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 60, now);

    assert_eq!(idle[0], focused[0]);
    let accent = {
        let state = shell.state.borrow();
        state
            .theme
            .model_rgb(Some(ModelLab::Anthropic))
            .expect("Anthropic model accent")
    };
    let accent = format!("38;2;{};{};{}", accent.0, accent.1, accent.2);
    assert!(focused[0].contains(&accent), "{:?}", focused[0]);
    assert!(focused[0].contains("38;2;169;99;76"), "{:?}", focused[0]);
    let plain_edge = strip_terminal_sequences(&idle[0]);
    assert!(
        !plain_edge.starts_with(' '),
        "full-width separator must reach the terminal edge: {plain_edge:?}"
    );
    assert_eq!(visible_width(&plain_edge), 60);
    let prompt = strip_terminal_sequences(&idle[1]);
    assert!(
        prompt.starts_with('›'),
        "composer content must use the shared grid: {prompt:?}"
    );
    let wide =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 120, now);
    assert_eq!(wide[0].matches("\x1b[38;2;").count(), 1);
    assert!(
        wide[0].len() < 450,
        "120-column uniform border encoded {} bytes",
        wide[0].len()
    );
    for edge in [
        &idle[0],
        &idle[idle.len() - 2],
        &focused[0],
        &focused[focused.len() - 2],
    ] {
        assert_eq!(
            edge.matches("\x1b[38;2;").count(),
            1,
            "uniform border reopened its RGB style per cell: {edge:?}"
        );
        assert!(
            edge.len() < 240,
            "uniform border encoded {} bytes",
            edge.len()
        );
    }
}

#[test]
fn explicit_theme_preserves_composer_and_code_chrome() {
    let theme = theme_with_layout("composer_padding = 2");
    assert!(theme.rich_renderer().options().code_borders);
    let shell = InteractiveShell::test_shell_with_theme(theme);
    let rendered = plain_composer_surface(&shell, 60, Instant::now());
    let is_rule = |line: &String| line.trim().chars().all(|c| c == '─' || c == '-');
    assert!(rendered.first().is_some_and(is_rule));
    assert!(rendered.get(1).is_some_and(
        |line| line.trim_start().starts_with('›') || line.trim_start().starts_with('>')
    ));
    assert!(
        rendered
            .get(1)
            .is_some_and(|line| !line.contains('│') && !line.contains('|')),
        "composer rows must not carry side borders"
    );
    assert!(rendered.get(2).is_some_and(is_rule));
}

fn theme_with_layout(layout: &str) -> YggTheme {
    crate::tui::theme::test_theme_from_source(&format!("[layout]\n{layout}"))
}

#[test]
fn theme_density_and_transcript_inset_change_semantic_block_geometry() {
    let previous = TranscriptBlock::Notice("previous".into());
    let current = TranscriptBlock::Notice("current".into());
    let render = |density: &str, inset: u16| {
        let theme = theme_with_layout(&format!(
            "density = \"{density}\"\ntranscript_inset = {inset}"
        ));
        let renderer = theme.rich_renderer();
        render_block(
            Some(&previous),
            &current,
            &theme,
            &renderer,
            &renderer,
            80,
            false,
        )
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
    };

    let compact = render("compact", 1);
    let comfortable = render("comfortable", 2);
    let airy = render("airy", 4);
    assert_eq!(compact.iter().take_while(|line| line.is_empty()).count(), 0);
    assert_eq!(
        comfortable
            .iter()
            .take_while(|line| line.is_empty())
            .count(),
        1
    );
    assert_eq!(airy.iter().take_while(|line| line.is_empty()).count(), 2);
    let dot = if theme_with_layout("").unicode() {
        "•"
    } else {
        "*"
    };
    assert!(compact[0].starts_with(&format!("{dot} current")));
    assert!(comfortable[1].starts_with(&format!("{dot} current")));
    assert!(airy[2].starts_with(&format!("  {dot} current")));

    let hidden_theme = theme_with_layout(
        "density = \"airy\"\nshow_reasoning = false\nnarrow_show_reasoning = false",
    );
    let hidden_renderer = hidden_theme.rich_renderer();
    let hidden_reasoning = TranscriptBlock::Reasoning(Box::new(
        AssistantBlock::finalized_reasoning("hidden".into()),
    ));
    let collapsed_reasoning = render_block(
        None,
        &hidden_reasoning,
        &hidden_theme,
        &hidden_renderer,
        &hidden_renderer,
        80,
        false,
    );
    assert_eq!(
        collapsed_reasoning.len(),
        0,
        "finished reasoning produces no collapsed lines when hidden: {collapsed_reasoning:?}"
    );
    let first_visible = render_block(
        Some(&hidden_reasoning),
        &current,
        &hidden_theme,
        &hidden_renderer,
        &hidden_renderer,
        80,
        false,
    );
    assert_eq!(
        first_visible
            .iter()
            .take_while(|line| line.is_empty())
            .count(),
        2
    );
}

#[test]
fn layout_breakpoint_is_resolved_from_terminal_width_before_inset() {
    let theme = theme_with_layout(
        r#"
                transcript_inset = 4
                narrow_breakpoint = 72
                show_reasoning = true
                narrow_show_reasoning = false
            "#,
    );
    let at_breakpoint = theme.layout_for_width(72);
    let below_breakpoint = theme.layout_for_width(71);
    assert!(
        !at_breakpoint.narrow && at_breakpoint.show_reasoning,
        "width == breakpoint stays on the wide layout"
    );
    assert!(
        below_breakpoint.narrow && !below_breakpoint.show_reasoning,
        "narrow fallbacks apply below the breakpoint before any inset"
    );
}

#[test]
fn selection_mapping_excludes_density_rows_and_transcript_inset() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_theme(theme_with_layout(
        "density = \"airy\"\ntranscript_inset = 4",
    ));
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("alpha".into()),
        )));
        state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("bravo".into()),
        )));
    }
    let second_start = {
        let state = shell.state.borrow();
        let _ = state.rendered_transcript(80);
        let second_start = state.transcript_cache.borrow().block_starts[1];
        second_start
    };
    assert!(selection_position_for_visual_cell(&shell.state.borrow(), second_start, 2).is_none());
    let start = selection_position_for_visual_cell(&shell.state.borrow(), second_start + 2, 4)
        .expect("first content cell should map");
    assert_eq!(start.block, 1);
    assert_eq!(start.offset, 0);
    let two_cells = selection_position_for_visual_cell(&shell.state.borrow(), second_start + 2, 6)
        .expect("content cell should map");
    assert_eq!(two_cells.offset, 2);
}

const SURFACE_TEST_THEME: &str = r##"
        [metadata]
        name = "Surface fixture"
        adaptive = false

        [roles."surface.user"]
        foreground = "default"
        background = "#112233"
        [roles."surface.user.border"]
        foreground = "#6688aa"
        [roles."surface.user.label"]
        foreground = "#99ccff"
        bold = true

        [roles."surface.assistant"]
        foreground = "default"
        background = "#221133"
        [roles."surface.assistant.border"]
        foreground = "#9966bb"
        [roles."surface.assistant.label"]
        foreground = "#ddbbff"
        bold = true

        [surfaces.user]
        chrome = "card"
        heading = "tab"
        label = "INPUT"
        padding = 1
        width = "full"
        narrow_chrome = "rail"
        narrow_heading = "none"
        narrow_padding = 0

        [surfaces.assistant]
        chrome = "card"
        heading = "overline"
        label = "RESPONSE"
        padding = 1
        width = "full"
        narrow_chrome = "plain"
        narrow_heading = "none"
        narrow_padding = 0

        [glyphs]
        top_left = "╭"
        top_right = "╮"
        bottom_left = "╰"
        bottom_right = "╯"
        horizontal = "─"
        vertical = "│"
        rail = "┃"
        prompt = "›"

        [glyphs_ascii]
        top_left = "+"
        top_right = "+"
        bottom_left = "+"
        bottom_right = "+"
        horizontal = "-"
        vertical = "|"
        rail = "|"
        prompt = ">"

        [layout]
        density = "compact"
        transcript_inset = 1
        narrow_breakpoint = 60
    "##;

#[test]
fn card_geometry_keeps_prompt_identity_and_decorations_out_of_selection() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_theme(crate::tui::theme::test_theme_from_source(
        SURFACE_TEST_THEME,
    ));
    {
        let mut state = shell.state.borrow_mut();
        state.push_block(TranscriptBlock::User {
            text: "hello surface".into(),
            model_lab: Some(ModelLab::Alibaba),
            prompt_color: Some("#ff7018".into()),
            persisted: true,
        });
    }
    let (start, length, geometry, rows) = {
        let state = shell.state.borrow();
        let rows = state.rendered_transcript(80).clone();
        let cache = state.transcript_cache.borrow();
        (
            cache.block_starts[0],
            cache.block_lengths[0],
            cache.block_geometries[0],
            rows,
        )
    };
    assert_eq!(geometry.leading_rows, 2);
    assert_eq!(geometry.trailing_rows, 2);
    assert!(selection_position_for_visual_cell(&shell.state.borrow(), start, 0).is_none());
    assert!(
        selection_position_for_visual_cell(&shell.state.borrow(), start + length - 1, 79,)
            .is_none()
    );

    let body_row = start + geometry.transition_rows + geometry.leading_rows;
    let first = selection_position_for_visual_cell(
        &shell.state.borrow(),
        body_row,
        geometry.content_left + 2,
    )
    .expect("first prompt text cell");
    assert_eq!(first.offset, 0);
    let second = selection_position_for_visual_cell(
        &shell.state.borrow(),
        body_row,
        geometry.content_left + 3,
    )
    .expect("second prompt text cell");
    assert_eq!(second.offset, 1);
    let row_start = selection_position_for_visual_cell(&shell.state.borrow(), body_row, 0)
        .expect("left card margin should select the row start");
    let row_end = selection_position_for_visual_cell(&shell.state.borrow(), body_row, 79)
        .expect("right card margin should select the row end");
    assert_eq!(row_start.offset, 0);
    assert_eq!(row_end.offset, "hello surface".len());

    let body = &rows[body_row];
    assert!(
        strip_terminal_sequences(body).contains("› hello surface"),
        "{body:?}"
    );
    assert!(
        body.contains("\x1b[48;2;255;112;24m"),
        "the prompt card must retain its exact stored model background: {body:?}"
    );
    assert!(
        !body.contains("\x1b[48;2;17;34;51m"),
        "the neutral surface colour must not replace model provenance: {body:?}"
    );
    assert!(
        body.ends_with("\x1b[0m"),
        "surface background leaked: {body:?}"
    );

    {
        let mut state = shell.state.borrow_mut();
        state.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                block: 0,
                offset: 1,
                trailing_affinity: false,
            },
            focus: TranscriptPosition {
                block: 0,
                offset: 5,
                trailing_affinity: false,
            },
        });
        assert!(state.copy_buffer.is_none());
    }
    assert_eq!(shell.selected_plain_text().as_deref(), Some("ello"));
    assert!(shell.state.borrow().copy_buffer.is_none());
}

#[test]
fn card_surface_degrades_to_rail_with_exact_cached_narrow_geometry() {
    let theme = crate::tui::theme::test_theme_from_source(SURFACE_TEST_THEME);
    let block = TranscriptBlock::User {
        text: "narrow request".into(),
        model_lab: None,
        prompt_color: None,
        persisted: true,
    };
    let wide = compile_surface_plan(None, &block, &theme, 80);
    assert_eq!(wide.chrome, ThemeSurfaceChrome::Card);
    assert_eq!(wide.geometry.leading_rows, 2);
    assert_eq!(wide.geometry.trailing_rows, 2);

    let narrow = compile_surface_plan(None, &block, &theme, 40);
    assert_eq!(narrow.chrome, ThemeSurfaceChrome::Rail);
    assert_eq!(narrow.heading, ThemeSurfaceHeading::None);
    assert_eq!(narrow.geometry.leading_rows, 0);
    assert_eq!(narrow.geometry.trailing_rows, 0);
    let renderer = theme.rich_renderer();
    let rendered =
        render_block_planned(None, &block, &theme, &renderer, &renderer, 40, false, 0, 0);
    assert_eq!(rendered.geometry, narrow.geometry);
    let plain = rendered
        .lines
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert!(
        plain.first().is_some_and(|line| line.contains("┃")),
        "{plain:?}"
    );
    assert!(plain
        .iter()
        .all(|line| !line.contains('╭') && !line.contains('╰')));
}

#[test]
fn card_background_and_glyphs_degrade_across_terminal_capabilities() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let block = TranscriptBlock::Assistant(Box::new(AssistantBlock::finalized(
        "# Result\n\n```rust\nlet answer = 42;\n```".into(),
    )));
    let render = |capabilities, background| {
        let theme =
            crate::tui::theme::test_theme_source_with(SURFACE_TEST_THEME, capabilities, background);
        let renderer = theme.rich_renderer();
        render_block(None, &block, &theme, &renderer, &renderer, 72, false)
    };

    let truecolor = render(
        TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
        TerminalBackground::Dark,
    );
    assert!(truecolor
        .iter()
        .any(|line| line.contains("\x1b[48;2;34;17;51m")));
    assert!(truecolor
        .iter()
        .filter(|line| !line.is_empty())
        .all(|line| line.ends_with("\x1b[0m")));

    let ansi = render(
        TerminalCapabilities::test(true, false, ColorDepth::Ansi16),
        TerminalBackground::Dark,
    );
    assert!(ansi
        .iter()
        .any(|line| line.contains("\x1b[4") || line.contains("\x1b[10")));
    assert!(ansi.iter().all(|line| !line.contains("48;2")));
    let ansi_plain = ansi
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>();
    assert!(ansi_plain
        .first()
        .is_some_and(|line| line.trim_start().ends_with("+")));

    let no_color = render(
        TerminalCapabilities::test(false, false, ColorDepth::None),
        TerminalBackground::Dark,
    );
    assert!(no_color.iter().all(|line| !line.contains('\x1b')));
    assert!(no_color
        .first()
        .is_some_and(|line| line.trim_start().ends_with("+")));

    let adaptive_source = SURFACE_TEST_THEME.replace("adaptive = false", "adaptive = true");
    let unknown = crate::tui::theme::test_theme_source_with(
        &adaptive_source,
        TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
        TerminalBackground::Unknown,
    );
    let renderer = unknown.rich_renderer();
    let unknown = render_block(None, &block, &unknown, &renderer, &renderer, 72, false);
    assert!(
        unknown.iter().any(|line| line.contains("\x1b[48;")),
        "unknown terminal backgrounds must retain adaptive surfaces"
    );
}

#[test]
fn theme_header_footer_status_and_composer_padding_have_narrow_fallbacks() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 20);
    shell.set_identity("local", "qwen3.6-27b", "high");
    shell.set_theme(theme_with_layout(
        r#"
                show_header = true
                show_footer = false
                show_status_line = false
                composer_padding = 3
                narrow_breakpoint = 50
                narrow_show_header = false
                narrow_show_footer = false
                narrow_show_status_line = false
            "#,
    ));

    let now = Instant::now();
    let composer = plain_composer_surface(&shell, 80, now);
    assert_eq!(composer.len(), 3, "hidden footer leaves only the frame");
    assert!(composer[1].trim_start().starts_with('›'), "{composer:?}");
    assert!(composer[1].starts_with("  "), "{composer:?}");
    assert_eq!(visible_width(&composer[0]), 80);
    assert_eq!(visible_width(&composer[1]), 78);
    assert_eq!(visible_width(&composer[2]), 80);

    let wide_header = shell_chrome(&shell.state.borrow(), 80, now).header;
    assert_eq!(wide_header.len(), 1);
    let wide_header = strip_terminal_sequences(&wide_header[0]);
    assert!(wide_header.contains("ygg"));
    assert!(
        wide_header.contains("local / Qwen3.6 27B"),
        "{wide_header:?}"
    );
    assert!(shell_chrome(&shell.state.borrow(), 40, now)
        .header
        .is_empty());
    let narrow_composer = plain_composer_surface(&shell, 40, now);
    assert_eq!(
        narrow_composer.len(),
        3,
        "extensions cannot force persistent header or footer chrome"
    );
}

#[test]
fn read_only_document_panel_scrolls_and_returns_to_its_owner() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(60, 14);
    shell.open_panel(Panel::ReadOnlyDocument {
        title: "worker · read-only transcript".into(),
        text: (0..30)
            .map(|line| format!("transcript line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
        styled: false,
        scroll_from_bottom: 0,
    });

    let initial = render_panel(&shell.state.borrow(), 60)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(initial.contains("transcript line 29"), "{initial}");
    assert!(!initial.contains("transcript line 00"), "{initial}");

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Home));
    let top = render_panel(&shell.state.borrow(), 60)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(top.contains("transcript line 00"), "{top}");
    assert!(!top.contains("transcript line 29"), "{top}");

    shell.update_read_only_document(
        (0..45)
            .map(|line| format!("transcript line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let refreshed_top = render_panel(&shell.state.borrow(), 60)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        refreshed_top.contains("transcript line 00"),
        "{refreshed_top}"
    );
    assert!(
        !refreshed_top.contains("transcript line 44"),
        "{refreshed_top}"
    );

    assert!(matches!(
        shell.panel_input(&panel_key(crossterm::event::KeyCode::Left)),
        Some((PanelResult::Cancel, PanelAction::ReadOnlyDocument))
    ));
    assert!(!shell.has_panel());
}

#[test]
fn read_only_document_home_reaches_top_with_wrapped_error_and_header_chrome() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(42, 16);
    shell.set_identity("local", "model", "high");
    shell.error(
        "a wrapped error consumes several rows before the focused document panel can render"
            .repeat(3),
    );
    shell.open_panel(Panel::ReadOnlyDocument {
        title: "worker · read-only transcript".into(),
        text: (0..40)
            .map(|line| format!("document row {line:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
        styled: false,
        scroll_from_bottom: 0,
    });

    shell.panel_input(&panel_key(crossterm::event::KeyCode::Home));
    let rendered = shell_chrome(&shell.state.borrow(), 42, Instant::now())
        .panel
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("document row 00"), "{rendered}");
}

#[test]
fn panel_border_layout_degrades_to_unframed_narrow_picker() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 20);
    shell.set_theme(theme_with_layout(
        r#"
                show_panel_borders = true
                narrow_breakpoint = 60
                narrow_show_panel_borders = false
            "#,
    ));
    open_select_panel(&mut shell, &["alpha", "beta", "gamma"]);

    let wide = render_panel(&shell.state.borrow(), 80)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    assert_eq!(wide.len(), 8);
    assert!(wide
        .first()
        .is_some_and(|line| line.chars().all(|ch| ch == '─')));
    assert!(wide
        .last()
        .is_some_and(|line| line.chars().all(|ch| ch == '─')));

    let narrow = render_panel(&shell.state.borrow(), 40)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>();
    assert_eq!(narrow.len(), 6);
    assert!(narrow
        .first()
        .is_some_and(|line| line.contains("Select model")));
    assert!(narrow.iter().all(|line| !line.chars().all(|ch| ch == '─')));
}

fn populate_theme_fixture(shell: &mut InteractiveShell) {
    shell.set_identity("local", "qwen3.6-27b", "high");
    let mut state = shell.state.borrow_mut();
    state.push_block(TranscriptBlock::User {
        text: "Review `src/lib.rs` and keep the public API stable.".into(),
        model_lab: Some(ModelLab::Alibaba),
        prompt_color: Some("#ff7018".into()),
        persisted: true,
    });
    state.push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized(
                "# Patch plan\n\nKeep the change **small** and verify it.\n\n```rust\nfn answer() -> u8 { 42 }\n```"
                    .into(),
            ),
        )));
    state.push_block(TranscriptBlock::Reasoning(Box::new(
        AssistantBlock::finalized_reasoning(
            "Checking ownership, invariants, and the narrow fallback.".into(),
        ),
    )));
    let args = serde_json::json!({"path": "src/lib.rs"});
    state.push_block(TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("fixture-edit".into()),
        "edit".into(),
        args.to_string(),
        summarize_tool("edit", &args),
        "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new".into(),
        true,
        false,
        None,
        Some(ModelLab::Alibaba),
    ))));
    state.push_block(TranscriptBlock::Shell(Box::new(ShellOutput {
        id: "fixture-shell".into(),
        command: "cargo test -p ygg-coding-agent".into(),
        output: "test result: ok. 386 passed".into(),
        exit_code: 0,
        running: false,
    })));
    state.push_block(TranscriptBlock::Notice(
        "Extension reloaded with one status contribution.".into(),
    ));
    state.push_block(TranscriptBlock::Outcome(OutcomeBlock::new(
        RunOutcome::Completed {
            elapsed: Duration::from_millis(13700),
            summary: crate::presentation::RunSummary {
                files_changed: 1,
                tool_calls: 2,
                warnings: 0,
            },
        },
        None,
    )));
    state.editor = "draft a local patch".into();
}

fn ansi_background_is_open_at_end(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut background_open = false;
    while index + 2 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let Some(relative_end) = bytes[index + 2..].iter().position(|byte| *byte == b'm') else {
            break;
        };
        let end = index + 2 + relative_end;
        let parameters = std::str::from_utf8(&bytes[index + 2..end]).unwrap_or("");
        if parameters.is_empty() {
            background_open = false;
        } else {
            let parameters = parameters
                .split(';')
                .filter_map(|value| value.parse::<u16>().ok())
                .collect::<Vec<_>>();
            let mut parameter = 0;
            while parameter < parameters.len() {
                match parameters[parameter] {
                    0 | 49 => background_open = false,
                    40..=47 | 100..=107 => background_open = true,
                    38 | 48 if parameters.get(parameter + 1) == Some(&2) => {
                        if parameters[parameter] == 48 {
                            background_open = true;
                        }
                        parameter = parameter.saturating_add(4);
                    }
                    38 | 48 if parameters.get(parameter + 1) == Some(&5) => {
                        if parameters[parameter] == 48 {
                            background_open = true;
                        }
                        parameter = parameter.saturating_add(2);
                    }
                    _ => {}
                }
                parameter = parameter.saturating_add(1);
            }
        }
        index = end + 1;
    }
    background_open
}

#[test]
fn custom_theme_keeps_safe_transcript_geometry_across_color_and_width_profiles() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let fixed_surface_theme = format!("{SURFACE_TEST_THEME}\n[model]\nuse_lab_color = false\n");
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(96, 80);
    shell.set_theme(crate::tui::theme::test_theme_source_with(
        &fixed_surface_theme,
        TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
        TerminalBackground::Dark,
    ));
    populate_theme_fixture(&mut shell);
    let transcript = shell.state.borrow().rendered_transcript(96).join("\n");
    assert!(
        !transcript.contains("\x1b[48;2;255;112;24m"),
        "custom theme leaked the default model-adaptive provenance paint"
    );
    assert!(
        !transcript.contains("\x1b[38;2;255;112;24m"),
        "custom theme rendered provenance as foreground-only"
    );
    let unclosed_backgrounds = transcript
        .lines()
        .filter(|line| ansi_background_is_open_at_end(line))
        .collect::<Vec<_>>();
    assert!(
        unclosed_backgrounds.is_empty(),
        "custom theme leaked a painted surface beyond its row: {unclosed_backgrounds:?}"
    );

    let mut plain_shell = InteractiveShell::test_shell();
    plain_shell.set_size(96, 80);
    plain_shell.set_theme(crate::tui::theme::test_theme_source_with(
        &fixed_surface_theme,
        TerminalCapabilities::test(false, false, ColorDepth::None),
        TerminalBackground::Dark,
    ));
    populate_theme_fixture(&mut plain_shell);
    let plain = plain_shell
        .state
        .borrow()
        .rendered_transcript(96)
        .join("\n");
    assert!(
        !plain.contains('\x1b'),
        "custom theme emitted ANSI in no-color mode"
    );

    let mut narrow_shell = InteractiveShell::test_shell();
    narrow_shell.set_size(40, 80);
    narrow_shell.set_theme(crate::tui::theme::test_theme_source_with(
        &fixed_surface_theme,
        TerminalCapabilities::test(false, false, ColorDepth::None),
        TerminalBackground::Dark,
    ));
    populate_theme_fixture(&mut narrow_shell);
    let narrow_frame = narrow_shell
        .state
        .borrow()
        .rendered_transcript(40)
        .join("\n");
    assert!(
        narrow_frame.lines().all(|line| visible_width(line) <= 40),
        "custom theme overflowed a narrow terminal"
    );

    if std::env::var_os("YGG_DUMP_THEME_FRAMES").is_some() {
        eprintln!(
            "\n===== custom / wide =====\n{}",
            strip_terminal_sequences(&transcript)
        );
        eprintln!("\n===== custom / narrow =====\n{narrow_frame}");
    }
}

#[test]
fn presentation_contract_renders_short_regular_and_wide_frames() {
    for (label, width, height) in [("short", 46, 8), ("regular", 80, 24), ("wide", 120, 40)] {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(width, height);
        populate_theme_fixture(&mut shell);

        let frame = render_shell(&shell.state.borrow(), width);
        assert!(!frame.is_empty(), "{label} frame is empty");
        assert!(
            frame
                .iter()
                .all(|line| visible_width(line) <= usize::from(width)),
            "{label} frame overflowed {width} columns: {frame:?}"
        );
        let plain = frame
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert!(
            plain.iter().any(|line| line.contains("Review src/lib.rs")),
            "{label} frame lost the durable prompt: {plain:?}"
        );
        assert!(
            plain
                .iter()
                .any(|line| line.contains("draft a local patch")),
            "{label} frame lost the composer draft: {plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.contains("completed")),
            "{label} frame lost the terminal outcome: {plain:?}"
        );
    }
}

#[test]
fn styled_read_only_document_preserves_trusted_ansi_and_sanitizes_plain_documents() {
    let shell = InteractiveShell::test_shell();
    let theme = crate::tui::theme::test_theme();
    let styled_text = format!(
        "{} {}",
        theme.bold(&theme.fg("foreground", "Worker")),
        "\x1b[31mraw-esc\x1b[0m"
    );
    // Styled documents keep theme styling but the producer had already
    // sanitized content; rendering must not re-sanitize away the bold.
    let lines = crate::tui::view::panel_render_test_hook::document_lines(&styled_text, 80, true);
    assert!(
        lines.iter().any(|line| line.contains("\x1b[1m")),
        "styled document lost its trusted ANSI: {lines:?}"
    );
    // Plain documents still sanitize embedded escapes.
    let lines =
        crate::tui::view::panel_render_test_hook::document_lines("before \x1b[31mafter", 80, false);
    assert!(
        lines.iter().all(|line| !line.contains("\x1b")),
        "plain document kept a raw escape: {lines:?}"
    );
    let _ = shell;
}
