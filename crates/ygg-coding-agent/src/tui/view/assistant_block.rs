//! Streaming assistant and reasoning block state with cached rich-text rendering.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use sexy_tui_rs::{
    parse_markdown, Block, Color, DiffRenderOptions, Inline, RichRenderer, StreamingLineUpdate,
    StreamingMarkdown, StreamingRenderCache, UnifiedDiff,
};

use super::terminal_text::sanitize_for_terminal;
use super::tool_render::looks_like_diff;
use crate::tui::theme::YggTheme;

fn reasoning_markdown_projection(source: &str) -> String {
    // OpenAI-style reasoning summaries can concatenate independently bolded
    // sections without whitespace: `**Plan****Verify**`. CommonMark treats the
    // middle four asterisks as literal text inside one strong span. Insert a
    // display-only block boundary while retaining `AssistantBlock::text` as the
    // exact provider/session source.
    source
        .replace("****", "**\n\n**")
        .replace("____", "__\n\n__")
}

fn append_reasoning_inline_text(inlines: &[Inline], output: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) | Inline::Raw(text) => output.push_str(text),
            Inline::Styled(span) => append_reasoning_inline_text(&span.content, output),
            Inline::Role { content, .. }
            | Inline::Status { content, .. }
            | Inline::Emphasis(content)
            | Inline::Strong(content)
            | Inline::Strikethrough(content) => append_reasoning_inline_text(content, output),
            Inline::Link { label, .. } => append_reasoning_inline_text(label, output),
            Inline::SoftBreak | Inline::HardBreak => output.push(' '),
        }
    }
}

fn normalized_reasoning_heading(inlines: &[Inline]) -> Option<String> {
    let mut heading = String::new();
    append_reasoning_inline_text(inlines, &mut heading);
    let heading = sanitize_for_terminal(&heading);
    let heading = heading.split_whitespace().collect::<Vec<_>>().join(" ");
    (!heading.is_empty()).then_some(heading)
}

pub(super) fn reasoning_heading_from_block(block: &Block) -> Option<String> {
    match block {
        Block::Heading { content, .. } => normalized_reasoning_heading(content),
        Block::Paragraph(content) => {
            let mut meaningful = content.iter().filter(|inline| {
                !matches!(inline, Inline::Text(text) | Inline::Raw(text) if text.trim().is_empty())
            });
            let Inline::Strong(heading) = meaningful.next()? else {
                return None;
            };
            meaningful
                .next()
                .is_none()
                .then(|| normalized_reasoning_heading(heading))
                .flatten()
        }
        _ => None,
    }
}

fn reasoning_delimiter_crosses_chunk_boundary(previous: &str, next: &str) -> bool {
    ['*', '_'].into_iter().any(|marker| {
        let trailing = previous
            .chars()
            .rev()
            .take_while(|character| *character == marker)
            .take(3)
            .count();
        let leading = next
            .chars()
            .take_while(|character| *character == marker)
            .take(3)
            .count();
        trailing > 0 && leading > 0 && trailing + leading >= 4
    })
}

#[derive(Clone, Debug)]
pub(super) struct AssistantBlock {
    pub(super) text: String,
    pub(super) markdown: StreamingMarkdown,
    pub(super) layout: RefCell<StreamingRenderCache>,
    /// Model that generated this block, for stable accent colour across
    /// model switches mid-session.
    pub(super) model_lab: Option<crate::tui::theme::ModelLab>,
    pub(super) finished: bool,
    /// Reasoning is retained verbatim but stays out of the mutable native
    /// scrollback tail until the user explicitly asks to inspect it.
    pub(super) reasoning_expanded: bool,
    /// First streamed reasoning delta, used to freeze elapsed timing when the
    /// block closes.
    pub(super) reasoning_started_at: Option<Instant>,
    /// Frozen reasoning duration after the block closes.
    pub(super) reasoning_elapsed: Option<Duration>,
    /// Start of the owning root run. Unlike reasoning timing, this survives
    /// steering, provider turns, and status-row replacement.
    pub(super) activity_started_at: Option<Instant>,
    /// Latest explicit ATX or standalone-bold heading emitted by the model.
    pub(super) reasoning_heading: Option<String>,
    /// Committed semantic blocks already inspected for reasoning headings.
    pub(super) reasoning_heading_committed_blocks: usize,
    /// Only the newest reasoning block advertises the global disclosure key.
    /// Older repeated hints become noise once a newer thinking event exists.
    pub(super) show_reasoning_hint: bool,
}

impl AssistantBlock {
    pub(super) fn streaming(text: &str) -> Self {
        let mut markdown = StreamingMarkdown::new();
        markdown.push_str(text);
        Self {
            text: text.to_owned(),
            markdown,
            layout: RefCell::new(StreamingRenderCache::default()),
            model_lab: None,
            finished: false,
            reasoning_expanded: false,
            reasoning_started_at: None,
            reasoning_elapsed: None,
            activity_started_at: None,
            reasoning_heading: None,
            reasoning_heading_committed_blocks: 0,
            show_reasoning_hint: true,
        }
    }

    pub(super) fn finalized(text: String) -> Self {
        let mut block = Self::streaming(&text);
        block.finish();
        block.text = text;
        block
    }

    pub(super) fn streaming_reasoning(text: &str) -> Self {
        let projection = reasoning_markdown_projection(text);
        let mut block = Self::streaming(&projection);
        block.text = text.to_owned();
        block.reasoning_started_at = Some(Instant::now());
        block.refresh_reasoning_heading();
        block
    }

