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
//! - [`chrome`] app bars, section headers, empty states and snackbars
//! - [`chips`] choice and attachment chips
//! - [`dialog`] modal cards
//!
//! The custom-drawn pieces iced lacks live alongside them: [`label`]'s
//! measured ellipsized label, [`scroll_fade`], [`elastic_scroll`] and
//! [`pull_refresh`]. The builders and custom widgets are adapted from
//! qlaunch's `components.rs` and companion modules (bitshift, MIT).

use iced::Color;
use iced::widget::{column, container, text};

use crate::theme::{self, TextRole, Typography};

pub mod avatar;
pub mod buttons;
pub mod chips;
pub mod chrome;
pub mod dialog;
pub mod elastic_scroll;
pub mod fields;
pub mod label;
pub mod pull_refresh;
pub mod rows;
pub mod scroll_fade;

pub use avatar::avatar;
pub use buttons::{ButtonVariant, button, floating_action_button, icon_button};
pub use chips::{attachment_chip, chip, format_size};
pub use chrome::{app_bar, empty_state, snackbar};
pub use dialog::{dialog, dialog_sized};
pub use fields::{password_field, search_field, text_area, text_field};
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
