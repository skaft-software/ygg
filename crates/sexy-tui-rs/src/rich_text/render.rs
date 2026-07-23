//! Width-aware terminal and plain-text rendering for semantic documents.

use std::cell::RefCell;
#[cfg(feature = "syntax-highlighting")]
use std::collections::{hash_map::DefaultHasher, HashMap, VecDeque};
#[cfg(feature = "syntax-highlighting")]
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;

use crate::capabilities::{SupportLevel, TerminalCapabilities};
use crate::glyphs::GlyphSet;
use crate::sanitize::{sanitize_text, ControlPictures, SafeUrl, SanitizeOptions};
use crate::style::{BlockRole, Color, TextRole, TextStyle};
use crate::theme::Theme;
use crate::width::WidthPolicy;

use super::diff::{DiffLineKind, DiffRenderOptions, UnifiedDiff};
use super::{
    Block, CodeBlock, DetailBlock, Document, Inline, List, ListItem, ListKind, StatusKind, Table,
    TableAlignment,
};

/// Long-line policy for code blocks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CodeOverflow {
    /// Preserve source rows and clip visually. The semantic copy text remains
    /// complete.
    #[default]
    Clip,
    /// Wrap at grapheme boundaries with a hanging code indent.
    Wrap,
}

/// Rich-rendering options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderOptions {
    pub width: WidthPolicy,
    pub code_overflow: CodeOverflow,
    pub code_borders: bool,
    pub syntax_highlighting: bool,
    pub tables: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: WidthPolicy::default(),
            code_overflow: CodeOverflow::Clip,
            code_borders: true,
            syntax_highlighting: cfg!(feature = "syntax-highlighting"),
            tables: true,
        }
    }
}

/// One rendered terminal row with a copyable escape-free equivalent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedLine {
    pub styled: String,
    pub plain: String,
}

/// Render output plus the original semantic copy text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RenderedDocument {
    pub lines: Vec<RenderedLine>,
    pub copy_text: String,
}

impl RenderedDocument {
    pub fn styled_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.styled.clone()).collect()
    }

    pub fn plain_lines(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.plain.clone()).collect()
    }

    pub fn styled_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.styled.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| line.plain.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Observable cache counters used by benchmarks and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyntaxCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub bytes: usize,
}

/// Stateless layout plus a small renderer-local syntax cache. There is no
/// global render lock; a renderer is intended to live with one UI surface.
pub struct RichRenderer {
    theme: Theme,
    capabilities: TerminalCapabilities,
    options: RenderOptions,
    syntax_cache: RefCell<SyntaxCache>,
}

impl RichRenderer {
    pub fn new(
        mut theme: Theme,
        capabilities: TerminalCapabilities,
        mut options: RenderOptions,
    ) -> Self {
        theme.set_capabilities(capabilities);
        if capabilities.plain {
            options.syntax_highlighting = false;
        }
        Self {
            theme,
            capabilities,
            options,
            syntax_cache: RefCell::new(SyntaxCache::default()),
        }
    }

    pub fn plain() -> Self {
        let capabilities = TerminalCapabilities::plain();
        Self::new(
            Theme::with_capabilities(capabilities),
            capabilities,
            RenderOptions::default(),
        )
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn theme_mut(&mut self) -> &mut Theme {
        &mut self.theme
    }

    pub const fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }

    pub const fn options(&self) -> RenderOptions {
        self.options
    }

    pub fn set_options(&mut self, mut options: RenderOptions) {
        if self.capabilities.plain {
            options.syntax_highlighting = false;
        }
        self.options = options;
    }

    pub fn syntax_cache_stats(&self) -> SyntaxCacheStats {
        self.syntax_cache.borrow().stats()
    }

    pub fn render(&self, document: &Document, width: u16) -> RenderedDocument {
        self.render_document(document, width, self.options.syntax_highlighting)
    }

    /// Render a document on a fixed row surface. The background is composed
    /// into every semantic run before ANSI encoding, so syntax foregrounds and
    /// inline resets cannot punch holes in the surface. Rows are padded to the
    /// requested width.
    pub fn render_on_background(
        &self,
        document: &Document,
        width: u16,
        background: Color,
    ) -> RenderedDocument {
        let width = usize::from(width);
        let mut rich_lines = self.render_blocks(
            &document.blocks,
            width,
            false,
            self.options.syntax_highlighting,
        );
        for line in &mut rich_lines {
            for run in &mut line.runs {
                run.style.background = background;
            }
            let current_width = self.line_width(line);
            if current_width < width {
                line.push(
                    " ".repeat(width - current_width),
                    TextStyle::plain().background(background),
                    None,
                );
            }
        }
        RenderedDocument {
            lines: rich_lines
                .into_iter()
                .map(|line| self.encode_line(line, width))
                .collect(),
            copy_text: self.sanitize(&document.plain_text()),
        }
    }

    /// Render an unstable streaming suffix without syntax work that would be
    /// immediately invalidated by the next token.
    pub fn render_unstable(&self, document: &Document, width: u16) -> RenderedDocument {
        self.render_document(document, width, false)
    }

    fn render_document(
        &self,
        document: &Document,
        width: u16,
        syntax_highlighting: bool,
    ) -> RenderedDocument {
        let width = usize::from(width);
        let rich_lines = self.render_blocks(&document.blocks, width, false, syntax_highlighting);
        RenderedDocument {
            lines: rich_lines
                .into_iter()
                .map(|line| self.encode_line(line, width))
                .collect(),
            copy_text: self.sanitize(&document.plain_text()),
        }
    }

    /// Render a block slice. Streaming caches use this to append newly
    /// committed blocks without cloning the complete document.
    pub fn render_blocks_only(&self, blocks: &[Block], width: u16) -> Vec<RenderedLine> {
        let width = usize::from(width);
        self.render_blocks(blocks, width, false, self.options.syntax_highlighting)
            .into_iter()
            .map(|line| self.encode_line(line, width))
            .collect()
    }

    pub fn render_diff(
        &self,
        diff: &UnifiedDiff,
        width: u16,
        options: DiffRenderOptions,
    ) -> RenderedDocument {
        let width = usize::from(width);
        let mut lines = Vec::new();
        let number_width = if options.line_numbers {
            diff.lines
                .iter()
                .flat_map(|line| [line.old_number, line.new_number])
                .flatten()
                .max()
                .map_or(1, |number| number.to_string().len())
        } else {
            0
        };
        let mut language: Option<String> = None;
        for line in &diff.lines {
            if line.kind == DiffLineKind::FileHeader {
                if let Some(hint) = diff_language_hint(&line.text) {
                    language = Some(hint);
                }
            }
            let role = match line.kind {
                DiffLineKind::Addition => TextRole::DiffAdd,
                DiffLineKind::Removal => TextRole::DiffRemove,
                DiffLineKind::Context | DiffLineKind::Metadata | DiffLineKind::Binary => {
                    TextRole::DiffContext
                }
                DiffLineKind::HunkHeader => TextRole::DiffHunk,
                DiffLineKind::FileHeader => TextRole::DiffHeader,
            };
            let style = self.theme.style(role);
            let mut gutter_style = self.theme.style(TextRole::Subtle);
            gutter_style.background = style.background;
            let mut prefix = RichLine::default();
            if options.line_numbers {
                // A unified diff already marks additions/removals in the text
                // column. One location gutter is enough: prefer the resulting
                // (new) line number, falling back to the old number for a
                // deletion-only row.
                let number = line.new_number.or(line.old_number).map_or_else(
                    || " ".repeat(number_width),
                    |number| format!("{number:>number_width$}"),
                );
                prefix.push(format!("{number} | "), gutter_style, None);
            }
            let text = self.sanitize(&line.text);
            let content = RichLine {
                runs: self
                    .diff_code_runs(&text, line.kind, language.as_deref(), style)
                    .unwrap_or_else(|| vec![RichRun::new(text, style, None)]),
            };
            let prefix_width = self.line_width(&prefix);
            let available = width.saturating_sub(prefix_width);
            let rows = if options.wrap {
                self.wrap_runs(&content.runs, available)
            } else {
                vec![self.clip_runs(&content.runs, available)]
            };
            for (index, row) in rows.into_iter().enumerate() {
                let mut rendered = if index == 0 {
                    prefix.clone()
                } else {
                    let mut continuation = RichLine::default();
                    continuation.push(" ".repeat(prefix_width), gutter_style, None);
                    continuation
                };
                rendered.extend(row);
                // Pad to full width so diff backgrounds span the line.
                let current_width = self.line_width(&rendered);
                if current_width < width {
                    let pad_style = rendered
                        .runs
                        .last()
                        .map(|run| run.style)
                        .unwrap_or(self.theme.style(TextRole::DiffContext));
                    rendered.push(" ".repeat(width - current_width), pad_style, None);
                }
                lines.push(self.encode_line(rendered, width));
            }
        }
        RenderedDocument {
            lines,
            copy_text: self.sanitize(&diff.plain_text()),
        }
    }

