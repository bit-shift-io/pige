//! The conversation list, with pull-to-refresh and paging.

use iced::widget::{column, container, scrollable};
use iced::{Element, Fill, Length};
use pige_core::mail::FolderKind;

use crate::app::{
    App, account_color, account_for_folder, avatar_color, format_list_date, initials, row_view,
};
use crate::components::elastic_scroll::{self, Policy};
use crate::components::{self, ButtonVariant, ListRow, PaneTone};
use crate::icons;
use crate::layout::{self, Breakpoint};
use crate::message::Message;
use crate::screens::search;
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
    // While any sync runs, the refresh button becomes a spinner; this shows
    // Syncing without shifting the list (only a pull moves the list).
    let mut actions: Vec<Element<'a, Message>> = Vec::new();
    if app.syncing() {
        actions.push(components::toolbar_spinner(app.spin_phase as f32));
    } else {
        actions.push(components::icon_button(
            icons::REFRESH,
            "Sync now",
            Message::SyncNow,
        ));
    }
    actions.push(components::icon_button(
        icons::SEARCH,
        "Search",
        Message::OpenSearch,
    ));
    actions.push(components::icon_button(
        icons::EDIT,
        "Compose",
        Message::Compose,
    ));

    let mut pane = column![].spacing(0).width(Fill).height(Fill);
    pane = pane.push(components::app_bar(folder_title(app), leading, actions));
    if app.search_open {
        pane = pane.push(
            container(search::controls(app))
                .padding(iced::Padding {
                    top: 0.0,
                    right: palette.space.lg,
                    bottom: palette.space.md,
                    left: palette.space.lg,
                })
                .width(Fill),
        );
    }

    if search::is_active(app) {
        let scroller: Element<'a, Message> = scrollable(search::results(app))
            .width(Fill)
            .height(Fill)
            .style(palette.scrollable())
            .into();
        pane = pane.push(scroller);
    } else if drafts_selected(app) {
        let drafts = selected_drafts(app);
        if drafts.is_empty() {
            pane = pane.push(components::empty_state(
                "No drafts",
                "Messages you save as drafts appear here.",
            ));
        } else {
            let scroller: Element<'a, Message> = scrollable(draft_rows(&drafts))
                .width(Fill)
                .height(Fill)
                .style(palette.scrollable())
                .into();
            pane = pane.push(scroller);
        }
    } else if app.snapshot.conversations.is_empty() {
        pane = pane.push(components::empty_state(empty_title(app), empty_body(app)));
    } else {
        let mut rows = column![].spacing(0).width(Fill);
        for conversation in &app.snapshot.conversations {
            let row = row_view(
                conversation,
                account_color(&app.snapshot, &conversation.account_id),
            );
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
        let scroller = components::pull_refresh::wrap(scroller, Message::Refresh);
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

fn selected_folder_kind(app: &App) -> Option<FolderKind> {
    let id = app.snapshot.selected_folder.as_ref()?;
    app.snapshot
        .sections
        .iter()
        .flat_map(|section| section.folders.iter())
        .find(|folder| &folder.id == id)
        .map(|folder| folder.kind)
}

fn drafts_selected(app: &App) -> bool {
    selected_folder_kind(app) == Some(FolderKind::Drafts)
}

fn selected_drafts(app: &App) -> Vec<&pige_core::send::OutboxEntry> {
    let account = app
        .snapshot
        .selected_folder
        .as_ref()
        .and_then(|folder| account_for_folder(&app.snapshot, folder));
    app.snapshot
        .drafts
        .iter()
        .filter(|draft| match &account {
            Some(account) => &draft.message.account_id == account,
            None => true,
        })
        .collect()
}

fn draft_rows<'a>(drafts: &[&'a pige_core::send::OutboxEntry]) -> Element<'a, Message> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let mut rows = column![].spacing(0).width(Fill);
    for draft in drafts {
        let recipient = draft
            .message
            .to
            .first()
            .map(|address| address.display_name())
            .unwrap_or_else(|| "Draft".to_string());
        let subject = if draft.message.subject.trim().is_empty() {
            "(no subject)".to_string()
        } else {
            draft.message.subject.clone()
        };
        let snippet = draft
            .message
            .body
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.chars().take(120).collect::<String>())
            .unwrap_or_default();
        rows = rows.push(components::list_row(
            ListRow {
                initials: initials(&recipient),
                avatar_color: avatar_color(&recipient),
                sender: recipient,
                date: format_list_date(now, draft.updated_at),
                subject,
                snippet,
                unread: false,
                has_attachment: false,
                count: 0,
                account_color: None,
            },
            Message::OpenDraft(draft.id.clone()),
        ));
    }
    rows.into()
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
