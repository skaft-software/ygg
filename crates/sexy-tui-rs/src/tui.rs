//! Retained component tree with line-differential terminal rendering.
use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::scrollback::reset_and_replay;
use crate::terminal::{key_to_string, Terminal, TerminalInput};
use crate::utils::visible_width;

/// Zero-width APC escape sequence used as a cursor position marker.
/// Pi's zero-cell APC cursor marker.
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Whether a rendered row carries a Kitty graphics placement.
pub(crate) fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G")
}

/// Kitty graphics protocol escape that deletes every placed image. Destructive
/// inline replays must emit it before rebuilding rows they may have erased.
pub(crate) fn delete_all_kitty_images() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_string()
}

const KITTY_SEQUENCE_PREFIX: &str = "\x1b_G";
const PI_LINE_RESET: &str = "\x1b[0m\x1b]8;;\x07";

#[derive(Debug, Default)]
struct KittyImageHeader {
    ids: Vec<u32>,
    rows: usize,
}

fn parse_kitty_image_header(line: &str) -> Option<KittyImageHeader> {
    let sequence_start = line.find(KITTY_SEQUENCE_PREFIX)?;
    let params_start = sequence_start.saturating_add(KITTY_SEQUENCE_PREFIX.len());
    let params_end = line[params_start..].find(';')?.saturating_add(params_start);
    let mut header = KittyImageHeader {
        ids: Vec::new(),
        rows: 1,
    };
    for parameter in line[params_start..params_end].split(',') {
        let Some((key, value)) = parameter.split_once('=') else {
            continue;
        };
        let Ok(value) = value.parse::<u32>() else {
            continue;
        };
        if value == 0 {
            continue;
        }
        match key {
            "i" => header.ids.push(value),
            "r" => header.rows = value as usize,
            _ => {}
        }
    }
    Some(header)
}

fn extract_kitty_image_ids(line: &str) -> Vec<u32> {
    parse_kitty_image_header(line)
        .map(|header| header.ids)
        .unwrap_or_default()
}

fn extract_kitty_image_rows(line: &str) -> usize {
    parse_kitty_image_header(line)
        .map(|header| header.rows)
        .unwrap_or(1)
}

fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

fn is_termux_session() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some()
}

/// Global input listener. Returning `Some` consumes the input event.
pub type InputListener<'a> = Box<dyn FnMut(&str) -> Option<String> + 'a>;

// =============================================================================
// Component Trait
// =============================================================================

/// Width-independent identity for a finalized semantic transcript boundary.
/// The renderer retains this cursor after the corresponding rows enter native
/// scrollback, then asks the component to map it into each new physical layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitCursor {
    pub generation: u64,
    pub block: u64,
    pub segment: u64,
}

/// A semantic commit cursor and its exclusive visual-row boundary in the
/// current frame layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitPosition {
    pub cursor: CommitCursor,
    pub row: usize,
}

/// Commit handshake for the native-scrollback renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PinnedFrame {
    /// Semantic timeline containing this frame.
    pub generation: u64,
    /// Current-layout position of the cursor supplied to
    /// [`Component::render_update_with_cursor`].
    pub acknowledged: Option<CommitPosition>,
    /// Furthest finalized semantic boundary that may enter scrollback this
    /// frame.
    pub target: Option<CommitPosition>,
    /// Exclusive current-layout row boundary proven immutable. Physical rows
    /// may enter terminal history up to this seam before a coarser semantic
    /// `target` can be acknowledged.
    pub stable_rows: usize,
    /// The visible tail is a temporary, screen-relative surface rather than an
    /// extension of the append-only transcript tape. While this is set, repaint
    /// the surface in place without advancing native history; the retained
    /// transcript ledger is reconciled when the surface closes.
    pub viewport_surface: bool,
}

/// A lazy replacement for the mutable tail of a retained frame. Lines before
/// `stable_prefix` are guaranteed byte-identical to the previous frame, so the
/// TUI can reuse them without cloning or comparing a long committed history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameUpdate {
    pub stable_prefix: usize,
    pub replacement: Vec<String>,
    /// Semantic commit metadata for the native-scrollback renderer. `None`
    /// keeps the generic shell-style differential renderer.
    pub pinned: Option<PinnedFrame>,
    /// Complete application-owned frame to replay on a destructive resize
    /// before the visible `replacement` is repainted. This lets a temporary
    /// screen surface obscure transcript rows without becoming the source of
    /// truth for rebuilt scrollback. The alternate frame must produce the same
    /// number of off-screen rows as the displayed frame.
    pub resize_replay: Option<Vec<String>>,
    /// The component replaced its logical timeline (for example, a resumed
    /// conversation was replaced by a new session). Repaint the visible tail
    /// from the top of the terminal so later fixed-height chrome remains
    /// anchored to the physical bottom row.
    pub reanchor_viewport: bool,
    /// The presentation of committed rows changed. Generic inline frames may
    /// rebuild retained history; pinned frames preserve terminal-owned history
    /// and re-anchor only their live suffix.
    pub rebuild_scrollback: bool,
}

/// Exact row-level facts captured while the old retained frame is still
/// available. A lazy update moves that frame into the next frame, so terminal
/// writing must not try to rediscover these facts afterward.
#[derive(Debug)]
struct FrameChangeHints {
    first_changed: usize,
    fixed_height: Option<FixedHeightChangeHints>,
    affected_tail_has_image: bool,
}

#[derive(Debug)]
struct FixedHeightChangeHints {
    last_changed: Option<usize>,
    changed_rows: Vec<usize>,
    image_rows: Vec<usize>,
}

fn frame_change_hints(
    previous: &[String],
    stable_prefix: usize,
    replacement: &[String],
) -> FrameChangeHints {
    let previous_tail = &previous[stable_prefix..];
    let mut changed_rows = Vec::new();
    let mut image_rows = Vec::new();
    for (offset, (old, new)) in previous_tail.iter().zip(replacement).enumerate() {
        if old == new {
            continue;
        }
        let row = stable_prefix.saturating_add(offset);
        changed_rows.push(row);
        if is_image_line(old) || is_image_line(new) {
            image_rows.push(row);
        }
    }
    let shared_len = previous_tail.len().min(replacement.len());
    let first_changed = changed_rows
        .first()
        .copied()
        .unwrap_or_else(|| stable_prefix.saturating_add(shared_len));
    let changed_offset = first_changed.saturating_sub(stable_prefix);
    let affected_tail_has_image = previous_tail[changed_offset.min(previous_tail.len())..]
        .iter()
        .chain(replacement[changed_offset.min(replacement.len())..].iter())
        .any(|line| is_image_line(line));
    let fixed_height =
        (stable_prefix.saturating_add(replacement.len()) == previous.len()).then(|| {
            FixedHeightChangeHints {
                last_changed: changed_rows.last().copied(),
                changed_rows,
                image_rows,
            }
        });
    FrameChangeHints {
        first_changed,
        fixed_height,
        affected_tail_has_image,
    }
}

/// Component interface — all UI elements must implement this.
pub trait Component {
    /// Render the component to lines for the given viewport width.
    fn render(&self, width: u16) -> Vec<String>;

    /// Optionally render only the mutable frame tail. Implementations must
    /// return `None` after any change that invalidates the stable-prefix
    /// guarantee (for example a width change).
    fn render_update(&self, _width: u16) -> Option<FrameUpdate> {
        None
    }

    /// Render a lazy update while mapping the native-scrollback commit cursor
    /// retained by the TUI. Components without semantic commit points can keep
    /// implementing [`Component::render_update`].
    fn render_update_with_cursor(
        &self,
        width: u16,
        _cursor: Option<CommitCursor>,
    ) -> Option<FrameUpdate> {
        self.render_update(width)
    }

    /// Handle keyboard input when component has focus.
    fn handle_input(&mut self, _data: &str) {}

    /// Handle a bracketed-paste payload when component has focus.
    ///
    /// The default preserves legacy single-string behavior. Multiline editors
    /// can override this to keep paste atomic instead of replaying it as keys.
    fn handle_paste(&mut self, data: &str) {
        self.handle_input(data);
    }

    /// If true, component receives key release events (Kitty protocol).
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Invalidate any cached rendering state.
    fn invalidate(&mut self);
}

// =============================================================================
// TUI — Main Interface
// =============================================================================

/// Main TUI instance managing the render loop.
pub struct TUI<'a> {
    terminal: Box<dyn Terminal + 'a>,
    children: Vec<Box<dyn Component>>,
    previous_frame: Vec<String>,
    /// Terminal dimensions used for `previous_frame`. A resize invalidates all
    /// cursor-relative differential-rendering assumptions.
    previous_size: Option<(u16, u16)>,
    first_render: bool,
    running: bool,
    capabilities: crate::capabilities::TerminalCapabilities,
    input_listeners: Vec<InputListener<'a>>,
    /// Pi-compatible logical row containing the end of rendered content.
    cursor_row: usize,
    /// Pi-compatible logical row containing the physical terminal cursor.
    hardware_cursor_row: usize,
    /// Largest logical frame painted since the most recent full redraw.
    max_lines_rendered: usize,
    /// Logical row represented by the top of the previous terminal viewport.
    previous_viewport_top: usize,
    /// Number of Pi full-frame renders, exposed for parity regressions.
    full_redraw_count: usize,
    /// Pi's optional full-redraw-on-shrink policy.
    clear_on_shrink: bool,
    /// Pi tracks Kitty placements by image ID so redraws delete only affected
    /// images before retransmitting their rows.
    previous_kitty_image_ids: BTreeSet<u32>,
    /// Pi positions the hardware cursor for IME but hides it by default because
    /// editor components render their own visual cursor.
    show_hardware_cursor: bool,
    /// Render into the primary screen. The initial paint is limited to the
    /// visible tail; later appended lines can flow into native scrollback.
    /// Off-screen logical rows remain retained in `previous_frame`, so callers
    /// must keep committed lines byte-stable.
    inline_scrollback: bool,
    /// Screen row (0-based) currently showing `previous_frame`'s last line.
    /// A frame shrink cannot scroll the screen back down, so the frame's tail
    /// can sit above the bottom row; every inline repaint derives its cursor
    /// addressing from this anchor rather than assuming a bottom-aligned tail.
    inline_bottom_row: usize,
    /// Current-layout prefix already represented in terminal-owned history.
    /// Immutable physical rows can advance beyond the coarser semantic commit
    /// cursor. A destructive replay can also place provisional rows here, so
    /// the seam is tracked independently to prevent later acknowledgement from
    /// appending them twice.
    inline_history_rows: usize,
    /// Current-layout row corresponding to `inline_commit_cursor`. This is
    /// remapped from semantic identity on every update and is never reused
    /// across a width change.
    inline_committed_rows: usize,
    /// Last semantic boundary physically appended to native scrollback.
    inline_commit_cursor: Option<CommitCursor>,
    /// Semantic timeline owning the current replay/commit ledger. This remains
    /// known after a destructive replay even though its commit cursor is reset,
    /// so an immediate session replacement cannot inherit the old row seam.
    inline_generation: Option<u64>,
    /// First logical row represented by grid row zero in pinned mode.
    inline_window_top: usize,
    /// Temporary screen-relative tails (pickers, completion menus, reports, or
    /// a retreating streamed layout) repaint the grid without advancing the
    /// append-only transcript ledger. The physical rows are retained here for
    /// bounded differential updates until the semantic tape is re-anchored.
    inline_surface_active: bool,
    inline_surface_window: Vec<String>,
    /// Nested renderer helpers share one synchronized-output transaction so
    /// cursor placement becomes visible atomically with the frame.
    synchronized_output_depth: usize,
}

impl<'a> TUI<'a> {
    pub fn new(terminal: Box<dyn Terminal + 'a>) -> Self {
        let capabilities = terminal.capabilities();
        TUI {
            terminal,
            children: Vec::new(),
            previous_frame: Vec::new(),
            previous_size: None,
            first_render: true,
            running: false,
            capabilities,
            input_listeners: Vec::new(),
            cursor_row: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_viewport_top: 0,
            full_redraw_count: 0,
            clear_on_shrink: std::env::var_os("PI_CLEAR_ON_SHRINK")
                .is_some_and(|value| value == "1"),
            previous_kitty_image_ids: BTreeSet::new(),
            show_hardware_cursor: std::env::var_os("PI_HARDWARE_CURSOR")
                .is_some_and(|value| value == "1"),
            inline_scrollback: false,
            inline_bottom_row: 0,
            inline_history_rows: 0,
            inline_committed_rows: 0,
            inline_commit_cursor: None,
            inline_generation: None,
            inline_window_top: 0,
            inline_surface_active: false,
            inline_surface_window: Vec::new(),
            synchronized_output_depth: 0,
        }
    }

    /// Opt into inline scrollback rendering (see the field's invariants).
    pub fn set_inline_scrollback(&mut self, enabled: bool) {
        self.inline_scrollback = enabled;
    }

    /// Number of Pi-compatible full redraws performed by this TUI.
    pub fn full_redraws(&self) -> usize {
        self.full_redraw_count
    }

    /// Whether Pi's optional full redraw on frame shrink is enabled.
    pub fn clear_on_shrink(&self) -> bool {
        self.clear_on_shrink
    }

    /// Match Pi's `setClearOnShrink` runtime policy.
    pub fn set_clear_on_shrink(&mut self, enabled: bool) {
        self.clear_on_shrink = enabled;
    }

    /// Whether the hardware cursor is visible after IME positioning.
    pub fn show_hardware_cursor(&self) -> bool {
        self.show_hardware_cursor
    }

    /// Match Pi's hardware-cursor visibility policy.
    pub fn set_show_hardware_cursor(&mut self, enabled: bool) {
        if self.show_hardware_cursor == enabled {
            return;
        }
        self.show_hardware_cursor = enabled;
        if !enabled {
            self.terminal.hide_cursor();
        }
        self.request_render();
    }

    /// Set the terminal window title via OSC 2. Useful with inline
    /// scrollback, where no chrome row stays visible while the user scrolls
    /// history — the title bar is the one surface that always remains.
    pub fn set_window_title(&mut self, title: &str) {
        if self.capabilities.plain || !self.capabilities.interactive {
            return;
        }
        // OSC payloads must never contain control bytes; a stray BEL or ESC
        // would terminate or corrupt the sequence.
        let clean: String = title.chars().filter(|c| !c.is_control()).collect();
        self.terminal.write(&format!("\x1b]2;{clean}\x07"));
    }

    /// Add a component. Input and paste are delivered to the most recently
    /// added child, matching the single active-component usage of the shell.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    /// Remove a component by index.
    pub fn remove_child(&mut self, idx: usize) {
        if idx < self.children.len() {
            self.children.remove(idx);
        }
    }

