//! Message-list rows, folder rows and section headers.
//!
//! These builders take plain view data. They never take a model snapshot, so a
//! screen can build one without a renderer and the tests can exercise their
//! data directly.

use iced::widget::{button, column, container, row, space};
use iced::{Color, Element, Fill};

use crate::Message;
use crate::components::{avatar, label, styled_text};
use crate::icons;
use crate::theme::{self, Palette, TextRole};

/// One conversation row in the message list.
#[derive(Debug, Clone, PartialEq)]
pub struct ListRow {
    pub initials: String,
    pub avatar_color: Color,
    pub sender: String,
    pub date: String,
    pub subject: String,
    pub snippet: String,
    pub unread: bool,
    pub has_attachment: bool,
    pub count: u32,
}

/// The icon a navigation folder draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    All,
    Inbox,
    Starred,
    Sent,
    Drafts,
    Outbox,
    Archive,
    Spam,
    Trash,
    Custom,
    AddAccount,
    Settings,
}

impl FolderKind {
    /// The Material glyph this folder uses.
    pub fn glyph(self) -> char {
        match self {
            FolderKind::All => icons::ALL_INBOX,
            FolderKind::Inbox => icons::INBOX,
            FolderKind::Starred => icons::STAR,
            FolderKind::Sent => icons::SEND,
            FolderKind::Drafts => icons::DRAFTS,
            FolderKind::Outbox => icons::OUTBOX,
            FolderKind::Archive => icons::ARCHIVE,
            FolderKind::Spam => icons::REPORT,
            FolderKind::Trash => icons::DELETE,
            FolderKind::Custom => icons::FOLDER,
            FolderKind::AddAccount => icons::PERSON_ADD,
            FolderKind::Settings => icons::SETTINGS,
        }
    }
}

/// One folder in the navigation pane.
#[derive(Debug, Clone, PartialEq)]
pub struct FolderView {
    pub name: String,
    pub kind: FolderKind,
    pub unread: u32,
}

fn badge(count: u32, palette: Palette) -> Element<'static, Message> {
    container(styled_text(
        count.to_string(),
        palette.role(TextRole::LabelSmall),
        palette.on_secondary_container,
    ))
    .padding([0.0, palette.space.xs])
    .center_y(palette.metrics.badge_height)
    .style(move |_| container::Style {
        background: Some(palette.secondary_container.into()),
        text_color: Some(palette.on_secondary_container),
        border: iced::border::rounded(palette.metrics.badge_height / 2.0),
        ..Default::default()
    })
    .into()
}

fn unread_dot(palette: Palette) -> Element<'static, Message> {
    container(space())
        .width(palette.metrics.unread_dot)
        .height(palette.metrics.unread_dot)
        .style(move |_| container::Style {
            background: Some(palette.primary.into()),
            border: iced::border::rounded(palette.metrics.unread_dot / 2.0),
            ..Default::default()
        })
        .into()
}

fn tile_style(
    palette: Palette,
    selected: bool,
) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| {
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let background = if selected {
            palette.secondary_container
        } else if hovered {
            palette.surface_container_high
        } else {
            Color::TRANSPARENT
        };
        button::Style {
            background: Some(background.into()),
            text_color: if selected {
                palette.on_secondary_container
            } else {
                palette.on_surface
            },
            border: iced::border::rounded(palette.radius.sm),
            ..Default::default()
        }
    }
}

/// A three-line conversation row: avatar, sender/date, subject and snippet.
pub fn list_row<'a>(item: ListRow, on_press: Message) -> Element<'a, Message> {
    let palette = theme::active();
    let sender_style = palette.role(TextRole::LabelLarge);
    let subject_style = palette.role(TextRole::BodyMedium);
    let snippet_style = palette.role(TextRole::BodySmall);
    let date_style = palette.role(TextRole::LabelMedium);

    let primary = palette.on_surface;
    let secondary = palette.on_surface_variant;
    let sender_color = if item.unread { primary } else { secondary };
    let subject_color = if item.unread { primary } else { secondary };

    let mut top = row![label::fitted(
        item.sender.clone(),
        sender_style,
        sender_color,
        false,
        None
    )]
    .align_y(iced::Alignment::Center)
    .spacing(palette.space.xs);

    if item.has_attachment {
        top = top.push(
            iced::widget::text(icons::glyph(icons::ATTACH_FILE))
                .font(icons::FONT)
                .size(palette.metrics.small_icon_size)
                .color(secondary),
        );
    }
    top = top.push(styled_text(item.date.clone(), date_style, secondary));

    let mut trailing = column![]
        .width(palette.space.lg)
        .align_x(iced::Alignment::End);
    if item.unread {
        trailing = trailing.push(unread_dot(palette));
    }
    if item.count > 1 {
        trailing = trailing.push(badge(item.count, palette));
    }

    let lines = column![
        top.width(Fill),
        label::fitted(
            item.subject.clone(),
            subject_style,
            subject_color,
            false,
            None
        ),
        label::fitted(item.snippet.clone(), snippet_style, secondary, false, None),
    ]
    .spacing(0)
    .width(Fill);

    let content = row![
        avatar::avatar(
            &item.initials,
            item.avatar_color,
            palette.metrics.avatar_size
        ),
        lines,
        trailing,
    ]
    .spacing(palette.space.md)
    .align_y(iced::Alignment::Center)
    .width(Fill);

    let background = if item.unread {
        palette.surface_container
    } else {
        Color::TRANSPARENT
    };

    button(
        container(content)
            .width(Fill)
            .padding([0.0, palette.space.lg])
            .center_y(palette.metrics.list_row_height)
            .style(move |_| container::Style {
                background: Some(background.into()),
                text_color: Some(palette.on_surface),
                ..Default::default()
            }),
    )
    .width(Fill)
    .padding(0)
    .style(tile_style(palette, false))
    .on_press(on_press)
    .into()
}

