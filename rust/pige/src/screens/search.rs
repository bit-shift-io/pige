//! Search controls and conversation results.
//!
//! Search hits are already one per conversation, so the result list can reuse
//! the message-list row unchanged. The scope chip cycles between every cached
//! account, the selected account, and the selected folder when those exist.

use iced::widget::{column, row};
use iced::{Element, Fill};
use pige_core::api::ModelSnapshot;
use pige_core::search::SearchScope;

use crate::app::{App, account_color, account_for_folder, search_row_view};
use crate::components;
use crate::icons;
use crate::message::Message;
use crate::theme;

/// Whether the list should show search results instead of recent mail. A
/// closed search never shows results, even if an old query is still stored.
pub fn is_active(app: &App) -> bool {
    app.search_open && !app.search.trim().is_empty()
}

/// The query field and, once a query exists, the cycling scope chip. The shell
/// places this directly under the app bar.
pub fn controls<'a>(app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let mut controls = row![components::search_field(
        &app.search,
        Message::SearchChanged
    )]
    .spacing(palette.space.sm)
    .align_y(iced::Alignment::Center)
    .width(Fill);
    if is_active(app) {
        controls = controls.push(scope_chip(app));
    }
    controls = controls.push(components::icon_button(
        icons::CLOSE,
        "Close search",
        Message::CloseSearch,
    ));
    controls.into()
}

/// The scope chip's label, naming the selected account or folder when possible.
pub fn scope_label(snapshot: &ModelSnapshot, scope: &SearchScope) -> String {
    match scope {
        SearchScope::AllAccounts => "All accounts".to_string(),
        SearchScope::Account(id) => snapshot
            .sections
            .iter()
            .find(|section| &section.account_id == id)
            .map(|section| section.label.clone())
            .unwrap_or_else(|| "Account".to_string()),
        SearchScope::Folder(id) => snapshot
            .sections
            .iter()
            .flat_map(|section| section.folders.iter())
            .find(|folder| &folder.id == id)
            .map(|folder| folder.name.clone())
            .unwrap_or_else(|| "Folder".to_string()),
    }
}

/// All accounts → current account → current folder → All accounts. Scopes that
/// do not exist for the current selection are skipped.
pub fn next_scope(snapshot: &ModelSnapshot, current: &SearchScope) -> SearchScope {
    let mut options = vec![SearchScope::AllAccounts];
    if let Some(folder) = snapshot.selected_folder.clone() {
        if let Some(account) = account_for_folder(snapshot, &folder) {
            options.push(SearchScope::Account(account));
        }
        options.push(SearchScope::Folder(folder));
    }

    let index = options
        .iter()
        .position(|scope| scope == current)
        .unwrap_or(0);
    options[(index + 1) % options.len()].clone()
}

/// The cycling scope chip shown beside the query field.
pub fn scope_chip<'a>(app: &'a App) -> Element<'a, Message> {
    let label = scope_label(&app.snapshot, &app.search_scope);
    let next = next_scope(&app.snapshot, &app.search_scope);
    components::chip(&label, true, Message::SearchScopeChanged(next))
}

/// The result rows, or the no-results state. The caller owns the scrollable so
/// this stays cheap to compose.
pub fn results<'a>(app: &'a App) -> Element<'a, Message> {
    if app.search_results.is_empty() {
        return components::empty_state(
            "No results",
            "Try a different word, or widen the search scope.",
        );
    }

    let mut rows = column![].spacing(0).width(Fill);
    for hit in &app.search_results {
        rows = rows.push(components::list_row(
            search_row_view(hit, account_color(&app.snapshot, &hit.account_id)),
            Message::OpenConversation(hit.thread_id.clone()),
        ));
    }
    rows.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    use pige_core::account::AccountId;
    use pige_core::api::{AccountSection, FolderSummary};
    use pige_core::mail::{FolderId, FolderKind};

    fn snapshot() -> ModelSnapshot {
        ModelSnapshot {
            selected_folder: Some(FolderId("inbox".into())),
            sections: vec![AccountSection {
                account_id: AccountId("personal".into()),
                label: "Personal".into(),
                email: "ada@example.com".into(),
                unread: 0,
                capabilities: Default::default(),
                folders: vec![FolderSummary {
                    id: FolderId("inbox".into()),
                    name: "INBOX".into(),
                    kind: FolderKind::Inbox,
                    unread: 0,
                }],
            }],
            ..ModelSnapshot::default()
        }
    }

    #[test]
    fn scope_chip_cycles_through_available_scopes() {
        let snapshot = snapshot();
        let all = SearchScope::AllAccounts;
        let account = next_scope(&snapshot, &all);
        assert_eq!(account, SearchScope::Account(AccountId("personal".into())));

        let folder = next_scope(&snapshot, &account);
        assert_eq!(folder, SearchScope::Folder(FolderId("inbox".into())));

        assert_eq!(next_scope(&snapshot, &folder), SearchScope::AllAccounts);
    }

    #[test]
    fn scope_labels_name_the_account_or_folder() {
        let snapshot = snapshot();
        assert_eq!(
            scope_label(&snapshot, &SearchScope::AllAccounts),
            "All accounts"
        );
        assert_eq!(
            scope_label(
                &snapshot,
                &SearchScope::Account(AccountId("personal".into()))
            ),
            "Personal"
        );
        assert_eq!(
            scope_label(&snapshot, &SearchScope::Folder(FolderId("inbox".into()))),
            "INBOX"
        );
    }

    #[test]
    fn unified_selection_only_cycles_all_accounts() {
        let mut snapshot = snapshot();
        snapshot.selected_folder = None;
        assert_eq!(
            next_scope(&snapshot, &SearchScope::AllAccounts),
            SearchScope::AllAccounts
        );
    }
}
