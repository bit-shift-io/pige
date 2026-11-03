//! The Elm update loop: turn messages into state changes and API tasks.

use std::future::Future;
use std::time::Duration;

use iced::window;
use iced::{Subscription, Task};
use pige_core::account::AccountId;
use pige_core::api::{self, AccountInput, ModelSnapshot};
use pige_core::mail::FolderId;
use pige_core::platform::credentials_unavailable;
use tokio::sync::broadcast::error::RecvError;

use crate::app::App;
use crate::layout::Breakpoint;
use crate::message::{AccountSaveResult, Message};
use crate::screens::accounts::{AccountField, TestState};

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            Message::Resized(size) => {
                self.size = size;
                if Breakpoint::from_width(size.width) == Breakpoint::Expanded {
                    self.drawer_open = false;
                }
            }
            Message::Tick => {
                // Keep SQLite work off the UI thread; the executor produces a
                // snapshot and delivers it as a normal message.
                return Task::perform(async { api::snapshot() }, Message::Snapshot);
            }
            Message::ToggleDrawer => self.drawer_open = true,
            Message::CloseDrawer => self.drawer_open = false,
            Message::SelectAllInboxes => {
                self.drawer_open = false;
                return snapshot_task(api::select_unified());
            }
            Message::SelectFolder(folder) => return self.select_folder(folder),
            Message::ToggleAccount(account) => {
                if !self.collapsed.remove(&account) {
                    self.collapsed.insert(account);
                }
            }
            Message::LoadOlder => {
                if self.snapshot.unified_selected {
                    return snapshot_task(api::load_older_unified());
                }
                if let Some(folder) = self.snapshot.selected_folder.clone() {
                    return snapshot_task(async move { api::load_older(&folder).await });
                }
            }
            Message::SyncNow | Message::Refresh => {
                return snapshot_task(api::sync_all());
            }
            Message::SearchChanged(query) => {
                self.search = query;
                return self.run_search();
            }
            Message::SearchScopeChanged(scope) => {
                self.search_scope = scope;
                return self.run_search();
            }
            Message::SearchResults {
                query,
                scope,
                result,
            } => {
                if query == self.search && scope == self.search_scope {
                    match result {
                        Ok(hits) => self.search_results = hits,
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Message::OpenConversation(thread_id) => {
                self.drawer_open = false;
                let open_thread = thread_id.clone();
                let open = Task::perform(
                    async move { api::open_conversation(&open_thread).await },
                    |result| match result {
                        Ok(snapshot) => Message::Snapshot(snapshot),
                        Err(error) => Message::Error(error),
                    },
                );
                let mark_thread = thread_id.clone();
                let mark_read = Task::perform(
                    async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        match api::mark_read(&mark_thread).await {
                            Ok(snapshot) => Message::Snapshot(snapshot),
                            Err(_) => Message::Noop,
                        }
                    },
                    |message| message,
                );
                return Task::batch([open, mark_read]);
            }
            Message::LoadImages => {
                let thread_id = self
                    .snapshot
                    .selected_conversation
                    .as_ref()
                    .map(|detail| detail.thread_id.clone());
                if let Some(thread_id) = thread_id {
                    return snapshot_task(async move { api::load_images(&thread_id) });
                }
            }
            Message::ToggleEarlier => self.reader_expanded = !self.reader_expanded,
            Message::OpenUrl(url) => {
                if let Err(error) = api::open_url(&url) {
                    self.error = Some(error);
                }
            }
            Message::OpenAccounts => {
                self.drawer_open = false;
                self.accounts_open = true;
                self.accounts.open(&self.snapshot);
            }
            Message::AddAccount => {
                self.drawer_open = false;
                self.accounts_open = true;
                self.accounts.open(&self.snapshot);
                // First run goes straight to the form; afterwards the screen
                // shows the list so labels and removals are one click away.
                if self.snapshot.accounts.is_empty() {
                    self.accounts.start_new();
                }
            }
            Message::CloseAccounts => {
                self.accounts_open = false;
                self.accounts.close_form();
                self.accounts.confirm_remove = None;
            }
            Message::AccountNew => self.accounts.start_new(),
            Message::AccountEdit(id) => match api::account(&id) {
                Ok(account) => self.accounts.edit(&account),
                Err(error) => self.error = Some(error),
            },
            Message::AccountCancel => self.accounts.close_form(),
            Message::AccountFieldChanged(field, value) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    let old_email = form.email.clone();
                    form.apply_field(field, value);
                    if field == AccountField::Email
                        && (form.username.is_empty() || form.username == old_email)
                    {
                        form.username = form.email.clone();
                    }
                }
            }
            Message::AccountPresetChanged(preset) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    form.choose_preset(preset);
                }
            }
            Message::AccountImapSecurityChanged(security) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    form.imap_security = security;
                    form.test = TestState::Idle;
                }
            }
            Message::AccountSmtpSecurityChanged(security) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    form.smtp_security = security;
                    form.test = TestState::Idle;
                }
            }
            Message::AccountTest => {
                let Some(form) = self.accounts.form.as_mut() else {
                    return Task::none();
                };
                if form.busy {
                    return Task::none();
                }
                let input = match form.to_input() {
                    Ok(input) => input,
                    Err(error) => {
                        form.test = TestState::Failed(error);
                        return Task::none();
                    }
                };
                form.busy = true;
                form.test = TestState::Testing;
                return Task::perform(
                    async move { api::test_account(&input).await },
                    Message::AccountTested,
                );
            }
            Message::AccountTested(result) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    form.busy = false;
                    form.test = match result {
                        Ok(()) => TestState::Passed,
                        Err(error) => TestState::Failed(error),
                    };
                }
            }
            Message::AccountSave => {
                let Some(form) = self.accounts.form.as_mut() else {
                    return Task::none();
                };
                if form.busy {
                    return Task::none();
                }
                let input = match form.to_input() {
                    Ok(input) => input,
                    Err(error) => {
                        form.test = TestState::Failed(error);
                        return Task::none();
                    }
                };
                let editing = form.editing.clone();
                form.busy = true;
                form.test = TestState::Testing;
                return Task::perform(save_account(editing, input), Message::AccountSaved);
            }
            Message::AccountSaved(result) => match result {
                Ok(result) => {
                    self.apply_snapshot(result.snapshot);
                    self.accounts.close_form();
                    self.accounts_open = false;
                    if let Some(warning) = result.warning {
                        self.error = Some(warning);
                    }
                }
                Err(error) => {
                    if let Some(form) = self.accounts.form.as_mut() {
                        form.busy = false;
                        form.test = TestState::Failed(error.clone());
                    }
                    self.error = Some(error);
                }
            },
            Message::AccountRemove(id) => self.accounts.confirm_remove = Some(id),
            Message::AccountRemoveConfirmed(id) => {
                self.accounts.confirm_remove = None;
                return Task::perform(
                    async move { api::remove_account(&id).await },
                    Message::AccountRemoved,
                );
            }
            Message::AccountRemoveCancelled => self.accounts.confirm_remove = None,
            Message::AccountRemoved(result) => match result {
                Ok(snapshot) => {
                    self.apply_snapshot(snapshot);
                    self.accounts.open(&self.snapshot);
                    self.accounts.notice = Some("Account removed.".to_string());
                }
                Err(error) => self.error = Some(error),
            },
            Message::AccountLabelDraft(id, value) => {
                self.accounts.label_drafts.insert(id, value);
            }
            Message::AccountLabelCommit(id) => {
                let Some(label) = self.accounts.label_drafts.get(&id).cloned() else {
                    return Task::none();
                };
                if label.trim().is_empty() {
                    self.error = Some("Account label is required".to_string());
                    return Task::none();
                }
                return Task::perform(
                    async move { api::rename_account(&id, &label).await },
                    Message::AccountLabelRenamed,
                );
            }
            Message::AccountLabelRenamed(result) => match result {
                Ok(snapshot) => {
                    self.apply_snapshot(snapshot);
                    self.accounts.open(&self.snapshot);
                    self.accounts.notice = Some("Account label updated.".to_string());
                }
                Err(error) => self.error = Some(error),
            },
            Message::Error(error) => self.error = Some(error),
            Message::DismissError => self.error = None,
            Message::Compose | Message::Settings | Message::Noop => {}
        }

        Task::none()
    }

    fn select_folder(&mut self, folder: FolderId) -> Task<Message> {
        self.drawer_open = false;
        snapshot_task(async move { api::select_folder(&folder).await })
    }

    /// Run the current query on the API's blocking search path. An empty query
    /// deliberately skips SQLite: the list falls back to recent mail.
    fn run_search(&mut self) -> Task<Message> {
        if self.search.trim().is_empty() {
            self.search_results.clear();
            return Task::none();
        }
        let query = self.search.clone();
        let scope = self.search_scope.clone();
        let completed = query.clone();
        let completed_scope = scope.clone();
        Task::perform(
            async move {
                // Coalesce typing bursts; a later keystroke supersedes this
                // request through the API generation guard.
                tokio::time::sleep(Duration::from_millis(150)).await;
                api::search_mail(&query, scope).await
            },
            move |result| Message::SearchResults {
                query: completed.clone(),
                scope: completed_scope.clone(),
                result,
            },
        )
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::resize_events().map(|(_, size)| Message::Resized(size)),
            Subscription::run(core_events),
            iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick),
        ])
    }
}

