//! The reader pane: subject, sender summary, message body, and an expandable
//! "Earlier messages" section. Remote images stay as blocked placeholders; v1
//! never fetches remote bytes, so there is no "Load images" action.

use iced::widget::{column, container, row, scrollable, space};
use iced::{Alignment, Element, Fill};
use pige_core::account::AccountId;
use pige_core::api::{self, ConversationDetail, MessageBody, MessageSummary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderBody {
    Document,
    TooLarge,
    Loading,
    Missing,
}

pub fn reader_body_kind(message: &MessageSummary) -> ReaderBody {
    match &message.body {
        Some(MessageBody::Document(_)) => ReaderBody::Document,
        Some(MessageBody::TooLarge) => ReaderBody::TooLarge,
        Some(MessageBody::Loading) => ReaderBody::Loading,
        None => ReaderBody::Missing,
    }
}

use pige_core::mail::FolderKind;
use pige_core::transport::Capabilities;

use crate::app::App;
use crate::components::{self, ButtonVariant, PaneTone};
use crate::html_view;
use crate::icons;
use crate::message::Message;
use crate::theme::{self, TextRole};
use crate::{avatar_color, initials};

/// Render the reader pane for the current selection.
pub fn reader_pane<'a>(app: &'a App) -> Element<'a, Message> {
    let (leading, actions) = toolbar(app);

    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return container(column![
            components::app_bar("Conversation", leading, actions),
            components::empty_state(
                "No conversation selected",
                "Choose a message from the list to read it here."
            ),
        ])
        .width(Fill)
        .height(Fill)
        .style(components::pane_style(PaneTone::Surface))
        .into();
    };

    let body = conversation_body(detail, app);
    container(column![
        components::app_bar("Conversation", leading, actions),
        body,
    ])
    .width(Fill)
    .height(Fill)
    .style(components::pane_style(PaneTone::Surface))
    .into()
}

/// The reader's toolbar: a back button plus the mutation actions available for
/// the open conversation's account and current folder.
fn toolbar(app: &App) -> (Option<Element<'_, Message>>, Vec<Element<'_, Message>>) {
    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return (None, Vec::new());
    };
    let leading = Some(components::icon_button(
        icons::ARROW_BACK,
        "Back",
        Message::ConversationBack,
    ));

    let Some(context) = reader_context(app, &detail.account_id) else {
        return (leading, Vec::new());
    };

    let archive = api::archive_available(
        &context.capabilities,
        context.current_kind,
        context.has_archive,
    );
    let trash = context.has_trash && context.current_kind != FolderKind::Trash;
    let permanent = context.current_kind == FolderKind::Trash || !context.has_trash;
    let read = detail
        .messages
        .first()
        .map(|message| message.flags.seen)
        .unwrap_or(true);

    let mut actions = Vec::new();
    if archive {
        actions.push(components::icon_button(
            icons::ARCHIVE,
            "Archive",
            Message::ArchiveConversation,
        ));
    }
    if trash {
        actions.push(components::icon_button(
            icons::DELETE,
            "Delete",
            Message::TrashConversation,
        ));
    } else if permanent {
        actions.push(components::icon_button(
            icons::DELETE,
            "Delete permanently",
            Message::OpenDeleteConfirm,
        ));
    }
    if read {
        actions.push(components::icon_button(
            icons::MARK_UNREAD,
            "Mark unread",
            Message::MarkUnread,
        ));
    }
    actions.push(components::icon_button(
        icons::REPLY,
        "Reply",
        Message::Reply,
    ));
    actions.push(components::icon_button(
        icons::FORWARD,
        "Forward",
        Message::Forward,
    ));
    actions.push(components::icon_button(
        icons::MORE_VERT,
        "More",
        Message::OpenMovePicker,
    ));
    (leading, actions)
}

/// The capabilities and special-folder availability for the open conversation.
struct ReaderContext {
    capabilities: Capabilities,
    current_kind: FolderKind,
    has_archive: bool,
    has_trash: bool,
}

fn reader_context(app: &App, account_id: &AccountId) -> Option<ReaderContext> {
    let section = app
        .snapshot
        .sections
        .iter()
        .find(|section| &section.account_id == account_id)?;
    let current_kind = app
        .snapshot
        .selected_folder
        .as_ref()
        .and_then(|folder| {
            section
                .folders
                .iter()
                .find(|candidate| &candidate.id == folder)
        })
        .map(|folder| folder.kind)
        .unwrap_or(FolderKind::Inbox);
    Some(ReaderContext {
        capabilities: section.capabilities,
        current_kind,
        has_archive: section
            .folders
            .iter()
            .any(|folder| folder.kind == FolderKind::Archive),
        has_trash: section
            .folders
            .iter()
            .any(|folder| folder.kind == FolderKind::Trash),
    })
}

