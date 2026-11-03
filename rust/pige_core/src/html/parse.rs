//! HTML/plain-text to [`MailDocument`] conversion.
//!
//! [`parse_html`] runs the input through the allowlist sanitizer and then
//! parses the result with `html5ever`, walking the DOM into a native AST. The
//! walk enforces a maximum nesting depth and node count so hostile input cannot
//! exhaust the stack or spend unbounded time in the builder.

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

use super::ast::{Block, ImageSource, Inline, InlineStyle, MailDocument};
use super::sanitize::sanitize;

/// Maximum DOM nesting depth the AST builder descends.
const MAX_DEPTH: u32 = 64;
/// Maximum number of DOM nodes the AST builder visits.
const MAX_NODES: usize = 10_000;

/// Parse an HTML mail body into a native document.
///
/// `allow_remote_images` controls remote images. When `false` (the safe
/// default) an `http(s)` image is kept as [`Block::Image`] with
/// [`ImageSource::Remote`] so the reader can show a placeholder and a "Load
/// images" prompt. When `true`, remote images are dropped: the pipeline never
/// fetches remote bytes, so an "allowed" remote image has nothing to render.
/// `cid:` images are preserved in both modes; their bytes resolve later.
pub fn parse_html(input: &str, allow_remote_images: bool) -> MailDocument {
    let clean = sanitize(input);
    if clean.is_empty() {
        return MailDocument::default();
    }

    let dom = parse_document(RcDom::default(), ParseOpts::default()).one(clean.as_str());
    let body = body_of(&dom.document);
    let mut ctx = Ctx {
        allow_remote_images,
        nodes: 0,
    };
    MailDocument {
        blocks: blocks_from_children(&body, 0, &mut ctx),
    }
}

/// Turn a plain-text body into a document: blank lines separate paragraphs,
/// and whitespace within a paragraph is collapsed.
pub fn plain_text_document(input: &str) -> MailDocument {
    let mut blocks = Vec::new();
    let mut current = String::new();
    let flush = |current: &mut String, blocks: &mut Vec<Block>| {
        let text = current.split_whitespace().collect::<Vec<_>>().join(" ");
        current.clear();
        if !text.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text {
                text,
                style: InlineStyle::default(),
            }]));
        }
    };

    for line in input.replace("\r\n", "\n").split('\n') {
        if line.trim().is_empty() {
            flush(&mut current, &mut blocks);
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(line.trim());
        }
    }
    flush(&mut current, &mut blocks);

    MailDocument { blocks }
}

/// Builder state: image policy and the node budget.
struct Ctx {
    allow_remote_images: bool,
    nodes: usize,
}

/// Find the `<body>` element of a parsed document, falling back to the
/// document root when the body is absent (empty input).
fn body_of(document: &Handle) -> Handle {
    for child in document.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data
            && &*name.local == "html"
        {
            for grandchild in child.children.borrow().iter() {
                if let NodeData::Element { name, .. } = &grandchild.data
                    && &*name.local == "body"
                {
                    return grandchild.clone();
                }
            }
        }
    }
    document.clone()
}

/// Whether `tag` produces a block-level element at the top of the tree.
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "blockquote"
            | "pre"
            | "hr"
            | "table"
            | "div"
    )
}

/// Whether `tag` is inline content (grouped into paragraphs at block level).
fn is_inline_tag(tag: &str) -> bool {
    matches!(
        tag,
        "b" | "strong"
            | "i"
            | "em"
            | "u"
            | "s"
            | "strike"
            | "small"
            | "sub"
            | "sup"
            | "code"
            | "a"
            | "span"
            | "br"
    )
}

