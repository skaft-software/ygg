//! Retained component tree with line-differential terminal rendering.
use std::cell::RefCell;
use std::rc::Rc;

use crate::terminal::{key_to_string, Terminal, TerminalInput};
use crate::terminal_image::{delete_all_kitty_images, is_image_line};
use crate::utils::visible_width;

/// Zero-width APC escape sequence used as a cursor position marker.
pub const CURSOR_MARKER: &str = "\x1b_\\";

type OverlayEntry = (Rc<RefCell<OverlayHandle>>, Box<dyn Component>);
/// Global input listener. Returning `Some` consumes the input event.
pub type InputListener<'a> = Box<dyn FnMut(&str) -> Option<String> + 'a>;

// =============================================================================
// Component Trait
// =============================================================================

/// A lazy replacement for the mutable tail of a retained frame. Lines before
/// `stable_prefix` are guaranteed byte-identical to the previous frame, so the
/// TUI can reuse them without cloning or comparing a long committed history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameUpdate {
    pub stable_prefix: usize,
    pub replacement: Vec<String>,
    /// First logical row that may still change. Rows before this boundary may
    /// enter native scrollback; rows at or after it remain viewport-local.
    /// `None` keeps the generic shell-style differential renderer.
    pub commit_boundary: Option<usize>,
    /// The component replaced its logical timeline (for example, a resumed
    /// conversation was replaced by a new session). Repaint the visible tail
    /// from the top of the terminal so later fixed-height chrome remains
    /// anchored to the physical bottom row.
    pub reanchor_viewport: bool,
    /// The presentation of committed rows changed. Clear terminal-owned
    /// scrollback and replay the retained logical frame once so scrolling
    /// cannot reveal stale or duplicate renderings from the previous theme.
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

/// Components that can receive focus and display a hardware cursor for IME.
pub trait Focusable {
    fn set_focused(&mut self, focused: bool);
    fn is_focused(&self) -> bool;
}

// =============================================================================
// Container
// =============================================================================

/// Container that groups child components vertically.
pub struct Container {
    children: Vec<Box<dyn Component>>,
    focused_child: Option<usize>,
}

impl Container {
    pub fn new() -> Self {
        Container {
            children: Vec::new(),
            focused_child: None,
        }
    }

    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    pub fn remove_child(&mut self, child_idx: usize) {
        if child_idx < self.children.len() {
            self.children.remove(child_idx);
            if self.focused_child == Some(child_idx) {
                self.focused_child = None;
            }
        }
    }

    pub fn set_focus(&mut self, idx: Option<usize>) {
        self.focused_child = idx;
    }

    pub fn focused_child_mut(&mut self) -> Option<&mut Box<dyn Component>> {
        self.focused_child.and_then(|i| self.children.get_mut(i))
    }

    fn render_update(&self, width: u16) -> Option<FrameUpdate> {
        (self.children.len() == 1)
            .then(|| self.children[0].render_update(width))
            .flatten()
    }
}

impl Component for Container {
    fn render(&self, width: u16) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn handle_input(&mut self, data: &str) {
        if let Some(idx) = self.focused_child {
            if let Some(child) = self.children.get_mut(idx) {
                child.handle_input(data);
            }
        }
    }

    fn handle_paste(&mut self, data: &str) {
        if let Some(idx) = self.focused_child {
            if let Some(child) = self.children.get_mut(idx) {
                child.handle_paste(data);
            }
        }
    }

