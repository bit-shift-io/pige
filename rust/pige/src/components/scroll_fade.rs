//! Scroll-edge opacity, applied to cell primitives rather than painting over
//! the background. The scrollable supplies its translated content viewport at
//! draw time, so wheel, touch and programmatic scrolling all use fresh bounds.
//!
//! Copied from qlaunch's `scroll_fade.rs` (bitshift, MIT). qlaunch read its
//! fade style from launcher settings; pige always uses the gradient, with the
//! same enum retained for tests and future settings.

use iced::advanced::Renderer as _;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, renderer, widget::Tree};
use iced::{Element, Event, Length, Rectangle, Size};

use crate::Message;

const EXTENT: f32 = 32.0;

/// Alpha steps across each 32 dp ramp. Iced has no alpha-mask layer, so every
/// distinct opacity is a separately built cell; 1 dp bands rebuilt an edge cell
/// up to ~64 times a frame for a ramp no one can resolve. Eight levels keep the
/// ramp smooth for a bounded number of rebuilds.
const GRADIENT_LEVELS: f32 = 8.0;

/// The fade treatment for a scrollable cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    None,
    Tile,
    Gradient,
}

/// The `(top, height, alpha)` bands that approximate the gradient mask over the
/// visible part of one cell. The opaque middle is a single band.
fn gradient_bands(visible: Rectangle, origin: f32, edges: &Edges) -> Vec<(f32, f32, f32)> {
    let mut bands = Vec::new();
    if visible.height <= 0.0 {
        return bands;
    }
    let band = (edges.extent / GRADIENT_LEVELS).max(1.0);
    let limit = visible.y + visible.height;
    let mut y = visible.y;
    while y < limit {
        let relative = y - origin;
        let end = if relative < edges.top + edges.extent && edges.before > 0.0 {
            (y + band).min(origin + edges.top + edges.extent)
        } else if relative < edges.bottom - edges.extent {
            origin + edges.bottom - edges.extent
        } else if edges.after > 0.0 {
            y + band
        } else {
            limit
        }
        .min(limit)
        // Always make progress, even on a degenerate extent.
        .max(y + 0.001);
        bands.push((y, end - y, edges.alpha((y + end) / 2.0 - origin)));
        y = end;
    }
    bands
}

fn tile_alpha(top: f32, height: f32, viewport_top: f32, viewport_height: f32) -> f32 {
    if height <= 0.0 {
        return 0.0;
    }
    let visible = (top + height).min(viewport_top + viewport_height) - top.max(viewport_top);
    ((visible / height - 0.5) * 2.0).clamp(0.0, 1.0)
}

struct Edges {
    top: f32,
    bottom: f32,
    extent: f32,
    before: f32,
    after: f32,
}

impl Edges {
    fn new(offset: f32, height: f32, content_height: f32) -> Self {
        Self {
            top: offset,
            bottom: offset + height,
            extent: EXTENT.min(height / 2.0),
            before: (offset / EXTENT).clamp(0.0, 1.0),
            after: ((content_height - offset - height) / EXTENT).clamp(0.0, 1.0),
        }
    }

    fn alpha(&self, y: f32) -> f32 {
        if self.extent <= 0.0 {
            return 1.0;
        }
        let top = 1.0 - self.before * (1.0 - (y - self.top) / self.extent).clamp(0.0, 1.0);
        let bottom = 1.0 - self.after * (1.0 - (self.bottom - y) / self.extent).clamp(0.0, 1.0);
        top.min(bottom)
    }
}

/// `make` must return identical geometry and widget structure for every alpha.
/// Only primitive colours/opacities change; the real child retains input state
/// and text layout.
pub fn cell<'a>(
    mode: Mode,
    row: usize,
    rows: usize,
    row_height: f32,
    make: impl Fn(f32) -> Element<'a, Message> + 'a,
) -> Element<'a, Message> {
    let content = make(1.0);
    Element::new(Fade {
        content,
        make: Box::new(make),
        mode,
        row,
        rows,
        row_height,
    })
}

struct Fade<'a> {
    content: Element<'a, Message>,
    make: Box<dyn Fn(f32) -> Element<'a, Message> + 'a>,
    mode: Mode,
    row: usize,
    rows: usize,
    row_height: f32,
}

