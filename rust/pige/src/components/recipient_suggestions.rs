//! Caret-aware recipient fields and the compose autocomplete list.
//!
//! Iced's single-line text input keeps its cursor inside the widget tree's
//! state, which a screen cannot read while building the next view. The scoped
//! [`recipient_field`] wrapper below forwards every event to the real input and
//! then reports the cursor as a UTF-8 byte offset. That keeps compose's token
//! logic independent of Iced's grapheme indexes without reworking other fields.

use std::cell::RefCell;
use std::rc::Rc;

use iced::advanced::widget::Tree;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, renderer};
use iced::keyboard;
use iced::widget::{button, column, container, text_input};
use iced::{Element, Fill, Rectangle, Size, Vector};
use pige_core::account::AccountId;
use pige_core::api::{RecipientSuggestion, SuggestionSource};

use crate::Message;
use crate::components::styled_text;
use crate::theme::{self, TextRole};

type InputState =
    iced::widget::text_input::State<<iced::Renderer as iced::advanced::text::Renderer>::Paragraph>;

/// Keyboard actions for the visible suggestion list. They are built by compose
/// for the current request, so pressing Enter chooses exactly the highlighted
/// row instead of a captured snapshot.
#[derive(Debug, Clone)]
pub struct SuggestionNavigation {
    pub previous: Message,
    pub next: Message,
    pub choose: Message,
    pub dismiss: Message,
}

/// The recipient field plus the caret adapter.
pub fn recipient_field<'a>(
    label: &str,
    value: &'a str,
    on_input: impl Fn(String) -> Message + 'a,
    on_caret: impl Fn(usize) -> Message + 'a,
    navigation: Option<SuggestionNavigation>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let label_style = palette.role(TextRole::LabelMedium);
    let value_style = palette.role(TextRole::BodyLarge);

    // The input's `on_input` runs inside the widget update with the freshly
    // edited text. Stash it so the wrapper can convert the cursor against the
    // same value even when Iced's widget copy is one frame behind the view.
    let latest_input = Rc::new(RefCell::new(None));
    let latest_for_input = Rc::clone(&latest_input);
    let input = text_input(label, value)
        .on_input(move |value: String| {
            *latest_for_input.borrow_mut() = Some(value.clone());
            on_input(value)
        })
        .size(value_style.size)
        .line_height(value_style.line_height())
        .font(value_style.font)
        .padding([palette.space.sm, 0.0])
        .style(palette.text_input());

    let field = RecipientField {
        content: input.into(),
        value,
        latest_input,
        on_caret: Box::new(on_caret),
        navigation,
    };

    column![
        styled_text(label, label_style, palette.on_surface_variant),
        container(Element::new(field))
            .center_y(palette.metrics.field_height - label_style.line_height)
            .width(Fill),
    ]
    .width(Fill)
    .into()
}

/// Display the core's local results under the active recipient field. Both
/// touch (button press) and keyboard (Enter) dispatch `on_choose` with an
/// index into this same slice.
pub fn suggestion_panel<'a>(
    suggestions: &'a [RecipientSuggestion],
    selected: usize,
    account: &'a AccountId,
    on_choose: impl Fn(usize) -> Message + 'a,
) -> Element<'a, Message> {
    let palette = theme::active();
    let selected = selected.min(suggestions.len().saturating_sub(1));

    let mut list = column![].spacing(0).width(Fill);
    for (index, suggestion) in suggestions.iter().enumerate() {
        let title_style = palette.role(TextRole::TitleSmall);
        let email_style = palette.role(TextRole::BodySmall);
        let hint_style = palette.role(TextRole::LabelSmall);

        let display_name = suggestion.address.display_name();
        let show_name = suggestion
            .address
            .name
            .as_ref()
            .is_some_and(|name| !name.is_empty());
        let mut labels = column![].spacing(palette.space.xxs).width(Fill);
        if show_name {
            labels = labels.push(styled_text(display_name, title_style, palette.on_surface));
        }
        labels = labels.push(styled_text(
            suggestion.address.email.clone(),
            if show_name { email_style } else { title_style },
            if show_name {
                palette.on_surface_variant
            } else {
                palette.on_surface
            },
        ));
        if let Some(hint) = source_hint(suggestion, account) {
            labels = labels.push(styled_text(hint, hint_style, palette.on_surface_variant));
        }

        let is_selected = index == selected;
        let row = button(labels)
            .width(Fill)
            .padding([palette.space.sm, palette.space.md])
            .style(move |_, status| {
                let background = match status {
                    button::Status::Hovered | button::Status::Pressed => {
                        palette.surface_container_high
                    }
                    _ if is_selected => palette.secondary_container,
                    _ => palette.surface,
                };
                button::Style {
                    background: Some(background.into()),
                    text_color: palette.on_surface,
                    border: iced::Border {
                        color: if is_selected {
                            palette.primary
                        } else {
                            palette.outline_variant
                        },
                        width: palette.metrics.border_width,
                        radius: palette.radius.md.into(),
                    },
                    ..Default::default()
                }
            })
            .on_press(on_choose(index));
        list = list.push(row);
    }

    container(list)
        .width(Fill)
        .padding(palette.space.xxs)
        .style(move |_| container::Style {
            background: Some(palette.surface.into()),
            border: iced::Border {
                color: palette.outline_variant,
                width: palette.metrics.border_width,
                radius: palette.radius.md.into(),
            },
            ..Default::default()
        })
        .into()
}

