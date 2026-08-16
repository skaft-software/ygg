//! Retained renderer scheduling, shared state, and frame bookkeeping.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use sexy_tui_rs::{CommitCursor, Component, FrameUpdate, TUI};

use super::native_scrollback::{
    render_shell, render_shell_update_with_cursor, synchronize_shell_frame,
};
use super::transcript_history::materialize_deferred_session_history;
use super::viewport::{render_shell_viewport_at, render_shell_viewport_update};
use super::welcome_card::welcome_animating;
use super::ShellState;
use crate::tui::terminal::{TerminalSize, YggTerminal};

/// Welcome-card motion is short-lived and limited to roughly 60 fps.
const RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// Transcript activity shares one restrained one-second breathing cycle.
/// Toggling the glyph ourselves works in terminals that ignore SGR blink.
const EVENT_DOT_TOGGLE_INTERVAL: Duration = Duration::from_millis(500);
/// Resize events are normally delivered by crossterm, but polling while idle
/// also catches terminal-manager resizes that do not emit an event.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Thread-safe handle to the mutable shell model. The TUI renderer owns a
/// clone of this handle and performs all expensive layout work away from the
/// async agent/input loop.
#[derive(Clone)]
pub(super) struct SharedState(Arc<Mutex<ShellState>>);

impl SharedState {
    pub(super) fn new(state: ShellState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    pub(super) fn borrow(&self) -> MutexGuard<'_, ShellState> {
        self.0.lock().expect("shell state mutex poisoned")
    }

    pub(super) fn borrow_mut(&self) -> MutexGuard<'_, ShellState> {
        self.0.lock().expect("shell state mutex poisoned")
    }
}

pub(super) enum RenderCommand {
    Render,
    Stop,
}

pub(super) fn event_dot_animating(state: &ShellState) -> bool {
    let capabilities = state.theme.capabilities();
    capabilities.animation && capabilities.interactive && state.has_active_event_dot()
}

fn render_wake_requires_frame(
    semantic_command: bool,
    resized: bool,
    welcome: bool,
    event_dot_due: bool,
) -> bool {
    semantic_command || resized || welcome || event_dot_due
}

fn frame_coalesce_delay(last_render: Option<Instant>, now: Instant) -> Duration {
    last_render
        .map(|last| (last + RENDER_INTERVAL).saturating_duration_since(now))
        .unwrap_or_default()
}

/// Reconcile the renderer's shared dimensions with the terminal itself. This
/// is a fallback for environments where the resize signal is delayed or
/// swallowed; the normal input path still updates the same cells immediately.
fn synchronize_terminal_size(state: &SharedState, size: &TerminalSize) -> bool {
    let Ok(dimensions) = crossterm::terminal::size() else {
        return false;
    };
    reconcile_terminal_size(state, size, dimensions)
}

pub(super) fn reconcile_terminal_size(
    state: &SharedState,
    size: &TerminalSize,
    dimensions: (u16, u16),
) -> bool {
    let changed = {
        let mut current = size.lock().expect("terminal size mutex poisoned");
        if *current == dimensions {
            false
        } else {
            *current = dimensions;
            true
        }
    };
    if !changed {
        return false;
    }

    // This delayed-signal fallback must obey the same transcript contract as
    // the normal resize path: destructive replay cannot run from a bounded
    // first-paint tail.
    if let Err(error) = materialize_deferred_session_history(state) {
        state.borrow_mut().error = Some(format!("could not load older session history: {error}"));
    }

    let mut shell = state.borrow_mut();
    shell.size = dimensions;
    // Do not ask for transcript geometry here: that would synchronously reflow
    // the complete history and then invalidate it, paying the resize cost
    // twice. Viewport readers clamp the retained scroll offset after the render
    // thread performs the single required layout pass.
    shell.invalidate_transcript_layout();
    true
}

