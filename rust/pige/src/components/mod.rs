//! The shared Material 3 component kit.
//!
//! Screens compose these builders and own only layout and state glue. Every
//! colour, radius, spacing value and text size comes from `theme::Palette`.
//!
//! The widget split follows responsibility rather than screen:
//! - [`buttons`] action buttons
//! - [`fields`] labelled and search text fields
//! - [`avatar`] deterministic initial avatars
//! - [`rows`] message-list and folder rows
//! - [`chrome`] app bars, section headers, empty states and toasts
//! - [`menu`] dropdown/overflow menus
//! - [`chips`] choice and attachment chips
//! - [`dialog`] modal cards
//!
//! The custom-drawn pieces iced lacks live alongside them: [`label`]'s
//! measured ellipsized label, [`scroll_fade`], [`elastic_scroll`] and
//! [`pull_refresh`]. The builders and custom widgets are adapted from
//! qlaunch's `components.rs` and companion modules (bitshift, MIT).

use iced::Color;
use iced::widget::{button as iced_button, column, container, row, text};

use crate::theme::{self, TextRole, Typography};
use crate::{Message, icons};

pub mod avatar;
pub mod buttons;
pub mod chips;
pub mod chrome;
pub mod dialog;
pub mod elastic_scroll;
pub mod fields;
pub mod label;
pub mod menu;
pub mod pull_refresh;
pub mod rows;
pub mod scroll_fade;
pub mod spinner;
pub use spinner::{spinner, toolbar_spinner};

pub use avatar::avatar;
pub use buttons::{ButtonVariant, button, floating_action_button, icon_button};
pub use chips::{attachment_chip, chip, format_size};
pub use chrome::{app_bar, app_bar_actions, empty_state, toast};
pub use dialog::{dialog, dialog_sized};
pub use fields::{password_field, search_field, text_area, text_field};
pub use menu::{MenuAnchor, MenuItem, menu, menu_overlay};
pub use rows::{FolderKind, FolderView, ListRow, folder_row, list_row, section_header};

/// A collapsible account header: the label, unread roll-up and the account's
/// email underneath. The label row is the click target that toggles it.
pub fn account_header<'a>(
    label: &str,
    email: &str,
    collapsed: bool,
    unread: u32,
    on_toggle: crate::Message,
) -> iced::Element<'a, crate::Message> {
    let palette = theme::active();
    let email_style = palette.role(TextRole::LabelMedium);

    column![
        section_header(label, collapsed, unread, on_toggle),
        container(styled_text(
            email.to_string(),
            email_style,
            palette.on_surface_variant,
        ))
        .width(iced::Fill)
        .padding(iced::Padding {
            top: 0.0,
            right: palette.space.md,
            bottom: palette.space.xs,
            left: palette.space.md,
        }),
    ]
    .width(iced::Fill)
    .into()
}

/// A tappable card wrapping a leading icon, a title and a supporting line.
/// Shared by the Settings and Advanced screens so their action rows match.
pub fn action_card<'a>(
    title: &str,
    subtitle: &str,
    icon: char,
    message: Message,
) -> iced::Element<'a, Message> {
    let palette = theme::active();
    let title_style = palette.role(TextRole::TitleSmall);
    let subtitle_style = palette.role(TextRole::BodySmall);

    let leading = text(icons::glyph(icon))
        .font(icons::FONT)
        .size(palette.metrics.icon_size)
        .color(palette.on_surface_variant);

    let labels = column![
        styled_text(title.to_string(), title_style, palette.on_surface),
        styled_text(
            subtitle.to_string(),
            subtitle_style,
            palette.on_surface_variant,
        ),
    ]
    .spacing(palette.space.xs)
    .width(iced::Fill);

    let content = row![leading, labels]
        .spacing(palette.space.md)
        .align_y(iced::Alignment::Center)
        .width(iced::Fill);

    iced_button(content)
        .width(iced::Fill)
        .padding(palette.space.lg)
        .style(move |_, status| {
            let background = match status {
                iced_button::Status::Hovered | iced_button::Status::Pressed => {
                    palette.surface_container_high
                }
                _ => palette.surface_container,
            };
            iced_button::Style {
                background: Some(background.into()),
                text_color: palette.on_surface,
                border: iced::border::rounded(palette.radius.md),
                ..Default::default()
            }
        })
        .on_press(message)
        .into()
}

