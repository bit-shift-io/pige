//! The reader pane: subject, sender summary, message body, and an expandable
//! "Earlier messages" section. Remote images stay as blocked placeholders; v1
//! never fetches remote bytes, so there is no "Load images" action.

use iced::widget::{MouseArea, column, container, row, scrollable, space, stack};
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

/// One toolbar / overflow-menu action.
struct ReaderAction {
    icon: char,
    label: &'static str,
    message: Message,
}

impl ReaderAction {
    fn icon_button(self) -> Element<'static, Message> {
        components::icon_button(self.icon, self.label, self.message)
    }

    fn menu_row(self) -> Element<'static, Message> {
        components::menu_row(self.icon, self.label, self.message)
    }
}

/// Render the reader pane for the current selection. `width` is the pane's
/// width in points, used to decide how many actions fit before the overflow.
pub fn reader_pane<'a>(app: &'a App, width: f32) -> Element<'a, Message> {
    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return container(column![
            components::app_bar_actions(None, Vec::new(), None),
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

    let primary = primary_actions(app);
    let fit = fit_action_count(primary.len(), width);
    let visible: Vec<Element<'static, Message>> = primary
        .into_iter()
        .take(fit)
        .map(ReaderAction::icon_button)
        .collect();
    let leading = Some(components::icon_button(
        icons::ARROW_BACK,
        "Back",
        Message::ConversationBack,
    ));
    let overflow = Some(components::icon_button(
        icons::MORE_VERT,
        "More",
        Message::OpenReaderMenu,
    ));
    let bar = components::app_bar_actions(leading, visible, overflow);

    let body = conversation_body(detail, app);
    container(column![bar, body])
        .width(Fill)
        .height(Fill)
        .style(components::pane_style(PaneTone::Surface))
        .into()
}

/// The actions available for the open conversation, most important first, so
/// the ones that do not fit are the least used.
fn primary_actions(app: &App) -> Vec<ReaderAction> {
    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return Vec::new();
    };
    let mut actions = Vec::new();
    let Some(context) = reader_context(app, &detail.account_id) else {
        return actions;
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

    if archive {
        actions.push(ReaderAction {
            icon: icons::ARCHIVE,
            label: "Archive",
            message: Message::ArchiveConversation,
        });
    }
    if trash {
        actions.push(ReaderAction {
            icon: icons::DELETE,
            label: "Delete",
            message: Message::TrashConversation,
        });
    } else if permanent {
        actions.push(ReaderAction {
            icon: icons::DELETE_FOREVER,
            label: "Delete permanently",
            message: Message::OpenDeleteConfirm,
        });
    }
    if read {
        actions.push(ReaderAction {
            icon: icons::MARK_UNREAD,
            label: "Mark unread",
            message: Message::MarkUnread,
        });
    }
    actions.push(ReaderAction {
        icon: icons::REPLY,
        label: "Reply",
        message: Message::Reply,
    });
    actions.push(ReaderAction {
        icon: icons::FORWARD,
        label: "Forward",
        message: Message::Forward,
    });
    actions
}

/// The overflow-only actions, after the primary ones that did not fit. Follows
/// Gmail's reader menu.
fn menu_extras(app: &App) -> Vec<ReaderAction> {
    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return Vec::new();
    };
    let mut extras = Vec::new();
    if move_targets(app, &detail.account_id) {
        extras.push(ReaderAction {
            icon: icons::LABEL,
            label: "Move to",
            message: Message::OpenMovePicker,
        });
    }
    if let Some(context) = reader_context(app, &detail.account_id)
        && context.has_junk
        && context.current_kind != FolderKind::Junk
    {
        extras.push(ReaderAction {
            icon: icons::REPORT,
            label: "Report spam",
            message: Message::ReportSpam,
        });
    }
    if detail
        .messages
        .first()
        .is_some_and(|message| !message.sender_email.trim().is_empty())
    {
        extras.push(ReaderAction {
            icon: icons::BLOCK,
            label: "Block sender",
            message: Message::BlockSender,
        });
    }
    extras.push(ReaderAction {
        icon: icons::PRINT,
        label: "Print",
        message: Message::PrintConversation,
    });
    extras
}

/// Whether the account has any folder other than the current one to move into.
fn move_targets(app: &App, account_id: &AccountId) -> bool {
    let Some(section) = app
        .snapshot
        .sections
        .iter()
        .find(|section| &section.account_id == account_id)
    else {
        return false;
    };
    let current = app.snapshot.selected_folder.as_ref();
    section
        .folders
        .iter()
        .any(|folder| current != Some(&folder.id))
}

/// The reader's overflow dropdown: a scrim that closes it plus a card anchored
/// under the app bar. Recomputes the split so it matches the visible bar.
pub fn reader_menu<'a>(app: &'a App, width: f32) -> Element<'a, Message> {
    let palette = theme::active();
    let primary = primary_actions(app);
    let fit = fit_action_count(primary.len(), width);

    let mut rows: Vec<Element<'static, Message>> = primary
        .into_iter()
        .skip(fit)
        .map(ReaderAction::menu_row)
        .collect();
    let extras = menu_extras(app);
    if !rows.is_empty() && !extras.is_empty() {
        rows.push(menu_divider());
    }
    rows.extend(extras.into_iter().map(ReaderAction::menu_row));

    let card = container(column(rows).spacing(0).width(Fill))
        .width(palette.metrics.menu_width)
        .padding(palette.space.xs)
        .style(move |_| container::Style {
            background: Some(palette.surface_container_high.into()),
            text_color: Some(palette.on_surface),
            border: iced::border::rounded(palette.radius.md),
            shadow: iced::Shadow {
                color: palette.shadow,
                offset: iced::Vector::new(0.0, palette.metrics.dialog_shadow_y),
                blur_radius: palette.metrics.dialog_shadow_blur,
            },
            ..Default::default()
        });

    let positioned = container(card)
        .width(Fill)
        .height(Fill)
        .align_x(Alignment::End)
        .align_y(Alignment::Start)
        .padding(iced::Padding {
            top: palette.metrics.app_bar_height + palette.space.xs,
            right: palette.space.md,
            bottom: 0.0,
            left: 0.0,
        });

    let scrim = MouseArea::new(
        container(space())
            .width(Fill)
            .height(Fill)
            .style(components::scrim_style()),
    )
    .on_press(Message::CloseReaderMenu);

    stack![scrim, positioned].into()
}

