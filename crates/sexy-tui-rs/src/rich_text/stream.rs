//! Stream-safe Markdown with a committed prefix and bounded mutable tail.
//!
//! Complete top-level blocks are committed once a following block proves their
//! boundary. The final document is parsed once from the complete raw text, so
//! completion is semantically identical to static parsing. An unstable suffix
//! is never reparsed beyond [`MAX_UNSTABLE_PARSE_BYTES`].

use std::str;

use super::markdown;
use super::render::{RenderOptions, RenderedDocument, RenderedLine, RichRenderer};
use super::{Block, CodeBlock, Document};

/// Maximum suffix considered by the CommonMark parser during an active stream.
pub const MAX_UNSTABLE_PARSE_BYTES: usize = 64 * 1024;
/// Small inline tails are cheap enough to keep semantically current after a
/// delimiter closes. Beyond this bound the geometric parser remains in charge.
const MAX_LIVE_INLINE_PREVIEW_BYTES: usize = 8 * 1024;

/// Streaming work counters for performance regression tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamingStats {
    pub parse_passes: u64,
    pub reparsed_bytes: u64,
    pub committed_blocks: usize,
    pub pending_utf8_bytes: usize,
}

#[derive(Clone, Debug)]
struct FenceState {
    marker: char,
    count: usize,
    start: usize,
    code_start: usize,
    language: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct FenceScanner {
    offset: usize,
    open: Option<FenceState>,
}

impl FenceScanner {
    fn scan(&mut self, source: &str) -> bool {
        let mut completed_fence = false;
        while self.offset < source.len() {
            let Some(relative_end) = source[self.offset..].find('\n') else {
                break;
            };
            let end = self.offset + relative_end + 1;
            let line = source[self.offset..end].trim_end_matches(['\r', '\n']);
            if let Some(open) = &self.open {
                if is_closing_fence(line, open.marker, open.count) {
                    self.open = None;
                    completed_fence = true;
                }
            } else if let Some((marker, count, info)) = opening_fence(line) {
                self.open = Some(FenceState {
                    marker,
                    count,
                    start: self.offset,
                    code_start: end,
                    language: info,
                });
            }
            self.offset = end;
        }
        completed_fence
    }

    fn drain_prefix(&mut self, bytes: usize) {
        self.offset = self.offset.saturating_sub(bytes);
        if let Some(open) = &mut self.open {
            open.start = open.start.saturating_sub(bytes);
            open.code_start = open.code_start.saturating_sub(bytes);
        }
    }
}

/// Incremental Markdown state. `raw_bytes()` always retains the original input,
/// including invalid UTF-8 bytes used for logging or diagnostics.
#[derive(Clone, Debug, Default)]
pub struct StreamingMarkdown {
    raw: Vec<u8>,
    decoded: String,
    pending_utf8: Vec<u8>,
    committed: Document,
    tail: String,
    preview: Document,
    scanner: FenceScanner,
    finished: bool,
    committed_revision: u64,
    tail_revision: u64,
    next_parse_at: usize,
    tail_semantic_parsed: bool,
    stats: StreamingStats,
}

impl StreamingMarkdown {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_text(text: &str) -> Self {
        let mut stream = Self::new();
        stream.push_str(text);
        stream
    }

    pub fn push_str(&mut self, chunk: &str) {
        self.push_bytes(chunk.as_bytes());
    }