    fn wants_key_release(&self) -> bool {
        if let Some(idx) = self.focused_child {
            self.children
                .get(idx)
                .is_some_and(|c| c.wants_key_release())
        } else {
            false
        }
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Overlay Support
// =============================================================================

/// Anchor position for overlays.
#[derive(Debug, Clone, Copy)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopCenter,
    TopRight,
    LeftCenter,
    RightCenter,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// Margin values for overlays.
#[derive(Debug, Clone, Copy)]
pub struct OverlayMargin {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl OverlayMargin {
    pub fn all(value: u16) -> Self {
        OverlayMargin {
            top: value,
            right: value,
            bottom: value,
            left: value,
        }
    }
}

/// Options for focusing/unfocusing an overlay.
pub struct OverlayUnfocusOptions {
    pub target: Option<Box<dyn Component>>,
}

/// Options for creating an overlay.
pub struct OverlayOptions {
    pub width: Option<u16>,
    pub min_width: Option<u16>,
    pub max_height: Option<u16>,
    pub anchor: OverlayAnchor,
    pub offset_x: i16,
    pub offset_y: i16,
    pub row: Option<u16>,
    pub col: Option<u16>,
    pub margin: Option<OverlayMargin>,
    pub non_capturing: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        OverlayOptions {
            width: None,
            min_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            offset_x: 0,
            offset_y: 0,
            row: None,
            col: None,
            margin: None,
            non_capturing: false,
        }
    }
}

/// Handle to an active overlay.
#[derive(Clone)]
pub struct OverlayHandle {
    pub id: usize,
    hidden: bool,
    focused: bool,
}

impl OverlayHandle {
    pub fn hide(&mut self) {
        self.hidden = true;
    }

    pub fn show(&mut self) {
        self.hidden = false;
    }

    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    pub fn focus(&mut self) {
        self.focused = true;
    }

    pub fn unfocus(&mut self, _options: Option<OverlayUnfocusOptions>) {
        self.focused = false;
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }
}

// =============================================================================
// TUI — Main Interface
// =============================================================================

/// Main TUI instance managing the render loop.
pub struct TUI<'a> {
    terminal: Box<dyn Terminal + 'a>,
    root: Container,
    overlays: Vec<OverlayEntry>,
    next_overlay_id: usize,
    previous_frame: Vec<String>,
    /// Terminal dimensions used for `previous_frame`. A resize invalidates all
    /// cursor-relative differential-rendering assumptions.
    previous_size: Option<(u16, u16)>,
    first_render: bool,
    running: bool,
    capabilities: crate::capabilities::TerminalCapabilities,
    input_listeners: Vec<InputListener<'a>>,
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
    /// Rows physically appended to native scrollback by the pinned live-region
    /// renderer. They are immutable and are never painted into the grid again.
    inline_committed_rows: usize,
    /// First logical row represented by grid row zero in pinned mode.
    inline_window_top: usize,
}

impl<'a> TUI<'a> {
    pub fn new(terminal: Box<dyn Terminal + 'a>) -> Self {
        let capabilities = terminal.capabilities();
        TUI {
            terminal,
            root: Container::new(),
            overlays: Vec::new(),
            next_overlay_id: 0,
            previous_frame: Vec::new(),
            previous_size: None,
            first_render: true,
            running: false,
            capabilities,
            input_listeners: Vec::new(),
            inline_scrollback: false,
            inline_bottom_row: 0,
            inline_committed_rows: 0,
            inline_window_top: 0,
        }
    }

    /// Opt into inline scrollback rendering (see the field's invariants).
    pub fn set_inline_scrollback(&mut self, enabled: bool) {
        self.inline_scrollback = enabled;
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

    /// Add a component to the root container.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.root.add_child(child);
    }

    /// Remove a component from the root container.
    pub fn remove_child(&mut self, idx: usize) {
        self.root.remove_child(idx);
    }

    /// Set focus to a specific child.
    pub fn set_focus(&mut self, idx: Option<usize>) {
        self.root.set_focus(idx);
    }

    /// Show an overlay on top of the current content.
    pub fn show_overlay(
        &mut self,
        component: Box<dyn Component>,
        options: OverlayOptions,
    ) -> Rc<RefCell<OverlayHandle>> {
        let id = self.next_overlay_id;
        self.next_overlay_id += 1;
        let handle = Rc::new(RefCell::new(OverlayHandle {
            id,
            hidden: false,
            focused: !options.non_capturing,
        }));
        self.overlays.push((handle.clone(), component));
        handle
    }

    /// Hide the topmost overlay.
    pub fn hide_overlay(&mut self) {
        self.overlays.pop();
    }

    /// Check if any visible overlay is active.
    pub fn has_overlay(&self) -> bool {
        self.overlays.iter().any(|(h, _)| !h.borrow().hidden)
    }

