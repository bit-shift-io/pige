//! Shared touch overscroll feedback: a small resisted bounce plus row spread.
//!
//! Iced remains the owner of logical scrolling: the wrapper only observes the
//! direct child scroller and never adds scrollable extent. Displacement is
//! visual, so accessibility offsets, scrollbars and layout stay unchanged.
//!
//! Copied from qlaunch's `elastic_scroll.rs` (bitshift, MIT). The launcher's
//! settings-driven feature switch is kept; pige can enable it once the message
//! list lands.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use iced::advanced::widget::operation::Scrollable;
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer};
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector, touch, window};

use crate::Message;

/// Feature switch for the elastic overscroll and velocity stretch.
///
/// The implementation is retained below; `wrap` is the only production entry
/// point, so returning the scroller unchanged switches off the bounce, the row
/// spread and the stretch together. Set this to `true` to restore them.
pub(crate) const ENABLED: bool = true;

/// Upper bound on the visual displacement, in logical pixels.
const MAX_DISPLACEMENT: f32 = 48.0;
/// The bounce is also bounded relative to a short viewport.
const VIEWPORT_FRACTION: f32 = 0.10;
/// Critically damped spring frequency.
const OMEGA: f32 = 15.0;
/// Longest frame integrated at once; a stalled frame must not overshoot.
const MAX_TICK: f32 = 0.032;
/// Vertical movement needed before deciding the gesture axis.
const AXIS_SLOP: f32 = 18.0;
/// A settled spring snaps to exactly zero below these thresholds.
const REST_POSITION: f32 = 0.05;
const REST_VELOCITY: f32 = 0.5;
/// How much of the pull moves the whole content; rows take the rest.
const BOUNCE_SHARE: f32 = 0.75;
/// Content stops accepting presses once it is visibly displaced.
pub(crate) const MOVING: f32 = 0.5;

/// Which clamped end the current feedback belongs to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Edge {
    #[default]
    None,
    Top,
    Bottom,
}

/// When a scroller may produce edge feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Both ends.
    Normal,
    /// The launcher grid keeps the top edge for pull-down search.
    LauncherTop,
    /// Preview pages and other input-gated scrollers.
    Disabled,
}

/// The visual state shared by the wrapper, its content and its rows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Values {
    /// Signed total displacement; positive is downward.
    pub(crate) total: f32,
    pub(crate) edge: Edge,
}

impl Values {
    fn bounce(self) -> f32 {
        self.total * BOUNCE_SHARE
    }

    fn spread(self) -> f32 {
        self.total * (1.0 - BOUNCE_SHARE)
    }

    pub(crate) fn moving(self) -> bool {
        self.total.abs() > MOVING
    }
}

/// Who owns the current touch drag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Owner {
    /// The inner scroller is scrolling or has cancelled the widget under the
    /// finger; outward movement may feed the bounce.
    #[default]
    Scroller,
    /// A widget that captures its own moves (a reorder grip, slider, text
    /// selection) is driving the gesture; the bounce must stay out of the way.
    Child,
}

#[derive(Debug, Default)]
struct Shared {
    values: Values,
    owner: Owner,
}

/// Cloneable handle to one scroll region's visual state.
#[derive(Debug, Clone, Default)]
pub(crate) struct Motion {
    shared: Arc<Mutex<Shared>>,
}

impl Motion {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn values(&self) -> Values {
        self.shared
            .lock()
            .map(|shared| shared.values)
            .unwrap_or_default()
    }

    pub(crate) fn set(&self, total: f32, edge: Edge) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.values = Values {
                total: if total.is_finite() { total } else { 0.0 },
                edge,
            };
        }
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.values = Values::default();
        }
    }

    fn begin_gesture(&self, scroller_owns: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.owner = if scroller_owns {
                Owner::Scroller
            } else {
                Owner::Child
            };
        }
    }

    fn scroller_owns(&self) -> bool {
        self.shared
            .lock()
            .map(|shared| shared.owner == Owner::Scroller)
            .unwrap_or(true)
    }
}

/// Rows spread away from the touched end, never beyond the displacement.
pub(crate) fn row_offset(values: Values, row: Rectangle, viewport: Rectangle) -> f32 {
    if values.total == 0.0 || !viewport.height.is_finite() || viewport.height <= 0.0 {
        return 0.0;
    }
    let from_edge = match values.edge {
        Edge::Top => (row.y - viewport.y) / viewport.height,
        Edge::Bottom => (viewport.y + viewport.height - (row.y + row.height)) / viewport.height,
        Edge::None => return 0.0,
    };
    values.spread() * from_edge.clamp(0.0, 1.0)
}

fn displacement_limit(viewport_height: f32) -> f32 {
    if !viewport_height.is_finite() || viewport_height <= 0.0 {
        return 0.0;
    }
    MAX_DISPLACEMENT.min(viewport_height * VIEWPORT_FRACTION)
}

/// Resisted displacement for a raw outward pull.
fn resistance(raw: f32, limit: f32) -> f32 {
    if !raw.is_finite() || !limit.is_finite() || limit <= 0.0 {
        return 0.0;
    }
    let sign = raw.signum();
    let magnitude = limit * (1.0 - (-raw.abs() / limit).exp());
    sign * magnitude
}

