//! Render a [`MailDocument`] AST as native iced elements.
//!
//! Blocks map to wrapping text, scaled headings, indented lists, quoted blocks
//! with a left rule, monospaced preformatted text, table rows, and image
//! placeholders. Links become clickable spans that emit `on_link(url)`; remote
//! images render as blocked placeholders because no bytes are fetched.

use std::rc::Rc;

use iced::font::{Family, Style, Weight};
use iced::widget::{column, container, rich_text, row, space, span, text};
use iced::{Alignment, Color, Element, Fill, Font, Length};
use pige_core::html::{Block, ImageSource, Inline, InlineStyle, MailDocument};

use crate::components::styled_text;
use crate::message::Message;
use crate::theme::{self, TextRole, Typography};

/// The type of a rich-text link, resolved when a link span is clicked.
type LinkSpan<'a> = iced::widget::text::Span<'a, String, Font>;
/// A shared link handler, cloned into every clickable span.
type LinkHandler<'a> = Rc<dyn Fn(String) -> Message + 'a>;

/// Render a document's blocks top-to-bottom.
pub fn render_document<'a>(
    doc: &'a MailDocument,
    on_link: impl Fn(String) -> Message + 'a,
) -> Vec<Element<'a, Message>> {
    let palette = theme::active();
    let on_link: LinkHandler<'a> = Rc::new(on_link);
    doc.blocks
        .iter()
        .map(|block| render_block(block, palette.on_surface, &on_link))
        .collect()
}

fn render_block<'a>(
    block: &'a Block,
    base_color: Color,
    on_link: &LinkHandler<'a>,
) -> Element<'a, Message> {
    let palette = theme::active();
    match block {
        Block::Paragraph(inlines) => {
            container(rich(inlines, TextRole::BodyMedium, base_color, on_link))
                .width(Fill)
                .padding(palette.space.xs)
                .into()
        }
        Block::Heading { level, content } => {
            let role = match level {
                1 => TextRole::TitleLarge,
                2 => TextRole::TitleMedium,
                _ => TextRole::TitleSmall,
            };
            container(rich(content, role, base_color, on_link))
                .width(Fill)
                .padding([palette.space.sm, palette.space.xs])
                .into()
        }
        Block::List { ordered, items } => list(*ordered, items, base_color, on_link),
        Block::Quote(inner) => {
            let content = column(
                inner
                    .iter()
                    .map(|child| render_block(child, palette.on_surface_variant, on_link)),
            )
            .spacing(palette.space.xs);
            let rule = container(space())
                .width(palette.metrics.border_width * 3.0)
                .height(Fill)
                .style(move |_| container::Style {
                    background: Some(palette.primary.into()),
                    ..Default::default()
                });
            container(row![rule, content].spacing(palette.space.md))
                .width(Fill)
                .padding(palette.space.xs)
                .into()
        }
        Block::Pre(content) => pre(content),
        Block::Rule => rule(),
        Block::Table(rows) => table(rows, base_color, on_link),
        Block::Image { src, alt } => image_block(src, alt),
    }
}

/// Build one wrapping rich-text block from inline content.
fn rich<'a>(
    inlines: &'a [Inline],
    role: TextRole,
    base_color: Color,
    on_link: &LinkHandler<'a>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let typography = palette.role(role);
    let spans = inline_spans(inlines, typography, base_color);
    let handler = Rc::clone(on_link);
    rich_text(spans)
        .size(typography.size)
        .line_height(typography.line_height())
        .on_link_click(move |url| handler(url))
        .into()
}

/// Convert inline content into styled, linkable spans.
fn inline_spans<'a>(
    inlines: &'a [Inline],
    typography: Typography,
    base_color: Color,
) -> Vec<LinkSpan<'a>> {
    let palette = theme::active();
    let mut spans = Vec::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                spans.push(text_span(text, *style, typography, base_color));
            }
            Inline::Link { content, href } => {
                for child in inline_spans(content, typography, base_color) {
                    spans.push(child.link(href.clone()).color(palette.primary));
                }
            }
            Inline::Image { alt, .. } => {
                let label = if alt.is_empty() {
                    "[image]"
                } else {
                    alt.as_str()
                };
                spans.push(
                    span(label)
                        .color(palette.on_surface_variant)
                        .font(typography.font),
                );
            }
        }
    }
    spans
}

