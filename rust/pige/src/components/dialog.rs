//! Modal dialog cards: a scrim, an elevated card, a title, body and actions.

use iced::widget::{column, container, row, space};
use iced::{Alignment, Element, Fill, Length, Shadow, Vector};

use crate::Message;
use crate::components::styled_text;
use crate::theme::{self, TextRole};

/// A centered modal card over a click-blocking scrim. Screens place this on
/// top of their content with `iced::widget::stack`.
pub fn dialog<'a>(
    title: &str,
    body: Element<'a, Message>,
    actions: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let title_style = palette.role(TextRole::TitleLarge);

    let mut action_row = row![space().width(Fill)]
        .spacing(palette.space.sm)
        .align_y(Alignment::Center)
        .width(Fill);
    for action in actions {
        action_row = action_row.push(action);
    }

    let card = container(
        column![
            styled_text(title.to_string(), title_style, palette.on_surface),
            body,
            action_row,
        ]
        .spacing(palette.space.lg)
        .width(Fill),
    )
    .width(Fill)
    .max_width(palette.metrics.dialog_width)
    .padding(palette.metrics.dialog_padding)
    .style(move |_| container::Style {
        background: Some(palette.surface_container_high.into()),
        text_color: Some(palette.on_surface),
        border: iced::border::rounded(palette.radius.xl),
        shadow: Shadow {
            color: palette.shadow,
            offset: Vector::new(0.0, palette.metrics.dialog_shadow_y),
            blur_radius: palette.metrics.dialog_shadow_blur,
        },
        ..Default::default()
    });

    container(card)
        .width(Fill)
        .height(Fill)
        .padding(palette.space.xl)
        .center(Length::Fill)
        .style(move |_| container::Style {
            background: Some(palette.scrim.into()),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogs_are_bounded_for_compact_windows() {
        let palette = theme::active();
        assert_eq!(palette.metrics.dialog_width, 480.0);
        assert!(palette.metrics.dialog_padding > 0.0);
    }
}
