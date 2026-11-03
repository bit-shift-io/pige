//! Choice chips and attachment chips.

use iced::widget::{button, container, row, text};
use iced::{Alignment, Color, Element};

use crate::Message;
use crate::components::styled_text;
use crate::icons;
use crate::theme::{self, TextRole};

/// A selectable filter chip. Selected chips carry a leading tick.
pub fn chip<'a>(label: &str, selected: bool, on_press: Message) -> Element<'a, Message> {
    let palette = theme::active();
    let style = palette.role(TextRole::LabelLarge);

    let mut content = row![].spacing(palette.space.xs).align_y(Alignment::Center);
    if selected {
        content = content.push(
            text(icons::glyph(icons::CHECK))
                .font(icons::FONT)
                .size(palette.metrics.small_icon_size)
                .color(palette.on_secondary_container),
        );
    }
    content = content.push(styled_text(
        label.to_string(),
        style,
        if selected {
            palette.on_secondary_container
        } else {
            palette.on_surface_variant
        },
    ));

    button(container(content).center_y(palette.metrics.chip_height.max(style.line_height)))
        .height(palette.metrics.chip_height.max(style.line_height))
        .padding([0.0, palette.space.md])
        .style(move |_, status| {
            let alpha = match status {
                button::Status::Hovered => palette.states.hover,
                button::Status::Pressed => palette.states.pressed,
                _ => 0.0,
            };
            if selected {
                button::Style {
                    background: Some(palette.secondary_container.into()),
                    text_color: palette.on_secondary_container,
                    border: iced::border::rounded(palette.metrics.chip_height / 2.0),
                    ..Default::default()
                }
            } else {
                button::Style {
                    background: Some(
                        Color {
                            a: alpha,
                            ..palette.on_surface
                        }
                        .into(),
                    ),
                    text_color: palette.on_surface_variant,
                    border: iced::Border {
                        color: palette.outline,
                        width: palette.metrics.border_width,
                        radius: (palette.metrics.chip_height / 2.0).into(),
                    },
                    ..Default::default()
                }
            }
        })
        .on_press(on_press)
        .into()
}

/// A file chip shown under a message: glyph, file name and human size.
pub fn attachment_chip<'a>(name: &str, size: u64, on_press: Message) -> Element<'a, Message> {
    let palette = theme::active();
    let name_style = palette.role(TextRole::LabelLarge);
    let size_style = palette.role(TextRole::LabelMedium);
    let content = row![
        text(icons::glyph(icons::ATTACH_FILE))
            .font(icons::FONT)
            .size(palette.metrics.small_icon_size)
            .color(palette.primary),
        styled_text(name.to_string(), name_style, palette.on_surface),
        styled_text(format_size(size), size_style, palette.on_surface_variant),
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center);

    button(
        container(content)
            .padding([0.0, palette.space.md])
            .center_y(palette.metrics.chip_height.max(name_style.line_height)),
    )
    .padding(0)
    .style(move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(
                if hovered {
                    palette.surface_container_highest
                } else {
                    palette.surface_container_high
                }
                .into(),
            ),
            text_color: palette.on_surface,
            border: iced::border::rounded(palette.radius.sm),
            ..Default::default()
        }
    })
    .on_press(on_press)
    .into()
}

/// Formats a byte count for an attachment chip.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        let short = format!("{value:.1}");
        format!("{} {}", short.trim_end_matches(".0"), UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_sizes_are_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(10 * 1024), "10 KB");
        assert_eq!(format_size(5 * 1024 * 1024 / 2), "2.5 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3 GB");
    }
}
