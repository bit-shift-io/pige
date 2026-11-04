//! Action buttons: filled, tonal, outlined, text and destructive variants.

use iced::widget::{button as widget_button, container, tooltip};
use iced::{Color, Element, Fill};

use crate::Message;
use crate::components::styled_text;
use crate::icons;
use crate::theme::{self, Palette, TextRole};

/// The emphasis of an action button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    Filled,
    Tonal,
    Outlined,
    Text,
    Danger,
}

fn style(
    palette: Palette,
    variant: ButtonVariant,
) -> impl Fn(&iced::Theme, widget_button::Status) -> widget_button::Style {
    move |_, status| {
        let is_disabled = matches!(status, widget_button::Status::Disabled);
        let alpha = match status {
            widget_button::Status::Hovered => palette.states.hover,
            widget_button::Status::Pressed => palette.states.pressed,
            _ => 0.0,
        };
        let state_layer = Color {
            a: alpha,
            ..palette.primary
        };

        let (background, text_color, border) = match variant {
            ButtonVariant::Filled => (
                if is_disabled {
                    palette.disabled(palette.on_surface)
                } else {
                    palette.primary
                },
                if is_disabled {
                    palette.surface
                } else {
                    palette.on_primary
                },
                iced::border::rounded(palette.metrics.action_height / 2.0),
            ),
            ButtonVariant::Tonal => (
                if is_disabled {
                    palette.disabled(palette.on_surface)
                } else {
                    palette.secondary_container
                },
                if is_disabled {
                    palette.surface
                } else {
                    palette.on_secondary_container
                },
                iced::border::rounded(palette.metrics.action_height / 2.0),
            ),
            ButtonVariant::Outlined => (
                state_layer,
                if is_disabled {
                    palette.disabled(palette.on_surface)
                } else {
                    palette.primary
                },
                iced::Border {
                    color: if is_disabled {
                        palette.disabled(palette.outline)
                    } else {
                        palette.outline
                    },
                    width: palette.metrics.border_width,
                    radius: (palette.metrics.action_height / 2.0).into(),
                },
            ),
            ButtonVariant::Text => (
                state_layer,
                if is_disabled {
                    palette.disabled(palette.on_surface)
                } else {
                    palette.primary
                },
                iced::border::rounded(palette.metrics.action_height / 2.0),
            ),
            ButtonVariant::Danger => (
                if is_disabled {
                    palette.disabled(palette.on_surface)
                } else {
                    palette.error
                },
                if is_disabled {
                    palette.surface
                } else {
                    palette.on_error
                },
                iced::border::rounded(palette.metrics.action_height / 2.0),
            ),
        };

        widget_button::Style {
            background: Some(background.into()),
            text_color,
            border,
            ..Default::default()
        }
    }
}

/// A labelled action button.
pub fn button<'a>(
    label: impl Into<String>,
    variant: ButtonVariant,
    on_press: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    let label = label.into();
    let label_style = palette.role(TextRole::LabelLarge);
    let text_color = match variant {
        ButtonVariant::Filled => palette.on_primary,
        ButtonVariant::Tonal => palette.on_secondary_container,
        ButtonVariant::Outlined | ButtonVariant::Text => palette.primary,
        ButtonVariant::Danger => palette.on_error,
    };

    widget_button(
        container(styled_text(label, label_style, text_color))
            .padding([0.0, palette.space.lg])
            .center_y(Fill),
    )
    .height(palette.metrics.action_height)
    .padding(0)
    .style(style(palette, variant))
    .on_press(on_press)
    .into()
}

