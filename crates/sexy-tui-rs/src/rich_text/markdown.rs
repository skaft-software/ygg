//! CommonMark/GFM parsing into the reusable rich-document model.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::{
    Block, CodeBlock, Document, Inline, List, ListItem, ListKind, Table, TableAlignment, TableCell,
};

/// Parser options chosen for rich terminal prose. Footnotes and raw HTML
/// semantics are intentionally not enabled; HTML-like input remains visible
/// fallback text.
pub fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_GFM
}

/// Parse Markdown into semantic content. Input is retained as text values and
/// is sanitized only when rendered, so logging/debugging can keep the original.
pub fn parse(source: &str) -> Document {
    let parser = Parser::new_ext(source, parser_options());
    Builder::new().build(parser)
}

/// Top-level block starts used by the bounded streaming parser.
pub(crate) fn top_level_block_starts(source: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut block_depth = 0usize;
    for (event, range) in Parser::new_ext(source, parser_options()).into_offset_iter() {
        match event {
            Event::Start(tag) if is_block_tag(&tag) => {
                if block_depth == 0 {
                    starts.push(range.start);
                }
                block_depth += 1;
            }
            Event::End(end) if is_block_end(end) => {
                block_depth = block_depth.saturating_sub(1);
            }
            Event::Rule if block_depth == 0 => starts.push(range.start),
            _ => {}
        }
    }
    starts.sort_unstable();
    starts.dedup();
    starts
}

/// Byte ranges of parser-observed top-level blocks. The final range may still
/// be mutable when the source is an unfinished stream.
pub fn block_ranges(source: &str) -> Vec<Range<usize>> {
    let starts = top_level_block_starts(source);
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(source.len());
            *start..end
        })
        .collect()
}

pub(crate) fn is_block_tag(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::BlockQuote(_)
            | Tag::CodeBlock(_)
            | Tag::HtmlBlock
            | Tag::List(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::Table(_)
            | Tag::MetadataBlock(_)
    )
}