/// Inverse of [`resistance`], used to continue smoothly when a new touch
/// interrupts a settling animation.
fn raw_for(position: f32, limit: f32) -> f32 {
    if position == 0.0 || limit <= 0.0 {
        return 0.0;
    }
    let ratio = (position.abs() / limit).min(0.999);
    position.signum() * (-(1.0 - ratio).ln()) * limit
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Spring {
    position: f32,
    velocity: f32,
}

impl Spring {
    pub(crate) fn position(self) -> f32 {
        self.position
    }

    /// Follow the finger with bounded resistance.
    pub(crate) fn pull(&mut self, raw: f32, limit: f32) {
        self.position = resistance(raw, limit);
        self.velocity = 0.0;
    }

    /// Resume from the current visual position without a jump.
    pub(crate) fn interrupt(&mut self, limit: f32) -> f32 {
        self.velocity = 0.0;
        raw_for(self.position, limit)
    }

    /// Sign of the current rebound speed; zero once settled.
    pub(crate) fn velocity(self) -> f32 {
        self.velocity
    }

    /// Start a rebound with an initial outward speed (a flick that ran past
    /// the end of the list while the finger was already gone).
    pub(crate) fn kick(&mut self, velocity: f32) {
        if velocity.is_finite() {
            self.velocity = velocity;
        }
    }

    /// Advance the release animation. Returns whether it is still moving.
    pub(crate) fn tick(&mut self, seconds: f32) -> bool {
        if !seconds.is_finite() || seconds <= 0.0 {
            return self.position != 0.0;
        }
        let dt = seconds.min(MAX_TICK);
        let acceleration = -OMEGA * OMEGA * self.position - 2.0 * OMEGA * self.velocity;
        self.velocity += acceleration * dt;
        self.position += self.velocity * dt;
        if !self.position.is_finite() || !self.velocity.is_finite() {
            self.reset();
            return false;
        }
        if self.position.abs() <= REST_POSITION && self.velocity.abs() <= REST_VELOCITY {
            self.reset();
            return false;
        }
        true
    }

    pub(crate) fn reset(&mut self) {
        self.position = 0.0;
        self.velocity = 0.0;
    }
}

#[derive(Debug, Clone, Copy)]
struct Scroller {
    bounds: Rectangle,
    content_bounds: Rectangle,
    translation: Vector,
}

#[derive(Default)]
struct Query {
    result: Option<Scroller>,
}

impl Operation for Query {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<()>)) {
        if self.result.is_none() {
            operate(self);
        }
    }

    fn scrollable(
        &mut self,
        _id: Option<&iced::widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn Scrollable,
    ) {
        if self.result.is_none() {
            self.result = Some(Scroller {
                bounds,
                content_bounds,
                translation,
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy)]
struct Touch {
    id: touch::Finger,
    start: Point,
    last: Point,
    axis: Option<Axis>,
    edge: Edge,
    raw: f32,
    /// The most recent move was fully outward (the list was clamped).
    overscrolled: bool,
}

#[derive(Default)]
struct State {
    touch: Option<Touch>,
    spring: Spring,
    released: bool,
    last_tick: Option<Instant>,
    viewport: Size,
    content_height: f32,
    translation_delta: f32,
    /// Velocity-proportional stretch, in logical pixels; folded into the spring
    /// when the finger leaves so the settle starts from the visible position.
    stretch: f32,
    /// Smoothed vertical finger speed in logical pixels per second.
    velocity: f32,
    last_move: Option<Instant>,
    /// Last nonzero logical movement of a fingerless fling.
    fling_delta: f32,
    fling_at: Option<Instant>,
}

impl State {
    fn reset(&mut self) {
        self.touch = None;
        self.spring.reset();
        self.released = false;
        self.last_tick = None;
        self.content_height = 0.0;
        self.translation_delta = 0.0;
        self.stretch = 0.0;
        self.velocity = 0.0;
        self.last_move = None;
        self.fling_delta = 0.0;
        self.fling_at = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    Top,
    Bottom,
}

/// A flick that reached an end may still be moving; only a frame this close
/// to the last logical movement counts as the arrival.
const FLING_WINDOW: std::time::Duration = std::time::Duration::from_millis(90);
/// Cap on the rebound speed injected when a fling hits an end.
const KICK_LIMIT: f32 = 2.5;
/// A released finger must be moving at least this fast to keep momentum.
const RELEASE_FLING: f32 = 260.0;
/// Frames longer than this are treated as a hitch, not a readable speed.
const VELOCITY_WINDOW: f32 = 0.12;

/// A drag slower than this (logical pixels per second) produces no stretch.
const SPEED_FLOOR: f32 = 120.0;
/// How quickly the lag approaches its cap; larger is more eager.
const EAGERNESS: f32 = 8.0;

/// Bounded, signed lag for a scroll speed, in logical pixels per second.
/// Adapted from qlaunch's `stretch::lag`.
fn lag(speed: f32, limit: f32) -> f32 {
    if !speed.is_finite() || !limit.is_finite() || limit <= 0.0 || speed.abs() < SPEED_FLOOR {
        return 0.0;
    }
    speed.signum() * limit * (1.0 - (-speed.abs() / (limit * EAGERNESS)).exp())
}

/// Whether the scroller is clamped at the given end.
fn at_end(scroller: Scroller, end: End) -> bool {
    let slack = 1.0;
    match end {
        End::Top => scroller.translation.y <= slack,
        End::Bottom => {
            scroller.translation.y
                >= scroller.content_bounds.height - scroller.bounds.height - slack
        }
    }
}

fn outward_delta(end: End, finger_delta: f32, translation_delta: f32) -> f32 {
    // Translation grows as the content scrolls down, so a finger moving up
    // (negative) is consumed by a positive translation delta. Adding them
    // leaves only the movement the clamped scroller could not use.
    let residual = finger_delta + translation_delta;
    match end {
        End::Top => residual.max(0.0),
        End::Bottom => residual.min(0.0),
    }
}

struct Wrapper<'a> {
    scroller: Element<'a, Message>,
    motion: Motion,
    policy: Policy,
}

/// Wrap a fully configured vertical scroller. The wrapper forwards every
/// operation and event; it only observes and decorates.
pub(crate) fn wrap<'a>(
    scroller: Element<'a, Message>,
    motion: Motion,
    policy: Policy,
) -> Element<'a, Message> {
    if !ENABLED {
        return scroller;
    }
    wrap_unconditional(scroller, motion, policy)
}