/// One folder in the navigation pane.
pub fn folder_row<'a>(
    folder: FolderView,
    selected: bool,
    on_press: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    let label_style = palette.role(TextRole::BodyMedium);
    let color = if selected {
        palette.on_secondary_container
    } else {
        palette.on_surface_variant
    };

    let mut content = row![
        iced::widget::text(icons::glyph(folder.kind.glyph()))
            .font(icons::FONT)
            .size(palette.metrics.nav_icon_size)
            .color(color),
        styled_text(folder.name, label_style, color).width(Fill),
    ]
    .spacing(palette.space.md)
    .align_y(iced::Alignment::Center)
    .width(Fill);

    if folder.unread > 0 {
        content = content.push(badge(folder.unread, palette));
    }

    button(
        container(content)
            .width(Fill)
            .padding([0.0, palette.space.md])
            .center_y(palette.metrics.folder_row_height),
    )
    .width(Fill)
    .padding(0)
    .style(tile_style(palette, selected))
    .on_press(on_press)
    .into()
}

/// A collapsible account section header with its unread roll-up.
pub fn section_header<'a>(
    label: &str,
    collapsed: bool,
    unread: u32,
    on_toggle: Message,
) -> Element<'a, Message> {
    let palette = theme::active();
    let style = palette.role(TextRole::TitleSmall);
    let mut content =
        row![styled_text(label.to_string(), style, palette.on_surface_variant).width(Fill),]
            .spacing(palette.space.sm)
            .align_y(iced::Alignment::Center)
            .width(Fill);

    if unread > 0 {
        content = content.push(badge(unread, palette));
    }
    content = content.push(
        iced::widget::text(icons::glyph(if collapsed {
            icons::EXPAND_MORE
        } else {
            icons::EXPAND_LESS
        }))
        .font(icons::FONT)
        .size(palette.metrics.nav_icon_size)
        .color(palette.on_surface_variant),
    );

    button(
        container(content)
            .width(Fill)
            .padding([0.0, palette.space.md])
            .center_y(palette.metrics.section_header_height),
    )
    .width(Fill)
    .padding(0)
    .style(move |_, status| {
        let alpha = match status {
            button::Status::Hovered => palette.states.hover,
            button::Status::Pressed => palette.states.pressed,
            _ => 0.0,
        };
        button::Style {
            background: Some(
                Color {
                    a: alpha,
                    ..palette.on_surface
                }
                .into(),
            ),
            text_color: palette.on_surface_variant,
            border: iced::border::rounded(palette.radius.sm),
            ..Default::default()
        }
    })
    .on_press(on_toggle)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_folder_kind_has_an_icon() {
        for kind in [
            FolderKind::All,
            FolderKind::Inbox,
            FolderKind::Starred,
            FolderKind::Sent,
            FolderKind::Drafts,
            FolderKind::Outbox,
            FolderKind::Archive,
            FolderKind::Spam,
            FolderKind::Trash,
            FolderKind::Custom,
            FolderKind::AddAccount,
            FolderKind::Settings,
        ] {
            assert!(kind.glyph() as u32 >= 0xe000);
        }
    }

    #[test]
    fn list_rows_are_plain_data() {
        let row = ListRow {
            initials: "AB".into(),
            avatar_color: Color::from_rgb8(0x67, 0x50, 0xa4),
            sender: "Ada Lovelace".into(),
            date: "09:41".into(),
            subject: "Analytical engines".into(),
            snippet: "A short preview of the latest message.".into(),
            unread: true,
            has_attachment: true,
            count: 3,
        };
        assert_eq!(row.count, 3);
        assert!(row.unread);
    }
}
