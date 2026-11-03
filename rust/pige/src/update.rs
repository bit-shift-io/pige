//! The Elm update loop: turn messages into state changes and API tasks.

use std::future::Future;
use std::time::Duration;

use iced::window;
use iced::{Subscription, Task};
use pige_core::api::{self, ModelSnapshot};
use pige_core::mail::FolderId;
use tokio::sync::broadcast::error::RecvError;

use crate::app::App;
use crate::layout::Breakpoint;
use crate::message::Message;

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
            Message::Search(query) => self.search = query,
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
                if let Some(detail) = self.snapshot.selected_conversation.as_mut() {
                    crate::screens::reader::load_images(detail);
                }
            }
            Message::ToggleEarlier => self.reader_expanded = !self.reader_expanded,
            Message::OpenUrl(url) => {
                if let Err(error) = api::open_url(&url) {
                    self.error = Some(error);
                }
            }
            Message::Error(error) => self.error = Some(error),
            Message::DismissError => self.error = None,
            Message::Compose | Message::AddAccount | Message::Settings | Message::Noop => {}
        }

        Task::none()
    }

    fn select_folder(&mut self, folder: FolderId) -> Task<Message> {
        self.drawer_open = false;
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