/// Group a node's children into blocks, accumulating inline content into
/// paragraphs and handling block elements directly.
fn blocks_from_children(node: &Handle, depth: u32, ctx: &mut Ctx) -> Vec<Block> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }

    let mut blocks = Vec::new();
    let mut pending: Vec<Inline> = Vec::new();
    for child in node.children.borrow().iter() {
        ctx.nodes += 1;
        if ctx.nodes > MAX_NODES {
            break;
        }
        match &child.data {
            NodeData::Text { contents } => {
                pending.extend(text_inline(&contents.borrow(), InlineStyle::default()));
            }
            NodeData::Element { name, .. } => {
                let tag = &*name.local;
                if is_block_tag(tag) {
                    flush(&mut pending, &mut blocks);
                    blocks.extend(block_from_element(child, tag, depth, ctx));
                } else if tag == "img" {
                    // A top-level image is a block image.
                    flush(&mut pending, &mut blocks);
                    blocks.extend(image_block(child, ctx));
                } else if is_inline_tag(tag) {
                    pending.extend(inline_from_node(child, depth, ctx, InlineStyle::default()));
                } else {
                    // Unknown or stray structural element: flatten to children.
                    flush(&mut pending, &mut blocks);
                    blocks.extend(blocks_from_children(child, depth + 1, ctx));
                }
            }
            _ => {}
        }
    }
    flush(&mut pending, &mut blocks);
    blocks
}

/// Produce the block(s) for a block-level element.
fn block_from_element(node: &Handle, tag: &str, depth: u32, ctx: &mut Ctx) -> Vec<Block> {
    match tag {
        "p" => vec![Block::Paragraph(inline_from_children(
            node,
            depth + 1,
            ctx,
            InlineStyle::default(),
        ))],
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag.as_bytes()[1] - b'0';
            vec![Block::Heading {
                level,
                content: inline_from_children(node, depth + 1, ctx, InlineStyle::default()),
            }]
        }
        "ul" => vec![Block::List {
            ordered: false,
            items: list_items(node, depth + 1, ctx),
        }],
        "ol" => vec![Block::List {
            ordered: true,
            items: list_items(node, depth + 1, ctx),
        }],
        "blockquote" => vec![Block::Quote(blocks_from_children(node, depth + 1, ctx))],
        "pre" => {
            let mut text = String::new();
            collect_pre(node, depth + 1, ctx, &mut text);
            vec![Block::Pre(text)]
        }
        "hr" => vec![Block::Rule],
        "table" => vec![Block::Table(table_rows(node, depth + 1, ctx))],
        // `div` and any unknown block are transparent: their children become
        // blocks directly.
        _ => blocks_from_children(node, depth + 1, ctx),
    }
}

/// Inline content of an element's children, applying `style` to text runs.
fn inline_from_children(
    node: &Handle,
    depth: u32,
    ctx: &mut Ctx,
    style: InlineStyle,
) -> Vec<Inline> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }
    let mut out = Vec::new();
    for child in node.children.borrow().iter() {
        out.extend(inline_from_node(child, depth, ctx, style));
    }
    out
}

/// Inline content of a single node.
fn inline_from_node(node: &Handle, depth: u32, ctx: &mut Ctx, style: InlineStyle) -> Vec<Inline> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }
    ctx.nodes += 1;
    if ctx.nodes > MAX_NODES {
        return Vec::new();
    }

    match &node.data {
        NodeData::Text { contents } => text_inline(&contents.borrow(), style),
        NodeData::Element { name, attrs, .. } => {
            let tag = &*name.local;
            let child_depth = depth + 1;
            match tag {
                "b" | "strong" => inline_from_children(node, child_depth, ctx, style.merge(bold())),
                "i" | "em" => inline_from_children(node, child_depth, ctx, style.merge(italic())),
                "u" => inline_from_children(node, child_depth, ctx, style.merge(underline())),
                "s" | "strike" => {
                    inline_from_children(node, child_depth, ctx, style.merge(strike()))
                }
                "code" => inline_from_children(node, child_depth, ctx, style.merge(code())),
                // Neutral inline elements keep the current style.
                "small" | "sub" | "sup" | "span" => {
                    inline_from_children(node, child_depth, ctx, style)
                }
                "a" => {
                    let content = inline_from_children(node, child_depth, ctx, style);
                    match sanitize_href(attr(attrs, "href").as_deref()) {
                        Some(href) => vec![Inline::Link { content, href }],
                        None => content,
                    }
                }
                "img" => image_inline(attrs, ctx),
                "br" => vec![Inline::Text {
                    text: "\n".to_string(),
                    style,
                }],
                // Unknown inline element: flatten to its children.
                _ => inline_from_children(node, child_depth, ctx, style),
            }
        }
        _ => Vec::new(),
    }
}

