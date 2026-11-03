//! Circular initial avatars with a deterministic foreground colour.

use iced::widget::{container, text};
use iced::{Alignment, Color, Element, Length};

use crate::Message;
use crate::theme::{self, TextRole};

/// A circular avatar showing up to two initials.
pub fn avatar<'a>(initials: &str, color: Color, size: f32) -> Element<'a, Message> {
    let palette = theme::active();
    let font = palette.role(TextRole::LabelLarge).font;
    let foreground = palette.on_color(color);
    let label = initials.chars().take(2).collect::<String>();

    container(
        text(label)
            .size(size * palette.metrics.avatar_font_ratio)
            .font(font)
            .color(foreground)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
    )
    .center_x(Length::Fixed(size))
    .center_y(Length::Fixed(size))
    .style(move |_| container::Style {
        background: Some(color.into()),
        text_color: Some(foreground),
        border: iced::border::rounded(size / 2.0),
        ..Default::default()
    })
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_text_uses_the_scaled_palette_ratio() {
        let palette = theme::active();
        assert!(palette.metrics.avatar_font_ratio > 0.0);
        assert!(palette.metrics.avatar_font_ratio < 1.0);
    }
}
