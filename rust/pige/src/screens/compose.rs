//! Compose, reply, and forward.
//!
//! The screen is pure state and layout; sending, saving, and discarding all run
//! through `Task::perform` in `update`. v1 is deliberately plain text only.

use iced::widget::{column, container, row, scrollable, text_editor};
use iced::{Alignment, Element, Fill, Length};
use pige_core::account::AccountId;
use pige_core::mail::Address;
use pige_core::send::{OutboxId, OutgoingMessage};

use crate::app::App;
use crate::components::{self, ButtonVariant, PaneTone};
use crate::icons;
use crate::message::Message;
use crate::theme::{self, TextRole};

/// Which compose text field changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    To,
    Cc,
    Bcc,
    Subject,
}

/// A prefill for the compose screen: a new reply/forward (no id) or an existing
/// draft being edited (id set).
#[derive(Debug, Clone)]
pub struct ComposeSeed {
    pub id: Option<OutboxId>,
    pub message: OutgoingMessage,
}

/// Everything the compose screen remembers between messages.
pub struct ComposeState {
    /// Set when editing a saved draft, so Save replaces instead of duplicating.
    pub id: Option<OutboxId>,
    pub account_id: AccountId,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub subject: String,
    pub body: text_editor::Content,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub busy: bool,
    /// Inline failure, kept beside the fields so the user can fix it without
    /// losing their text.
    pub notice: Option<String>,
}

impl ComposeState {
    pub fn new(account_id: AccountId) -> Self {
        Self {
            id: None,
            account_id,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            body: text_editor::Content::new(),
            in_reply_to: None,
            references: Vec::new(),
            busy: false,
            notice: None,
        }
    }

    pub fn from_message(message: OutgoingMessage, id: Option<OutboxId>) -> Self {
        Self {
            id,
            account_id: message.account_id,
            to: addresses_to_field(&message.to),
            cc: addresses_to_field(&message.cc),
            bcc: addresses_to_field(&message.bcc),
            subject: message.subject,
            body: text_editor::Content::with_text(&message.body),
            in_reply_to: message.in_reply_to,
            references: message.references,
            busy: false,
            notice: None,
        }
    }

    pub fn apply_field(&mut self, field: ComposeField, value: String) {
        match field {
            ComposeField::To => self.to = value,
            ComposeField::Cc => self.cc = value,
            ComposeField::Bcc => self.bcc = value,
            ComposeField::Subject => self.subject = value,
        }
        self.notice = None;
    }

    /// Validate the recipient fields and turn the form into the core's message.
    /// Empty Cc/Bcc are fine; send-time MIME generation enforces at least one
    /// recipient because drafts may legitimately have none.
    pub fn outgoing(&self) -> Result<OutgoingMessage, String> {
        Ok(OutgoingMessage {
            account_id: self.account_id.clone(),
            to: parse_addresses(&self.to)?,
            cc: parse_addresses(&self.cc)?,
            bcc: parse_addresses(&self.bcc)?,
            subject: self.subject.trim().to_string(),
            body: self.body.text(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
        })
    }

    fn title(&self) -> &'static str {
        if self.id.is_some() {
            "Draft"
        } else if self.in_reply_to.is_some() {
            "Reply"
        } else if self
            .subject
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("fwd:")
        {
            "Forward"
        } else {
            "New message"
        }
    }
}

fn addresses_to_field(addresses: &[Address]) -> String {
    addresses
        .iter()
        .map(Address::to_header_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse comma/semicolon-separated recipient fields. Names may be quoted and
/// contain separators when they sit inside quotes or angle brackets.
pub fn parse_addresses(input: &str) -> Result<Vec<Address>, String> {
    let mut addresses = Vec::new();
    for part in split_addresses(input) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, email) = match (part.rfind('<'), part.ends_with('>')) {
            (Some(start), true) => (
                Some(unescape_name(part[..start].trim().trim_matches('"'))),
                part[start + 1..part.len() - 1].trim(),
            ),
            _ => (None, part),
        };
        let email = email.trim();
        validate_email(email)?;
        let name = name.filter(|name| !name.is_empty());
        addresses.push(Address::new(name, email));
    }
    Ok(addresses)
}

fn split_addresses(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut angled = false;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => {
                current.push(character);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                current.push(character);
            }
            '<' if !quoted => {
                angled = true;
                current.push(character);
            }
            '>' if !quoted => {
                angled = false;
                current.push(character);
            }
            ',' | ';' if !quoted && !angled => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(character),
        }
    }
    parts.push(current);
    parts
}

fn unescape_name(name: &str) -> String {
    name.replace("\\\"", "\"").replace("\\\\", "\\")
}

fn validate_email(email: &str) -> Result<(), String> {
    if email.is_empty() {
        return Err("Recipient email is required".to_string());
    }
    if email.chars().any(char::is_whitespace) {
        return Err(format!("Invalid recipient address: {email}"));
    }
    let Some((local, domain)) = email.split_once('@') else {
        return Err(format!("Invalid recipient address: {email}"));
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return Err(format!("Invalid recipient address: {email}"));
    }
    Ok(())
}

/// The full-screen compact/medium compose surface.
pub fn screen<'a>(_app: &'a App, compose: &'a ComposeState) -> Element<'a, Message> {
    let palette = theme::active();
    let leading = Some(components::icon_button(
        icons::CLOSE,
        "Close compose",
        Message::ComposeClose,
    ));

    let page = column![
        components::app_bar(compose.title(), leading, Vec::new()),
        scrollable(form(
            compose,
            Length::Fixed(palette.metrics.compose_body_height)
        ))
        .width(Fill)
        .height(Fill)
        .style(palette.scrollable()),
        action_bar(compose),
    ]
    .spacing(0)
    .width(Fill)
    .height(Fill);

    container(page)
        .width(Fill)
        .height(Fill)
        .style(components::pane_style(PaneTone::Surface))
        .into()
}