/// A block-level image, or nothing when the source is dropped.
fn image_block(node: &Handle, ctx: &Ctx) -> Vec<Block> {
    if let NodeData::Element { attrs, .. } = &node.data
        && let Some((src, alt)) = image_source(attrs, ctx.allow_remote_images)
    {
        return vec![Block::Image { src, alt }];
    }
    Vec::new()
}

/// An inline image, or nothing when the source is dropped.
fn image_inline(attrs: &std::cell::RefCell<Vec<html5ever::Attribute>>, ctx: &Ctx) -> Vec<Inline> {
    image_source(attrs, ctx.allow_remote_images)
        .map(|(src, alt)| vec![Inline::Image { src, alt }])
        .unwrap_or_default()
}

/// Resolve an `<img>` to an [`ImageSource`] plus alt text, applying the remote
/// image policy.
fn image_source(
    attrs: &std::cell::RefCell<Vec<html5ever::Attribute>>,
    allow_remote: bool,
) -> Option<(ImageSource, String)> {
    let src = attr(attrs, "src")?;
    let alt = attr(attrs, "alt").unwrap_or_default();

    if src.len() > 4 && src[..4].eq_ignore_ascii_case("cid:") {
        return Some((ImageSource::Cid(src[4..].to_string()), alt));
    }

    let lower = src.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if allow_remote {
            // No bytes are fetched, so an allowed remote image has nothing to
            // render; it is dropped (see the module docs).
            return None;
        }
        return Some((ImageSource::Remote(src), alt));
    }

    // Relative, `mailto:`, and any other scheme are not renderable.
    None
}

/// Keep only `http`, `https`, and `mailto` link targets.
fn sanitize_href(href: Option<&str>) -> Option<String> {
    let href = href?;
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
    {
        Some(href.to_string())
    } else {
        None
    }
}

/// Read one attribute's value by local name.
fn attr(attrs: &std::cell::RefCell<Vec<html5ever::Attribute>>, name: &str) -> Option<String> {
    attrs
        .borrow()
        .iter()
        .find(|attribute| &*attribute.name.local == name)
        .map(|attribute| attribute.value.to_string())
}

/// A text run, with whitespace runs collapsed to single spaces.
fn text_inline(contents: &str, style: InlineStyle) -> Vec<Inline> {
    let text = collapse_whitespace(contents);
    if text.is_empty() {
        Vec::new()
    } else {
        vec![Inline::Text { text, style }]
    }
}

/// Collapse each run of whitespace to a single space without trimming, so
/// adjacent nodes keep their word separators.
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_whitespace = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !in_whitespace {
                out.push(' ');
                in_whitespace = true;
            }
        } else {
            out.push(ch);
            in_whitespace = false;
        }
    }
    out
}

/// Append a paragraph from pending inline content, skipping whitespace-only
/// runs and trimming the ends.
fn flush(pending: &mut Vec<Inline>, blocks: &mut Vec<Block>) {
    if pending.is_empty() {
        return;
    }
    trim_inlines(pending);
    if pending.iter().all(inline_is_blank) {
        pending.clear();
        return;
    }
    blocks.push(Block::Paragraph(std::mem::take(pending)));
}

fn inline_is_blank(inline: &Inline) -> bool {
    match inline {
        Inline::Text { text, .. } => text.trim().is_empty(),
        Inline::Link { content, .. } => content.iter().all(inline_is_blank),
        Inline::Image { .. } => false,
    }
}

fn trim_inlines(inlines: &mut Vec<Inline>) {
    if let Some(Inline::Text { text, .. }) = inlines.first_mut() {
        *text = text.trim_start().to_string();
    }
    if let Some(Inline::Text { text, .. }) = inlines.last_mut() {
        *text = text.trim_end().to_string();
    }
    inlines.retain(|inline| !matches!(inline, Inline::Text { text, .. } if text.is_empty()));
}

