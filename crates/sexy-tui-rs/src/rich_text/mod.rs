//! Backend-independent rich-document model.
//!
//! Callers may construct these values directly or use [`markdown::parse`].
//! Strings are semantic data, not ANSI fragments; renderers sanitize every
//! string at the terminal boundary.

pub mod diff;
pub mod markdown;
pub mod render;
pub mod stream;

#[cfg(feature = "syntax-highlighting")]
pub(crate) mod highlight;

use crate::style::{TextRole, TextStyle};

/// A semantic document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub blocks: Vec<Block>,
}

impl Document {
    pub fn new(blocks: Vec<Block>) -> Self {
        Self { blocks }
    }

    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::new(vec![Block::Paragraph(vec![Inline::Text(text.into())])])
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Semantic copy representation without generated border decoration or
    /// styling. Call a renderer's `sanitize_copy` before terminal/log output.
    pub fn plain_text(&self) -> String {
        let mut output = String::new();
        write_blocks_plain(&self.blocks, 0, &mut output);
        while output.ends_with("\n\n") {
            output.pop();
        }
        output
    }
}

/// Block-level content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    CodeBlock(CodeBlock),
    List(List),
    BlockQuote(Vec<Block>),
    Divider,
    Table(Table),
    /// Generic disclosure block. Applications own the expanded state.
    Detail(DetailBlock),
    /// Safe fallback for unsupported or intentionally literal content.
    Plain(String),
}

/// Inline content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    /// A caller-supplied typed style.
    Styled(StyledSpan),
    /// A semantic role resolved by the active theme.
    Role {
        role: TextRole,
        content: Vec<Inline>,
    },
    /// Status with a non-color marker selected by terminal capabilities.
    Status {
        kind: StatusKind,
        content: Vec<Inline>,
    },
    Emphasis(Vec<Inline>),
    Strong(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    Link {
        label: Vec<Inline>,
        target: String,
    },
    SoftBreak,
    HardBreak,
    /// Literal fallback text. It is still sanitized and never interpreted as
    /// a terminal escape sequence.
    Raw(String),
}

impl Inline {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn role(role: TextRole, content: impl Into<String>) -> Self {
        Self::Role {
            role,
            content: vec![Self::Text(content.into())],
        }
    }

    pub fn status(kind: StatusKind, content: impl Into<String>) -> Self {
        Self::Status {
            kind,
            content: vec![Self::Text(content.into())],
        }
    }

    pub fn strong(content: impl Into<String>) -> Self {
        Self::Strong(vec![Self::Text(content.into())])
    }

    pub fn emphasis(content: impl Into<String>) -> Self {
        Self::Emphasis(vec![Self::Text(content.into())])
    }

    pub fn code(content: impl Into<String>) -> Self {
        Self::Code(content.into())
    }

    pub fn link(label: impl Into<String>, target: impl Into<String>) -> Self {
        Self::Link {
            label: vec![Self::Text(label.into())],
            target: target.into(),
        }
    }
}

/// Generic status semantics whose meaning is never color-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusKind {
    Success,
    Warning,
    Error,
    Pending,
}

/// A composable styled span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub style: TextStyle,
    pub content: Vec<Inline>,
}

impl StyledSpan {
    pub fn new(style: TextStyle, content: Vec<Inline>) -> Self {
        Self { style, content }
    }

    pub fn text(style: TextStyle, text: impl Into<String>) -> Self {
        Self::new(style, vec![Inline::Text(text.into())])
    }
}

/// Fenced or programmatically constructed code.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeBlock {
    pub language: Option<String>,
    pub code: String,
}

impl CodeBlock {
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            language: None,
            code: code.into(),
        }
    }

    pub fn with_language(language: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            language: Some(language.into()),
            code: code.into(),
        }
    }
}

/// Generic expandable/collapsible detail content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetailBlock {
    pub summary: Vec<Inline>,
    pub blocks: Vec<Block>,
    pub expanded: bool,
}

impl DetailBlock {
    pub fn new(summary: impl Into<String>, blocks: Vec<Block>, expanded: bool) -> Self {
        Self {
            summary: vec![Inline::Text(summary.into())],
            blocks,
            expanded,
        }
    }
}

/// Ordered or unordered list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct List {
    pub kind: ListKind,
    pub items: Vec<ListItem>,
}

impl List {
    pub fn unordered(items: Vec<ListItem>) -> Self {
        Self {
            kind: ListKind::Unordered,
            items,
        }
    }