    /// The child that currently receives input and paste events.
    fn active_child_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        self.children.last_mut()
    }

    /// Add an input listener for global key handling.
    pub fn add_input_listener(&mut self, f: InputListener<'a>) {
        self.input_listeners.push(f);
    }

    /// Request a re-render at the next opportunity.
    pub fn request_render(&mut self) {
        self.request_render_force(false);
    }

    /// Match Pi's forced-render path by invalidating every retained cursor and
    /// viewport assumption before rendering.
    pub fn request_render_force(&mut self, force: bool) {
        if force {
            self.previous_frame.clear();
            self.previous_size = Some((u16::MAX, u16::MAX));
            self.cursor_row = 0;
            self.hardware_cursor_row = 0;
            self.max_lines_rendered = 0;
            self.previous_viewport_top = 0;
        }
        if self.running {
            self.render_frame();
        }
    }

    /// Start the TUI render loop.
    pub fn start(&mut self) {
        self.running = true;
        if self.capabilities.interactive {
            self.terminal.hide_cursor();
        }

        // Perform first render
        self.render_frame();

        // Input/event loop is handled externally by the caller
        // (matching pi-tui's architecture where the consumer drives the loop)
    }

    /// Stop the TUI render loop.
    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        self.running = false;
        if self.uses_pi_renderer() {
            // Pi clears the editor's inverted fake cursor, moves to the line
            // after the complete logical frame, and only then restores the
            // process terminal.
            if !self.previous_frame.is_empty() {
                self.terminal.write(" ");
                let target_row = self.previous_frame.len();
                let mut buffer = String::new();
                push_vertical_move(
                    &mut buffer,
                    signed_difference(target_row, self.hardware_cursor_row),
                );
                buffer.push_str("\r\n");
                self.terminal.write(&buffer);
                self.hardware_cursor_row = target_row;
            }
            self.terminal.show_cursor();
            self.terminal.stop();
            return;
        }
        // Close any interrupted synchronized frame and all text/hyperlink
        // styling before restoring the backend. Repeated backend cleanup is
        // expected to be idempotent.
        if self.capabilities.synchronized_output {
            self.terminal.write("\x1b[?2026l");
            self.synchronized_output_depth = 0;
        }
        if !self.capabilities.plain {
            // Inline scrollback paints a mutable frame on the primary screen.
            // Its editor marker can leave the hardware cursor in the middle of
            // that frame. Anchor it at the final painted row before handing
            // the terminal back so the caller's normal-mode cleanup can move
            // to a fresh line without letting the shell prompt overwrite the
            // composer while leaving its footer behind.
            if self.inline_scrollback
                && self.capabilities.cursor_addressing
                && !self.previous_frame.is_empty()
            {
                self.terminal.write(&format!(
                    "\x1b[{};1H",
                    self.inline_bottom_row.saturating_add(1)
                ));
            }
            self.terminal.write("\x1b[0m\x1b]8;;\x1b\\");
            self.terminal.show_cursor();
        }
        self.terminal.stop();
    }

    /// Process input data. Should be called by the consumer's event loop.
    pub fn handle_input(&mut self, data: &str) {
        // Run input listeners first
        for listener in &mut self.input_listeners {
            if let Some(_modified) = listener(data) {
                // Listener consumed/modified the input
                return;
            }
        }

        if let Some(child) = self.active_child_mut() {
            child.handle_input(data);
        }

        self.request_render();
    }

    /// Route semantic terminal input without serializing printable keys into
    /// escape strings.  In particular, bracketed paste stays atomic until the
    /// focused component decides how to insert it.
    pub fn handle_terminal_input(&mut self, input: TerminalInput) {
        match input {
            TerminalInput::Text(text) => self.handle_input(&text),
            TerminalInput::Key(key) => {
                if let Some(control) = key_to_string(&key) {
                    self.handle_input(&control);
                }
            }
            TerminalInput::Paste(text) => {
                // Existing listeners receive the exact payload for backwards
                // compatibility. A consumed paste must not reach the editor.
                for listener in &mut self.input_listeners {
                    if listener(&text).is_some() {
                        return;
                    }
                }
                if let Some(child) = self.active_child_mut() {
                    child.handle_paste(&text);
                }
                self.request_render();
            }
        }
    }

    fn uses_pi_renderer(&self) -> bool {
        !self.capabilities.plain
            && !self.inline_scrollback
            && self.capabilities.cursor_addressing
            && self.capabilities.line_clearing
    }

    /// Render the current frame. Interactive non-inline terminals use Pi's
    /// normative differential algorithm; plain output and the explicit legacy
    /// inline extension retain their separate compatibility contracts.
    fn render_frame(&mut self) {
        if self.uses_pi_renderer() {
            self.render_pi_frame();
        } else {
            self.render_extended_frame();
        }
    }

    /// Rust port of Pi TUI's `doRender()` at revision
    /// `20be4b18d4c57487f8993d2762bace129f0cf7c6`.
    /// Keep this control flow structurally aligned with
    /// `packages/tui/src/tui.ts`; named upstream cases live in
    /// `tests/pi_tui_render.rs`. Ygg-specific native-scrollback policy belongs
    /// only to the explicit `inline_scrollback` compatibility path below.
    fn render_pi_frame(&mut self) {
        let width_u16 = self.terminal.columns();
        let height_u16 = self.terminal.rows().max(1);
        let width = usize::from(width_u16);
        let height = usize::from(height_u16);
        let previous_width = self.previous_size.map_or(0, |size| size.0);
        let previous_height = self.previous_size.map_or(0, |size| size.1);
        let width_changed = previous_width != 0 && previous_width != width_u16;
        let height_changed = previous_height != 0 && previous_height != height_u16;
        let previous_buffer_length = if previous_height > 0 {
            self.previous_viewport_top
                .saturating_add(usize::from(previous_height))
        } else {
            height
        };
        let mut previous_viewport_top = if height_changed {
            previous_buffer_length.saturating_sub(height)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = previous_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        let mut new_lines = self.root_render(width_u16);
        let cursor_position = extract_pi_cursor_position(&mut new_lines, height);
        for line in &mut new_lines {
            if !is_image_line(line) {
                *line = format!(
                    "{}{}",
                    crate::utils::normalize_terminal_output(line),
                    PI_LINE_RESET
                );
            }
        }

        // Pi's first render writes the complete frame without touching saved
        // lines. Subsequent structural fallbacks clear and replay it.
        if self.previous_frame.is_empty() && !width_changed && !height_changed {
            self.pi_full_render(new_lines, width_u16, height_u16, false, cursor_position);
            return;
        }
        if width_changed {
            self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
            return;
        }
        if height_changed && !is_termux_session() {
            self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
            return;
        }
        if self.clear_on_shrink && new_lines.len() < self.max_lines_rendered && !self.first_render {
            self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
            return;
        }

        let mut first_changed = None;
        let mut last_changed = None;
        let max_lines = new_lines.len().max(self.previous_frame.len());
        for index in 0..max_lines {
            let old_line = self.previous_frame.get(index).map_or("", String::as_str);
            let new_line = new_lines.get(index).map_or("", String::as_str);
            if old_line != new_line {
                first_changed.get_or_insert(index);
                last_changed = Some(index);
            }
        }
        let appended_lines = new_lines.len() > self.previous_frame.len();
        if appended_lines {
            first_changed.get_or_insert(self.previous_frame.len());
            last_changed = new_lines.len().checked_sub(1);
        }
        if let (Some(first), Some(last)) = (first_changed, last_changed) {
            let (expanded_first, expanded_last) =
                self.pi_expand_changed_range_for_kitty_images(first, last, &new_lines);
            first_changed = Some(expanded_first);
            last_changed = Some(expanded_last);
        }
        let append_start = appended_lines
            && first_changed == Some(self.previous_frame.len())
            && first_changed.is_some_and(|index| index > 0);

        if first_changed.is_none() {
            self.pi_position_hardware_cursor(cursor_position, new_lines.len());
            self.previous_viewport_top = previous_viewport_top;
            self.previous_size = Some((width_u16, height_u16));
            self.first_render = false;
            return;
        }
        let first_changed = first_changed.expect("checked above");
        let last_changed = last_changed.expect("a changed frame has a last row");

        // All changes are deleted rows. Clear those cells without scrolling
        // unless the target moved above the old viewport, where Pi rebuilds.
        if first_changed >= new_lines.len() {
            if self.previous_frame.len() > new_lines.len() {
                let target_row = new_lines.len().saturating_sub(1);
                if target_row < previous_viewport_top {
                    self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
                    return;
                }
                let extra_lines = self.previous_frame.len().saturating_sub(new_lines.len());
                if extra_lines > height {
                    self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
                    return;
                }

                let mut buffer = String::from("\x1b[?2026h");
                buffer.push_str(&self.pi_delete_changed_kitty_images(first_changed, last_changed));
                push_vertical_move(
                    &mut buffer,
                    pi_line_difference(
                        hardware_cursor_row,
                        previous_viewport_top,
                        target_row,
                        viewport_top,
                    ),
                );
                buffer.push('\r');
                let clear_start_offset = usize::from(!new_lines.is_empty());
                if extra_lines > 0 && clear_start_offset > 0 {
                    push_cursor_down(&mut buffer, clear_start_offset);
                }
                for index in 0..extra_lines {
                    buffer.push_str("\r\x1b[2K");
                    if index + 1 < extra_lines {
                        push_cursor_down(&mut buffer, 1);
                    }
                }
                let move_back = extra_lines
                    .saturating_sub(1)
                    .saturating_add(clear_start_offset);
                if move_back > 0 {
                    push_cursor_up(&mut buffer, move_back);
                }
                buffer.push_str("\x1b[?2026l");
                self.terminal.write(&buffer);
                self.cursor_row = target_row;
                self.hardware_cursor_row = target_row;
            }
            self.pi_position_hardware_cursor(cursor_position, new_lines.len());
            self.previous_frame = new_lines;
            self.previous_kitty_image_ids = Self::pi_collect_kitty_image_ids(&self.previous_frame);
            self.previous_size = Some((width_u16, height_u16));
            self.previous_viewport_top = previous_viewport_top;
            self.first_render = false;
            return;
        }

        if first_changed < previous_viewport_top {
            self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
            return;
        }

        let mut buffer = String::from("\x1b[?2026h");
        buffer.push_str(&self.pi_delete_changed_kitty_images(first_changed, last_changed));
        let previous_viewport_bottom = previous_viewport_top.saturating_add(height - 1);
        let move_target_row = if append_start {
            first_changed.saturating_sub(1)
        } else {
            first_changed
        };
        if move_target_row > previous_viewport_bottom {
            let current_screen_row = hardware_cursor_row
                .saturating_sub(previous_viewport_top)
                .min(height - 1);
            let move_to_bottom = height.saturating_sub(1).saturating_sub(current_screen_row);
            if move_to_bottom > 0 {
                push_cursor_down(&mut buffer, move_to_bottom);
            }
            let scroll = move_target_row.saturating_sub(previous_viewport_bottom);
            buffer.push_str(&"\r\n".repeat(scroll));
            previous_viewport_top = previous_viewport_top.saturating_add(scroll);
            viewport_top = viewport_top.saturating_add(scroll);
            hardware_cursor_row = move_target_row;
        }

        push_vertical_move(
            &mut buffer,
            pi_line_difference(
                hardware_cursor_row,
                previous_viewport_top,
                move_target_row,
                viewport_top,
            ),
        );
        buffer.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last_changed.min(new_lines.len().saturating_sub(1));
        let mut index = first_changed;
        while index <= render_end {
            if index > first_changed {
                buffer.push_str("\r\n");
            }
            let line = &new_lines[index];
            let image = is_image_line(line);
            let image_reserved_rows = if image {
                self.pi_kitty_image_reserved_rows(&new_lines, index, render_end)
            } else {
                1
            };
            if image_reserved_rows > 1 {
                let image_start_screen_row = index.checked_sub(viewport_top);
                if image_start_screen_row
                    .is_none_or(|row| row.saturating_add(image_reserved_rows) > height)
                {
                    self.pi_full_render(new_lines, width_u16, height_u16, true, cursor_position);
                    return;
                }
                buffer.push_str("\x1b[2K");
                for _ in 1..image_reserved_rows {
                    buffer.push_str("\r\n\x1b[2K");
                }
                push_cursor_up(&mut buffer, image_reserved_rows - 1);
                buffer.push_str(line);
                push_cursor_down(&mut buffer, image_reserved_rows - 1);
                index = index.saturating_add(image_reserved_rows);
                continue;
            }

            buffer.push_str("\x1b[2K");
            if !image && visible_width(line) > width {
                self.stop();
                panic!(
                    "rendered line {index} exceeds terminal width ({} > {width}); components must wrap or truncate to the supplied width",
                    visible_width(line)
                );
            }
            buffer.push_str(line);
            index = index.saturating_add(1);
        }

        let mut final_cursor_row = render_end;
        if self.previous_frame.len() > new_lines.len() {
            if render_end < new_lines.len().saturating_sub(1) {
                let move_down = new_lines.len() - 1 - render_end;
                push_cursor_down(&mut buffer, move_down);
                final_cursor_row = new_lines.len() - 1;
            }
            let extra_lines = self.previous_frame.len().saturating_sub(new_lines.len());
            for _ in new_lines.len()..self.previous_frame.len() {
                buffer.push_str("\r\n\x1b[2K");
            }
            push_cursor_up(&mut buffer, extra_lines);
        }
        buffer.push_str("\x1b[?2026l");
        self.terminal.write(&buffer);

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_cursor_row;
        self.max_lines_rendered = self.max_lines_rendered.max(new_lines.len());
        self.previous_viewport_top =
            previous_viewport_top.max(final_cursor_row.saturating_sub(height.saturating_sub(1)));
        self.pi_position_hardware_cursor(cursor_position, new_lines.len());
        self.previous_kitty_image_ids = Self::pi_collect_kitty_image_ids(&new_lines);
        self.previous_frame = new_lines;
        self.previous_size = Some((width_u16, height_u16));
        self.first_render = false;
    }

    fn pi_full_render(
        &mut self,
        new_lines: Vec<String>,
        width: u16,
        height: u16,
        clear: bool,
        cursor_position: Option<(usize, usize)>,
    ) {
        self.full_redraw_count = self.full_redraw_count.saturating_add(1);
        let height_rows = usize::from(height.max(1));
        let mut buffer = String::from("\x1b[?2026h");
        if clear {
            buffer.push_str(&self.pi_delete_kitty_images(&self.previous_kitty_image_ids));
            buffer.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        let mut index = 0;
        while index < new_lines.len() {
            if index > 0 {
                buffer.push_str("\r\n");
            }
            let line = &new_lines[index];
            let image_reserved_rows = if is_image_line(line) {
                self.pi_kitty_image_reserved_rows(
                    &new_lines,
                    index,
                    new_lines.len().saturating_sub(1),
                )
            } else {
                1
            };
            if image_reserved_rows > 1 && image_reserved_rows <= height_rows {
                buffer.push_str(&"\r\n".repeat(image_reserved_rows - 1));
                push_cursor_up(&mut buffer, image_reserved_rows - 1);
                buffer.push_str(line);
                push_cursor_down(&mut buffer, image_reserved_rows - 1);
                index = index.saturating_add(image_reserved_rows);
                continue;
            }
            buffer.push_str(line);
            index = index.saturating_add(1);
        }
        buffer.push_str("\x1b[?2026l");
        self.terminal.write(&buffer);

        self.cursor_row = new_lines.len().saturating_sub(1);
        self.hardware_cursor_row = self.cursor_row;
        self.max_lines_rendered = if clear {
            new_lines.len()
        } else {
            self.max_lines_rendered.max(new_lines.len())
        };
        let buffer_length = height_rows.max(new_lines.len());
        self.previous_viewport_top = buffer_length.saturating_sub(height_rows);
        self.pi_position_hardware_cursor(cursor_position, new_lines.len());
        self.previous_kitty_image_ids = Self::pi_collect_kitty_image_ids(&new_lines);
        self.previous_frame = new_lines;
        self.previous_size = Some((width, height));
        self.first_render = false;
    }

    fn pi_position_hardware_cursor(
        &mut self,
        cursor_position: Option<(usize, usize)>,
        total_lines: usize,
    ) {
        let Some((row, column)) = cursor_position.filter(|_| total_lines > 0) else {
            self.terminal.hide_cursor();
            return;
        };
        let target_row = row.min(total_lines.saturating_sub(1));
        let mut buffer = String::new();
        push_vertical_move(
            &mut buffer,
            signed_difference(target_row, self.hardware_cursor_row),
        );
        buffer.push_str(&format!("\x1b[{}G", column.saturating_add(1)));
        self.terminal.write(&buffer);
        self.hardware_cursor_row = target_row;
        if self.show_hardware_cursor {
            self.terminal.show_cursor();
        } else {
            self.terminal.hide_cursor();
        }
    }

    fn pi_collect_kitty_image_ids(lines: &[String]) -> BTreeSet<u32> {
        lines
            .iter()
            .flat_map(|line| extract_kitty_image_ids(line))
            .collect()
    }

    fn pi_delete_kitty_images(&self, ids: &BTreeSet<u32>) -> String {
        ids.iter()
            .map(|image_id| delete_kitty_image(*image_id))
            .collect()
    }

    fn pi_kitty_image_reserved_rows(
        &self,
        lines: &[String],
        index: usize,
        max_index: usize,
    ) -> usize {
        let rows = lines
            .get(index)
            .map_or(1, |line| extract_kitty_image_rows(line));
        if rows <= 1 {
            return 1;
        }
        let max_rows = rows
            .min(max_index.saturating_sub(index).saturating_add(1))
            .min(lines.len().saturating_sub(index));
        let mut reserved_rows = 1;
        while reserved_rows < max_rows {
            let line = lines
                .get(index.saturating_add(reserved_rows))
                .map_or("", String::as_str);
            if is_image_line(line) || visible_width(line) > 0 {
                break;
            }
            reserved_rows = reserved_rows.saturating_add(1);
        }
        reserved_rows
    }

    fn pi_expand_changed_range_for_kitty_images(
        &self,
        first_changed: usize,
        last_changed: usize,
        new_lines: &[String],
    ) -> (usize, usize) {
        let mut expanded_first = first_changed;
        let mut expanded_last = last_changed;
        for lines in [&self.previous_frame, new_lines] {
            for index in 0..lines.len() {
                if extract_kitty_image_ids(&lines[index]).is_empty() {
                    continue;
                }
                let block_end = index
                    .saturating_add(self.pi_kitty_image_reserved_rows(
                        lines,
                        index,
                        lines.len() - 1,
                    ))
                    .saturating_sub(1);
                if index >= first_changed || (index <= last_changed && block_end >= first_changed) {
                    expanded_first = expanded_first.min(index);
                    expanded_last = expanded_last.max(block_end);
                }
            }
        }
        (expanded_first, expanded_last)
    }

    fn pi_delete_changed_kitty_images(&self, first_changed: usize, last_changed: usize) -> String {
        if last_changed < first_changed {
            return String::new();
        }
        let mut ids = BTreeSet::new();
        let Some(max_line) = self
            .previous_frame
            .len()
            .checked_sub(1)
            .map(|last| last.min(last_changed))
        else {
            return String::new();
        };
        for index in first_changed..=max_line {
            ids.extend(extract_kitty_image_ids(&self.previous_frame[index]));
        }
        self.pi_delete_kitty_images(&ids)
    }

    fn render_extended_frame(&mut self) {
        let width = self.terminal.columns();
        let height = self.terminal.rows();
        let size_changed = self
            .previous_size
            .is_some_and(|size| size != (width, height));
        let width_changed = self
            .previous_size
            .is_some_and(|(previous_width, _)| previous_width != width);

        let reset_scrollback_on_resize = size_changed
            && self.inline_scrollback
            && self.capabilities.cursor_addressing
            && self.capabilities.line_clearing;
        let render_commit_cursor = if reset_scrollback_on_resize {
            // The old cursor described history that the resize reset replaces.
            // Ask the component for a fresh boundary in the new layout.
            None
        } else {
            self.inline_commit_cursor
        };
        let lazy_update = (self.capabilities.plain
            || (self.inline_scrollback
                && self.capabilities.cursor_addressing
                && self.capabilities.line_clearing))
            .then(|| self.root_render_update(width, render_commit_cursor))
            .flatten();
        let previous_len = self.previous_frame.len();
        // A prefix beyond the retained frame cannot be validated. Fall back to
        // the component's full renderer rather than pairing its replacement
        // with the wrong historic rows. Width reflow likewise requires a full
        // replacement, but retaining a zero-prefix update preserves pinned
        // viewport metadata across both width and height changes.
        let mut lazy_update = lazy_update.filter(|update| {
            update.stable_prefix <= previous_len && (!width_changed || update.stable_prefix == 0)
        });
        let resize_replay = lazy_update
            .as_mut()
            .and_then(|update| update.resize_replay.take())
            .map(|mut lines| {
                let _ = extract_cursor_position(&mut lines, width, height);
                let mut lines = lines
                    .into_iter()
                    .map(|line| self.prepare_line(line, width))
                    .collect::<Vec<_>>();
                if !self.capabilities.plain {
                    for line in &mut lines {
                        line.push_str("\x1b[0m\x1b]8;;\x1b\\");
                    }
                }
                lines
            });
        let reanchor_viewport = lazy_update
            .as_ref()
            .is_some_and(|update| update.reanchor_viewport);
        let rebuild_scrollback = lazy_update
            .as_ref()
            .is_some_and(|update| update.rebuild_scrollback);
        // Kitty placements only need a full-frame presence check when a
        // destructive inline replay may erase them. Ordinary differential
        // frames inspect just the rows they repaint below.
        let previous_frame_has_image = (reset_scrollback_on_resize || rebuild_scrollback)
            && self.previous_frame.iter().any(|line| is_image_line(line));
        let pinned = lazy_update.as_ref().and_then(|update| update.pinned);
        // Lazy frame assembly reuses `previous_frame` with `mem::take` below.
        // Preserve only the old physical viewport needed by pinned diffing;
        // cloning the complete retained transcript would defeat lazy updates.
        let pinned_previous_window = pinned.map_or_else(Vec::new, |_| {
            let rows = usize::from(height.max(1));
            (0..rows)
                .map(|screen_row| {
                    let logical_row = self.inline_window_top.saturating_add(screen_row);
                    if logical_row < self.inline_history_rows {
                        String::new()
                    } else {
                        self.previous_frame
                            .get(logical_row)
                            .cloned()
                            .unwrap_or_default()
                    }
                })
                .collect()
        });
        let mut first_changed_hint = None;
        let mut lazy_change_hints = None;
        let cursor;

        let new_lines: Vec<String> = if let Some(update) = lazy_update {
            let stable_prefix = update.stable_prefix.min(previous_len);
            let mut replacement = update.replacement;
            let total_len = stable_prefix.saturating_add(replacement.len());
            cursor = extract_cursor_position_from(
                &mut replacement,
                stable_prefix,
                total_len,
                width,
                height,
            );
            let mut replacement = replacement
                .into_iter()
                .map(|line| self.prepare_line(line, width))
                .collect::<Vec<_>>();
            if !self.capabilities.plain {
                for line in &mut replacement {
                    line.push_str("\x1b[0m\x1b]8;;\x1b\\");
                }
            }
            let hints = frame_change_hints(&self.previous_frame, stable_prefix, &replacement);
            first_changed_hint = Some(hints.first_changed);
            lazy_change_hints = Some(hints);

            // Reuse the committed prefix in place. No historic String is
            // cloned and no committed row is compared on an active-run tick.
            let mut reused = std::mem::take(&mut self.previous_frame);
            reused.truncate(stable_prefix);
            reused.extend(replacement);
            reused
        } else {
            let mut rendered = self.root_render(width);
            // Extract the typed cursor marker before clipping or sanitizing the
            // line. It is a trusted library control token, never accepted from
            // semantic text.
            cursor = extract_cursor_position(&mut rendered, width, height);

            // Prepare the children in order. Plain/log mode is escape-free and
            // does not right-pad every row with terminal-width spaces. Inline
            // scrollback also skips padding: every repaint erases before
            // writing, and padded rows would put trailing spaces into native
            // text selection.
            rendered = rendered
                .into_iter()
                .map(|line| self.prepare_line(line, width))
                .collect();

            // Apply per-line resets only in terminal-control mode. Plain/log
            // backends receive escape-free chronological output.
            if !self.capabilities.plain {
                rendered = rendered
                    .into_iter()
                    .map(|line| format!("{}\x1b[0m\x1b]8;;\x1b\\", line))
                    .collect();
            }
            rendered
        };

        // Cursor movement caused by frame writes must not become visible before
        // the final hardware-cursor address. This is especially noticeable as
        // a transient hollow cursor in the terminal's bottom-right cell.
        self.begin_synchronized_output();

        // A terminal reflows the old grid and saved lines before delivering
        // its resize event. Rebuilding Ygg-owned history below avoids trying to
        // repair terminal-dependent physical rows after that reflow.
        if self.capabilities.plain {
            self.write_plain_changes(&new_lines, first_changed_hint, previous_len);
            self.first_render = false;
        } else if self.inline_scrollback
            && self.capabilities.cursor_addressing
            && self.capabilities.line_clearing
        {
            self.write_inline_changes(
                &new_lines,
                height,
                size_changed,
                reanchor_viewport,
                rebuild_scrollback,
                pinned,
                &pinned_previous_window,
                first_changed_hint,
                previous_len,
                previous_frame_has_image,
                lazy_change_hints.as_ref(),
                resize_replay.as_deref(),
            );
            self.first_render = false;
        } else if self.first_render {
            if self.capabilities.cursor_addressing {
                self.terminal.write("\x1b[H");
            }
            self.write_all_lines(&new_lines);
            self.first_render = false;
        } else if previous_len == 0 {
            self.write_all_lines(&new_lines);
        } else if size_changed {
            self.redraw_all_from_home(&new_lines);
        } else {
            // Strategy 3: update only the changed tail. This handles pure
            // append, replacement, shrink, and empty frames.
            let first_changed = first_changed_hint.unwrap_or_else(|| {
                self.previous_frame
                    .iter()
                    .zip(&new_lines)
                    .position(|(prev, new)| prev != new)
                    .unwrap_or(previous_len.min(new_lines.len()))
            });

            let old_viewport_start = previous_len.saturating_sub(usize::from(height));
            let new_viewport_start = new_lines.len().saturating_sub(usize::from(height));
            let viewport_shifted = old_viewport_start != new_viewport_start;
            if !self.capabilities.cursor_addressing || !self.capabilities.line_clearing {
                // A styled but non-addressable backend behaves like an append-only
                // log: never emit cursor/erase controls it did not advertise.
                self.write_all_lines(&new_lines);
            } else if (first_changed == 0 && previous_len != new_lines.len())
                || viewport_shifted
                || first_changed < new_viewport_start
            {
                self.redraw_all_from_home(&new_lines);
            } else if first_changed < previous_len || first_changed < new_lines.len() {
                self.begin_synchronized_output();
                let screen_row = first_changed.saturating_sub(new_viewport_start);
                self.terminal
                    .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
                self.terminal.clear_from_cursor();
                let changed = &new_lines[first_changed..];
                for (index, line) in changed.iter().enumerate() {
                    self.terminal.write(line);
                    // A newline after the terminal's bottom row scrolls the
                    // alternate screen and invalidates every absolute row in
                    // the retained frame. Cursor-addressed updates do not need
                    // a trailing newline after their final row.
                    if index + 1 < changed.len() {
                        self.terminal.write("\n");
                    }
                }
                if new_lines.len() < previous_len {
                    self.terminal.clear_from_cursor();
                }
                self.end_synchronized_output();
            }
        }

        if let Some((row, column)) = cursor.filter(|_| self.capabilities.cursor_addressing) {
            let row = if self.inline_scrollback && !self.capabilities.plain {
                // Re-anchor from the bottom-aligned viewport model to the
                // frame's true on-screen bottom row (a shrink can leave the
                // tail above the screen's last row).
                let viewport_start = new_lines.len().saturating_sub(usize::from(height));
                let logical = usize::from(row) + viewport_start;
                let from_end = new_lines.len().saturating_sub(1).saturating_sub(logical);
                self.inline_bottom_row.saturating_sub(from_end) as u16
            } else {
                row
            };
            self.terminal.write(&format!(
                "\x1b[{};{}H",
                row.saturating_add(1),
                column.saturating_add(1)
            ));
            self.terminal.show_cursor();
        } else if self.capabilities.cursor_addressing {
            self.terminal.hide_cursor();
        }
        self.end_synchronized_output();
        self.previous_frame = new_lines;
        self.previous_size = Some((width, height));
    }

    /// Differential update against the primary screen. Logical rows above the
    /// visible region are never repainted; rows appended after first paint can
    /// enter native scrollback when a bottom-row newline scrolls naturally.
    /// `inline_bottom_row` anchors all cursor addressing because a frame shrink
    /// leaves the tail above the bottom row (the screen cannot scroll back down).
    #[allow(clippy::too_many_arguments)]
    fn write_inline_changes(
        &mut self,
        new_lines: &[String],
        height: u16,
        size_changed: bool,
        reanchor_viewport: bool,
        rebuild_scrollback: bool,
        pinned: Option<PinnedFrame>,
        pinned_previous_window: &[String],
        first_changed_hint: Option<usize>,
        previous_len: usize,
        previous_frame_has_image: bool,
        frame_change_hints: Option<&FrameChangeHints>,
        resize_replay: Option<&[String]>,
    ) {
        let rows = usize::from(height.max(1));
        if size_changed && !self.first_render {
            let displayed_window_top = new_lines.len().saturating_sub(rows);
            let retained_generation = self
                .inline_generation
                .or_else(|| self.inline_commit_cursor.map(|cursor| cursor.generation));
            let generation_continues = pinned.is_none_or(|frame| {
                retained_generation.is_none_or(|generation| generation == frame.generation)
            });
            let preserve_native_history = pinned.filter(|frame| {
                generation_continues
                    && !frame.viewport_surface
                    && frame.stable_rows >= displayed_window_top
                    && resize_replay.is_none()
                    && !rebuild_scrollback
                    && !previous_frame_has_image
                    && !new_lines.iter().any(|line| is_image_line(line))
            });
            if let Some(pinned) = preserve_native_history {
                // The terminal has already reflowed its grid and saved lines.
                // Treat that reflowed prefix as the new physical history seam
                // and repaint only one complete visible grid. Replaying the
                // application tape here duplicates history in multiplexers and
                // makes resize cost proportional to the whole conversation.
                self.inline_history_rows = displayed_window_top;
                self.inline_committed_rows = 0;
                self.inline_commit_cursor = None;
                self.inline_generation = Some(pinned.generation);
                self.inline_window_top = displayed_window_top;
                self.inline_surface_active = false;
                self.inline_surface_window.clear();
                self.write_inline_pinned(
                    new_lines,
                    rows,
                    pinned,
                    true,
                    false,
                    pinned_previous_window,
                );
                return;
            }

            let reanchor_replacement_timeline = pinned.filter(|frame| {
                !generation_continues
                    && !frame.viewport_surface
                    && resize_replay.is_none()
                    && !previous_frame_has_image
                    && !new_lines.iter().any(|line| is_image_line(line))
            });
            if let Some(pinned) = reanchor_replacement_timeline {
                // The terminal may reflow its old saved lines, but they belong
                // to another semantic tape. Preserve that native history while
                // starting the replacement generation at row zero and repainting
                // its live grid; never claim the old off-screen prefix as new
                // history and never clear scrollback merely because resize and
                // timeline replacement arrived in the same frame.
                self.write_inline_pinned(
                    new_lines,
                    rows,
                    pinned,
                    true,
                    rebuild_scrollback,
                    pinned_previous_window,
                );
                return;
            }

            // A temporary surface or Kitty placement cannot be reconstructed
            // safely from terminal reflow alone. Keep the destructive replay
            // fallback for those bounded exceptional paths.
            // Modern terminals reflow both the grid and saved lines before the
            // application observes a resize. Physical-row repair cannot be
            // made terminal-independent, so discard that presentation and
            // replay the complete application-owned tape. A temporary screen
            // surface may provide the unobscured tape, then repaint its visible
            // frame without scrolling any additional rows.
            let displayed_window_top = new_lines.len().saturating_sub(rows);
            let replay = resize_replay.filter(|replay| {
                replay.len().saturating_sub(rows) == displayed_window_top
                    && !replay
                        .iter()
                        .chain(new_lines)
                        .any(|line| is_image_line(line))
            });
            let pinned_surface = pinned.is_some_and(|frame| frame.viewport_surface);
            self.reset_inline_scrollback(
                replay.unwrap_or(new_lines),
                rows,
                previous_frame_has_image,
            );
            self.inline_generation = pinned.map(|frame| frame.generation);
            if replay.is_some() {
                self.terminal.write("\x1b[H");
                let visible = &new_lines[displayed_window_top..];
                for (index, line) in visible.iter().enumerate() {
                    self.terminal.clear_line();
                    self.terminal.write(line);
                    if index + 1 < visible.len() {
                        self.terminal.write("\n");
                    }
                }
                for row in visible.len()..rows {
                    self.terminal
                        .write(&format!("\x1b[{};1H", row.saturating_add(1)));
                    self.terminal.clear_line();
                }
                self.inline_history_rows = displayed_window_top;
                self.inline_window_top = displayed_window_top;
                self.inline_bottom_row = visible.len().saturating_sub(1);
            }
            if pinned_surface {
                let visible = &new_lines[displayed_window_top..];
                self.inline_surface_window = visible.to_vec();
                self.inline_surface_window.resize(rows, String::new());
                self.inline_surface_active = true;
                self.inline_bottom_row = visible.len().saturating_sub(1);
            }
            return;
        }
        if rebuild_scrollback && !self.first_render && pinned.is_none() {
            // Generic inline frames have no semantic commit boundary, so a
            // disclosure rebuild must replace their complete presentation.
            self.reset_inline_scrollback(new_lines, rows, previous_frame_has_image);
            return;
        }
        if let Some(pinned) = pinned {
            self.write_inline_pinned(
                new_lines,
                rows,
                pinned,
                reanchor_viewport || rebuild_scrollback,
                rebuild_scrollback,
                pinned_previous_window,
            );
            return;
        }
        if self.first_render {
            // Push the caller's existing screen content into scrollback
            // instead of erasing it, then paint the visible tail from home.
            // The complete logical frame remains retained for differential
            // updates, but restoring a large session must not synchronously
            // stream megabytes of off-screen history through the PTY before
            // the composer becomes usable.
            self.terminal.write(&"\n".repeat(rows));
            self.terminal.write("\x1b[H");
            self.terminal.clear_screen();
            self.terminal.write("\x1b[H");
            let visible = &new_lines[new_lines.len().saturating_sub(rows)..];
            self.write_all_lines(visible);
            self.inline_bottom_row = visible.len().saturating_sub(1);
            return;
        }

        let prev_len = previous_len;
        // Frame lines currently on screen span [visible_start, prev_len).
        let visible_start = prev_len.saturating_sub(self.inline_bottom_row + 1);
        if reanchor_viewport || prev_len == 0 || new_lines.len() <= visible_start {
            // Reflow, an explicit logical-timeline replacement, or a frame
            // shrink past the on-screen region invalidates every row
            // assumption. Repaint the visible tail from home; history above
            // stays in scrollback at its old wrap.
            self.begin_synchronized_output();
            let erased_has_image = frame_change_hints.map_or_else(
                || {
                    self.previous_frame[visible_start..]
                        .iter()
                        .any(|line| is_image_line(line))
                },
                |hints| hints.affected_tail_has_image,
            );
            if erased_has_image {
                // Erasing text cells does not remove Kitty placements. The
                // complete new visible tail is painted below, so a global
                // delete cannot strand any unchanged on-screen image.
                self.terminal.write(&delete_all_kitty_images());
            }
            self.terminal.write("\x1b[H");
            let start = new_lines.len().saturating_sub(rows);
            let visible = &new_lines[start..];
            // ED 2 is not history-neutral in multiplexers such as tmux: cells
            // erased from the grid are retained as native scrollback. Erase
            // each physical row instead so a transient overlay, resize, or
            // timeline reanchor cannot commit mutable chrome.
            for (index, line) in visible.iter().enumerate() {
                self.terminal.clear_line();
                self.terminal.write(line);
                if index + 1 < visible.len() {
                    self.terminal.write("\n");
                }
            }
            for row in visible.len()..rows {
                self.terminal
                    .write(&format!("\x1b[{};1H", row.saturating_add(1)));
                self.terminal.clear_line();
            }
            self.end_synchronized_output();
            self.inline_bottom_row = visible.len().saturating_sub(1);
            return;
        }

        let first_changed = first_changed_hint.unwrap_or_else(|| {
            self.previous_frame
                .iter()
                .zip(new_lines)
                .position(|(prev, new)| prev != new)
                .unwrap_or(prev_len.min(new_lines.len()))
        });
        if first_changed >= prev_len && new_lines.len() == prev_len {
            return;
        }

        if first_changed < visible_start {
            // Rows already owned by native scrollback cannot be edited. Do not
            // clear and replay the retained timeline here: multiplexers may
            // preserve the old history and append that replay, while terminals
            // without synchronized paint expose it as a full-screen flash.
            // Align the old and new visible tails instead and repaint only the
            // physical rows whose final cells differ. Off-screen history keeps
            // the version that was committed when it originally scrolled out.
            self.repaint_inline_visible_rows(new_lines, rows);
            return;
        }

        let mut delete_images_before_repaint = false;

        // A fixed-height frame can change in the middle when an application
        // replaces elastic viewport padding with a newly arrived event. Repaint
        // only the changed rows in that case: clearing the entire tail would
        // needlessly erase and redraw pinned composer/footer rows and visibly
        // flickers on terminals without synchronized-output support.
        if new_lines.len() == prev_len {
            let fixed_height_hint =
                frame_change_hints.and_then(|hints| hints.fixed_height.as_ref());
            let last_changed = fixed_height_hint.map_or_else(
                || {
                    self.previous_frame
                        .iter()
                        .zip(new_lines)
                        .rposition(|(previous, next)| previous != next)
                },
                |hints| hints.last_changed,
            );
            if let Some(last_changed) = last_changed {
                let repaint_from = first_changed.max(visible_start);
                if repaint_from > last_changed {
                    return;
                }
                let changed_has_image = fixed_height_hint.map_or_else(
                    || {
                        (repaint_from..=last_changed).any(|index| {
                            self.previous_frame[index] != new_lines[index]
                                && (is_image_line(&self.previous_frame[index])
                                    || is_image_line(&new_lines[index]))
                        })
                    },
                    |hints| {
                        hints
                            .image_rows
                            .iter()
                            .any(|row| *row >= repaint_from && *row <= last_changed)
                    },
                );
                if !changed_has_image {
                    self.begin_synchronized_output();
                    if let Some(hints) = fixed_height_hint {
                        for &index in hints
                            .changed_rows
                            .iter()
                            .filter(|row| **row >= repaint_from && **row <= last_changed)
                        {
                            let from_end = prev_len.saturating_sub(1).saturating_sub(index);
                            let screen_row = self.inline_bottom_row.saturating_sub(from_end);
                            self.terminal
                                .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
                            self.terminal.clear_line();
                            self.terminal.write(&new_lines[index]);
                        }
                    } else {
                        let mut index = repaint_from;
                        while index <= last_changed {
                            if self.previous_frame[index] != new_lines[index] {
                                let from_end = prev_len.saturating_sub(1).saturating_sub(index);
                                let screen_row = self.inline_bottom_row.saturating_sub(from_end);
                                self.terminal
                                    .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
                                self.terminal.clear_line();
                                self.terminal.write(&new_lines[index]);
                            }
                            index = index.saturating_add(1);
                        }
                    }
                    self.end_synchronized_output();
                    return;
                }
                // Text erase controls do not remove Kitty graphics
                // placements. Delete them before the generic tail redraw and
                // repaint the complete visible viewport so unchanged images
                // removed by the global delete are restored as well.
                delete_images_before_repaint = true;
            }
        } else {
            // Length changes clear and rewrite the affected tail. Kitty image
            // placements survive those text controls, and retransmitting an
            // affected image without first deleting it can also leave stacked
            // placements. Repaint the complete visible viewport after a
            // global delete so unchanged visible images are restored too.
            let affected_from = first_changed
                .min(prev_len.saturating_sub(1))
                .max(visible_start);
            delete_images_before_repaint = frame_change_hints.map_or_else(
                || {
                    let affected_old_has_image = self.previous_frame[affected_from..]
                        .iter()
                        .any(|line| is_image_line(line));
                    let affected_new_has_image = new_lines[affected_from.min(new_lines.len())..]
                        .iter()
                        .any(|line| is_image_line(line));
                    affected_old_has_image || affected_new_has_image
                },
                |hints| hints.affected_tail_has_image,
            );
        }

        // Start at or before the last existing line so appends write a
        // newline from the current tail (scrolling as needed) rather than
        // addressing a row past the screen. A change above the visible
        // region cannot be painted (those rows are scrollback); clamp and
        // accept the stale history.
        let repaint_from = if delete_images_before_repaint {
            visible_start
        } else {
            first_changed
                .min(prev_len.saturating_sub(1))
                .max(visible_start)
        };
        let screen_row = self.inline_bottom_row - (prev_len - 1 - repaint_from);
        self.begin_synchronized_output();
        if delete_images_before_repaint {
            self.terminal.write(&delete_all_kitty_images());
        }
        self.terminal
            .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
        self.terminal.clear_from_cursor();
        let changed = &new_lines[repaint_from.min(new_lines.len())..];
        for (index, line) in changed.iter().enumerate() {
            self.terminal.write(line);
            if index + 1 < changed.len() {
                self.terminal.write("\n");
            }
        }
        self.end_synchronized_output();
        self.inline_bottom_row = if changed.is_empty() {
            screen_row.saturating_sub(1)
        } else {
            (screen_row + changed.len() - 1).min(rows - 1)
        };
    }

    /// Append-only native-scrollback renderer for a frame with semantic commit
    /// points. The acknowledged cursor is remapped into the current width, so
    /// no physical row coordinate survives terminal reflow. The mutable grid
    /// always starts at or after the independently tracked physical-history
    /// seam.
    fn write_inline_pinned(
        &mut self,
        new_lines: &[String],
        rows: usize,
        pinned: PinnedFrame,
        mut reanchor: bool,
        atomic_presentation_rebuild: bool,
        previous_window: &[String],
    ) {
        let desired_window_top = new_lines.len().saturating_sub(rows);
        let same_generation = self
            .inline_generation
            .or_else(|| self.inline_commit_cursor.map(|cursor| cursor.generation))
            .is_none_or(|generation| generation == pinned.generation);
        if !same_generation {
            // A replacement timeline starts a new append-only tape after the
            // old terminal-owned history. Its row zero is unrelated to the old
            // cursor, but the old history itself remains untouched.
            self.inline_commit_cursor = None;
            self.inline_history_rows = 0;
            self.inline_committed_rows = 0;
            self.inline_surface_active = false;
            self.inline_surface_window.clear();
            reanchor = true;
        }

        let acknowledged = self.inline_commit_cursor.and_then(|cursor| {
            pinned
                .acknowledged
                .filter(|position| position.cursor == cursor)
        });
        let cursor_unmapped = self.inline_commit_cursor.is_some() && acknowledged.is_none();
        debug_assert!(
            !cursor_unmapped,
            "component did not map the retained semantic commit cursor"
        );

        // The semantic cursor can lag a large finalized block while immutable
        // physical rows from that block move into history one at a time.
        let prior_history_rows = self
            .inline_history_rows
            .max(acknowledged.map_or(0, |position| position.row.min(new_lines.len())));

        // Temporary chrome and reports are physical-screen surfaces, not new
        // transcript tape. An ordinary streaming frame can also contract after
        // Markdown reparses. In either case, advancing or repainting before the
        // monotonic history seam would either commit chrome, duplicate history,
        // or punch blank rows into the live grid. Keep the append ledger frozen
        // and repaint the complete visible tail in place. Explicit presentation
        // rebuilds still honor their atomic semantic commit boundary below.
        if pinned.viewport_surface
            || (!atomic_presentation_rebuild && desired_window_top < prior_history_rows)
        {
            self.write_inline_viewport_surface(new_lines, rows, previous_window);
            self.inline_generation = Some(pinned.generation);
            return;
        }
        if self.inline_surface_active {
            reanchor = true;
        }

        let mut commit_row = acknowledged
            .map(|position| position.row.min(new_lines.len()))
            .unwrap_or_else(|| {
                if cursor_unmapped {
                    reanchor = true;
                    self.inline_committed_rows.min(new_lines.len())
                } else {
                    0
                }
            });
        let mut commit_cursor = self.inline_commit_cursor;
        let target = if cursor_unmapped { None } else { pinned.target }.filter(|target| {
            target.cursor.generation == pinned.generation
                && target.row >= commit_row
                && target.row <= desired_window_top
                && commit_cursor.is_none_or(|cursor| target.cursor > cursor)
        });

        // Stable rows may cross the seam incrementally. A semantic target is
        // also safe to stage once its complete boundary is above the live
        // viewport, even when rows inside that block are disclosure-sensitive.
        let append_limit = target.map_or(pinned.stable_rows, |target| {
            pinned.stable_rows.max(target.row)
        });
        let stable_rows = if cursor_unmapped {
            prior_history_rows
        } else {
            append_limit
                .max(acknowledged.map_or(0, |position| position.row))
                .min(desired_window_top)
                .max(prior_history_rows)
        };
        let append_start = prior_history_rows.min(new_lines.len());
        let append_end = stable_rows.min(new_lines.len());
        let appended = &new_lines[append_start..append_end];
        let history_rows = prior_history_rows.max(append_end);

        // Advance semantic identity only after its complete boundary is known
        // to be in physical history. A resize replay may already have put that
        // boundary there without an append in this frame.
        if let Some(target) = target.filter(|target| target.row <= history_rows) {
            commit_row = target.row;
            commit_cursor = Some(target.cursor);
        }

        // Ordinary streaming retreats use the temporary-surface path above.
        // An explicit semantic presentation rebuild may still contract behind
        // its atomic history boundary; those terminal-owned rows stay blank in
        // the live grid rather than being duplicated.
        let window_top = desired_window_top;
        let window_line = |screen_row: usize| {
            let logical_row = window_top.saturating_add(screen_row);
            (logical_row >= history_rows)
                .then(|| new_lines.get(logical_row))
                .flatten()
                .map(String::as_str)
                .unwrap_or("")
        };

        // When the old grid begins exactly at the physical history seam, its
        // stable top rows can enter scrollback with bottom-row newlines. This
        // is the terminal's native append operation: it preserves a reader's
        // scrollback anchor and avoids repainting the whole live grid.
        let can_scroll_naturally = !appended.is_empty()
            && !self.first_render
            && !reanchor
            && self.inline_window_top == prior_history_rows
            && appended.len() <= rows
            && previous_window.get(..appended.len()) == Some(appended);

        self.begin_synchronized_output();
        if self.first_render {
            // Preserve whatever preceded the application, then establish a
            // clean grid without erasing terminal-owned history.
            self.terminal.write(&"\n".repeat(rows));
        }

        if self.first_render || reanchor || (!appended.is_empty() && !can_scroll_naturally) {
            self.terminal.write("\x1b[H");
            if self.first_render {
                self.terminal.clear_screen();
                self.terminal.write("\x1b[H");
            }

            // A reanchor or a previously compressed mutable window may not
            // contain the rows now becoming stable. Stage that semantic chunk
            // above one complete grid so only the staged rows scroll out.
            let paint_len = appended.len().saturating_add(rows);
            for index in 0..paint_len {
                let line = if index < appended.len() {
                    appended[index].as_str()
                } else {
                    window_line(index - appended.len())
                };
                self.terminal.clear_line();
                self.terminal.write(line);
                if index + 1 < paint_len {
                    self.terminal.write("\r\n");
                }
            }
        } else {
            let shifted_rows = if can_scroll_naturally {
                // Address the live grid's bottom row before emitting newlines;
                // cursor placement from the prior differential frame is not a
                // reliable scroll origin.
                self.terminal
                    .write(&format!("\x1b[{rows};1H{}", "\r\n".repeat(appended.len())));
                appended.len()
            } else {
                0
            };

            // Compare against the grid after any natural scroll. Pure appends
            // now repaint only newly exposed bottom rows instead of replaying
            // every physical row at a shifted logical index.
            for screen_row in 0..rows {
                let previous = previous_window
                    .get(screen_row.saturating_add(shifted_rows))
                    .map(String::as_str)
                    .unwrap_or("");
                let next = window_line(screen_row);
                if previous == next {
                    continue;
                }
                self.terminal
                    .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
                self.terminal.clear_line();
                self.terminal.write(next);
            }
        }
        self.end_synchronized_output();

        self.inline_history_rows = history_rows;
        self.inline_committed_rows = commit_row;
        self.inline_commit_cursor = commit_cursor;
        self.inline_generation = Some(pinned.generation);
        self.inline_window_top = window_top;
        self.inline_surface_active = false;
        self.inline_surface_window.clear();
        self.inline_bottom_row = new_lines
            .len()
            .saturating_sub(window_top)
            .saturating_sub(1)
            .min(rows.saturating_sub(1));
    }

    fn write_inline_viewport_surface(
        &mut self,
        new_lines: &[String],
        rows: usize,
        previous_window: &[String],
    ) {
        let visible = &new_lines[new_lines.len().saturating_sub(rows)..];
        let visible_len = visible.len();
        let mut next_window = visible.to_vec();
        next_window.resize(rows, String::new());

        let previous = if self.inline_surface_active {
            self.inline_surface_window.clone()
        } else {
            previous_window.to_vec()
        };
        let delete_images = previous
            .iter()
            .zip(&next_window)
            .any(|(old, new)| old != new && (is_image_line(old) || is_image_line(new)))
            || previous
                .get(next_window.len()..)
                .is_some_and(|tail| tail.iter().any(|line| is_image_line(line)));
        let repaint_all = self.first_render || !self.inline_surface_active || delete_images;

        self.begin_synchronized_output();
        if self.first_render {
            // Preserve content that preceded the application, then establish a
            // clean primary-screen grid without committing any surface rows.
            self.terminal.write(&"\n".repeat(rows));
            self.terminal.write("\x1b[H");
            self.terminal.clear_screen();
            self.terminal.write("\x1b[H");
        }
        if delete_images {
            self.terminal.write(&delete_all_kitty_images());
        }
        for (screen_row, next) in next_window.iter().enumerate() {
            if !repaint_all && previous.get(screen_row) == Some(next) {
                continue;
            }
            self.terminal
                .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
            self.terminal.clear_line();
            self.terminal.write(next);
        }
        self.end_synchronized_output();

        self.inline_surface_active = true;
        self.inline_surface_window = next_window;
        self.inline_bottom_row = visible_len.saturating_sub(1).min(rows.saturating_sub(1));
    }

    fn repaint_inline_visible_rows(&mut self, new_lines: &[String], rows: usize) {
        let visible_rows = (self.inline_bottom_row + 1)
            .min(rows)
            .min(self.previous_frame.len())
            .min(new_lines.len());
        if visible_rows == 0 {
            return;
        }
        let previous_start = self.previous_frame.len() - visible_rows;
        let next_start = new_lines.len() - visible_rows;
        let previous = &self.previous_frame[previous_start..];
        let next = &new_lines[next_start..];
        let delete_images = previous
            .iter()
            .zip(next)
            .any(|(old, new)| old != new && (is_image_line(old) || is_image_line(new)));
        let changed = previous
            .iter()
            .zip(next)
            .enumerate()
            .filter(|(_, (old, new))| delete_images || old != new)
            .map(|(screen_row, (_, new))| (screen_row, new.clone()))
            .collect::<Vec<_>>();
        if changed.is_empty() {
            return;
        }

        self.begin_synchronized_output();
        if delete_images {
            self.terminal.write(&delete_all_kitty_images());
        }
        for (screen_row, new) in changed {
            self.terminal
                .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
            self.terminal.clear_line();
            self.terminal.write(&new);
        }
        self.end_synchronized_output();
    }

    /// Destructively replace terminal-owned history after a resize or an
    /// explicit generic presentation rebuild.
    fn reset_inline_scrollback(
        &mut self,
        new_lines: &[String],
        rows: usize,
        previous_frame_has_image: bool,
    ) {
        let window_top = new_lines.len().saturating_sub(rows);
        let delete_images =
            previous_frame_has_image || new_lines.iter().any(|line| is_image_line(line));

        self.begin_synchronized_output();
        reset_and_replay(
            self.terminal.as_mut(),
            delete_images,
            new_lines.iter().map(String::as_str),
        );
        self.end_synchronized_output();

        // The old semantic cursor referred to the discarded presentation.
        // The next frame negotiates a fresh cursor while `inline_history_rows`
        // prevents that acknowledgement from duplicating replayed rows.
        self.inline_history_rows = window_top;
        self.inline_committed_rows = 0;
        self.inline_commit_cursor = None;
        self.inline_generation = None;
        self.inline_window_top = window_top;
        self.inline_surface_active = false;
        self.inline_surface_window.clear();
        self.inline_bottom_row = new_lines
            .len()
            .saturating_sub(window_top)
            .saturating_sub(1)
            .min(rows.saturating_sub(1));
    }

    fn redraw_all_from_home(&mut self, lines: &[String]) {
        // `Clear(All)` does not universally home the cursor. Do both before
        // repainting so resize and line-count redraws cannot append a frame.
        if self.capabilities.cursor_addressing {
            self.terminal.write("\x1b[H");
        }
        self.terminal.clear_screen();
        if self.capabilities.cursor_addressing {
            self.terminal.write("\x1b[H");
        }
        self.write_all_lines(lines);
    }

    fn write_all_lines(&mut self, lines: &[String]) {
        self.begin_synchronized_output();
        if !self.first_render && self.previous_frame.iter().any(|line| is_image_line(line)) {
            self.terminal.write(&delete_all_kitty_images());
        }
        for (index, line) in lines.iter().enumerate() {
            self.terminal.write(line);
            // Keep append-only/non-addressable terminals line-delimited. An
            // addressable retained frame deliberately leaves its cursor on the
            // last row so a full-height frame cannot scroll by one line.
            if index + 1 < lines.len() || !self.capabilities.cursor_addressing {
                self.terminal.write("\n");
            }
        }
        self.end_synchronized_output();
    }

    fn write_plain_changes(
        &mut self,
        lines: &[String],
        first_changed_hint: Option<usize>,
        previous_len: usize,
    ) {
        let first_changed = if self.first_render {
            0
        } else {
            first_changed_hint.unwrap_or_else(|| {
                self.previous_frame
                    .iter()
                    .zip(lines)
                    .position(|(previous, next)| previous != next)
                    .unwrap_or(previous_len.min(lines.len()))
            })
        };
        for line in &lines[first_changed..] {
            self.terminal.write(line);
            self.terminal.write("\n");
        }
    }

    fn root_render_update(&self, width: u16, cursor: Option<CommitCursor>) -> Option<FrameUpdate> {
        // Lazy updates require exactly one child: a multi-child frame has no
        // single stable prefix to reuse.
        (self.children.len() == 1)
            .then(|| self.children[0].render_update_with_cursor(width, cursor))
            .flatten()
    }

    fn root_render(&self, width: u16) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn prepare_line(&self, line: String, width: u16) -> String {
        if self.capabilities.plain {
            ensure_plain_line(&line, width)
        } else if self.inline_scrollback {
            clip_line_width(&line, width)
        } else {
            ensure_line_width(&line, width)
        }
    }

    fn begin_synchronized_output(&mut self) {
        if !self.capabilities.synchronized_output {
            return;
        }
        if self.synchronized_output_depth == 0 {
            self.terminal.write("\x1b[?2026h");
        }
        self.synchronized_output_depth = self.synchronized_output_depth.saturating_add(1);
    }

    fn end_synchronized_output(&mut self) {
        if !self.capabilities.synchronized_output || self.synchronized_output_depth == 0 {
            return;
        }
        self.synchronized_output_depth -= 1;
        if self.synchronized_output_depth == 0 {
            self.terminal.write("\x1b[?2026l");
        }
    }
}

fn extract_pi_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        let Some(marker) = lines[row].find(CURSOR_MARKER) else {
            continue;
        };
        let column = visible_width(&lines[row][..marker]);
        lines[row].replace_range(marker..marker + CURSOR_MARKER.len(), "");
        return Some((row, column));
    }
    None
}

