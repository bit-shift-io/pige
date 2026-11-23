//! Full-height colour strips pinned to the edges of a rounded surface.
//!
//! A strip is the surface's rounded rectangle clipped to a band hugging one
//! edge: the flush side follows the surface's corner radius while the inner
//! side stays straight, so the strip reads as part of the surface rather than
//! a floating sliver. Iced clamps a quad's border radius to half its smaller
//! dimension, which would square off a 4 dp strip; the shape is therefore
//! filled as a clipped path, which carries the full radius.

use iced::widget::canvas::{self, Canvas, Path};
use iced::{Color, Element, Point, Rectangle, Size, mouse};

use crate::{Message, theme};

/// The edge of a row surface a strip hugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// A full-height colour strip on one edge of the surface, drawn under the
/// content. The strip spans the whole surface height; its ends follow the
/// surface's corner radius.
pub fn edge_strip(color: Color, side: Side, radius: f32) -> Element<'static, Message> {
    Canvas::new(Strip {
        color,
        side,
        radius,
    })
    .width(iced::Fill)
    .height(iced::Fill)
    .into()
}

struct Strip {
    color: Color,
    side: Side,
    radius: f32,
}

impl canvas::Program<Message> for Strip {
    type State = ();

    fn draw(
        &self,
        _: &(),
        renderer: &iced::Renderer,
        _: &iced::Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let width = theme::active().metrics.edge_bar_width.min(bounds.width);
        let x = match self.side {
            Side::Left => 0.0,
            Side::Right => bounds.width - width,
        };
        let clip = Rectangle::new(Point::new(x, 0.0), Size::new(width, bounds.height));

        // Fill the whole surface silhouette and let the clip keep only the
        // edge band: the ends inherit the surface's rounded corners exactly.
        frame.with_clip(clip, |band| {
            band.fill(
                &Path::rounded_rectangle(Point::ORIGIN, bounds.size(), self.radius.into()),
                self.color,
            );
        });
        vec![frame.into_geometry()]
    }
}