    /// Ingest arbitrary byte chunks, buffering incomplete UTF-8 scalar values
    /// and replacing only permanently malformed sequences in the display text.
    pub fn push_bytes(&mut self, chunk: &[u8]) {
        if self.finished || chunk.is_empty() {
            return;
        }
        self.raw.extend_from_slice(chunk);
        self.pending_utf8.extend_from_slice(chunk);
        let decoded_chunk = decode_available(&mut self.pending_utf8, false);
        self.stats.pending_utf8_bytes = self.pending_utf8.len();
        if decoded_chunk.is_empty() {
            return;
        }
        let had_open_fence = self.scanner.open.is_some();
        self.decoded.push_str(&decoded_chunk);
        let proven_boundary = self.tail.ends_with("\n\n")
            || decoded_chunk.contains("\n\n")
            || (self.tail.ends_with('\n') && decoded_chunk.starts_with('\n'));
        self.tail.push_str(&decoded_chunk);
        if had_open_fence {
            if let [Block::CodeBlock(code)] = self.preview.blocks.as_mut_slice() {
                code.code.push_str(&decoded_chunk);
            }
        }
        let completed_fence = self.scanner.scan(&self.tail);
        self.stabilize(
            had_open_fence,
            decoded_chunk.contains('\n'),
            completed_fence,
            proven_boundary,
        );
        self.tail_revision = self.tail_revision.saturating_add(1);
    }

    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    /// Lossy UTF-8 view used for terminal display. The raw bytes remain exact.
    pub fn raw_text(&self) -> &str {
        &self.decoded
    }

    pub fn committed(&self) -> &Document {
        &self.committed
    }

    pub fn unstable_source(&self) -> &str {
        &self.tail
    }

    pub fn preview(&self) -> &Document {
        &self.preview
    }

    /// Current semantic copy text. Incomplete delimiters in the mutable tail
    /// remain visible until they become valid syntax.
    pub fn copy_text(&self) -> String {
        let mut output = self.committed.plain_text();
        let tail = self.preview.plain_text();
        if !output.is_empty() && !output.ends_with('\n') && !tail.is_empty() {
            output.push('\n');
        }
        output.push_str(&tail);
        output
    }

    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    pub const fn committed_revision(&self) -> u64 {
        self.committed_revision
    }

    pub const fn tail_revision(&self) -> u64 {
        self.tail_revision
    }

    pub fn stats(&self) -> StreamingStats {
        StreamingStats {
            committed_blocks: self.committed.blocks.len(),
            pending_utf8_bytes: self.pending_utf8.len(),
            ..self.stats
        }
    }

    /// Finish the stream. The result is exactly the static parser's semantic
    /// output for the decoded text.
    pub fn finish(&mut self) -> &Document {
        if self.finished {
            return &self.committed;
        }
        let remainder = decode_available(&mut self.pending_utf8, true);
        if !remainder.is_empty() {
            self.decoded.push_str(&remainder);
            self.tail.push_str(&remainder);
        }
        self.stats.pending_utf8_bytes = 0;
        self.stats.parse_passes = self.stats.parse_passes.saturating_add(1);
        self.stats.reparsed_bytes = self
            .stats
            .reparsed_bytes
            .saturating_add(self.decoded.len() as u64);
        self.committed = markdown::parse(&self.decoded);
        self.tail.clear();
        self.preview = Document::default();
        self.finished = true;
        self.committed_revision = self.committed_revision.saturating_add(1);
        self.tail_revision = self.tail_revision.saturating_add(1);
        &self.committed
    }

