//! Retained renderer scheduling, shared state, and frame bookkeeping.

use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use sexy_tui_rs::{CommitCursor, Component, FrameUpdate, TUI};

use super::native_scrollback::{
    render_shell, render_shell_update_with_cursor, synchronize_shell_frame,
};
use super::transcript_history::materialize_deferred_session_history;
use super::viewport::{render_shell_viewport_at, render_shell_viewport_update};
use super::ShellState;
use crate::tui::terminal::{TerminalSize, YggTerminal};

/// Default render cap — roughly 60 fps. Decorative shimmer uses the separate,
/// deliberately slower cap below; input and streamed output stay on this path.
const RENDER_INTERVAL: Duration = Duration::from_millis(16);
/// Modern terminals get a restrained 20 FPS shimmer. The retained renderer
/// emits only changed border cells, so this leaves input and streaming work
/// well ahead of decorative frames.
const ANIMATION_RENDER_INTERVAL: Duration = Duration::from_millis(50);
/// Wake near the next eligible animation frame; input commands still preempt
/// this wait through the bounded render channel.
const ANIMATION_POLL_TIMEOUT: Duration = Duration::from_millis(45);
/// Tool activity dots share one restrained 900 ms cycle. Toggling the cell
/// ourselves works in terminals that intentionally ignore SGR blink.
const EVENT_DOT_TOGGLE_INTERVAL: Duration = Duration::from_millis(450);
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

/// True when the perimeter-shimmer animation is visible and moving. When
/// false we can use a lazy poll interval to save CPU.
pub(super) fn shimmer_animating(state: &ShellState) -> bool {
    let capabilities = state.theme.capabilities();
    if !capabilities.animation
        || capabilities.color == crate::tui::terminal::ColorDepth::None
        || state.size.0 < 12
    {
        return false;
    }
    if state.run_label == "compacting" {
        return true;
    }
    let Some(run) = state.run.current() else {
        return false;
    };
    if !run.is_active() || state.reasoning.trim().eq_ignore_ascii_case("off") {
        return false;
    }
    // The helper returns a fixed positive velocity only for working phases;
    // approval waits deliberately leave the border still and do not need a
    // high-frequency repaint.
    crate::tui::composer_surface::phase_speed_for(Some(run.phase())) > 0.0
}

pub(super) fn welcome_animating(state: &ShellState, now: Instant) -> bool {
    state.welcome_is_mutable()
        && state.theme.capabilities().animation
        && state.overlay.is_none()
        && state.startup_card_started_at.is_some_and(|started| {
            now.saturating_duration_since(started).as_secs_f32() < crate::tui::splash::DURATION
        })
}

pub(super) fn event_dot_animating(state: &ShellState) -> bool {
    let capabilities = state.theme.capabilities();
    capabilities.animation && capabilities.interactive && state.has_active_event_dot()
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
        // Choose the poll timeout based on whether the shimmer animation
        // would be rendered this frame. When it is, use a short timeout so
        // the wave stays fluid on high-refresh terminals. Otherwise use a
        // 100 ms status/resize poll; idle timeouts do not render unless the
        // terminal dimensions actually changed.
        let (animating, welcome, event_dot, is_active) = {
            let shell = state.borrow();
            let active = shell.run.is_active();
            let compacting = shell.run_label == "compacting";
            let shimmer = (active || compacting) && shimmer_animating(&shell);
            let welcome = welcome_animating(&shell, Instant::now());
            let event_dot = event_dot_animating(&shell);
            (
                shimmer || welcome,
                welcome,
                event_dot,
                active || compacting || welcome || event_dot,
            )
        };
        if !event_dot {
            last_event_dot_toggle = Instant::now();
        }
        let command = if animating {
            let poll = if welcome {
                RENDER_INTERVAL
            } else {
                ANIMATION_POLL_TIMEOUT
            };
            match rx.recv_timeout(poll) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv_timeout(RESIZE_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        if matches!(command, Some(RenderCommand::Stop)) {
            break;
        }

        let resized = if command.is_none() {
            synchronize_terminal_size(&state, &size)
        } else {
            false
        };
        if command.is_none() && !resized && !animating && !is_active {
            continue;
        }

        // Cap rendering to a sensible upper bound. Shimmer is deliberately
        // slower than input/streaming frames and changes only a few cells.
        let cap = if welcome {
            RENDER_INTERVAL
        } else if animating {
            ANIMATION_RENDER_INTERVAL
        } else {
            RENDER_INTERVAL
        };
        if let Some(last) = last_render {
            let elapsed = last.elapsed();
            if elapsed < cap {
                thread::sleep(cap - elapsed);
            }
        }

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

        let advance_event_dot =
            event_dot && last_event_dot_toggle.elapsed() >= EVENT_DOT_TOGGLE_INTERVAL;
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
