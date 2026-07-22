//! Conservative, caller-overridable terminal capability detection.
//!
//! Detection is deliberately a hint, not a negotiation protocol. Applications
//! may probe a terminal and apply [`CapabilityOverrides`] afterwards; explicit
//! values always win over environment heuristics.

use std::io::IsTerminal;

/// Terminal colour precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorDepth {
    /// Do not emit SGR colour sequences.
    #[default]
    None,
    /// The base and bright ANSI palette.
    Ansi16,
    /// The xterm 256-colour palette.
    Ansi256,
    /// 24-bit RGB colour.
    TrueColor,
}

/// Confidence attached to capabilities that are not reliably negotiable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SupportLevel {
    /// The feature is known not to be usable.
    Unsupported,
    /// There is not enough evidence to enable the feature by default.
    #[default]
    Unknown,
    /// The terminal is known or strongly expected to support the feature.
    Supported,
}

impl SupportLevel {
    /// Whether progressive enhancement may use this feature.
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

/// Terminal dimensions measured in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

/// A central description shared by renderers, widgets, and terminal backends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// Input and output are attached to an interactive terminal.
    pub interactive: bool,
    /// Conservative UTF-8/Unicode rendering is available.
    pub unicode: bool,
    /// Selected colour precision.
    pub color_depth: ColorDepth,
    /// Confidence that SGR italic renders distinctly.
    pub italics: SupportLevel,
    /// OSC 8 hyperlinks may be emitted.
    pub hyperlinks: bool,
    /// Relative or absolute cursor movement may be emitted.
    pub cursor_addressing: bool,
    /// Erase-in-line/display operations may be emitted.
    pub line_clearing: bool,
    /// Alternate-screen mode may be used.
    pub alternate_screen: bool,
    /// CSI 2026 synchronized output may be used.
    pub synchronized_output: bool,
    /// Bracketed paste may be enabled.
    pub bracketed_paste: bool,
    /// Animation is allowed. Comprehension must never depend on it.
    pub animation: bool,
    /// Dimensions known at detection time, if any.
    pub dimensions: Option<TerminalSize>,
    /// Plain/log mode: no cursor control, colour, hyperlinks, or animation.
    pub plain: bool,

    // Protocol-specific capabilities retained for the image/input APIs.
    pub kitty_graphics: bool,
    pub iterm2_images: bool,
    pub kitty_keyboard: bool,
    /// Compatibility alias for `synchronized_output`.
    pub sync_output: bool,
    /// Compatibility alias for `color_depth == TrueColor`.
    pub true_color: bool,
    /// Never inferred. Applications may explicitly opt into icon fonts.
    pub nerd_font: bool,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self::plain()
    }
}

impl TerminalCapabilities {
    /// A deterministic, escape-free profile suitable for logs and redirects.
    pub const fn plain() -> Self {
        Self {
            interactive: false,
            unicode: false,
            color_depth: ColorDepth::None,
            italics: SupportLevel::Unsupported,
            hyperlinks: false,
            cursor_addressing: false,
            line_clearing: false,
            alternate_screen: false,
            synchronized_output: false,
            bracketed_paste: false,
            animation: false,
            dimensions: None,
            plain: true,
            kitty_graphics: false,
            iterm2_images: false,
            kitty_keyboard: false,
            sync_output: false,
            true_color: false,
            nerd_font: false,
        }
    }

    /// A useful explicit profile for tests and embedding applications.
    pub const fn interactive(color_depth: ColorDepth, unicode: bool) -> Self {
        let true_color = matches!(color_depth, ColorDepth::TrueColor);
        Self {
            interactive: true,
            unicode,
            color_depth,
            italics: SupportLevel::Unknown,
            hyperlinks: false,
            cursor_addressing: true,
            line_clearing: true,
            alternate_screen: true,
            synchronized_output: false,
            bracketed_paste: true,
            animation: true,
            dimensions: None,
            plain: false,
            kitty_graphics: false,
            iterm2_images: false,
            kitty_keyboard: false,
            sync_output: false,
            true_color,
            nerd_font: false,
        }
    }

    /// Detect capabilities from process streams and environment variables.
    pub fn detect() -> Self {
        let probe = CapabilityProbe::from_process();
        Self::detect_from(&probe, &CapabilityOverrides::default())
    }

