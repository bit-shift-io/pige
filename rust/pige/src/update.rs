//! The Elm update loop: turn messages into state changes and API tasks.

use std::future::Future;
use std::time::Duration;

use iced::window;
use iced::{Subscription, Task};
use pige_core::account::AccountId;
use pige_core::api::{self, AccountInput, ConfigOutcome, ModelSnapshot};
use pige_core::mail::{FolderId, ThreadId};
use pige_core::platform::credentials_unavailable;
use pige_core::settings::{Settings, ThemeMode};
use tokio::sync::broadcast::error::RecvError;

use crate::app::App;
use crate::layout::Breakpoint;
use crate::message::{AccountSaveResult, Message};
use crate::screens::accounts::{AccountField, TestState};
use crate::screens::compose::{ComposeSeed, ComposeState};

impl App {
    /// Show `message` in the shared toast and schedule its expiry. A newer
    /// toast supersedes an older one; the generation guard keeps an older
    /// expiry from clearing a newer message.
    pub(crate) fn show_toast(&mut self, message: String) -> Task<Message> {
        let generation = self.toast.show(message);
        Task::perform(
            async move {
                tokio::time::sleep(Duration::from_secs(4)).await;
                generation
            },
            Message::ToastExpired,
        )
    }

    /// Apply a restore or reset: refresh the snapshot, adopt the new settings
    /// (theme and background sync), and keep the edited interval text in step.
    fn apply_config_outcome(&mut self, outcome: ConfigOutcome) {
        self.apply_snapshot(outcome.snapshot);
        self.settings = outcome.settings;
        self.settings_sync = self.settings.sync_minutes.to_string();
        match self.settings.theme {
            ThemeMode::Light => self.set_dark(false),
            ThemeMode::Dark => self.set_dark(true),
            ThemeMode::System => {}
        }
        crate::platform::apply_background_sync(
            self.settings.background_sync,
            self.settings.sync_minutes,
        );
    }

    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Snapshot(snapshot) => self.apply_snapshot(snapshot),
            Message::Resized(size) => {
                self.size = size;
                if Breakpoint::from_width(size.width) == Breakpoint::Expanded {
                    self.drawer_open = false;
                }
            }
            Message::Insets(insets) => self.insets = insets,
            Message::Spin => self.spin_phase = self.spin_phase.wrapping_add(1),
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
                return synced_with_prefetch(api::sync_all());
            }
            Message::Synced(result) => match result {
                Ok(snapshot) => {
                    self.apply_snapshot(snapshot);
                    return prefetch_task();
                }
                Err(error) => return self.show_toast(error),
            },
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
                        Err(error) => return self.show_toast(error),
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
                // Mark read through the server-backed, current-folder-scoped
                // path. The automatic open path stays silent when offline: a
                // failed STORE must not raise a snackbar the user did not ask
                // for, and the next sync reconciles flags from the server.
                let mark_thread = thread_id.clone();
                let mark_read = Task::perform(
                    async move {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        match api::set_read(&mark_thread, true).await {
                            Ok(snapshot) => Message::Snapshot(snapshot),
                            Err(_) => Message::Noop,
                        }
                    },
                    |message| message,
                );
                return Task::batch([open, mark_read]);
            }
            Message::ToggleEarlier => self.reader_expanded = !self.reader_expanded,
            Message::ConversationBack => return self.reader_back(),
            Message::Back => {
                if self.delete_confirm_open {
                    self.delete_confirm_open = false;
                } else if self.move_picker_open {
                    self.move_picker_open = false;
                } else if self.compose.is_some() {
                    return self.close_compose();
                } else if self.accounts_open {
                    self.accounts_open = false;
                    self.accounts.close_form();
                    self.accounts.confirm_remove = None;
                } else if self.advanced_open {
                    self.advanced_open = false;
                } else if self.settings_open {
                    self.settings_open = false;
                } else if self.drawer_open {
                    self.drawer_open = false;
                } else if self.snapshot.selected_conversation.is_some() {
                    return self.reader_back();
                }
            }
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
                    return self.show_toast(error);
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
                self.settings_open = false;
                self.accounts_open = true;
                self.accounts.open(&self.snapshot);
            }
            Message::AddAccount => {
                self.drawer_open = false;
                self.settings_open = false;
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
                Err(error) => return self.show_toast(error),
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
                        return self.show_toast(warning);
                    }
                }
                Err(error) => {
                    if let Some(form) = self.accounts.form.as_mut() {
                        form.busy = false;
                        form.test = TestState::Failed(error.clone());
                    }
                    return self.show_toast(error);
                }
            },
            Message::AccountRemove(id) => {
                // The remove confirmation lives on the account screen, so a
                // remove started from settings hands off to it rather than
                // duplicating the dialog.
                if self.settings_open {
                    self.settings_open = false;
                    self.accounts_open = true;
                    self.accounts.open(&self.snapshot);
                }
                self.accounts.confirm_remove = Some(id);
            }
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
                Err(error) => return self.show_toast(error),
            },
            Message::AccountLabelDraft(id, value) => {
                self.accounts.label_drafts.insert(id, value);
            }
            Message::AccountLabelCommit(id) => {
                let Some(label) = self.accounts.label_drafts.get(&id).cloned() else {
                    return Task::none();
                };
                if label.trim().is_empty() {
                    return self.show_toast("Account label is required".to_string());
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
                Err(error) => return self.show_toast(error),
            },
            Message::Error(error) => return self.show_toast(error),
            Message::Toast(message) => return self.show_toast(message),
            Message::ToastExpired(generation) => self.toast.expire(generation),
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
                        return self.show_toast("Add an account before composing.".to_string());
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
                Err(error) => return self.show_toast(error),
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
            Message::ComposeClose => return self.close_compose(),
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
                    return self.show_toast(if outcome.queued {
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
                    return self.show_toast(error);
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
                    return self.show_toast("Draft saved.".to_string());
                }
                Err(error) => {
                    if let Some(compose) = self.compose.as_mut() {
                        compose.busy = false;
                        compose.notice = Some(error.clone());
                    }
                    return self.show_toast(error);
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
                Err(error) => return self.show_toast(error),
            },
            Message::Settings => {
                self.drawer_open = false;
                self.accounts_open = false;
                self.settings_open = true;
                // The settings list edits labels through the shared account
                // actions, so seed their drafts from the authoritative snapshot.
                self.accounts.open(&self.snapshot);
                // Read the stored settings off the UI thread; `SettingsLoaded`
                // fills in the screen once the query finishes.
                return Task::perform(async { api::settings() }, Message::SettingsLoaded);
            }
            Message::SettingsLoaded(settings) => {
                self.settings = settings;
                self.settings_sync = self.settings.sync_minutes.to_string();
                match self.settings.theme {
                    ThemeMode::Light => self.set_dark(false),
                    ThemeMode::Dark => self.set_dark(true),
                    ThemeMode::System => {
                        return iced::system::theme().map(Message::SystemTheme);
                    }
                }
            }
            Message::CloseSettings => {
                self.settings_open = false;
                self.advanced_open = false;
            }
            Message::OpenAdvanced => self.advanced_open = true,
            Message::CloseAdvanced => self.advanced_open = false,
            Message::Advanced(row) => {
                use crate::screens::advanced::Row;
                match row {
                    Row::Backup => {
                        return Task::perform(api::backup_config(), Message::BackupDone);
                    }
                    Row::Restore => {
                        // Backups written by a previous install are hidden from
                        // the scoped query until "All files access" is granted,
                        // so the permission is checked before listing.
                        return Task::perform(
                            async { crate::storage::has_all_files_access() },
                            Message::RestoreAccessChecked,
                        );
                    }
                    Row::FactoryReset => self.confirm_reset = true,
                }
            }
            Message::BackupDone(result) => {
                return self.show_toast(crate::screens::advanced::backup_message(&result));
            }
            Message::BackupsListed(result) => match result {
                Ok(names) if names.is_empty() => {
                    return self.show_toast(crate::screens::advanced::NO_BACKUPS.to_string());
                }
                Ok(names) => self.restore_picker = Some(names),
                Err(error) => return self.show_toast(error),
            },
            Message::RestoreAccessChecked(granted) => {
                if granted {
                    return Task::perform(api::list_backups(), Message::BackupsListed);
                }
                return Task::perform(
                    async { crate::storage::request_all_files_access() },
                    Message::RestoreAccessRequested,
                );
            }
            Message::RestoreAccessRequested(result) => {
                let message = match result {
                    Ok(()) => crate::screens::advanced::NEEDS_ALL_FILES_ACCESS.to_string(),
                    Err(error) => error,
                };
                return self.show_toast(message);
            }
            Message::RestoreChosen(name) => {
                // The picker closes and the confirmation opens; nothing is
                // replaced until Restore is confirmed.
                self.restore_picker = None;
                self.restore_confirm = Some(name);
            }
            Message::RestoreConfirmed => {
                let Some(name) = self.restore_confirm.take() else {
                    return Task::none();
                };
                let restore_name = name.clone();
                return Task::perform(
                    async move { api::restore_config(restore_name).await },
                    move |result| Message::RestoreDone(name.clone(), result),
                );
            }
            Message::RestoreCancelled => {
                self.restore_picker = None;
                self.restore_confirm = None;
            }
            Message::RestoreDone(name, result) => {
                let message = crate::screens::advanced::restore_message(&result, &name);
                self.restore_picker = None;
                self.restore_confirm = None;
                if let Ok(outcome) = result {
                    self.apply_config_outcome(outcome);
                }
                return self.show_toast(message);
            }
            Message::ResetConfirmed => {
                self.confirm_reset = false;
                return Task::perform(api::factory_reset(), Message::ResetDone);
            }
            Message::ResetCancelled => self.confirm_reset = false,
            Message::ResetDone(result) => {
                self.confirm_reset = false;
                match result {
                    Ok(outcome) => {
                        let mut message = "The app was reset.".to_string();
                        for warning in &outcome.warnings {
                            message.push(' ');
                            message.push_str(warning);
                        }
                        self.apply_config_outcome(outcome);
                        return self.show_toast(message);
                    }
                    Err(error) => return self.show_toast(error),
                }
            }
            Message::SettingsSyncChanged(value) => self.settings_sync = value,
            Message::SettingsNotificationsToggled(on) => self.settings.notifications = on,
            Message::SettingsBackgroundSyncToggled(on) => self.settings.background_sync = on,
            Message::SettingsPrefetchToggled(on) => self.settings.prefetch_bodies = on,
            Message::SettingsThemeChanged(mode) => {
                self.settings.theme = mode;
                match mode {
                    ThemeMode::Light => self.set_dark(false),
                    ThemeMode::Dark => self.set_dark(true),
                    ThemeMode::System => {
                        return iced::system::theme().map(Message::SystemTheme);
                    }
                }
            }
            Message::SettingsSave => {
                let minutes = self
                    .settings_sync
                    .trim()
                    .parse()
                    .unwrap_or(self.settings.sync_minutes);
                self.settings.sync_minutes = Settings::clamp_minutes(minutes);
                self.settings_sync = self.settings.sync_minutes.to_string();
                // Write the settings off the UI thread; `SettingsSaved` reports
                // the result and reschedules background sync.
                let settings = self.settings.clone();
                return Task::perform(
                    async move { api::save_settings(&settings) },
                    Message::SettingsSaved,
                );
            }
            Message::SettingsSaved(result) => match result {
                Ok(()) => {
                    crate::platform::apply_background_sync(
                        self.settings.background_sync,
                        self.settings.sync_minutes,
                    );
                    return self.show_toast("Settings saved.".to_string());
                }
                Err(error) => return self.show_toast(error),
            },
            Message::SystemTheme(mode) => {
                if self.settings.theme == ThemeMode::System
                    && let Some(dark) = system_theme_dark(mode)
                {
                    self.set_dark(dark);
                }
            }
            Message::Noop => {}
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

    /// Close the compose surface. A non-empty form is saved as a draft through
    /// the normal save path so typed text is not silently discarded; the
    /// surface stays open until the save result arrives. An empty form closes
    /// immediately, and a form whose recipients do not parse is dropped rather
    /// than reported on close.
    fn close_compose(&mut self) -> Task<Message> {
        let Some(compose) = self.compose.as_ref() else {
            return Task::none();
        };
        // A send or save is already in flight; the Cancel action closes the
        // surface without starting a second one.
        if compose.busy {
            self.compose = None;
            return Task::none();
        }
        if compose.is_empty() {
            self.compose = None;
            return Task::none();
        }
        let outgoing = compose.outgoing();
        let id = compose.id.clone();
        match outgoing {
            Ok(message) => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.busy = true;
                    compose.notice = None;
                }
                match id {
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
                }
            }
            Err(_) => {
                self.compose = None;
                Task::none()
            }
        }
    }

    /// Apply a light/dark change to the shell and, on Android, the system-bar
    /// icon polarity. A no-op off Android.
    pub(crate) fn set_dark(&mut self, dark: bool) {
        self.dark = dark;
        iced_android::set_system_bars_dark(dark);
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
        // Only follow the OS palette while the user chose "System"; an explicit
        // Light/Dark choice must not be overridden by a desktop theme change.
        let system_theme = if self.settings.theme == ThemeMode::System {
            iced::system::theme_changes().map(Message::SystemTheme)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            if self
                .snapshot
                .selected_conversation
                .as_ref()
                .is_some_and(|d| {
                    d.messages
                        .iter()
                        .any(|m| matches!(m.body, Some(api::MessageBody::Loading)))
                })
            {
                iced::time::every(Duration::from_millis(80)).map(|_| Message::Spin)
            } else {
                Subscription::none()
            },
            window::resize_events().map(|(_, size)| Message::Resized(size)),
            system_theme,
            // Edge-to-edge is mandatory on Android 15+. The window covers the
            // whole screen, so the root view pads by these insets to stay clear
            // of the status bar, navigation bar, cutout, and soft keyboard.
            // Other platforms return an empty subscription.
            iced_android::insets().map(Message::Insets),
            Subscription::run(core_events),
            iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick),
            // Android 13+ stops calling `onBackPressed` once
            // `enableOnBackInvokedCallback` is set; iced_android registers the
            // system callback and reports the gesture as a `BrowserBack` key
            // press instead (see its `nativeBack`). Map it to the same Back
            // message the `nativeBack`/`onBackPressedEvent` JNI exports use so
            // the gesture closes the innermost surface. Desktop never emits
            // `BrowserBack`.
            iced::keyboard::listen().filter_map(|event| match event {
                // Desktop Escape and the Android Back gesture both close the
                // innermost surface.
                iced::keyboard::Event::KeyPressed {
                    key:
                        iced::keyboard::Key::Named(
                            iced::keyboard::key::Named::Escape
                            | iced::keyboard::key::Named::BrowserBack,
                        ),
                    ..
                } => Some(Message::Back),
                _ => None,
            }),
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

/// Map the OS theme preference to the shell's dark flag. `None` (the platform
/// did not report a preference) leaves the current choice alone.
fn system_theme_dark(mode: iced::theme::Mode) -> Option<bool> {
    match mode {
        iced::theme::Mode::Light => Some(false),
        iced::theme::Mode::Dark => Some(true),
        iced::theme::Mode::None => None,
    }
}

fn session_only_warning() -> String {
    "Secret Service is unavailable. The account password is kept for this session only and will not be saved after pige exits.".to_string()
}

/// Deliver sync results before starting downloads, so prefetch cannot delay UI.
pub(crate) fn synced_with_prefetch(
    future: impl Future<Output = Result<ModelSnapshot, String>> + Send + 'static,
) -> Task<Message> {
    Task::perform(future, Message::Synced)
}

fn prefetch_task() -> Task<Message> {
    Task::perform(api::prefetch_bodies(), |_| Message::Noop)
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
                            api::CoreEvent::BackPressed => Message::Back,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_sync_applies_snapshot_and_schedules_separate_prefetch() {
        let mut app = App::for_test();
        let mut snapshot = ModelSnapshot::default();
        snapshot.unified_inbox_unread = 7;
        let task = app.update(Message::Synced(Ok(snapshot)));
        assert_eq!(app.snapshot.unified_inbox_unread, 7);
        assert!(task.units() > 0, "prefetch is scheduled after applying mail");
    }

    #[test]
    fn spin_tick_advances_and_wraps() {
        let mut app = App::for_test();
        app.spin_phase = u32::MAX;
        let _ = app.update(Message::Spin);
        assert_eq!(app.spin_phase, 0);
        let _ = app.update(Message::Spin);
        assert_eq!(app.spin_phase, 1);
    }

    #[test]
    fn prefetch_toggle_updates_settings() {
        let mut app = App::for_test();
        let _ = app.update(Message::SettingsPrefetchToggled(false));
        assert!(!app.settings.prefetch_bodies);
    }

    #[test]
    fn system_theme_maps_to_dark_light_and_ignores_none() {
        assert_eq!(system_theme_dark(iced::theme::Mode::Dark), Some(true));
        assert_eq!(system_theme_dark(iced::theme::Mode::Light), Some(false));
        // A platform that reports no preference must not override the user's.
        assert_eq!(system_theme_dark(iced::theme::Mode::None), None);
    }

    #[test]
    fn restore_asks_for_confirmation_before_replacing() {
        let mut app = App::for_test();
        app.restore_picker = Some(vec!["a.json".to_string()]);
        let _ = app.update(Message::RestoreChosen("a.json".to_string()));

        assert!(app.restore_picker.is_none(), "the picker closes");
        assert_eq!(
            app.restore_confirm.as_deref(),
            Some("a.json"),
            "the restore must wait for confirmation"
        );
    }

    #[test]
    fn cancelling_the_restore_confirmation_leaves_that_pending_name_off() {
        let mut app = App::for_test();
        app.restore_confirm = Some("a.json".to_string());
        let _ = app.update(Message::RestoreCancelled);
        assert!(app.restore_confirm.is_none());
    }

    #[test]
    fn factory_reset_requires_confirmation() {
        let mut app = App::for_test();
        let _ = app.update(Message::Advanced(
            crate::screens::advanced::Row::FactoryReset,
        ));
        assert!(app.confirm_reset, "the reset must wait for confirmation");
    }

    #[test]
    fn empty_backup_list_shows_a_toast_not_a_dialog() {
        let mut app = App::for_test();
        let _ = app.update(Message::BackupsListed(Ok(Vec::new())));
        assert!(app.restore_picker.is_none());
        assert_eq!(
            app.toast.current.as_ref().unwrap().1,
            crate::screens::advanced::NO_BACKUPS
        );
    }

    #[test]
    fn restore_success_applies_settings_and_snapshot_and_closes_the_picker() {
        let mut app = App::for_test();
        app.restore_picker = Some(vec!["a.json".to_string()]);
        let outcome = ConfigOutcome {
            snapshot: ModelSnapshot::default(),
            settings: Settings {
                notifications: false,
                ..Settings::default()
            },
            warnings: Vec::new(),
        };
        let _ = app.update(Message::RestoreDone("a.json".to_string(), Ok(outcome)));

        assert!(app.restore_picker.is_none());
        assert!(!app.settings.notifications, "restored settings must apply");
        assert!(app.toast.current.is_some(), "restore reports via toast");
    }
}