/// Test, then create or update, then kick off the first sync. The account is
/// saved even if the first sync fails, but the failure becomes a warning so the
/// user knows the mailbox has no mail yet.
async fn save_account(
    editing: Option<AccountId>,
    input: AccountInput,
) -> Result<AccountSaveResult, String> {
    api::test_account(&input).await?;

    let mut warning = None;
    let snapshot = match editing.clone() {
        Some(id) => match api::update_account(&id, input.clone()).await {
            Ok(snapshot) => snapshot,
            Err(error) if credentials_unavailable(&error) => {
                warning = Some(session_only_warning());
                api::update_account_with_session(&id, input.clone()).await?
            }
            Err(error) => return Err(error),
        },
        None => match api::add_account(input.clone()).await {
            Ok(snapshot) => snapshot,
            Err(error) if credentials_unavailable(&error) => {
                warning = Some(session_only_warning());
                api::add_account_with_session(input.clone()).await?
            }
            Err(error) => return Err(error),
        },
    };

    let id = editing.or_else(|| snapshot.accounts.last().map(|account| account.id.clone()));
    if let Some(id) = id
        && let Err(error) = api::sync_account(&id).await
    {
        warning = Some(match warning {
            Some(warning) => format!("{warning} The first sync failed: {error}"),
            None => format!("Account saved, but the first sync failed: {error}"),
        });
    }

    Ok(AccountSaveResult { snapshot, warning })
}

fn session_only_warning() -> String {
    "Secret Service is unavailable. The account password is kept for this session only and will not be saved after pige exits.".to_string()
}

fn snapshot_task(
    future: impl Future<Output = Result<ModelSnapshot, String>> + Send + 'static,
) -> Task<Message> {
    Task::perform(future, |result| match result {
        Ok(snapshot) => Message::Snapshot(snapshot),
        Err(error) => Message::Error(error),
    })
}

fn core_events() -> impl iced::futures::Stream<Item = Message> {
    use iced::futures::SinkExt;

    iced::stream::channel::<Message>(
        64,
        |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
            let mut events = api::subscribe();
            loop {
                match events.recv().await {
                    Ok(event) => {
                        let message = match event {
                            api::CoreEvent::Error(error) => Message::Error(error),
                            _ => Message::Snapshot(api::snapshot()),
                        };
                        if output.send(message).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        log::warn!("ui fell {skipped} core events behind; refreshing snapshot");
                        if output
                            .send(Message::Snapshot(api::snapshot()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        },
    )
}
