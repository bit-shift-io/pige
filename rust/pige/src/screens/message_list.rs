//! The conversation list, with pull-to-refresh and paging.

use iced::widget::{column, container, scrollable};
use iced::{Element, Fill, Length};
use pige_core::api::SyncStatus;

use crate::app::{App, row_view};
use crate::components::elastic_scroll::{self, Policy};
use crate::components::{self, ButtonVariant, PaneTone};
use crate::icons;
use crate::layout::{self, Breakpoint};
use crate::message::Message;
use crate::theme;

/// The list pane. Compact shows it alone; medium and expanded share the width
/// with the reader.
pub fn list_pane<'a>(
    app: &'a App,
    breakpoint: Breakpoint,
    reader_visible: bool,
) -> Element<'a, Message> {
    let palette = theme::active();
    let leading = (breakpoint != Breakpoint::Expanded)
        .then(|| components::icon_button(icons::MENU, "Open navigation", Message::ToggleDrawer));
    let actions = vec![
        components::icon_button(icons::REFRESH, "Sync now", Message::SyncNow),
        components::icon_button(icons::EDIT, "Compose", Message::Compose),
    ];

    let mut pane = column![].spacing(0).width(Fill).height(Fill);
    pane = pane.push(components::app_bar(folder_title(app), leading, actions));
    pane = pane.push(
        container(components::search_field(&app.search, Message::Search))
            .padding(iced::Padding {
                top: 0.0,
                right: palette.space.lg,
                bottom: palette.space.md,
                left: palette.space.lg,
            })
            .width(Fill),
    );

    if app.snapshot.conversations.is_empty() {
        pane = pane.push(components::empty_state(empty_title(app), empty_body(app)));
    } else {
        let mut rows = column![].spacing(0).width(Fill);
        for conversation in &app.snapshot.conversations {
            let row = row_view(conversation);
            rows = rows.push(elastic_scroll::row(
                components::list_row(
                    row,
                    Message::OpenConversation(conversation.thread_id.clone()),
                ),
                &app.list_motion,
            ));
        }
        if app.snapshot.has_more {
            rows = rows.push(
                container(components::button(
                    "Load older",
                    ButtonVariant::Text,
                    Message::LoadOlder,
                ))
                .padding(palette.space.md)
                .center_x(Fill),
            );
        }

        let scroller: Element<'a, Message> =
            scrollable(elastic_scroll::content(rows.into(), &app.list_motion))
                .width(Fill)
                .height(Fill)
                .style(palette.scrollable())
                .into();
        let scroller = components::pull_refresh::wrap(
            scroller,
            app.snapshot.status == SyncStatus::Syncing,
            Message::Refresh,
        );
        pane = pane.push(elastic_scroll::wrap(
            scroller,
            app.list_motion.clone(),
            // Pull-to-refresh owns the top edge; the bounce still handles the
            // bottom edge.
            Policy::PullToRefresh,
        ));
    }

    let width = if reader_visible {
        Length::Fixed(layout::LIST_WIDTH)
    } else {
        Length::Fill
    };
    container(pane)
        .width(width)
        .height(Fill)
        .style(components::pane_style(PaneTone::Surface))
        .into()
}

fn folder_title(app: &App) -> &str {
    app.snapshot
        .selected_folder
        .as_ref()
        .and_then(|id| {
            app.snapshot
                .sections
                .iter()
                .flat_map(|section| section.folders.iter())
                .find(|folder| &folder.id == id)
        })
        .map_or("All inboxes", |folder| folder.name.as_str())
}

fn empty_title(app: &App) -> &'static str {
    if app.snapshot.accounts.is_empty() {
        "No accounts yet"
    } else {
        "Nothing here"
    }
}

fn empty_body(app: &App) -> &'static str {
    if app.snapshot.accounts.is_empty() {
        "Use Add account in the navigation rail to connect your first mailbox."
    } else {
        "This folder is empty, or its first sync has not finished."
    }
}
