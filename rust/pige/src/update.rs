//! The Elm update loop: turn messages into state changes and API tasks.

use std::future::Future;
use std::time::Duration;

use iced::window;
use iced::{Subscription, Task};
use pige_core::account::AccountId;
use pige_core::api::{self, AccountInput, ModelSnapshot};
use pige_core::mail::{FolderId, ThreadId};
use pige_core::platform::credentials_unavailable;
use tokio::sync::broadcast::error::RecvError;

use crate::app::App;
use crate::layout::Breakpoint;
use crate::message::{AccountSaveResult, Message};
use crate::screens::accounts::{AccountField, TestState};
use crate::screens::compose::{ComposeSeed, ComposeState};

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
            Message::ConversationBack => return self.reader_back(),
            Message::ArchiveConversation => {
                if let Some(thread) = self.open_thread() {
                    return snapshot_task(async move { api::archive_thread(&thread).await });
                }
            }
            Message::TrashConversation => {
                if let Some(thread) = self.open_thread() {
                    return snapshot_task(async move { api::trash_thread(&thread).await });
                }
            }
            Message::MarkUnread => {
                if let Some(thread) = self.open_thread() {
                    return snapshot_task(async move { api::set_read(&thread, false).await });
                }
            }
            Message::OpenMovePicker => self.move_picker_open = true,
            Message::CloseMovePicker => self.move_picker_open = false,
            Message::MoveConversation(folder) => {
                self.move_picker_open = false;
                if let Some(thread) = self.open_thread() {
                    return snapshot_task(async move { api::move_thread(&thread, &folder).await });
                }
            }
            Message::OpenDeleteConfirm => self.delete_confirm_open = true,
            Message::CancelDeleteConfirm => self.delete_confirm_open = false,
            Message::ConfirmDelete => {
                self.delete_confirm_open = false;
                if let Some(thread) = self.open_thread() {
                    return snapshot_task(
                        async move { api::delete_permanently(&thread, true).await },
                    );
                }
            }
            Message::OpenUrl(url) => {
                if let Err(error) = api::open_url(&url) {
                    self.error = Some(error);
                }
            }
            Message::OpenAttachment(message, part) => {
                return Task::perform(
                    async move { api::open_attachment(&message, &part).await },
                    |result| match result {
                        Ok(_) => Message::Noop,
                        Err(error) => Message::Error(error),
                    },
                );
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
            Message::AccountEdit(id) => {
                return Task::perform(async move { api::account(&id) }, Message::AccountLoaded);
            }
            Message::AccountLoaded(result) => match result {
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
            Message::AccountPasswordChanged(password) => {
                if let Some(form) = self.accounts.form.as_mut() {
                    form.apply_field(AccountField::Password, password.into_inner());
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
            Message::Compose => {
                let account = self.selected_account.clone().or_else(|| {
                    self.snapshot
                        .accounts
                        .first()
                        .map(|account| account.id.clone())
                });
                match account {
                    Some(account) => {
                        self.drawer_open = false;
                        self.accounts_open = false;
                        self.compose = Some(ComposeState::new(account));
                    }
                    None => {
                        self.error = Some("Add an account before composing.".to_string());
                    }
                }
            }
            Message::Reply => return self.reply_or_forward(true),
            Message::Forward => return self.reply_or_forward(false),
            Message::ComposeLoaded(result) => match result {
                Ok(seed) => {
                    self.drawer_open = false;
                    self.accounts_open = false;
                    self.compose = Some(ComposeState::from_message(seed.message, seed.id));
                }
                Err(error) => self.error = Some(error),
            },
            Message::OpenDraft(id) => {
                return Task::perform(async move { api::draft(&id) }, |result| match result {
                    Ok(Some(entry)) => Message::ComposeLoaded(Ok(ComposeSeed {
                        id: Some(entry.id),
                        message: entry.message,
                    })),
                    Ok(None) => Message::Error("Draft was not found.".to_string()),
                    Err(error) => Message::Error(error),
                });
            }
            Message::ComposeField(field, value) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.apply_field(field, value);
                }
            }
            Message::ComposeBody(action) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.body.perform(action);
                }
            }
            Message::ComposeClose => self.compose = None,
            Message::ComposeSend => {
                let Some(compose) = self.compose.as_mut() else {
                    return Task::none();
                };
                if compose.busy {
                    return Task::none();
                }
                let message = match compose.outgoing() {
                    Ok(message) => message,
                    Err(error) => {
                        compose.notice = Some(error);
                        return Task::none();
                    }
                };
                if message.recipients().is_empty() {
                    compose.notice = Some("Add at least one recipient.".to_string());
                    return Task::none();
                }
                compose.busy = true;
                compose.notice = None;
                return Task::perform(api::send_mail(message), Message::ComposeSent);
            }
            Message::ComposeSent(result) => match result {
                Ok(outcome) => {
                    self.apply_snapshot(outcome.snapshot);
                    self.compose = None;
                    self.error = Some(if outcome.queued {
                        "Message queued; it will send when you are online.".to_string()
                    } else {
                        "Message sent.".to_string()
                    });
                }
                Err(error) => {
                    if let Some(compose) = self.compose.as_mut() {
                        compose.busy = false;
                        compose.notice = Some(error.clone());
                    }
                    self.error = Some(error);
                }
            },
            Message::ComposeSaveDraft => {
                let Some(compose) = self.compose.as_mut() else {
                    return Task::none();
                };
                if compose.busy {
                    return Task::none();
                }
                let message = match compose.outgoing() {
                    Ok(message) => message,
                    Err(error) => {
                        compose.notice = Some(error);
                        return Task::none();
                    }
                };
                let id = compose.id.clone();
                compose.busy = true;
                compose.notice = None;
                return match id {
                    Some(id) => Task::perform(
                        async move {
                            api::update_draft(&id, message)?;
                            Ok(id)
                        },
                        Message::ComposeSaved,
                    ),
                    None => Task::perform(
                        async move { api::save_draft(message) },
                        Message::ComposeSaved,
                    ),
                };
            }
            Message::ComposeSaved(result) => match result {
                Ok(_) => {
                    self.compose = None;
                    self.error = Some("Draft saved.".to_string());
                }
                Err(error) => {
                    if let Some(compose) = self.compose.as_mut() {
                        compose.busy = false;
                        compose.notice = Some(error.clone());
                    }
                    self.error = Some(error);
                }
            },
            Message::ComposeDiscard => {
                if let Some(compose) = self.compose.take()
                    && let Some(id) = compose.id
                {
                    return Task::perform(
                        async move { api::discard_draft(&id) },
                        Message::ComposeDiscarded,
                    );
                }
            }
            Message::ComposeDiscarded(result) => match result {
                Ok(snapshot) => self.apply_snapshot(snapshot),
                Err(error) => self.error = Some(error),
            },
            Message::Settings | Message::Noop => {}
        }

        Task::none()
    }

    /// Build a reply or forward prefill from the open conversation.
    fn reply_or_forward(&mut self, reply: bool) -> Task<Message> {
        let Some(thread_id) = self
            .snapshot
            .selected_conversation
            .as_ref()
            .map(|detail| detail.thread_id.clone())
        else {
            return Task::none();
        };
        Task::perform(
            async move {
                if reply {
                    api::compose_reply(&thread_id)
                } else {
                    api::compose_forward(&thread_id)
                }
            },
            |result| {
                Message::ComposeLoaded(result.map(|message| ComposeSeed { id: None, message }))
            },
        )
    }

    fn select_folder(&mut self, folder: FolderId) -> Task<Message> {
        self.drawer_open = false;
        snapshot_task(async move { api::select_folder(&folder).await })
    }

    /// The thread id of the conversation open in the reader, if any.
    fn open_thread(&self) -> Option<ThreadId> {
        self.snapshot
            .selected_conversation
            .as_ref()
            .map(|detail| detail.thread_id.clone())
    }

    /// Return to the list, re-selecting the current view so the reader clears.
    fn reader_back(&mut self) -> Task<Message> {
        if self.snapshot.unified_selected {
            snapshot_task(api::select_unified())
        } else if let Some(folder) = self.snapshot.selected_folder.clone() {
            snapshot_task(async move { api::select_folder(&folder).await })
        } else {
            Task::none()
        }
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
    let (id, snapshot) = match editing {
        Some(id) => {
            let snapshot = match api::update_account(&id, input.clone()).await {
                Ok(snapshot) => snapshot,
                Err(error) if credentials_unavailable(&error) => {
                    warning = Some(session_only_warning());
                    api::update_account_with_session(&id, input.clone()).await?
                }
                Err(error) => return Err(error),
            };
            (id, snapshot)
        }
        None => match api::add_account_and_id(input.clone()).await {
            Ok(added) => added,
            Err(error) if credentials_unavailable(&error) => {
                warning = Some(session_only_warning());
                api::add_account_with_session_and_id(input.clone()).await?
            }
            Err(error) => return Err(error),
        },
    };

    if let Err(error) = api::sync_account(&id).await {
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
