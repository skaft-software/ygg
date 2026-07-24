use crate::rich_text::render::RichRenderer;
use crate::rich_text::Document;
use crate::tui::Component;

pub type TextBackground = Box<dyn Fn(&str) -> String>;

/// Text widget — displays text with word wrapping and padding.
pub struct Text {
    content: String,
    padding_x: u16,
    padding_y: u16,
    bg_fn: Option<TextBackground>,
    trusted_ansi: bool,
    capabilities: crate::TerminalCapabilities,
}

impl Text {
    pub fn new(
        content: &str,
        padding_x: u16,
        padding_y: u16,
        bg_fn: Option<TextBackground>,
    ) -> Self {
        Text {
            content: content.to_string(),
            padding_x,
            padding_y,
            bg_fn,
            trusted_ansi: false,
            capabilities: crate::terminal_image::get_capabilities(),
        }
    }

    /// Opt into the legacy trusted-ANSI path. Never pass untrusted text here.
    pub fn trusted_ansi(
        content: &str,
        padding_x: u16,
        padding_y: u16,
        bg_fn: Option<TextBackground>,
    ) -> Self {
        Self {
            content: content.to_owned(),
            padding_x,
            padding_y,
            bg_fn,
            trusted_ansi: true,
            capabilities: crate::terminal_image::get_capabilities(),
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.content = text.to_string();
    }

    pub fn set_capabilities(&mut self, capabilities: crate::TerminalCapabilities) {
        self.capabilities = capabilities;
    }
}

impl Component for Text {
    fn render(&self, width: u16) -> Vec<String> {
        let padding_x = self.padding_x.min(width / 2);
        let inner = width.saturating_sub(padding_x.saturating_mul(2));
        let spacer = " ".repeat(usize::from(padding_x));
        let mut lines = vec!["".to_string(); self.padding_y as usize];
        let capabilities = self.capabilities;
        let safe_content = if self.trusted_ansi {
            std::borrow::Cow::Borrowed(self.content.as_str())
        } else {
            crate::sanitize::sanitize_text(
                &self.content,
                crate::sanitize::SanitizeOptions {
                    controls: if capabilities.unicode {
                        crate::sanitize::ControlPictures::Unicode
                    } else {
                        crate::sanitize::ControlPictures::Ascii
                    },
                    preserve_newlines: true,
                    preserve_tabs: true,
                },
            )
        };
        for line in crate::utils::wrap_text_with_ansi(&safe_content, inner as usize) {
            let padded = format!("{}{}", spacer, line);
            lines.push(if let Some(ref bg) = self.bg_fn {
                if capabilities.plain {
                    padded
                } else {
                    bg(&padded)
                }
            } else {
                padded
            });
        }
        lines.extend(vec!["".to_string(); self.padding_y as usize]);
        lines
    }
    fn invalidate(&mut self) {}
}

/// Semantic rich-text component for documents built without Markdown.
pub struct RichText {
    document: Document,
    renderer: RichRenderer,
}

impl RichText {
    pub fn new(document: Document, renderer: RichRenderer) -> Self {
        Self { document, renderer }
    }

    pub fn plain(document: Document) -> Self {
        Self::new(document, RichRenderer::plain())
    }

    pub fn set_document(&mut self, document: Document) {
        self.document = document;
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn renderer_mut(&mut self) -> &mut RichRenderer {
        &mut self.renderer
    }
}

impl Component for RichText {
    fn render(&self, width: u16) -> Vec<String> {
        self.renderer.render(&self.document, width).styled_lines()
    }

    fn invalidate(&mut self) {}
}

/// TruncatedText widget — single-line text that truncates to fit width.
pub struct TruncatedText {
    content: String,
    padding_x: u16,
    padding_y: u16,
    trusted_ansi: bool,
    capabilities: crate::TerminalCapabilities,
}

impl TruncatedText {
    pub fn new(content: &str, padding_x: u16, padding_y: u16) -> Self {
        TruncatedText {
            content: content.to_string(),
            padding_x,
            padding_y,
            trusted_ansi: true,
            capabilities: crate::terminal_image::get_capabilities(),
        }
    }

    /// Rust/Ygg compatibility layer for untrusted text. Pi's core constructor
    /// treats SGR in component content as intentional styling.
    pub fn untrusted(content: &str, padding_x: u16, padding_y: u16) -> Self {
        let mut component = Self::new(content, padding_x, padding_y);
        component.trusted_ansi = false;
        component
    }

    pub fn set_capabilities(&mut self, capabilities: crate::TerminalCapabilities) {
        self.capabilities = capabilities;
    }
}

impl Component for TruncatedText {
    fn render(&self, width: u16) -> Vec<String> {
        let width = usize::from(width);
        let empty = " ".repeat(width);
        let padding_x = usize::from(self.padding_x);
        let inner = width.saturating_sub(padding_x.saturating_mul(2)).max(1);
        let first_line = self.content.split('\n').next().unwrap_or_default();
        let safe = if self.trusted_ansi {
            std::borrow::Cow::Borrowed(first_line)
        } else {
            crate::sanitize::sanitize_line(first_line, !self.capabilities.unicode)
        };
        let truncated = crate::utils::truncate_to_width(&safe, inner, None);
        let mut content = format!(
            "{}{}{}",
            " ".repeat(padding_x),
            truncated,
            " ".repeat(padding_x)
        );
        content.push_str(&" ".repeat(width.saturating_sub(crate::utils::visible_width(&content))));

        let mut lines = vec![empty.clone(); usize::from(self.padding_y)];
        lines.push(content);
        lines.extend(vec![empty; usize::from(self.padding_y)]);
        lines
    }
    fn invalidate(&mut self) {}
}

/// Spacer widget — empty vertical space.
pub struct Spacer {
    lines: u16,
}

impl Spacer {
    pub fn new(lines: u16) -> Self {
        Spacer { lines }
    }
}

impl Component for Spacer {
    fn render(&self, _width: u16) -> Vec<String> {
        vec!["".to_string(); self.lines as usize]
    }
    fn invalidate(&mut self) {}
}
