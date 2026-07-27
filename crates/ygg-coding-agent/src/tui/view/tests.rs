use std::collections::HashSet;

use super::*;
use crate::commands;

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
        if lines < 0 {
            self.push(format!("\x1b[{}A", lines.unsigned_abs()).as_bytes());
        } else if lines > 0 {
            self.push(format!("\x1b[{}B", lines.unsigned_abs()).as_bytes());
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
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let size = Arc::new(Mutex::new((width, height)));
    let state = SharedState::new(ShellState {
        theme,
        size: (width, height),
        follow_tail: true,
        ..ShellState::default()
    });
    let mut tui = TUI::new(Box::new(EmulatedTerminal {
        size: size.clone(),
        bytes: bytes.clone(),
        synchronized_output,
    }));
    tui.set_inline_scrollback(true);
    tui.add_child(Box::new(ShellComponent {
        state: state.clone(),
        frame: RefCell::new(ShellFrameState::default()),
        application_viewport: false,
    }));
    tui.start();
    (
        InteractiveShell {
            tui: Some(tui),
            state,
            size,
            render_tx: None,
            render_thread: None,
            theme_config: None,
            capture_mouse: false,
        },
        bytes,
    )
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
        title: "Select model".into(),
        items: items.iter().map(|item| item.to_string()).collect(),
        descriptions: vec![None; items.len()],
        selected: 0,
        filter: String::new(),
        action: PanelAction::SelectTheme(vec![]),
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
fn select_list_filter_is_case_insensitive_and_matches_descriptions() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 24);
    shell.open_panel(Panel::SelectList {
        title: "Select model".into(),
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
    assert!(rendered.contains("No matches for"));

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
fn select_list_aligns_muted_metadata_and_drops_it_before_narrow_labels() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(100, 24);
    shell.open_panel(Panel::SelectList {
        title: "Select model".into(),
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
    let selected = wide
        .iter()
        .find(|line| line.contains("Claude Opus"))
        .expect("selected model should render");
    assert!(selected.trim_start().starts_with('›') || selected.trim_start().starts_with('>'));

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
        title: "Select model".into(),
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
fn overlay_truecolor_does_not_leak_a_background_to_following_rows() {
    let theme = crate::tui::theme::test_theme();
    let selected = theme.fg("accent", "selected");
    // The universal Ygg green includes RGB channel 107. It must remain an
    // RGB component rather than becoming a bright-white background SGR.
    assert!(selected.contains(";107m"));

    let wrapped = wrap_overlay_text(&format!("{selected}\nnext row"), 80);
    assert_eq!(wrapped.len(), 2);
    assert!(wrapped[0].contains("selected"));
    assert!(wrapped[1].contains("next row"));
    assert!(!wrapped[1].contains("\x1b[107m"));
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
fn welcome_card_shows_the_current_package_version() {
    let shell = InteractiveShell::test_shell();
    shell.state.borrow_mut().startup_card_started_at = Some(Instant::now());
    let rendered = render_welcome_card(&shell.state.borrow(), 80, 10, Instant::now()).join("\n");
    let rendered = strip_terminal_sequences(&rendered);
    assert!(
        rendered.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
        "{rendered}"
    );
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
    assert!(!popup.contains("/help"));
    assert!(popup.contains("› /new"));

    shell.slash_menu(SlashMenuAction::Last);
    let scrolled = render_slash_suggestions(&shell.state.borrow(), 80, 7).join("\n");
    assert!(scrolled.contains("/quit"), "{scrolled}");
    assert!(scrolled.contains('/'), "{scrolled}");

    shell.slash_menu(SlashMenuAction::First);
    shell.slash_menu(SlashMenuAction::Next);
    shell.slash_menu(SlashMenuAction::Select);
    assert_eq!(shell.pending(), "/resume ");
    assert!(!shell.slash_popup_open());

    shell.drain_editor();
    shell.apply_edit(EditAction::Char('/'));
    for character in "mod".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    shell.complete_slash_command();
    assert_eq!(shell.pending(), "/model ");
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
    let extension_names = state
        .extension_commands
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<HashSet<_>>();
    for suggestion in suggestions.iter().filter(|suggestion| {
        suggestion.description.starts_with("prompt ·")
            || suggestion.description.starts_with("extension ·")
    }) {
        let registered = if suggestion.description.starts_with("prompt ·") {
            prompt_names.contains(suggestion.name.as_str())
        } else {
            extension_names.contains(suggestion.name.as_str())
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
        suggestion.name == "local-review" && suggestion.description.starts_with("prompt ·")
    }));
    assert!(suggestions.iter().any(|suggestion| {
        suggestion.name == "checkpoint" && suggestion.description.starts_with("extension ·")
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
        .any(|line| line.contains("project files · tab completes")));
    assert!(rendered.iter().any(|line| line.contains("src/main.rs")));
    shell.complete_mention();
    assert_eq!(shell.pending(), "see @src/main.rs ");
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
    shell.complete_mention();
    assert_eq!(shell.pending(), "[Image #1]");
    let composed = shell.drain_composed();
    assert!(composed
        .parts
        .iter()
        .any(|part| matches!(part, ygg_agent::InputPart::Media(_))));
}

#[test]
fn set_workspace_keeps_the_file_index_when_the_root_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), b"x").unwrap();

    let mut shell = InteractiveShell::test_shell();
    shell.set_workspace(dir.path().to_path_buf());
    for character in "@a".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    assert!(shell.state.borrow().file_index.is_some());

    // Re-asserting the same root (update_status runs after every turn)
    // must not drop the lazily built index and force a workspace re-walk.
    shell.set_workspace(dir.path().to_path_buf());
    assert!(shell.state.borrow().file_index.is_some());

    // A genuinely different root invalidates it.
    let other = tempfile::tempdir().unwrap();
    shell.set_workspace(other.path().to_path_buf());
    assert!(shell.state.borrow().file_index.is_none());
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
    shell.complete_mention();

    assert_eq!(shell.pending(), "@shot.png ");
    assert!(shell
        .debug_snapshot()
        .contains("does not accept image input"));
}

#[test]
fn output_token_rate_uses_authoritative_usage_and_generation_elapsed_time() {
    assert_eq!(
        output_tokens_per_second(120, Duration::from_secs(2)),
        Some(60.0)
    );
    assert!(output_tokens_per_second(1, Duration::from_millis(250))
        .is_some_and(|rate| (rate - 4.0).abs() < f64::EPSILON));
    assert_eq!(output_tokens_per_second(0, Duration::from_secs(1)), None);
    assert_eq!(output_tokens_per_second(1, Duration::ZERO), None);
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
        strip_terminal_sequences(footer).contains("900.0k/967.2k"),
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
    let rendered = shell
        .state
        .borrow()
        .rendered_transcript(80)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains("LIVE OUTPUT 1"), "{rendered}");
    assert!(rendered.contains("LIVE OUTPUT 4"), "{rendered}");
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
        .position(|line| line.contains("Steering prompts · 2 queued"))
        .expect("steering queue");
    assert!(queue < prompt);
    assert!(plain
        .iter()
        .any(|line| line.starts_with("    ↳ check the docs")));
    assert!(plain
        .iter()
        .any(|line| line.starts_with("    ↳ then run the tests")));

    shell.on_agent_event(&AgentEvent::SteeringDelivered {
        messages: vec!["check the docs".into(), "then run the tests".into()],
    });
    let snapshot = shell.debug_snapshot();
    assert!(snapshot.contains("check the docs"));
    assert!(snapshot.contains("then run the tests"));
    assert!(!render_shell(&shell.state.borrow(), 120)
        .iter()
        .any(|line| line.contains("Steering prompts")));
}

#[test]
fn terminal_native_prompt_wraps_and_shrinks_without_a_panel() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_size(24, 10);
    for character in "abcdefghijklmnopqrstuvwxyz0123456789".chars() {
        shell.apply_edit(EditAction::Char(character));
    }

    let rendered = render_prompt_box(&shell.state.borrow(), 24, 8);
    assert!(rendered.len() > 1, "long input should grow the editor");
    assert!(rendered.iter().all(|line| visible_width(line) <= 24));
    assert!(rendered.iter().any(|line| line.contains(CURSOR_MARKER)));
    assert!(!rendered.iter().any(|line| {
        line.chars()
            .any(|character| matches!(character, '┏' | '┓' | '┗' | '┛'))
    }));

    shell.drain_editor();
    let rendered = render_prompt_box(&shell.state.borrow(), 24, 8);
    assert_eq!(rendered.len(), 1, "empty editor should shrink to one row");
    assert!(rendered[0].contains('›'));
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
fn resumed_history_is_tail_first_and_materializes_when_scrolling_past_it() {
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

    let mut shell = InteractiveShell::test_shell();
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

    let mut shell = InteractiveShell::test_shell();
    shell.set_size(80, 12);
    shell.hydrate(&session).unwrap();
    assert!(shell.state.borrow().deferred_session_history.is_some());

    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Outcome(RunOutcome::Completed {
            elapsed: Duration::from_secs(1),
            summary: crate::presentation::RunSummary {
                files_changed: 0,
                tool_calls: 0,
                warnings: 0,
            },
        }));
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
fn resize_materializes_deferred_history_during_an_active_stream() {
    const WIDTH: u16 = 80;
    const RESIZED_WIDTH: u16 = 96;
    const HEIGHT: u16 = 12;

    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("active-resize-session.jsonl"),
        "active resize prompt",
        100,
    );
    let (mut shell, bytes) = emulated_shell(crate::tui::theme::test_theme(), WIDTH, HEIGHT);
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
        assert!(state.deferred_session_history.is_none());
        assert!(state.run.is_active());
        assert!(
            state.transcript.iter().any(|block| matches!(
                block,
                TranscriptBlock::User { text, .. } if text == "active resize prompt 0"
            )),
            "resize must materialize the complete immutable branch"
        );
        assert!(state
            .transcript_commit_ids
            .windows(2)
            .all(|ids| ids[0] < ids[1]));
        let index = state.active_text.expect("remapped assistant stream");
        assert!(index > active_index_before);
        assert_eq!(state.transcript_commit_ids[index], active_commit_id);
        index
    };

    shell.render();
    let resize = String::from_utf8_lossy(&drain(&bytes)).into_owned();
    assert!(resize.contains("\x1b[2J\x1b[H\x1b[3J"), "{resize:?}");
    assert!(resize.contains("active resize prompt 0"), "{resize:?}");
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
fn delayed_resize_reconciliation_materializes_deferred_history() {
    let directory = tempfile::tempdir().unwrap();
    let session = session_with_user_prompts(
        &directory.path().join("reconciled-resize-session.jsonl"),
        "reconciled resize prompt",
        100,
    );
    let mut shell = InteractiveShell::test_shell();
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
    assert!(state.deferred_session_history.is_none());
    assert!(state.transcript.iter().any(|block| matches!(
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
    let mut shell = InteractiveShell::test_shell();
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
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: ToolCallId("duplicate".into()),
                    name: "read".into(),
                    arguments_json: r#"{"path":"second"}"#.into(),
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
    assert!(visible.contains("ctrl+o to expand"), "{visible:?}");
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

    shell.expand_focused_tool();
    let expanded = plain(&shell);
    assert!(expanded.contains("Grounded summary"), "{expanded}");
    assert!(expanded.contains("summary sentinel"), "{expanded}");
    assert!(expanded.contains("ctrl+o to collapse"), "{expanded}");
    assert!(!shell.has_overlay(), "compaction must expand inline");

    shell.expand_focused_tool();
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

    shell.on_run_event(
        run_id,
        &AgentEvent::CompactionFinished {
            reason: ygg_agent::CompactionReason::Threshold,
            result: Ok(ygg_agent::CompactionInfo {
                kind: ygg_agent::CompactionKind::Local,
                summary: "# Automatic summary\n\nauto-summary sentinel".into(),
                first_kept: ygg_agent::EntryId("kept".into()),
            }),
        },
    );
    let collapsed =
        strip_terminal_sequences(&shell.state.borrow().rendered_transcript(80).join("\n"));
    assert!(
        collapsed.contains("Context compacted automatically"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("auto-summary sentinel"), "{collapsed}");
    shell.expand_focused_tool();
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

    shell.expand_focused_tool();
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

        shell.expand_focused_tool();
        shell.render();
        let expansion = drain(&bytes);
        let expansion_text = String::from_utf8_lossy(&expansion);
        assert!(
            expansion.len() < 8 * 1024,
            "compaction expansion replayed an unbounded frame ({} bytes)",
            expansion.len()
        );
        assert!(
            !expansion
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "compaction expansion cleared terminal-owned history: {expansion_text:?}"
        );
        assert!(
            !expansion_text.contains("compaction-history-00"),
            "compaction expansion replayed committed history: {expansion_text:?}"
        );

        terminal.process(&expansion);
        terminal.set_scrollback(0);
        let visible = terminal.screen().contents();
        assert!(visible.contains("compaction-detail-39"), "{visible}");
        assert!(visible.contains("│ ›"), "composer disappeared: {visible}");

        shell.expand_focused_tool();
        shell.render();
        let collapse = drain(&bytes);
        assert!(
            collapse.len() < 8 * 1024,
            "compaction collapse replayed an unbounded frame ({} bytes)",
            collapse.len()
        );
        assert!(
            !collapse
                .windows(b"\x1b[3J".len())
                .any(|bytes| bytes == b"\x1b[3J"),
            "compaction collapse cleared terminal-owned history"
        );
        terminal.process(&collapse);
        terminal.set_scrollback(0);
        let collapsed = terminal.screen().contents();
        assert!(collapsed.contains("ctrl+o to view"), "{collapsed}");
        assert!(!collapsed.contains("compaction-detail-"), "{collapsed}");
        assert!(
            collapsed.contains("│ ›"),
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
fn resize_discards_pre_ygg_history_and_replays_all_owned_rows() {
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
        "{resize_text:?}"
    );
    assert!(
        resize_text.contains("YGG-OWNED-RESIZE-00"),
        "{resize_text:?}"
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
    assert!(!physical.contains(SHELL_SENTINEL), "{physical}");
    for index in 0..18 {
        let sentinel = format!("YGG-OWNED-RESIZE-{index:02}");
        assert_eq!(
            physical.matches(&sentinel).count(),
            1,
            "{sentinel} was not replayed exactly once:\n{physical}"
        );
    }
}

#[test]
fn resize_while_overlayed_replays_owned_transcript_before_repainting_overlay() {
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
        resize_text.contains("OVERLAY-ACTIVE-STREAM-BEFORE"),
        "{resize_text:?}"
    );
    for index in 0..18 {
        let sentinel = format!("YGG-OVERLAY-RESIZE-{index:02}");
        assert!(
            resize_text.contains(&sentinel),
            "{sentinel} was not replayed beneath the overlay:\n{resize_text:?}"
        );
    }

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
    assert!(
        !String::from_utf8_lossy(&close).contains("\x1b[3J"),
        "closing the overlay must use a differential repaint"
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
    assert_eq!(
        physical.matches("│ ›").count(),
        1,
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
    // Active reasoning occupies two rows; the first answer delta replaces it
    // with one transition row plus one assistant row, so the composer stays
    // at the same content-relative position.
    assert_eq!(composer_row(&streamed), initial_composer);
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
    assert!(composer_row(&tool) > composer_row(&streamed));
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
    assert!(steering_plain.contains("Steering prompt · queued"));
    assert!(steering_plain.contains("    ↳ also inspect tests"));

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

    for _ in 0..3 {
        shell.apply_edit(EditAction::Backspace);
    }
    assert_eq!(shell.pending(), "/");
    let expanded = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(
        !expanded.reanchor_viewport,
        "growing mutable chrome must not replay the viewport"
    );

    for character in "res".chars() {
        shell.apply_edit(EditAction::Char(character));
    }
    let collapsed = render_shell_update(&shell.state.borrow(), 80, Instant::now(), &mut frame);
    assert!(
        !collapsed.reanchor_viewport,
        "shrinking mutable chrome must clear only its changed tail"
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
fn theme_swap_repaints_visible_cells_but_preserves_native_scrollback_styles() {
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
        !complete
            .windows(b"\x1b[3J".len())
            .any(|window| window == b"\x1b[3J"),
        "theme swap cleared terminal-owned scrollback"
    );
    let mut terminal = vt100::Parser::new(HEIGHT, WIDTH, 128);
    terminal.process(&complete);
    assert!(
        find_ascii_cell(terminal.screen(), "historic-").is_some(),
        "visible tail lost after theme repaint: {:?}",
        terminal.screen().contents()
    );
    assert_ascii_foreground(&terminal, "historic-11", new_foreground);
    assert!(
        find_ascii_cell(terminal.screen(), "X").is_none(),
        "full viewport repaint left a stale cell: {:?}",
        terminal.screen().contents()
    );

    let mut native_history = None;
    for offset in 1..=usize::from(HEIGHT) {
        terminal.set_scrollback(offset);
        for (row, contents) in terminal.screen().rows(0, WIDTH).enumerate() {
            let Some(column) = contents.find("historic-") else {
                continue;
            };
            let color = terminal
                .screen()
                .cell(row as u16, column as u16)
                .expect("historic cell inside terminal bounds")
                .fgcolor();
            if color == old_foreground {
                native_history = Some((row as u16, column as u16));
                break;
            }
        }
        if native_history.is_some() {
            break;
        }
    }
    assert!(
        native_history.is_some(),
        "rows committed before the theme swap should retain their original cell style"
    );
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
fn switching_back_to_default_clears_named_theme_attributes() {
    const WIDTH: u16 = 48;
    const HEIGHT: u16 = 10;
    let capabilities = crate::tui::terminal::TerminalCapabilities::test(
        true,
        true,
        crate::tui::terminal::ColorDepth::TrueColor,
    );
    let violet = crate::tui::theme::test_bundled_theme_with(
        "violet-hour",
        capabilities,
        crate::tui::theme::TerminalBackground::Unknown,
    );
    let (mut shell, bytes) = emulated_shell(violet, WIDTH, HEIGHT);
    shell
        .state
        .borrow_mut()
        .push_block(TranscriptBlock::Assistant(Box::new(
            AssistantBlock::finalized("plain-default-prose".into()),
        )));
    shell.render();

    // Model a theme renderer ending a frame with every supported text
    // attribute active. Returning to default must reset the terminal's
    // rendition before it clears and repaints the visible viewport.
    bytes
        .lock()
        .expect("emulated terminal output mutex poisoned")
        .extend_from_slice(b"\x1b[1;3;4;7;48;2;12;34;56m");

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
        title: "Models".into(),
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

        // Establish the cached viewport. Prompts deliberately bypass the
        // theme transcript inset and begin with their two-cell marker.
        let _ = render_shell(&shell.state.borrow(), 80);
        let start = 3; // marker (2) + byte/cell index of 'e' (1)
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
fn named_theme_keeps_active_work_out_of_the_footer() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_theme(crate::tui::theme::test_bundled_theme_with(
        "bone-machine",
        crate::tui::terminal::TerminalCapabilities::test(
            true,
            true,
            crate::tui::terminal::ColorDepth::TrueColor,
        ),
        crate::tui::theme::TerminalBackground::Dark,
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
fn renderer_covers_all_terminal_outcomes() {
    let theme = crate::tui::theme::test_theme();
    let summary = crate::presentation::RunSummary {
        files_changed: 2,
        tool_calls: 4,
        warnings: 0,
    };
    let outcomes = [
        (
            RunOutcome::Completed {
                elapsed: Duration::from_millis(13700),
                summary: summary.clone(),
            },
            "completed · 13.7s",
        ),
        (
            RunOutcome::CompletedWithWarnings {
                elapsed: Duration::from_millis(18200),
                warnings: 2,
                summary: crate::presentation::RunSummary {
                    warnings: 2,
                    ..summary.clone()
                },
            },
            "completed with 2 notes · 18.2s",
        ),
        (
            RunOutcome::Failed {
                elapsed: Duration::from_millis(9400),
                reason: "command exited 1".into(),
            },
            "failed",
        ),
        (
            RunOutcome::Interrupted {
                elapsed: Duration::from_millis(6800),
            },
            "interrupted · 6.8s",
        ),
        (
            RunOutcome::NeedsInput {
                prompt: "choose an implementation".into(),
            },
            "needs input",
        ),
        (
            RunOutcome::Cancelled {
                elapsed: Duration::from_secs(1),
            },
            "interrupted · 1.0s",
        ),
    ];
    for (outcome, expected) in outcomes {
        let rendered = outcome_line(&outcome, &theme);
        assert!(rendered.contains(expected), "{rendered:?}");
        if matches!(outcome, RunOutcome::CompletedWithWarnings { .. }) {
            assert!(
                strip_terminal_sequences(&rendered).starts_with('✓'),
                "completed-with-notes should use a checkmark: {rendered:?}"
            );
        }
        assert!(
            rendered.contains('✓')
                || rendered.contains('◇')
                || rendered.contains('×')
                || rendered.contains('■')
        );
    }
}

#[test]
fn failed_outcome_keeps_the_headline_and_shows_a_bounded_safe_reason() {
    let theme = crate::tui::theme::test_theme();
    let reason = format!(
        "\x1b[31mProvider unavailable\x1b[0m\x07\n{}",
        "é".repeat(MAX_OUTCOME_DETAIL_BYTES)
    );
    let outcome = RunOutcome::Failed {
        elapsed: Duration::from_millis(9400),
        reason,
    };

    assert_eq!(
        strip_terminal_sequences(&outcome_line(&outcome, &theme)),
        "× failed · 9.4s"
    );
    let RunOutcome::Failed { reason, .. } = &outcome else {
        unreachable!()
    };
    let detail = bounded_outcome_detail(reason);
    assert!(detail.starts_with("Provider unavailable␇\n"), "{detail:?}");
    assert!(detail.ends_with('…'));
    assert!(detail.len() <= MAX_OUTCOME_DETAIL_BYTES);
    assert!(detail.is_char_boundary(detail.len()));
    assert!(!detail.contains("\x1b[31m"));
    assert!(detail
        .chars()
        .all(|character| !character.is_control() || character == '\n'));

    let rendered = render_outcome(&outcome, &theme, 48)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.starts_with("× failed · 9.4s\n"), "{rendered:?}");
    assert!(rendered.contains("Provider unavailable␇"), "{rendered:?}");

    let copied = block_copy_text(&TranscriptBlock::Outcome(outcome));
    assert!(copied.starts_with("failed · 9.4s\nProvider unavailable␇\n"));
    assert!(copied.ends_with('…'));
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

    shell.expand_focused_tool();
    let state = shell.state.borrow();
    let cache = state.transcript_cache.borrow();
    assert_eq!(cache.width, Some(100));
    assert_eq!(cache.dirty_blocks, [256]);
}

#[test]
fn tool_output_starts_under_tool_input() {
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
    let command = lines
        .iter()
        .find(|line| line.contains("Bash  printf hello"))
        .expect("tool input should render");
    let output = lines
        .iter()
        .find(|line| line.trim_start().starts_with("hello"))
        .expect("tool output should render");
    let input_column = command
        .find("printf hello")
        .map(|index| visible_width(&command[..index]))
        .expect("tool input value should render");
    let output_column = output
        .find("hello")
        .map(|index| visible_width(&output[..index]))
        .expect("tool output value should render");
    assert_eq!(
        input_column, output_column,
        "tool output should align with the tool input: {lines:?}"
    );
}

#[test]
fn tool_rendering_hides_failure_evidence_but_keeps_intent() {
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
        },
    );
    let plain = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 80).join("\n"));
    assert!(plain.contains("Bash  cargo test --workspace"), "{plain:?}");
    assert!(!plain.contains("provider-call-secret"), "{plain:?}");
    assert!(!plain.contains("exit=1"), "{plain:?}");
    assert!(!plain.contains("duration=0.2s"), "{plain:?}");
    assert!(!plain.contains("76 passed"), "{plain:?}");
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
        },
    );
    let plain = strip_terminal_sequences(&render_shell(&shell.state.borrow(), 120).join("\n"));
    assert!(plain.contains("Edit  src/lib.rs"), "{plain:?}");
    assert!(!plain.contains("The file changed"), "{plain:?}");
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
        shell.on_run_event(run_id, &AgentEvent::ToolFinished { id, result });
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
fn status_metadata_uses_the_model_accent_but_no_color_stays_plain() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

    let mut theme = crate::tui::theme::test_theme();
    crate::tui::theme::apply_model_lab(&mut theme, crate::tui::theme::ModelLab::Anthropic);
    let styled = styled_status_text(
            &theme,
            "Provider       anthropic\nModel          claude\nReasoning      high\n\nSecurity model: trusted local agent",
        );
    assert!(styled.contains("38;2;169;99;76"), "{styled:?}");
    assert!(styled.contains("Model"));
    assert!(styled.contains("claude"));

    let mut plain = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
        true,
        true,
        ColorDepth::None,
    ));
    crate::tui::theme::apply_model_lab(&mut plain, crate::tui::theme::ModelLab::Anthropic);
    let plain = styled_status_text(&plain, "Model          claude");
    assert_eq!(plain, "Model          claude");
    assert!(!plain.contains('\x1b'));
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
        state.push_block(TranscriptBlock::Outcome(RunOutcome::Completed {
            elapsed: Duration::from_secs(1),
            summary: crate::presentation::RunSummary {
                files_changed: 1,
                tool_calls: 1,
                warnings: 0,
            },
        }));
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
fn verbose_reasoning_deltas_keep_complete_incremental_state() {
    let theme = crate::tui::theme::test_theme();
    let mut reasoning = AssistantBlock::streaming_reasoning("First complete thought.\n\n");
    let initial_revision = reasoning.markdown.tail_revision();

    for step in 0..256 {
        reasoning.append_reasoning(&format!("Thought {step} stays visible.\n\n"));
    }

    assert!(
        reasoning.markdown.tail_revision() >= initial_revision + 256,
        "ordinary deltas must extend one incremental Markdown stream"
    );
    reasoning.reasoning_expanded = true;
    let live = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live.contains("First complete thought."), "{live}");
    assert!(live.contains("Thought 0 stays visible."), "{live}");
    assert!(live.contains("Thought 255 stays visible."), "{live}");

    reasoning.finish_reasoning();
    let finished = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
        .into_iter()
        .map(|line| strip_terminal_sequences(&line))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(finished.contains("First complete thought."), "{finished}");
    assert!(finished.contains("Thought 0 stays visible."), "{finished}");
    assert!(
        finished.contains("Thought 255 stays visible."),
        "{finished}"
    );
}

#[test]
fn reasoning_enabled_run_shows_fallback_before_provider_deltas() {
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
    assert_eq!(rendered.len(), 2, "{rendered:?}");
    assert!(rendered[0].contains("• Thinking"), "{rendered:?}");
    assert!(rendered[1].contains("ctrl+o to expand"), "{rendered:?}");
}

#[test]
fn collapsed_reasoning_dot_blinks_without_moving_the_label() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "high");
    shell.begin_run("codex");
    let render = |shell: &InteractiveShell| {
        shell
            .state
            .borrow()
            .rendered_transcript(80)
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .find(|line| line.contains("Thinking"))
            .expect("reasoning status row")
    };

    let visible = render(&shell);
    assert!(visible.contains('•'), "{visible:?}");
    {
        let mut state = shell.state.borrow_mut();
        assert!(event_dot_animating(&state));
        state.advance_event_dot_animation();
    }
    let hidden = render(&shell);
    assert!(!hidden.contains('•'), "{hidden:?}");
    let visual_label_column = |line: &str| {
        let offset = line.find("Thinking").expect("reasoning label");
        visible_width(&line[..offset])
    };
    assert_eq!(
        visual_label_column(&visible),
        visual_label_column(&hidden),
        "blinking the dot must not move the reasoning label"
    );
}

#[test]
fn collapsed_reasoning_aligns_with_tool_event_margin() {
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
        AssistantBlock::streaming_reasoning("").with_model_lab(Some(ModelLab::OpenAi)),
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
        .find(|line| line.contains("ctrl+o to expand"))
        .expect("reasoning disclosure row");
    let visual_column = |line: &str, needle: &str| {
        line.find(needle)
            .map(|offset| visible_width(&line[..offset]))
    };
    assert_eq!(
        visual_column(tool_line, "•"),
        visual_column(reasoning_line, "•"),
        "event dots must share the margin: {tool_line:?} vs {reasoning_line:?}"
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
fn reasoning_off_run_does_not_create_a_fallback_status() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("codex", "gpt-5.3-codex-spark", "off");
    shell.begin_run("codex");
    assert!(shell.state.borrow().transcript.is_empty());
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
    shell.on_run_event(
        run_id,
        &AgentEvent::ToolFinished {
            id,
            result: Ok(ygg_agent::ToolOutput::new("x".repeat(4_000))),
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
    assert!(initial[1].contains("ctrl+o to expand"), "{initial:?}");
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

    shell.expand_focused_tool();
    let expanded = transcript(&shell).join("\n");
    assert!(expanded.contains("first private sentinel"), "{expanded}");
    assert!(expanded.contains("private reasoning row 127"), "{expanded}");
    assert!(!expanded.contains("ctrl+o to expand"), "{expanded}");

    shell.expand_focused_tool();
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
        rendered.matches("ctrl+o to expand").count(),
        1,
        "{rendered}"
    );
    shell.expand_focused_tool();
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
    assert!(!expanded.contains("ctrl+o to expand"), "{expanded}");
}

#[test]
fn collapsed_reasoning_label_is_plain_model_colored_text() {
    let theme = crate::tui::theme::test_theme();
    let reasoning = AssistantBlock::streaming_reasoning("## Verifying `implementation`")
        .with_model_lab(Some(ModelLab::Alibaba));
    let label = live_reasoning_label(&theme, &reasoning);
    assert_eq!(strip_terminal_sequences(&label), "Verifying implementation");
    assert!(!label.contains("\x1b[1m"), "{label:?}");
    let accent = theme
        .model_rgb(Some(ModelLab::Alibaba))
        .expect("Alibaba model accent");
    assert!(
        label.contains(&format!(
            "\x1b[38;2;{};{};{}m",
            accent.0, accent.1, accent.2
        )),
        "reasoning label must retain the block model's accent: {label:?}"
    );
}

#[test]
fn collapsed_reasoning_shows_two_live_rows_and_no_settled_rows() {
    let theme = crate::tui::theme::test_theme();
    let renderer = theme.reasoning_renderer();
    let mut reasoning =
        AssistantBlock::streaming_reasoning("private").with_model_lab(Some(ModelLab::Alibaba));
    let live = render_reasoning(&reasoning, &renderer, &theme, 80, false);
    assert_eq!(live.len(), 2, "{live:?}");
    assert!(strip_terminal_sequences(&live[0]).contains("• Thinking"));
    assert!(strip_terminal_sequences(&live[1]).contains("└ (ctrl+o to expand)"));
    assert!(!live[0].contains("\x1b[1m"), "{live:?}");
    let accent = theme
        .model_rgb(Some(ModelLab::Alibaba))
        .expect("Alibaba model accent");
    assert!(
        live[0].contains(&format!(
            "\x1b[38;2;{};{};{}m",
            accent.0, accent.1, accent.2
        )),
        "reasoning label must retain the block model's accent: {live:?}"
    );

    reasoning.reasoning_elapsed = Some(Duration::from_millis(13_700));
    reasoning.finish_reasoning();
    let settled = render_reasoning(&reasoning, &renderer, &theme, 80, false);
    assert!(
        settled.is_empty(),
        "finished reasoning leaves no trace when collapsed"
    );
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
fn collapsed_reasoning_has_ascii_fallback_and_width_bounded_rows() {
    let theme =
        crate::tui::theme::test_theme_with(crate::tui::terminal::TerminalCapabilities::test(
            false,
            false,
            crate::tui::terminal::ColorDepth::None,
        ));
    let reasoning = AssistantBlock::streaming_reasoning(
        "## A heading that is intentionally much wider than the viewport\n",
    );
    let lines = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 16, false);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("* A heading"), "{lines:?}");
    assert!(lines[1].starts_with("  `- (ctrl+o to"), "{lines:?}");
    assert!(lines.iter().all(|line| visible_width(line) <= 16));
    assert!(lines.iter().all(|line| !line.contains('\x1b')));
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

    shell.expand_focused_tool();
    let expanded = render(&shell).join("\n");
    assert!(expanded.contains("durable private thought"), "{expanded}");
    assert!(expanded.contains("with a second line"), "{expanded}");

    shell.expand_focused_tool();
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
    shell.expand_focused_tool();
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
        "prompts should begin at the primary transcript edge: {prompt:?}"
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
        strip_terminal_sequences(&reasoning_block).starts_with("  · Thinking"),
        "reasoning keeps the transcript inset without a second dot: {reasoning_block:?}"
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
fn prompt_row_keeps_exact_persisted_color_across_theme_changes() {
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
        assert!(visible_width(rendered) <= 40, "{rendered:?}");
    }
}

#[test]
fn persisted_prompt_background_fills_the_semantic_row_in_terminal_cells() {
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
    let expected = vt100::Color::Rgb(0x12, 0x34, 0x56);
    assert_eq!(rendered.len(), 2, "fixture should wrap to two prompt rows");
    assert!(
        strip_terminal_sequences(&rendered[0]).starts_with('›'),
        "prompt should begin at the primary transcript edge: {rendered:?}"
    );
    for row in 0..rendered.len() as u16 {
        for column in 0..WIDTH {
            let background = terminal
                .screen()
                .cell(row, column)
                .expect("prompt row cell inside terminal bounds")
                .bgcolor();
            assert_eq!(
                background, expected,
                "prompt background did not reach row {row}, column {column}"
            );
        }
    }
    assert!(rendered[0].contains("48;2;18;52;86m"), "{rendered:?}");

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
    assert!(
        card_cells > 0,
        "card prompt should retain a coloured interior"
    );
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
    assert!(!failed.screen().contents().contains("permission denied"));

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
fn event_margin_markers_toggle_live_and_settle_with_tool_specific_tones() {
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

    let active = event_margin_marker(&panel(false, false), &theme, true, false)
        .expect("visible active marker");
    assert_eq!(strip_terminal_sequences(&active), "•");
    assert!(!active.contains("\x1b[5m"), "{active:?}");
    let hidden = event_margin_marker(&panel(false, false), &theme, false, false)
        .expect("hidden active marker");
    assert_eq!(hidden, " ");

    let settled_edit =
        event_margin_marker(&panel(true, false), &theme, false, false).expect("edit marker");
    assert_eq!(settled_edit, theme.settled_event_dot("neutral", "•"));

    let failed =
        event_margin_marker(&panel(true, true), &theme, false, false).expect("failure marker");
    assert_eq!(failed, theme.settled_event_dot("error", "•"));

    let bash_args = serde_json::json!({"command":"cargo test"});
    let bash = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("bash".into()),
        "bash".into(),
        bash_args.to_string(),
        summarize_tool("bash", &bash_args),
        String::new(),
        true,
        false,
        None,
        None,
    )));
    assert_eq!(
        event_margin_marker(&bash, &theme, false, false),
        Some(theme.settled_event_dot("success", "•"))
    );

    let read = TranscriptBlock::Tool(Box::new(ToolPanel::new(
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
    assert_eq!(
        event_margin_marker(&read, &theme, false, false),
        Some(theme.settled_event_dot("neutral", "•"))
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
    assert_eq!(
        event_margin_marker(&reasoning, &theme, true, true)
            .map(|marker| strip_terminal_sequences(&marker)),
        Some("•".into())
    );
    assert_eq!(
        event_margin_marker(&reasoning, &theme, false, true),
        Some(" ".into())
    );
    let outcome = TranscriptBlock::Outcome(RunOutcome::CompletedWithWarnings {
        elapsed: Duration::from_secs(1),
        warnings: 1,
        summary: crate::presentation::RunSummary {
            files_changed: 0,
            tool_calls: 1,
            warnings: 1,
        },
    });
    assert_eq!(event_margin_marker(&outcome, &theme, true, false), None);
}

#[test]
fn event_dot_animation_invalidates_active_tool_rows_in_lockstep() {
    let shell = InteractiveShell::test_shell();
    {
        let mut state = shell.state.borrow_mut();
        for (id, name) in [("read", "read"), ("edit", "edit")] {
            let args = serde_json::json!({"path":"src/lib.rs"});
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
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>()
    };
    let visible = active_rows();
    assert_eq!(visible.len(), 2, "{visible:?}");
    assert!(
        visible.iter().all(|line| line.starts_with("• ")),
        "{visible:?}"
    );

    shell.state.borrow_mut().advance_event_dot_animation();
    let hidden = active_rows();
    assert_eq!(hidden.len(), 2, "{hidden:?}");
    assert!(
        hidden.iter().all(|line| line.starts_with("  ")),
        "{hidden:?}"
    );

    shell.state.borrow_mut().advance_event_dot_animation();
    let visible_again = active_rows();
    assert_eq!(visible_again, visible);
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
    assert!(
        lines[1..]
            .iter()
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with("            ")),
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
fn compiled_default_shimmer_moves_only_while_work_is_active() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let capabilities = TerminalCapabilities::test(true, true, ColorDepth::TrueColor);
    let theme = crate::tui::theme::test_theme_for(TerminalBackground::Dark, capabilities);
    let mut shell = InteractiveShell::test_shell_with_theme(theme);
    shell.state.borrow_mut().reasoning = "high".into();
    let idle_now = Instant::now();
    assert!(!shimmer_animating(&shell.state.borrow()));
    let idle_before =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, idle_now);
    let idle_after = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        idle_now + Duration::from_millis(250),
    );
    assert_eq!(idle_before[0], idle_after[0]);

    let run_id = shell.begin_run("anthropic");
    let started = shell
        .state
        .borrow()
        .shimmer_started_at
        .expect("active run shimmer anchor");
    assert!(shimmer_animating(&shell.state.borrow()));
    let active_before =
        crate::tui::composer_surface::render_composer_surface(&shell.state.borrow(), 80, started);
    let active_after = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        started + Duration::from_millis(250),
    );
    assert_ne!(active_before[0], active_after[0]);
    assert!(active_before[..3]
        .iter()
        .chain(&active_after[..3])
        .all(|line| !line.contains("\x1b[48;2;")));

    shell.interrupt_run(run_id);
    assert!(!shimmer_animating(&shell.state.borrow()));
    let rest = crate::tui::composer_surface::render_composer_surface(
        &shell.state.borrow(),
        80,
        started + Duration::from_secs(1),
    );
    assert_ne!(active_after[0], rest[0]);
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
        },
        AgentEvent::TurnFinished {
            message: AssistantMessage {
                content: vec![AssistantPart::Text("answer".into())],
                model: ModelId("m".into()),
                protocol: Protocol::OpenAiChat,
            },
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
    assert!(shell.debug_tool_output(&id).unwrap().contains("reading"));
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
fn bash_wraps_and_indents_output_without_connector_glyphs() {
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
    )
    .into_iter()
    .map(|line| strip_terminal_sequences(&line))
    .collect::<Vec<_>>();

    assert!(rendered[0].starts_with("• Bash"), "{rendered:?}");
    let command_byte = rendered[0].find("node").expect("command on Bash row");
    let command_column = visible_width(&rendered[0][..command_byte]);
    let no_output = rendered
        .iter()
        .find(|line| line.contains("(no output)"))
        .expect("no-output metadata");
    assert_eq!(
        no_output.find("(no output)"),
        Some(command_column),
        "Bash metadata must share the command content gutter: {rendered:?}"
    );
    for continuation in rendered
        .iter()
        .skip(1)
        .take_while(|line| !line.contains("(no output)"))
    {
        assert!(
            continuation.len() - continuation.trim_start().len() >= command_column,
            "wrapped commands must stay at or beyond their first command cell: {rendered:?}"
        );
    }
    assert!(
        rendered
            .iter()
            .all(|line| !line.contains('│') && !line.contains('└')),
        "tool rows must not contain connector glyphs: {rendered:?}"
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

    let details =
        render_compact_bash_output(&panel, &theme, 80, false, false, &tool_value_indent("Bash"));
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
    let command_byte = rendered[0].find(command).expect("command on Bash row");
    let command_column = visible_width(&rendered[0][..command_byte]);
    let hidden = rendered
        .iter()
        .find(|line| line.contains("3 lines hidden"))
        .expect("synthetic hidden-line metadata");
    let output = rendered
        .iter()
        .find(|line| line.contains("result line 4"))
        .expect("first retained output row");
    let hidden_byte = hidden.find('…').expect("hidden metadata marker");
    assert_eq!(
        visible_width(&hidden[..hidden_byte]),
        command_column,
        "{rendered:?}"
    );
    assert_eq!(
        output.find("result line 4"),
        Some(command_column),
        "{rendered:?}"
    );
    let TranscriptBlock::Tool(panel) = &block else {
        unreachable!("fixture is a Bash tool panel");
    };
    assert!(
        !panel.output.contains("lines hidden"),
        "synthetic UI metadata must not enter the raw tool payload"
    );
}

#[test]
fn footer_collapses_semantically_and_keeps_one_adjacent_row() {
    let mut shell = InteractiveShell::test_shell();
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
        state.show_turn_cost = true;
        state.telemetry_model = Some(state.model.clone());
    }
    let now = Instant::now();
    assert_eq!(
        plain_footer(&shell, 100, now),
        "  Qwen3.6 35B A3B · high   5.6k/246k   ↑26.8k ↓422   41.9 tok/s   $0"
    );
    assert_eq!(
        plain_footer(&shell, 68, now),
        "  Qwen3.6 35B A3B   5.6k/246k   ↑26.8k ↓422   41.9 tok/s   $0"
    );
    assert_eq!(
        plain_footer(&shell, 44, now),
        "  Qwen3.6 35B A3B   41.9 tok/s   $0"
    );
    assert_eq!(plain_footer(&shell, 30, now), "  Qwen3.6  41.9 tok/s  $0");

    let surface = plain_composer_surface(&shell, 100, now);
    assert_eq!(surface.len(), 4, "one editor row, two borders, one footer");
    assert!(!surface[surface.len() - 2].is_empty());
    assert_eq!(surface.last().unwrap(), &plain_footer(&shell, 100, now));
    assert!(surface.iter().all(|line| visible_width(line) <= 100));
}

#[test]
fn footer_omits_unknown_cost_and_shows_live_throughput_with_active_status() {
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
        state.show_turn_cost = true;
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
        live.contains("~72.4 tok/s"),
        "live estimate missing: {live:?}"
    );
    assert!(
        live.contains("~↓630"),
        "live output estimate missing: {live:?}"
    );
    assert!(
        live.contains("~21.6k/256k"),
        "live context missing: {live:?}"
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
        !paid.contains("session"),
        "session cost stays in /status: {paid:?}"
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
        active_sample.contains("72.4 tok/s"),
        "provider-final throughput should remain visible while tools run: {active_sample:?}"
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
        completed_sample.contains("72.4 tok/s"),
        "final metrics should appear after the whole run settles: {completed_sample:?}"
    );
    assert!(!completed_sample.contains('~'), "{completed_sample:?}");
}

#[test]
fn footer_distinguishes_explicit_zero_from_unavailable_pricing() {
    let mut shell = InteractiveShell::test_shell();
    shell.set_identity("local", "qwen3.6-35b-a3b", "high");
    let now = Instant::now();
    shell.state.borrow_mut().show_turn_cost = true;

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
    assert!(footer.contains("102/272k"), "{footer:?}");
    assert!(footer.contains("cache 92.4%"), "{footer:?}");
    assert!(!footer.contains("session"), "{footer:?}");
    assert!(
        footer.contains("$0.0914"),
        "accumulated session cost missing: {footer:?}"
    );
    assert!(!footer.contains('~'), "{footer:?}");
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

    shell.expand_focused_tool();
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
    assert!(collapsed.contains("3 lines hidden"), "{collapsed}");
    assert_eq!(
        shell.debug_tool_output(&id).as_deref(),
        Some(secret.as_str())
    );

    shell.expand_focused_tool();
    assert!(shell.verbose_tools());
    let expanded = transcript(&shell);
    assert!(expanded.contains("private result line 1"), "{expanded}");
    assert!(expanded.contains("private result line 8"), "{expanded}");
    assert!(!expanded.to_ascii_lowercase().contains("evidence"));

    shell.expand_focused_tool();
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

    shell.expand_focused_tool();
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
            spinner: "".into(),
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

    shell.expand_focused_tool();
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

    shell.expand_focused_tool();
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
    shell.expand_focused_tool();
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
        state.show_turn_cost = true;
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
        .position(|line| line.starts_with('┌') || line.starts_with('╭') || line.starts_with('+'))
        .expect("composer top border");
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
fn composer_border_is_restrained_at_rest_and_uses_model_accent_when_focused() {
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

    assert_ne!(idle[0], focused[0]);
    assert!(!idle[0].contains("38;2;169;99;76"), "{:?}", idle[0]);
    assert!(focused[0].contains("38;2;169;99;76"), "{:?}", focused[0]);
    assert_eq!(visible_width(&idle[0]), 60);
    assert_eq!(visible_width(&focused[0]), 60);
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
    assert!(rendered.first().is_some_and(|line| line.starts_with('┌')
        || line.starts_with('╭')
        || line.starts_with('+')));
    assert!(rendered
        .get(1)
        .is_some_and(|line| line.starts_with('│') || line.starts_with('|')));
    assert!(rendered.get(2).is_some_and(|line| line.starts_with('└')
        || line.starts_with('╰')
        || line.starts_with('+')));
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
    let note = theme_with_layout("").glyph("note").to_owned();
    assert!(compact[0].starts_with(&format!(" {note} ")));
    assert!(comfortable[1].starts_with(&format!("  {note} ")));
    assert!(airy[2].starts_with(&format!("    {note} ")));

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
                show_tool_duration = true
                narrow_show_tool_duration = false
            "#,
    );
    let renderer = theme.rich_renderer();
    let reasoning = TranscriptBlock::Reasoning(Box::new(AssistantBlock::finalized_reasoning(
        "visible at the breakpoint".into(),
    )));
    let at_breakpoint = render_block(None, &reasoning, &theme, &renderer, &renderer, 72, false);
    let below_breakpoint = render_block(None, &reasoning, &theme, &renderer, &renderer, 71, false);
    assert!(
        at_breakpoint.is_empty(),
        "finished reasoning leaves no collapsed trace"
    );
    assert!(
        below_breakpoint.is_empty(),
        "finished reasoning leaves no collapsed trace"
    );

    let args = serde_json::json!({"command": "cargo check"});
    let tool = TranscriptBlock::Tool(Box::new(ToolPanel::new(
        ToolCallId("duration-breakpoint".into()),
        "bash".into(),
        args.to_string(),
        summarize_tool("bash", &args),
        "exit=0 duration=0.2s".into(),
        true,
        false,
        None,
        None,
    )));
    let at_breakpoint = strip_terminal_sequences(
        &render_block(None, &tool, &theme, &renderer, &renderer, 72, false).join("\n"),
    );
    let below_breakpoint = strip_terminal_sequences(
        &render_block(None, &tool, &theme, &renderer, &renderer, 71, false).join("\n"),
    );
    assert!(at_breakpoint.contains("0.2s"), "{at_breakpoint:?}");
    assert!(!below_breakpoint.contains("0.2s"), "{below_breakpoint:?}");
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
    assert!(selection_position_for_visual_cell(&shell.state.borrow(), second_start, 4).is_none());
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

    let body = &rows[body_row];
    assert!(body.contains("\x1b[38;2;255;255;255m"), "{body:?}");
    assert!(
        body.contains("\x1b[48;2;255;112;24m"),
        "the prompt should retain its exact persisted provenance: {body:?}"
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
        render_block_planned(None, &block, &theme, &renderer, &renderer, 40, false, true);
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
    assert_eq!(composer.len(), 3, "hidden footer leaves only the box");
    assert!(composer[1].starts_with("│   ›"), "{composer:?}");
    assert!(composer.iter().all(|line| visible_width(line) == 80));

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

    shell.set_extension_header(Some((
        "EXT\x1b[31m red\ntail".into(),
        Some("invalid role!".into()),
    )));
    shell.set_extension_status(Some(("branch main".into(), None)));
    let narrow = shell_chrome(&shell.state.borrow(), 40, now);
    assert_eq!(narrow.header.len(), 1);
    let extension_header = strip_terminal_sequences(&narrow.header[0]);
    assert!(
        extension_header.contains("EXT red tail"),
        "{extension_header:?}"
    );
    assert!(!extension_header.contains('\x1b'));
    let composer = plain_composer_surface(&shell, 40, now);
    assert_eq!(composer.len(), 4, "explicit status restores one footer row");
    assert!(composer
        .last()
        .is_some_and(|line| line.contains("branch main")));
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
    assert_eq!(wide.len(), 7);
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
    assert_eq!(narrow.len(), 5);
    assert!(narrow
        .first()
        .is_some_and(|line| line.contains("Select model")));
    assert!(narrow.iter().all(|line| !line.chars().all(|ch| ch == '─')));
}

const BUNDLED_THEME_NAMES: [&str; 10] = [
    "bone-machine",
    "circuit-garden",
    "field-notes",
    "oxide-console",
    "paper-ledger",
    "signal-noir",
    "synthwave-relay",
    "tidepool",
    "violet-hour",
    "zen-mono",
];

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
        spinner: "✓".into(),
    })));
    state.push_block(TranscriptBlock::Notice(
        "Extension reloaded with one status contribution.".into(),
    ));
    state.push_block(TranscriptBlock::Outcome(RunOutcome::Completed {
        elapsed: Duration::from_millis(13700),
        summary: crate::presentation::RunSummary {
            files_changed: 1,
            tool_calls: 2,
            warnings: 0,
        },
    }));
    state.extension_header = Some(("workspace · main".into(), None));
    state.extension_status = Some(("git clean".into(), None));
    state.editor = "draft a local patch".into();
}

