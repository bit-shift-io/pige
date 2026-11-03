//! Pull-to-refresh for the message list.
//!
//! The wrapper observes the direct child scrollable and starts a refresh only
//! when a downward drag begins while that scroller is already at the top. Iced
//! stays the owner of logical scrolling; this widget only translates the drawn
//! content and paints a small indicator in the revealed gap.
//!
//! The gesture and resisted displacement follow qlaunch's elastic-scroll
//! wrapper (bitshift, MIT), simplified to the single top-edge refresh action.

use iced::advanced::widget::operation::Scrollable;
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer};
use iced::{
    Color, Element, Event, Length, Point, Rectangle, Size, Vector, mouse as mouse_event, touch,
};

use crate::Message;
use crate::theme::{self, Palette};

/// Resistance approaches the pull limit asymptotically, like the elastic
/// bounce, so a long drag never turns into an unbounded offset.
fn resistance(distance: f32, limit: f32) -> f32 {
    if !distance.is_finite() || !limit.is_finite() || limit <= 0.0 || distance <= 0.0 {
        return 0.0;
    }
    limit * (1.0 - (-distance / limit).exp())
}

fn progress(pull: f32, threshold: f32) -> f32 {
    if !pull.is_finite() || !threshold.is_finite() || threshold <= 0.0 {
        return 0.0;
    }
    (pull / threshold).clamp(0.0, 1.0)
}

fn triggers(pull: f32, threshold: f32) -> bool {
    pull.is_finite() && pull >= threshold
}

/// Wrap a configured vertical scroller so dragging from the top refreshes.
pub fn wrap<'a>(
    content: Element<'a, Message>,
    refreshing: bool,
    on_refresh: Message,
) -> Element<'a, Message> {
    Element::new(Pull {
        content,
        refreshing,
        on_refresh,
        palette: theme::active(),
    })
}

struct Pull<'a> {
    content: Element<'a, Message>,
    refreshing: bool,
    on_refresh: Message,
    palette: Palette,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    /// `None` identifies a mouse gesture.
    finger: Option<touch::Finger>,
    start: Point,
    vertical: Option<bool>,
}

#[derive(Default)]
struct State {
    drag: Option<Drag>,
    pull: f32,
}

#[derive(Default)]
struct Query {
    at_top: Option<bool>,
}

impl Operation for Query {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
        if self.at_top.is_none() {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&iced::widget::Id>,
        _bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if self.at_top.is_none() {
            self.at_top = Some(translation.y <= 1.0);
        }
    }
}