fn signed_difference(left: usize, right: usize) -> i64 {
    if left >= right {
        i64::try_from(left - right).unwrap_or(i64::MAX)
    } else {
        -i64::try_from(right - left).unwrap_or(i64::MAX)
    }
}

fn pi_line_difference(
    hardware_cursor_row: usize,
    previous_viewport_top: usize,
    target_row: usize,
    viewport_top: usize,
) -> i64 {
    let current_screen_row = signed_difference(hardware_cursor_row, previous_viewport_top);
    let target_screen_row = signed_difference(target_row, viewport_top);
    target_screen_row.saturating_sub(current_screen_row)
}

fn push_cursor_up(buffer: &mut String, rows: usize) {
    if rows > 0 {
        buffer.push_str(&format!("\x1b[{rows}A"));
    }
}

fn push_cursor_down(buffer: &mut String, rows: usize) {
    if rows > 0 {
        buffer.push_str(&format!("\x1b[{rows}B"));
    }
}

fn push_vertical_move(buffer: &mut String, rows: i64) {
    match rows.cmp(&0) {
        Ordering::Greater => push_cursor_down(buffer, rows as usize),
        Ordering::Less => push_cursor_up(buffer, rows.unsigned_abs() as usize),
        Ordering::Equal => {}
    }
}

