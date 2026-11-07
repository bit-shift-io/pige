//! The navigation rail and compact/medium drawer.

use iced::widget::{column, container, space};
use iced::{Element, Fill};

use crate::app::{App, sections};
use crate::components::{self, FolderView, PaneTone};
use crate::layout;
use crate::message::Message;
use crate::theme;

/// The pinned unified-inbox entry, one collapsible group per account, then the
/// account and settings actions. The account groups scroll; the bottom actions
/// stay pinned.
pub fn navigation<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let mut body = column![].spacing(0).width(Fill);
    body = body.push(components::folder_row(
        FolderView {
            name: "All inboxes".to_string(),
            kind: components::FolderKind::All,
            unread: app.snapshot.unified_inbox_unread,
        },
        app.snapshot.unified_selected,
        Message::SelectAllInboxes,
    ));

    for section in sections(&app.snapshot, &app.collapsed) {
        let collapsed = app.collapsed.contains(&section.account_id);
        body = body.push(components::account_header(
            &section.label,
            &section.email,
            collapsed,
            section.unread,
            Message::ToggleAccount(section.account_id.clone()),
        ));
        for folder in &section.folders {
            let selected = app.snapshot.selected_folder.as_ref() == Some(&folder.id);
            body = body.push(components::folder_row(
                folder.view.clone(),
                selected,
                Message::SelectFolder(folder.id.clone()),
            ));
        }
    }

    let body = components::scroller(body);

    let nav = column![
        components::app_bar("pige", None, Vec::new()),
        space().height(palette.space.sm),
        body,
        components::folder_row(
            FolderView {
                name: "Add account".to_string(),
                kind: components::FolderKind::AddAccount,
                unread: 0,
            },
            false,
            Message::AddAccount,
        ),
        components::folder_row(
            FolderView {
                name: "Settings".to_string(),
                kind: components::FolderKind::Settings,
                unread: 0,
            },
            false,
            Message::Settings,
        ),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill);

    container(nav)
        .width(layout::NAV_WIDTH)
        .height(Fill)
        .padding([0.0, palette.space.sm])
        .style(components::pane_style(PaneTone::Navigation))
        .into()
}
