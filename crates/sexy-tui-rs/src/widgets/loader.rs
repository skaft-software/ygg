use crate::capabilities::TerminalCapabilities;
use crate::glyphs::GlyphSet;
use crate::sanitize::sanitize_line;
use crate::tui::Component;

pub type LoaderStyle = Box<dyn Fn(&str) -> String>;

/// Loader indicator options. Animation is decorative and capability-gated.
pub struct LoaderIndicatorOptions {
    pub frames: Vec<String>,
    pub interval_ms: u64,
}

impl Default for LoaderIndicatorOptions {
    fn default() -> Self {
        Self {
            frames: vec![
                "⠋".into(),
                "⠙".into(),
                "⠹".into(),
                "⠸".into(),
                "⠼".into(),
                "⠴".into(),
                "⠦".into(),
                "⠧".into(),
                "⠇".into(),
                "⠏".into(),
            ],
            interval_ms: 80,
        }
    }
}

/// Animated loading spinner. The message carries all meaning; plain and
/// animation-disabled profiles render one static ASCII status.
pub struct Loader {
    message: String,
    spinner_color: LoaderStyle,
    msg_color: LoaderStyle,
    frame: usize,
    options: LoaderIndicatorOptions,
    capabilities: TerminalCapabilities,
}

impl Loader {
    pub fn new(spinner_color: LoaderStyle, msg_color: LoaderStyle, message: &str) -> Self {
        Self::with_capabilities(
            spinner_color,
            msg_color,
            message,
            crate::terminal_image::get_capabilities(),
        )
    }

    pub fn with_capabilities(
        spinner_color: LoaderStyle,
        msg_color: LoaderStyle,
        message: &str,
        capabilities: TerminalCapabilities,
    ) -> Self {
        Self {
            message: message.to_owned(),
            spinner_color,
            msg_color,
            frame: 0,
            options: LoaderIndicatorOptions::default(),
            capabilities,
        }
    }

    pub fn set_message(&mut self, message: &str) {
        self.message.clear();
        self.message.push_str(message);
    }

    pub fn set_indicator_options(&mut self, options: LoaderIndicatorOptions) {
        self.options = options;
        self.frame = 0;
    }

    pub fn tick(&mut self) {
        if self.capabilities.animation && !self.options.frames.is_empty() {
            self.frame = (self.frame + 1) % self.options.frames.len();
        }
    }
}

impl Component for Loader {
    fn render(&self, width: u16) -> Vec<String> {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let safe_message = sanitize_line(&self.message, !self.capabilities.unicode);
        let indicator = if !self.capabilities.animation || self.options.frames.is_empty() {
            glyphs.pending.to_owned()
        } else if self.capabilities.unicode {
            sanitize_line(
                &self.options.frames[self.frame % self.options.frames.len()],
                false,
            )
            .into_owned()
        } else {
            ["-", "\\", "|", "/"][self.frame % 4].to_owned()
        };
        let line = if self.capabilities.plain {
            format!("{} {safe_message}", glyphs.pending)
        } else {
            format!(
                "{} {}",
                (self.spinner_color)(&indicator),
                (self.msg_color)(&safe_message)
            )
        };
        vec![crate::utils::truncate_to_width(
            &line,
            usize::from(width),
            Some(glyphs.ellipsis),
        )]
    }

    fn invalidate(&mut self) {}
}

/// Cancellable loader — adds Escape handling and abort state.
pub struct CancellableLoader {
    loader: Loader,
    pub aborted: bool,
}

impl CancellableLoader {
    pub fn new(spinner_color: LoaderStyle, msg_color: LoaderStyle, message: &str) -> Self {
        Self {
            loader: Loader::new(spinner_color, msg_color, message),
            aborted: false,
        }
    }
}

impl Component for CancellableLoader {
    fn render(&self, width: u16) -> Vec<String> {
        self.loader.render(width)
    }

    fn handle_input(&mut self, data: &str) {
        use crate::keys::{matches_key, Key};
        if matches_key(data, Key::escape) {
            self.aborted = true;
        }
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> LoaderStyle {
        Box::new(str::to_owned)
    }

    #[test]
    fn plain_loader_is_static_ascii_safe_and_width_bounded() {
        let mut loader = Loader::with_capabilities(
            identity(),
            identity(),
            "wait\x1b]52;c;bad\x07",
            TerminalCapabilities::plain(),
        );
        let first = loader.render(12);
        loader.tick();
        assert_eq!(loader.render(12), first);
        assert!(first[0].is_ascii());
        assert!(!first[0].contains('\x1b'));
        assert!(crate::utils::visible_width(&first[0]) <= 12);
    }

    #[test]
    fn empty_custom_frames_do_not_panic() {
        let mut loader = Loader::with_capabilities(
            identity(),
            identity(),
            "waiting",
            TerminalCapabilities::interactive(crate::ColorDepth::Ansi16, true),
        );
        loader.set_indicator_options(LoaderIndicatorOptions {
            frames: Vec::new(),
            interval_ms: 80,
        });
        loader.tick();
        assert!(loader.render(20)[0].contains("waiting"));
    }
}