pub(super) fn render_loop(
    terminal: YggTerminal,
    state: SharedState,
    size: TerminalSize,
    rx: Receiver<RenderCommand>,
    application_viewport: bool,
) {
    let mut tui = TUI::new(Box::new(terminal));
    tui.set_inline_scrollback(true);
    tui.add_child(Box::new(ShellComponent::new(
        state.clone(),
        application_viewport,
    )));
    tui.start();

    let mut last_render: Option<Instant> = None;
    let mut last_event_dot_toggle = Instant::now();
    loop {
        // Only the short-lived welcome card gets a high-frequency wake. Work
        // itself is event-driven; the transcript pulse owns the sole slow
        // timer once a run is underway.
        let (welcome, event_dot) = {
            let shell = state.borrow();
            let welcome = welcome_animating(&shell, Instant::now());
            let event_dot = event_dot_animating(&shell);
            (welcome, event_dot)
        };
        if !event_dot {
            last_event_dot_toggle = Instant::now();
        }
        let poll = if welcome {
            RENDER_INTERVAL
        } else {
            RESIZE_POLL_INTERVAL
        };
        let command = match rx.recv_timeout(poll) {
            Ok(command) => Some(command),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if matches!(command, Some(RenderCommand::Stop)) {
            break;
        }

        let resized = if command.is_none() {
            synchronize_terminal_size(&state, &size)
        } else {
            false
        };
        let advance_event_dot =
            event_dot && last_event_dot_toggle.elapsed() >= EVENT_DOT_TOGGLE_INTERVAL;
        let semantic_command = matches!(command, Some(RenderCommand::Render));
        if !render_wake_requires_frame(semantic_command, resized, welcome, advance_event_dot) {
            continue;
        }

        // Coalesce everything already queued into the latest semantic state.
        let mut stop = false;
        while let Ok(next) = rx.try_recv() {
            if matches!(next, RenderCommand::Stop) {
                stop = true;
                break;
            }
        }
        if stop {
            break;
        }

        // Bound high-throughput model streams to one frame per terminal refresh
        // without bringing back the old uninterruptible animation sleep. The
        // receiver remains live during the short deadline: more semantic work
        // is folded into the pending frame, and Stop takes effect immediately.
        if let Some(last) = last_render {
            let delay = frame_coalesce_delay(Some(last), Instant::now());
            let deadline = Instant::now() + delay;
            while !delay.is_zero() && Instant::now() < deadline {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(RenderCommand::Render) => {}
                    Ok(RenderCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        stop = true;
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => break,
                }
            }
        }
        if stop {
            break;
        }

        if welcome || advance_event_dot {
            let mut shell = state.borrow_mut();
            if welcome {
                shell.invalidate_transcript_layout();
            }
            if advance_event_dot {
                shell.advance_event_dot_animation();
                last_event_dot_toggle = Instant::now();
            }
        }
        tui.request_render();
        last_render = Some(Instant::now());
    }

    tui.stop();
}

#[cfg(test)]
mod scheduler_tests {
    use std::time::{Duration, Instant};

    use super::{frame_coalesce_delay, render_wake_requires_frame};

    #[test]
    fn active_state_without_a_concrete_wake_never_requests_a_frame() {
        assert!(!render_wake_requires_frame(false, false, false, false));
    }

    #[test]
    fn semantic_work_and_due_visual_transitions_each_request_a_frame() {
        assert!(render_wake_requires_frame(true, false, false, false));
        assert!(render_wake_requires_frame(false, true, false, false));
        assert!(render_wake_requires_frame(false, false, true, false));
        assert!(render_wake_requires_frame(false, false, false, true));
    }

    #[test]
    fn semantic_bursts_coalesce_for_at_most_one_terminal_frame() {
        let last = Instant::now();
        assert_eq!(
            frame_coalesce_delay(Some(last), last + Duration::from_millis(1)),
            Duration::from_millis(15)
        );
        assert_eq!(
            frame_coalesce_delay(Some(last), last + Duration::from_millis(16)),
            Duration::ZERO
        );
        assert_eq!(frame_coalesce_delay(None, last), Duration::ZERO);
    }
}

#[derive(Default)]
pub(super) struct ShellFrameState {
    pub(super) initialized: bool,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) theme_epoch: u64,
    pub(super) transcript_epoch: u64,
    pub(super) transcript_generation: u64,
    pub(super) transcript_len: usize,
    pub(super) verbose_tools: bool,
    pub(super) overlay_active: bool,
    /// Rows of the native transcript frame retained above the screen-sized
    /// overlay surface. This bounds lazy diffs when mutable chrome changes the
    /// overlay's seam with terminal-owned history.
    pub(super) overlay_prefix_len: usize,
}

/// The retained root component. It reads the shell state at render time, while
/// `InteractiveShell` mutates that same state in response to events.
pub(super) struct ShellComponent {
    state: SharedState,
    frame: RefCell<ShellFrameState>,
    /// Explicit `--mouse app` mode keeps a bounded semantic viewport and pins
    /// chrome inside it. The default terminal-owned path lets committed rows
    /// enter native scrollback and leaves selection to the terminal.
    application_viewport: bool,
}

impl ShellComponent {
    pub(super) fn new(state: SharedState, application_viewport: bool) -> Self {
        Self {
            state,
            frame: RefCell::new(ShellFrameState::default()),
            application_viewport,
        }
    }
}

impl Component for ShellComponent {
    fn render(&self, width: u16) -> Vec<String> {
        let state = self.state.borrow();
        if self.application_viewport {
            let lines = render_shell_viewport_at(&state, width, Instant::now());
            let mut frame = self.frame.borrow_mut();
            frame.initialized = true;
            frame.width = width;
            frame.height = state.size.1;
            frame.theme_epoch = state.theme_epoch;
            frame.transcript_epoch = state.transcript_epoch;
            frame.verbose_tools = state.verbose_tools;
            lines
        } else {
            let lines = render_shell(&state, width);
            synchronize_shell_frame(&state, width, &mut self.frame.borrow_mut());
            lines
        }
    }

    fn render_update(&self, width: u16) -> Option<FrameUpdate> {
        self.render_update_with_cursor(width, None)
    }

    fn render_update_with_cursor(
        &self,
        width: u16,
        cursor: Option<CommitCursor>,
    ) -> Option<FrameUpdate> {
        let state = self.state.borrow();
        Some(if self.application_viewport {
            render_shell_viewport_update(
                &state,
                width,
                Instant::now(),
                &mut self.frame.borrow_mut(),
            )
        } else {
            render_shell_update_with_cursor(
                &state,
                width,
                Instant::now(),
                &mut self.frame.borrow_mut(),
                cursor,
            )
        })
    }

    fn invalidate(&mut self) {
        *self.frame.get_mut() = ShellFrameState::default();
    }
}