/// List items: each direct `li` child contributes its block children.
fn list_items(node: &Handle, depth: u32, ctx: &mut Ctx) -> Vec<Vec<Block>> {
    let mut items = Vec::new();
    for child in node.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data
            && &*name.local == "li"
        {
            items.push(blocks_from_children(child, depth, ctx));
        }
    }
    items
}

/// Table rows, descending through `thead`/`tbody`/`tfoot`.
fn table_rows(node: &Handle, depth: u32, ctx: &mut Ctx) -> Vec<Vec<Vec<Inline>>> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for child in node.children.borrow().iter() {
        ctx.nodes += 1;
        if ctx.nodes > MAX_NODES {
            break;
        }
        if let NodeData::Element { name, .. } = &child.data {
            let tag = &*name.local;
            if tag == "tr" {
                rows.push(table_cells(child, depth + 1, ctx));
            } else if matches!(tag, "thead" | "tbody" | "tfoot") {
                rows.extend(table_rows(child, depth + 1, ctx));
            }
        }
    }
    rows
}

/// One table row's cells as inline content.
fn table_cells(row: &Handle, depth: u32, ctx: &mut Ctx) -> Vec<Vec<Inline>> {
    let mut cells = Vec::new();
    for child in row.children.borrow().iter() {
        if let NodeData::Element { name, .. } = &child.data
            && matches!(&*name.local, "td" | "th")
        {
            cells.push(inline_from_children(
                child,
                depth,
                ctx,
                InlineStyle::default(),
            ));
        }
    }
    cells
}

/// Preformatted text, preserving whitespace and translating `<br>` to newlines.
fn collect_pre(node: &Handle, depth: u32, ctx: &mut Ctx, out: &mut String) {
    if depth >= MAX_DEPTH {
        return;
    }
    for child in node.children.borrow().iter() {
        ctx.nodes += 1;
        if ctx.nodes > MAX_NODES {
            return;
        }
        match &child.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { name, .. } => {
                if &*name.local == "br" {
                    out.push('\n');
                } else {
                    collect_pre(child, depth + 1, ctx, out);
                }
            }
            _ => {}
        }
    }
}

fn bold() -> InlineStyle {
    InlineStyle {
        bold: true,
        ..InlineStyle::default()
    }
}

fn italic() -> InlineStyle {
    InlineStyle {
        italic: true,
        ..InlineStyle::default()
    }
}

fn underline() -> InlineStyle {
    InlineStyle {
        underline: true,
        ..InlineStyle::default()
    }
}

fn strike() -> InlineStyle {
    InlineStyle {
        strike: true,
        ..InlineStyle::default()
    }
}