    pub(super) fn finalized_reasoning(text: String) -> Self {
        let mut block = Self::streaming_reasoning(&text);
        // Hydrated sessions preserve reasoning text but do not currently store
        // provider-phase timing, so do not invent a duration on replay.
        block.reasoning_started_at = None;
        block.finish_reasoning();
        block
    }

    pub(super) fn with_model_lab(mut self, lab: Option<crate::tui::theme::ModelLab>) -> Self {
        self.model_lab = lab;
        self
    }

    pub(super) fn with_activity_started_at(mut self, started_at: Option<Instant>) -> Self {
        self.activity_started_at = started_at;
        self
    }

    pub(super) fn is_working_activity(&self) -> bool {
        !self.finished
            && self.text.is_empty()
            && !self.show_reasoning_hint
            && self.reasoning_heading.as_deref() == Some("Working")
    }

    pub(super) fn append(&mut self, text: &str) {
        self.text.push_str(text);
        self.markdown.push_str(text);
    }

    pub(super) fn append_reasoning(&mut self, text: &str) {
        let repairs_boundary = reasoning_delimiter_crosses_chunk_boundary(&self.text, text);
        self.text.push_str(text);
        if repairs_boundary {
            // This is rare (normally one boundary per provider summary
            // heading), so repair the cross-delta delimiter only when needed.
            self.markdown =
                StreamingMarkdown::from_text(&reasoning_markdown_projection(&self.text));
            self.reasoning_heading_committed_blocks = 0;
            self.invalidate_layout();
        } else {
            // Preserve the parser's committed prefix for ordinary token deltas.
            // Rebuilding here made verbose reasoning quadratic. Most deltas do
            // not contain the provider-specific adjacency at all, so avoid an
            // allocation on that hot path too.
            if text.contains("****") || text.contains("____") {
                self.markdown.push_str(&reasoning_markdown_projection(text));
            } else {
                self.markdown.push_str(text);
            }
        }
        self.refresh_reasoning_heading();
    }

    fn refresh_reasoning_heading(&mut self) {
        let (committed_blocks, heading) = {
            let committed = &self.markdown.committed().blocks;
            let start = self.reasoning_heading_committed_blocks.min(committed.len());
            let mut heading = committed[start..]
                .iter()
                .filter_map(reasoning_heading_from_block)
                .next_back();
            if let Some(preview_heading) = self
                .markdown
                .preview()
                .blocks
                .iter()
                .filter_map(reasoning_heading_from_block)
                .next_back()
            {
                heading = Some(preview_heading);
            }
            (committed.len(), heading)
        };
        self.reasoning_heading_committed_blocks = committed_blocks;
        if let Some(heading) = heading {
            self.reasoning_heading = Some(heading);
        }
    }

    pub(super) fn finish_reasoning(&mut self) {
        // A four-character emphasis boundary can straddle provider deltas. Fix
        // that rare boundary once at completion rather than reparsing the full
        // trace after every delta.
        let projection = reasoning_markdown_projection(&self.text);
        if self.markdown.raw_text() != projection {
            self.markdown = StreamingMarkdown::from_text(&projection);
            self.reasoning_heading_committed_blocks = 0;
            self.invalidate_layout();
        }
        if self.reasoning_elapsed.is_none() {
            self.reasoning_elapsed = self.reasoning_started_at.map(|started| started.elapsed());
        }
        self.finish();
        self.refresh_reasoning_heading();
    }

    pub(super) fn finish(&mut self) {
        self.markdown.finish();
        self.finished = true;
    }

    pub(super) fn invalidate_layout(&self) {
        *self.layout.borrow_mut() = StreamingRenderCache::default();
    }

    #[cfg(test)]
    pub(super) fn render(
        &self,
        renderer: &RichRenderer,
        theme: &YggTheme,
        width: u16,
    ) -> Vec<String> {
        self.render_on_surface(renderer, theme, width, None)
    }

    pub(super) fn render_update(
        &self,
        renderer: &RichRenderer,
        theme: &YggTheme,
        width: u16,
    ) -> Option<StreamingLineUpdate> {
        if self.finished || looks_like_diff(&self.text) {
            return None;
        }
        let use_plain = theme.capabilities().color == crate::tui::terminal::ColorDepth::None;
        Some(self.layout.borrow_mut().render_line_update(
            &self.markdown,
            renderer,
            width,
            !use_plain,
        ))
    }

    pub(super) fn render_on_surface(
        &self,
        renderer: &RichRenderer,
        theme: &YggTheme,
        width: u16,
        background: Option<Color>,
    ) -> Vec<String> {
        // Blocks are rendered at the caller's exact content width. Every
        // transcript block shares the same outer baseline; semantic styling
        // supplies hierarchy without changing horizontal geometry.
        let use_plain = theme.capabilities().color == crate::tui::terminal::ColorDepth::None;
        if looks_like_diff(&self.text) {
            return renderer
                .render_diff(
                    &UnifiedDiff::parse(&self.text),
                    width,
                    DiffRenderOptions {
                        line_numbers: width >= 70,
                        wrap: true,
                    },
                )
                .lines
                .into_iter()
                .map(|line| if use_plain { line.plain } else { line.styled })
                .collect();
        }
        if self.finished && background.is_some_and(|background| background != Color::Default) {
            return renderer
                .render_on_background(
                    &parse_markdown(self.markdown.raw_text()),
                    width,
                    background.expect("checked above"),
                )
                .lines
                .into_iter()
                .map(|line| if use_plain { line.plain } else { line.styled })
                .collect();
        }
        self.layout
            .borrow_mut()
            .render_lines(&self.markdown, renderer, width, !use_plain)
    }
}