/// The "Move to folder" picker, listing the current account's other folders.
pub fn move_picker_modal<'a>(app: &'a App) -> Element<'a, Message> {
    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return space().into();
    };
    let Some(section) = app
        .snapshot
        .sections
        .iter()
        .find(|section| section.account_id == detail.account_id)
    else {
        return space().into();
    };
    let palette = theme::active();
    let current = app.snapshot.selected_folder.as_ref();
    let mut list = column![].spacing(palette.space.xs).width(Fill);
    let mut any = false;
    for folder in &section.folders {
        if current == Some(&folder.id) {
            continue;
        }
        any = true;
        list = list.push(components::button(
            folder.name.clone(),
            ButtonVariant::Text,
            Message::MoveConversation(folder.id.clone()),
        ));
    }
    if !any {
        list = list.push(components::styled_text(
            "No other folders to move to.".to_string(),
            palette.role(TextRole::BodyMedium),
            palette.on_surface_variant,
        ));
    }
    components::dialog(
        "Move to folder",
        list.into(),
        vec![components::button(
            "Cancel",
            ButtonVariant::Text,
            Message::CloseMovePicker,
        )],
    )
}

/// The permanent-delete confirmation dialog.
pub fn delete_confirm_modal<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    let body = components::styled_text(
        "This permanently deletes the conversation and cannot be undone.".to_string(),
        palette.role(TextRole::BodyMedium),
        palette.on_surface_variant,
    );
    components::dialog(
        "Delete permanently?",
        body.into(),
        vec![
            components::button("Cancel", ButtonVariant::Text, Message::CancelDeleteConfirm),
            components::button("Delete", ButtonVariant::Danger, Message::ConfirmDelete),
        ],
    )
}

/// The scrollable reader content for one conversation.
fn conversation_body<'a>(detail: &'a ConversationDetail, app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();
    let mut content = column![].spacing(0).width(Fill);

    // Subject and sender summary.
    content = content.push(header(detail));

    let Some(latest) = detail.messages.first() else {
        return scrollable(content)
            .width(Fill)
            .height(Fill)
            .style(palette.scrollable())
            .into();
    };

    // Body, or a clear state when it is unavailable. Remote images remain
    // "[image blocked]" placeholders: v1 fetches no remote bytes.
    match reader_body_kind(latest) {
        ReaderBody::Loading => {
            content = content.push(
                container(components::spinner(app.spin_phase as f32))
                    .center_x(Fill)
                    .padding(palette.space.lg),
            );
        }
        ReaderBody::Document => {
            let Some(MessageBody::Document(document)) = &latest.body else {
                unreachable!()
            };
            let rendered = html_view::render_document(document, Message::OpenUrl);
            let mut body = column![].spacing(palette.space.xs).width(Fill);
            for element in rendered {
                body = body.push(element);
            }
            content = content.push(
                container(body)
                    .width(Fill)
                    .padding([palette.space.xs, palette.space.lg]),
            );
        }
        ReaderBody::TooLarge => {
            content = content.push(
                container(components::styled_text(
                    "This message is too large to display.",
                    palette.role(TextRole::BodyMedium),
                    palette.on_surface_variant,
                ))
                .width(Fill)
                .padding(palette.space.lg),
            );
        }
        ReaderBody::Missing => {
            content = content.push(
                container(components::styled_text(
                    "This message has no readable body, or it could not be loaded.",
                    palette.role(TextRole::BodyMedium),
                    palette.on_surface_variant,
                ))
                .width(Fill)
                .padding(palette.space.lg),
            );
        }
    }

    // One chip per user-visible attachment. Metadata is stored when the body
    // is fetched; a cached pre-v3 body has no metadata and shows no chips.
    if !latest.attachments.is_empty() {
        content = content.push(attachment_chips(latest));
    }

    // Earlier messages (collapsed; empty until threading).
    if detail.messages.len() > 1 {
        content = content.push(earlier_messages(detail, app));
    }

    scrollable(content)
        .width(Fill)
        .height(Fill)
        .style(palette.scrollable())
        .into()
}

/// Subject line plus the newest message's sender summary.
fn header<'a>(detail: &'a ConversationDetail) -> Element<'a, Message> {
    let palette = theme::active();
    let subject = if detail.subject.trim().is_empty() {
        "(no subject)"
    } else {
        detail.subject.as_str()
    };

    let mut column_el = column![].spacing(palette.space.xs).width(Fill);
    column_el = column_el.push(components::styled_text(
        subject.to_string(),
        palette.role(TextRole::TitleLarge),
        palette.on_surface,
    ));

    if let Some(latest) = detail.messages.first() {
        let sender = if latest.sender.trim().is_empty() {
            "Unknown sender"
        } else {
            latest.sender.as_str()
        };
        let avatar = components::avatar(
            &initials(sender),
            avatar_color(sender),
            palette.metrics.avatar_size,
        );
        let mut meta = column![].spacing(0);
        meta = meta.push(components::styled_text(
            sender.to_string(),
            palette.role(TextRole::TitleMedium),
            palette.on_surface,
        ));
        if !latest.to.is_empty() {
            meta = meta.push(components::styled_text(
                format!("To {}", latest.to),
                palette.role(TextRole::BodySmall),
                palette.on_surface_variant,
            ));
        }
        column_el = column_el.push(
            row![avatar, meta]
                .spacing(palette.space.sm)
                .align_y(Alignment::Center),
        );
    }

    container(column_el)
        .width(Fill)
        .padding(iced::Padding {
            top: palette.space.lg,
            right: palette.space.lg,
            bottom: palette.space.sm,
            left: palette.space.lg,
        })
        .into()
}