    /// Detect from an explicit probe. This is deterministic and testable.
    pub fn detect_from(probe: &CapabilityProbe, overrides: &CapabilityOverrides) -> Self {
        let term = probe
            .term
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let program = probe
            .term_program
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let colorterm = probe
            .colorterm
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let multiplexer = probe.tmux || term.starts_with("screen") || term.starts_with("tmux");
        let dumb = term == "dumb";
        let known_term = !term.is_empty() && !dumb;
        let interactive = probe.stdin_tty && probe.stdout_tty && known_term;
        let utf8_locale = probe.locale.as_deref().is_some_and(|locale| {
            let locale = locale.to_ascii_lowercase();
            locale.contains("utf-8") || locale.contains("utf8")
        });
        let rich = term.contains("ghostty")
            || term.contains("kitty")
            || term.contains("wezterm")
            || program.contains("ghostty")
            || program.contains("wezterm")
            || program == "iterm.app";
        let windows_terminal = probe.wt_session;
        let jetbrains = program.contains("jetbrains") || term.contains("jetbrains");

        let color_depth = if !interactive || probe.no_color {
            ColorDepth::None
        } else if colorterm == "truecolor" || colorterm == "24bit" || rich || windows_terminal {
            ColorDepth::TrueColor
        } else if term.contains("256color") {
            ColorDepth::Ansi256
        } else {
            ColorDepth::Ansi16
        };

        // Hyperlinks are intentionally disabled through tmux/screen unless an
        // application explicitly overrides after negotiating passthrough.
        let hyperlinks = interactive
            && !multiplexer
            && !jetbrains
            && (rich || windows_terminal || program.contains("vscode"));
        let synchronized_output = interactive
            && !multiplexer
            && (term.contains("ghostty") || term.contains("kitty") || term.contains("wezterm"));
        let kitty_family = interactive
            && (term.contains("kitty") || term.contains("ghostty") || term.contains("wezterm"));

        let mut capabilities = Self {
            interactive,
            unicode: interactive && utf8_locale,
            color_depth,
            italics: if interactive && rich {
                SupportLevel::Supported
            } else if interactive {
                SupportLevel::Unknown
            } else {
                SupportLevel::Unsupported
            },
            hyperlinks,
            cursor_addressing: interactive,
            line_clearing: interactive,
            alternate_screen: interactive,
            synchronized_output,
            bracketed_paste: interactive,
            animation: interactive,
            dimensions: probe.dimensions,
            plain: !interactive,
            kitty_graphics: kitty_family && !probe.cmux,
            iterm2_images: interactive && program == "iterm.app",
            kitty_keyboard: kitty_family,
            sync_output: synchronized_output,
            true_color: color_depth == ColorDepth::TrueColor,
            nerd_font: false,
        };

        overrides.apply(&mut capabilities);
        capabilities.normalize();
        capabilities
    }

    fn normalize(&mut self) {
        if self.plain || !self.interactive {
            self.plain = true;
            self.interactive = false;
            self.color_depth = ColorDepth::None;
            self.italics = SupportLevel::Unsupported;
            self.hyperlinks = false;
            self.cursor_addressing = false;
            self.line_clearing = false;
            self.alternate_screen = false;
            self.synchronized_output = false;
            self.bracketed_paste = false;
            self.animation = false;
            self.kitty_graphics = false;
            self.iterm2_images = false;
            self.kitty_keyboard = false;
            self.nerd_font = false;
        }
        self.sync_output = self.synchronized_output;
        self.true_color = self.color_depth == ColorDepth::TrueColor;
    }

    /// Apply explicit caller values after detection.
    pub fn with_overrides(mut self, overrides: &CapabilityOverrides) -> Self {
        overrides.apply(&mut self);
        self.normalize();
        self
    }
}

/// Inputs used by conservative capability detection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CapabilityProbe {
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub term: Option<String>,
    pub term_program: Option<String>,
    pub colorterm: Option<String>,
    pub locale: Option<String>,
    pub no_color: bool,
    pub tmux: bool,
    pub ssh: bool,
    pub wt_session: bool,
    pub cmux: bool,
    pub dimensions: Option<TerminalSize>,
}

impl CapabilityProbe {
    /// Capture process state without writing terminal queries.
    pub fn from_process() -> Self {
        let locale = std::env::var("LC_ALL")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::env::var("LC_CTYPE")
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| std::env::var("LANG").ok().filter(|value| !value.is_empty()));
        let dimensions = crossterm::terminal::size()
            .ok()
            .map(|(columns, rows)| TerminalSize { columns, rows });
        Self {
            stdin_tty: std::io::stdin().is_terminal(),
            stdout_tty: std::io::stdout().is_terminal(),
            term: std::env::var("TERM").ok().filter(|value| !value.is_empty()),
            term_program: std::env::var("TERM_PROGRAM")
                .ok()
                .filter(|value| !value.is_empty()),
            colorterm: std::env::var("COLORTERM")
                .ok()
                .filter(|value| !value.is_empty()),
            locale,
            no_color: std::env::var_os("NO_COLOR").is_some(),
            tmux: std::env::var_os("TMUX").is_some(),
            ssh: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some(),
            wt_session: std::env::var_os("WT_SESSION").is_some(),
            cmux: std::env::var_os("CMUX_SOCKET_PATH").is_some(),
            dimensions,
        }
    }
}

