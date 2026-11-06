//! An indeterminate indicator; the shell ticks only while a body is loading.
use iced::widget::canvas::{self, Canvas, Path, Stroke};
use iced::{Element, Rectangle, Renderer, Theme, mouse};

use crate::{message::Message, theme};

struct Spinner {
    rotation: f32,
}

impl canvas::Program<Message> for Spinner {
    type State = ();

    fn draw(
        &self,
        _: &(),
        renderer: &Renderer,
        _: &Theme,
        bounds: Rectangle,
        _: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let palette = theme::active();
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let radius = bounds.width.min(bounds.height) / 2.0 - 2.0;
        let arc = Path::new(|path| {
            // A 270-degree arc with its start rotated on each tick.
            for step in 0..=48 {
                let angle = self.rotation * 0.3 + step as f32 * std::f32::consts::TAU * 0.75 / 48.0;
                let point = iced::Point::new(
                    frame.center().x + radius * angle.cos(),
                    frame.center().y + radius * angle.sin(),
                );
                if step == 0 {
                    path.move_to(point);
                } else {
                    path.line_to(point);
                }
            }
        });
        frame.stroke(
            &arc,
            Stroke::default()
                .with_color(palette.on_surface_variant)
                .with_width(2.0),
        );
        vec![frame.into_geometry()]
    }
}

pub fn spinner(rotation: f32) -> Element<'static, Message> {
    let size = theme::active().space.lg;
    Canvas::new(Spinner { rotation })
        .width(size)
        .height(size)
        .into()
}