    fn stabilize(
        &mut self,
        had_open_fence: bool,
        saw_newline: bool,
        completed_fence: bool,
        proven_boundary: bool,
    ) {
        if let Some(open) = self.scanner.open.clone() {
            if !had_open_fence {
                if open.start > 0 {
                    self.commit_prefix(open.start);
                }
                // Prefix draining adjusts the scanner's offsets.
                let open = self.scanner.open.as_ref().expect("open fence retained");
                self.preview = Document::new(vec![Block::CodeBlock(CodeBlock {
                    language: open.language.clone(),
                    code: self.tail[open.code_start..].to_owned(),
                })]);
                self.tail_semantic_parsed = true;
            }
            return;
        }

        if had_open_fence {
            // The close marker may be present in `open_code`; discard the
            // incremental preview and parse the now-complete fenced block once.
        }

        if !saw_newline {
            // Reasoning summaries and short answers often contain complete
            // inline Markdown but no newline (for example `**Planning**`).
            // Promote those tails immediately, then keep their bounded preview
            // current as more tokens arrive. Huge paragraphs still use the
            // geometric fallback below and never incur unbounded reparsing.
            let live_inline = self.tail.len() <= MAX_LIVE_INLINE_PREVIEW_BYTES
                && (self.tail_semantic_parsed || likely_complete_inline(&self.tail));
            if live_inline {
                self.record_parse(self.tail.len());
                self.preview = markdown::parse(&self.tail);
                self.tail_semantic_parsed = true;
            } else {
                self.append_plain_preview();
            }
            return;
        }

        let parse_threshold = self.next_parse_at.max(1024);
        let structural_line = self.tail.lines().next().is_some_and(|line| {
            let line = line.trim_start();
            line.starts_with('#')
                || line.starts_with('>')
                || is_list_marker(line)
                || matches!(line, "---" | "***" | "___")
        });
        let structural_tail = self.tail.lines().next_back().is_some_and(|line| {
            let line = line.trim();
            matches!(line, "---" | "***" | "___") || (line.contains('|') && line.contains("---"))
        });
        if self.tail.len() <= MAX_UNSTABLE_PARSE_BYTES
            && (had_open_fence
                || completed_fence
                || proven_boundary
                || ((structural_line || structural_tail) && !self.tail_semantic_parsed)
                || self.tail.len() >= parse_threshold)
        {
            self.parse_and_commit_stable_tail();
            self.next_parse_at = self
                .tail
                .len()
                .saturating_mul(2)
                .clamp(1024, MAX_UNSTABLE_PARSE_BYTES);
        } else if self.tail.len() <= MAX_UNSTABLE_PARSE_BYTES {
            self.append_plain_preview();
        } else if proven_boundary {
            if let Some(offset) = lexical_stable_offset(&self.tail) {
                self.commit_prefix(offset);
                self.parse_and_commit_stable_tail();
            } else {
                self.append_plain_preview();
            }
        } else {
            // An enormous single paragraph/list remains mutable. Display it as
            // safe literal text and parse it only once on completion.
            self.preview = Document::new(vec![Block::Plain(self.tail.clone())]);
        }
    }

    fn parse_and_commit_stable_tail(&mut self) {
        self.record_parse(self.tail.len());
        let starts = markdown::top_level_block_starts(&self.tail);
        if starts.len() >= 2 {
            if let Some(offset) = starts.last().copied().filter(|offset| *offset > 0) {
                self.commit_prefix(offset);
            }
        }
        self.record_parse(self.tail.len());
        self.preview = markdown::parse(&self.tail);
        self.tail_semantic_parsed = true;
    }

    fn commit_prefix(&mut self, offset: usize) {
        if offset == 0 || offset > self.tail.len() || !self.tail.is_char_boundary(offset) {
            return;
        }
        let prefix_len = offset;
        let mut document = markdown::parse(&self.tail[..offset]);
        self.record_parse(prefix_len);
        self.committed.blocks.append(&mut document.blocks);
        self.tail.drain(..offset);
        self.scanner.drain_prefix(offset);
        // A drained tail invalidates any literal preview prefix. Callers either
        // replace it immediately with a semantic render or append afresh.
        self.preview = Document::default();
        self.next_parse_at = 1024;
        self.tail_semantic_parsed = false;
        self.committed_revision = self.committed_revision.saturating_add(1);
    }

    fn record_parse(&mut self, bytes: usize) {
        self.stats.parse_passes = self.stats.parse_passes.saturating_add(1);
        self.stats.reparsed_bytes = self.stats.reparsed_bytes.saturating_add(bytes as u64);
    }

    fn append_plain_preview(&mut self) {
        match self.preview.blocks.as_mut_slice() {
            [Block::Plain(text)] if text.len() <= self.tail.len() => {
                // Literal tails change only by append; `commit_prefix` clears
                // the preview before draining. Trust that invariant instead of
                // comparing the complete accumulated paragraph on every token.
                if text.len() < self.tail.len() {
                    text.push_str(&self.tail[text.len()..]);
                }
            }
            _ => self.preview = Document::new(vec![Block::Plain(self.tail.clone())]),
        }
    }
}

/// Incremental line-layout cache. At a stable width, only newly committed
/// blocks and the bounded mutable tail are rendered.
#[derive(Clone, Debug, Default)]
pub struct StreamingRenderCache {
    width: Option<u16>,
    options: Option<RenderOptions>,
    theme_revision: u64,
    committed_revision: u64,
    committed_blocks: usize,
    committed_lines: Vec<RenderedLine>,
    tail_revision: u64,
    tail_lines: Vec<RenderedLine>,
    /// Pre-built merged result, invalidated when either committed or tail changes.
    merged_lines: Vec<RenderedLine>,
    merged_revision: u64,
    finished: bool,
}

impl StreamingRenderCache {
    /// Rows produced by parser-committed blocks at the current width. These
    /// rows are byte-stable across later token deltas and may safely cross a
    /// native-scrollback commit boundary.
    pub fn committed_rows(&self) -> usize {
        self.committed_lines.len()
    }

