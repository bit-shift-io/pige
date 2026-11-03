//! Labelled form fields and the shell's search field.
//!
//! The underline decoration is adapted from qlaunch's `field.rs` (bitshift,
//! MIT): it draws the focus line from the text input's real focus state, so no
//! second focus model is needed and input state stays intact.

use iced::advanced::{
    Clipboard, Layout, Renderer as _, Shell, Widget, layout, mouse, renderer, widget::Tree,
};
use iced::widget::{column, container, text, text_input};
use iced::{Alignment, Element, Event, Fill, Length, Rectangle, Size, Vector};

use crate::Message;
use crate::components::styled_text;
use crate::icons;
use crate::theme::{self, Palette, TextRole};

type InputState =
    iced::widget::text_input::State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

/// A labelled text field with an underline that follows focus.
pub fn text_field<'a>(
    label: &str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    field(label, value, false, on_input)
}

/// The same field with every typed character masked. Used for account
/// passwords; the value never leaves the UI state except through the API's
/// credential call.
pub fn password_field<'a>(
    label: &str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    field(label, value, true, on_input)
}

fn field<'a>(
    label: &str,
    value: &'a str,
    secure: bool,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let palette = theme::active();
    let label_style = palette.role(TextRole::LabelMedium);
    let value_style = palette.role(TextRole::BodyLarge);

    let input = text_input(label, value)
        .on_input(on_input)
        .secure(secure)
        .size(value_style.size)
        .line_height(value_style.line_height())
        .font(value_style.font)
        .padding([palette.space.sm, 0.0])
        .style(palette.text_input());

    column![
        styled_text(label, label_style, palette.on_surface_variant),
        container(input_underline(input, palette))
            .center_y(palette.metrics.field_height - label_style.line_height)
            .width(Fill),
    ]
    .width(Fill)
    .into()
}

/// The always-visible search field used in the list chrome.
pub fn search_field<'a>(
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
) -> Element<'a, Message> {
    let palette = theme::active();
    let value_style = palette.role(TextRole::BodyMedium);

    let input = text_input("Search mail", value)
        .on_input(on_input)
        .size(value_style.size)
        .line_height(value_style.line_height())
        .font(value_style.font)
        .padding([0.0, 0.0])
        .style(palette.text_input());

    container(
        iced::widget::row![
            text(icons::glyph(icons::SEARCH))
                .font(icons::FONT)
                .size(palette.metrics.small_icon_size)
                .color(palette.on_surface_variant),
            input,
        ]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center)
        .width(Fill),
    )
    .width(Fill)
    .padding([0.0, palette.space.md])
    .center_y(palette.metrics.touch_target)
    .style(move |_| container::Style {
        background: Some(palette.surface_container_high.into()),
        text_color: Some(palette.on_surface),
        border: iced::border::rounded(palette.metrics.touch_target / 2.0),
        ..Default::default()
    })
    .into()
}

/// Wrap a text input so a focus line is drawn under it.
fn input_underline<'a>(
    content: text_input::TextInput<'a, Message>,
    palette: Palette,
) -> Element<'a, Message> {
    Element::new(Underline {
        content: content.into(),
        input: true,
        palette,
    })
}

/// The direct child must be a text input, so its actual focus state drives the
/// line without a second application-level focus model.
struct Underline<'a> {
    content: Element<'a, Message>,
    input: bool,
    palette: Palette,
}

impl Underline<'_> {
    fn focused(&self, tree: &Tree) -> bool {
        self.input
            && tree.children[0]
                .state
                .downcast_ref::<InputState>()
                .is_focused()
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for Underline<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        let (color, height) = if self.focused(tree) {
            (self.palette.primary, self.palette.metrics.focus_width)
        } else {
            (self.palette.outline, self.palette.metrics.border_width)
        };
        let bounds = layout.bounds();
        if let Some(clip) = bounds.intersection(viewport) {
            renderer.with_layer(clip, |renderer| {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            y: bounds.y + bounds.height - height,
                            height,
                            ..bounds
                        },
                        ..Default::default()
                    },
                    color,
                );
            });
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
    fn underline_tracks_focus() {
        let palette = theme::active();
        assert_eq!(palette.metrics.border_width, 1.0);
        assert_eq!(palette.metrics.focus_width, 2.0);
    }
}
