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

/// The body spinner: a small indeterminate arc sized from the palette.
pub fn spinner(rotation: f32) -> Element<'static, Message> {
    spinner_sized(rotation, theme::active().space.lg)
}

/// A spinner sized for an app-bar/toolbar slot: it fills a touch target so
/// swapping it for an icon button never changes the bar layout. Shared so the
/// list app bar and any future toolbar show the same shape.
pub fn toolbar_spinner(rotation: f32) -> Element<'static, Message> {
    let palette = theme::active();
    iced::widget::container(spinner_sized(rotation, palette.metrics.icon_size))
        .center(palette.metrics.touch_target)
        .into()
}

/// An indeterminate spinner with an explicit diameter.
fn spinner_sized(rotation: f32, size: f32) -> Element<'static, Message> {
    Canvas::new(Spinner { rotation })
        .width(size)
        .height(size)
        .into()
}

#[cfg(test)]
mod tests {
    /// The toolbar spinner must occupy exactly one touch target, so swapping it
    /// for the refresh icon button never changes the app bar layout.
    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn toolbar_spinner_fills_a_touch_target() {
        use crate::theme;
        use iced::Size;
        use iced::advanced::{layout, renderer::Headless, widget::Tree};

        let renderer = futures::executor::block_on(<iced::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            16.0.into(),
            None,
        ))
        .expect("headless wgpu adapter");
        let mut element = super::toolbar_spinner(0.0);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0)),
        );
        let target = theme::active().metrics.touch_target;
        assert_eq!(node.size().width, target);
        assert_eq!(node.size().height, target);
    }
}