fn is_block_end(end: TagEnd) -> bool {
    matches!(
        end,
        TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::CodeBlock
            | TagEnd::HtmlBlock
            | TagEnd::List(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::Table
            | TagEnd::MetadataBlock(_)
    )
}

enum Frame {
    Root(Vec<Block>),
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Quote(Vec<Block>),
    Code {
        language: Option<String>,
        code: String,
    },
    Html(String),
    List {
        kind: ListKind,
        items: Vec<ListItem>,
    },
    Item(ListItem),
    Inline(InlineKind, Vec<Inline>),
    Link {
        target: String,
        label: Vec<Inline>,
    },
    Image {
        target: String,
        alt: Vec<Inline>,
    },
    Table {
        alignments: Vec<TableAlignment>,
        header: Vec<TableCell>,
        rows: Vec<Vec<TableCell>>,
    },
    TableHead(Vec<TableCell>),
    TableRow(Vec<TableCell>),
    TableCell(Vec<Inline>),
    Fallback {
        label: String,
        blocks: Vec<Block>,
        inlines: Vec<Inline>,
    },
}

#[derive(Clone, Copy)]
enum InlineKind {
    Emphasis,
    Strong,
    Strikethrough,
}

struct Builder {
    stack: Vec<Frame>,
}

impl Builder {
    fn new() -> Self {
        Self {
            stack: vec![Frame::Root(Vec::new())],
        }
    }

    fn build<'a>(mut self, parser: impl Iterator<Item = Event<'a>>) -> Document {
        for event in parser {
            self.event(event);
        }
        // Pulldown guarantees balanced events, but keeping this recovery path
        // makes the adapter robust to future parser extensions.
        while self.stack.len() > 1 {
            self.close_top();
        }
        match self.stack.pop() {
            Some(Frame::Root(blocks)) => Document::new(blocks),
            _ => Document::default(),
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(_) => self.close_top(),
            Event::Text(text) => {
                if let Some(Frame::Code { code, .. } | Frame::Html(code)) = self.stack.last_mut() {
                    code.push_str(&text);
                } else {
                    for inline in split_bare_urls(&text) {
                        self.append_inline(inline);
                    }
                }
            }
            Event::Code(code) => self.append_inline(Inline::Code(code.into_string())),
            Event::InlineMath(math) => {
                self.append_inline(Inline::Raw(format!("${}$", math.into_string())))
            }
            Event::DisplayMath(math) => {
                self.append_block(Block::Plain(format!("$${}$$", math.into_string())))
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.append_inline(Inline::Raw(html.into_string()))
            }
            Event::FootnoteReference(label) => {
                self.append_inline(Inline::Raw(format!("[^{}]", label)))
            }
            Event::SoftBreak => self.append_inline(Inline::SoftBreak),
            Event::HardBreak => self.append_inline(Inline::HardBreak),
            Event::Rule => self.append_block(Block::Divider),
            Event::TaskListMarker(checked) => {
                if let Some(item) = self.stack.iter_mut().rev().find_map(|frame| match frame {
                    Frame::Item(item) => Some(item),
                    _ => None,
                }) {
                    item.task = Some(checked);
                }
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        let frame = match tag {
            Tag::Paragraph => Frame::Paragraph(Vec::new()),
            Tag::Heading { level, .. } => Frame::Heading(heading_level(level), Vec::new()),
            Tag::BlockQuote(_) => Frame::Quote(Vec::new()),
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .filter(|language| !language.is_empty())
                        .map(str::to_owned),
                };
                Frame::Code {
                    language,
                    code: String::new(),
                }
            }
            Tag::HtmlBlock => Frame::Html(String::new()),
            Tag::List(start) => Frame::List {
                kind: start.map_or(ListKind::Unordered, |start| ListKind::Ordered { start }),
                items: Vec::new(),
            },
            Tag::Item => Frame::Item(ListItem::default()),
            Tag::Emphasis => Frame::Inline(InlineKind::Emphasis, Vec::new()),
            Tag::Strong => Frame::Inline(InlineKind::Strong, Vec::new()),
            Tag::Strikethrough => Frame::Inline(InlineKind::Strikethrough, Vec::new()),
            Tag::Link { dest_url, .. } => Frame::Link {
                target: dest_url.into_string(),
                label: Vec::new(),
            },
            Tag::Image { dest_url, .. } => Frame::Image {
                target: dest_url.into_string(),
                alt: Vec::new(),
            },
            Tag::Table(alignments) => Frame::Table {
                alignments: alignments.into_iter().map(table_alignment).collect(),
                header: Vec::new(),
                rows: Vec::new(),
            },
            Tag::TableHead => Frame::TableHead(Vec::new()),
            Tag::TableRow => Frame::TableRow(Vec::new()),
            Tag::TableCell => Frame::TableCell(Vec::new()),
            Tag::FootnoteDefinition(label) => Frame::Fallback {
                label: format!("[^{}]:", label),
                blocks: Vec::new(),
                inlines: Vec::new(),
            },
            Tag::DefinitionList => Frame::Fallback {
                label: String::new(),
                blocks: Vec::new(),
                inlines: Vec::new(),
            },
            Tag::DefinitionListTitle => Frame::Inline(InlineKind::Strong, Vec::new()),
            Tag::DefinitionListDefinition => Frame::Fallback {
                label: ":".into(),
                blocks: Vec::new(),
                inlines: Vec::new(),
            },
            Tag::MetadataBlock(_) => Frame::Fallback {
                label: String::new(),
                blocks: Vec::new(),
                inlines: Vec::new(),
            },
        };
        self.stack.push(frame);
    }

    fn close_top(&mut self) {
        let Some(frame) = self.stack.pop() else {
            return;
        };
        match frame {
            Frame::Root(blocks) => self.stack.push(Frame::Root(blocks)),
            Frame::Paragraph(content) => self.append_block(Block::Paragraph(content)),
            Frame::Heading(level, content) => self.append_block(Block::Heading { level, content }),
            Frame::Quote(blocks) => self.append_block(Block::BlockQuote(blocks)),
            Frame::Code { language, code } => {
                self.append_block(Block::CodeBlock(CodeBlock { language, code }))
            }
            Frame::Html(html) => self.append_block(Block::Plain(html)),
            Frame::List { kind, items } => self.append_block(Block::List(List { kind, items })),
            Frame::Item(item) => {
                if let Some(Frame::List { items, .. }) = self.stack.last_mut() {
                    items.push(item);
                } else {
                    self.append_block(Block::List(List::unordered(vec![item])));
                }
            }
            Frame::Inline(kind, content) => self.append_inline(match kind {
                InlineKind::Emphasis => Inline::Emphasis(content),
                InlineKind::Strong => Inline::Strong(content),
                InlineKind::Strikethrough => Inline::Strikethrough(content),
            }),
            Frame::Link { target, label } => self.append_inline(Inline::Link { label, target }),
            Frame::Image { target, alt } => {
                let mut label = vec![Inline::Raw("[image: ".into())];
                label.extend(alt);
                label.push(Inline::Raw("]".into()));
                self.append_inline(Inline::Link { label, target });
            }
            Frame::Table {
                alignments,
                header,
                rows,
            } => self.append_block(Block::Table(Table {
                alignments,
                header,
                rows,
            })),
            Frame::TableHead(cells) => {
                if let Some(Frame::Table { header, .. }) = self.stack.last_mut() {
                    *header = cells;
                }
            }
            Frame::TableRow(cells) => {
                if let Some(Frame::Table { rows, .. }) = self.stack.last_mut() {
                    rows.push(cells);
                }
            }
            Frame::TableCell(content) => {
                if let Some(parent) = self.stack.last_mut() {
                    match parent {
                        Frame::TableHead(cells) | Frame::TableRow(cells) => cells.push(content),
                        _ => self.append_inline(Inline::Raw(cell_plain(&content))),
                    }
                }
            }
            Frame::Fallback {
                label,
                blocks,
                inlines,
            } => {
                if !inlines.is_empty() {
                    let mut content = vec![Inline::Raw(label)];
                    content.extend(inlines);
                    self.append_block(Block::Paragraph(content));
                } else if blocks.is_empty() {
                    if !label.is_empty() {
                        self.append_block(Block::Plain(label));
                    }
                } else {
                    if !label.is_empty() {
                        self.append_block(Block::Plain(label));
                    }
                    for block in blocks {
                        self.append_block(block);
                    }
                }
            }
        }
    }

    fn append_inline(&mut self, inline: Inline) {
        let Some(parent) = self.stack.last_mut() else {
            return;
        };
        match parent {
            Frame::Paragraph(content)
            | Frame::Heading(_, content)
            | Frame::Inline(_, content)
            | Frame::TableCell(content) => content.push(inline),
            Frame::Link { label, .. } => label.push(inline),
            Frame::Image { alt, .. } => alt.push(inline),
            Frame::Fallback { inlines, .. } => inlines.push(inline),
            Frame::Code { code, .. } | Frame::Html(code) => code.push_str(&inline_to_text(&inline)),
            // pulldown-cmark intentionally omits Paragraph tags for tight list
            // items. Merge adjacent inline events into one implicit paragraph.
            Frame::Item(item) => {
                if let Some(Block::Paragraph(content)) = item.blocks.last_mut() {
                    content.push(inline);
                } else {
                    item.blocks.push(Block::Paragraph(vec![inline]));
                }
            }
            Frame::Root(blocks) | Frame::Quote(blocks) => {
                if let Some(Block::Paragraph(content)) = blocks.last_mut() {
                    content.push(inline);
                } else {
                    blocks.push(Block::Paragraph(vec![inline]));
                }
            }
            _ => self.append_block(Block::Paragraph(vec![inline])),
        }
    }

    fn append_block(&mut self, block: Block) {
        let Some(parent) = self.stack.last_mut() else {
            return;
        };
        match parent {
            Frame::Root(blocks) | Frame::Quote(blocks) | Frame::Fallback { blocks, .. } => {
                blocks.push(block)
            }
            Frame::Item(item) => item.blocks.push(block),
            _ => {
                // Malformed nesting should remain visible rather than vanish.
                let text = Document::new(vec![block]).plain_text();
                self.append_inline(Inline::Raw(text));
            }
        }
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn table_alignment(alignment: Alignment) -> TableAlignment {
    match alignment {
        Alignment::None => TableAlignment::None,
        Alignment::Left => TableAlignment::Left,
        Alignment::Center => TableAlignment::Center,
        Alignment::Right => TableAlignment::Right,
    }
}

fn split_bare_urls(text: &str) -> Vec<Inline> {
    let mut output = Vec::new();
    let mut cursor = 0usize;
    let mut offsets = text
        .match_indices("http://")
        .chain(text.match_indices("https://"))
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    offsets.sort_unstable();
    offsets.dedup();
    for offset in offsets {
        if offset < cursor
            || (offset > 0
                && !text[..offset]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace))
        {
            continue;
        }
        let end = text[offset..]
            .find(char::is_whitespace)
            .map_or(text.len(), |relative| offset + relative);
        let mut url_end = end;
        while url_end > offset
            && text[..url_end]
                .chars()
                .next_back()
                .is_some_and(|character| matches!(character, '.' | ',' | ';' | ':' | '!' | '?'))
        {
            url_end -= text[..url_end].chars().next_back().unwrap().len_utf8();
        }
        if offset > cursor {
            output.push(Inline::Text(text[cursor..offset].into()));
        }
        let url = text[offset..url_end].to_owned();
        output.push(Inline::Link {
            label: vec![Inline::Text(url.clone())],
            target: url,
        });
        cursor = url_end;
    }
    if cursor < text.len() {
        output.push(Inline::Text(text[cursor..].into()));
    }
    if output.is_empty() {
        output.push(Inline::Text(text.into()));
    }
    output
}

fn inline_to_text(inline: &Inline) -> String {
    Document::new(vec![Block::Paragraph(vec![inline.clone()])]).plain_text()
}

fn cell_plain(content: &[Inline]) -> String {
    Document::new(vec![Block::Paragraph(content.to_vec())])
        .plain_text()
        .trim_end()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_markdown_constructs() {
        let document = parse(
            "# Heading\n\nParagraph with **strong**, *emphasis*, ~~old~~, `code`, and [docs](https://example.com).\n\n> quote\n\n1. first\n   - nested\n2. second\n\n```rust\nfn main() {}\n```\n\n---",
        );
        assert!(matches!(
            document.blocks[0],
            Block::Heading { level: 1, .. }
        ));
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block, Block::BlockQuote(_))));
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block, Block::List(_))));
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block, Block::CodeBlock(_))));
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Divider)));
        let plain = document.plain_text();
        for text in [
            "Heading", "strong", "emphasis", "old", "code", "quote", "nested",
        ] {
            assert!(plain.contains(text), "missing {text:?}: {plain}");
        }
        assert!(!plain.contains("**"));
    }

    #[test]
    fn tight_list_inline_runs_stay_in_one_paragraph() {
        let document = parse("- before **strong** `code` after");
        let Block::List(list) = &document.blocks[0] else {
            panic!("expected list");
        };
        assert_eq!(list.items[0].blocks.len(), 1);
        assert_eq!(document.plain_text(), "- before strong code after\n");
    }

    #[test]
    fn parses_tables_tasks_autolinks_and_escaped_markers() {
        let document = parse(
            "- [x] done\n- [ ] todo\n\n| Key | Value |\n| --- | ---: |\n| a | 1 |\n\nhttps://example.com and \\*literal\\*",
        );
        let list = document
            .blocks
            .iter()
            .find_map(|block| match block {
                Block::List(list) => Some(list),
                _ => None,
            })
            .unwrap();
        assert_eq!(list.items[0].task, Some(true));
        assert_eq!(list.items[1].task, Some(false));
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Table(_))));
        let plain = document.plain_text();
        assert!(plain.contains("https://example.com"));
        assert!(plain.contains("*literal*"));
    }

    #[test]
    fn html_like_and_malformed_markdown_remain_visible() {
        let source = "<thinking>visible</thinking>\n\n**unfinished `code [link](x";
        let plain = parse(source).plain_text();
        assert!(plain.contains("thinking"));
        assert!(plain.contains("visible"));
        assert!(plain.contains("unfinished"));
    }
}