    /// Add an input listener for global key handling.
    pub fn add_input_listener(&mut self, f: InputListener<'a>) {
        self.input_listeners.push(f);
    }

    /// Request a re-render at the next opportunity.
    pub fn request_render(&mut self) {
        // Trigger immediate re-render
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
        // Close any interrupted synchronized frame and all text/hyperlink
        // styling before restoring the backend. Repeated backend cleanup is
        // expected to be idempotent.
        if self.capabilities.synchronized_output {
            self.terminal.write("\x1b[?2026l");
        }
        if !self.capabilities.plain {
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

        // Route to focused overlay or root
        let has_capturing_overlay = self
            .overlays
            .iter()
            .any(|(h, _)| h.borrow().focused && !h.borrow().hidden);

        if has_capturing_overlay {
            if let Some((_, component)) = self.overlays.last_mut() {
                component.handle_input(data);
            }
        } else {
            self.root.handle_input(data);
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
                let has_capturing_overlay = self
                    .overlays
                    .iter()
                    .any(|(h, _)| h.borrow().focused && !h.borrow().hidden);
                if has_capturing_overlay {
                    if let Some((_, component)) = self.overlays.last_mut() {
                        component.handle_paste(&text);
                    }
                } else {
                    self.root.handle_paste(&text);
                }
                self.request_render();
            }
        }
    }