/// A confirmation modal: a title, an explanatory line, and Cancel/confirm
/// actions. Shared by the restore and factory-reset confirmations so they match.
pub fn confirm<'a>(
    title: &str,
    body: &str,
    confirm_label: &str,
    destructive: bool,
    on_confirm: Message,
    on_cancel: Message,
) -> iced::Element<'a, Message> {
    let palette = theme::active();
    let body_style = palette.role(TextRole::BodyMedium);
    let body_widget = text(body.to_string())
        .size(body_style.size)
        .line_height(body_style.line_height())
        .font(body_style.font)
        .color(palette.on_surface_variant);
    let variant = if destructive {
        ButtonVariant::Danger
    } else {
        ButtonVariant::Filled
    };

    dialog::dialog_sized(
        title,
        body_widget.into(),
        vec![
            button("Cancel", ButtonVariant::Text, on_cancel),
            button(confirm_label, variant, on_confirm),
        ],
        palette.metrics.dialog_width,
    )
}

/// A full-pane vertical scroller with the shared scrollbar: slim, and hidden
/// until the user scrolls. Screens use this instead of `scrollable` so every
/// list, form and settings page matches. It reports scroll offsets through
/// [`Message::Scrolled`] so the shell can show the bar only while scrolling.
pub fn scroller<'a>(content: impl Into<iced::Element<'a, Message>>) -> iced::Element<'a, Message> {
    iced::widget::scrollable(content)
        .width(iced::Fill)
        .height(iced::Fill)
        .direction(theme::vertical_scrollbar(theme::scrollbars_visible()))
        .style(theme::active().scrollable())
        .on_scroll(|viewport| Message::Scrolled(viewport.absolute_offset().y))
        .into()
}

/// A text widget that already carries the palette's type style and colour.
pub(crate) fn styled_text<'a>(
    value: impl Into<String>,
    typography: Typography,
    color: Color,
) -> iced::widget::Text<'a> {
    text(value.into())
        .size(typography.size)
        .line_height(typography.line_height())
        .font(typography.font)
        .color(color)
}

/// The tone of a screen pane's surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneTone {
    /// The window background behind every pane.
    Page,
    /// The default pane (list and reader).
    Surface,
    /// The navigation rail/drawer.
    Navigation,
}

/// The container style for a pane, resolved from the active palette.
pub fn pane_style(tone: PaneTone) -> impl Fn(&iced::Theme) -> iced::widget::container::Style {
    let palette = theme::active();
    let background = match tone {
        PaneTone::Page => palette.background,
        PaneTone::Surface => palette.surface,
        PaneTone::Navigation => palette.surface_container,
    };
    move |_| iced::widget::container::Style {
        background: Some(background.into()),
        text_color: Some(palette.on_surface),
        ..Default::default()
    }
}

/// The container style for a modal or drawer scrim.
pub fn scrim_style() -> impl Fn(&iced::Theme) -> iced::widget::container::Style {
    let palette = theme::active();
    move |_| iced::widget::container::Style {
        background: Some(palette.scrim.into()),
        ..Default::default()
    }
}

/// A 1-point vertical divider.
pub(crate) fn divider<'a>() -> iced::Element<'a, crate::Message> {
    let palette = crate::theme::active();
    iced::widget::container(iced::widget::space())
        .width(palette.metrics.border_width)
        .height(iced::Length::Fill)
        .style(move |_| iced::widget::container::Style {
            background: Some(palette.outline_variant.into()),
            ..Default::default()
        })
        .into()
}
