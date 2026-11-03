//! The native mail-document AST: a safe, renderable subset of HTML.
//!
//! The parser emits a flat list of block elements; inline formatting is carried
//! on each text run rather than as a nested tree, which keeps the renderer and
//! the JSON cache simple. Every type derives `PartialEq`/`Eq` so snapshots and
//! tests can compare documents directly.

/// A parsed mail body: a sequence of block-level elements.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MailDocument {
    pub blocks: Vec<Block>,
}

impl MailDocument {
    /// Whether any part of the document references a remote image. The reader
    /// shows these as placeholders until the user opts in to remote content.
    pub fn has_remote_images(&self) -> bool {
        self.blocks.iter().any(Block::has_remote_image)
    }

    /// Whether the document has no blocks.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Plain visible text for search indexing. Walks every nested block and
    /// inline run, including link labels, list items, quotes, table cells, and
    /// image alt text. Styling and markup are discarded; this is never raw
    /// HTML.
    pub fn visible_text(&self) -> String {
        let mut parts = Vec::new();
        for block in &self.blocks {
            collect_block_text(block, &mut parts);
        }
        parts
            .into_iter()
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn collect_block_text(block: &Block, parts: &mut Vec<String>) {
    match block {
        Block::Paragraph(inlines)
        | Block::Heading {
            content: inlines, ..
        } => {
            let text = inline_text(inlines);
            if !text.trim().is_empty() {
                parts.push(text);
            }
        }
        Block::List { items, .. } => {
            for item in items {
                for block in item {
                    collect_block_text(block, parts);
                }
            }
        }
        Block::Quote(blocks) => {
            for block in blocks {
                collect_block_text(block, parts);
            }
        }
        Block::Pre(text) => {
            if !text.trim().is_empty() {
                parts.push(text.clone());
            }
        }
        Block::Rule => {}
        Block::Table(rows) => {
            for row in rows {
                let cells: Vec<String> = row
                    .iter()
                    .map(|cell| inline_text(cell))
                    .filter(|cell| !cell.trim().is_empty())
                    .collect();
                if !cells.is_empty() {
                    parts.push(cells.join(" "));
                }
            }
        }
        Block::Image { alt, .. } => {
            if !alt.trim().is_empty() {
                parts.push(alt.clone());
            }
        }
    }
}

fn inline_text(inlines: &[Inline]) -> String {
    let mut text = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { text: run, .. } => text.push_str(run),
            Inline::Link { content, .. } => text.push_str(&inline_text(content)),
            Inline::Image { alt, .. } => text.push_str(alt),
        }
    }
    text
}

/// A block-level element in a mail body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A paragraph of inline content.
    Paragraph(Vec<Inline>),
    /// A heading; `level` is 1–6.
    Heading { level: u8, content: Vec<Inline> },
    /// An ordered or unordered list; each item is itself a sequence of blocks.
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    /// A quoted block, rendered with a left rule and muted text.
    Quote(Vec<Block>),
    /// Preformatted, monospaced text; whitespace is preserved.
    Pre(String),
    /// A horizontal rule.
    Rule,
    /// A table of rows of cells of inline content.
    Table(Vec<Vec<Vec<Inline>>>),
    /// An image. Remote images are placeholders until the user allows them;
    /// `cid:` images stay local and are resolved to bytes later.
    Image { src: ImageSource, alt: String },
}

impl Block {
    fn has_remote_image(&self) -> bool {
        match self {
            Block::Paragraph(inlines)
            | Block::Heading {
                content: inlines, ..
            } => inlines.iter().any(Inline::has_remote_image),
            Block::List { items, .. } => items.iter().flatten().any(Block::has_remote_image),
            Block::Quote(blocks) => blocks.iter().any(Block::has_remote_image),
            Block::Table(rows) => rows
                .iter()
                .flatten()
                .flatten()
                .any(Inline::has_remote_image),
            Block::Image {
                src: ImageSource::Remote(_),
                ..
            } => true,
            _ => false,
        }
    }
}

/// A run of inline content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    /// A run of text with the given style.
    Text { text: String, style: InlineStyle },
    /// A hyperlink. The sanitizer guarantees `href` is `http`, `https`, or
    /// `mailto`.
    Link { content: Vec<Inline>, href: String },
    /// An inline image.
    Image { src: ImageSource, alt: String },
}

impl Inline {
    fn has_remote_image(&self) -> bool {
        match self {
            Inline::Image {
                src: ImageSource::Remote(_),
                ..
            } => true,
            Inline::Link { content, .. } => content.iter().any(Inline::has_remote_image),
            Inline::Text { .. } | Inline::Image { .. } => false,
        }
    }
}

/// Where an image's bytes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// An `http(s)` URL. Blocked until the user opts in to remote content.
    Remote(String),
    /// A `cid:` reference into the message's own attachments.
    Cid(String),
}

/// The inline formatting applied to a run of text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
}

impl InlineStyle {
    /// Combine two styles; nesting `<b><i>` yields a bold-italic run.
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            bold: self.bold || other.bold,
            italic: self.italic || other.italic,
            underline: self.underline || other.underline,
            strike: self.strike || other.strike,
            code: self.code || other.code,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Inline {
        Inline::Text {
            text: value.to_string(),
            style: InlineStyle::default(),
        }
    }

    #[test]
    fn visible_text_walks_nested_blocks_and_link_labels() {
        let document = MailDocument {
            blocks: vec![
                Block::Heading {
                    level: 1,
                    content: vec![text("Report")],
                },
                Block::Paragraph(vec![
                    text("Read the "),
                    Inline::Link {
                        content: vec![text("details")],
                        href: "https://example.com".into(),
                    },
                ]),
                Block::Quote(vec![Block::Paragraph(vec![text("quoted line")])]),
                Block::List {
                    ordered: false,
                    items: vec![vec![Block::Paragraph(vec![text("first item")])]],
                },
                Block::Table(vec![vec![vec![text("cell one")], vec![text("cell two")]]]),
                Block::Pre("preformatted".into()),
                Block::Image {
                    src: ImageSource::Remote("https://example.com/x.png".into()),
                    alt: "chart".into(),
                },
                Block::Rule,
            ],
        };

        let text = document.visible_text();
        for expected in [
            "Report",
            "Read the details",
            "quoted line",
            "first item",
            "cell one cell two",
            "preformatted",
            "chart",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text:?}");
        }
        assert!(!text.contains("https://example.com"));
    }
}