/// The retained wrapper construction, shared by [`wrap`] and by the tests that
/// keep the implementation covered while the feature is switched off.
fn wrap_unconditional<'a>(
    scroller: Element<'a, Message>,
    motion: Motion,
    policy: Policy,
) -> Element<'a, Message> {
    Element::new(Wrapper {
        scroller,
        motion,
        policy,
    })
}

impl Wrapper<'_> {
    fn query(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
    ) -> Option<Scroller> {
        let mut query = Query::default();
        self.scroller
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut query);
        query.result
    }

    fn press(&self, state: &mut State, scroller: Scroller, id: touch::Finger, position: Point) {
        if self.policy == Policy::Disabled
            || state.touch.is_some()
            || !scroller.bounds.contains(position)
        {
            return;
        }
        let limit = displacement_limit(scroller.bounds.height);
        let raw = state.spring.interrupt(limit);
        state.touch = Some(Touch {
            id,
            start: position,
            last: position,
            axis: None,
            edge: Edge::None,
            raw,
            overscrolled: false,
        });
        state.velocity = 0.0;
        state.last_move = None;
        state.fling_delta = 0.0;
        state.fling_at = None;
        // A settling animation keeps ticking under the new finger until it
        // actually pulls, so a cancelled gesture cannot freeze the bounce.
        state.last_tick = None;
    }

    fn moved(&self, state: &mut State, id: touch::Finger, position: Point) {
        let Some(mut touch) = state.touch else {
            return;
        };
        if touch.id != id {
            return;
        }
        let delta = position - touch.last;
        touch.last = position;
        let total = position - touch.start;
        let now = Instant::now();
        if let Some(last) = state.last_move {
            let seconds = (now - last).as_secs_f32();
            if seconds > 0.0 && seconds < VELOCITY_WINDOW {
                state.velocity = state.velocity * 0.6 + (delta.y / seconds) * 0.4;
            }
        }
        state.last_move = Some(now);
        if touch.axis.is_none() {
            if total.x.abs() >= AXIS_SLOP && total.x.abs() > total.y.abs() {
                touch.axis = Some(Axis::Horizontal);
            } else if total.y.abs() >= AXIS_SLOP {
                touch.axis = Some(Axis::Vertical);
                touch.edge = if total.y > 0.0 {
                    Edge::Top
                } else {
                    Edge::Bottom
                };
            }
        }
        if touch.axis != Some(Axis::Vertical) || touch.edge == Edge::None {
            state.touch = Some(touch);
            return;
        }
        if self.policy == Policy::LauncherTop && touch.edge == Edge::Top {
            return;
        }
        // A reorder grip, slider, or other drag owner keeps its gesture: until
        // the scroller has cancelled that press there is nothing to bounce.
        if !self.motion.scroller_owns() {
            state.touch = Some(touch);
            return;
        }
        let end = match touch.edge {
            Edge::Top => End::Top,
            Edge::Bottom => End::Bottom,
            Edge::None => return,
        };
        let residual = outward_delta(end, delta.y, state.translation_delta);
        touch.overscrolled = residual != 0.0;
        touch.raw += residual;
        touch.raw = match end {
            End::Top => touch.raw.max(0.0),
            End::Bottom => touch.raw.min(0.0),
        };
        let limit = displacement_limit(state.viewport.height);
        state.spring.pull(touch.raw, limit);
        // A list that is actually moving also stretches with its speed, even
        // in the middle where the edge spring stays at rest.
        state.stretch = if state.translation_delta.abs() > 0.5 {
            lag(state.velocity, limit)
        } else {
            0.0
        };
        let visual = (state.spring.position() + state.stretch).clamp(-limit, limit);
        state.touch = Some(touch);
        state.released = false;
        state.last_tick = None;
        self.motion.set(visual, touch.edge);
    }

    fn release(&self, state: &mut State, id: Option<touch::Finger>) {
        let Some(touch) = state.touch else {
            return;
        };
        if id.is_some_and(|id| id != touch.id) {
            return;
        }
        state.touch = None;
        // Start the settle from what is on screen: fold the velocity stretch
        // into the spring so the rebound has no visible jump.
        if state.stretch != 0.0 {
            let limit = displacement_limit(state.viewport.height);
            let total = state.spring.position() + state.stretch;
            state.spring.pull(raw_for(total, limit), limit);
            state.stretch = 0.0;
        }
        // A fast flick keeps its momentum so a quick overscroll springs back
        // instead of stopping flat.
        if touch.edge != Edge::None && touch.overscrolled {
            let limit = displacement_limit(state.viewport.height);
            let outward = match touch.edge {
                Edge::Top => state.velocity.max(0.0),
                Edge::Bottom => state.velocity.min(0.0),
                Edge::None => 0.0,
            };
            if outward.abs() >= RELEASE_FLING && limit > 0.0 {
                let room = (limit - state.spring.position().abs()).max(0.0);
                let cap = room * OMEGA * KICK_LIMIT;
                state.spring.kick(outward.signum() * outward.abs().min(cap));
            }
        }
        if state.spring.position() != 0.0 || state.spring.velocity() != 0.0 {
            state.released = true;
            state.last_tick = None;
        } else {
            state.released = false;
            self.motion.clear();
        }
    }

    /// A fingerless fling reaches the logical end without any touch events.
    /// Detect that arrival and give it the same springy rebound.
    fn fling_frame(
        &self,
        state: &mut State,
        now: Instant,
        scroller: Option<Scroller>,
        shell: &mut Shell<'_, Message>,
    ) {
        if state.touch.is_some() || state.released {
            return;
        }
        if state.translation_delta.abs() > 0.5 {
            state.fling_delta = state.translation_delta;
            state.fling_at = Some(now);
            return;
        }
        let Some(at) = state.fling_at.take() else {
            return;
        };
        let Some(scroller) = scroller else {
            return;
        };
        let seconds = (now - at).as_secs_f32();
        if !(0.0..=FLING_WINDOW.as_secs_f32()).contains(&seconds) {
            return;
        }
        let end = if state.fling_delta > 0.0 {
            End::Bottom
        } else {
            End::Top
        };
        if !at_end(scroller, end) {
            return;
        }
        let limit = displacement_limit(scroller.bounds.height);
        if limit <= 0.0 {
            return;
        }
        let speed =
            (state.fling_delta.abs() / seconds.max(1.0 / 240.0)).min(limit * OMEGA * KICK_LIMIT);
        let velocity = match end {
            End::Bottom => -speed,
            End::Top => speed,
        };
        let edge = match end {
            End::Bottom => Edge::Bottom,
            End::Top => Edge::Top,
        };
        state.spring.kick(velocity);
        if state.spring.tick(1.0 / 120.0) {
            state.released = true;
            state.last_tick = Some(now);
            self.motion.set(state.spring.position(), edge);
            shell.request_redraw();
        }
    }

    fn settle(&self, state: &mut State, now: Instant) {
        if !state.released {
            return;
        }
        let seconds = state
            .last_tick
            .map_or(0.0, |last| (now - last).as_secs_f32());
        state.last_tick = Some(now);
        if state.spring.tick(seconds) {
            self.motion
                .set(state.spring.position(), self.motion.values().edge);
        } else {
            state.released = false;
            self.motion.clear();
        }
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Wrapper<'_> {
    fn size(&self) -> Size<Length> {
        self.scroller.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.scroller.as_widget().size_hint()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.scroller)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.scroller));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let node = self
            .scroller
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let state = tree.state.downcast_mut::<State>();
        if state.viewport != node.size() {
            state.reset();
            state.viewport = node.size();
            self.motion.clear();
        }
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        // Programmatic scrolling moves the logical position without a touch;
        // any visual displacement from before that move is now stale.
        let before = self.query(tree, layout, renderer);
        self.scroller
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        let after = self.query(tree, layout, renderer);
        if let (Some(before), Some(after)) = (before, after) {
            let moved = (after.translation.y - before.translation.y).abs() > 0.5;
            let state = tree.state.downcast_mut::<State>();
            if moved && state.touch.is_none() {
                state.reset();
                self.motion.clear();
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
        let before = self.query(tree, layout, renderer);
        self.scroller.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let after = self.query(tree, layout, renderer);
        let state = tree.state.downcast_mut::<State>();

        if let (Some(before), Some(after)) = (before, after) {
            state.translation_delta = after.translation.y - before.translation.y;
            if state.content_height != after.content_bounds.height {
                if state.touch.is_none() {
                    state.reset();
                    self.motion.clear();
                }
                state.content_height = after.content_bounds.height;
            }
        }

        match event {
            Event::Touch(touch::Event::FingerPressed { id, position }) => {
                if let Some(scroller) = after {
                    self.press(state, scroller, *id, *position);
                }
            }
            Event::Touch(touch::Event::FingerMoved { id, position }) => {
                self.moved(state, *id, *position);
                if state.touch.is_some_and(|touch| {
                    touch.axis == Some(Axis::Vertical) && touch.edge != Edge::None
                }) {
                    shell.request_redraw();
                }
            }
            Event::Touch(
                touch::Event::FingerLifted { id, .. } | touch::Event::FingerLost { id, .. },
            ) => {
                self.release(state, Some(*id));
                if state.released {
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                self.settle(state, *now);
                if state.released {
                    shell.request_redraw();
                }
                self.fling_frame(state, *now, after, shell);
            }
            Event::Mouse(mouse::Event::WheelScrolled { .. }) => {
                if state.touch.is_none() {
                    state.reset();
                    self.motion.clear();
                }
            }
            _ => {
                // A wheel, fling, or programmatic scroll moved the list:
                // visual feedback must not fight the new logical position.
                if state.touch.is_none() && state.translation_delta.abs() > 0.5 {
                    state.reset();
                    self.motion.clear();
                }
            }
        }
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
        self.scroller.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
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
        self.scroller.as_widget().mouse_interaction(
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
        self.scroller.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

struct Content<'a> {
    child: Element<'a, Message>,
    motion: Motion,
}

/// Place around a scroller's content to translate the bounce and to stop
/// presses from activating content that is visibly displaced.
pub(crate) fn content<'a>(child: Element<'a, Message>, motion: &Motion) -> Element<'a, Message> {
    Element::new(Content {
        child,
        motion: motion.clone(),
    })
}

impl Content<'_> {
    fn moving(&self) -> bool {
        self.motion.values().moving()
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Content<'_> {
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.child)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.child));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        if matches!(event, Event::Touch(touch::Event::FingerLost { .. })) {
            // The scroller cancelled the widget under the finger: it owns the
            // drag from here on, so outward movement may feed the bounce.
            self.motion.begin_gesture(true);
        }
        let suppress = self.moving()
            && match event {
                Event::Touch(touch::Event::FingerLost { .. }) => false,
                Event::Touch(_) => true,
                Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::ButtonReleased(_)) => {
                    true
                }
                _ => false,
            };
        if suppress {
            return;
        }
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if matches!(event, Event::Touch(touch::Event::FingerPressed { .. })) {
            // A widget that captured the press (button, grip, slider) owns the
            // gesture until the scroller cancels it; otherwise the scroller
            // already owns this drag.
            self.motion.begin_gesture(!shell.is_event_captured());
        }
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
        let values = self.motion.values();
        if values.bounce() == 0.0 {
            self.child.as_widget().draw(
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
        if let Some(clip) = layout.bounds().intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                renderer.with_translation(Vector::new(0.0, values.bounce()), |renderer| {
                    self.child.as_widget().draw(
                        &tree.children[0],
                        renderer,
                        theme,
                        style,
                        layout,
                        cursor,
                        &clip.intersection(&layout.bounds()).unwrap_or(clip),
                    );
                });
            });
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if self.moving() {
            return mouse::Interaction::None;
        }
        self.child.as_widget().mouse_interaction(
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
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

struct Row<'a> {
    child: Element<'a, Message>,
    motion: Motion,
}