impl Drop for TUI<'_> {
    fn drop(&mut self) {
        if self.running {
            self.stop();
        }
    }
}

/// Ensure a line is exactly `width` columns wide without splitting a grapheme
/// or an ANSI sequence.
fn ensure_line_width(line: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let visible = visible_width(line);
    match visible.cmp(&width) {
        Ordering::Less => format!("{}{}", line, " ".repeat(width - visible)),
        Ordering::Greater => crate::utils::truncate_to_width(line, width, Some("")),
        Ordering::Equal => line.to_owned(),
    }
}

/// Clip a line to `width` columns without right-padding it. Used by inline
/// scrollback mode, where trailing pad spaces would pollute native selection.
fn clip_line_width(line: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    if visible_width(line) > width {
        crate::utils::truncate_to_width(line, width, Some(""))
    } else {
        line.to_owned()
    }
}

fn ensure_plain_line(line: &str, width: u16) -> String {
    let mut safe = String::new();
    for (index, part) in line.split(CURSOR_MARKER).enumerate() {
        if index > 0 {
            safe.push_str(CURSOR_MARKER);
        }
        safe.push_str(&crate::sanitize::sanitize_line(part, true));
    }
    crate::utils::truncate_to_width(
        &safe,
        usize::from(width),
        Some(crate::GlyphSet::ASCII.ellipsis),
    )
}

