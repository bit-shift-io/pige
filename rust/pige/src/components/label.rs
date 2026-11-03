//! Single-line labels that use real font measurements to fit an ellipsis.
//!
//! Copied from qlaunch's `label.rs` (bitshift, MIT). The custom-drawn widget
//! is kept because iced's plain text widget cannot both fill a slot and
//! ellipsize with measured glyphs.

use iced::advanced::text::{Paragraph as _, Renderer as _};
use iced::advanced::{
    Layout, Widget, layout, mouse, renderer, text,
    widget::{Tree, tree},
};
use iced::{Color, Element, Fill, Length, Point, Rectangle, Size, Vector};

use crate::Message;
use crate::theme::Typography;

type Paragraph = <iced::Renderer as text::Renderer>::Paragraph;

/// A label that keeps its left edge visible. Short text is centred when
/// `centered`; text too wide for the slot is ellipsized on the right at a
/// character boundary and drawn from the left, so the start of a long name is
/// never clipped away. `shadow` draws the same glyphs behind in a contrasting
/// colour.
pub fn fitted<'a>(
    value: String,
    typography: Typography,
    color: Color,
    centered: bool,
    shadow: Option<Color>,
) -> Element<'a, Message> {
    Element::new(Label {
        value,
        typography,
        color: Some(color),
        centered,
        shadow,
    })
}

/// The width `value` needs in the given style, in the same logical units the
/// layout uses, so callers can size labels without guessing per character.
pub fn measured_width(value: &str, typography: Typography) -> f32 {
    Paragraph::with_text(
        Label {
            value: value.to_owned(),
            typography,
            color: None,
            centered: false,
            shadow: None,
        }
        .text(value),
    )
    .min_width()
}

struct Label {
    value: String,
    typography: Typography,
    /// `None` uses the renderer's default text colour.
    color: Option<Color>,
    centered: bool,
    shadow: Option<Color>,
}

#[derive(Default)]
struct State {
    paragraph: text::paragraph::Plain<Paragraph>,
    key: Option<(String, Typography, f32)>,
    /// The full value was too wide and was shortened with an ellipsis, so it
    /// must be drawn from the left even when `centered` is set.
    ellipsized: bool,
}

impl Label {
    fn text<'a>(&self, content: &'a str) -> text::Text<&'a str> {
        text::Text {
            content,
            bounds: Size::new(f32::INFINITY, self.typography.line_height),
            size: self.typography.size.into(),
            line_height: self.typography.line_height(),
            font: self.typography.font,
            align_x: text::Alignment::Default,
            align_y: iced::alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Label {
    fn size(&self) -> Size<Length> {
        Size::new(Fill, Length::Fixed(self.typography.line_height))
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(Fill, self.typography.line_height, Size::ZERO);
        let state = tree.state.downcast_mut::<State>();
        let key = (self.value.clone(), self.typography, size.width);
        if state.key.as_ref() != Some(&key) {
            let display = fit_label(&self.value, |candidate| {
                Paragraph::with_text(self.text(candidate)).min_width() <= size.width
            });
            state.ellipsized = display != self.value;
            state.paragraph.update(self.text(&display));
            state.key = Some(key);
        }
        layout::Node::new(size)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let state = tree.state.downcast_ref::<State>();
        let paragraph = state.paragraph.raw();
        let position = Point::new(
            label_x(
                bounds,
                paragraph.min_width(),
                self.centered,
                state.ellipsized,
            ),
            bounds.y,
        );
        if let Some(shadow) = self.shadow {
            let offset = if self.centered {
                Vector::new(2.0, 1.0)
            } else {
                Vector::new(1.0, 1.0)
            };
            renderer.fill_paragraph(paragraph, position + offset, shadow, clip);
        }
        renderer.fill_paragraph(
            paragraph,
            position,
            self.color.unwrap_or(style.text_color),
            clip,
        );
    }

    fn operate(
        &mut self,
        _tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        operation.text(None, layout.bounds(), &self.value);
    }
}

/// Where the fitted paragraph is drawn. Short text centres when asked; a long
/// word keeps its left edge at `bounds.x` even when centred, so the ellipsis
/// only ever eats the right side.
fn label_x(bounds: Rectangle, text_width: f32, centered: bool, ellipsized: bool) -> f32 {
    if centered && !ellipsized && text_width <= bounds.width {
        bounds.x + (bounds.width - text_width) / 2.0
    } else {
        bounds.x
    }
}

fn fit_label(value: &str, mut fits: impl FnMut(&str) -> bool) -> String {
    if fits(value) {
        return value.to_owned();
    }
    if !fits("…") {
        return String::new();
    }
    let boundaries: Vec<usize> = value.char_indices().map(|(index, _)| index).collect();
    let mut low = 0;
    let mut high = boundaries.len();
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let candidate = format!("{}…", &value[..boundaries[mid - 1]]);
        if fits(&candidate) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    format!(
        "{}…",
        &value[..if low == 0 { 0 } else { boundaries[low - 1] }]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_labels_end_with_an_ellipsis_instead_of_overflowing() {
        assert_eq!(
            fit_label("Folder name", |s| s.chars().count() <= 6),
            "Folde…"
        );
        assert_eq!(fit_label("Apps", |s| s.chars().count() <= 6), "Apps");
    }

    #[test]
    fn long_labels_keep_their_left_edge_when_centred() {
        let bounds = Rectangle::new(Point::new(100.0, 0.0), Size::new(80.0, 20.0));
        // A short label centres in the slot.
        assert_eq!(label_x(bounds, 40.0, true, false), 120.0);
        // An ellipsized label starts at the left edge, so the name is readable.
        assert_eq!(label_x(bounds, 79.0, true, true), 100.0);
        // Left-aligned labels never centre.
        assert_eq!(label_x(bounds, 40.0, false, false), 100.0);
    }

    #[test]
    fn ellipsis_fitting_handles_unicode_and_tiny_widths() {
        assert_eq!(
            fit_label("日本語のフォルダ", |s| s.chars().count() <= 4),
            "日本語…"
        );
        assert_eq!(fit_label("Folder", |s| s.is_empty()), "");
        assert_eq!(fit_label("", |_| true), "");
    }
}