impl Widget<Message, iced::Theme, iced::Renderer> for Fade<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(visible) = bounds.intersection(viewport) else {
            return;
        };
        let draw = |renderer: &mut iced::Renderer, alpha: f32, clip: Rectangle| {
            if alpha <= 0.0 {
                return;
            }
            let faded;
            let content = if alpha == 1.0 {
                &self.content
            } else {
                faded = (self.make)(alpha);
                &faded
            };
            renderer.with_layer(clip, |renderer| {
                content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor,
                    viewport,
                );
            });
        };
        match self.mode {
            Mode::None => draw(renderer, 1.0, visible),
            Mode::Tile => draw(
                renderer,
                tile_alpha(bounds.y, bounds.height, viewport.y, viewport.height),
                visible,
            ),
            Mode::Gradient => {
                let origin = bounds.y - self.row as f32 * self.row_height;
                let edges = Edges::new(
                    viewport.y - origin,
                    viewport.height,
                    self.rows as f32 * self.row_height,
                );
                // No background quad obscures the wallpaper: the mask is applied
                // to the cell's own primitives, banded across its two ramps.
                for (y, height, alpha) in gradient_bands(visible, origin, &edges) {
                    let clip = Rectangle {
                        y,
                        height,
                        ..visible
                    };
                    draw(renderer, alpha, clip);
                }
            }
        }
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn iced::advanced::widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_fades_uniformly_and_disappears_at_half_visible() {
        for (top, expected) in [
            (0.0, 1.0),
            (-25.0, 0.5),
            (-50.0, 0.0),
            (225.0, 0.5),
            (250.0, 0.0),
        ] {
            assert_eq!(tile_alpha(top, 100.0, 0.0, 300.0), expected);
        }
    }

    #[test]
    fn gradient_eases_out_at_real_list_boundaries() {
        // A viewport at offset 16 in a 600 dp list: top strength is half,
        // bottom strength full. The ramp is 32 dp wide.
        let fade = Edges::new(16.0, 300.0, 600.0);
        for (y, expected) in [
            (16.0, 0.5),
            (32.0, 0.75),
            (48.0, 1.0),
            (284.0, 1.0),
            (300.0, 0.5),
            (316.0, 0.0),
        ] {
            assert_eq!(fade.alpha(y), expected);
        }
        assert_eq!(Edges::new(0.0, 300.0, 600.0).alpha(0.0), 1.0);
        assert_eq!(Edges::new(300.0, 300.0, 600.0).alpha(600.0), 1.0);
    }

    #[test]
    fn fitting_content_has_no_gradient_and_short_viewports_do_not_overlap_ramps() {
        let fitting = Edges::new(0.0, 300.0, 200.0);
        assert_eq!(fitting.alpha(0.0), 1.0);
        assert_eq!(fitting.alpha(300.0), 1.0);
        let short = Edges::new(100.0, 40.0, 600.0);
        assert_eq!(short.alpha(100.0), 0.0);
        assert_eq!(short.alpha(110.0), 0.5);
        assert_eq!(short.alpha(120.0), 1.0);
        assert_eq!(short.alpha(130.0), 0.5);
    }

    #[test]
    fn gradient_bands_tile_the_cell_with_a_bounded_number_of_opacities() {
        use iced::Point;
        // A viewport at offset 16 in a 600 dp list: both ramps live.
        let edges = Edges::new(16.0, 300.0, 600.0);
        let visible = Rectangle::new(Point::new(0.0, 0.0), Size::new(100.0, 300.0));
        let bands = gradient_bands(visible, 0.0, &edges);

        // Bands cover the visible cell exactly: no gap, no overlap.
        let mut cursor = visible.y;
        for (y, height, _) in &bands {
            assert!((*y - cursor).abs() < 0.01);
            assert!(*height > 0.0);
            cursor = y + height;
        }
        assert!((cursor - (visible.y + visible.height)).abs() < 0.01);

        // Each band with a partial alpha costs one rebuilt cell, and that count
        // is bounded by the level count instead of growing with a 1 dp step.
        let rebuilt = bands.iter().filter(|(_, _, alpha)| *alpha < 1.0).count();
        assert!(
            rebuilt <= 2 * GRADIENT_LEVELS as usize + 2,
            "{rebuilt} cells would be rebuilt in {} bands",
            bands.len()
        );
        assert!(
            bands.len() <= 2 * GRADIENT_LEVELS as usize + 6,
            "{} bands",
            bands.len()
        );
    }

    #[test]
    fn gradient_bands_are_empty_or_opaque_without_a_ramp() {
        use iced::Point;
        let visible = Rectangle::new(Point::new(0.0, 0.0), Size::new(100.0, 50.0));
        // Content shorter than the viewport has no ramp at all.
        let fitting = Edges::new(0.0, 300.0, 200.0);
        let bands = gradient_bands(visible, 0.0, &fitting);
        assert!(bands.iter().all(|(_, _, alpha)| *alpha == 1.0));
    }
}