    pub fn render(
        &mut self,
        stream: &StreamingMarkdown,
        renderer: &RichRenderer,
        width: u16,
    ) -> RenderedDocument {
        let full_reflow = self.width != Some(width)
            || self.options != Some(renderer.options())
            || self.theme_revision != renderer.theme().revision()
            || (stream.is_finished() && !self.finished)
            || stream.committed().blocks.len() < self.committed_blocks;

        if full_reflow {
            self.committed_lines = renderer.render(stream.committed(), width).lines;
            self.committed_blocks = stream.committed().blocks.len();
        } else if self.committed_revision != stream.committed_revision()
            && self.committed_blocks < stream.committed().blocks.len()
        {
            let new_blocks = &stream.committed().blocks[self.committed_blocks..];
            if !self.committed_lines.is_empty() && !new_blocks.is_empty() {
                self.committed_lines.push(RenderedLine::default());
            }
            self.committed_lines
                .extend(renderer.render_blocks_only(new_blocks, width));
            self.committed_blocks = stream.committed().blocks.len();
        }

        if full_reflow || self.tail_revision != stream.tail_revision() {
            self.tail_lines = if stream.is_finished() {
                Vec::new()
            } else {
                // Unclosed code is intentionally rendered without syntax work
                // in the mutable tail. It is highlighted once committed/final.
                renderer.render_unstable(stream.preview(), width).lines
            };
        }

        self.width = Some(width);
        self.options = Some(renderer.options());
        self.theme_revision = renderer.theme().revision();
        self.committed_revision = stream.committed_revision();
        self.tail_revision = stream.tail_revision();
        self.finished = stream.is_finished();

        // Rebuild the merged view only when the committed portion or the
        // mutable tail actually changed.  On a typical streaming frame only
        // the tail revision increments; the committed prefix stays stable
        // and we avoid cloning hundreds of RenderedLine strings per frame.
        let merge_rev = self
            .committed_revision
            .wrapping_mul(2)
            .wrapping_add(self.tail_revision);
        if self.merged_revision != merge_rev || full_reflow {
            let total = self.committed_lines.len()
                + if self.committed_lines.is_empty() || self.tail_lines.is_empty() {
                    0
                } else {
                    1
                }
                + self.tail_lines.len();
            self.merged_lines.clear();
            self.merged_lines.reserve(total);
            self.merged_lines
                .extend(self.committed_lines.iter().cloned());
            if !self.committed_lines.is_empty() && !self.tail_lines.is_empty() {
                self.merged_lines.push(RenderedLine::default());
            }
            self.merged_lines.extend(self.tail_lines.iter().cloned());
            self.merged_revision = merge_rev;
        }

        RenderedDocument {
            lines: self.merged_lines.clone(),
            copy_text: renderer.sanitize_copy(&stream.copy_text()),
        }
    }
}

fn decode_available(buffer: &mut Vec<u8>, finish: bool) -> String {
    let mut output = String::new();
    loop {
        match str::from_utf8(buffer) {
            Ok(valid) => {
                output.push_str(valid);
                buffer.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    // Safety: UTF-8 validation identified this exact prefix.
                    output.push_str(
                        str::from_utf8(&buffer[..valid]).expect("validated UTF-8 prefix"),
                    );
                    buffer.drain(..valid);
                    continue;
                }
                if let Some(length) = error.error_len() {
                    output.push('\u{fffd}');
                    buffer.drain(..length.min(buffer.len()));
                    continue;
                }
                if finish {
                    output.push('\u{fffd}');
                    buffer.clear();
                }
                break;
            }
        }
    }
    output
}

