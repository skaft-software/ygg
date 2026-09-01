//! Startup welcome-card lifecycle and presentation.

use std::path::PathBuf;
use std::time::Instant;

use super::{fit_line, ShellState, TranscriptBlock};

fn welcome_is_mutable(state: &ShellState) -> bool {
    state.startup_card_started_at.is_some()
        && !state.transcript.iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::User { .. }
                    | TranscriptBlock::Assistant(_)
                    | TranscriptBlock::Reasoning(_)
                    | TranscriptBlock::Tool(_)
                    | TranscriptBlock::Shell(_)
                    | TranscriptBlock::Outcome(_)
                    | TranscriptBlock::Compaction(_)
            )
        })
}

pub(super) fn welcome_animating(state: &ShellState, now: Instant) -> bool {
    welcome_is_mutable(state)
        && state.theme.capabilities().animation
        && state.overlay.is_none()
        && state.startup_card_started_at.is_some_and(|started| {
            now.saturating_duration_since(started).as_secs_f32() < crate::tui::splash::DURATION
        })
}

pub(super) fn restart_welcome_animation(state: &mut ShellState) {
    if welcome_is_mutable(state) {
        state.startup_card_started_at = Some(Instant::now());
        state.invalidate_transcript_layout();
    }
}

fn welcome_workspace(state: &ShellState) -> String {
    let Some(path) = state.workspace.as_deref() else {
        return "workspace unavailable".to_owned();
    };
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        if let Ok(relative) = path.strip_prefix(home) {
            return format!("~/{}", relative.display());
        }
    }
    path.display().to_string()
}

fn render_pi_startup(state: &ShellState, width: u16) -> Vec<String> {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let accent = |text: &str| state.theme.fg("accent", text);
    let muted = |text: &str| state.theme.dim(text);
    [
        format!("{} {}", accent("ygg"), muted(&version)),
        muted("escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more"),
        muted("Press ctrl+o to show full startup help and loaded resources."),
        String::new(),
        muted(
            "Ygg can explain its own features and look up its docs. Ask it how to use or extend Ygg.",
        ),
    ]
    .into_iter()
    .map(|line| fit_line(&line, width))
    .collect()
}

