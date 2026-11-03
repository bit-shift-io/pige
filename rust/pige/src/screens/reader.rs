//! The reader pane: subject, sender summary, message body, and a "Load images"
//! bar when remote content is blocked. Earlier messages collapse behind a
//! header; the current data model has one message per conversation, so that
//! section is empty until real threading lands.

use iced::widget::{column, container, row, scrollable, space};
use iced::{Alignment, Element, Fill};
use pige_core::api::{ConversationDetail, MessageBody};

use crate::app::App;
use crate::components::{self, ButtonVariant, PaneTone};
use crate::html_view;
use crate::icons;
use crate::message::Message;
use crate::theme::{self, TextRole};
use crate::{avatar_color, initials};

/// Render the reader pane for the current selection.
pub fn reader_pane<'a>(app: &'a App) -> Element<'a, Message> {
    let actions = vec![
        components::icon_button(icons::REPLY, "Reply", Message::Reply),
        components::icon_button(icons::FORWARD, "Forward", Message::Forward),
        components::icon_button(icons::MORE_VERT, "More", Message::Noop),
    ];

    let Some(detail) = app.snapshot.selected_conversation.as_ref() else {
        return container(column![
            components::app_bar("Conversation", None, actions),
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
        components::app_bar("Conversation", None, actions),
        body,
    ])
    .width(Fill)
    .height(Fill)
    .style(components::pane_style(PaneTone::Surface))
    .into()
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

    // Body, or a clear state when it is unavailable.
    match &latest.body {
        Some(MessageBody::Document(document)) => {
            if document.has_remote_images() {
                content = content.push(load_images_bar());
            }
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
        Some(MessageBody::TooLarge) => {
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
        None => {
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

    // Attachment chips placeholder.
    if latest.has_attachments {
        content = content.push(attachments_placeholder());
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

/// The bar shown when a message has blocked remote images.
fn load_images_bar<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    let content = row![
        components::styled_text(
            "This message has blocked remote images.".to_string(),
            palette.role(TextRole::BodySmall),
            palette.on_surface_variant,
        ),
        space().width(Fill),
        components::button("Load images", ButtonVariant::Text, Message::LoadImages),
    ]
    .spacing(palette.space.sm)
    .align_y(Alignment::Center);

    container(content)
        .width(Fill)
        .padding([palette.space.xs, palette.space.lg])
        .style(move |_| container::Style {
            background: Some(palette.secondary_container.into()),
            border: iced::border::rounded(palette.radius.sm),
            ..Default::default()
        })
        .into()
}

/// A placeholder row of attachment chips (real attachments land in Task 15).
fn attachments_placeholder<'a>() -> Element<'a, Message> {
    let palette = theme::active();
    container(components::attachment_chip("Attachment", 0, Message::Noop))
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
            let name = if earlier.sender.trim().is_empty() {
                "Unknown sender".to_string()
            } else {
                earlier.sender.clone()
            };
            section = section.push(
                container(components::styled_text(
                    name,
                    palette.role(TextRole::BodyMedium),
                    palette.on_surface_variant,
                ))
                .width(Fill)
                .padding(palette.space.xs),
            );
        }
    }

    container(section)
        .width(Fill)
        .padding([palette.space.sm, palette.space.lg])
        .into()
}

/// Whether the "Load images" bar should show for a body.
pub fn has_blocked_images(body: &Option<MessageBody>) -> bool {
    matches!(body, Some(MessageBody::Document(document)) if document.has_remote_images())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pige_core::api::MessageSummary;
    use pige_core::html::parse_html;
    use pige_core::mail::{Flags, MessageId};

    fn summary_with_remote_image() -> MessageSummary {
        MessageSummary {
            id: MessageId("m1".into()),
            sender: "Ada".into(),
            to: String::new(),
            date: 0,
            snippet: String::new(),
            flags: Flags::default(),
            has_attachments: false,
            body: Some(MessageBody::Document(parse_html(
                "<img src='https://tracker.example/p.gif'>",
                false,
            ))),
        }
    }

    #[test]
    fn blocked_images_are_detected_only_on_documents() {
        assert!(has_blocked_images(&summary_with_remote_image().body));

        let mut plain = summary_with_remote_image();
        plain.body = Some(MessageBody::Document(parse_html("<p>no images</p>", false)));
        assert!(!has_blocked_images(&plain.body));

        assert!(!has_blocked_images(&Some(MessageBody::TooLarge)));
        assert!(!has_blocked_images(&None));
    }
}