/// Decorate one real row (never a virtualization spacer) so the spacing
/// grows away from the touched end.
pub(crate) fn row<'a>(child: Element<'a, Message>, motion: &Motion) -> Element<'a, Message> {
    Element::new(Row {
        child,
        motion: motion.clone(),
    })
}

impl Widget<Message, iced::Theme, iced::Renderer> for Row<'_> {
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.child)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.child));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        self.child.as_widget_mut().update(
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
        let offset = row_offset(self.motion.values(), layout.bounds(), *viewport);
        if offset == 0.0 {
            self.child.as_widget().draw(
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
        renderer.with_translation(Vector::new(0.0, offset), |renderer| {
            self.child.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                layout,
                cursor,
                viewport,
            );
        });
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
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
        self.child.as_widget_mut().overlay(
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
    use iced::Fill;
    use iced::advanced::renderer::Headless;
    use iced::widget::{column, container, scrollable};

    fn new_spring() -> Spring {
        Spring::default()
    }

    #[test]
    fn resistance_grows_but_stays_within_the_limit() {
        let limit = 12.0;
        let near = resistance(4.0, limit);
        let far = resistance(100.0, limit);
        assert!(near > 0.0 && near < 4.0);
        assert!(far > near);
        assert!(far < limit);
        assert_eq!(resistance(f32::NAN, limit), 0.0);
        assert_eq!(resistance(10.0, 0.0), 0.0);
        assert_eq!(resistance(-4.0, limit), -near);
    }

    #[test]
    fn consumed_scroll_movement_leaves_no_outward_residual() {
        // Translation grows as content scrolls down, so scrolling consumes a
        // finger delta of the opposite sign.
        assert_eq!(outward_delta(End::Top, 10.0, -10.0), 0.0);
        assert_eq!(outward_delta(End::Bottom, -10.0, 10.0), 0.0);
        // At a clamped edge nothing is consumed and the whole pull remains.
        assert_eq!(outward_delta(End::Top, 10.0, 0.0), 10.0);
        assert_eq!(outward_delta(End::Bottom, -10.0, 0.0), -10.0);
    }

    #[test]
    fn displacement_limit_uses_the_smaller_cap() {
        assert_eq!(displacement_limit(1000.0), MAX_DISPLACEMENT);
        assert_eq!(displacement_limit(400.0), 40.0);
        assert_eq!(displacement_limit(200.0), 20.0);
        assert_eq!(displacement_limit(0.0), 0.0);
        assert_eq!(displacement_limit(f32::NAN), 0.0);
    }

    #[test]
    fn a_firm_pull_reaches_most_of_the_cap() {
        let limit = displacement_limit(400.0);
        assert!(resistance(220.0, limit) > limit * 0.8);
        assert!(resistance(220.0, limit) < limit);
    }

    #[test]
    fn released_spring_settles_exactly_to_zero() {
        let mut settled = new_spring();
        settled.pull(80.0, 20.0);
        assert!(settled.position() > 0.0);
        let mut moving = true;
        for _ in 0..200 {
            moving = settled.tick(1.0 / 60.0);
            if !moving {
                break;
            }
        }
        assert!(!moving);
        assert_eq!(settled.position(), 0.0);
        assert!(!settled.tick(0.016));
    }

    #[test]
    fn spring_tick_is_bounded_and_finite() {
        let mut long = new_spring();
        long.pull(80.0, 32.0);
        assert!(long.tick(10.0));
        assert!(long.position().is_finite());
        let mut nan = new_spring();
        nan.pull(80.0, 32.0);
        assert!(nan.tick(f32::NAN));
        assert!(nan.position().is_finite());
    }

    #[test]
    fn interruption_reuses_the_current_position() {
        let limit = 32.0;
        let mut spring = Spring::default();
        spring.pull(60.0, limit);
        let position = spring.position();
        let raw = spring.interrupt(limit);
        assert!((resistance(raw, limit) - position).abs() < 0.001);
    }

    #[test]
    fn row_spread_is_zero_at_the_touched_edge_and_bounded_away_from_it() {
        let viewport = Rectangle {
            x: 0.0,
            y: 100.0,
            width: 300.0,
            height: 400.0,
        };
        let top = Rectangle {
            x: 0.0,
            y: 100.0,
            width: 300.0,
            height: 48.0,
        };
        let middle = Rectangle {
            x: 0.0,
            y: 300.0,
            width: 300.0,
            height: 48.0,
        };
        let bottom = Rectangle {
            x: 0.0,
            y: 452.0,
            width: 300.0,
            height: 48.0,
        };
        let values = Values {
            total: 20.0,
            edge: Edge::Top,
        };
        assert_eq!(row_offset(values, top, viewport), 0.0);
        assert!(row_offset(values, middle, viewport) > 1.0);
        assert!(row_offset(values, bottom, viewport) > row_offset(values, middle, viewport));
        assert!(row_offset(values, bottom, viewport) <= 6.0);

        let values = Values {
            total: -20.0,
            edge: Edge::Bottom,
        };
        assert!(row_offset(values, top, viewport) < row_offset(values, middle, viewport));
        assert!(row_offset(values, top, viewport) < -1.0);
        assert_eq!(row_offset(values, bottom, viewport), 0.0);
    }

    thread_local! {
        static RENDERER: std::cell::OnceCell<&'static iced::Renderer> =
            const { std::cell::OnceCell::new() };
    }

    /// One adapter per test thread. Creating a headless renderer per event used
    /// to take ~130 ms, which made every synthetic flick look stationary to the
    /// scrollable and to velocity tracking. The renderer is leaked so its wgpu
    /// teardown cannot run during thread-local destruction.
    fn with_renderer<T>(f: impl FnOnce(&iced::Renderer) -> T) -> T {
        RENDERER.with(|cell| {
            let renderer = *cell.get_or_init(|| {
                Box::leak(Box::new(
                    futures::executor::block_on(<iced::Renderer as Headless>::new(
                        iced::Font::DEFAULT,
                        16.0.into(),
                        None,
                    ))
                    .expect("headless wgpu adapter"),
                ))
            });
            f(renderer)
        })
    }

    fn list_content() -> Element<'static, Message> {
        let mut rows = column![];
        for index in 0..30 {
            rows = rows.push(container(iced::widget::text(format!("{index}"))).height(48.0));
        }
        rows.into()
    }

    fn list() -> Element<'static, Message> {
        scrollable(list_content())
            .id("test-scroll")
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn harness(policy: Policy) -> (Element<'static, Message>, Tree, layout::Node, Motion) {
        harness_with(policy, true)
    }

    /// `retained` builds the wrapper even while the feature switch is off, so
    /// the implementation keeps its coverage; `false` goes through the same
    /// production entry point the launcher uses.
    fn harness_with(
        policy: Policy,
        retained: bool,
    ) -> (Element<'static, Message>, Tree, layout::Node, Motion) {
        let motion = Motion::new();
        let scroller: Element<'static, Message> = scrollable(content(list_content(), &motion))
            .id("test-scroll")
            .width(Fill)
            .height(Fill)
            .into();
        let mut element = if retained {
            wrap_unconditional(scroller, motion.clone(), policy)
        } else {
            wrap(scroller, motion.clone(), policy)
        };
        let mut tree = Tree::new(&element);
        let node = with_renderer(|renderer| {
            element.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 200.0)),
            )
        });
        (element, tree, node, motion)
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn the_production_entry_point_enables_the_elastic_wrapper() {
        let (mut element, mut tree, node, motion) = harness_with(Policy::Normal, false);
        let mut scroll = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            iced::widget::Id::new("test-scroll"),
            iced::widget::scrollable::AbsoluteOffset {
                x: 0.0,
                y: 100_000.0,
            }
            .into(),
        );
        with_renderer(|renderer| {
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut scroll);
        });
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 150.0),
            Point::new(150.0, 70.0),
        );
        assert!(motion.values().total < 0.0, "{:?}", motion.values());
        assert_eq!(motion.values().edge, Edge::Bottom);
    }

    fn event(
        element: &mut Element<'static, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        event: Event,
        messages: &mut Vec<Message>,
    ) {
        send(element, tree, node, event, messages, false);
    }

    fn send(
        element: &mut Element<'static, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        event: Event,
        messages: &mut Vec<Message>,
        watch_redraw: bool,
    ) -> window::RedrawRequest {
        let mut shell = Shell::new(messages);
        // Iced maps a live finger to a cursor position; without it the
        // scrollable cannot consume touch movement.
        let cursor = match &event {
            Event::Touch(
                touch::Event::FingerPressed { position, .. }
                | touch::Event::FingerMoved { position, .. }
                | touch::Event::FingerLifted { position, .. },
            ) => mouse::Cursor::Available(*position),
            _ => mouse::Cursor::Unavailable,
        };
        with_renderer(|renderer| {
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                cursor,
                renderer,
                &mut iced::advanced::clipboard::Null,
                &mut shell,
                &Rectangle::with_size(Size::new(300.0, 200.0)),
            );
        });
        if watch_redraw {
            shell.redraw_request()
        } else {
            window::RedrawRequest::Wait
        }
    }

    fn drag(
        element: &mut Element<'static, Message>,
        tree: &mut Tree,
        node: &layout::Node,
        from: Point,
        to: Point,
    ) {
        let mut messages = Vec::new();
        event(
            element,
            tree,
            node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: from,
            }),
            &mut messages,
        );
        let steps = 8;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let position = Point::new(from.x + (to.x - from.x) * t, from.y + (to.y - from.y) * t);
            event(
                element,
                tree,
                node,
                Event::Touch(touch::Event::FingerMoved {
                    id: touch::Finger(1),
                    position,
                }),
                &mut messages,
            );
        }
        event(
            element,
            tree,
            node,
            Event::Touch(touch::Event::FingerLifted {
                id: touch::Finger(1),
                position: to,
            }),
            &mut messages,
        );
    }

    fn scrolled_to_bottom(
        policy: Policy,
    ) -> (Element<'static, Message>, Tree, layout::Node, Motion) {
        let (mut element, mut tree, node, motion) = harness(policy);
        let mut messages = Vec::new();
        let mut scroll = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            iced::widget::Id::new("test-scroll"),
            iced::widget::scrollable::AbsoluteOffset {
                x: 0.0,
                y: 100_000.0,
            }
            .into(),
        );
        with_renderer(|renderer| {
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut scroll);
        });
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Window(window::Event::RedrawRequested(Instant::now())),
            &mut messages,
        );
        assert_eq!(motion.values().total, 0.0);
        (element, tree, node, motion)
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn outward_drag_at_the_bottom_bounces_but_inward_does_not() {
        let (mut element, mut tree, node, motion) = scrolled_to_bottom(Policy::Normal);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 150.0),
            Point::new(150.0, 70.0),
        );
        assert!(motion.values().total < 0.0, "{:?}", motion.values());
        assert_eq!(motion.values().edge, Edge::Bottom);

        // Dragging back down scrolls the list away from the bottom: it may
        // stretch with its speed, but never as a bottom overscroll, and it
        // must return to rest.
        let (mut element, mut tree, node, motion) = scrolled_to_bottom(Policy::Normal);
        let mut messages = Vec::new();
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 70.0),
            Point::new(150.0, 150.0),
        );
        assert!(
            motion.values().total >= 0.0,
            "inward scrolling must not push past the bottom: {:?}",
            motion.values()
        );
        let start = Instant::now();
        for frame in 1..=240 {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Window(window::Event::RedrawRequested(
                    start + std::time::Duration::from_millis(16 * frame),
                )),
                &mut messages,
            );
            if motion.values().total == 0.0 {
                break;
            }
        }
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn a_fling_that_reaches_the_bottom_stretches_then_settles() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let mut messages = Vec::new();
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: Point::new(150.0, 180.0),
            }),
            &mut messages,
        );
        for y in [160.0, 140.0, 120.0, 100.0, 80.0] {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Touch(touch::Event::FingerMoved {
                    id: touch::Finger(1),
                    position: Point::new(150.0, y),
                }),
                &mut messages,
            );
        }
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerLifted {
                id: touch::Finger(1),
                position: Point::new(150.0, 80.0),
            }),
            &mut messages,
        );
        let start = Instant::now();
        let mut peak = 0.0_f32;
        for frame in 1..=180 {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Window(window::Event::RedrawRequested(
                    start + std::time::Duration::from_millis(16 * frame),
                )),
                &mut messages,
            );
            peak = peak.min(motion.values().total);
        }
        assert!(peak < -3.0, "fling must stretch the bottom: {peak}");
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn a_single_locking_move_past_the_edge_still_bounces() {
        // A quick flick often arrives as one large move; it must not be
        // discarded just because it also decides the gesture axis.
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let mut messages = Vec::new();
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: Point::new(150.0, 60.0),
            }),
            &mut messages,
        );
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerMoved {
                id: touch::Finger(1),
                position: Point::new(150.0, 160.0),
            }),
            &mut messages,
        );
        assert!(motion.values().total > 5.0, "{:?}", motion.values());
        assert_eq!(motion.values().edge, Edge::Top);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn outward_drag_at_the_top_bounces_and_launcher_policy_skips_it() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        assert!(motion.values().total > 0.0, "{:?}", motion.values());
        assert_eq!(motion.values().edge, Edge::Top);

        let (mut element, mut tree, node, motion) = harness(Policy::LauncherTop);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        assert_eq!(motion.values().total, 0.0);
        assert_eq!(motion.values().edge, Edge::None);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn horizontal_drag_second_finger_and_outside_touch_do_not_bounce() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let mut messages = Vec::new();
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(30.0, 100.0),
            Point::new(200.0, 108.0),
        );
        assert_eq!(motion.values().total, 0.0);

        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: Point::new(150.0, 150.0),
            }),
            &mut messages,
        );
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerMoved {
                id: touch::Finger(1),
                position: Point::new(150.0, 90.0),
            }),
            &mut messages,
        );
        let first = motion.values();
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(2),
                position: Point::new(150.0, 150.0),
            }),
            &mut messages,
        );
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerMoved {
                id: touch::Finger(2),
                position: Point::new(150.0, 20.0),
            }),
            &mut messages,
        );
        assert_eq!(motion.values(), first);

        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 300.0),
            Point::new(150.0, 200.0),
        );
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn released_bounce_settles_on_redraw_and_layout_does_not_change() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let height = node.size().height;
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        assert!(motion.values().total > 0.0);
        let start = Instant::now();
        let mut messages = Vec::new();
        let mut last = motion.values().total;
        for frame in 1..=200 {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Window(window::Event::RedrawRequested(
                    start + std::time::Duration::from_millis(16 * frame),
                )),
                &mut messages,
            );
            let now = motion.values().total;
            assert!(
                now.abs() <= last.abs() + 0.001,
                "must not grow after release"
            );
            last = now;
            if now == 0.0 {
                break;
            }
        }
        assert_eq!(motion.values().total, 0.0);
        let relaid = with_renderer(|renderer| {
            element.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 200.0)),
            )
        });
        assert_eq!(relaid.size().height, height);
        assert_eq!(relaid.size().width, 300.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn scrolling_in_the_middle_stretches_with_speed_and_settles() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let mut messages = Vec::new();
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: Point::new(150.0, 150.0),
            }),
            &mut messages,
        );
        for y in [140.0, 130.0, 120.0, 110.0, 100.0] {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Touch(touch::Event::FingerMoved {
                    id: touch::Finger(1),
                    position: Point::new(150.0, y),
                }),
                &mut messages,
            );
        }
        assert!(
            motion.values().total.abs() > 0.5,
            "a moving list should stretch away from the edges: {:?}",
            motion.values()
        );
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerLifted {
                id: touch::Finger(1),
                position: Point::new(150.0, 100.0),
            }),
            &mut messages,
        );
        let start = Instant::now();
        for frame in 1..=240 {
            event(
                &mut element,
                &mut tree,
                &node,
                Event::Window(window::Event::RedrawRequested(
                    start + std::time::Duration::from_millis(16 * frame),
                )),
                &mut messages,
            );
            if motion.values().total == 0.0 {
                break;
            }
        }
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn a_stationary_tap_does_not_bounce_and_settling_stops_requesting_frames() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        let mut messages = Vec::new();
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerPressed {
                id: touch::Finger(1),
                position: Point::new(150.0, 100.0),
            }),
            &mut messages,
        );
        event(
            &mut element,
            &mut tree,
            &node,
            Event::Touch(touch::Event::FingerLifted {
                id: touch::Finger(1),
                position: Point::new(150.0, 100.0),
            }),
            &mut messages,
        );
        assert_eq!(motion.values().total, 0.0);

        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        let start = Instant::now();
        let mut frame = 0;
        loop {
            frame += 1;
            let request = send(
                &mut element,
                &mut tree,
                &node,
                Event::Window(window::Event::RedrawRequested(
                    start + std::time::Duration::from_millis(16 * frame),
                )),
                &mut messages,
                true,
            );
            if motion.values().total == 0.0 {
                assert_eq!(request, window::RedrawRequest::Wait);
                break;
            }
            assert_ne!(request, window::RedrawRequest::Wait);
            assert!(frame < 200);
        }
        assert!(frame < 200);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn programmatic_scroll_clears_a_stale_bounce() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        assert!(motion.values().total > 0.0);
        let mut scroll = iced::advanced::widget::operation::scrollable::scroll_to::<()>(
            iced::widget::Id::new("test-scroll"),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 40.0 }.into(),
        );
        with_renderer(|renderer| {
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut scroll);
        });
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn resize_clears_a_stale_bounce() {
        let (mut element, mut tree, node, motion) = harness(Policy::Normal);
        drag(
            &mut element,
            &mut tree,
            &node,
            Point::new(150.0, 60.0),
            Point::new(150.0, 160.0),
        );
        assert!(motion.values().total > 0.0);
        with_renderer(|renderer| {
            element.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 300.0)),
            );
        });
        assert_eq!(motion.values().total, 0.0);
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn content_ignores_presses_while_displaced_but_forwards_scroll_operations() {
        use iced::advanced::widget::operation::scrollable::{scroll_by, scroll_to};
        let motion = Motion::new();
        motion.set(10.0, Edge::Top);
        let mut content = content(list(), &motion);
        let mut tree = Tree::new(&content);
        let node = with_renderer(|renderer| {
            content.as_widget_mut().layout(
                &mut tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(300.0, 200.0)),
            )
        });
        let mut messages = Vec::new();
        with_renderer(|renderer| {
            content.as_widget_mut().update(
                &mut tree,
                &Event::Touch(touch::Event::FingerPressed {
                    id: touch::Finger(1),
                    position: Point::new(150.0, 100.0),
                }),
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(150.0, 100.0)),
                renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &Rectangle::with_size(Size::new(300.0, 200.0)),
            );
        });
        assert!(messages.is_empty());

        let mut offset = scroll_to::<()>(
            iced::widget::Id::new("test-scroll"),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 40.0 }.into(),
        );
        let mut by = scroll_by::<()>(
            iced::widget::Id::new("test-scroll"),
            iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 10.0 },
        );
        with_renderer(|renderer| {
            content
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut offset);
            content
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), renderer, &mut by);
        });
    }
}
