//! Dropdown menus: an elevated card of rows, and a scrimmed overlay pinned to
//! a corner. Screens describe the entries; this module owns the layout.
//!
//! [`menu`] builds just the card, for callers that position it themselves;
//! [`menu_overlay`] adds the click-blocking scrim and the corner anchoring the
//! app bars use.

use iced::widget::{MouseArea, button, column, container, row, space, stack, text};
use iced::{Alignment, Element, Fill, Padding};

use crate::Message;
use crate::components::styled_text;
use crate::icons;
use crate::theme::{self, TextRole};

/// One entry in a dropdown menu. `Message` is a large value type, so an action
/// variant outweighs a divider; boxing every entry would allocate for no gain.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum MenuItem {
    /// A tappable row with a leading icon and a label.
    Action {
        icon: char,
        label: String,
        message: Message,
    },
    /// A full-width separator between groups of rows.
    Divider,
}

impl MenuItem {
    /// An action row, so callers need not spell out the struct.
    pub fn action(icon: char, label: impl Into<String>, message: Message) -> Self {
        Self::Action {
            icon,
            label: label.into(),
            message,
        }
    }
}

/// Which corner a popup pins to. The top corners also clear the app bar so the
/// card lands just below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAnchor {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// The menu card: an elevated surface with one fixed-height row per entry.
pub fn menu<'a>(items: Vec<MenuItem>) -> Element<'a, Message> {
    let palette = theme::active();
    let rows: Vec<Element<'a, Message>> = items.into_iter().map(row_for).collect();

    container(column(rows).spacing(0).width(Fill))
        .width(palette.metrics.menu_width)
        .padding(palette.space.xs)
        .style(move |_| container::Style {
            background: Some(palette.surface_container_high.into()),
            text_color: Some(palette.on_surface),
            border: iced::border::rounded(palette.radius.md),
            shadow: iced::Shadow {
                color: palette.shadow,
                offset: iced::Vector::new(0.0, palette.metrics.dialog_shadow_y),
                blur_radius: palette.metrics.dialog_shadow_blur,
            },
            ..Default::default()
        })
        .into()
}

/// A menu plus a full-screen scrim that dismisses it when tapped, pinned to the
/// chosen corner.
pub fn menu_overlay<'a>(
    items: Vec<MenuItem>,
    anchor: MenuAnchor,
    on_dismiss: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    let card = menu(items);
    let top = palette.metrics.app_bar_height + palette.space.xs;
    let side = palette.space.md;
    let (align_x, align_y, padding) = match anchor {
        MenuAnchor::TopRight => (
            Alignment::End,
            Alignment::Start,
            Padding {
                top,
                right: side,
                bottom: 0.0,
                left: 0.0,
            },
        ),
        MenuAnchor::TopLeft => (
            Alignment::Start,
            Alignment::Start,
            Padding {
                top,
                right: 0.0,
                bottom: 0.0,
                left: side,
            },
        ),
        MenuAnchor::BottomRight => (
            Alignment::End,
            Alignment::End,
            Padding {
                top: 0.0,
                right: side,
                bottom: side,
                left: 0.0,
            },
        ),
        MenuAnchor::BottomLeft => (
            Alignment::Start,
            Alignment::End,
            Padding {
                top: 0.0,
                right: 0.0,
                bottom: side,
                left: side,
            },
        ),
    };

    let positioned = container(card)
        .width(Fill)
        .height(Fill)
        .align_x(align_x)
        .align_y(align_y)
        .padding(padding);

    let scrim = MouseArea::new(
        container(space())
            .width(Fill)
            .height(Fill)
            .style(crate::components::scrim_style()),
    )
    .on_press(on_dismiss);

    stack![scrim, positioned].into()
}

fn row_for<'a>(item: MenuItem) -> Element<'a, Message> {
    match item {
        MenuItem::Action {
            icon,
            label,
            message,
        } => action_row(icon, &label, message),
        MenuItem::Divider => divider(),
    }
}

/// One action row: a leading icon and a label, one touch target tall.
fn action_row<'a>(icon: char, label: &str, on_press: Message) -> Element<'a, Message> {
    let palette = theme::active();
    let label_style = palette.role(TextRole::BodyMedium);

    let content = row![
        text(icons::glyph(icon))
            .font(icons::FONT)
            .size(palette.metrics.nav_icon_size)
            .color(palette.on_surface_variant),
        styled_text(label.to_string(), label_style, palette.on_surface).width(Fill),
    ]
    .spacing(palette.space.md)
    .align_y(Alignment::Center)
    .width(Fill);

    button(
        container(content)
            .width(Fill)
            .padding([0.0, palette.space.md])
            .center_y(palette.metrics.touch_target),
    )
    .width(Fill)
    .padding(0)
    .style(move |_, status| {
        let background = match status {
            button::Status::Hovered | button::Status::Pressed => palette.surface_container_high,
            _ => iced::Color::TRANSPARENT,
        };
        button::Style {
            background: Some(background.into()),
            text_color: palette.on_surface,
            border: iced::border::rounded(palette.radius.xs),
            ..Default::default()
        }
    })
    .on_press(on_press)
    .into()
}

/// A 1-point full-width separator.
fn divider<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    container(space())
        .width(Fill)
        .height(palette.metrics.border_width)
        .style(move |_| container::Style {
            background: Some(palette.outline_variant.into()),
            ..Default::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A menu row must be one touch target tall: if it used a Fill height, a
    /// short menu would stretch its few rows across the whole screen.
    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn menu_rows_are_one_touch_target_tall() {
        use iced::advanced::{layout, renderer::Headless, widget::Tree};
        use iced::{Font, Size};

        let renderer = futures::executor::block_on(<iced::Renderer as Headless>::new(
            Font::DEFAULT,
            16.0.into(),
            None,
        ))
        .expect("headless wgpu adapter");
        let mut element = action_row(icons::MORE_VERT, "Move to", Message::Noop);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(400.0, 1000.0)),
        );
        assert_eq!(
            node.size().height,
            theme::active().metrics.touch_target,
            "{node:?}"
        );
    }
}