fn extract_cursor_position(lines: &mut [String], width: u16, height: u16) -> Option<(u16, u16)> {
    let total_lines = lines.len();
    extract_cursor_position_from(lines, 0, total_lines, width, height)
}

fn extract_cursor_position_from(
    lines: &mut [String],
    row_offset: usize,
    total_lines: usize,
    width: u16,
    height: u16,
) -> Option<(u16, u16)> {
    let viewport_start = total_lines.saturating_sub(usize::from(height));
    let max_column = usize::from(width.saturating_sub(1));
    let mut cursor = None;
    for (local_row, line) in lines.iter_mut().enumerate() {
        let row = row_offset.saturating_add(local_row);
        while let Some(offset) = line.find(CURSOR_MARKER) {
            let column = visible_width(&line[..offset]).min(max_column) as u16;
            line.replace_range(offset..offset + CURSOR_MARKER.len(), "");
            if row >= viewport_start {
                cursor = Some(((row - viewport_start) as u16, column));
            }
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    struct RecordingTerminal {
        size: Rc<Cell<(u16, u16)>>,
        clears: Rc<Cell<usize>>,
        tail_clears: Rc<Cell<usize>>,
        stops: Rc<Cell<usize>>,
        shows: Rc<Cell<usize>>,
        writes: Rc<RefCell<Vec<String>>>,
        capabilities: crate::capabilities::TerminalCapabilities,
    }

    impl Terminal for RecordingTerminal {
        fn start_events(
            &mut self,
            _on_input: Box<dyn FnMut(TerminalInput)>,
            _on_resize: Box<dyn FnMut()>,
        ) {
        }
        fn stop(&mut self) {
            self.stops.set(self.stops.get() + 1);
        }
        fn write(&mut self, data: &str) {
            self.writes.borrow_mut().push(data.to_owned());
        }
        fn columns(&self) -> u16 {
            self.size.get().0
        }
        fn rows(&self) -> u16 {
            self.size.get().1
        }
        fn move_by(&mut self, _lines: i16) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {
            self.shows.set(self.shows.get() + 1);
        }
        fn clear_line(&mut self) {}
        fn clear_from_cursor(&mut self) {
            self.tail_clears.set(self.tail_clears.get() + 1);
        }
        fn clear_screen(&mut self) {
            self.clears.set(self.clears.get() + 1);
        }
        fn capabilities(&self) -> crate::capabilities::TerminalCapabilities {
            self.capabilities
        }
    }

    type RecordingParts = (
        RecordingTerminal,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
        Rc<Cell<usize>>,
        Rc<RefCell<Vec<String>>>,
    );

    fn recording_terminal(
        size: Rc<Cell<(u16, u16)>>,
        capabilities: crate::capabilities::TerminalCapabilities,
    ) -> RecordingParts {
        let clears = Rc::new(Cell::new(0));
        let tail_clears = Rc::new(Cell::new(0));
        let stops = Rc::new(Cell::new(0));
        let shows = Rc::new(Cell::new(0));
        let writes = Rc::new(RefCell::new(Vec::new()));
        (
            RecordingTerminal {
                size,
                clears: clears.clone(),
                tail_clears: tail_clears.clone(),
                stops: stops.clone(),
                shows: shows.clone(),
                writes: writes.clone(),
                capabilities,
            },
            clears,
            tail_clears,
            stops,
            shows,
            writes,
        )
    }

    fn test_commit_position(row: usize) -> CommitPosition {
        CommitPosition {
            cursor: CommitCursor {
                generation: 0,
                block: row as u64,
                segment: 0,
            },
            row,
        }
    }

    fn test_pinned_frame_with_stable(
        acknowledged: Option<CommitCursor>,
        target: Option<usize>,
        stable_rows: usize,
    ) -> PinnedFrame {
        PinnedFrame {
            generation: 0,
            acknowledged: acknowledged.map(|cursor| CommitPosition {
                row: cursor.block as usize,
                cursor,
            }),
            target: target.map(test_commit_position),
            stable_rows,
            viewport_surface: false,
        }
    }

    fn test_pinned_frame(acknowledged: Option<CommitCursor>, target: Option<usize>) -> PinnedFrame {
        test_pinned_frame_with_stable(acknowledged, target, target.unwrap_or(0))
    }

    struct OneLine;

    impl Component for OneLine {
        fn render(&self, _width: u16) -> Vec<String> {
            vec!["line".to_owned()]
        }

        fn invalidate(&mut self) {}
    }

    struct MutableLines(Rc<RefCell<Vec<String>>>);

    impl Component for MutableLines {
        fn render(&self, _width: u16) -> Vec<String> {
            self.0.borrow().clone()
        }

        fn invalidate(&mut self) {}
    }

    struct LazyTail {
        stable_prefix: usize,
        tail: Rc<RefCell<String>>,
        full_renders: Rc<Cell<usize>>,
        replacement_rows: Rc<Cell<usize>>,
    }

    impl Component for LazyTail {
        fn render(&self, _width: u16) -> Vec<String> {
            self.full_renders.set(self.full_renders.get() + 1);
            Vec::new()
        }

        fn render_update(&self, _width: u16) -> Option<FrameUpdate> {
            self.replacement_rows.set(1);
            Some(FrameUpdate {
                stable_prefix: self.stable_prefix,
                replacement: vec![self.tail.borrow().clone()],
                pinned: None,
                resize_replay: None,
                reanchor_viewport: false,
                rebuild_scrollback: false,
            })
        }

        fn invalidate(&mut self) {}
    }

    struct LazyFixedLines {
        lines: Rc<RefCell<Vec<String>>>,
    }

    impl Component for LazyFixedLines {
        fn render(&self, _width: u16) -> Vec<String> {
            panic!("lazy fixed-height updates must not invoke the full renderer")
        }

        fn render_update(&self, _width: u16) -> Option<FrameUpdate> {
            Some(FrameUpdate {
                stable_prefix: 0,
                replacement: self.lines.borrow().clone(),
                pinned: None,
                resize_replay: None,
                reanchor_viewport: false,
                rebuild_scrollback: false,
            })
        }

        fn invalidate(&mut self) {}
    }

    struct LazyObscuredLines {
        displayed: Rc<RefCell<Vec<String>>>,
        replay: Rc<RefCell<Vec<String>>>,
    }

    impl Component for LazyObscuredLines {
        fn render(&self, _width: u16) -> Vec<String> {
            panic!("lazy obscured updates must not invoke the full renderer")
        }

        fn render_update(&self, _width: u16) -> Option<FrameUpdate> {
            Some(FrameUpdate {
                stable_prefix: 0,
                replacement: self.displayed.borrow().clone(),
                pinned: None,
                resize_replay: Some(self.replay.borrow().clone()),
                reanchor_viewport: false,
                rebuild_scrollback: false,
            })
        }

        fn invalidate(&mut self) {}
    }

    struct LazyPinnedLines {
        lines: Rc<RefCell<Vec<String>>>,
        commit_boundary: Rc<Cell<usize>>,
        rebuild_scrollback: Rc<Cell<bool>>,
        generation: Rc<Cell<u64>>,
    }

    impl Component for LazyPinnedLines {
        fn render(&self, _width: u16) -> Vec<String> {
            self.lines.borrow().clone()
        }

        fn render_update(&self, width: u16) -> Option<FrameUpdate> {
            self.render_update_with_cursor(width, None)
        }

        fn render_update_with_cursor(
            &self,
            _width: u16,
            cursor: Option<CommitCursor>,
        ) -> Option<FrameUpdate> {
            let boundary = self.commit_boundary.get();
            let generation = self.generation.get();
            let acknowledged =
                cursor
                    .filter(|cursor| cursor.generation == generation)
                    .map(|cursor| CommitPosition {
                        row: cursor.block as usize,
                        cursor,
                    });
            let target = (boundary > 0).then_some(CommitPosition {
                cursor: CommitCursor {
                    generation,
                    block: boundary as u64,
                    segment: 0,
                },
                row: boundary,
            });
            Some(FrameUpdate {
                stable_prefix: 0,
                replacement: self.lines.borrow().clone(),
                pinned: Some(PinnedFrame {
                    generation,
                    acknowledged,
                    target,
                    stable_rows: boundary,
                    viewport_surface: false,
                }),
                resize_replay: None,
                reanchor_viewport: false,
                rebuild_scrollback: self.rebuild_scrollback.replace(false),
            })
        }

        fn invalidate(&mut self) {}
    }

    struct LazyReanchoredLines {
        lines: Rc<RefCell<Vec<String>>>,
        reanchor: Rc<Cell<bool>>,
        rebuild_scrollback: Rc<Cell<bool>>,
    }

    impl Component for LazyReanchoredLines {
        fn render(&self, _width: u16) -> Vec<String> {
            panic!("lazy viewport updates must not invoke the full renderer")
        }

        fn render_update(&self, _width: u16) -> Option<FrameUpdate> {
            Some(FrameUpdate {
                stable_prefix: 0,
                replacement: self.lines.borrow().clone(),
                pinned: None,
                resize_replay: None,
                reanchor_viewport: self.reanchor.replace(false),
                rebuild_scrollback: self.rebuild_scrollback.replace(false),
            })
        }

        fn invalidate(&mut self) {}
    }

    #[test]
    fn pinned_window_diffs_by_physical_row_after_window_shift() {
        let size = Rc::new(Cell::new((40, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = ["history 0", "history 1", "screen A", "screen B", "screen C"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        tui.inline_window_top = 2;
        tui.inline_bottom_row = 2;

        let shifted = [
            "new history 0",
            "new history 1",
            "new history 2",
            "screen A",
            "screen B",
            "screen C",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let previous_window = tui.previous_frame[2..].to_vec();
        tui.write_inline_pinned(
            &shifted,
            3,
            test_pinned_frame(None, None),
            false,
            false,
            &previous_window,
        );

        assert!(
            writes.borrow().is_empty(),
            "unchanged physical cells were repainted: {:?}",
            writes.borrow()
        );
    }

    #[test]
    fn pinned_stable_rows_scroll_naturally_in_one_multi_row_append() {
        let size = Rc::new(Cell::new((40, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = [
            "history 0",
            "history 1",
            "stable 2",
            "stable 3",
            "tail 4",
            "tail 5",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        tui.inline_history_rows = 2;
        tui.inline_window_top = 2;
        tui.inline_bottom_row = 3;

        let grown = tui
            .previous_frame
            .iter()
            .cloned()
            .chain(["tail 6".to_owned(), "tail 7".to_owned()])
            .collect::<Vec<_>>();
        let previous_window = tui.previous_frame[2..].to_vec();
        tui.write_inline_pinned(
            &grown,
            4,
            test_pinned_frame_with_stable(None, None, 4),
            false,
            false,
            &previous_window,
        );

        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[4;1H\r\n\r\n"), "{output:?}");
        assert!(!output.contains("\x1b[H"), "{output:?}");
        assert!(!output.contains("stable 2"), "{output:?}");
        assert!(!output.contains("stable 3"), "{output:?}");
        assert_eq!(output.matches("tail 6").count(), 1, "{output:?}");
        assert_eq!(output.matches("tail 7").count(), 1, "{output:?}");
        assert_eq!(tui.inline_history_rows, 4);
        assert_eq!(tui.inline_window_top, 4);
    }

    #[test]
    fn pinned_physical_stability_can_run_ahead_of_semantic_acknowledgement() {
        let size = Rc::new(Cell::new((40, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = ["row 0", "row 1", "row 2", "row 3"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        tui.inline_window_top = 0;
        tui.inline_bottom_row = 3;

        let grown = tui
            .previous_frame
            .iter()
            .cloned()
            .chain(
                ["tail 4", "tail 5", "tail 6"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .collect::<Vec<_>>();
        let previous_window = tui.previous_frame.clone();
        tui.write_inline_pinned(
            &grown,
            4,
            test_pinned_frame_with_stable(None, Some(2), 3),
            false,
            false,
            &previous_window,
        );

        let cursor = test_commit_position(2).cursor;
        assert_eq!(tui.inline_history_rows, 3);
        assert_eq!(tui.inline_committed_rows, 2);
        assert_eq!(tui.inline_commit_cursor, Some(cursor));

        // The next acknowledgement maps only row two. The separately retained
        // physical seam must prevent row two from being appended a second time.
        writes.borrow_mut().clear();
        tui.previous_frame = grown.clone();
        let previous_window = grown[3..].to_vec();
        let mut next = grown;
        next.push("tail 7".to_owned());
        tui.write_inline_pinned(
            &next,
            4,
            test_pinned_frame_with_stable(Some(cursor), None, 4),
            false,
            false,
            &previous_window,
        );

        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[4;1H\r\n"), "{output:?}");
        assert!(!output.contains("row 2"), "{output:?}");
        assert!(!output.contains("row 3"), "{output:?}");
        assert_eq!(tui.inline_history_rows, 4);
        assert_eq!(tui.inline_committed_rows, 2);
        assert_eq!(tui.inline_commit_cursor, Some(cursor));
    }

    #[test]
    fn pinned_reanchor_stages_an_atomic_target_and_one_complete_grid() {
        let size = Rc::new(Cell::new((40, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.inline_history_rows = 1;
        tui.inline_committed_rows = 1;
        tui.inline_commit_cursor = Some(test_commit_position(1).cursor);
        tui.inline_generation = Some(0);
        tui.inline_window_top = 4;
        tui.inline_bottom_row = 2;

        let replacement = (0..8).map(|row| format!("row {row}")).collect::<Vec<_>>();
        tui.write_inline_pinned(
            &replacement,
            3,
            test_pinned_frame_with_stable(tui.inline_commit_cursor, Some(3), 1),
            true,
            false,
            &[],
        );

        let output = writes.borrow().join("");
        assert_eq!(clears.get(), 0, "reanchor must preserve saved lines");
        assert!(output.starts_with("\x1b[H"), "{output:?}");
        for row in [1, 2, 5, 6, 7] {
            assert_eq!(
                output.matches(&format!("row {row}")).count(),
                1,
                "row {row} was not staged exactly once: {output:?}"
            );
        }
        for row in [0, 3, 4] {
            assert!(!output.contains(&format!("row {row}")), "{output:?}");
        }
        assert_eq!(output.matches("\r\n").count(), 4, "{output:?}");
        assert_eq!(tui.inline_history_rows, 3);
        assert_eq!(tui.inline_committed_rows, 3);
        assert_eq!(
            tui.inline_commit_cursor,
            Some(test_commit_position(3).cursor)
        );
        assert_eq!(tui.inline_window_top, 5);
    }

    #[test]
    fn pinned_window_shrink_repaints_a_temporary_surface_before_the_history_seam() {
        let size = Rc::new(Cell::new((40, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = [
            "settled 0",
            "settled 1",
            "settled 2",
            "settled 3",
            "old visible 4",
            "old visible 5",
            "empty composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        tui.inline_history_rows = 4;
        tui.inline_committed_rows = 4;
        tui.inline_commit_cursor = Some(test_commit_position(4).cursor);
        tui.inline_window_top = 4;
        tui.inline_bottom_row = 3;

        // Finalizing streamed Markdown can reduce the logical row count after
        // rows above the old viewport have already entered native scrollback.
        // The new bottom-aligned top retreats to row three, which is already
        // terminal-owned. Paint the complete semantic tail as a temporary
        // cursor-addressed surface instead of committing it or punching a blank
        // row into the live grid.
        let shrunk = [
            "settled 0",
            "settled 1",
            "settled 2",
            "new visible 3",
            "new visible 4",
            "typed composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let previous_window = tui.previous_frame[4..].to_vec();
        let cursor = tui.inline_commit_cursor;
        tui.write_inline_pinned(
            &shrunk,
            4,
            test_pinned_frame(cursor, Some(7)),
            false,
            false,
            &previous_window,
        );

        let output = writes.borrow().join("");
        assert!(output.contains("new visible 3"), "{output:?}");
        assert!(output.contains("new visible 4"), "{output:?}");
        assert!(output.contains("typed composer"), "{output:?}");
        assert!(output.contains("footer"), "{output:?}");
        assert!(
            !output.contains('\n'),
            "surface repaint scrolled: {output:?}"
        );
        assert_eq!(tui.inline_history_rows, 4);
        assert_eq!(tui.inline_committed_rows, 4);
        assert_eq!(tui.inline_window_top, 4);
        assert!(tui.inline_surface_active);
        assert_eq!(tui.inline_bottom_row, 3);
    }

    #[test]
    fn generation_change_after_resize_does_not_inherit_the_replayed_row_seam() {
        let size = Rc::new(Cell::new((40, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = ["old 0", "old 1", "old 2"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        // A destructive replay knows its timeline even though it deliberately
        // clears the semantic commit cursor until the next handshake.
        tui.inline_generation = Some(7);
        tui.inline_history_rows = 3;
        tui.inline_commit_cursor = None;
        tui.inline_window_top = 0;
        tui.inline_bottom_row = 2;

        let replacement = ["new 0", "new 1", "new 2"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let previous_window = tui.previous_frame.clone();
        tui.write_inline_pinned(
            &replacement,
            3,
            PinnedFrame {
                generation: 8,
                acknowledged: None,
                target: None,
                stable_rows: 0,
                viewport_surface: false,
            },
            false,
            false,
            &previous_window,
        );

        let output = writes.borrow().join("");
        assert!(output.contains("new 0"), "{output:?}");
        assert!(output.contains("new 2"), "{output:?}");
        assert_eq!(tui.inline_history_rows, 0);
        assert_eq!(tui.inline_generation, Some(8));
    }

    #[test]
    fn pinned_window_clears_rows_left_by_a_shorter_frame() {
        let size = Rc::new(Cell::new((40, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = ["screen A", "screen B", "stale C"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        tui.inline_window_top = 0;
        tui.inline_bottom_row = 2;
        let shorter = ["screen A", "screen B"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let previous_window = tui.previous_frame.clone();

        tui.write_inline_pinned(
            &shorter,
            3,
            test_pinned_frame(None, None),
            false,
            false,
            &previous_window,
        );

        let output = writes.borrow().join("");
        assert!(
            output.contains("\x1b[3;1H"),
            "stale trailing row was not addressed for clearing: {output:?}"
        );
        assert!(!output.contains("stale C"), "{output:?}");
    }

    #[test]
    fn pinned_reanchor_erases_rows_without_clear_screen() {
        let size = Rc::new(Cell::new((40, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = ["old A", "old composer", "old footer"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        tui.inline_window_top = 0;
        tui.inline_bottom_row = 2;
        let replacement = ["new A", "new composer", "new footer"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let previous_window = tui.previous_frame.clone();

        tui.write_inline_pinned(
            &replacement,
            3,
            test_pinned_frame(None, None),
            true,
            false,
            &previous_window,
        );

        assert_eq!(
            clears.get(),
            0,
            "tmux records clear-screen contents in native scrollback"
        );
        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[H"), "{output:?}");
        assert!(output.contains("new composer"), "{output:?}");
        assert!(!output.contains("old composer"), "{output:?}");
    }

    #[test]
    fn pinned_presentation_reset_preserves_history_without_replay() {
        let size = Rc::new(Cell::new((40, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.first_render = false;
        tui.previous_frame = vec!["old presentation".to_owned()];
        tui.inline_commit_cursor = Some(test_commit_position(2).cursor);
        tui.inline_committed_rows = 2;
        tui.inline_window_top = 4;
        let replacement = [
            "settled 0",
            "settled 1",
            "mutable omitted 2",
            "mutable omitted 3",
            "mutable visible 4",
            "mutable visible 5",
            "composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        let cursor = tui.inline_commit_cursor;

        tui.write_inline_pinned(
            &replacement,
            4,
            test_pinned_frame(cursor, None),
            true,
            false,
            &[],
        );

        let output = writes.borrow().join("");
        assert!(!output.contains("\x1b[3J"), "{output:?}");
        assert!(!output.contains("settled 0"), "{output:?}");
        assert!(!output.contains("settled 1"), "{output:?}");
        assert!(!output.contains("mutable omitted 2"), "{output:?}");
        assert!(!output.contains("mutable omitted 3"), "{output:?}");
        assert!(output.contains("mutable visible 4"), "{output:?}");
        assert!(output.contains("composer"), "{output:?}");
        assert_eq!(tui.inline_committed_rows, 2);
        assert_eq!(tui.inline_window_top, 4);
        assert_eq!(tui.inline_bottom_row, 3);
    }

    #[test]
    fn lazy_update_does_not_render_or_emit_a_hundred_thousand_stable_rows() {
        const HISTORY: usize = 100_000;
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        let size = Rc::new(Cell::new((80, 24)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let tail = Rc::new(RefCell::new("new mutable tail".to_owned()));
        let full_renders = Rc::new(Cell::new(0));
        let replacement_rows = Rc::new(Cell::new(0));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyTail {
            stable_prefix: HISTORY,
            tail,
            full_renders: full_renders.clone(),
            replacement_rows: replacement_rows.clone(),
        }));
        tui.previous_frame = (0..HISTORY)
            .map(|index| format!("historic row {index}{RESET}"))
            .chain(std::iter::once(format!("old mutable tail{RESET}")))
            .collect();
        tui.previous_size = Some((80, 24));
        tui.first_render = false;
        tui.inline_bottom_row = 23;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        assert_eq!(full_renders.get(), 0, "the full component renderer ran");
        assert_eq!(replacement_rows.get(), 1);
        assert!(output.contains("new mutable tail"), "{output:?}");
        assert!(
            !output.contains("historic row"),
            "stable history was emitted"
        );
        assert_eq!(tui.previous_frame.len(), HISTORY + 1);
    }

    #[test]
    fn lazy_fixed_height_update_emits_only_exact_changed_rows() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        let size = Rc::new(Cell::new((40, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, tail_clears, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "history".to_owned(),
            "new event".to_owned(),
            String::new(),
            String::new(),
            "composer top".to_owned(),
            "composer input".to_owned(),
            "composer bottom".to_owned(),
            "footer telemetry".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyFixedLines { lines }));
        tui.previous_frame = [
            "history",
            "",
            "",
            "",
            "composer top",
            "composer input",
            "composer bottom",
            "footer telemetry",
        ]
        .into_iter()
        .map(|line| format!("{line}{RESET}"))
        .collect();
        tui.previous_size = Some((40, 8));
        tui.first_render = false;
        tui.inline_bottom_row = 7;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        assert!(output.contains("new event"), "{output:?}");
        assert!(!output.contains("composer"), "{output:?}");
        assert!(!output.contains("footer telemetry"), "{output:?}");
        assert_eq!(tail_clears.get(), 0, "the pinned tail must not be erased");
        assert!(
            output.len() < 80,
            "unexpected repaint payload: {} B",
            output.len()
        );
    }

    #[test]
    fn lazy_fixed_height_image_removal_deletes_kitty_placements_before_redraw() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        const KITTY_IMAGE: &str = "\x1b_Ga=T,f=100,i=1,s=1,v=1,c=1,r=1;AAAA\x1b\\";
        let size = Rc::new(Cell::new((40, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::TrueColor,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "unchanged top".to_owned(),
            "image replaced by text".to_owned(),
            "unchanged bottom".to_owned(),
            "footer".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyFixedLines { lines }));
        tui.previous_frame = ["unchanged top", KITTY_IMAGE, "unchanged bottom", "footer"]
            .into_iter()
            .map(|line| format!("{line}{RESET}"))
            .collect();
        tui.previous_size = Some((40, 4));
        tui.first_render = false;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        let delete = delete_all_kitty_images();
        let delete_at = output.find(&delete).expect("Kitty placements were deleted");
        let replacement_at = output
            .find("image replaced by text")
            .expect("replacement row was painted");
        assert!(delete_at < replacement_at, "{output:?}");
        assert!(
            output.contains("unchanged top") && output.contains("unchanged bottom"),
            "the complete viewport must be restored after a global image delete: {output:?}"
        );
    }

    #[test]
    fn inline_resize_deletes_kitty_placements_before_destructive_replay() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        const KITTY_IMAGE: &str = "\x1b_Ga=T,f=100,i=2,s=1,v=1,c=1,r=1;AAAA\x1b\\";
        let size = Rc::new(Cell::new((41, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::TrueColor,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "unchanged top".to_owned(),
            "resized plain row".to_owned(),
            "unchanged bottom".to_owned(),
            "footer".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines)));
        tui.previous_frame = ["unchanged top", KITTY_IMAGE, "unchanged bottom", "footer"]
            .into_iter()
            .map(|line| format!("{line}{RESET}"))
            .collect();
        tui.previous_size = Some((40, 4));
        tui.first_render = false;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        let delete_at = output
            .find(&delete_all_kitty_images())
            .expect("Kitty placements were deleted during resize");
        let replacement_at = output
            .find("resized plain row")
            .expect("resized replacement row was painted");
        assert!(delete_at < replacement_at, "{output:?}");
        assert_eq!(clears.get(), 1, "resize must clear the reflowed grid");
        assert!(
            output.contains("\x1b[H\x1b[3J"),
            "resize must discard terminal-owned saved lines: {output:?}"
        );
    }

    #[test]
    fn inline_length_change_deletes_kitty_placements_and_restores_the_viewport() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        const KITTY_IMAGE: &str = "\x1b_Ga=T,f=100,i=3,s=1,v=1,c=1,r=1;AAAA\x1b\\";
        let size = Rc::new(Cell::new((40, 5)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::TrueColor,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "unchanged top".to_owned(),
            "length-changed plain row".to_owned(),
            "inserted row".to_owned(),
            "unchanged bottom".to_owned(),
            "footer".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyFixedLines { lines }));
        tui.previous_frame = ["unchanged top", KITTY_IMAGE, "unchanged bottom", "footer"]
            .into_iter()
            .map(|line| format!("{line}{RESET}"))
            .collect();
        tui.previous_size = Some((40, 5));
        tui.first_render = false;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        let delete_at = output
            .find(&delete_all_kitty_images())
            .expect("Kitty placements were deleted during the length change");
        let replacement_at = output
            .find("length-changed plain row")
            .expect("length-change replacement row was painted");
        assert!(delete_at < replacement_at, "{output:?}");
        assert!(
            output.contains("unchanged top") && output.contains("unchanged bottom"),
            "the viewport must be restored after deleting all placements: {output:?}"
        );
    }

    #[test]
    fn inline_scrollback_first_render_preserves_screen_and_appends_scroll() {
        let size = Rc::new(Cell::new((20, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, tail_clears, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "one".to_owned(),
            "two".to_owned(),
            "three".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();

        // First render scrolls the shell's screen into scrollback (one
        // newline per row) before clearing and painting from home.
        let strip = |text: String| text.replace("\u{1b}[0m\u{1b}]8;;\u{1b}\\", "");
        let first = strip(writes.borrow().join(""));
        assert!(first.starts_with("\n\n\n\n"), "{first:?}");
        assert_eq!(clears.get(), 1);
        assert!(first.contains("one\ntwo\nthree"), "{first:?}");
        assert!(!first.ends_with('\n'));

        // A pure append repaints from the last on-screen line — never a
        // full-screen clear, so scrollback history is never rewritten.
        writes.borrow_mut().clear();
        lines.borrow_mut().push("four".to_owned());
        tui.request_render();
        let appended = strip(writes.borrow().join(""));
        assert_eq!(clears.get(), 1, "append must not clear the screen");
        assert_eq!(tail_clears.get(), 1);
        assert!(appended.contains("three\nfour"), "{appended:?}");
        assert!(!appended.contains("one"), "history must not be rewritten");

        // Growing past the screen height keeps repainting only the tail.
        writes.borrow_mut().clear();
        lines
            .borrow_mut()
            .extend(["five".to_owned(), "six".to_owned()]);
        tui.request_render();
        let grown = strip(writes.borrow().join(""));
        assert!(grown.contains("four\nfive\nsix"), "{grown:?}");
        assert!(!grown.contains("one"));
        assert_eq!(clears.get(), 1);
    }

    #[test]
    fn inline_first_paint_is_bounded_to_the_terminal_viewport() {
        const HISTORY: usize = 10_000;
        let size = Rc::new(Cell::new((120, 24)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::TrueColor,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(
            (0..HISTORY)
                .map(|index| format!("historic row {index}"))
                .collect::<Vec<_>>(),
        ));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines)));

        tui.start();

        let output = writes.borrow().join("");
        let painted = output
            .rsplit_once("\x1b[H")
            .map_or(output.as_str(), |(_, painted)| painted);
        assert_eq!(clears.get(), 1);
        assert_eq!(painted.matches('\n').count(), 23, "{painted:?}");
        assert!(painted.contains("historic row 9999"), "{painted:?}");
        assert!(!painted.contains("historic row 0\x1b"), "{painted:?}");
        assert!(
            output.len() < 4_096,
            "first paint unexpectedly emitted {} bytes",
            output.len()
        );
        assert_eq!(tui.previous_frame.len(), HISTORY);
    }

    #[test]
    fn window_title_is_osc2_with_controls_stripped_and_silent_when_plain() {
        let size = Rc::new(Cell::new((20, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_window_title("ygg · model\x07\x1b · thinking");
        assert_eq!(
            writes.borrow().join(""),
            "\x1b]2;ygg · model · thinking\x07"
        );

        let (terminal, _, _, _, _, writes) =
            recording_terminal(size, crate::capabilities::TerminalCapabilities::plain());
        let mut plain = TUI::new(Box::new(terminal));
        plain.set_window_title("ygg");
        assert!(writes.borrow().is_empty());
    }

    #[test]
    fn fixed_height_middle_update_does_not_repaint_pinned_tail() {
        let size = Rc::new(Cell::new((40, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            false,
        );
        let (terminal, _, tail_clears, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "history".to_owned(),
            String::new(),
            String::new(),
            String::new(),
            "composer top".to_owned(),
            "composer input".to_owned(),
            "composer bottom".to_owned(),
            "footer telemetry".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();

        writes.borrow_mut().clear();
        lines.borrow_mut()[1] = "new event".to_owned();
        tui.request_render();

        let output = writes.borrow().join("");
        assert!(output.contains("new event"), "{output:?}");
        assert!(!output.contains("composer"), "{output:?}");
        assert!(!output.contains("footer telemetry"), "{output:?}");
        assert_eq!(tail_clears.get(), 0, "the pinned tail must not be erased");
    }

    #[test]
    fn large_growth_above_native_viewport_never_replays_displaced_history() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        for synchronized_output in [false, true] {
            let size = Rc::new(Cell::new((40, 4)));
            let capabilities = crate::capabilities::TerminalCapabilities::interactive(
                crate::capabilities::ColorDepth::Ansi16,
                true,
            )
            .with_overrides(&crate::capabilities::CapabilityOverrides {
                synchronized_output: Some(synchronized_output),
                ..crate::capabilities::CapabilityOverrides::default()
            });
            let (terminal, clears, tail_clears, _, _, writes) =
                recording_terminal(size, capabilities);
            let mut next = (0..372)
                .map(|index| format!("inserted row {index}"))
                .collect::<Vec<_>>();
            next.extend(
                [
                    "history 0",
                    "history 1",
                    "history 2",
                    "history 3",
                    "visible a",
                    "visible b",
                    "visible C updated",
                    "footer",
                ]
                .into_iter()
                .map(str::to_owned),
            );
            let lines = Rc::new(RefCell::new(next));
            let mut tui = TUI::new(Box::new(terminal));
            tui.set_inline_scrollback(true);
            tui.add_child(Box::new(MutableLines(lines)));
            tui.previous_frame = [
                "history 0",
                "history 1",
                "history 2",
                "history 3",
                "visible a",
                "visible b",
                "visible c",
                "footer",
            ]
            .into_iter()
            .map(|line| format!("{line}{RESET}"))
            .collect();
            tui.previous_size = Some((40, 4));
            tui.first_render = false;
            tui.inline_bottom_row = 3;
            tui.running = true;

            tui.request_render();

            let output = writes.borrow().join("");
            assert_eq!(
                output.contains("\x1b[?2026h"),
                synchronized_output,
                "{output:?}"
            );
            assert!(!output.contains("\x1b[3J"), "{output:?}");
            assert!(!output.contains('\n'), "{output:?}");
            assert_eq!(clears.get(), 0);
            assert_eq!(tail_clears.get(), 0);
            assert!(output.contains("\x1b[3;1H"), "{output:?}");
            assert!(output.contains("visible C updated"), "{output:?}");
            assert!(output.len() < 256, "unbounded repaint: {} B", output.len());
            for replayed in [
                "inserted row",
                "history 0",
                "history 3",
                "visible a",
                "visible b",
                "footer",
            ] {
                assert!(!output.contains(replayed), "{replayed:?}: {output:?}");
            }
        }
    }

    #[test]
    fn offscreen_mutation_preserves_short_frame_screen_row_anchor() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        let size = Rc::new(Cell::new((40, 5)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let mut next = (0..64)
            .map(|index| format!("new offscreen row {index}"))
            .collect::<Vec<_>>();
        next.extend(
            [
                "history 0",
                "history 1",
                "history 2",
                "history 3",
                "history 4",
                "visible a",
                "visible B updated",
                "footer",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(Rc::new(RefCell::new(next)))));
        tui.previous_frame = [
            "history 0",
            "history 1",
            "history 2",
            "history 3",
            "history 4",
            "visible a",
            "visible b",
            "footer",
        ]
        .into_iter()
        .map(|line| format!("{line}{RESET}"))
        .collect();
        tui.previous_size = Some((40, 5));
        tui.first_render = false;
        // A prior shrink left the three-row frame tail at rows 0..=2 rather
        // than bottom-aligning it to the five-row terminal.
        tui.inline_bottom_row = 2;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[2;1H"), "{output:?}");
        assert!(!output.contains("\x1b[4;1H"), "{output:?}");
        assert!(output.contains("visible B updated"), "{output:?}");
        assert!(!output.contains('\n'), "{output:?}");
        assert!(!output.contains("new offscreen row"), "{output:?}");
    }

    #[test]
    fn inline_scrollback_shrink_keeps_row_anchoring_for_later_repaints() {
        // Regression: growing suggestion lists then shrinking them (e.g.
        // slash-command completion after a resumed session) must not leave
        // stale rows behind — after a shrink the frame's tail sits above the
        // bottom row and later repaints must anchor to it, not to a
        // bottom-aligned viewport.
        let size = Rc::new(Cell::new((20, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
            "e".to_owned(),
            "f".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();
        // Screen shows c d e f on rows 0..4.

        // Shrink: drop e and f. Repaint starts at the row that held e.
        writes.borrow_mut().clear();
        lines.borrow_mut().truncate(4);
        tui.request_render();
        assert!(writes.borrow().join("").contains("\x1b[3;1H"));

        // Append after the shrink: the new line must paint directly below
        // "d" (row 2), not at the bottom of the screen.
        writes.borrow_mut().clear();
        lines.borrow_mut().push("g".to_owned());
        tui.request_render();
        let strip = |text: String| text.replace("\u{1b}[0m\u{1b}]8;;\u{1b}\\", "");
        let appended = strip(writes.borrow().join(""));
        assert!(appended.contains("\x1b[2;1H"), "{appended:?}");
        assert!(appended.contains("d\ng"), "{appended:?}");
    }

    #[test]
    fn logical_timeline_reset_reanchors_later_panel_updates_to_the_bottom() {
        // Regression: after replacing a long resumed conversation with `/new`,
        // the shorter frame was painted relative to the old scrollback origin.
        // The composer landed near the top and opening `/model` kept expanding
        // relative to that incorrect physical bottom.
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        let size = Rc::new(Cell::new((30, 6)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            String::new(),
            String::new(),
            String::new(),
            "composer top".to_owned(),
            format!("prompt {CURSOR_MARKER}"),
            "footer".to_owned(),
        ]));
        let reanchor = Rc::new(Cell::new(true));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyReanchoredLines {
            lines: lines.clone(),
            reanchor,
            rebuild_scrollback: Rc::new(Cell::new(false)),
        }));
        tui.previous_frame = (0..10)
            .map(|index| format!("historic row {index}{RESET}"))
            .collect();
        tui.previous_size = Some((30, 6));
        tui.first_render = false;
        tui.inline_bottom_row = 5;
        tui.running = true;

        tui.request_render();
        assert_eq!(
            clears.get(),
            0,
            "timeline replacement must repaint by row without mutating history"
        );
        assert!(writes.borrow().join("").contains("\x1b[H"));
        assert_eq!(tui.inline_bottom_row, 5);
        assert!(writes.borrow().join("").contains("\x1b[5;8H"));

        writes.borrow_mut().clear();
        *lines.borrow_mut() = vec![
            String::new(),
            "Models".to_owned(),
            "  model-a".to_owned(),
            "composer top".to_owned(),
            format!("prompt {CURSOR_MARKER}"),
            "footer".to_owned(),
        ];
        tui.request_render();

        let panel_frame = writes.borrow().join("");
        assert!(panel_frame.contains("Models"), "{panel_frame:?}");
        assert!(panel_frame.contains("\x1b[5;8H"), "{panel_frame:?}");
        assert_eq!(tui.inline_bottom_row, 5);
    }

    #[test]
    fn presentation_reset_clears_and_rebuilds_native_scrollback_once() {
        const RESET: &str = "\x1b[0m\x1b]8;;\x1b\\";
        const KITTY_IMAGE: &str = "\x1b_Ga=T,f=100,i=4,s=1,v=1,c=1,r=1;AAAA\x1b\\";
        let size = Rc::new(Cell::new((30, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(
            (0..6)
                .map(|index| format!("new-theme row {index}"))
                .collect::<Vec<_>>(),
        ));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyReanchoredLines {
            lines,
            reanchor: Rc::new(Cell::new(true)),
            rebuild_scrollback: Rc::new(Cell::new(true)),
        }));
        tui.previous_frame = (0..6)
            .map(|index| {
                if index == 2 {
                    format!("{KITTY_IMAGE}{RESET}")
                } else {
                    format!("old-theme row {index}{RESET}")
                }
            })
            .collect();
        tui.previous_size = Some((30, 4));
        tui.first_render = false;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        let delete_images = output
            .find(&delete_all_kitty_images())
            .expect("presentation reset did not delete Kitty placements");
        let clear_saved = output
            .find("\x1b[3J")
            .expect("presentation reset did not erase saved lines");
        assert!(delete_images < clear_saved, "{output:?}");
        let rebuilt = &output[clear_saved + "\x1b[3J".len()..];
        assert_eq!(clears.get(), 1);
        assert!(!rebuilt.contains("old-theme"), "{rebuilt:?}");
        for index in 0..6 {
            assert_eq!(
                rebuilt.matches(&format!("new-theme row {index}")).count(),
                1,
                "row {index} was not rebuilt exactly once: {rebuilt:?}"
            );
        }
        assert_eq!(tui.inline_bottom_row, 3);
    }

    #[test]
    fn inline_scrollback_resize_destructively_replays_the_complete_frame() {
        let size = Rc::new(Cell::new((20, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let lines = Rc::new(RefCell::new(
            (0..6)
                .map(|index| format!("row {index}"))
                .collect::<Vec<_>>(),
        ));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();

        writes.borrow_mut().clear();
        let clears_before = clears.get();
        size.set((30, 4));
        tui.request_render();
        let repaint = writes
            .borrow()
            .join("")
            .replace("\u{1b}[0m\u{1b}]8;;\u{1b}\\", "");
        assert_eq!(clears.get(), clears_before + 1);
        assert!(
            repaint.contains("\x1b[H\x1b[3J"),
            "resize did not clear saved lines: {repaint:?}"
        );
        for index in 0..6 {
            assert_eq!(
                repaint.matches(&format!("row {index}")).count(),
                1,
                "row {index} was not replayed exactly once: {repaint:?}"
            );
        }
        assert_eq!(tui.inline_window_top, 2);
    }

    #[test]
    fn resize_replays_an_unobscured_frame_before_repainting_its_screen_surface() {
        let size = Rc::new(Cell::new((20, 3)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let displayed = Rc::new(RefCell::new(
            ["owned 0", "owned 1", "overlay 0", "overlay 1", "overlay 2"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ));
        let replay = Rc::new(RefCell::new(
            ["owned 0", "owned 1", "owned 2", "owned 3", "owned 4"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        ));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyObscuredLines { displayed, replay }));
        tui.start();

        writes.borrow_mut().clear();
        size.set((30, 3));
        tui.request_render();
        let output = writes.borrow().join("");
        let clear_saved = output.find("\x1b[3J").expect("saved-line clear");
        let owned_tail = output.find("owned 4").expect("unobscured replay tail");
        let overlay = output.rfind("overlay 0").expect("screen-surface repaint");
        assert!(
            clear_saved < owned_tail && owned_tail < overlay,
            "{output:?}"
        );
        for index in 0..5 {
            assert_eq!(
                output.matches(&format!("owned {index}")).count(),
                1,
                "owned row {index} was not replayed exactly once: {output:?}"
            );
        }
        for index in 0..3 {
            assert_eq!(
                output.matches(&format!("overlay {index}")).count(),
                1,
                "overlay row {index} was not repainted exactly once: {output:?}"
            );
        }
        assert_eq!(tui.inline_window_top, 2);
    }

    #[test]
    fn pinned_presentation_rebuild_preserves_committed_native_history() {
        let size = Rc::new(Cell::new((30, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![
            "committed history 0".to_owned(),
            "committed history 1".to_owned(),
            "expanded summary 0".to_owned(),
            "expanded summary 1".to_owned(),
            "expanded summary 2".to_owned(),
            "later event 3".to_owned(),
            "later event 4".to_owned(),
            "later event 5".to_owned(),
            format!("composer {CURSOR_MARKER}"),
            "footer".to_owned(),
        ]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyPinnedLines {
            lines,
            commit_boundary: Rc::new(Cell::new(5)),
            rebuild_scrollback: Rc::new(Cell::new(true)),
            generation: Rc::new(Cell::new(0)),
        }));
        tui.previous_frame = [
            "committed history 0",
            "committed history 1",
            "collapsed summary",
            "later event 3",
            "later event 4",
            "later event 5",
            "composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        tui.previous_size = Some((30, 4));
        tui.first_render = false;
        tui.inline_history_rows = 2;
        tui.inline_committed_rows = 2;
        tui.inline_commit_cursor = Some(test_commit_position(2).cursor);
        tui.inline_generation = Some(0);
        tui.inline_window_top = 4;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let expanded = writes.borrow().join("");
        assert!(!expanded.contains("\x1b[2J"), "{expanded:?}");
        assert!(!expanded.contains("\x1b[3J"), "{expanded:?}");
        assert!(!expanded.contains("committed history"), "{expanded:?}");
        for row in 0..=2 {
            assert!(
                expanded.contains(&format!("expanded summary {row}")),
                "{expanded:?}"
            );
        }
        assert_eq!(clears.get(), 0);
        assert_eq!(tui.inline_history_rows, 5);
        assert_eq!(tui.inline_window_top, 6);
    }

    #[test]
    fn text_only_pinned_resize_preserves_native_history_and_repaints_one_grid() {
        let size = Rc::new(Cell::new((30, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        )
        .with_overrides(&crate::capabilities::CapabilityOverrides {
            synchronized_output: Some(true),
            ..crate::capabilities::CapabilityOverrides::default()
        });
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let mut initial_lines = [
            "history 0",
            "history 1",
            "history 2",
            "history 3",
            "visible 4",
            "visible 5",
            "empty composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        initial_lines[6].push_str(CURSOR_MARKER);
        let lines = Rc::new(RefCell::new(initial_lines));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyPinnedLines {
            lines,
            commit_boundary: Rc::new(Cell::new(2)),
            rebuild_scrollback: Rc::new(Cell::new(false)),
            generation: Rc::new(Cell::new(0)),
        }));
        tui.start();
        let clears_after_start = clears.get();

        writes.borrow_mut().clear();
        size.set((50, 6));
        tui.request_render();
        let resized = writes.borrow().join("");

        assert_eq!(clears.get(), clears_after_start);
        assert!(!resized.contains("\x1b[3J"), "{resized:?}");
        assert!(!resized.contains("history 0"), "{resized:?}");
        assert!(!resized.contains("history 1"), "{resized:?}");
        assert!(resized.contains("history 2"), "{resized:?}");
        assert!(resized.contains("footer"), "{resized:?}");
        assert_eq!(tui.inline_history_rows, 2);
        assert_eq!(tui.inline_window_top, 2);
        assert_eq!(tui.inline_bottom_row, 5);
    }

    #[test]
    fn resize_and_generation_change_start_a_new_tape_without_clearing_old_history() {
        let size = Rc::new(Cell::new((30, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let lines = Rc::new(RefCell::new(
            [
                "old history 0",
                "old history 1",
                "old visible 2",
                "old visible 3",
                "old composer",
                "old footer",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        ));
        let boundary = Rc::new(Cell::new(2));
        let generation = Rc::new(Cell::new(0));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyPinnedLines {
            lines: Rc::clone(&lines),
            commit_boundary: Rc::clone(&boundary),
            rebuild_scrollback: Rc::new(Cell::new(false)),
            generation: Rc::clone(&generation),
        }));
        tui.start();
        let clears_after_start = clears.get();

        *lines.borrow_mut() = [
            "new history 0",
            "new history 1",
            "new visible 2",
            "new visible 3",
            "new composer",
            "new footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        boundary.set(0);
        generation.set(1);
        writes.borrow_mut().clear();
        size.set((50, 6));
        tui.request_render();

        let resized = writes.borrow().join("");
        assert_eq!(clears.get(), clears_after_start);
        assert!(!resized.contains("\x1b[3J"), "{resized:?}");
        assert!(!resized.contains("old history"), "{resized:?}");
        assert!(resized.contains("new history 0"), "{resized:?}");
        assert!(resized.contains("new footer"), "{resized:?}");
        assert_eq!(tui.inline_history_rows, 0);
        assert_eq!(tui.inline_generation, Some(1));
    }

    #[test]
    fn pinned_resize_resets_scrollback_and_updates_the_next_diff_origin() {
        const KITTY_IMAGE: &str = "\x1b_Ga=T,f=100,i=9,s=1,v=1,c=1,r=1;AAAA\x1b\\";
        let size = Rc::new(Cell::new((30, 4)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        )
        .with_overrides(&crate::capabilities::CapabilityOverrides {
            synchronized_output: Some(true),
            ..crate::capabilities::CapabilityOverrides::default()
        });
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let mut initial_lines = [
            "history 0",
            "history 1",
            "history 2",
            "history 3",
            "visible 4",
            "visible 5",
            "empty composer",
            "footer",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
        initial_lines[1].push_str(KITTY_IMAGE);
        initial_lines[6].push_str(CURSOR_MARKER);
        let lines = Rc::new(RefCell::new(initial_lines));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyPinnedLines {
            lines: lines.clone(),
            commit_boundary: Rc::new(Cell::new(2)),
            rebuild_scrollback: Rc::new(Cell::new(false)),
            generation: Rc::new(Cell::new(0)),
        }));
        tui.start();
        assert_eq!(tui.inline_window_top, 4);
        let clears_after_start = clears.get();

        // Growing and widening the terminal invalidates both grid and saved
        // row coordinates. The reset must replay every owned row, then retain
        // the new window origin for the next composer-only update.
        writes.borrow_mut().clear();
        size.set((50, 6));
        tui.request_render();
        let resized = writes.borrow().join("");
        assert_eq!(tui.inline_window_top, 2);
        assert_eq!(tui.inline_bottom_row, 5);
        assert_eq!(clears.get(), clears_after_start + 1);
        assert!(
            resized.contains("\x1b[H\x1b[3J"),
            "pinned resize did not erase saved lines: {resized:?}"
        );
        for index in 0..=3 {
            assert!(resized.contains(&format!("history {index}")), "{resized:?}");
        }
        assert!(resized.contains("visible 4"), "{resized:?}");
        assert!(resized.contains("visible 5"), "{resized:?}");
        let delete_at = resized
            .find(&delete_all_kitty_images())
            .expect("pinned resize did not delete Kitty placements");
        let image_at = resized
            .find(KITTY_IMAGE)
            .expect("pinned resize did not retransmit the image");
        let begin_at = resized
            .find("\x1b[?2026h")
            .expect("resize replay did not begin synchronized output");
        let clear_saved_at = resized.find("\x1b[3J").expect("saved-line clear");
        let end_at = resized
            .rfind("\x1b[?2026l")
            .expect("resize replay did not end synchronized output");
        assert!(
            begin_at < delete_at
                && delete_at < clear_saved_at
                && clear_saved_at < image_at
                && image_at < end_at,
            "resize replay was not one ordered synchronized transaction: {resized:?}"
        );
        assert!(
            resized.contains("\x1b[5;15H"),
            "composer cursor must follow the resized pinned window: {resized:?}"
        );

        writes.borrow_mut().clear();
        lines.borrow_mut()[6] = format!("typed composer{CURSOR_MARKER}");
        tui.request_render();
        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[5;1H"), "{output:?}");
        assert!(output.contains("typed composer"), "{output:?}");
    }

    #[test]
    fn pi_resize_clears_saved_lines_and_replays_the_complete_frame() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(OneLine));
        tui.start();
        let redraws = tui.full_redraws();
        writes.borrow_mut().clear();
        size.set((80, 24));
        tui.request_render();

        assert!(tui.full_redraws() > redraws);
        let output = writes.borrow().join("");
        assert!(output.contains("\x1b[2J\x1b[H\x1b[3J"), "{output:?}");
        assert!(output.contains("line"), "{output:?}");
    }

    #[test]
    fn cursor_addressed_frames_never_end_with_a_scrolling_newline() {
        let size = Rc::new(Cell::new((20, 2)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec!["first".to_owned(), "second".to_owned()]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();
        assert!(!writes.borrow().join("").ends_with('\n'));

        writes.borrow_mut().clear();
        lines.borrow_mut()[1] = "changed".into();
        tui.request_render();
        assert!(!writes.borrow().join("").ends_with('\n'));
    }

    #[test]
    fn pi_pure_append_and_shrink_update_only_the_changed_tail() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec!["first".to_owned()]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();
        writes.borrow_mut().clear();

        lines.borrow_mut().push("second".to_owned());
        tui.request_render();
        let append = writes.borrow().join("");
        assert!(append.contains("\r\n"), "{append:?}");
        assert!(append.contains("second"), "{append:?}");
        assert!(!append.contains("\x1b[3J"), "{append:?}");

        writes.borrow_mut().clear();
        lines.borrow_mut().truncate(1);
        tui.request_render();
        let shrink = writes.borrow().join("");
        assert!(shrink.contains("\x1b[2K"), "{shrink:?}");
        assert!(!shrink.contains("\x1b[3J"), "{shrink:?}");
    }

    #[test]
    fn plain_backend_is_escape_free_ascii_structured_and_not_right_padded() {
        let size = Rc::new(Cell::new((20, 8)));
        let (terminal, _, _, _, _, writes) =
            recording_terminal(size, crate::TerminalCapabilities::plain());
        let lines = Rc::new(RefCell::new(vec!["safe\x1b]52;c;bad\x07".into()]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(MutableLines(lines)));
        tui.start();
        let output = writes.borrow().join("");
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\x07'));
        assert!(output.starts_with("safe^["));
        assert!(!output.contains("                    "));
    }

    #[test]
    fn pi_cursor_marker_uses_unclipped_display_cells() {
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let cases = [
            (2, format!("界{CURSOR_MARKER}x"), "\x1b[3G"),
            (3, format!("abcdef{CURSOR_MARKER}"), "\x1b[7G"),
        ];

        for (width, line, expected_cursor) in cases {
            let size = Rc::new(Cell::new((width, 2)));
            let (terminal, _, _, _, shows, writes) = recording_terminal(size, capabilities);
            let mut tui = TUI::new(Box::new(terminal));
            tui.set_show_hardware_cursor(true);
            tui.add_child(Box::new(MutableLines(Rc::new(RefCell::new(vec![line])))));
            tui.start();

            let output = writes.borrow().join("");
            assert!(!output.contains(CURSOR_MARKER), "{output:?}");
            assert!(output.contains(expected_cursor), "{output:?}");
            assert_eq!(shows.get(), 1, "width {width} did not reveal the cursor");
        }
    }

    #[test]
    fn lazy_replacement_extracts_cursor_before_width_clipping() {
        let size = Rc::new(Cell::new((3, 2)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, _, shows, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![format!("abcdef{CURSOR_MARKER}")]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(LazyFixedLines { lines }));
        tui.start();

        let output = writes.borrow().join("");
        assert!(!output.contains(CURSOR_MARKER), "{output:?}");
        assert!(output.contains("\x1b[1;3H"), "{output:?}");
        assert_eq!(shows.get(), 1);
    }

    #[test]
    fn pi_synchronized_frame_precedes_ime_cursor_positioning() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, stops, shows, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec![format!("界{CURSOR_MARKER}x")]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(MutableLines(lines)));
        tui.start();
        let output = writes.borrow().join("");
        assert!(!output.contains(CURSOR_MARKER));
        let begin = output.find("\x1b[?2026h").expect("Pi frame begin");
        let end = output.find("\x1b[?2026l").expect("Pi frame end");
        let cursor = output.rfind("\x1b[3G").expect("IME cursor column");
        assert!(begin < end && end < cursor, "{output:?}");
        assert_eq!(shows.get(), 0, "Pi hides the hardware cursor by default");
        drop(output);

        tui.stop();
        assert_eq!(stops.get(), 1);
        assert_eq!(shows.get(), 1, "stop restores the user's cursor");
        assert!(writes.borrow().join("").contains("\r\n"));
    }

    #[test]
    fn inline_scrollback_stop_anchors_after_the_final_frame_row() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, _, stops, shows, writes) = recording_terminal(size, capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.set_inline_scrollback(true);
        tui.add_child(Box::new(MutableLines(Rc::new(RefCell::new(vec![
            "header".into(),
            format!("composer{CURSOR_MARKER}"),
            "footer".into(),
        ])))));
        tui.start();
        assert_eq!(tui.inline_bottom_row, 2);
        writes.borrow_mut().clear();

        tui.stop();

        assert_eq!(stops.get(), 1);
        assert_eq!(shows.get(), 2);
        assert_eq!(
            writes.borrow().join(""),
            "\x1b[3;1H\x1b[0m\x1b]8;;\x1b\\",
            "shutdown must leave the caller below the complete inline frame"
        );
    }
}
