//! Markdown components backed by the semantic rich-text pipeline.

use std::cell::RefCell;

use crate::capabilities::TerminalCapabilities;
use crate::rich_text::markdown;
use crate::rich_text::render::{RenderOptions, RichRenderer};
use crate::rich_text::stream::{StreamingMarkdown, StreamingRenderCache};
use crate::rich_text::Document;
use crate::theme::Theme;
use crate::tui::Component;

/// Legacy closure theme retained for source compatibility. New code should use
/// [`Theme`] with [`Markdown::with_renderer`].
pub struct MarkdownTheme {
    pub heading: Box<dyn Fn(&str) -> String>,
    pub bold: Box<dyn Fn(&str) -> String>,
    pub code: Box<dyn Fn(&str) -> String>,
    pub code_block_border: Box<dyn Fn(&str) -> String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkdownOptions {
    pub padding_x: u16,
    pub padding_y: u16,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            padding_x: 1,
            padding_y: 1,
        }
    }
}

/// Static semantic Markdown renderer. Parsing occurs only when text changes;
/// resizing reuses the retained document.
pub struct Markdown {
    content: String,
    document: Document,
    padding_x: u16,
    padding_y: u16,
    renderer: RichRenderer,
    // Retained so dropping a widget has the same ownership behavior as v0.1.
    // Closure-based styling cannot safely participate in semantic rendering.
    _legacy_theme: Option<MarkdownTheme>,
}

impl Markdown {
    /// Backward-compatible constructor.
    pub fn new(
        content: &str,
        padding_x: u16,
        padding_y: u16,
        theme: Option<MarkdownTheme>,
    ) -> Self {
        let capabilities = crate::terminal_image::get_capabilities();
        Self {
            content: content.to_owned(),
            document: markdown::parse(content),
            padding_x,
            padding_y,
            renderer: RichRenderer::new(
                Theme::with_capabilities(capabilities),
                capabilities,
                RenderOptions::default(),
            ),
            _legacy_theme: theme,
        }
    }

    pub fn with_renderer(content: &str, options: MarkdownOptions, renderer: RichRenderer) -> Self {
        Self {
            content: content.to_owned(),
            document: markdown::parse(content),
            padding_x: options.padding_x,
            padding_y: options.padding_y,
            renderer,
            _legacy_theme: None,
        }
    }

    pub fn plain(content: &str, options: MarkdownOptions) -> Self {
        Self::with_renderer(content, options, RichRenderer::plain())
    }

    pub fn set_text(&mut self, text: &str) {
        if self.content == text {
            return;
        }
        self.content.clear();
        self.content.push_str(text);
        self.document = markdown::parse(text);
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    pub fn renderer(&self) -> &RichRenderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut RichRenderer {
        &mut self.renderer
    }

    pub const fn capabilities(&self) -> TerminalCapabilities {
        self.renderer.capabilities()
    }
}

impl Component for Markdown {
    fn render(&self, width: u16) -> Vec<String> {
        let padding_x = self.padding_x.min(width / 2);
        let inner = width.saturating_sub(padding_x.saturating_mul(2));
        let prefix = " ".repeat(usize::from(padding_x));
        let mut lines = vec![String::new(); usize::from(self.padding_y)];
        lines.extend(
            self.renderer
                .render(&self.document, inner)
                .lines
                .into_iter()
                .map(|line| format!("{prefix}{}", line.styled)),
        );
        lines.extend(vec![String::new(); usize::from(self.padding_y)]);
        lines
    }

    fn invalidate(&mut self) {}
}

/// Streaming Markdown component with stable-prefix and mutable-tail caches.
pub struct StreamingMarkdownWidget {
    stream: StreamingMarkdown,
    renderer: RichRenderer,
    cache: RefCell<StreamingRenderCache>,
    options: MarkdownOptions,
}

impl StreamingMarkdownWidget {
    pub fn new(renderer: RichRenderer, options: MarkdownOptions) -> Self {
        Self {
            stream: StreamingMarkdown::new(),
            renderer,
            cache: RefCell::new(StreamingRenderCache::default()),
            options,
        }
    }

    pub fn plain(options: MarkdownOptions) -> Self {
        Self::new(RichRenderer::plain(), options)
    }

    pub fn push_str(&mut self, chunk: &str) {
        self.stream.push_str(chunk);
    }

    pub fn push_bytes(&mut self, chunk: &[u8]) {
        self.stream.push_bytes(chunk);
    }

    pub fn finish(&mut self) -> &Document {
        self.stream.finish()
    }

    pub fn stream(&self) -> &StreamingMarkdown {
        &self.stream
    }

    pub fn stream_mut(&mut self) -> &mut StreamingMarkdown {
        &mut self.stream
    }

    pub fn renderer(&self) -> &RichRenderer {
        &self.renderer
    }

    pub fn renderer_mut(&mut self) -> &mut RichRenderer {
        self.cache
            .get_mut()
            .clone_from(&StreamingRenderCache::default());
        &mut self.renderer
    }
}

impl Component for StreamingMarkdownWidget {
    fn render(&self, width: u16) -> Vec<String> {
        let padding_x = self.options.padding_x.min(width / 2);
        let inner = width.saturating_sub(padding_x.saturating_mul(2));
        let prefix = " ".repeat(usize::from(padding_x));
        let mut lines = vec![String::new(); usize::from(self.options.padding_y)];
        let rendered = self
            .cache
            .borrow_mut()
            .render(&self.stream, &self.renderer, inner);
        lines.extend(
            rendered
                .lines
                .into_iter()
                .map(|line| format!("{prefix}{}", line.styled)),
        );
        lines.extend(vec![String::new(); usize::from(self.options.padding_y)]);
        lines
    }

    fn invalidate(&mut self) {
        *self.cache.get_mut() = StreamingRenderCache::default();
    }
}