/// A square icon button with a tooltip and touch-sized hit area.
pub fn icon_button<'a>(glyph: char, tooltip: &str, on_press: Message) -> Element<'a, Message> {
    let palette = theme::active();
    let glyph = glyph.to_string();
    let tooltip = tooltip.to_string();

    let content = iced::widget::text(glyph)
        .font(icons::FONT)
        .size(palette.metrics.icon_size)
        .color(palette.on_surface_variant);

    let button = widget_button(container(content).center(Fill))
        .width(palette.metrics.touch_target)
        .height(palette.metrics.touch_target)
        .padding(0)
        .style(move |_, status| {
            let alpha = match status {
                widget_button::Status::Hovered => palette.states.hover,
                widget_button::Status::Pressed => palette.states.pressed,
                _ => 0.0,
            };
            widget_button::Style {
                background: Some(
                    Color {
                        a: alpha,
                        ..palette.on_surface
                    }
                    .into(),
                ),
                text_color: if matches!(status, widget_button::Status::Disabled) {
                    palette.disabled(palette.on_surface_variant)
                } else {
                    palette.on_surface_variant
                },
                border: iced::border::rounded(palette.metrics.touch_target / 2.0),
                ..Default::default()
            }
        })
        .on_press(on_press);

    let tooltip_style = palette.role(TextRole::LabelSmall);
    let tip = container(styled_text(
        tooltip,
        tooltip_style,
        palette.on_inverse_surface,
    ))
    .padding([palette.space.xs, palette.space.sm])
    .style(move |_| container::Style {
        background: Some(palette.inverse_surface.into()),
        text_color: Some(palette.on_inverse_surface),
        border: iced::border::rounded(palette.radius.xs),
        ..Default::default()
    });

    iced::widget::tooltip(button, tip, tooltip::Position::Bottom)
        .gap(palette.space.xs)
        .into()
}

/// A circular floating action button for compact layouts: filled primary
/// colours, a touch-plus target, and a top tooltip. Placed bottom-end over the
/// message list by `view`.
pub fn floating_action_button<'a>(
    glyph: char,
    tooltip: &str,
    on_press: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    let size = palette.metrics.touch_target + palette.space.sm;
    let glyph = glyph.to_string();
    let tooltip = tooltip.to_string();

    let content = iced::widget::text(glyph)
        .font(icons::FONT)
        .size(palette.metrics.icon_size)
        .color(palette.on_primary);

    let button = widget_button(container(content).center(Fill))
        .width(size)
        .height(size)
        .padding(0)
        .style(move |_, status| {
            let background = match status {
                widget_button::Status::Hovered | widget_button::Status::Pressed => {
                    palette.primary_container
                }
                widget_button::Status::Disabled => palette.disabled(palette.on_surface),
                _ => palette.primary,
            };
            widget_button::Style {
                background: Some(background.into()),
                text_color: palette.on_primary,
                border: iced::border::rounded(size / 2.0),
                ..Default::default()
            }
        })
        .on_press(on_press);

    let tip = container(styled_text(
        tooltip,
        palette.role(TextRole::LabelSmall),
        palette.on_inverse_surface,
    ))
    .padding([palette.space.xs, palette.space.sm])
    .style(move |_| container::Style {
        background: Some(palette.inverse_surface.into()),
        text_color: Some(palette.on_inverse_surface),
        border: iced::border::rounded(palette.radius.xs),
        ..Default::default()
    });

    iced::widget::tooltip(button, tip, tooltip::Position::Top)
        .gap(palette.space.xs)
        .into()
}

/// The height of a standard action button.
pub fn height() -> f32 {
    theme::active().metrics.action_height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_draws_with_the_active_palette() {
        let palette = theme::active();
        for variant in [
            ButtonVariant::Filled,
            ButtonVariant::Tonal,
            ButtonVariant::Outlined,
            ButtonVariant::Text,
            ButtonVariant::Danger,
        ] {
            let active = style(palette, variant)(&iced::Theme::Dark, widget_button::Status::Active);
            assert!(active.background.is_some());
            assert!(active.border.radius.top_left > 0.0);
            assert_eq!(active.text_color.a, 1.0);
        }

        // Text-like variants use a state layer, so pressing is visible.
        for variant in [ButtonVariant::Outlined, ButtonVariant::Text] {
            let active = style(palette, variant)(&iced::Theme::Dark, widget_button::Status::Active);
            let pressed =
                style(palette, variant)(&iced::Theme::Dark, widget_button::Status::Pressed);
            assert_ne!(active.background, pressed.background);
        }
    }

    #[test]
    fn buttons_are_40_points_high() {
        assert_eq!(height(), 40.0);
    }
}