fn code() -> InlineStyle {
    InlineStyle {
        code: true,
        ..InlineStyle::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test-only plain-text flattening of a document, used to assert the
    /// sanitizer and parser outputs without depending on the renderer.
    fn render_plain(doc: &MailDocument) -> String {
        let mut out = String::new();
        render_blocks(&doc.blocks, &mut out);
        out
    }

    fn render_blocks(blocks: &[Block], out: &mut String) {
        for block in blocks {
            match block {
                Block::Paragraph(inlines) => {
                    render_inlines(inlines, out);
                    out.push('\n');
                }
                Block::Heading { content, .. } => {
                    render_inlines(content, out);
                    out.push('\n');
                }
                Block::List { items, .. } => {
                    for item in items {
                        out.push_str("- ");
                        render_blocks(item, out);
                    }
                }
                Block::Quote(blocks) => {
                    out.push_str("> ");
                    render_blocks(blocks, out);
                }
                Block::Pre(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::Rule => out.push('\n'),
                Block::Table(rows) => {
                    for row in rows {
                        for cell in row {
                            render_inlines(cell, out);
                            out.push(' ');
                        }
                        out.push('\n');
                    }
                }
                Block::Image {
                    src: ImageSource::Remote(_),
                    ..
                } => out.push_str("[image blocked]"),
                Block::Image {
                    src: ImageSource::Cid(_),
                    ..
                } => out.push_str("[image]"),
            }
        }
    }

    fn render_inlines(inlines: &[Inline], out: &mut String) {
        for inline in inlines {
            match inline {
                Inline::Text { text, .. } => out.push_str(text),
                Inline::Link { content, .. } => render_inlines(content, out),
                Inline::Image {
                    src: ImageSource::Remote(_),
                    ..
                } => out.push_str("[image blocked]"),
                Inline::Image {
                    src: ImageSource::Cid(_),
                    ..
                } => out.push_str("[image]"),
            }
        }
    }

    #[test]
    fn scripts_forms_and_iframes_are_dropped() {
        let doc = parse_html(
            "<p>hi</p><script>alert(1)</script><iframe src=x></iframe><form></form>",
            false,
        );
        assert_eq!(render_plain(&doc), "hi\n");
    }

    #[test]
    fn remote_images_are_placeholders_until_allowed() {
        let blocked = parse_html("<img src='https://tracker.example/p.gif'>", false);
        assert!(matches!(
            blocked.blocks[0],
            Block::Image {
                src: ImageSource::Remote(_),
                ..
            }
        ));
        assert!(render_plain(&blocked).contains("[image blocked]") || blocked.is_empty());
        let allowed = parse_html("<img src='https://tracker.example/p.gif'>", true);
        assert!(render_plain(&allowed).is_empty());
    }

    #[test]
    fn cid_images_are_local_and_kept() {
        let doc = parse_html("<img src='cid:logo@x'>", false);
        assert!(matches!(
            doc.blocks[0],
            Block::Image {
                src: ImageSource::Cid(_),
                ..
            }
        ));
    }

    #[test]
    fn unsafe_link_schemes_are_removed() {
        let doc = parse_html("<a href='javascript:alert(1)'>x</a>", false);
        assert_eq!(render_plain(&doc), "x\n");
    }

    #[test]
    fn deep_nesting_is_capped() {
        let html = "<div>".repeat(10_000) + "deep" + &"</div>".repeat(10_000);
        assert!(parse_html(&html, false).blocks.len() < 100);
    }

    #[test]
    fn safe_links_are_kept() {
        let doc = parse_html("<a href='https://example.com'>site</a>", false);
        let Block::Paragraph(inlines) = &doc.blocks[0] else {
            panic!("expected a paragraph");
        };
        assert_eq!(inlines.len(), 1);
        assert!(matches!(
            &inlines[0],
            Inline::Link { href, .. } if href == "https://example.com"
        ));
    }

    #[test]
    fn headings_lists_and_quotes_are_structured() {
        let doc = parse_html(
            "<h2>Title</h2><ul><li>one</li><li>two</li></ul><blockquote>cited</blockquote>",
            false,
        );
        assert!(matches!(&doc.blocks[0], Block::Heading { level: 2, .. }));
        assert!(matches!(&doc.blocks[1], Block::List { ordered: false, .. }));
        assert!(matches!(&doc.blocks[2], Block::Quote(_)));
    }

    #[test]
    fn rich_fixture_parses_without_remote_content() {
        let html = include_str!("../../tests/fixtures/html/rich.html");
        let doc = parse_html(html, false);
        assert!(!doc.blocks.is_empty());
        assert!(!doc.has_remote_images());
        assert!(render_plain(&doc).contains("Welcome"));
    }

    #[test]
    fn unsafe_fixture_strips_scripts_and_handlers() {
        let html = include_str!("../../tests/fixtures/html/unsafe.html");
        let doc = parse_html(html, false);
        let plain = render_plain(&doc);
        assert!(plain.contains("safe text"));
        assert!(!plain.contains("alert"));
        assert!(!plain.contains("onclick"));
    }

    #[test]
    fn plain_text_becomes_paragraphs() {
        let doc = plain_text_document("Hello world\n\nSecond paragraph");
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(
            doc.blocks[0],
            Block::Paragraph(vec![Inline::Text {
                text: "Hello world".to_string(),
                style: InlineStyle::default(),
            }])
        );
    }

    #[test]
    fn unknown_elements_flatten_to_children() {
        let doc = parse_html("<section><p>kept</p></section><custom>also</custom>", false);
        let plain = render_plain(&doc);
        assert!(plain.contains("kept"));
        assert!(plain.contains("also"));
    }
}