fn text_span<'a>(
    content: &'a str,
    style: InlineStyle,
    typography: Typography,
    color: Color,
) -> LinkSpan<'a> {
    let mut font = typography.font;
    if style.bold {
        font.weight = Weight::Bold;
    }
    if style.italic {
        font.style = Style::Italic;
    }
    if style.code {
        font.family = Family::Monospace;
    }
    let mut span = span(content).font(font).color(color);
    if style.underline {
        span = span.underline(true);
    }
    if style.strike {
        span = span.strikethrough(true);
    }
    span
}

fn list<'a>(
    ordered: bool,
    items: &'a [Vec<Block>],
    base_color: Color,
    on_link: &LinkHandler<'a>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let marker_role = palette.role(TextRole::BodyMedium);
    let mut col = column![].spacing(palette.space.xs).width(Fill);

    for (index, item) in items.iter().enumerate() {
        let marker = if ordered {
            format!("{}.", index + 1)
        } else {
            "•".to_string()
        };
        let marker = styled_text(marker, marker_role, palette.on_surface_variant);
        let content = column(
            item.iter()
                .map(|child| render_block(child, base_color, on_link)),
        )
        .spacing(palette.space.xxs)
        .width(Fill);

        col = col.push(
            row![marker, content]
                .spacing(palette.space.sm)
                .align_y(Alignment::Start),
        );
    }

    container(col).width(Fill).padding(palette.space.xs).into()
}

fn pre<'a>(content: &'a str) -> Element<'a, Message> {
    let palette = theme::active();
    let typography = palette.role(TextRole::BodyMedium);
    let mut font = typography.font;
    font.family = Family::Monospace;

    container(
        text(content)
            .size(typography.size)
            .line_height(typography.line_height())
            .font(font)
            .color(palette.on_surface)
            .width(Fill),
    )
    .width(Fill)
    .padding(palette.space.md)
    .style(move |_| container::Style {
        background: Some(palette.surface_container.into()),
        border: iced::border::rounded(palette.radius.sm),
        ..Default::default()
    })
    .into()
}

fn rule<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    container(space())
        .width(Fill)
        .height(palette.metrics.border_width)
        .style(move |_| container::Style {
            background: Some(palette.outline_variant.into()),
            ..Default::default()
        })
        .into()
}

fn table<'a>(
    rows: &'a [Vec<Vec<Inline>>],
    base_color: Color,
    on_link: &LinkHandler<'a>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let mut col = column![].spacing(palette.space.xs).width(Fill);

    for cells in rows {
        let mut row_el = row![].spacing(palette.space.md);
        for cell in cells {
            row_el = row_el
                .push(container(rich(cell, TextRole::BodyMedium, base_color, on_link)).width(Fill));
        }
        col = col.push(row_el);
    }

    container(col).width(Fill).padding(palette.space.xs).into()
}

fn image_block<'a>(src: &'a ImageSource, alt: &'a str) -> Element<'a, Message> {
    let palette = theme::active();
    let label = match src {
        ImageSource::Remote(_) => {
            if alt.is_empty() {
                "[image blocked]".to_string()
            } else {
                alt.to_string()
            }
        }
        ImageSource::Cid(_) => {
            if alt.is_empty() {
                "[image]".to_string()
            } else {
                alt.to_string()
            }
        }
    };

    container(styled_text(
        label,
        palette.role(TextRole::BodySmall),
        palette.on_surface_variant,
    ))
    .width(Length::Shrink)
    .padding([palette.space.sm, palette.space.md])
    .style(move |_| container::Style {
        background: Some(palette.surface_container.into()),
        border: iced::Border {
            color: palette.outline_variant,
            width: palette.metrics.border_width,
            radius: palette.radius.sm.into(),
        },
        ..Default::default()
    })
    .into()
}
