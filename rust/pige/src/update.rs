//! The Elm update loop: turn messages into state changes and API tasks.

use std::future::Future;
use std::time::Duration;

use iced::window;
use iced::{Subscription, Task};
use pige_core::api::{self, ModelSnapshot};
use pige_core::mail::FolderId;
use tokio::sync::broadcast::error::RecvError;

use crate::app::{App, account_for_folder};
use crate::layout::Breakpoint;
use crate::message::Message;

impl App {
    pub(crate) fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Snapshot(snapshot) => {
                self.snapshot = snapshot;
                if let Some(account) = &self.selected_account
                    && !self
                        .snapshot
                        .sections
                        .iter()
                        .any(|section| &section.account_id == account)
                {
                    self.selected_account = None;
                }
            }
            Message::Resized(size) => {
                self.size = size;
                if Breakpoint::from_width(size.width) == Breakpoint::Expanded {
                    self.drawer_open = false;
                }
            }
            Message::Tick => {
                // The event stream is authoritative, but a tick also catches
                // anything missed while the UI was paused.
                self.snapshot = api::snapshot();
            }
            Message::ToggleDrawer => self.drawer_open = true,
            Message::CloseDrawer => self.drawer_open = false,
            Message::SelectAllInboxes => {
                let first_inbox = self
                    .snapshot
                    .sections
                    .iter()
                    .flat_map(|section| section.folders.iter())
                    .find(|folder| folder.kind == pige_core::mail::FolderKind::Inbox)
                    .map(|folder| folder.id.clone());
                if let Some(folder) = first_inbox {
                    return self.select_folder(folder);
                }
                self.drawer_open = false;
            }
            Message::SelectFolder(folder) => return self.select_folder(folder),
            Message::ToggleAccount(account) => {
                if !self.collapsed.remove(&account) {
                    self.collapsed.insert(account);
                }
            }
            Message::LoadOlder => {
                if let Some(folder) = self.snapshot.selected_folder.clone() {
                    return snapshot_task(async move { api::load_older(&folder).await });
                }
            }
            Message::SyncNow | Message::Refresh => {
                return snapshot_task(api::sync_all());
            }
            Message::Search(query) => self.search = query,
            Message::OpenConversation(_) => {
                // The reader and its message selection arrive in Task 9.
            }
            Message::Error(error) => self.error = Some(error),
            Message::DismissError => self.error = None,
            Message::Compose | Message::AddAccount | Message::Settings | Message::Noop => {}
        }

        Task::none()
    }

    fn select_folder(&mut self, folder: FolderId) -> Task<Message> {
        self.drawer_open = false;
        self.selected_account = account_for_folder(&self.snapshot, &folder);
        snapshot_task(async move { api::select_folder(&folder).await })
    }

    pub(crate) fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::resize_events().map(|(_, size)| Message::Resized(size)),
            Subscription::run(core_events),
            iced::time::every(Duration::from_secs(30)).map(|_| Message::Tick),
        ])
    }
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