impl Pull<'_> {
    fn at_top(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer) -> bool {
        let mut query = Query::default();
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut query);
        // Content that is not scrollable cannot be anywhere but the top.
        query.at_top.unwrap_or(true)
    }

    fn begin(&self, state: &mut State, finger: Option<touch::Finger>, position: Point) {
        state.drag = Some(Drag {
            finger,
            start: position,
            vertical: None,
        });
        state.pull = 0.0;
    }

    fn moved(&self, state: &mut State, finger: Option<touch::Finger>, position: Point) -> bool {
        let Some(drag) = state.drag else {
            return false;
        };
        if drag.finger != finger {
            return false;
        }
        let delta = position - drag.start;
        let vertical = match drag.vertical {
            Some(vertical) => vertical,
            None => {
                if delta.y.abs() < self.palette.metrics.pull_slop {
                    return false;
                }
                let vertical = delta.y.abs() > delta.x.abs() && delta.y > 0.0;
                if let Some(drag) = state.drag.as_mut() {
                    drag.vertical = Some(vertical);
                }
                vertical
            }
        };
        if !vertical {
            return false;
        }
        let metrics = self.palette.metrics;
        state.pull = resistance(delta.y - metrics.pull_slop, metrics.pull_limit);
        true
    }

    fn finish(&self, state: &mut State, shell: &mut Shell<'_, Message>) {
        let metrics = self.palette.metrics;
        if triggers(state.pull, metrics.pull_threshold) {
            shell.publish(self.on_refresh.clone());
        }
        state.drag = None;
        state.pull = 0.0;
        shell.request_redraw();
    }

    fn draw_indicator(&self, renderer: &mut iced::Renderer, bounds: Rectangle, pull: f32) {
        let palette = self.palette;
        let metrics = palette.metrics;
        let progress = if self.refreshing {
            1.0
        } else {
            progress(pull, metrics.pull_threshold)
        };
        let height = metrics.pull_indicator_height;
        let full = metrics.pull_indicator_width;
        let width = (full * 0.25 + full * 0.75 * progress).max(height);
        let y = bounds.y + (pull - height).max(0.0) / 2.0;
        let track = Rectangle {
            x: bounds.center_x() - full / 2.0,
            y,
            width: full,
            height,
        };
        let fill = Rectangle {
            x: bounds.center_x() - width / 2.0,
            y,
            width,
            height,
        };
        let radius = height / 2.0;
        renderer.fill_quad(
            renderer::Quad {
                bounds: track,
                border: iced::border::rounded(radius),
                ..Default::default()
            },
            Color {
                a: palette.states.track,
                ..palette.outline
            },
        );
        renderer.fill_quad(
            renderer::Quad {
                bounds: fill,
                border: iced::border::rounded(radius),
                ..Default::default()
            },
            palette.primary,
        );
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Pull<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
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
        let at_top = self.at_top(tree, layout, renderer);
        let state = tree.state.downcast_mut::<State>();

        match event {
            Event::Touch(touch::Event::FingerPressed { id, position }) if at_top => {
                self.begin(state, Some(*id), *position);
            }
            Event::Touch(touch::Event::FingerMoved { id, position }) => {
                if self.moved(state, Some(*id), *position) {
                    shell.request_redraw();
                }
            }
            Event::Touch(
                touch::Event::FingerLifted { id, .. } | touch::Event::FingerLost { id, .. },
            ) => {
                if state.drag.is_some_and(|drag| drag.finger == Some(*id)) {
                    self.finish(state, shell);
                }
            }
            Event::Mouse(mouse_event::Event::ButtonPressed(mouse_event::Button::Left))
                if at_top =>
            {
                if let Some(position) = cursor.position() {
                    self.begin(state, None, position);
                }
            }
            Event::Mouse(mouse_event::Event::CursorMoved { .. }) => {
                if cursor
                    .position()
                    .is_some_and(|position| self.moved(state, None, position))
                {
                    shell.request_redraw();
                }
            }
            Event::Mouse(mouse_event::Event::ButtonReleased(mouse_event::Button::Left))
                if state.drag.is_some_and(|drag| drag.finger.is_none()) =>
            {
                self.finish(state, shell);
            }
            _ => {}
        }

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
        let state = tree.state.downcast_ref::<State>();
        let pull = if self.refreshing {
            self.palette.metrics.pull_threshold
        } else {
            state.pull
        };
        let bounds = layout.bounds();

        if pull <= 0.0 {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
            return;
        }

        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        renderer.with_layer(clip, |renderer| {
            renderer.with_translation(Vector::new(0.0, pull), |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    style,
                    layout,
                    cursor,
                    &clip,
                );
            });
        });
        self.draw_indicator(renderer, bounds, pull);
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<iced::advanced::overlay::Element<'a, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resistance_is_bounded_and_monotonic() {
        let limit = 96.0;
        assert_eq!(resistance(0.0, limit), 0.0);
        assert_eq!(resistance(-20.0, limit), 0.0);
        assert_eq!(resistance(f32::NAN, limit), 0.0);
        let small = resistance(40.0, limit);
        let large = resistance(400.0, limit);
        assert!(small > 0.0 && small < large);
        assert!(large < limit);
    }

    #[test]
    fn a_pull_triggers_only_past_the_threshold() {
        assert!(!triggers(63.9, 64.0));
        assert!(triggers(64.0, 64.0));
        assert!(triggers(80.0, 64.0));
        assert!(!triggers(f32::NAN, 64.0));
    }

    #[test]
    fn progress_is_clamped_for_the_indicator() {
        assert_eq!(progress(-1.0, 64.0), 0.0);
        assert_eq!(progress(0.0, 64.0), 0.0);
        assert_eq!(progress(32.0, 64.0), 0.5);
        assert_eq!(progress(128.0, 64.0), 1.0);
        assert_eq!(progress(32.0, 0.0), 0.0);
    }

    #[test]
    fn a_strong_drag_reaches_the_threshold() {
        let metrics = theme::palette(true).metrics;
        let pull = resistance(140.0, metrics.pull_limit);
        assert!(triggers(pull, metrics.pull_threshold), "{pull}");
    }
}