/// Remove colors and semantic words while retaining whitespace,
/// punctuation, rails, rules, and card geometry. Palette/wordmark-only
/// changes therefore cannot satisfy the bundled identity test.
fn structural_signature(rendered: &str) -> String {
    let plain = strip_terminal_sequences(rendered);
    let mut signature = String::with_capacity(plain.len());
    let mut word = false;
    for character in plain.chars() {
        if character.is_alphanumeric() || character == '_' {
            if !word {
                signature.push('x');
                word = true;
            }
        } else {
            word = false;
            signature.push(character);
        }
    }
    signature
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
fn bundled_theme_pack_has_ten_color_independent_wide_and_narrow_identities() {
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::TerminalBackground;

    let mut wide = HashSet::new();
    let mut ascii = HashSet::new();
    let mut narrow = HashSet::new();
    for name in BUNDLED_THEME_NAMES {
        let mut shell = InteractiveShell::test_shell();
        shell.set_size(96, 80);
        shell.set_theme(crate::tui::theme::test_bundled_theme_with(
            name,
            TerminalCapabilities::test(true, true, ColorDepth::TrueColor),
            TerminalBackground::Dark,
        ));
        populate_theme_fixture(&mut shell);
        let transcript = shell.state.borrow().rendered_transcript(96).join("\n");
        assert!(
            transcript.contains("\x1b[48;2;255;112;24m"),
            "{name} changed the immutable prompt provenance background"
        );
        assert!(
            !transcript.contains("\x1b[38;2;255;112;24m"),
            "{name} rendered provenance as foreground-only"
        );
        let unclosed_backgrounds = transcript
            .lines()
            .filter(|line| ansi_background_is_open_at_end(line))
            .collect::<Vec<_>>();
        assert!(
            unclosed_backgrounds.is_empty(),
            "{name} leaked a painted surface beyond its row: {unclosed_backgrounds:?}"
        );
        assert!(
            wide.insert(structural_signature(&transcript)),
            "{name} duplicated another color-stripped transcript geometry"
        );

        let mut plain_shell = InteractiveShell::test_shell();
        plain_shell.set_size(96, 80);
        plain_shell.set_theme(crate::tui::theme::test_bundled_theme_with(
            name,
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
            "{name} emitted ANSI in no-color mode"
        );
        assert!(
            ascii.insert(structural_signature(&plain)),
            "{name} duplicated another ASCII transcript geometry"
        );

        let mut narrow_shell = InteractiveShell::test_shell();
        narrow_shell.set_size(40, 80);
        narrow_shell.set_theme(crate::tui::theme::test_bundled_theme_with(
            name,
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
            "{name} overflowed a narrow terminal"
        );
        assert!(
            narrow.insert(structural_signature(&narrow_frame)),
            "{name} duplicated another narrow transcript geometry"
        );

        if std::env::var_os("YGG_DUMP_THEME_FRAMES").is_some() {
            eprintln!(
                "\n===== {name} / wide =====\n{}",
                strip_terminal_sequences(&transcript)
            );
            eprintln!("\n===== {name} / narrow =====\n{narrow_frame}");
        }
    }
    assert_eq!(wide.len(), 10);
    assert_eq!(ascii.len(), 10);
    assert_eq!(narrow.len(), 10);
}
