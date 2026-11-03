//! Screen chrome: app bars, section headers, empty states and snackbars.

use iced::widget::{column, container, row, text};
use iced::{Alignment, Element, Fill};

use crate::Message;
use crate::components::label::fitted;
use crate::components::styled_text;
use crate::icons;
use crate::theme::{self, TextRole};

/// A Material app bar: an optional leading widget, a title and trailing
/// actions. The shell passes its menu button as `leading`.
pub fn app_bar<'a>(
    title: &str,
    leading: Option<Element<'a, Message>>,
    actions: Vec<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = theme::active();
    let title_style = palette.role(TextRole::TitleLarge);

    let mut bar = row![]
        .align_y(Alignment::Center)
        .spacing(palette.space.xs)
        .height(palette.metrics.app_bar_height);

    if let Some(leading) = leading {
        bar = bar.push(leading);
    }
    bar = bar.push(fitted(
        title.to_string(),
        title_style,
        palette.on_surface,
        false,
        None,
    ));
    for action in actions {
        bar = bar.push(action);
    }

    container(bar)
        .width(Fill)
        .padding([0.0, palette.space.xs])
        .style(move |_| container::Style {
            background: Some(palette.surface.into()),
            text_color: Some(palette.on_surface),
            ..Default::default()
        })
        .into()
}

/// A centred empty state with a mail glyph, title and supporting body.
pub fn empty_state<'a>(title: &str, body: &str) -> Element<'a, Message> {
    let palette = theme::active();
    let title_style = palette.role(TextRole::TitleLarge);
    let body_style = palette.role(TextRole::BodyMedium);

    container(
        column![
            text(icons::glyph(icons::MAIL))
                .font(icons::FONT)
                .size(palette.metrics.empty_icon_size)
                .color(palette.outline),
            styled_text(title.to_string(), title_style, palette.on_surface)
                .width(Fill)
                .align_x(Alignment::Center),
            text(body.to_string())
                .size(body_style.size)
                .line_height(body_style.line_height())
                .font(body_style.font)
                .color(palette.on_surface_variant)
                .width(Fill)
                .align_x(Alignment::Center),
        ]
        .spacing(palette.space.sm)
        .align_x(Alignment::Center)
        .width(Fill),
    )
    .width(Fill)
    .height(Fill)
    .padding(palette.space.xl)
    .center(Fill)
    .into()
}

/// A transient message bar.
pub fn snackbar<'a>(message: &str) -> Element<'a, Message> {
    let palette = theme::active();
    let style = palette.role(TextRole::BodyMedium);

    container(styled_text(
        message.to_string(),
        style,
        palette.on_inverse_surface,
    ))
    .width(Fill)
    .padding([0.0, palette.space.lg])
    .center_y(palette.metrics.snackbar_height)
    .style(move |_| container::Style {
        background: Some(palette.inverse_surface.into()),
        text_color: Some(palette.on_inverse_surface),
        border: iced::border::rounded(palette.radius.sm),
        ..Default::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bar_height_comes_from_the_palette() {
        assert_eq!(theme::active().metrics.app_bar_height, 64.0);
    }
}