    pub fn ordered(start: u64, items: Vec<ListItem>) -> Self {
        Self {
            kind: ListKind::Ordered { start },
            items,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Unordered,
    Ordered { start: u64 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListItem {
    pub blocks: Vec<Block>,
    pub task: Option<bool>,
}

impl ListItem {
    pub fn new(blocks: Vec<Block>) -> Self {
        Self { blocks, task: None }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::new(vec![Block::Paragraph(vec![Inline::Text(text.into())])])
    }

    pub fn task(checked: bool, blocks: Vec<Block>) -> Self {
        Self {
            blocks,
            task: Some(checked),
        }
    }
}

/// Width-aware table.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub alignments: Vec<TableAlignment>,
    pub header: Vec<TableCell>,
    pub rows: Vec<Vec<TableCell>>,
}

pub type TableCell = Vec<Inline>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAlignment {
    #[default]
    None,
    Left,
    Center,
    Right,
}

fn inline_plain(inlines: &[Inline], output: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) | Inline::Code(text) | Inline::Raw(text) => output.push_str(text),
            Inline::Styled(span) => inline_plain(&span.content, output),
            Inline::Role { content, .. }
            | Inline::Emphasis(content)
            | Inline::Strong(content)
            | Inline::Strikethrough(content) => inline_plain(content, output),
            Inline::Status { kind, content } => {
                output.push_str(match kind {
                    StatusKind::Success => "+ ",
                    StatusKind::Warning => "! ",
                    StatusKind::Error => "x ",
                    StatusKind::Pending => ". ",
                });
                inline_plain(content, output);
            }
            Inline::Link { label, target } => {
                let start = output.len();
                inline_plain(label, output);
                if output[start..].trim() != target.trim() {
                    output.push_str(" (");
                    output.push_str(target);
                    output.push(')');
                }
            }
            Inline::SoftBreak => output.push(' '),
            Inline::HardBreak => output.push('\n'),
        }
    }
}

fn write_blocks_plain(blocks: &[Block], indent: usize, output: &mut String) {
    for (block_index, block) in blocks.iter().enumerate() {
        match block {
            Block::Paragraph(content) | Block::Heading { content, .. } => {
                output.push_str(&" ".repeat(indent));
                inline_plain(content, output);
                output.push('\n');
            }
            Block::CodeBlock(code) => {
                output.push_str(&code.code);
                if !code.code.ends_with('\n') {
                    output.push('\n');
                }
            }
            Block::List(list) => {
                for (index, item) in list.items.iter().enumerate() {
                    let marker = match list.kind {
                        ListKind::Unordered => "- ".to_owned(),
                        ListKind::Ordered { start } => format!("{}. ", start + index as u64),
                    };
                    output.push_str(&" ".repeat(indent));
                    output.push_str(&marker);
                    if let Some(checked) = item.task {
                        output.push_str(if checked { "[x] " } else { "[ ] " });
                    }
                    if let Some(Block::Paragraph(content)) = item.blocks.first() {
                        inline_plain(content, output);
                        output.push('\n');
                        write_blocks_plain(&item.blocks[1..], indent + marker.len(), output);
                    } else {
                        output.push('\n');
                        write_blocks_plain(&item.blocks, indent + marker.len(), output);
                    }
                }
            }
            Block::BlockQuote(inner) => {
                let mut quote = String::new();
                write_blocks_plain(inner, 0, &mut quote);
                for line in quote.lines() {
                    output.push_str(&" ".repeat(indent));
                    output.push_str("> ");
                    output.push_str(line);
                    output.push('\n');
                }
            }
            Block::Divider => {
                output.push_str(&" ".repeat(indent));
                output.push_str("---\n");
            }
            Block::Table(table) => {
                let mut write_row = |row: &[TableCell]| {
                    for (index, cell) in row.iter().enumerate() {
                        if index > 0 {
                            output.push('\t');
                        }
                        inline_plain(cell, output);
                    }
                    output.push('\n');
                };
                write_row(&table.header);
                for row in &table.rows {
                    write_row(row);
                }
            }
            Block::Detail(detail) => {
                output.push_str(&" ".repeat(indent));
                inline_plain(&detail.summary, output);
                output.push('\n');
                // Copy text includes hidden detail content so visual collapse
                // never causes data loss.
                write_blocks_plain(&detail.blocks, indent + 2, output);
            }
            Block::Plain(text) => {
                output.push_str(&" ".repeat(indent));
                output.push_str(text);
                if !text.ends_with('\n') {
                    output.push('\n');
                }
            }
        }
        if block_index + 1 < blocks.len()
            && matches!(
                block,
                Block::Heading { .. } | Block::CodeBlock(_) | Block::Divider
            )
        {
            output.push('\n');
        }
    }
}