/// A 1-point full-width menu separator.
fn menu_divider<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    container(space())
        .width(Fill)
        .height(palette.metrics.border_width)
        .style(move |_| container::Style {
            background: Some(palette.outline_variant.into()),
            ..Default::default()
        })
        .into()
}

/// How many icon actions fit in a bar `width` points wide, reserving room for
/// the back button, the overflow button and the bar's padding.
fn fit_action_count(total: usize, width: f32) -> usize {
    let palette = theme::active();
    if !width.is_finite() || width <= 0.0 {
        return 0;
    }
    let reserved = palette.metrics.touch_target * 2.0 + palette.space.xs * 2.0;
    let usable = (width - reserved).max(0.0);
    let per_action = palette.metrics.touch_target + palette.space.xs;
    (((usable / per_action).floor()) as usize).min(total)
}

/// The capabilities and special-folder availability for the open conversation.
struct ReaderContext {
    capabilities: Capabilities,
    current_kind: FolderKind,
    has_archive: bool,
    has_trash: bool,
    has_junk: bool,
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
        has_junk: section
            .folders
            .iter()
            .any(|folder| folder.kind == FolderKind::Junk),
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
            sender_email: "ada@example.com".into(),
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
    fn the_toolbar_shows_what_fits_and_overflows_the_rest() {
        // touch target 48, x.s 4: reserved 104, per action 52.
        assert_eq!(fit_action_count(5, 600.0), 5);
        assert_eq!(fit_action_count(5, 300.0), 3);
        assert_eq!(fit_action_count(5, 200.0), 1);
        assert_eq!(fit_action_count(5, 100.0), 0);
        assert_eq!(fit_action_count(0, 600.0), 0);
        assert_eq!(fit_action_count(5, f32::NAN), 0);
    }

    fn menu_app(junk: bool, current: &str, sender_email: &str) -> App {
        use pige_core::account::AccountId;
        use pige_core::api::{AccountSection, ConversationDetail, FolderSummary};
        use pige_core::mail::{FolderId, MessageId, ThreadId};

        let mut app = App::for_test();
        let account = AccountId("acc".into());
        let folder = |id: &str, name: &str, kind: FolderKind| FolderSummary {
            id: FolderId(id.into()),
            name: name.into(),
            kind,
            unread: 0,
        };
        let mut folders = vec![
            folder("inbox", "Inbox", FolderKind::Inbox),
            folder("sent", "Sent", FolderKind::Sent),
        ];
        if junk {
            folders.push(folder("junk", "Spam", FolderKind::Junk));
        }
        app.snapshot.sections = vec![AccountSection {
            account_id: account.clone(),
            label: "A".into(),
            email: "a@example.com".into(),
            unread: 0,
            capabilities: Default::default(),
            folders,
        }];
        app.snapshot.selected_folder = Some(FolderId(current.into()));
        app.snapshot.selected_conversation = Some(ConversationDetail {
            thread_id: ThreadId("t".into()),
            account_id: account,
            subject: "s".into(),
            messages: vec![MessageSummary {
                id: MessageId("m".into()),
                sender: "Bob".into(),
                sender_email: sender_email.into(),
                to: "me".into(),
                date: 0,
                snippet: String::new(),
                flags: pige_core::mail::Flags::default(),
                has_attachments: false,
                attachments: Vec::new(),
                body: None,
            }],
        });
        app
    }

    fn extra_labels(app: &App) -> Vec<&'static str> {
        menu_extras(app)
            .into_iter()
            .map(|action| action.label)
            .collect()
    }

    #[test]
    fn overflow_menu_follows_the_folder_and_sender_context() {
        assert_eq!(
            extra_labels(&menu_app(true, "inbox", "bob@example.com")),
            vec!["Move to", "Report spam", "Block sender", "Print"]
        );
        // No spam folder: no Report spam.
        assert_eq!(
            extra_labels(&menu_app(false, "inbox", "bob@example.com")),
            vec!["Move to", "Block sender", "Print"]
        );
        // Already in Spam: Report spam hidden.
        assert_eq!(
            extra_labels(&menu_app(true, "junk", "bob@example.com")),
            vec!["Move to", "Block sender", "Print"]
        );
        // No sender address: Block hidden.
        assert_eq!(
            extra_labels(&menu_app(true, "inbox", "")),
            vec!["Move to", "Report spam", "Print"]
        );
    }

    #[test]
    fn reader_pane_and_menu_build_for_an_open_conversation() {
        let app = menu_app(true, "inbox", "bob@example.com");
        // Both a comfortable and a cramped pane must build; the cramped one
        // pushes the primary actions into the overflow.
        for width in [600.0, 150.0] {
            let _ = reader_pane(&app, width);
            let _ = reader_menu(&app, width);
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