    /// Render the current frame using the differential rendering algorithm.
    fn render_frame(&mut self) {
        let width = self.terminal.columns();
        let height = self.terminal.rows();
        let size_changed = self
            .previous_size
            .is_some_and(|size| size != (width, height));

        let lazy_update = (!size_changed
            && self
                .overlays
                .iter()
                .all(|(handle, _)| handle.borrow().hidden)
            && (self.capabilities.plain
                || (self.inline_scrollback
                    && self.capabilities.cursor_addressing
                    && self.capabilities.line_clearing)))
            .then(|| self.root.render_update(width))
            .flatten();
        let previous_len = self.previous_frame.len();
        // A prefix beyond the retained frame cannot be validated. Fall back to
        // the component's full renderer rather than pairing its replacement
        // with the wrong historic rows.
        let lazy_update = lazy_update.filter(|update| update.stable_prefix <= previous_len);
        let reanchor_viewport = lazy_update
            .as_ref()
            .is_some_and(|update| update.reanchor_viewport);
        let rebuild_scrollback = lazy_update
            .as_ref()
            .is_some_and(|update| update.rebuild_scrollback);
        let commit_boundary = lazy_update
            .as_ref()
            .and_then(|update| update.commit_boundary);
        // Lazy frame assembly reuses `previous_frame` with `mem::take` below.
        // Preserve only the old physical viewport needed by pinned diffing;
        // cloning the complete retained transcript would defeat lazy updates.
        let pinned_previous_window = commit_boundary.map_or_else(Vec::new, |_| {
            self.previous_frame
                .iter()
                .skip(self.inline_window_top)
                .take(usize::from(height.max(1)))
                .cloned()
                .collect()
        });
        let mut first_changed_hint = None;
        let mut lazy_change_hints = None;
        let cursor;

        let new_lines: Vec<String> = if let Some(update) = lazy_update {
            let stable_prefix = update.stable_prefix.min(previous_len);
            let mut replacement = update
                .replacement
                .into_iter()
                .map(|line| self.prepare_line(line, width))
                .collect::<Vec<_>>();
            let total_len = stable_prefix.saturating_add(replacement.len());
            cursor =
                extract_cursor_position_from(&mut replacement, stable_prefix, total_len, height);
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
            let mut rendered = Vec::new();
            // Render root container. Plain/log mode is escape-free and does not
            // right-pad every row with terminal-width spaces. Inline scrollback
            // also skips padding: every repaint erases before writing, and padded
            // rows would put trailing spaces into native text selection.
            for line in self.root.render(width) {
                rendered.push(self.prepare_line(line, width));
            }

            // Composite any visible overlays on top of root content.
            for (handle, component) in &self.overlays {
                if handle.borrow().hidden {
                    continue;
                }
                let overlay_lines = component
                    .render(width)
                    .into_iter()
                    .map(|line| {
                        if self.capabilities.plain {
                            ensure_plain_line(&line, width)
                        } else {
                            ensure_line_width(&line, width)
                        }
                    })
                    .collect::<Vec<_>>();
                // Composite: overlay lines replace root lines at matching indices.
                // Lines beyond the root frame extend it, giving a "floating above" effect.
                let n_overlay = overlay_lines.len();
                let n_root = rendered.len();
                for i in 0..n_overlay.max(n_root) {
                    if i < n_overlay && i < n_root {
                        rendered[i] = overlay_lines[i].clone();
                    } else if i < n_overlay {
                        rendered.push(overlay_lines[i].clone());
                    }
                    // else: root line exists beyond overlay — keep as-is
                }
            }

            // Extract the typed cursor marker before frame comparison. It is a
            // trusted library control token, never accepted from semantic text.
            cursor = extract_cursor_position(&mut rendered, height);

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

        // A terminal reflows the old frame before delivering its resize event.
        // Cursor-relative differential updates are therefore invalid even when
        // the logical line count did not change; clear and redraw from home.
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
                commit_boundary,
                &pinned_previous_window,
                first_changed_hint,
                previous_len,
                lazy_change_hints.as_ref(),
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
        commit_boundary: Option<usize>,
        pinned_previous_window: &[String],
        first_changed_hint: Option<usize>,
        previous_len: usize,
        frame_change_hints: Option<&FrameChangeHints>,
    ) {
        let rows = usize::from(height.max(1));
        if let Some(boundary) = commit_boundary {
            self.write_inline_pinned(
                new_lines,
                rows,
                boundary.min(new_lines.len()),
                size_changed || reanchor_viewport || rebuild_scrollback,
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

        if rebuild_scrollback {
            // Native scrollback owns committed rows, so it cannot be restyled
            // in place. Theme switches deliberately replace that presentation:
            // erase saved lines, then replay the retained frame exactly once.
            // The renderer thread owns this bounded-by-retained-state write;
            // ordinary streaming/status updates continue to use lazy tails.
            self.rebuild_inline_scrollback(new_lines, rows);
            return;
        }

        let prev_len = previous_len;
        // Frame lines currently on screen span [visible_start, prev_len).
        let visible_start = prev_len.saturating_sub(self.inline_bottom_row + 1);
        if reanchor_viewport || size_changed || prev_len == 0 || new_lines.len() <= visible_start {
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

    /// Append-only native-scrollback renderer for a frame with an explicit
    /// live-region seam. Only finalized rows before `commit_boundary` may
    /// scroll off the grid. The mutable suffix is always repainted in place,
    /// so provisional Markdown layouts can never become duplicate history.
    fn write_inline_pinned(
        &mut self,
        new_lines: &[String],
        rows: usize,
        commit_boundary: usize,
        reanchor: bool,
        previous_window: &[String],
    ) {
        if reanchor
            && (new_lines.len() <= self.inline_committed_rows
                || commit_boundary < self.inline_committed_rows)
        {
            // A shorter replacement timeline cannot share row coordinates
            // with the old tape. Preserve old history, but restart the ledger
            // for the new frame instead of pinning its window beyond EOF.
            self.inline_committed_rows = 0;
            self.inline_window_top = 0;
        }
        let desired_window_top = new_lines.len().saturating_sub(rows);
        let commit_target = commit_boundary
            .min(desired_window_top)
            .max(self.inline_committed_rows);
        let window_top = desired_window_top.max(commit_target);
        let window = &new_lines[window_top.min(new_lines.len())..];
        let commit_advanced = commit_target > self.inline_committed_rows;

        self.begin_synchronized_output();
        if self.first_render {
            // Preserve whatever preceded the application, then establish a
            // clean grid without erasing terminal-owned history.
            self.terminal.write(&"\n".repeat(rows));
        }

        if self.first_render || reanchor || commit_advanced {
            // Rebuild only the grid. When the commit boundary advances, write
            // exactly the newly-final chunk before the next window; natural
            // scrolling moves that chunk, and only that chunk, into history.
            self.terminal.write("\x1b[H");
            if self.first_render {
                self.terminal.clear_screen();
                self.terminal.write("\x1b[H");
            }
            // Reanchors erase every addressed row below. Avoid ED 2 here:
            // tmux preserves cells erased by a clear-screen operation in its
            // native history, which would commit the mutable composer once per
            // theme or session transition.
            let committed = &new_lines[self.inline_committed_rows.min(new_lines.len())
                ..commit_target.min(new_lines.len())];
            let paint = committed.iter().chain(window.iter()).collect::<Vec<_>>();
            for (index, line) in paint.iter().enumerate() {
                self.terminal.clear_line();
                self.terminal.write(line);
                if index + 1 < paint.len() {
                    self.terminal.write("\r\n");
                }
            }
            for row in window.len()..rows {
                self.terminal
                    .write(&format!("\x1b[{};1H", row.saturating_add(1)));
                self.terminal.clear_line();
            }
        } else {
            // Ordinary token ticks never scroll. Compare the old and new
            // logical rows occupying each physical screen row. The caller
            // snapshots this small old window before lazy frame reuse.
            let previous_window_len = previous_window.len().min(rows);
            for screen_row in 0..window.len().max(previous_window_len) {
                let previous = previous_window.get(screen_row);
                let next = window.get(screen_row);
                if previous == next {
                    continue;
                }
                self.terminal
                    .write(&format!("\x1b[{};1H", screen_row.saturating_add(1)));
                self.terminal.clear_line();
                if let Some(line) = next {
                    self.terminal.write(line);
                }
            }
        }
        self.end_synchronized_output();

        self.inline_committed_rows = commit_target;
        self.inline_window_top = window_top;
        self.inline_bottom_row = window.len().saturating_sub(1).min(rows.saturating_sub(1));
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

    fn rebuild_inline_scrollback(&mut self, new_lines: &[String], rows: usize) {
        self.begin_synchronized_output();
        if self.previous_frame.iter().any(|line| is_image_line(line))
            || new_lines.iter().any(|line| is_image_line(line))
        {
            self.terminal.write(&delete_all_kitty_images());
        }
        self.terminal.write("\x1b[H");
        self.terminal.clear_screen();
        self.terminal.write("\x1b[3J");
        self.terminal.write("\x1b[H");
        for (index, line) in new_lines.iter().enumerate() {
            self.terminal.write(line);
            if index + 1 < new_lines.len() {
                self.terminal.write("\n");
            }
        }
        self.end_synchronized_output();
        self.inline_bottom_row = new_lines.len().min(rows).saturating_sub(1);
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
        if self.capabilities.synchronized_output {
            self.terminal.write("\x1b[?2026h");
        }
    }

    fn end_synchronized_output(&mut self) {
        if self.capabilities.synchronized_output {
            self.terminal.write("\x1b[?2026l");
        }
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
    if visible < width {
        format!("{}{}", line, " ".repeat(width - visible))
    } else if visible > width {
        crate::utils::truncate_to_width(line, width, Some(""))
    } else {
        line.to_owned()
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

fn extract_cursor_position(lines: &mut [String], height: u16) -> Option<(u16, u16)> {
    let total_lines = lines.len();
    extract_cursor_position_from(lines, 0, total_lines, height)
}

fn extract_cursor_position_from(
    lines: &mut [String],
    row_offset: usize,
    total_lines: usize,
    height: u16,
) -> Option<(u16, u16)> {
    let viewport_start = total_lines.saturating_sub(usize::from(height));
    let mut cursor = None;
    for (local_row, line) in lines.iter_mut().enumerate() {
        let row = row_offset.saturating_add(local_row);
        while let Some(offset) = line.find(CURSOR_MARKER) {
            let column = visible_width(&line[..offset]);
            line.replace_range(offset..offset + CURSOR_MARKER.len(), "");
            if row >= viewport_start {
                cursor = Some(((row - viewport_start) as u16, column as u16));
            }
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

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
                commit_boundary: None,
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
                commit_boundary: None,
                reanchor_viewport: false,
                rebuild_scrollback: false,
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
                commit_boundary: None,
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
        tui.write_inline_pinned(&shifted, 3, 0, false, &previous_window);

        assert!(
            writes.borrow().is_empty(),
            "unchanged physical cells were repainted: {:?}",
            writes.borrow()
        );
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

        tui.write_inline_pinned(&shorter, 3, 0, false, &previous_window);

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

        tui.write_inline_pinned(&replacement, 3, 0, true, &previous_window);

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
    fn inline_resize_deletes_kitty_placements_before_repainting_plain_rows() {
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
        assert_eq!(
            clears.get(),
            0,
            "resize must not push the old grid into native scrollback"
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
            .map(|index| format!("old-theme row {index}{RESET}"))
            .collect();
        tui.previous_size = Some((30, 4));
        tui.first_render = false;
        tui.inline_bottom_row = 3;
        tui.running = true;

        tui.request_render();

        let output = writes.borrow().join("");
        let clear_saved = output
            .find("\x1b[3J")
            .expect("presentation reset did not erase saved lines");
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
    fn inline_scrollback_resize_repaints_only_the_visible_tail() {
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
        assert_eq!(
            clears.get(),
            clears_before,
            "inline resize must preserve terminal-owned history"
        );
        // Only the last `height` logical lines are repainted; earlier retained
        // rows remain virtual and are not emitted during resize.
        assert!(
            repaint.contains("row 2\nrow 3\nrow 4\nrow 5"),
            "{repaint:?}"
        );
        assert!(!repaint.contains("row 1"));
    }

    #[test]
    fn resize_forces_a_home_and_full_redraw() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, clears, _, _, _, writes) = recording_terminal(size.clone(), capabilities);
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(OneLine));
        tui.start();
        writes.borrow_mut().clear();
        size.set((80, 24));
        tui.request_render();

        assert_eq!(clears.get(), 1);
        assert_eq!(
            writes
                .borrow()
                .iter()
                .filter(|write| write.as_str() == "\x1b[H")
                .count(),
            2
        );
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
    fn pure_append_and_shrink_update_only_the_changed_tail() {
        let size = Rc::new(Cell::new((20, 8)));
        let capabilities = crate::capabilities::TerminalCapabilities::interactive(
            crate::capabilities::ColorDepth::Ansi16,
            true,
        );
        let (terminal, _, tail_clears, _, _, writes) = recording_terminal(size, capabilities);
        let lines = Rc::new(RefCell::new(vec!["first".to_owned()]));
        let mut tui = TUI::new(Box::new(terminal));
        tui.add_child(Box::new(MutableLines(lines.clone())));
        tui.start();
        writes.borrow_mut().clear();

        lines.borrow_mut().push("second".to_owned());
        tui.request_render();
        assert!(writes.borrow().iter().any(|write| write.contains("second")));

        writes.borrow_mut().clear();
        lines.borrow_mut().truncate(1);
        tui.request_render();
        assert!(tail_clears.get() >= 2);
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
    fn synchronized_output_is_gated_and_cursor_marker_uses_display_cells() {
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
        assert!(!output.contains("?2026"));
        assert!(!output.contains(CURSOR_MARKER));
        assert!(output.contains("\x1b[1;3H"));
        assert_eq!(shows.get(), 1, "cursor marker must reveal the block cursor");
        drop(output);

        tui.stop();
        assert_eq!(stops.get(), 1);
        assert_eq!(shows.get(), 2, "stop restores the user's cursor");
        assert!(writes.borrow().join("").contains("\x1b]8;;\x1b\\"));

        let synchronized = capabilities.with_overrides(&crate::capabilities::CapabilityOverrides {
            synchronized_output: Some(true),
            ..crate::capabilities::CapabilityOverrides::default()
        });
        let size = Rc::new(Cell::new((20, 8)));
        let (terminal, _, _, _, _, synchronized_writes) = recording_terminal(size, synchronized);
        let mut synchronized_tui = TUI::new(Box::new(terminal));
        synchronized_tui.add_child(Box::new(OneLine));
        synchronized_tui.start();
        let output = synchronized_writes.borrow().join("");
        assert!(output.contains("\x1b[?2026h"));
        assert!(output.contains("\x1b[?2026l"));
    }
}