/// The wide-window modal. It shares the dialog scrim and action layout but is
/// wider than a confirmation card so the message is comfortable to edit.
pub fn modal<'a>(_app: &'a App, compose: &'a ComposeState) -> Element<'a, Message> {
    let palette = theme::active();
    components::dialog_sized(
        compose.title(),
        form(compose, Length::Fixed(palette.metrics.compose_body_height)),
        action_buttons(compose),
        palette.metrics.compose_width,
    )
}

fn form<'a>(compose: &'a ComposeState, body_height: Length) -> Element<'a, Message> {
    let palette = theme::active();
    let mut fields = column![
        components::text_field("To", &compose.to, |value| {
            Message::ComposeField(ComposeField::To, value)
        }),
        components::text_field("Cc", &compose.cc, |value| {
            Message::ComposeField(ComposeField::Cc, value)
        }),
        components::text_field("Bcc", &compose.bcc, |value| {
            Message::ComposeField(ComposeField::Bcc, value)
        }),
        components::text_field("Subject", &compose.subject, |value| {
            Message::ComposeField(ComposeField::Subject, value)
        }),
        components::text_area("Message", &compose.body, body_height, Message::ComposeBody),
    ]
    .spacing(palette.space.md)
    .width(Fill);

    if let Some(notice) = &compose.notice {
        fields = fields.push(components::styled_text(
            notice.clone(),
            palette.role(TextRole::BodyMedium),
            palette.error,
        ));
    }
    if compose.busy {
        fields = fields.push(components::styled_text(
            "Working…",
            palette.role(TextRole::BodyMedium),
            palette.on_surface_variant,
        ));
    }
    fields.into()
}

fn action_bar<'a>(compose: &'a ComposeState) -> Element<'a, Message> {
    let palette = theme::active();
    container(
        row![iced::widget::space().width(Fill)]
            .push(row(action_buttons(compose)).spacing(palette.space.sm))
            .align_y(Alignment::Center)
            .width(Fill),
    )
    .width(Fill)
    .padding([palette.space.sm, palette.space.lg])
    .into()
}

fn action_buttons<'a>(compose: &'a ComposeState) -> Vec<Element<'a, Message>> {
    if compose.busy {
        return vec![components::button(
            "Cancel",
            ButtonVariant::Text,
            Message::ComposeClose,
        )];
    }
    vec![
        components::button("Discard", ButtonVariant::Danger, Message::ComposeDiscard),
        components::button(
            "Save draft",
            ButtonVariant::Outlined,
            Message::ComposeSaveDraft,
        ),
        components::button("Send", ButtonVariant::Filled, Message::ComposeSend),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_accepts_names_quotes_and_multiple_recipients() {
        let addresses = parse_addresses(
            "\"Lovelace, Ada\" <ada@example.com>, charles@example.com; grace@example.com",
        )
        .expect("valid recipients");

        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name.as_deref(), Some("Lovelace, Ada"));
        assert_eq!(addresses[0].email, "ada@example.com");
        assert_eq!(addresses[1].email, "charles@example.com");
        assert_eq!(addresses[2].email, "grace@example.com");
    }

    #[test]
    fn invalid_recipients_are_reported_before_sending() {
        assert!(parse_addresses("not an address").is_err());
        assert!(parse_addresses("ada@").is_err());
        assert!(parse_addresses("@example.com").is_err());
        assert!(parse_addresses("").unwrap().is_empty());
    }

    #[test]
    fn ui_validation_is_intentionally_lenient_about_domains() {
        // The core's MIME builder is the final validator. This address passes
        // the form but lettre rejects the consecutive dots, exercising the outbox's
        // terminal-failure path.
        assert!(parse_addresses("ada..b@example.com").is_ok());
    }

    #[test]
    fn compose_round_trips_addresses_and_body() {
        let message = OutgoingMessage {
            account_id: AccountId("a".into()),
            to: vec![Address::new(
                Some("Charles Babbage".into()),
                "charles@example.com",
            )],
            cc: vec![Address::new(None, "grace@example.com")],
            bcc: Vec::new(),
            subject: "Report".into(),
            body: "Line one\nLine two".into(),
            in_reply_to: Some("<parent@example.com>".into()),
            references: vec!["<root@example.com>".into()],
        };

        let state = ComposeState::from_message(message.clone(), None);
        assert_eq!(state.title(), "Reply");
        let outgoing = state.outgoing().expect("valid form");
        assert_eq!(outgoing.to, message.to);
        assert_eq!(outgoing.cc, message.cc);
        assert_eq!(outgoing.subject, message.subject);
        assert_eq!(outgoing.body, message.body);
        assert_eq!(outgoing.in_reply_to, message.in_reply_to);
        assert_eq!(outgoing.references, message.references);
    }

    #[test]
    fn a_saved_draft_is_titled_as_a_draft() {
        let state = ComposeState::from_message(
            OutgoingMessage {
                account_id: AccountId("a".into()),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Notes".into(),
                body: String::new(),
                in_reply_to: None,
                references: Vec::new(),
            },
            Some(OutboxId("draft".into())),
        );
        assert_eq!(state.title(), "Draft");
        assert_eq!(state.id, Some(OutboxId("draft".into())));
    }
}