fn source_hint(suggestion: &RecipientSuggestion, account: &AccountId) -> Option<String> {
    let mut parts: Vec<&'static str> = suggestion
        .sources
        .iter()
        .map(|source| match source {
            SuggestionSource::SavedContact | SuggestionSource::OtherContact => "Contact",
            SuggestionSource::Outgoing => "Sent mail",
            SuggestionSource::Incoming => "Received mail",
            SuggestionSource::Selected => "Chosen before",
        })
        .collect();
    parts.sort_unstable();
    parts.dedup();
    if suggestion.accounts.iter().any(|other| other != account) {
        parts.push("Other account");
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Persistent wrapper state: the last byte offset published to the app.
#[derive(Debug, Default)]
struct CaretState {
    last_caret: Option<usize>,
}

struct RecipientField<'a> {
    content: Element<'a, Message>,
    value: &'a str,
    latest_input: Rc<RefCell<Option<String>>>,
    on_caret: Box<dyn Fn(usize) -> Message + 'a>,
    navigation: Option<SuggestionNavigation>,
}

impl RecipientField<'_> {
    fn intercept_navigation(&self, event: &iced::Event, shell: &mut Shell<'_, Message>) -> bool {
        let Some(navigation) = &self.navigation else {
            return false;
        };
        let iced::Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) = event else {
            return false;
        };
        let message = match key.as_ref() {
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => navigation.previous.clone(),
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => navigation.next.clone(),
            keyboard::Key::Named(keyboard::key::Named::Enter) => navigation.choose.clone(),
            keyboard::Key::Named(keyboard::key::Named::Escape) => navigation.dismiss.clone(),
            _ => return false,
        };
        shell.publish(message);
        shell.capture_event();
        true
    }
}

impl Widget<Message, iced::Theme, iced::Renderer> for RecipientField<'_> {
    fn size(&self) -> Size<iced::Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<iced::Length> {
        self.content.as_widget().size_hint()
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<CaretState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(CaretState::default())
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
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // While a list is visible, arrows/Enter/Escape belong to the chooser
        // instead of the text input's cursor and focus handling.
        if self.intercept_navigation(event, shell) {
            return;
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

        let changed = self.latest_input.borrow_mut().take();
        let text = changed.as_deref().unwrap_or(self.value).to_string();
        let caret = {
            let state = tree.children[0].state.downcast_ref::<InputState>();
            let cursor = state.cursor();
            let grapheme = match cursor.state(&text_input::Value::new(&text)) {
                iced::widget::text_input::cursor::State::Index(index) => index,
                iced::widget::text_input::cursor::State::Selection { start, end } => start.max(end),
            };
            grapheme_caret_to_byte(&text, grapheme)
        };
        let state = tree.state.downcast_mut::<CaretState>();
        // Pointer events republish even an unchanged offset so clicking from
        // one recipient field to another always switches the active field.
        let pointer = matches!(
            event,
            iced::Event::Mouse(
                iced::mouse::Event::ButtonPressed(_) | iced::mouse::Event::ButtonReleased(_)
            ) | iced::Event::Touch(
                iced::touch::Event::FingerPressed { .. } | iced::touch::Event::FingerLifted { .. }
            )
        );
        if changed.is_some() || pointer || state.last_caret != Some(caret) {
            state.last_caret = Some(caret);
            shell.publish((self.on_caret)(caret));
        }
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

/// Keep byte offsets on UTF-8 boundaries; the caret adapter only ever supplies
/// positions derived from Iced's own text value, but a platform index is still
/// clamped before it reaches the screen.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Convert Iced's grapheme cursor index into the UTF-8 byte offset the compose
/// token scanner uses. `Value::until` applies the same grapheme segmentation as
/// the text input, so emoji and combining marks never split a character.
fn grapheme_caret_to_byte(text: &str, grapheme_index: usize) -> usize {
    let value = text_input::Value::new(text);
    floor_char_boundary(text, value.until(grapheme_index).to_string().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_adapter_returns_utf8_byte_offsets() {
        let text = "Zoë 日本語";
        let value = text_input::Value::new(text);
        let graphemes = value.len();

        assert_eq!(grapheme_caret_to_byte(text, 0), 0);
        assert_eq!(grapheme_caret_to_byte(text, graphemes), text.len());
        assert_eq!(grapheme_caret_to_byte(text, usize::MAX), text.len());
        for grapheme in 0..=graphemes {
            let byte = grapheme_caret_to_byte(text, grapheme);
            assert!(text.is_char_boundary(byte), "grapheme {grapheme} split");
            assert!(byte <= text.len());
        }
    }

    #[test]
    fn caret_adapter_never_slices_a_combining_cluster() {
        let text = "e\u{301}x";
        let value = text_input::Value::new(text);
        let bytes: Vec<usize> = (0..=value.len())
            .map(|index| grapheme_caret_to_byte(text, index))
            .collect();
        assert_eq!(bytes, vec![0, 3, 4]);
        assert!(bytes.iter().all(|byte| text.is_char_boundary(*byte)));
    }
}