pub(super) fn render_welcome_card(
    state: &ShellState,
    width: u16,
    max_rows: usize,
    now: Instant,
) -> Vec<String> {
    let Some(started) = state.startup_card_started_at else {
        return Vec::new();
    };
    if state.overlay.is_some() || width < 24 || max_rows < 7 {
        return Vec::new();
    }
    if state
        .theme
        .resolve::<String>("startup")
        .is_some_and(|startup| startup.trim().eq_ignore_ascii_case("pi"))
    {
        return render_pi_startup(state, width);
    }

    const ROWS: usize = 6;
    let elapsed = if state.theme.capabilities().animation && welcome_is_mutable(state) {
        now.saturating_duration_since(started).as_secs_f32()
    } else {
        crate::tui::splash::DURATION
    };
    let logo_width = (usize::from(width) / 3).clamp(14, 24);
    let adaptive_accent = state
        .theme
        .is_compiled_default()
        .then(|| state.theme.model_rgb(state.model_lab))
        .flatten();
    let splash_color = state.theme.role_rgb("splash");
    let logo = crate::tui::splash::render_logo(
        &state.theme,
        logo_width,
        ROWS,
        elapsed,
        adaptive_accent,
        splash_color,
    );
    let splash_text = |text: &str| {
        splash_color
            .map(|_| state.theme.fg("splash", text))
            .unwrap_or_else(|| text.to_owned())
    };
    let splash_bold = |text: &str| {
        splash_color
            .map(|_| state.theme.bold(&state.theme.fg("splash", text)))
            .unwrap_or_else(|| state.theme.bold(text))
    };

    let model = if state.model_display.trim().is_empty() {
        state.model.as_str()
    } else {
        state.model_display.as_str()
    };
    let model = if model.trim().is_empty() {
        "selecting model…"
    } else {
        model
    };
    let text = if splash_color.is_some() {
        [
            format!(
                "{} {}",
                splash_bold("ygg"),
                splash_text(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            ),
            String::new(),
            splash_text(&format!("{model} / {}", state.reasoning)),
            splash_text(&welcome_workspace(state)),
            if state.safe_mode {
                format!(
                    "{} {}",
                    splash_text("permissions:"),
                    splash_bold("safe mode")
                )
            } else {
                format!(
                    "{} {}",
                    splash_text("permissions:"),
                    splash_bold("full access")
                )
            },
            format!("{} {}", splash_bold("Ctrl+D"), splash_text("to exit")),
        ]
    } else {
        [
            format!(
                "{} {}",
                state.theme.bold(&state.theme.fg("model_accent", "ygg")),
                state.theme.dim(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            ),
            String::new(),
            state
                .theme
                .fg("foreground", &format!("{model} / {}", state.reasoning)),
            state.theme.dim(&welcome_workspace(state)),
            if state.safe_mode {
                format!(
                    "{} {}",
                    state.theme.dim("permissions:"),
                    state.theme.bold(&state.theme.fg("accent", "safe mode"))
                )
            } else {
                format!(
                    "{} {}",
                    state.theme.dim("permissions:"),
                    state.theme.bold(&state.theme.fg("error", "full access"))
                )
            },
            format!(
                "{} {}",
                state.theme.bold("Ctrl+D"),
                state.theme.dim("to exit")
            ),
        ]
    };

    let box_color = state.theme.role_rgb("splash_box");
    if let Some(color) = box_color {
        let inner_width = usize::from(width).saturating_sub(2);
        let paint = |text: &str| state.theme.rgb_fg(color, text);
        let pad = |text: String| {
            let text_width = sexy_tui_rs::visible_width(&text);
            format!(
                "{text}{}",
                " ".repeat(inner_width.saturating_sub(text_width))
            )
        };
        let edge = |left: &str, right: &str| {
            paint(&format!(
                "{left}{}{right}",
                state.theme.glyph("horizontal").repeat(inner_width)
            ))
        };
        let body = |content: String| {
            let content = pad(fit_line(&content, inner_width as u16));
            paint(&format!(
                "{}{}{}",
                state.theme.glyph("vertical"),
                content,
                state.theme.glyph("vertical")
            ))
        };
        let mut lines = Vec::with_capacity(ROWS + 2);
        lines.push(edge(
            state.theme.glyph("top_left"),
            state.theme.glyph("top_right"),
        ));
        for row in 0..ROWS {
            lines.push(body(format!("  {}   {}", logo[row], text[row])));
        }
        lines.push(edge(
            state.theme.glyph("bottom_left"),
            state.theme.glyph("bottom_right"),
        ));
        lines
    } else {
        let mut lines = Vec::with_capacity(ROWS + 2);
        lines.push(String::new());
        for row in 0..ROWS {
            lines.push(fit_line(&format!("  {}   {}", logo[row], text[row]), width));
        }
        lines.push(String::new());
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::InteractiveShell;
    use sexy_tui_rs::strip_terminal_sequences;

    #[test]
    fn custom_welcome_theme_is_solid_accent_and_framed() {
        let shell =
            InteractiveShell::test_shell_with_theme(crate::tui::theme::test_theme_from_source(
                r##"
                    [metadata]
                    name = "Welcome fixture"
                    adaptive = false
                    [colors]
                    splash = "#d97757"
                    splash_box = "#d97757"
                "##,
            ));
        shell.state.borrow_mut().startup_card_started_at = Some(Instant::now());
        let rendered = render_welcome_card(&shell.state.borrow(), 80, 10, Instant::now());
        assert!(
            rendered.iter().all(|line| line.contains("38;2;217;119;87")),
            "{rendered:?}"
        );
        let plain = rendered
            .iter()
            .map(|line| strip_terminal_sequences(line))
            .collect::<Vec<_>>();
        assert!(
            plain[0].starts_with('╭') && plain[0].ends_with('╮'),
            "{plain:?}"
        );
        assert!(plain
            .last()
            .is_some_and(|line| line.starts_with('╰') && line.ends_with('╯')));
        assert!(rendered
            .iter()
            .all(|line| sexy_tui_rs::visible_width(line) == 80));
    }

    #[test]
    fn welcome_card_shows_access_mode_and_safe_mode_hint() {
        let shell = InteractiveShell::test_shell();
        shell.state.borrow_mut().startup_card_started_at = Some(Instant::now());
        let rendered =
            render_welcome_card(&shell.state.borrow(), 80, 10, Instant::now()).join("\n");
        let rendered = strip_terminal_sequences(&rendered);
        assert!(rendered.contains("permissions: full access"), "{rendered}");
        assert!(
            rendered.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "{rendered}"
        );

        let safe_shell = InteractiveShell::test_shell();
        safe_shell.state.borrow_mut().startup_card_started_at = Some(Instant::now());
        safe_shell.state.borrow_mut().safe_mode = true;
        let rendered =
            render_welcome_card(&safe_shell.state.borrow(), 80, 10, Instant::now()).join("\n");
        let rendered = strip_terminal_sequences(&rendered);
        assert!(rendered.contains("permissions: safe mode"), "{rendered}");
    }
}
