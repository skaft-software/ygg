//! Named behavioral ports of Pi's pinned `tui-render.test.ts` and
//! `tui-shrink.test.ts` rendering cases.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;

use sexy_tui_rs::{
    ColorDepth, Component, Terminal, TerminalCapabilities, TerminalInput, CURSOR_MARKER, TUI,
};

const PI_SYNC_BEGIN: &str = "\x1b[?2026h";
const PI_SYNC_END: &str = "\x1b[?2026l";
const PI_CLEAR_AND_REPLAY: &str = "\x1b[2J\x1b[H\x1b[3J";

#[derive(Clone)]
struct Lines(Rc<RefCell<Vec<String>>>);

impl Component for Lines {
    fn render(&self, _width: u16) -> Vec<String> {
        self.0.borrow().clone()
    }

    fn invalidate(&mut self) {}
}

struct VirtualTerminal {
    size: Rc<Cell<(u16, u16)>>,
    parser: Rc<RefCell<vt100::Parser>>,
    writes: Rc<RefCell<String>>,
}

impl Terminal for VirtualTerminal {
    fn start_events(
        &mut self,
        _on_input: Box<dyn FnMut(TerminalInput)>,
        _on_resize: Box<dyn FnMut()>,
    ) {
    }

    fn stop(&mut self) {}

    fn write(&mut self, data: &str) {
        self.writes.borrow_mut().push_str(data);
        self.parser.borrow_mut().process(data.as_bytes());
    }

    fn columns(&self) -> u16 {
        self.size.get().0
    }

    fn rows(&self) -> u16 {
        self.size.get().1
    }

    fn move_by(&mut self, lines: i16) {
        if lines > 0 {
            self.write(&format!("\x1b[{}B", lines as u16));
        } else if lines < 0 {
            self.write(&format!("\x1b[{}A", lines.unsigned_abs()));
        }
    }

    fn hide_cursor(&mut self) {
        self.write("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.write("\x1b[?25h");
    }

    fn clear_line(&mut self) {
        self.write("\x1b[2K");
    }

    fn clear_from_cursor(&mut self) {
        self.write("\x1b[0J");
    }

    fn clear_screen(&mut self) {
        self.write("\x1b[2J\x1b[H");
    }

    fn capabilities(&self) -> TerminalCapabilities {
        TerminalCapabilities::interactive(ColorDepth::Ansi16, true)
    }
}

struct Harness {
    tui: TUI<'static>,
    lines: Rc<RefCell<Vec<String>>>,
    size: Rc<Cell<(u16, u16)>>,
    parser: Rc<RefCell<vt100::Parser>>,
    writes: Rc<RefCell<String>>,
}

impl Harness {
    fn new(columns: u16, rows: u16, lines: Vec<String>) -> Self {
        let size = Rc::new(Cell::new((columns, rows)));
        let parser = Rc::new(RefCell::new(vt100::Parser::new(rows, columns, 512)));
        let writes = Rc::new(RefCell::new(String::new()));
        let terminal = VirtualTerminal {
            size: size.clone(),
            parser: parser.clone(),
            writes: writes.clone(),
        };
        let lines = Rc::new(RefCell::new(lines));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(Lines(lines.clone())));
        Self {
            tui,
            lines,
            size,
            parser,
            writes,
        }
    }

    fn start(&mut self) {
        self.tui.start();
    }

    fn render(&mut self) {
        self.tui.request_render();
    }

    fn clear_writes(&self) {
        self.writes.borrow_mut().clear();
    }

    fn take_writes(&self) -> String {
        std::mem::take(&mut *self.writes.borrow_mut())
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        self.size.set((columns, rows));
        self.parser.borrow_mut().set_size(rows, columns);
        self.render();
    }

    fn viewport(&self) -> Vec<String> {
        let width = self.size.get().0;
        self.parser
            .borrow()
            .screen()
            .rows(0, width)
            .map(|line| line.trim_end().to_owned())
            .collect()
    }
}

fn numbered(prefix: &str, count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("{prefix} {index}"))
        .collect()
}

#[test]
fn pi_height_change_triggers_full_rerender() {
    let mut harness = Harness::new(40, 10, numbered("Line", 3));
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();

    harness.resize(40, 15);

    assert!(harness.tui.full_redraws() > redraws);
    let output = harness.take_writes();
    assert!(output.contains(PI_CLEAR_AND_REPLAY), "{output:?}");
    assert!(harness.viewport()[0].contains("Line 0"));
}

