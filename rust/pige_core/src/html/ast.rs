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