/// Caller overrides. Every `Some` value beats environment heuristics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapabilityOverrides {
    pub interactive: Option<bool>,
    pub unicode: Option<bool>,
    pub color_depth: Option<ColorDepth>,
    pub italics: Option<SupportLevel>,
    pub hyperlinks: Option<bool>,
    pub cursor_addressing: Option<bool>,
    pub line_clearing: Option<bool>,
    pub alternate_screen: Option<bool>,
    pub synchronized_output: Option<bool>,
    pub bracketed_paste: Option<bool>,
    pub animation: Option<bool>,
    pub dimensions: Option<TerminalSize>,
    pub plain: Option<bool>,
    pub kitty_graphics: Option<bool>,
    pub iterm2_images: Option<bool>,
    pub kitty_keyboard: Option<bool>,
    pub nerd_font: Option<bool>,
}

impl CapabilityOverrides {
    fn apply(self, capabilities: &mut TerminalCapabilities) {
        macro_rules! assign {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    capabilities.$field = value;
                }
            };
        }
        assign!(interactive);
        assign!(unicode);
        assign!(color_depth);
        assign!(italics);
        assign!(hyperlinks);
        assign!(cursor_addressing);
        assign!(line_clearing);
        assign!(alternate_screen);
        assign!(synchronized_output);
        assign!(bracketed_paste);
        assign!(animation);
        if let Some(value) = self.dimensions {
            capabilities.dimensions = Some(value);
        }
        assign!(plain);
        assign!(kitty_graphics);
        assign!(iterm2_images);
        assign!(kitty_keyboard);
        assign!(nerd_font);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(term: &str) -> CapabilityProbe {
        CapabilityProbe {
            stdin_tty: true,
            stdout_tty: true,
            term: Some(term.into()),
            locale: Some("en_US.UTF-8".into()),
            ..CapabilityProbe::default()
        }
    }

    #[test]
    fn dumb_redirected_and_unknown_are_plain() {
        assert!(
            TerminalCapabilities::detect_from(&probe("dumb"), &CapabilityOverrides::default())
                .plain
        );
        let mut redirected = probe("xterm-256color");
        redirected.stdout_tty = false;
        assert!(
            TerminalCapabilities::detect_from(&redirected, &CapabilityOverrides::default()).plain
        );
        assert!(
            TerminalCapabilities::detect_from(&probe(""), &CapabilityOverrides::default()).plain
        );
    }

    #[test]
    fn color_depth_degrades_conservatively() {
        let mut truecolor = probe("xterm-256color");
        truecolor.colorterm = Some("truecolor".into());
        assert_eq!(
            TerminalCapabilities::detect_from(&truecolor, &Default::default()).color_depth,
            ColorDepth::TrueColor
        );
        assert_eq!(
            TerminalCapabilities::detect_from(&probe("screen-256color"), &Default::default())
                .color_depth,
            ColorDepth::Ansi256
        );
        assert_eq!(
            TerminalCapabilities::detect_from(&probe("xterm"), &Default::default()).color_depth,
            ColorDepth::Ansi16
        );
    }

    #[test]
    fn no_color_and_tmux_are_respected() {
        let mut no_color = probe("xterm-256color");
        no_color.no_color = true;
        assert_eq!(
            TerminalCapabilities::detect_from(&no_color, &Default::default()).color_depth,
            ColorDepth::None
        );

        let mut tmux = probe("xterm-256color");
        tmux.term_program = Some("WezTerm".into());
        tmux.tmux = true;
        let caps = TerminalCapabilities::detect_from(&tmux, &Default::default());
        assert!(!caps.hyperlinks);
        assert!(!caps.synchronized_output);
    }

    #[test]
    fn explicit_overrides_win() {
        let mut no_color = probe("dumb");
        no_color.no_color = true;
        let caps = TerminalCapabilities::detect_from(
            &no_color,
            &CapabilityOverrides {
                interactive: Some(true),
                plain: Some(false),
                color_depth: Some(ColorDepth::TrueColor),
                hyperlinks: Some(true),
                ..CapabilityOverrides::default()
            },
        );
        assert!(caps.interactive);
        assert!(!caps.plain);
        assert_eq!(caps.color_depth, ColorDepth::TrueColor);
        assert!(caps.hyperlinks);
    }
}