#[test]
fn pi_termux_height_changes_skip_full_rerender() {
    const CHILD: &str = "SEXY_TUI_TERMUX_PARITY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "pi_termux_height_changes_skip_full_rerender"])
            .env(CHILD, "1")
            .env("TERMUX_VERSION", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child stdout:\n{}\nchild stderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let mut harness = Harness::new(40, 10, numbered("Line", 20));
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    for height in [15, 8, 14, 11] {
        harness.resize(40, height);
    }

    assert_eq!(harness.tui.full_redraws(), redraws);
    assert!(!harness.writes.borrow().contains("\x1b[3J"));
    harness.clear_writes();
    harness.lines.borrow_mut()[19] = "Latest content".into();
    harness.render();
    assert!(harness.take_writes().contains("Latest content"));
}

#[test]
fn pi_width_change_triggers_full_rerender() {
    let mut harness = Harness::new(40, 10, numbered("Line", 3));
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    harness.resize(60, 10);
    assert!(harness.tui.full_redraws() > redraws);
    assert!(harness.take_writes().contains(PI_CLEAR_AND_REPLAY));
}

#[test]
fn pi_clear_on_shrink_replays_and_clears_empty_rows() {
    let mut harness = Harness::new(40, 10, numbered("Line", 6));
    harness.tui.set_clear_on_shrink(true);
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    *harness.lines.borrow_mut() = numbered("Line", 2);
    harness.render();

    assert!(harness.tui.full_redraws() > redraws);
    assert!(harness.take_writes().contains(PI_CLEAR_AND_REPLAY));
    let viewport = harness.viewport();
    assert_eq!(&viewport[..4], ["Line 0", "Line 1", "", ""]);
}

#[test]
fn pi_clear_on_shrink_handles_single_line() {
    let mut harness = Harness::new(40, 10, numbered("Line", 4));
    harness.tui.set_clear_on_shrink(true);
    harness.start();

    *harness.lines.borrow_mut() = vec!["Only line".into()];
    harness.render();
    assert_eq!(&harness.viewport()[..2], ["Only line", ""]);
}

#[test]
fn pi_clear_on_shrink_handles_empty_frame() {
    let mut harness = Harness::new(40, 10, numbered("Line", 4));
    harness.tui.set_clear_on_shrink(true);
    harness.start();

    harness.lines.borrow_mut().clear();
    harness.render();
    assert!(harness.viewport().iter().all(String::is_empty));
}

#[test]
fn pi_shrink_tracks_cursor_for_the_next_middle_change() {
    let mut harness = Harness::new(40, 10, numbered("Line", 5));
    harness.start();
    harness.lines.borrow_mut().truncate(3);
    harness.render();
    harness.lines.borrow_mut()[1] = "CHANGED".into();
    harness.render();
    assert_eq!(&harness.viewport()[..3], ["Line 0", "CHANGED", "Line 2"]);
}

#[test]
fn pi_middle_line_spinner_updates_without_repainting_neighbors() {
    let mut harness = Harness::new(
        40,
        10,
        vec!["Header".into(), "Working...".into(), "Footer".into()],
    );
    harness.start();
    for frame in ['|', '/', '-', '\\'] {
        harness.lines.borrow_mut()[1] = format!("Working {frame}");
        harness.clear_writes();
        harness.render();
        let output = harness.take_writes();
        assert!(output.contains(&format!("Working {frame}")));
        assert!(!output.contains("Header"));
        assert!(!output.contains("Footer"));
        assert_eq!(
            &harness.viewport()[..3],
            ["Header", &format!("Working {frame}"), "Footer"]
        );
    }
}

#[test]
fn pi_resets_styles_after_every_non_image_line() {
    let mut harness = Harness::new(20, 6, vec!["\x1b[3mItalic".into(), "Plain".into()]);
    harness.start();
    let parser = harness.parser.borrow();
    assert!(!parser.screen().cell(1, 0).unwrap().italic());
}

#[test]
fn pi_first_line_change_preserves_following_rows() {
    let mut harness = Harness::new(40, 10, numbered("Line", 4));
    harness.start();
    harness.lines.borrow_mut()[0] = "FIRST".into();
    harness.render();
    assert_eq!(
        &harness.viewport()[..4],
        ["FIRST", "Line 1", "Line 2", "Line 3"]
    );
}

#[test]
fn pi_last_line_change_preserves_preceding_rows() {
    let mut harness = Harness::new(40, 10, numbered("Line", 4));
    harness.start();
    harness.lines.borrow_mut()[3] = "LAST".into();
    harness.render();
    assert_eq!(
        &harness.viewport()[..4],
        ["Line 0", "Line 1", "Line 2", "LAST"]
    );
}

#[test]
fn pi_nonadjacent_line_changes_preserve_unchanged_rows() {
    let mut harness = Harness::new(40, 10, numbered("Line", 5));
    harness.start();
    harness.lines.borrow_mut()[1] = "SECOND".into();
    harness.lines.borrow_mut()[3] = "FOURTH".into();
    harness.render();
    assert_eq!(
        &harness.viewport()[..5],
        ["Line 0", "SECOND", "Line 2", "FOURTH", "Line 4"]
    );
}

#[test]
fn pi_content_can_transition_to_empty_and_back() {
    let mut harness = Harness::new(40, 10, numbered("Line", 3));
    harness.start();
    harness.lines.borrow_mut().clear();
    harness.render();
    *harness.lines.borrow_mut() = numbered("New Line", 2);
    harness.render();
    assert_eq!(&harness.viewport()[..2], ["New Line 0", "New Line 1"]);
}

#[test]
fn pi_deleted_rows_above_the_viewport_force_complete_replay() {
    let mut harness = Harness::new(20, 5, numbered("Line", 12));
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    *harness.lines.borrow_mut() = numbered("Line", 7);
    harness.render();

    assert!(harness.tui.full_redraws() > redraws);
    assert!(harness.take_writes().contains(PI_CLEAR_AND_REPLAY));
    assert_eq!(
        harness.viewport(),
        vec!["Line 2", "Line 3", "Line 4", "Line 5", "Line 6"]
    );
}

#[test]
fn pi_append_after_shrink_stays_on_differential_path() {
    let mut harness = Harness::new(20, 5, numbered("Line", 8));
    harness.start();
    *harness.lines.borrow_mut() = numbered("Line", 2);
    harness.render();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    *harness.lines.borrow_mut() = numbered("Line", 3);
    harness.render();

    assert_eq!(harness.tui.full_redraws(), redraws);
    assert!(!harness.take_writes().contains("\x1b[3J"));
    assert_eq!(&harness.viewport()[..3], ["Line 0", "Line 1", "Line 2"]);
}

#[test]
fn pi_transient_frame_height_does_not_leave_stale_rows() {
    let long_chat = numbered("Chat", 15);
    let short_chat = numbered("Chat", 12);
    let editor = vec!["Editor 0".into(), "Editor 1".into(), "Editor 2".into()];
    let selector = numbered("Selector", 8);
    let mut initial = long_chat.clone();
    initial.extend(editor.clone());
    let mut harness = Harness::new(40, 10, initial);
    harness.start();

    let mut with_selector = long_chat;
    with_selector.extend(selector);
    *harness.lines.borrow_mut() = with_selector;
    harness.render();

    let mut restored = short_chat;
    restored.extend(editor);
    *harness.lines.borrow_mut() = restored;
    harness.render();

    assert_eq!(
        harness.viewport(),
        vec![
            "Chat 5", "Chat 6", "Chat 7", "Chat 8", "Chat 9", "Chat 10", "Chat 11", "Editor 0",
            "Editor 1", "Editor 2"
        ]
    );
}

fn kitty_image(id: u32, rows: usize, payload: &str) -> String {
    format!("\x1b_Ga=T,f=100,i={id},c=2,r={rows};{payload}\x1b\\")
}

fn delete_kitty_image(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

#[test]
fn pi_appended_kitty_placement_clears_reserved_rows_before_drawing() {
    let mut harness = Harness::new(40, 10, vec!["before".into()]);
    harness.start();
    harness.clear_writes();
    let image = kitty_image(1, 2, "AAAA");
    *harness.lines.borrow_mut() = vec!["before".into(), image.clone(), "".into(), "after".into()];
    harness.render();
    let output = harness.take_writes();
    let expected = format!("\x1b[2K\r\n\x1b[2K\x1b[1A{image}\x1b[1B");
    assert!(output.contains(&expected), "{output:?}");
}

#[test]
fn pi_unsafe_kitty_preclear_falls_back_to_full_replay() {
    let mut harness = Harness::new(40, 2, vec!["before".into()]);
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    let image = kitty_image(2, 3, "AAAA");
    *harness.lines.borrow_mut() =
        vec!["before".into(), image, "".into(), "".into(), "after".into()];
    harness.render();
    assert!(harness.tui.full_redraws() > redraws);
    assert!(harness.take_writes().contains(PI_CLEAR_AND_REPLAY));
}

#[test]
fn pi_taller_than_viewport_kitty_image_avoids_cursor_up_placement() {
    let image = kitty_image(3, 6, "AAAA");
    let mut lines = vec![image.clone()];
    lines.extend((0..5).map(|_| String::new()));
    lines.push("after".into());
    let mut harness = Harness::new(40, 5, lines);
    harness.start();
    let output = harness.take_writes();
    assert!(output.contains(&image));
    assert!(!output.contains(&format!("\x1b[5A{image}")), "{output:?}");
}

#[test]
fn pi_changed_kitty_placement_is_deleted_before_redraw() {
    let old = kitty_image(42, 2, "AAAA");
    let mut harness = Harness::new(40, 10, vec!["top".into(), old, "".into()]);
    harness.start();
    harness.clear_writes();
    let new = kitty_image(42, 1, "BBBB");
    *harness.lines.borrow_mut() = vec![new.clone(), "".into()];
    harness.render();
    let output = harness.take_writes();
    let delete = output.find(&delete_kitty_image(42)).expect("Kitty delete");
    let draw = output.find(&new).expect("Kitty redraw");
    assert!(delete < draw, "{output:?}");
}

#[test]
fn pi_full_replay_reserves_kitty_rows_before_drawing() {
    let mut harness = Harness::new(40, 5, numbered("Line", 5));
    harness.start();
    let redraws = harness.tui.full_redraws();
    harness.clear_writes();
    let image = kitty_image(4, 3, "AAAA");
    let mut lines = numbered("Line", 5);
    lines.extend([image.clone(), String::new(), String::new(), "after".into()]);
    *harness.lines.borrow_mut() = lines;
    harness.render();
    let output = harness.take_writes();
    assert!(harness.tui.full_redraws() > redraws);
    assert!(output.contains(PI_CLEAR_AND_REPLAY));
    assert!(
        output.contains(&format!("\r\n\r\n\x1b[2A{image}\x1b[2B")),
        "{output:?}"
    );
}

#[test]
fn pi_reserved_row_change_deletes_and_redraws_kitty_image() {
    let image = kitty_image(88, 2, "AAAA");
    let mut harness = Harness::new(40, 10, vec![String::new(), image.clone(), String::new()]);
    harness.start();
    harness.clear_writes();
    *harness.lines.borrow_mut() = vec!["covered".into(), image.clone(), String::new()];
    harness.render();
    let output = harness.take_writes();
    let delete = output.find(&delete_kitty_image(88)).expect("Kitty delete");
    let draw = output.find(&image).expect("Kitty redraw");
    assert!(delete < draw, "{output:?}");
    assert!(!output.contains("\x1b[2J"), "{output:?}");
}

#[test]
fn pi_full_replay_deletes_previous_kitty_placement() {
    let image = kitty_image(77, 2, "AAAA");
    let mut harness = Harness::new(40, 10, vec![image, "".into()]);
    harness.start();
    harness.clear_writes();
    *harness.lines.borrow_mut() = vec!["plain text".into()];
    harness.tui.request_render_force(true);
    let output = harness.take_writes();
    let delete = output.find(&delete_kitty_image(77)).expect("Kitty delete");
    let clear = output.find("\x1b[2J").expect("screen clear");
    assert!(delete < clear, "{output:?}");
}

#[test]
fn pi_cursor_marker_positions_ime_after_the_synchronized_frame() {
    let mut harness = Harness::new(20, 5, vec![format!("界{CURSOR_MARKER}x")]);
    harness.start();
    let output = harness.take_writes();
    assert!(!output.contains(CURSOR_MARKER));
    let end = output.find(PI_SYNC_END).unwrap();
    let cursor = output.find("\x1b[3G").unwrap();
    assert!(end < cursor, "{output:?}");
}

#[test]
fn pi_forced_initial_render_clears_before_replay() {
    let mut harness = Harness::new(20, 5, vec!["frame".into()]);
    harness.tui.request_render_force(true);
    harness.start();
    assert!(harness.take_writes().contains(PI_CLEAR_AND_REPLAY));
}

#[test]
fn every_pi_frame_is_synchronized_even_without_capability_detection() {
    let mut harness = Harness::new(20, 5, vec!["frame".into()]);
    harness.start();
    let output = harness.take_writes();
    assert!(output.contains(PI_SYNC_BEGIN));
    assert!(output.contains(PI_SYNC_END));
}