fn opening_fence(line: &str) -> Option<(char, usize, Option<String>)> {
    let line = line
        .strip_prefix("   ")
        .or_else(|| line.strip_prefix("  "))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line);
    let marker = line.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let count = line
        .chars()
        .take_while(|character| *character == marker)
        .count();
    if count < 3 {
        return None;
    }
    let info = line[count..]
        .split_whitespace()
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Some((marker, count, info))
}

fn is_closing_fence(line: &str, marker: char, minimum: usize) -> bool {
    let line = line
        .strip_prefix("   ")
        .or_else(|| line.strip_prefix("  "))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line);
    let count = line
        .chars()
        .take_while(|character| *character == marker)
        .count();
    count >= minimum && line[count..].trim().is_empty()
}

fn likely_complete_inline(source: &str) -> bool {
    let paired = |marker: char| source.matches(marker).nth(1).is_some();
    paired('*')
        || paired('_')
        || paired('`')
        || source.match_indices("~~").nth(1).is_some()
        || source
            .find("](")
            .is_some_and(|start| source[start.saturating_add(2)..].contains(')'))
}

fn lexical_stable_offset(source: &str) -> Option<usize> {
    let offset = source.rfind("\n\n")?.saturating_add(2);
    if offset >= source.len() {
        return None;
    }
    let first = source.lines().next().unwrap_or_default().trim_start();
    let candidate = source[offset..]
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start();
    let source_is_list = is_list_marker(first);
    let candidate_is_list = is_list_marker(candidate);
    let same_quote = first.starts_with('>') && candidate.starts_with('>');
    let possible_list_continuation = source_is_list
        && (candidate_is_list || candidate.starts_with("  ") || candidate.starts_with('\t'));
    let possible_indented_code = (first.starts_with("    ") || first.starts_with('\t'))
        && (candidate.starts_with("    ") || candidate.starts_with('\t'));
    let possible_html_block = first.starts_with('<');
    if same_quote || possible_list_continuation || possible_indented_code || possible_html_block {
        None
    } else {
        Some(offset)
    }
}

