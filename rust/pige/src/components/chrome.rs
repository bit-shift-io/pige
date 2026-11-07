//! Screen chrome: app bars, section headers, empty states and snackbars.

use iced::widget::{column, container, row, space, text};
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

    bar_surface(bar)
}

/// An app bar without a title: a leading widget, left-aligned action buttons
/// that fill from the title's old space, and an optional trailing widget pinned
/// to the far right (the reader's overflow button).
pub fn app_bar_actions<'a>(
    leading: Option<Element<'a, Message>>,
    actions: Vec<Element<'a, Message>>,
    trailing: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = theme::active();

    let mut bar = row![]
        .align_y(Alignment::Center)
        .spacing(palette.space.xs)
        .height(palette.metrics.app_bar_height);

    if let Some(leading) = leading {
        bar = bar.push(leading);
    }
    for action in actions {
        bar = bar.push(action);
    }
    bar = bar.push(space().width(Fill));
    if let Some(trailing) = trailing {
        bar = bar.push(trailing);
    }

    bar_surface(bar)
}

/// Wrap an app bar's row in the shared surface, padding and height. Kept private
/// so [`app_bar`] and [`app_bar_actions`] cannot drift.
fn bar_surface<'a>(bar: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    let palette = theme::active();
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

/// The rounded dark card behind the toast, shared so a test can pin its colour.
/// Adapted from qlaunch's `components::toast_style` (bitshift, MIT).
pub fn toast_style() -> container::Style {
    let palette = theme::active();
    container::Style {
        background: Some(palette.surface_container_highest.into()),
        text_color: Some(palette.on_surface),
        border: iced::border::rounded(palette.metrics.toast_radius),
        shadow: iced::Shadow {
            color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.30),
            offset: iced::Vector::new(0.0, 6.0),
            blur_radius: 12.0,
        },
        ..Default::default()
    }
}

/// A small, dark, content-hugging message card used by the shell's toast
/// channel, so a brief result never paints the screen. Adapted from qlaunch's
/// `components::toast` (bitshift, MIT).
pub fn toast<'a>(message: &str) -> Element<'a, Message> {
    let palette = theme::active();
    let style = palette.role(TextRole::BodyMedium);

    container(styled_text(message.to_string(), style, palette.on_surface))
        .max_width(palette.metrics.toast_max_width)
        .padding([14.0, 16.0])
        .style(|_| toast_style())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bar_height_comes_from_the_palette() {
        assert_eq!(theme::active().metrics.app_bar_height, 64.0);
    }

    #[test]
    fn toast_style_is_a_dark_surface() {
        let palette = theme::active();
        let style = toast_style();
        assert_eq!(
            style.background,
            Some(palette.surface_container_highest.into())
        );
        assert_eq!(style.text_color, Some(palette.on_surface));
    }

    #[test]
    #[ignore = "requires a headless wgpu adapter; run explicitly with --ignored"]
    fn toast_hugs_its_text_height() {
        use iced::advanced::{layout, renderer::Headless, widget::Tree};
        let renderer = futures::executor::block_on(<iced::Renderer as Headless>::new(
            iced::Font::DEFAULT,
            16.0.into(),
            None,
        ))
        .expect("headless wgpu adapter");
        let mut control = toast("Backup saved.");
        let mut tree = Tree::new(&control);
        let node = control.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(360.0, 1000.0)),
        );
        // 1000 px is the layout limit; a Fill height would hit it.
        assert!(node.bounds().height < 100.0, "toast stretched: {node:?}");
        assert!(node.bounds().width <= 360.0, "toast exceeded the width limit");
    }
}