/// One attachment chip per MIME part. Tapping asks the core to download (if
/// needed), cache, and open the file through the platform opener.
fn attachment_chips<'a>(message: &'a MessageSummary) -> Element<'a, Message> {
    let palette = theme::active();
    let mut chips = column![].spacing(palette.space.xs).width(Fill);
    for attachment in &message.attachments {
        chips = chips.push(components::attachment_chip(
            &attachment.filename,
            attachment.size,
            Message::OpenAttachment(message.id.clone(), attachment.part_id.clone()),
        ));
    }
    container(chips)
        .width(Fill)
        .padding([palette.space.xs, palette.space.lg])
        .into()
}

/// The expandable earlier-message section.
fn earlier_messages<'a>(detail: &'a ConversationDetail, app: &'a App) -> Element<'a, Message> {
    let palette = theme::active();

    let mut section = column![components::button(
        "Earlier messages",
        ButtonVariant::Text,
        Message::ToggleEarlier
    ),]
    .spacing(0)
    .width(Fill);

    if app.reader_expanded {
        for earlier in detail.messages.iter().skip(1) {
            section = section.push(earlier_message(earlier));
        }
    }

    container(section)
        .width(Fill)
        .padding([palette.space.sm, palette.space.lg])
        .into()
}

/// One earlier message inside the expanded section: sender, date, and its
/// cached body. A body that was never downloaded shows the snippet plus a short
/// "Not downloaded" label rather than a blank or dead control.
fn earlier_message<'a>(message: &'a MessageSummary) -> Element<'a, Message> {
    let palette = theme::active();
    let sender = if message.sender.trim().is_empty() {
        "Unknown sender".to_string()
    } else {
        message.sender.clone()
    };
    let date = crate::format_list_date(now_unix(), message.date);

    let mut meta = row![].spacing(palette.space.sm).align_y(Alignment::Center);
    meta = meta.push(components::styled_text(
        sender,
        palette.role(TextRole::TitleSmall),
        palette.on_surface,
    ));
    if !date.is_empty() {
        meta = meta.push(components::styled_text(
            date,
            palette.role(TextRole::BodySmall),
            palette.on_surface_variant,
        ));
    }

    let mut card = column![].spacing(palette.space.xs).width(Fill);
    card = card.push(meta);
    match &message.body {
        Some(MessageBody::Loading) => {}
        Some(MessageBody::Document(document)) => {
            let rendered = html_view::render_document(document, Message::OpenUrl);
            let mut body = column![].spacing(palette.space.xs).width(Fill);
            for element in rendered {
                body = body.push(element);
            }
            card = card.push(body);
        }
        Some(MessageBody::TooLarge) => {
            card = card.push(components::styled_text(
                "This message is too large to display.",
                palette.role(TextRole::BodySmall),
                palette.on_surface_variant,
            ));
        }
        None => {
            if !message.snippet.trim().is_empty() {
                card = card.push(components::styled_text(
                    message.snippet.clone(),
                    palette.role(TextRole::BodySmall),
                    palette.on_surface_variant,
                ));
            }
            card = card.push(components::styled_text(
                "Not downloaded",
                palette.role(TextRole::LabelSmall),
                palette.on_surface_variant,
            ));
        }
    }

    container(card)
        .width(Fill)
        .padding(palette.space.sm)
        .style(move |_| container::Style {
            background: Some(palette.surface_container.into()),
            border: iced::border::rounded(palette.radius.sm),
            ..Default::default()
        })
        .into()
}

/// Unix seconds, UTC, for the earlier-message date labels.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_body(body: Option<MessageBody>) -> MessageSummary {
        MessageSummary {
            id: pige_core::MessageId("message".into()),
            sender: "Ada".into(),
            to: "me".into(),
            date: 0,
            snippet: String::new(),
            flags: pige_core::mail::Flags::default(),
            has_attachments: false,
            attachments: Vec::new(),
            body,
        }
    }

    #[test]
    fn body_kind_distinguishes_loading_from_missing() {
        assert_eq!(
            reader_body_kind(&with_body(Some(MessageBody::Loading))),
            ReaderBody::Loading
        );
        assert_eq!(reader_body_kind(&with_body(None)), ReaderBody::Missing);
        assert_eq!(
            reader_body_kind(&with_body(Some(MessageBody::TooLarge))),
            ReaderBody::TooLarge
        );
    }
}
