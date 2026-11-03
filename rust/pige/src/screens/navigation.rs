//! The navigation rail and compact/medium drawer.

use iced::widget::{column, container, space};
use iced::{Element, Fill};

use crate::app::{App, SectionView, sections};
use crate::components::{self, FolderView, PaneTone};
use crate::layout;
use crate::message::Message;
use crate::theme;

/// The pinned unified-inbox entry, one collapsible group per account, then the
/// account and settings actions.
pub fn navigation<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let mut nav = column![].spacing(0).width(Fill);

    nav = nav.push(components::app_bar("pige", None, Vec::new()));
    nav = nav.push(space().height(palette.space.sm));
    nav = nav.push(components::folder_row(
        FolderView {
            name: "All inboxes".to_string(),
            kind: components::FolderKind::All,
            unread: app.snapshot.unified_inbox_unread,
        },
        app.snapshot.selected_folder.is_none(),
        Message::SelectAllInboxes,
    ));

    for section in sections(&app.snapshot, &app.collapsed) {
        let collapsed = app.collapsed.contains(&section.account_id);
        nav = nav.push(components::account_header(
            &section.label,
            &section.email,
            collapsed,
            section.unread,
            Message::ToggleAccount(section.account_id.clone()),
        ));
        nav = nav.push(folder_rows(app, &section));
    }

    nav = nav.push(space().height(Fill));
    nav = nav.push(components::folder_row(
        FolderView {
            name: "Add account".to_string(),
            kind: components::FolderKind::AddAccount,
            unread: 0,
        },
        false,
        Message::AddAccount,
    ));
    nav = nav.push(components::folder_row(
        FolderView {
            name: "Settings".to_string(),
            kind: components::FolderKind::Settings,
            unread: 0,
        },
        false,
        Message::Settings,
    ));

    container(nav)
        .width(layout::NAV_WIDTH)
        .height(Fill)
        .padding([0.0, palette.space.sm])
        .style(components::pane_style(PaneTone::Navigation))
        .into()
}

fn folder_rows<'a>(app: &App, section: &SectionView) -> Element<'a, Message> {
    if section.folders.is_empty() {
        return space().height(0.0).into();
    }

    let source = app
        .snapshot
        .sections
        .iter()
        .find(|source| source.account_id == section.account_id);

    let mut rows = column![].spacing(0).width(Fill);
    for (index, folder) in section.folders.iter().enumerate() {
        let id = source
            .and_then(|source| source.folders.get(index))
            .map(|folder| folder.id.clone());
        let selected = id.as_ref() == app.snapshot.selected_folder.as_ref();
        let on_press = id.map(Message::SelectFolder).unwrap_or(Message::Noop);
        rows = rows.push(components::folder_row(
            FolderView {
                name: folder.name.clone(),
                kind: folder.kind,
                unread: folder.unread,
            },
            selected,
            on_press,
        ));
    }
    rows.into()
}