fn is_list_marker(line: &str) -> bool {
    line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
        || line
            .split_once(". ")
            .is_some_and(|(number, _)| number.chars().all(|character| character.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich_text::render::RichRenderer;

    const ADVERSARIAL: &str = "# Heading\n\nA **strong** link to [docs](https://example.com) and `code`.\n\n- first\n  - nested\n- second\n\n```rust\nfn main() {\n    println!(\"界\");\n}\n```\n";

    #[test]
    fn committed_blocks_are_a_monotonic_prefix_of_the_final_document() {
        let expected = markdown::parse(ADVERSARIAL);
        let mut stream = StreamingMarkdown::new();
        for byte in ADVERSARIAL.as_bytes() {
            stream.push_bytes(&[*byte]);
            assert!(expected.blocks.starts_with(&stream.committed().blocks));
        }
    }

    #[test]
    fn every_utf8_chunk_boundary_finishes_like_static_parsing() {
        let expected = markdown::parse(ADVERSARIAL);
        for split in 0..=ADVERSARIAL.len() {
            if !ADVERSARIAL.is_char_boundary(split) {
                continue;
            }
            let mut stream = StreamingMarkdown::new();
            stream.push_str(&ADVERSARIAL[..split]);
            stream.push_str(&ADVERSARIAL[split..]);
            assert_eq!(stream.finish(), &expected, "split at {split}");
            assert_eq!(stream.raw_bytes(), ADVERSARIAL.as_bytes());
        }
    }

    #[test]
    fn arbitrary_byte_chunks_buffer_utf8_and_never_panic() {
        let mut stream = StreamingMarkdown::new();
        for byte in ADVERSARIAL.as_bytes() {
            stream.push_bytes(&[*byte]);
        }
        assert_eq!(stream.finish(), &markdown::parse(ADVERSARIAL));
        assert_eq!(stream.stats().pending_utf8_bytes, 0);
    }

    #[test]
    fn complete_fence_in_one_chunk_is_parsed_without_waiting_for_finish() {
        let mut stream = StreamingMarkdown::new();
        stream.push_str("```rust\nfn main() {}\n```\n");
        assert!(matches!(
            stream.preview().blocks.as_slice(),
            [Block::CodeBlock(_)]
        ));
    }

    #[test]
    fn incomplete_inline_and_block_syntax_is_mutable_not_permanently_wrong() {
        let mut stream = StreamingMarkdown::new();
        stream.push_str("Opening **strong");
        assert!(stream.preview().plain_text().contains("**strong"));
        stream.push_str("**\n\n```ru");
        stream.push_str("st\nfn main()");
        assert!(stream.preview().plain_text().contains("fn main"));
        stream.push_str(" {}\n```\n");
        let final_document = stream.finish().clone();
        assert_eq!(final_document, markdown::parse(stream.raw_text()));
        assert!(!final_document.plain_text().contains("**"));
    }

    #[test]
    fn complete_inline_markdown_becomes_rich_before_a_newline_or_finish() {
        let mut stream = StreamingMarkdown::new();
        stream.push_str("**Plan");
        assert!(stream.preview().plain_text().contains("**Plan"));
        stream.push_str("ning**");
        assert_eq!(stream.preview().plain_text(), "Planning\n");
        assert!(matches!(
            stream.preview().blocks.as_slice(),
            [Block::Paragraph(content)]
                if matches!(content.as_slice(), [crate::rich_text::Inline::Strong(_)])
        ));

        stream.push_str(" and `testing`");
        assert_eq!(stream.preview().plain_text(), "Planning and testing\n");
        let rendered = RichRenderer::plain()
            .render_unstable(stream.preview(), 80)
            .plain_text();
        assert!(!rendered.contains("**"));
        assert!(!rendered.contains('`'));
    }

    #[test]
    fn malformed_utf8_and_escape_sequences_are_recoverable_and_safe() {
        let mut stream = StreamingMarkdown::new();
        stream.push_bytes(b"safe \xf0\x9f");
        assert_eq!(stream.stats().pending_utf8_bytes, 2);
        stream.push_bytes(b"\x92\xa1 \x1b]52;c;bad\x07");
        stream.finish();
        assert_eq!(
            stream.raw_bytes(),
            b"safe \xf0\x9f\x92\xa1 \x1b]52;c;bad\x07"
        );
        let rendered = RichRenderer::plain()
            .render(stream.committed(), 80)
            .styled_text();
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
    }

    #[test]
    fn long_single_paragraph_uses_geometric_not_per_line_reparsing() {
        let mut stream = StreamingMarkdown::new();
        for _ in 0..20_000 {
            stream.push_str("word line\n");
        }
        assert!(stream.stats().reparsed_bytes < 4 * MAX_UNSTABLE_PARSE_BYTES as u64);
        stream.finish();
    }

    #[test]
    fn unstable_reparse_is_bounded_for_huge_open_blocks() {
        let mut stream = StreamingMarkdown::new();
        stream.push_str("```text\n");
        for _ in 0..10_000 {
            stream.push_str("a long code line\n");
        }
        let before_finish = stream.stats();
        assert!(before_finish.reparsed_bytes < 2 * MAX_UNSTABLE_PARSE_BYTES as u64);
        assert!(stream.preview().plain_text().contains("a long code line"));
        stream.push_str("```\n");
        stream.finish();
        assert_eq!(stream.committed(), &markdown::parse(stream.raw_text()));
    }

    #[test]
    fn render_cache_reflows_on_resize_and_tracks_latest_tail() {
        let mut stream = StreamingMarkdown::new();
        let renderer = RichRenderer::plain();
        let mut cache = StreamingRenderCache::default();
        stream.push_str("first paragraph\n\nsecond");
        let wide = cache.render(&stream, &renderer, 80).plain_text();
        assert!(wide.contains("second"));
        let narrow = cache.render(&stream, &renderer, 10).plain_text();
        assert!(narrow.lines().all(|line| line.len() <= 10));
    }
}