    fn render_blocks(
        &self,
        blocks: &[Block],
        width: usize,
        compact: bool,
        syntax_highlighting: bool,
    ) -> Vec<RichLine> {
        let mut output = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let mut rendered = self.render_block(block, width, syntax_highlighting);
            output.append(&mut rendered);
            if !compact && index + 1 < blocks.len() {
                push_blank(&mut output);
            }
        }
        while output.last().is_some_and(RichLine::is_empty) {
            output.pop();
        }
        if output.is_empty() && !blocks.is_empty() {
            output.push(RichLine::default());
        }
        output
    }

    fn render_block(
        &self,
        block: &Block,
        width: usize,
        syntax_highlighting: bool,
    ) -> Vec<RichLine> {
        match block {
            Block::Paragraph(content) => {
                let runs = self.inline_runs(content, self.theme.style(TextRole::Text));
                self.wrap_runs(&runs, width)
            }
            Block::Heading { level, content } => {
                let mut base = self.theme.style(TextRole::Heading);
                if *level <= 3 {
                    base.attributes.bold = true;
                }
                let runs = self.inline_runs(content, base);
                self.wrap_runs(&runs, width)
            }
            Block::CodeBlock(code)
                if code.language.as_deref().is_some_and(|lang| {
                    lang.eq_ignore_ascii_case("diff") || lang.eq_ignore_ascii_case("patch")
                }) =>
            {
                // Fenced diff blocks carry the same semantics as bare unified
                // diffs. Render them through the dedicated diff pipeline so
                // they get full-width backgrounds, color, and line numbers.
                self.render_diff_as_rich_lines(&code.code, width)
            }
            Block::CodeBlock(code) => self.render_code(code, width, syntax_highlighting),
            Block::List(list) => self.render_list(list, width, syntax_highlighting),
            Block::BlockQuote(blocks) => self.render_quote(blocks, width, syntax_highlighting),
            Block::Divider => {
                if width == 0 {
                    vec![RichLine::default()]
                } else {
                    let glyphs = GlyphSet::for_capabilities(self.capabilities);
                    let count =
                        width.min(60) / self.options.width.line_width(glyphs.horizontal).max(1);
                    let mut border = self.theme.style(TextRole::Border);
                    if let Some(color) = self.theme.resolve_color("md_hr") {
                        border.foreground = color;
                    }
                    let mut line = RichLine::default();
                    line.push(glyphs.horizontal.repeat(count), border, None);
                    vec![line]
                }
            }
            Block::Table(table) if self.options.tables => self.render_table(table, width),
            Block::Table(table) => self.render_table_fallback(table, width),
            Block::Detail(detail) => self.render_detail(detail, width, syntax_highlighting),
            Block::Plain(text) => {
                let safe = self.sanitize(text);
                safe.split('\n')
                    .flat_map(|line| {
                        let run =
                            RichRun::new(line.to_owned(), self.theme.style(TextRole::Text), None);
                        self.wrap_runs(&[run], width)
                    })
                    .collect()
            }
        }
    }

    fn render_list(&self, list: &List, width: usize, syntax_highlighting: bool) -> Vec<RichLine> {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let mut output = Vec::new();
        for (index, item) in list.items.iter().enumerate() {
            if index > 0 && (item.blocks.len() > 1 || list.items[index - 1].blocks.len() > 1) {
                push_blank(&mut output);
            }
            let marker = match list.kind {
                ListKind::Unordered => format!("{} ", glyphs.bullet),
                ListKind::Ordered { start } => format!("{}. ", start.saturating_add(index as u64)),
            };
            let marker = match item.task {
                Some(true) => format!("{marker}[x] "),
                Some(false) => format!("{marker}[ ] "),
                None => marker,
            };
            self.render_list_item(item, &marker, width, syntax_highlighting, &mut output);
        }
        output
    }

    fn render_list_item(
        &self,
        item: &ListItem,
        marker: &str,
        width: usize,
        syntax_highlighting: bool,
        output: &mut Vec<RichLine>,
    ) {
        let marker_style = self.theme.style(TextRole::ListMarker);
        let marker_width = self.options.width.line_width(marker);
        let content_width = width.saturating_sub(marker_width);
        let mut blocks = item.blocks.as_slice();
        if let Some(Block::Paragraph(content)) = blocks.first() {
            let runs = self.inline_runs(content, self.theme.style(TextRole::Text));
            let rows = self.wrap_runs(&runs, content_width);
            if rows.is_empty() {
                let mut line = RichLine::default();
                line.push(marker.to_owned(), marker_style, None);
                output.push(line);
            } else {
                for (index, row) in rows.into_iter().enumerate() {
                    let mut line = RichLine::default();
                    if index == 0 {
                        line.push(marker.to_owned(), marker_style, None);
                    } else {
                        line.push(" ".repeat(marker_width), TextStyle::plain(), None);
                    }
                    line.extend(row);
                    output.push(line);
                }
            }
            blocks = &blocks[1..];
        } else {
            let mut line = RichLine::default();
            line.push(marker.to_owned(), marker_style, None);
            output.push(line);
        }

        for block in blocks {
            let nested_width = width.saturating_sub(marker_width);
            for row in self.render_block(block, nested_width, syntax_highlighting) {
                let mut line = RichLine::from_plain(" ".repeat(marker_width));
                line.extend(row);
                output.push(line);
            }
        }
    }

    fn render_quote(
        &self,
        blocks: &[Block],
        width: usize,
        syntax_highlighting: bool,
    ) -> Vec<RichLine> {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let prefix = format!("{} ", glyphs.vertical);
        let prefix_width = self.options.width.line_width(&prefix);
        let inner = self.render_blocks(
            blocks,
            width.saturating_sub(prefix_width),
            false,
            syntax_highlighting,
        );
        let block_style = self.theme.block_style(BlockRole::Quote);
        let mut output = Vec::new();
        for mut row in inner {
            for run in &mut row.runs {
                run.style = block_style.text.merge(run.style);
                if let Some(background) = block_style.background {
                    run.style.background = background;
                }
            }
            let mut line = RichLine::default();
            line.push(prefix.clone(), block_style.border, None);
            line.extend(row);
            output.push(line);
        }
        if output.is_empty() {
            let mut line = RichLine::default();
            line.push(prefix, block_style.border, None);
            output.push(line);
        }
        output
    }

    fn render_code(
        &self,
        code: &CodeBlock,
        width: usize,
        syntax_highlighting: bool,
    ) -> Vec<RichLine> {
        let style = self.theme.block_style(BlockRole::Code);
        let mut code_text_style = style.text;
        let mut padding_style = TextStyle::plain();
        if let Some(background) = style.background {
            code_text_style.background = background;
            padding_style.background = background;
        }

        // CommonMark includes the line ending immediately before a closing
        // fence in the code payload. It terminates the final source row; it is
        // not an additional blank row. Removing exactly one line ending keeps
        // intentional blank lines intact while avoiding a phantom row.
        let display_source = code
            .code
            .strip_suffix('\n')
            .map(|source| source.strip_suffix('\r').unwrap_or(source))
            .unwrap_or(&code.code);
        let source_lines = if display_source.is_empty() {
            vec![""]
        } else {
            display_source.split('\n').collect::<Vec<_>>()
        };
        // Generic prose-fence tags add no information and look like a stray
        // badge. Preserve meaningful language labels and the original code
        // metadata used for highlighting/copy behavior.
        let language = code.language.as_deref().map(str::trim).filter(|language| {
            !language.is_empty()
                && !language.eq_ignore_ascii_case("text")
                && !language.eq_ignore_ascii_case("plaintext")
        });

        // A code surface should fit its content rather than painting the full
        // terminal width for a short snippet. Long rows still use all available
        // space and follow the configured clip/wrap policy.
        let natural_content_width = source_lines
            .iter()
            .map(|line| self.options.width.line_width(&self.sanitize(line)))
            .max()
            .unwrap_or(0);
        let bordered = self.options.code_borders && width >= 3;
        let frame_width = usize::from(bordered) * 2;
        let configured_left = usize::from(style.padding_left);
        let configured_right = usize::from(style.padding_right);
        let language_width = language
            .map(|label| self.options.width.line_width(&self.sanitize(label)))
            .unwrap_or(0);
        let required_for_content = natural_content_width
            .saturating_add(configured_left)
            .saturating_add(configured_right)
            .saturating_add(frame_width);
        let required_for_label = if bordered {
            language_width.saturating_add(5)
        } else {
            language_width.saturating_add(configured_left)
        };
        let minimum_width = if bordered { 3 } else { 1 };
        let block_width = required_for_content
            .max(required_for_label)
            .max(minimum_width)
            .min(width);
        let inner_width = block_width.saturating_sub(frame_width);
        let left_padding = configured_left.min(inner_width.saturating_sub(1));
        let right_padding =
            configured_right.min(inner_width.saturating_sub(left_padding).saturating_sub(1));
        let content_width = inner_width
            .saturating_sub(left_padding)
            .saturating_sub(right_padding);

        let mut output = Vec::new();
        if bordered {
            output.push(self.code_border_line(block_width, language, true, style.border));
        } else if let Some(language) = language {
            let mut label = RichLine::default();
            label.push(" ".repeat(left_padding), TextStyle::plain(), None);
            label.push(
                self.sanitize(language),
                self.theme.style(TextRole::Muted),
                None,
            );
            output.push(self.clip_line(label, block_width));
        }

        for _ in 0..style.padding_top {
            output.push(self.code_content_line(
                RichLine::default(),
                block_width,
                content_width,
                left_padding,
                right_padding,
                bordered,
                padding_style,
                style.border,
                style.background.is_some(),
            ));
        }

        let highlighted = syntax_highlighting
            .then(|| self.highlighted(code))
            .flatten();
        for (line_index, source) in source_lines.iter().enumerate() {
            let mut runs = if let Some(lines) = highlighted.as_deref() {
                lines
                    .get(line_index)
                    .map(|regions| {
                        regions
                            .iter()
                            .map(|region| {
                                RichRun::new(
                                    self.sanitize(&region.text),
                                    region.role.map_or(code_text_style, |role| {
                                        code_text_style.merge(self.theme.style(role))
                                    }),
                                    None,
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            } else {
                vec![RichRun::new(self.sanitize(source), code_text_style, None)]
            };
            if runs.is_empty() {
                runs.push(RichRun::new(String::new(), code_text_style, None));
            }
            let rows = match self.options.code_overflow {
                CodeOverflow::Clip => vec![self.clip_runs(&runs, content_width)],
                CodeOverflow::Wrap => self.hard_wrap_runs(&runs, content_width),
            };
            for row in rows {
                output.push(self.code_content_line(
                    row,
                    block_width,
                    content_width,
                    left_padding,
                    right_padding,
                    bordered,
                    padding_style,
                    style.border,
                    style.background.is_some(),
                ));
            }
        }

        for _ in 0..style.padding_bottom {
            output.push(self.code_content_line(
                RichLine::default(),
                block_width,
                content_width,
                left_padding,
                right_padding,
                bordered,
                padding_style,
                style.border,
                style.background.is_some(),
            ));
        }
        if bordered {
            output.push(self.code_border_line(block_width, None, false, style.border));
        }
        output
    }

    fn code_border_line(
        &self,
        width: usize,
        label: Option<&str>,
        top: bool,
        border_style: TextStyle,
    ) -> RichLine {
        if width == 0 {
            return RichLine::default();
        }
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        if width < 3 {
            let mut line = RichLine::default();
            line.push(glyphs.horizontal.repeat(width), border_style, None);
            return line;
        }

        let mut line = RichLine::default();
        line.push(
            if top {
                glyphs.top_left
            } else {
                glyphs.bottom_left
            }
            .to_owned(),
            border_style,
            None,
        );
        let inner_width = width - 2;
        let safe_label = label.map(|label| self.sanitize(label));
        if top && safe_label.as_deref().is_some_and(|label| !label.is_empty()) && inner_width >= 4 {
            let label_width = inner_width.saturating_sub(3);
            let mut label_style = self.theme.style(TextRole::Muted);
            label_style.attributes.italic = true;
            let label = self.clip_runs(
                &[RichRun::new(
                    safe_label.unwrap_or_default(),
                    label_style,
                    None,
                )],
                label_width,
            );
            let rendered_label_width = self.line_width(&label);
            line.push(glyphs.horizontal.to_owned(), border_style, None);
            line.push(" ".to_owned(), border_style, None);
            line.extend(label);
            line.push(" ".to_owned(), border_style, None);
            line.push(
                glyphs
                    .horizontal
                    .repeat(inner_width.saturating_sub(rendered_label_width + 3)),
                border_style,
                None,
            );
        } else {
            line.push(glyphs.horizontal.repeat(inner_width), border_style, None);
        }
        line.push(
            if top {
                glyphs.top_right
            } else {
                glyphs.bottom_right
            }
            .to_owned(),
            border_style,
            None,
        );
        line
    }

    #[allow(clippy::too_many_arguments)]
    fn code_content_line(
        &self,
        row: RichLine,
        block_width: usize,
        content_width: usize,
        left_padding: usize,
        right_padding: usize,
        bordered: bool,
        padding_style: TextStyle,
        border_style: TextStyle,
        has_background: bool,
    ) -> RichLine {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let mut line = RichLine::default();
        if bordered {
            line.push(glyphs.vertical.to_owned(), border_style, None);
        }
        line.push(" ".repeat(left_padding), padding_style, None);
        let row_width = self.line_width(&row).min(content_width);
        line.extend(row);
        if bordered || has_background {
            line.push(
                " ".repeat(content_width.saturating_sub(row_width) + right_padding),
                padding_style,
                None,
            );
        }
        if bordered {
            line.push(glyphs.vertical.to_owned(), border_style, None);
        }
        self.clip_line(line, block_width)
    }

    fn render_detail(
        &self,
        detail: &DetailBlock,
        width: usize,
        syntax_highlighting: bool,
    ) -> Vec<RichLine> {
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let marker = format!(
            "{} ",
            if detail.expanded {
                glyphs.detail_expanded
            } else {
                glyphs.detail_collapsed
            }
        );
        let marker_width = self.options.width.line_width(&marker);
        let block_style = self.theme.block_style(BlockRole::Detail);
        let runs = self.inline_runs(
            &detail.summary,
            block_style.text.merge(self.theme.style(TextRole::Strong)),
        );
        let mut output = Vec::new();
        for (index, row) in self
            .wrap_runs(&runs, width.saturating_sub(marker_width))
            .into_iter()
            .enumerate()
        {
            let mut line = RichLine::default();
            if index == 0 {
                line.push(marker.clone(), block_style.border, None);
            } else {
                line.push(" ".repeat(marker_width), TextStyle::plain(), None);
            }
            line.extend(row);
            output.push(line);
        }
        if detail.expanded {
            for row in self.render_blocks(
                &detail.blocks,
                width.saturating_sub(marker_width),
                false,
                syntax_highlighting,
            ) {
                let mut line = RichLine::from_plain(" ".repeat(marker_width));
                line.extend(row);
                output.push(line);
            }
        }
        output
    }

    fn render_table(&self, table: &Table, width: usize) -> Vec<RichLine> {
        let columns = table
            .header
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return Vec::new();
        }
        let overhead = columns.saturating_mul(3).saturating_add(1);
        // One-cell columns are technically drawable but not readable. Prefer
        // the labeled-list fallback until each column can hold a short token.
        if width < overhead.saturating_add(columns.saturating_mul(3)) {
            return self.render_table_fallback(table, width);
        }

        let mut natural = vec![1usize; columns];
        for row in std::iter::once(&table.header).chain(table.rows.iter()) {
            for (column, cell) in row.iter().enumerate() {
                let runs = self.inline_runs(cell, self.theme.style(TextRole::Text));
                natural[column] = natural[column].max(self.runs_width(&runs).min(40));
            }
        }
        let interior = width.saturating_sub(overhead);
        let mut widths = vec![1usize; columns];
        let mut remaining = interior.saturating_sub(columns);
        while remaining > 0 {
            let mut progressed = false;
            for column in 0..columns {
                if remaining == 0 {
                    break;
                }
                if widths[column] < natural[column] {
                    widths[column] += 1;
                    remaining -= 1;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        let unicode = self.capabilities.unicode && !self.capabilities.plain;
        let (
            vertical,
            left,
            middle,
            right,
            top_left,
            top_mid,
            top_right,
            bottom_left,
            bottom_mid,
            bottom_right,
        ) = if unicode {
            ("│", "├", "┼", "┤", "┌", "┬", "┐", "└", "┴", "┘")
        } else {
            ("|", "+", "+", "+", "+", "+", "+", "+", "+", "+")
        };
        let horizontal = if unicode { "─" } else { "-" };
        let border_style = self.theme.block_style(BlockRole::Table).border;
        let border = |left: &str, middle: &str, right: &str| {
            let text = format!(
                "{}{}{}",
                left,
                widths
                    .iter()
                    .map(|width| horizontal.repeat(width + 2))
                    .collect::<Vec<_>>()
                    .join(middle),
                right
            );
            let mut line = RichLine::default();
            line.push(text, border_style, None);
            line
        };

        let mut output = vec![border(top_left, top_mid, top_right)];
        output.extend(self.render_table_row(
            &table.header,
            &widths,
            &table.alignments,
            vertical,
            true,
        ));
        output.push(border(left, middle, right));
        for (index, row) in table.rows.iter().enumerate() {
            output.extend(self.render_table_row(row, &widths, &table.alignments, vertical, false));
            if index + 1 < table.rows.len() {
                output.push(border(left, middle, right));
            }
        }
        output.push(border(bottom_left, bottom_mid, bottom_right));
        output
            .into_iter()
            .map(|line| self.clip_line(line, width))
            .collect()
    }

    fn render_table_row(
        &self,
        row: &[Vec<Inline>],
        widths: &[usize],
        alignments: &[TableAlignment],
        vertical: &str,
        header: bool,
    ) -> Vec<RichLine> {
        let mut cells = Vec::new();
        let mut height = 1usize;
        for (column, width) in widths.iter().copied().enumerate() {
            let table_style = self.theme.block_style(BlockRole::Table);
            let base = if header {
                table_style.text.merge(self.theme.style(TextRole::Strong))
            } else {
                table_style.text
            };
            let runs = row
                .get(column)
                .map_or_else(Vec::new, |cell| self.inline_runs(cell, base));
            let wrapped = self.wrap_runs(&runs, width);
            height = height.max(wrapped.len());
            cells.push(wrapped);
        }

        let mut output = Vec::new();
        for line_index in 0..height {
            let mut line = RichLine::default();
            line.push(
                vertical.to_owned(),
                self.theme.block_style(BlockRole::Table).border,
                None,
            );
            for (column, width) in widths.iter().copied().enumerate() {
                line.push(" ".into(), TextStyle::plain(), None);
                let cell = cells[column].get(line_index).cloned().unwrap_or_default();
                let cell_width = self.line_width(&cell);
                let padding = width.saturating_sub(cell_width);
                let alignment = alignments.get(column).copied().unwrap_or_default();
                let left = match alignment {
                    TableAlignment::Right => padding,
                    TableAlignment::Center => padding / 2,
                    TableAlignment::None | TableAlignment::Left => 0,
                };
                line.push(" ".repeat(left), TextStyle::plain(), None);
                line.extend(cell);
                line.push(" ".repeat(padding - left), TextStyle::plain(), None);
                line.push(" ".into(), TextStyle::plain(), None);
                line.push(
                    vertical.to_owned(),
                    self.theme.block_style(BlockRole::Table).border,
                    None,
                );
            }
            output.push(line);
        }
        output
    }

    fn render_table_fallback(&self, table: &Table, width: usize) -> Vec<RichLine> {
        let mut output = Vec::new();
        for (row_index, row) in table.rows.iter().enumerate() {
            if row_index > 0 {
                push_blank(&mut output);
            }
            for (column, value) in row.iter().enumerate() {
                let label = table
                    .header
                    .get(column)
                    .cloned()
                    .unwrap_or_else(|| vec![Inline::Text(format!("Column {}", column + 1))]);
                let table_style = self.theme.block_style(BlockRole::Table);
                let mut runs = self.inline_runs(
                    &label,
                    table_style.text.merge(self.theme.style(TextRole::Strong)),
                );
                runs.push(RichRun::new(
                    ": ".into(),
                    self.theme.style(TextRole::Text),
                    None,
                ));
                runs.extend(self.inline_runs(value, table_style.text));
                output.extend(self.wrap_runs(&runs, width));
            }
        }
        if table.rows.is_empty() {
            for cell in &table.header {
                output.extend(
                    self.wrap_runs(
                        &self.inline_runs(
                            cell,
                            self.theme
                                .block_style(BlockRole::Table)
                                .text
                                .merge(self.theme.style(TextRole::Strong)),
                        ),
                        width,
                    ),
                );
            }
        }
        output
    }

    fn inline_runs(&self, content: &[Inline], base: TextStyle) -> Vec<RichRun> {
        let mut runs = Vec::new();
        self.flatten_inline(content, base, None, &mut runs);
        runs
    }

    fn flatten_inline(
        &self,
        content: &[Inline],
        base: TextStyle,
        link: Option<&str>,
        output: &mut Vec<RichRun>,
    ) {
        for inline in content {
            match inline {
                Inline::Text(text) | Inline::Raw(text) => {
                    push_run(output, self.sanitize(text), base, link.map(str::to_owned));
                }
                Inline::Styled(span) => {
                    self.flatten_inline(&span.content, base.merge(span.style), link, output)
                }
                Inline::Role { role, content } => {
                    self.flatten_inline(content, base.merge(self.theme.style(*role)), link, output)
                }
                Inline::Status { kind, content } => {
                    let glyphs = GlyphSet::for_capabilities(self.capabilities);
                    let (marker, role) = match kind {
                        StatusKind::Success => (glyphs.success, TextRole::Success),
                        StatusKind::Warning => (glyphs.warning, TextRole::Warning),
                        StatusKind::Error => (glyphs.error, TextRole::Error),
                        StatusKind::Pending => (glyphs.pending, TextRole::Muted),
                    };
                    let style = base.merge(self.theme.style(role));
                    push_run(output, format!("{marker} "), style, link.map(str::to_owned));
                    self.flatten_inline(content, style, link, output);
                }
                Inline::Emphasis(content) => {
                    let mut style = self.theme.style(TextRole::Emphasis);
                    // Unsupported italics should degrade to regular text, not
                    // underline: underline is reserved for explicit Markdown
                    // links/underline semantics.
                    if self.capabilities.italics != SupportLevel::Supported {
                        style.attributes.italic = false;
                    }
                    self.flatten_inline(content, base.merge(style), link, output);
                }
                Inline::Strong(content) => self.flatten_inline(
                    content,
                    base.merge(self.theme.style(TextRole::Strong)),
                    link,
                    output,
                ),
                Inline::Strikethrough(content) => {
                    let mut style = TextStyle::plain();
                    style.attributes.strikethrough = true;
                    self.flatten_inline(content, base.merge(style), link, output);
                }
                Inline::Code(code) => {
                    let mut style = base.merge(self.theme.style(TextRole::InlineCode));
                    // Inline code is a technical surface, not prose. Do not
                    // inherit an enclosing paragraph's faint/italic treatment
                    // (for example, from a subdued reasoning renderer).
                    style.attributes.dim = false;
                    style.attributes.italic = false;
                    // Backgrounds are a theme decision, never inferred from
                    // content. The terminal-neutral default is foreground-only.
                    if let Some(background) = self
                        .theme
                        .resolve_color("md_code_inline_bg")
                        .filter(|color| *color != Color::Default)
                    {
                        style.background = background;
                    }
                    push_run(output, self.sanitize(code), style, link.map(str::to_owned));
                }
                Inline::Link { label, target } => {
                    let before = output.len();
                    self.flatten_inline(
                        label,
                        base.merge(self.theme.style(TextRole::Link)),
                        Some(target),
                        output,
                    );
                    let label_text = output[before..]
                        .iter()
                        .map(|run| run.text.as_str())
                        .collect::<String>();
                    let safe_target = self.sanitize(target);
                    if label_text.trim() != safe_target.trim() {
                        push_run(
                            output,
                            format!(" ({safe_target})"),
                            base.merge(self.theme.style(TextRole::Link)),
                            Some(target.clone()),
                        );
                    }
                }
                Inline::SoftBreak => push_run(output, " ".into(), base, link.map(str::to_owned)),
                Inline::HardBreak => push_run(output, "\n".into(), base, link.map(str::to_owned)),
            }
        }
    }

    fn wrap_runs(&self, runs: &[RichRun], width: usize) -> Vec<RichLine> {
        if width == 0 {
            return vec![RichLine::default()];
        }
        let runs = self.expand_run_tabs(runs);
        let logical = split_runs_at_newlines(&runs);
        let mut output = Vec::new();
        for line_runs in logical {
            let units = units(&line_runs, self.options.width);
            if units.is_empty() {
                output.push(RichLine::default());
                continue;
            }
            let mut start = 0usize;
            while start < units.len() {
                let mut end = start;
                let mut cells = 0usize;
                let mut last_space = None;
                while end < units.len() {
                    let unit = &units[end];
                    if cells.saturating_add(unit.width) > width {
                        break;
                    }
                    cells += unit.width;
                    if unit.whitespace {
                        last_space = Some(end);
                    }
                    end += 1;
                }
                if end == start {
                    // A wide grapheme cannot fit at width one. A visible ASCII
                    // fallback keeps the line within the promised cell bound.
                    let mut replacement = RichLine::default();
                    let source = &line_runs[units[start].run];
                    replacement.push("?".into(), source.style, source.link.clone());
                    output.push(replacement);
                    start += 1;
                    continue;
                }

                let mut next = end;
                let line_end = if end < units.len() {
                    if let Some(space) = last_space.filter(|space| *space > start) {
                        next = space + 1;
                        while next < units.len() && units[next].whitespace {
                            next += 1;
                        }
                        space
                    } else {
                        end
                    }
                } else {
                    end
                };
                while next < units.len() && units[next].whitespace {
                    next += 1;
                }
                output.push(line_from_units(&line_runs, &units[start..line_end]));
                start = next;
            }
        }
        if output.is_empty() {
            output.push(RichLine::default());
        }
        output
    }

    /// Render a unified-diff string through the semantic diff pipeline and
    /// return styled RichLines. Fenced `diff` / `patch` code blocks in
    /// Markdown are promoted to this path so they get full-width backgrounds,
    /// color, and optional line numbers instead of bare monospace.
    fn render_diff_as_rich_lines(&self, source: &str, width: usize) -> Vec<RichLine> {
        let diff = UnifiedDiff::parse(source);
        let number_width = diff
            .lines
            .iter()
            .flat_map(|line| [line.old_number, line.new_number])
            .flatten()
            .max()
            .map_or(1, |number| number.to_string().len());
        let show_numbers = width >= 70;

        // Extract the language hint from file headers so code lines can be
        // syntax-highlighted.  Multiple files in one diff update the language
        // as each new header is encountered.
        let mut language: Option<String> = None;

        let mut output = Vec::new();
        for line in &diff.lines {
            // Track the file language from `+++ b/…` headers.
            if line.kind == DiffLineKind::FileHeader {
                if let Some(hint) = diff_language_hint(&line.text) {
                    language = Some(hint);
                }
            }

            let role = match line.kind {
                DiffLineKind::Addition => TextRole::DiffAdd,
                DiffLineKind::Removal => TextRole::DiffRemove,
                DiffLineKind::Context | DiffLineKind::Metadata | DiffLineKind::Binary => {
                    TextRole::DiffContext
                }
                DiffLineKind::HunkHeader => TextRole::DiffHunk,
                DiffLineKind::FileHeader => TextRole::DiffHeader,
            };
            let base_style = self.theme.style(role);
            let mut gutter_style = self.theme.style(TextRole::Subtle);
            gutter_style.background = base_style.background;

            let mut prefix = String::new();
            if show_numbers {
                let number = line.new_number.or(line.old_number).map_or_else(
                    || " ".repeat(number_width),
                    |number| format!("{number:>number_width$}"),
                );
                prefix = format!("{number} | ");
            }

            let prefix_width = self.options.width.line_width(&prefix);
            let available = width.saturating_sub(prefix_width);
            let text = self.sanitize(&line.text);
            let diff_style = base_style;

            // Build runs: try syntax highlighting for code lines when we
            // know the file language, otherwise render as plain text.
            // Strip the single-character diff prefix (+, -, space) from code
            // lines so the syntax highlighter receives valid source text.
            let runs = self
                .diff_code_runs(&text, line.kind, language.as_deref(), diff_style)
                .unwrap_or_else(|| vec![RichRun::new(text, diff_style, None)]);

            let rows = if self.options.code_overflow == CodeOverflow::Wrap {
                self.wrap_runs(&runs, available)
            } else {
                vec![self.clip_runs(&runs, available)]
            };

            for (row_index, row) in rows.into_iter().enumerate() {
                let mut rendered = RichLine::default();
                if row_index == 0 && !prefix.is_empty() {
                    rendered.push(prefix.clone(), gutter_style, None);
                } else if !prefix.is_empty() {
                    rendered.push(" ".repeat(prefix_width), gutter_style, None);
                }
                rendered.extend(row);

                // Pad to full width so backgrounds span the entire line.
                let current_width = self.line_width(&rendered);
                if current_width < width {
                    let pad_style = rendered
                        .runs
                        .last()
                        .map(|run| run.style)
                        .unwrap_or(diff_style);
                    rendered.push(" ".repeat(width - current_width), pad_style, None);
                }
                output.push(rendered);
            }
        }
        output
    }

    fn diff_code_runs(
        &self,
        text: &str,
        kind: DiffLineKind,
        language: Option<&str>,
        diff_style: TextStyle,
    ) -> Option<Vec<RichRun>> {
        if !matches!(
            kind,
            DiffLineKind::Context | DiffLineKind::Addition | DiffLineKind::Removal
        ) {
            return None;
        }
        let (marker, source) = match kind {
            DiffLineKind::Addition => ("+", text.strip_prefix('+').unwrap_or(text)),
            DiffLineKind::Removal => ("-", text.strip_prefix('-').unwrap_or(text)),
            DiffLineKind::Context if text.starts_with(' ') => {
                (" ", text.strip_prefix(' ').unwrap_or(text))
            }
            DiffLineKind::Context => ("", text),
            _ => return None,
        };

        let mut runs = vec![RichRun::new(
            marker.to_owned(),
            self.diff_marker_style(kind, diff_style),
            None,
        )];
        #[cfg(not(feature = "syntax-highlighting"))]
        let _ = language;

        #[cfg(feature = "syntax-highlighting")]
        if self.options.syntax_highlighting {
            if let Some(language) = language {
                if let Some(highlighted) = super::highlight::highlight(source, language) {
                    for region in highlighted.into_iter().flatten() {
                        let token_style = region
                            .role
                            .map_or(diff_style, |role| diff_style.merge(self.theme.style(role)));
                        runs.push(RichRun::new(region.text, token_style, None));
                    }
                    return Some(runs);
                }
            }
        }

        runs.push(RichRun::new(source.to_owned(), diff_style, None));
        Some(runs)
    }

    fn diff_marker_style(&self, kind: DiffLineKind, mut style: TextStyle) -> TextStyle {
        let token = match kind {
            DiffLineKind::Addition => "diff_added_marker",
            DiffLineKind::Removal => "diff_removed_marker",
            _ => return style,
        };
        if let Some(color) = self
            .theme
            .resolve_color(token)
            .filter(|color| *color != Color::Default)
        {
            style.foreground = color;
        }
        style
    }

    /// Code wrapping is deliberately literal: unlike prose wrapping it never
    /// discards indentation or separator whitespace to find a word boundary.
    fn hard_wrap_runs(&self, runs: &[RichRun], width: usize) -> Vec<RichLine> {
        if width == 0 {
            return vec![RichLine::default()];
        }
        let runs = self.expand_run_tabs(runs);
        let logical = split_runs_at_newlines(&runs);
        let mut output = Vec::new();
        for line_runs in logical {
            let units = units(&line_runs, self.options.width);
            if units.is_empty() {
                output.push(RichLine::default());
                continue;
            }
            let mut start = 0usize;
            while start < units.len() {
                let mut end = start;
                let mut cells = 0usize;
                while end < units.len() && cells.saturating_add(units[end].width) <= width {
                    cells = cells.saturating_add(units[end].width);
                    end += 1;
                }
                if end == start {
                    let source = &line_runs[units[start].run];
                    let mut replacement = RichLine::default();
                    replacement.push("?".into(), source.style, source.link.clone());
                    output.push(replacement);
                    start += 1;
                } else {
                    output.push(line_from_units(&line_runs, &units[start..end]));
                    start = end;
                }
            }
        }
        if output.is_empty() {
            output.push(RichLine::default());
        }
        output
    }

    fn clip_runs(&self, runs: &[RichRun], width: usize) -> RichLine {
        if width == 0 {
            return RichLine::default();
        }
        let runs = self.expand_run_tabs(runs);
        let logical = split_runs_at_newlines(&runs);
        let line_runs = logical.first().cloned().unwrap_or_default();
        if self.runs_width(&line_runs) <= width {
            return RichLine { runs: line_runs };
        }
        let glyphs = GlyphSet::for_capabilities(self.capabilities);
        let indicator = glyphs.ellipsis;
        let indicator_width = self.options.width.line_width(indicator).min(width);
        let available = width.saturating_sub(indicator_width);
        let line_units = units(&line_runs, self.options.width);
        let mut end = 0;
        let mut cells = 0usize;
        while end < line_units.len() && cells + line_units[end].width <= available {
            cells += line_units[end].width;
            end += 1;
        }
        let mut line = line_from_units(&line_runs, &line_units[..end]);
        let indicator = if indicator_width == 0 {
            ""
        } else if self.options.width.line_width(indicator) <= width {
            indicator
        } else {
            "."
        };
        line.push(
            indicator.to_owned(),
            self.theme.style(TextRole::Subtle),
            None,
        );
        line
    }

    fn clip_line(&self, line: RichLine, width: usize) -> RichLine {
        self.clip_runs(&line.runs, width)
    }

    fn expand_run_tabs(&self, runs: &[RichRun]) -> Vec<RichRun> {
        let mut column = 0usize;
        runs.iter()
            .map(|run| {
                let expanded = self
                    .options
                    .width
                    .expand_tabs(&run.text, column)
                    .into_owned();
                for grapheme in expanded.graphemes(true) {
                    if matches!(grapheme, "\n" | "\r") {
                        column = 0;
                    } else {
                        column = column
                            .saturating_add(self.options.width.grapheme_width(grapheme, column));
                    }
                }
                RichRun::new(expanded, run.style, run.link.clone())
            })
            .collect()
    }

    fn runs_width(&self, runs: &[RichRun]) -> usize {
        let mut column = 0usize;
        for run in runs {
            column = column.saturating_add(self.options.width.line_width_from(&run.text, column));
        }
        column
    }

    fn line_width(&self, line: &RichLine) -> usize {
        self.runs_width(&line.runs)
    }

    fn encode_line(&self, line: RichLine, width: usize) -> RenderedLine {
        let line = self.clip_line(line, width);
        let mut plain = String::new();
        let mut styled = String::new();
        for run in line.runs {
            plain.push_str(&run.text);
            let styled_text = self.theme.apply_style(run.style, &run.text);
            if self.capabilities.hyperlinks {
                if let Some(target) = run.link.as_deref().and_then(SafeUrl::parse) {
                    styled.push_str("\x1b]8;;");
                    styled.push_str(target.as_str());
                    styled.push_str("\x1b\\");
                    styled.push_str(&styled_text);
                    styled.push_str("\x1b]8;;\x1b\\");
                    continue;
                }
            }
            styled.push_str(&styled_text);
        }
        RenderedLine { styled, plain }
    }

    /// Escape-free copy/log representation under this renderer's ASCII or
    /// Unicode fallback policy.
    pub fn sanitize_copy(&self, text: &str) -> String {
        self.sanitize(text)
    }

    fn sanitize(&self, text: &str) -> String {
        sanitize_text(
            text,
            SanitizeOptions {
                controls: if self.capabilities.unicode {
                    ControlPictures::Unicode
                } else {
                    ControlPictures::Ascii
                },
                preserve_newlines: true,
                preserve_tabs: true,
            },
        )
        .into_owned()
    }

    #[cfg(feature = "syntax-highlighting")]
    fn highlighted(&self, code: &CodeBlock) -> Option<Arc<Vec<super::highlight::HighlightedLine>>> {
        if !self.options.syntax_highlighting {
            return None;
        }
        let language = code.language.as_deref()?;
        self.syntax_cache
            .borrow_mut()
            .get_or_insert(language, &code.code)
    }

    #[cfg(not(feature = "syntax-highlighting"))]
    fn highlighted(&self, _code: &CodeBlock) -> Option<Arc<Vec<NeverHighlightedLine>>> {
        None
    }
}

#[cfg(not(feature = "syntax-highlighting"))]
type NeverHighlightedLine = Vec<NeverHighlightedRegion>;
#[cfg(not(feature = "syntax-highlighting"))]
struct NeverHighlightedRegion {
    text: String,
    role: Option<TextRole>,
}

/// Infer the syntax token from a unified-diff file header. Timestamps are
/// ignored, and backup suffixes are not treated as programming languages.
fn diff_language_hint(header: &str) -> Option<String> {
    let path = header
        .strip_prefix("+++ ")
        .or_else(|| header.strip_prefix("--- "))?
        .split('\t')
        .next()?;
    if path == "/dev/null" {
        return None;
    }
    let name = path.rsplit('/').next()?;
    let token = name
        .rsplit_once('.')
        .map_or(name, |(_, extension)| extension);
    (!token.is_empty()
        && token.len() <= 16
        && !token.eq_ignore_ascii_case("orig")
        && !token.eq_ignore_ascii_case("bak"))
    .then(|| token.to_ascii_lowercase())
}

#[derive(Clone, Debug, Default)]
struct RichLine {
    runs: Vec<RichRun>,
}

impl RichLine {
    fn from_plain(text: String) -> Self {
        let mut line = Self::default();
        line.push(text, TextStyle::plain(), None);
        line
    }

    fn push(&mut self, text: String, style: TextStyle, link: Option<String>) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self
            .runs
            .last_mut()
            .filter(|last| last.style == style && last.link == link)
        {
            last.text.push_str(&text);
        } else {
            self.runs.push(RichRun::new(text, style, link));
        }
    }

    fn extend(&mut self, other: RichLine) {
        for run in other.runs {
            self.push(run.text, run.style, run.link);
        }
    }

    fn is_empty(&self) -> bool {
        self.runs.iter().all(|run| run.text.is_empty())
    }
}

#[derive(Clone, Debug)]
struct RichRun {
    text: String,
    style: TextStyle,
    link: Option<String>,
}

impl RichRun {
    fn new(text: String, style: TextStyle, link: Option<String>) -> Self {
        Self { text, style, link }
    }
}

fn push_run(output: &mut Vec<RichRun>, text: String, style: TextStyle, link: Option<String>) {
    if let Some(last) = output
        .last_mut()
        .filter(|last| last.style == style && last.link == link)
    {
        last.text.push_str(&text);
    } else {
        output.push(RichRun::new(text, style, link));
    }
}

fn push_blank(output: &mut Vec<RichLine>) {
    if !output.last().is_some_and(RichLine::is_empty) {
        output.push(RichLine::default());
    }
}

fn split_runs_at_newlines(runs: &[RichRun]) -> Vec<Vec<RichRun>> {
    let mut output = vec![Vec::new()];
    for run in runs {
        for (index, part) in run.text.split('\n').enumerate() {
            if index > 0 {
                output.push(Vec::new());
            }
            if !part.is_empty() {
                output.last_mut().unwrap().push(RichRun::new(
                    part.to_owned(),
                    run.style,
                    run.link.clone(),
                ));
            }
        }
    }
    output
}

struct Unit {
    run: usize,
    start: usize,
    end: usize,
    width: usize,
    whitespace: bool,
}

fn units(runs: &[RichRun], policy: WidthPolicy) -> Vec<Unit> {
    let mut output = Vec::new();
    let mut column = 0usize;
    for (run_index, run) in runs.iter().enumerate() {
        for (start, grapheme) in run.text.grapheme_indices(true) {
            let width = policy.grapheme_width(grapheme, column);
            output.push(Unit {
                run: run_index,
                start,
                end: start + grapheme.len(),
                width,
                whitespace: grapheme.chars().all(char::is_whitespace),
            });
            column = column.saturating_add(width);
        }
    }
    output
}

fn line_from_units(runs: &[RichRun], units: &[Unit]) -> RichLine {
    let mut line = RichLine::default();
    let mut index = 0usize;
    while index < units.len() {
        let first = &units[index];
        let mut end = first.end;
        let mut next = index + 1;
        while next < units.len() && units[next].run == first.run && units[next].start == end {
            end = units[next].end;
            next += 1;
        }
        let source = &runs[first.run];
        line.push(
            source.text[first.start..end].to_owned(),
            source.style,
            source.link.clone(),
        );
        index = next;
    }
    line
}

#[cfg(feature = "syntax-highlighting")]
type CachedHighlightedLine = super::highlight::HighlightedLine;

#[derive(Default)]
struct SyntaxCache {
    #[cfg(feature = "syntax-highlighting")]
    entries: HashMap<SyntaxKey, Arc<Vec<CachedHighlightedLine>>>,
    #[cfg(feature = "syntax-highlighting")]
    order: VecDeque<SyntaxKey>,
    bytes: usize,
    hits: u64,
    misses: u64,
}

#[cfg(feature = "syntax-highlighting")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SyntaxKey {
    language: String,
    hash: u64,
    length: usize,
}

impl SyntaxCache {
    #[cfg(feature = "syntax-highlighting")]
    fn get_or_insert(
        &mut self,
        language: &str,
        code: &str,
    ) -> Option<Arc<Vec<super::highlight::HighlightedLine>>> {
        const MAX_ENTRIES: usize = 24;
        const MAX_BYTES: usize = 4 * 1024 * 1024;
        let mut hasher = DefaultHasher::new();
        code.hash(&mut hasher);
        let key = SyntaxKey {
            language: language.to_ascii_lowercase(),
            hash: hasher.finish(),
            length: code.len(),
        };
        if let Some(lines) = self.entries.get(&key) {
            self.hits = self.hits.saturating_add(1);
            return Some(lines.clone());
        }
        self.misses = self.misses.saturating_add(1);
        let lines = Arc::new(super::highlight::highlight(code, language)?);
        self.bytes = self.bytes.saturating_add(code.len());
        self.order.push_back(key.clone());
        self.entries.insert(key, lines.clone());
        while self.entries.len() > MAX_ENTRIES || self.bytes > MAX_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self.entries.remove(&oldest).is_some() {
                self.bytes = self.bytes.saturating_sub(oldest.length);
            }
        }
        Some(lines)
    }

    fn stats(&self) -> SyntaxCacheStats {
        SyntaxCacheStats {
            hits: self.hits,
            misses: self.misses,
            entries: {
                #[cfg(feature = "syntax-highlighting")]
                {
                    self.entries.len()
                }
                #[cfg(not(feature = "syntax-highlighting"))]
                {
                    0
                }
            },
            bytes: self.bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{CapabilityOverrides, ColorDepth};
    use crate::rich_text::markdown;
    use crate::style::Color;

    fn renderer(color: ColorDepth, unicode: bool) -> RichRenderer {
        let capabilities = TerminalCapabilities::interactive(color, unicode).with_overrides(
            &CapabilityOverrides {
                hyperlinks: Some(true),
                ..CapabilityOverrides::default()
            },
        );
        RichRenderer::new(
            Theme::with_capabilities(capabilities),
            capabilities,
            RenderOptions::default(),
        )
    }

    #[test]
    fn headings_lists_quotes_links_and_code_are_semantic_and_width_bounded() {
        let document = markdown::parse(
            "# Session recovery\n\nThe invalid **final record** is removed before the next append.\n\n## Changes\n- preserves valid records without a trailing newline\n- removes invalid trailing bytes\n\n> safe quote\n\n```rust\nfn main() { println!(\"hello\"); }\n```\n\n[docs](https://example.com)",
        );
        for width in [20, 40, 60, 80, 120, 160] {
            let rendered = renderer(ColorDepth::TrueColor, true).render(&document, width);
            assert!(rendered
                .lines
                .iter()
                .all(|line| WidthPolicy::default().line_width(&line.plain) <= usize::from(width)));
            let plain = rendered.plain_text();
            assert!(!plain.contains("# "));
            assert!(!plain.contains("**"));
            assert!(plain.contains("Session recovery"));
            assert!(rendered.copy_text.contains("docs (https://example.com)"));
        }
    }

    #[test]
    fn plain_ascii_mode_contains_no_escape_or_unicode_structure() {
        let document =
            markdown::parse("# Heading\n\n- item\n\n> quote\n\n[docs](https://example.com)");
        let rendered = RichRenderer::plain().render(&document, 40);
        let text = rendered.styled_text();
        assert!(!text.contains('\x1b'));
        assert!(text.contains("* item"));
        assert!(text.contains("| quote"));
        assert!(text.is_ascii());
    }

    #[test]
    fn typed_accent_and_unknown_language_degrade_safely() {
        let capabilities = TerminalCapabilities::interactive(ColorDepth::Ansi16, true);
        let mut theme = Theme::with_capabilities(capabilities);
        theme.set_accent(Color::Rgb(1, 2, 3));
        let renderer = RichRenderer::new(theme, capabilities, RenderOptions::default());
        let document = Document::new(vec![
            Block::Heading {
                level: 1,
                content: vec![Inline::Role {
                    role: TextRole::Accent,
                    content: vec![Inline::Text("accent".into())],
                }],
            },
            Block::CodeBlock(CodeBlock::with_language("unknown-lang", "plain code")),
        ]);
        let rendered = renderer.render(&document, 30);
        assert!(rendered.plain_text().contains("plain code"));
        assert!(!rendered.styled_text().contains("38;2;"));
    }

    #[test]
    fn code_surfaces_are_compact_terminal_neutral_and_have_no_phantom_row() {
        let document = Document::new(vec![Block::CodeBlock(CodeBlock::with_language(
            "rust",
            "let answer = 42;\n",
        ))]);
        let rendered = renderer(ColorDepth::TrueColor, true).render(&document, 160);
        assert_eq!(rendered.lines.len(), 3, "{}", rendered.plain_text());
        assert!(rendered
            .lines
            .iter()
            .all(|line| WidthPolicy::default().line_width(&line.plain) < 40));
        assert!(!rendered.styled_text().contains("\x1b[48;"));

        let plain = RichRenderer::plain().render(&document, 160);
        let widths = plain
            .lines
            .iter()
            .map(|line| WidthPolicy::default().line_width(&line.plain))
            .collect::<Vec<_>>();
        assert!(widths.iter().all(|width| *width == widths[0]), "{widths:?}");
        assert!(plain.lines[0].plain.contains("rust"));
    }

    #[test]
    fn generic_code_fence_labels_are_suppressed_but_meaningful_labels_remain() {
        for language in ["text", "TEXT", "plaintext", "PlainText"] {
            let document = Document::new(vec![Block::CodeBlock(CodeBlock::with_language(
                language, "value",
            ))]);
            let rendered = renderer(ColorDepth::TrueColor, true).render(&document, 40);
            let plain = rendered.plain_text();
            assert!(plain.contains("value"), "{plain:?}");
            assert!(!plain.to_ascii_lowercase().contains("text"), "{plain:?}");
            assert!(rendered.copy_text.contains("value"));
        }

        let rust = Document::new(vec![Block::CodeBlock(CodeBlock::with_language(
            "rust",
            "let answer = 42;",
        ))]);
        assert!(renderer(ColorDepth::TrueColor, true)
            .render(&rust, 40)
            .plain_text()
            .contains("rust"));
    }

    #[test]
    fn code_and_diff_backgrounds_are_explicit_theme_opt_ins() {
        let capabilities = TerminalCapabilities::interactive(ColorDepth::TrueColor, true);
        let mut theme = Theme::with_capabilities(capabilities);
        let document = markdown::parse("`name`\n\n```text\nvalue\n```");
        let neutral = RichRenderer::new(theme.clone(), capabilities, RenderOptions::default())
            .render(&document, 40)
            .styled_text();
        assert!(!neutral.contains("\x1b[48;"), "{neutral:?}");

        theme.override_token("md_code_bg", "#202020");
        theme.override_token("md_code_inline_bg", "#303030");
        theme.override_token("diff_added_bg", "#103010");
        let themed = RichRenderer::new(theme, capabilities, RenderOptions::default());
        assert!(themed
            .render(&document, 40)
            .styled_text()
            .contains("\x1b[48;2;"));
        assert!(themed
            .render_diff(
                &UnifiedDiff::parse("@@ -0,0 +1 @@\n+value"),
                40,
                DiffRenderOptions::default(),
            )
            .styled_text()
            .contains("\x1b[48;2;"));
    }

    #[cfg(feature = "syntax-highlighting")]
    #[test]
    fn public_diff_renderer_syntax_highlights_code_from_file_headers() {
        let capabilities = TerminalCapabilities::interactive(ColorDepth::TrueColor, true);
        let mut theme = Theme::with_capabilities(capabilities);
        theme.override_token("diff_added", "#04aa05");
        theme.override_token("diff_added_marker", "#04aa05");
        theme.override_token("syntax_keyword", "#010203");
        theme.override_token("syntax_string", "#060708");
        let renderer = RichRenderer::new(theme, capabilities, RenderOptions::default());
        let rendered = renderer
            .render_diff(
                &UnifiedDiff::parse(
                    "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -0,0 +1 @@\n+fn main() { let value = \"text\"; }",
                ),
                100,
                DiffRenderOptions {
                    line_numbers: true,
                    wrap: true,
                },
            )
            .styled_text();

        assert!(rendered.contains("\x1b[38;2;4;170;5m+"), "{rendered:?}");
        assert!(rendered.contains("\x1b[38;2;1;2;3mfn"), "{rendered:?}");
        assert!(
            rendered.contains("\x1b[38;2;6;7;8m\"text\""),
            "{rendered:?}"
        );
    }

    #[test]
    fn wrapped_code_retains_source_whitespace_instead_of_prose_wrapping() {
        let capabilities = TerminalCapabilities::plain();
        let options = RenderOptions {
            code_overflow: CodeOverflow::Wrap,
            ..RenderOptions::default()
        };
        let renderer = RichRenderer::new(
            Theme::with_capabilities(capabilities),
            capabilities,
            options,
        );
        let rendered = renderer.render(
            &Document::new(vec![Block::CodeBlock(CodeBlock::new("a  b  cdefgh"))]),
            12,
        );
        assert!(rendered.plain_text().contains("a  b  c"), "{rendered:?}");
        assert_eq!(rendered.copy_text, "a  b  cdefgh\n");
    }

    #[test]
    fn semantic_statuses_have_plain_non_color_markers() {
        let document = Document::new(vec![Block::Paragraph(vec![
            Inline::status(StatusKind::Success, "saved"),
            Inline::Text(" ".into()),
            Inline::status(StatusKind::Warning, "check"),
            Inline::Text(" ".into()),
            Inline::status(StatusKind::Error, "failed"),
            Inline::Text(" ".into()),
            Inline::status(StatusKind::Pending, "waiting"),
        ])]);
        let rendered = RichRenderer::plain().render(&document, 80);
        assert_eq!(rendered.plain_text(), "+ saved ! check x failed . waiting");
        assert_eq!(rendered.copy_text, "+ saved ! check x failed . waiting\n");
    }

    #[test]
    fn detail_blocks_have_non_color_expand_markers_and_copy_hidden_content() {
        let hidden = Block::Paragraph(vec![Inline::Text("hidden content".into())]);
        let collapsed = Document::new(vec![Block::Detail(DetailBlock::new(
            "Details",
            vec![hidden.clone()],
            false,
        ))]);
        let expanded = Document::new(vec![Block::Detail(DetailBlock::new(
            "Details",
            vec![hidden],
            true,
        ))]);
        let renderer = RichRenderer::plain();
        let collapsed_output = renderer.render(&collapsed, 40);
        assert!(collapsed_output.plain_text().starts_with("[+] Details"));
        assert!(!collapsed_output.plain_text().contains("hidden content"));
        assert!(collapsed_output.copy_text.contains("hidden content"));
        let expanded_output = renderer.render(&expanded, 40).plain_text();
        assert!(expanded_output.starts_with("[-] Details"));
        assert!(expanded_output.contains("hidden content"));
    }

    #[test]
    fn tables_fall_back_at_narrow_widths() {
        let document = markdown::parse(
            "| Key | Value |\n| --- | --- |\n| language | Rust |\n| status | green |",
        );
        let narrow = RichRenderer::plain().render(&document, 10).plain_text();
        assert!(narrow.contains("Key:"));
        assert!(narrow.contains("language"));
        let wide = renderer(ColorDepth::TrueColor, true)
            .render(&document, 60)
            .plain_text();
        assert!(wide.contains('│'));
    }

    #[test]
    fn diff_keeps_prefixes_in_plain_mode() {
        let diff = UnifiedDiff::parse("--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n same");
        let rendered = RichRenderer::plain().render_diff(
            &diff,
            20,
            DiffRenderOptions {
                line_numbers: true,
                wrap: false,
            },
        );
        let plain = rendered.plain_text();
        assert!(plain.contains("-old"));
        assert!(plain.contains("+new"));
        assert!(plain.contains("1 | -old"), "single gutter was {plain:?}");
        assert!(plain.contains("1 | +new"), "single gutter was {plain:?}");
        assert!(plain.lines().all(|line| {
            line.split_once('|')
                .is_none_or(|(gutter, _)| gutter.split_whitespace().count() <= 1)
        }));
        assert!(!rendered.styled_text().contains('\x1b'));
    }

    #[cfg(feature = "syntax-highlighting")]
    #[test]
    fn unstable_rendering_defers_syntax_work() {
        let renderer = renderer(ColorDepth::TrueColor, true);
        let document = Document::new(vec![Block::CodeBlock(CodeBlock::with_language(
            "rust",
            "fn partial(",
        ))]);
        renderer.render_unstable(&document, 80);
        renderer.render_unstable(&document, 80);
        assert_eq!(renderer.syntax_cache_stats(), SyntaxCacheStats::default());
        renderer.render(&document, 80);
        assert_eq!(renderer.syntax_cache_stats().misses, 1);
    }

    #[cfg(feature = "syntax-highlighting")]
    #[test]
    fn syntax_data_is_cached_per_renderer() {
        let renderer = renderer(ColorDepth::TrueColor, true);
        let document = Document::new(vec![Block::CodeBlock(CodeBlock::with_language(
            "rust",
            "fn main() {}",
        ))]);
        renderer.render(&document, 80);
        renderer.render(&document, 80);
        let stats = renderer.syntax_cache_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
    }
}
