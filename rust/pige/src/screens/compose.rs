//! Compose, reply, and forward.
//!
//! The screen is pure state and layout; sending, saving, and discarding all run
//! through `Task::perform` in `update`. v1 is deliberately plain text only.

use iced::widget::{column, container, row, text_editor};
use iced::{Alignment, Element, Fill, Length};
use pige_core::account::AccountId;
use pige_core::api::{DeliveryPreview, RecipientSuggestion};
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

impl ComposeField {
    /// Whether this field holds recipient addresses and therefore offers
    /// autocomplete.
    pub fn is_recipient(self) -> bool {
        matches!(self, Self::To | Self::Cc | Self::Bcc)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::To => "To",
            Self::Cc => "Cc",
            Self::Bcc => "Bcc",
            Self::Subject => "Subject",
        }
    }
}

/// Identity of one autocomplete request.
///
/// The generation changes on every keystroke, caret move, acceptance, or
/// dismissal. A response is only allowed to touch the compose form when all of
/// these fields still describe the active field: a slow answer for another
/// account, field, token, or generation is dropped instead of replacing a
/// completed address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientRequestKey {
    pub generation: u64,
    pub account_id: AccountId,
    pub field: ComposeField,
    /// UTF-8 byte offset of the first byte of the active token.
    pub token_start: usize,
    /// UTF-8 byte offset one past the last byte of the active token.
    pub token_end: usize,
    pub query: String,
}

/// The suggestions currently offered for one request and the highlighted row.
#[derive(Debug, Clone)]
pub struct RecipientSuggestionList {
    pub key: RecipientRequestKey,
    pub items: Vec<RecipientSuggestion>,
    pub selected: usize,
}

/// A prefill for the compose screen: a new reply/forward (no id) or an existing
/// draft being edited (id set).
#[derive(Debug, Clone)]
pub struct ComposeSeed {
    pub id: Option<OutboxId>,
    pub message: OutgoingMessage,
}

/// The recipient-level delivery classification shown beside the fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum DeliveryState {
    #[default]
    Unknown,
    Checking,
    Ready(DeliveryPreview),
    /// The provider refused the policy (missing key, unverified key, lookup
    /// failure). The message is actionable and sending stays blocked.
    Blocked(String),
}

/// One renderable delivery line. `encrypted` is false for ordinary external
/// recipients so the UI can never paint an encryption badge for cleartext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryNotice {
    pub message: String,
    pub encrypted: bool,
    pub warning: bool,
}

/// Describe the current delivery classification for the compose form.
pub fn delivery_notice(state: &DeliveryState) -> Option<DeliveryNotice> {
    match state {
        DeliveryState::Unknown => None,
        DeliveryState::Checking => Some(DeliveryNotice {
            message: "Checking how each recipient will receive this message…".to_string(),
            encrypted: false,
            warning: false,
        }),
        DeliveryState::Blocked(error) => Some(DeliveryNotice {
            message: format!("Cannot send yet: {error}"),
            encrypted: false,
            warning: true,
        }),
        DeliveryState::Ready(preview) => {
            if preview.recipients.is_empty() {
                return None;
            }
            let ordinary: Vec<&str> = preview
                .ordinary_external()
                .into_iter()
                .map(|address| address.email.as_str())
                .collect();
            if !ordinary.is_empty() {
                let encrypted = preview
                    .recipients
                    .iter()
                    .filter(|recipient| recipient.class.is_encrypted())
                    .count();
                return Some(DeliveryNotice {
                    message: if encrypted > 0 {
                        format!(
                            "Encrypted for {} recipient(s); {} will receive ordinary external mail, not end-to-end encrypted: {}.",
                            encrypted,
                            ordinary.len(),
                            ordinary.join(", ")
                        )
                    } else {
                        format!(
                            "Ordinary external delivery: this message is not end-to-end encrypted for {}.",
                            ordinary.join(", ")
                        )
                    },
                    encrypted: false,
                    warning: true,
                });
            }
            Some(DeliveryNotice {
                message: format!(
                    "End-to-end encrypted delivery for {} recipient(s).",
                    preview.recipients.len()
                ),
                encrypted: true,
                warning: false,
            })
        }
    }
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
    /// The recipient field that most recently held the caret. `None` while a
    /// non-recipient field (Subject/body) is active.
    pub recipient_field: Option<ComposeField>,
    /// The active recipient caret as a UTF-8 byte offset.
    pub recipient_caret: usize,
    /// Monotonic identity for autocomplete requests; never persisted.
    pub recipient_generation: u64,
    /// Results for the current request. Keyboard, touch and rendering all read
    /// this same list.
    pub recipient_suggestions: Option<RecipientSuggestionList>,
    /// Set only by an explicit `accept_suggestion`. The update loop records it
    /// through the core API; typing and merely showing results never do.
    pub pending_selection: Option<Address>,
    /// Latest recipient-delivery classification. It is advisory presentation
    /// only; the provider re-resolves policy before delivery.
    pub delivery: DeliveryState,
    /// Monotonic identity for debounced delivery previews.
    pub delivery_generation: u64,
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
            recipient_field: Some(ComposeField::To),
            recipient_caret: 0,
            recipient_generation: 0,
            recipient_suggestions: None,
            pending_selection: None,
            delivery: DeliveryState::Unknown,
            delivery_generation: 0,
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
            recipient_field: Some(ComposeField::To),
            recipient_caret: 0,
            recipient_generation: 0,
            recipient_suggestions: None,
            pending_selection: None,
            delivery: DeliveryState::Unknown,
            delivery_generation: 0,
        }
    }

    pub fn apply_field(&mut self, field: ComposeField, value: String) {
        match field {
            ComposeField::To => self.to = value,
            ComposeField::Cc => self.cc = value,
            ComposeField::Bcc => self.bcc = value,
            ComposeField::Subject => self.subject = value,
        }
        if field.is_recipient() {
            self.recipient_field = Some(field);
            let length = self.field_value(field).len();
            self.recipient_caret = floor_char_boundary(self.field_value(field), length);
        } else {
            self.recipient_field = None;
        }
        // Any edit invalidates results for the previous text. The caret event
        // that follows the input event schedules the replacement request.
        self.invalidate_recipient_suggestions();
        if field.is_recipient() {
            self.delivery = DeliveryState::Unknown;
            self.delivery_generation = self.delivery_generation.wrapping_add(1);
        }
        self.notice = None;
    }

    /// Start one debounced delivery-preview request. Returns the generation and
    /// message when the recipient fields parse; an incomplete address leaves the
    /// preview hidden until the next edit.
    pub fn begin_delivery_check(&mut self) -> Option<(u64, OutgoingMessage)> {
        self.delivery_generation = self.delivery_generation.wrapping_add(1);
        let generation = self.delivery_generation;
        match self.outgoing() {
            Ok(message) if !message.recipients().is_empty() => {
                self.delivery = DeliveryState::Checking;
                Some((generation, message))
            }
            _ => {
                self.delivery = DeliveryState::Unknown;
                None
            }
        }
    }

    /// Apply a preview only if no later edit superseded it.
    pub fn apply_delivery(
        &mut self,
        generation: u64,
        result: Result<DeliveryPreview, String>,
    ) -> bool {
        if generation != self.delivery_generation {
            return false;
        }
        self.delivery = match result {
            Ok(preview) => DeliveryState::Ready(preview),
            Err(error) => DeliveryState::Blocked(error),
        };
        true
    }

    /// The policy error that must keep Send disabled, if any.
    pub fn blocked_delivery(&self) -> Option<&str> {
        match &self.delivery {
            DeliveryState::Blocked(error) => Some(error.as_str()),
            _ => None,
        }
    }

    fn field_value(&self, field: ComposeField) -> &str {
        match field {
            ComposeField::To => &self.to,
            ComposeField::Cc => &self.cc,
            ComposeField::Bcc => &self.bcc,
            ComposeField::Subject => &self.subject,
        }
    }

    fn set_field_value(&mut self, field: ComposeField, value: String) {
        match field {
            ComposeField::To => self.to = value,
            ComposeField::Cc => self.cc = value,
            ComposeField::Bcc => self.bcc = value,
            ComposeField::Subject => self.subject = value,
        }
    }

    /// Update the recipient caret reported by the scoped text field wrapper.
    /// The offset is always snapped down to a UTF-8 character boundary so no
    /// later token slice can panic.
    pub fn set_recipient_caret(&mut self, field: ComposeField, caret: usize) {
        if !field.is_recipient() {
            self.recipient_field = None;
            return;
        }
        self.recipient_field = Some(field);
        self.recipient_caret = floor_char_boundary(self.field_value(field), caret);
    }

    /// Drop any current results and start a new request generation. The
    /// returned key is `None` when the caret is not inside a usable token, in
    /// which case no debounce or API work should be scheduled.
    pub fn begin_recipient_query(&mut self) -> Option<RecipientRequestKey> {
        self.recipient_generation = self.recipient_generation.wrapping_add(1);
        self.recipient_suggestions = None;
        self.recipient_request(self.recipient_caret)
    }

    fn invalidate_recipient_suggestions(&mut self) {
        self.recipient_generation = self.recipient_generation.wrapping_add(1);
        self.recipient_suggestions = None;
    }

    /// Describe the token around `caret` for the active recipient field. The
    /// token boundary is the next unquoted/unbracketed comma or semicolon, so
    /// a completed recipient never participates in the replacement range.
    pub fn recipient_request(&self, caret: usize) -> Option<RecipientRequestKey> {
        let field = self.recipient_field?;
        if !field.is_recipient() {
            return None;
        }
        let text = self.field_value(field);
        let caret = floor_char_boundary(text, caret);
        let (token_start, token_end) = active_recipient_token(text, caret)?;
        let query = text[token_start..token_end].trim().to_string();
        if !query_valid(&query) {
            return None;
        }
        Some(RecipientRequestKey {
            generation: self.recipient_generation,
            account_id: self.account_id.clone(),
            field,
            token_start,
            token_end,
            query,
        })
    }

    /// Whether a response carries the still-active request identity.
    pub fn is_current_recipient_request(&self, key: &RecipientRequestKey) -> bool {
        key.generation == self.recipient_generation
            && key.account_id == self.account_id
            && self.recipient_field == Some(key.field)
            && self.recipient_key_matches_text(key)
    }

    /// The key's byte span must still exist in the active field and still hold
    /// exactly the query that produced it. This rejects a result after the text
    /// changed even if a caller did not advance the generation explicitly.
    fn recipient_key_matches_text(&self, key: &RecipientRequestKey) -> bool {
        let text = self.field_value(key.field);
        let span_intact = key.token_start <= key.token_end
            && key.token_end <= text.len()
            && text.is_char_boundary(key.token_start)
            && text.is_char_boundary(key.token_end)
            && text[key.token_start..key.token_end] == key.query;
        // The stored span must still be the whole active token. Comparing only
        // the query bytes misses drift that extends the token (`gra` -> `grac`),
        // so re-scan from the token end and require identical boundaries.
        span_intact
            && active_recipient_token(text, key.token_end) == Some((key.token_start, key.token_end))
    }

    /// Accept a local result page. Returns `false` for any stale or empty
    /// request so callers can ignore late responses safely.
    pub fn apply_suggestions(
        &mut self,
        key: RecipientRequestKey,
        suggestions: Vec<RecipientSuggestion>,
    ) -> bool {
        if !self.is_current_recipient_request(&key) {
            return false;
        }
        self.recipient_suggestions = Some(RecipientSuggestionList {
            key,
            items: suggestions,
            selected: 0,
        });
        true
    }

    /// The key of the visible suggestion list, if it has any rows.
    pub fn active_suggestion_key(&self) -> Option<&RecipientRequestKey> {
        let list = self.recipient_suggestions.as_ref()?;
        if list.items.is_empty() || !self.is_current_recipient_request(&list.key) {
            return None;
        }
        Some(&list.key)
    }

    pub fn selected_suggestion_index(&self) -> Option<usize> {
        let list = self.recipient_suggestions.as_ref()?;
        if list.items.is_empty() {
            return None;
        }
        Some(list.selected.min(list.items.len() - 1))
    }

    /// Highlight a row from an arrow key; indexes are clamped so wrap-around
    /// behavior stays in the screen rather than the widget.
    pub fn highlight_suggestion(&mut self, key: &RecipientRequestKey, index: usize) -> bool {
        if !self.is_current_recipient_request(key) {
            return false;
        }
        let Some(list) = self.recipient_suggestions.as_mut() else {
            return false;
        };
        if &list.key != key || list.items.is_empty() {
            return false;
        }
        list.selected = index.min(list.items.len() - 1);
        true
    }

    /// Escape/Back: hide the current list without inserting or recording
    /// anything. Bumping the generation also rejects any in-flight response.
    pub fn dismiss_suggestions(&mut self, key: &RecipientRequestKey) -> bool {
        let matches = self
            .recipient_suggestions
            .as_ref()
            .is_some_and(|list| &list.key == key)
            && self.is_current_recipient_request(key);
        if !matches {
            return false;
        }
        self.invalidate_recipient_suggestions();
        true
    }

    /// Focus moved away from every recipient field (e.g. into the message
    /// body). Drop the list and mark that no recipient field is active so a
    /// stale panel neither lingers nor intercepts later navigation. The
    /// generation bump rejects in-flight responses too.
    pub fn recipient_focus_lost(&mut self) {
        self.recipient_field = None;
        self.invalidate_recipient_suggestions();
    }

    /// Resolve a choice message against the list that is current *now*.
    /// Touch supplies the tapped row; keyboard Enter supplies `None` and gets
    /// the currently highlighted row, so an ArrowDown and Enter batched from
    /// one view do not accept the stale row the view was built with.
    pub fn choose_suggestion(
        &mut self,
        key: &RecipientRequestKey,
        index: Option<usize>,
    ) -> Result<Address, String> {
        let index = index
            .or_else(|| self.selected_suggestion_index())
            .ok_or_else(|| "No recipient suggestions are available.".to_string())?;
        self.accept_suggestion(key, index)
    }

    /// Replace only the active token with the explicitly chosen address. No
    /// completed recipient and no other field is touched. The address is left
    /// in `pending_selection` for the update loop to record exactly once.
    pub fn accept_suggestion(
        &mut self,
        key: &RecipientRequestKey,
        index: usize,
    ) -> Result<Address, String> {
        if !self.is_current_recipient_request(key) {
            return Err("This recipient suggestion is no longer current.".to_string());
        }
        let list = self
            .recipient_suggestions
            .as_ref()
            .filter(|list| &list.key == key)
            .ok_or_else(|| "No recipient suggestions are available.".to_string())?;
        let selected = list
            .items
            .get(index)
            .cloned()
            .ok_or_else(|| "That recipient suggestion is no longer available.".to_string())?;

        let field = key.field;
        let text = self.field_value(field).to_string();
        let start = floor_char_boundary(&text, key.token_start);
        let end = floor_char_boundary(&text, key.token_end);
        if start > end || end > text.len() {
            return Err("The recipient token is no longer valid.".to_string());
        }

        let replacement = selected.address.to_header_string();
        let rest = &text[end..];
        // At the end of the field, leave a separator ready for the next
        // recipient. In the middle, the existing separator is already there.
        let (rest, separator) = if rest.trim().is_empty() {
            ("", ", ")
        } else {
            (rest, "")
        };
        let updated = format!("{}{replacement}{separator}{rest}", &text[..start]);
        self.set_field_value(field, updated);
        self.recipient_field = Some(field);
        self.recipient_caret = start + replacement.len() + separator.len();
        self.recipient_generation = self.recipient_generation.wrapping_add(1);
        self.recipient_suggestions = None;
        self.pending_selection = Some(selected.address.clone());
        Ok(selected.address)
    }

    /// Take the address that an explicit acceptance asked to record.
    pub fn take_pending_selection(&mut self) -> Option<Address> {
        self.pending_selection.take()
    }

    /// Whether the form holds no user content worth keeping. Used when closing
    /// the surface: an empty form closes immediately, a non-empty one is saved
    /// as a draft first.
    pub fn is_empty(&self) -> bool {
        self.to.trim().is_empty()
            && self.cc.trim().is_empty()
            && self.bcc.trim().is_empty()
            && self.subject.trim().is_empty()
            && self.body.text().trim().is_empty()
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

/// Snap a byte offset down to the nearest UTF-8 character boundary. The caret
/// adapter converts grapheme positions from Iced into byte offsets, but a
/// stale or platform-supplied index must never make a token slice panic.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// The trimmed span of the comma/semicolon-separated recipient segment that
/// contains `caret`. Quoted names and angle-bracket addresses may contain
/// separators and are kept whole, exactly like `split_addresses`.
fn active_recipient_token(text: &str, caret: usize) -> Option<(usize, usize)> {
    let caret = floor_char_boundary(text, caret);
    let mut segment_start = 0usize;
    let mut quoted = false;
    let mut angled = false;
    let mut escaped = false;

    let token_in_segment = |start: usize, end: usize| -> Option<(usize, usize)> {
        let segment = &text[start..end];
        let leading = segment.len() - segment.trim_start().len();
        let token_start = start + leading;
        let token_end = start + segment.trim_end().len();
        if token_start >= token_end {
            return None;
        }
        // A caret in the leading whitespace after a separator is not inside a
        // token; a caret after the trimmed text (including a typed trailing
        // space) still belongs to the token so name queries stay useful.
        if caret < token_start || caret > end {
            return None;
        }
        Some((token_start, token_end))
    };

    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angled = true,
            '>' if !quoted => angled = false,
            ',' | ';' if !quoted && !angled => {
                if let Some(span) = token_in_segment(segment_start, index) {
                    return Some(span);
                }
                segment_start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    token_in_segment(segment_start, text.len())
}

/// Empty, whitespace-only, and control-character tokens cannot produce a
/// useful local lookup.
fn query_valid(query: &str) -> bool {
    !query.is_empty()
        && !query.chars().any(char::is_control)
        && query.chars().any(|character| !character.is_whitespace())
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
        components::scroller(form(
            compose,
            Length::Fixed(palette.metrics.compose_body_height)
        )),
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
    let mut fields = column![].spacing(palette.space.md).width(Fill);
    for field in [ComposeField::To, ComposeField::Cc, ComposeField::Bcc] {
        fields = fields.push(recipient_field_with_suggestions(compose, field));
        if let Some(panel) = suggestion_panel_for(compose, field) {
            fields = fields.push(panel);
        }
    }
    if let Some(notice) = delivery_notice(&compose.delivery) {
        fields = fields.push(components::styled_text(
            notice.message,
            palette.role(TextRole::BodyMedium),
            if notice.warning {
                palette.error
            } else if notice.encrypted {
                palette.primary
            } else {
                palette.on_surface_variant
            },
        ));
    }
    fields = fields
        .push(components::text_field(
            "Subject",
            &compose.subject,
            |value| Message::ComposeField(ComposeField::Subject, value),
        ))
        .push(components::text_area(
            "Message",
            &compose.body,
            body_height,
            Message::ComposeBody,
        ));

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

fn recipient_field_with_suggestions<'a>(
    compose: &'a ComposeState,
    field: ComposeField,
) -> Element<'a, Message> {
    let active = compose
        .recipient_suggestions
        .as_ref()
        .filter(|list| {
            list.key.field == field
                && !list.items.is_empty()
                && compose.is_current_recipient_request(&list.key)
        })
        .map(|list| {
            let selected = list.selected.min(list.items.len() - 1);
            components::SuggestionNavigation {
                previous: Message::RecipientSuggestionMoved {
                    key: list.key.clone(),
                    index: selected.saturating_sub(1),
                },
                next: Message::RecipientSuggestionMoved {
                    key: list.key.clone(),
                    index: (selected + 1).min(list.items.len() - 1),
                },
                // Enter resolves the highlighted row in `update`, so arrows
                // batched with Enter still choose the row the user sees.
                choose: Message::RecipientSuggestionChosen {
                    key: list.key.clone(),
                    index: None,
                },
                dismiss: Message::RecipientSuggestionDismissed(list.key.clone()),
            }
        });

    components::recipient_field(
        field.label(),
        compose.field_value(field),
        move |value| Message::ComposeField(field, value),
        move |caret| Message::ComposeCaret(field, caret),
        active,
    )
}

fn suggestion_panel_for<'a>(
    compose: &'a ComposeState,
    field: ComposeField,
) -> Option<Element<'a, Message>> {
    let list = compose.recipient_suggestions.as_ref().filter(|list| {
        list.key.field == field
            && !list.items.is_empty()
            && compose.is_current_recipient_request(&list.key)
    })?;
    let key = list.key.clone();
    Some(components::suggestion_panel(
        &list.items,
        list.selected,
        &compose.account_id,
        move |index| Message::RecipientSuggestionChosen {
            key: key.clone(),
            index: Some(index),
        },
    ))
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
    use pige_core::api::{RecipientSuggestion, SuggestionSource};

    #[test]
    fn external_ordinary_warning_is_not_encryption_badge() {
        use pige_core::api::DeliveryClass;
        use pige_core::mail::Address;

        let preview = pige_core::api::DeliveryPreview {
            recipients: vec![
                pige_core::api::DeliveryClassification {
                    address: Address::new(None, "friend@example.com"),
                    class: DeliveryClass::ExternalOrdinary,
                },
                pige_core::api::DeliveryClassification {
                    address: Address::new(None, "colleague@proton.me"),
                    class: DeliveryClass::EndToEndEncrypted,
                },
            ],
        };
        let state = DeliveryState::Ready(preview);
        let notice = delivery_notice(&state).expect("a mixed classification is shown");
        assert!(
            !notice.encrypted,
            "an ordinary external recipient must never get an encryption badge"
        );
        assert!(
            notice.message.to_ascii_lowercase().contains("not")
                && notice.message.to_ascii_lowercase().contains("encrypt"),
            "the warning must say the ordinary copy is not encrypted: {}",
            notice.message
        );
        assert!(
            notice.message.contains("friend@example.com"),
            "the warning names the ordinary recipient: {}",
            notice.message
        );

        let encrypted = DeliveryState::Ready(pige_core::api::DeliveryPreview {
            recipients: vec![pige_core::api::DeliveryClassification {
                address: Address::new(None, "colleague@proton.me"),
                class: DeliveryClass::EndToEndEncrypted,
            }],
        });
        let notice = delivery_notice(&encrypted).expect("encrypted classification");
        assert!(notice.encrypted);
        assert!(
            !notice
                .message
                .to_ascii_lowercase()
                .contains("not encrypted")
        );
    }

    fn suggestion(name: &str, email: &str) -> RecipientSuggestion {
        RecipientSuggestion {
            address: Address::new(Some(name.to_string()), email),
            sources: vec![SuggestionSource::SavedContact],
            accounts: Vec::new(),
        }
    }

    fn compose_with(field: ComposeField, text: &str) -> (ComposeState, RecipientRequestKey) {
        let mut compose = ComposeState::new(AccountId("a".into()));
        compose.apply_field(field, text.to_string());
        compose.set_recipient_caret(field, text.len());
        let key = compose
            .begin_recipient_query()
            .expect("an active recipient token");
        (compose, key)
    }

    #[test]
    fn autocomplete_replaces_only_active_token() {
        let (mut compose, key) =
            compose_with(ComposeField::To, "\"Lovelace, Ada\" <ada@example.com>, gra");
        assert_eq!(key.query, "gra");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));

        let accepted = compose.accept_suggestion(&key, 0).expect("accept");

        assert_eq!(accepted.email, "grace@example.com");
        assert!(
            compose.to.starts_with(
                "\"Lovelace, Ada\" <ada@example.com>, \"Grace Hopper\" <grace@example.com>"
            ),
            "completed recipient must survive: {}",
            compose.to
        );
        let addresses = parse_addresses(&compose.to).expect("valid recipients");
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].email, "ada@example.com");
        assert_eq!(addresses[1].email, "grace@example.com");
    }

    #[test]
    fn quoted_comma_name_keeps_completed_recipient() {
        let completed = "\"Lovelace, Ada\" <ada@example.com>";
        let (mut compose, key) = compose_with(ComposeField::To, &format!("{completed}; gra"));
        assert_eq!(key.query, "gra");

        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        compose.accept_suggestion(&key, 0).expect("accept");

        assert!(
            compose.to.starts_with(completed),
            "the quoted comma must not split the completed recipient: {}",
            compose.to
        );
        let addresses = parse_addresses(&compose.to).expect("valid recipients");
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].name.as_deref(), Some("Lovelace, Ada"));
        assert_eq!(addresses[1].email, "grace@example.com");
    }

    #[test]
    fn stale_field_account_or_generation_is_rejected() {
        let suggestions = vec![suggestion("Grace Hopper", "grace@example.com")];

        let (mut stale_field, field_key) = compose_with(ComposeField::To, "gr");
        stale_field.set_recipient_caret(ComposeField::Cc, 0);
        assert!(!stale_field.apply_suggestions(field_key.clone(), suggestions.clone()));
        assert!(stale_field.accept_suggestion(&field_key, 0).is_err());

        let (mut stale_account, account_key) = compose_with(ComposeField::To, "gr");
        stale_account.account_id = AccountId("b".into());
        assert!(!stale_account.apply_suggestions(account_key.clone(), suggestions.clone()));
        assert!(stale_account.accept_suggestion(&account_key, 0).is_err());

        let (mut stale_generation, generation_key) = compose_with(ComposeField::To, "gr");
        stale_generation.recipient_generation =
            stale_generation.recipient_generation.wrapping_add(1);
        assert!(!stale_generation.apply_suggestions(generation_key.clone(), suggestions));
        assert!(
            stale_generation
                .accept_suggestion(&generation_key, 0)
                .is_err()
        );
    }

    #[test]
    fn unicode_caret_never_slices_invalid_boundary() {
        let text = "\"Zoë\" <zoe@example.com>, 日本語";
        let mut compose = ComposeState::new(AccountId("a".into()));
        compose.apply_field(ComposeField::To, text.to_string());
        compose.set_recipient_caret(ComposeField::To, text.len());

        let inside_character = text.find('本').expect("multibyte token") + 1;
        let key = compose
            .recipient_request(inside_character)
            .expect("the whole grapheme token is active");

        assert_eq!(key.query, "日本語");
        assert!(text.is_char_boundary(key.token_start));
        assert!(text.is_char_boundary(key.token_end));
        assert_eq!(&text[key.token_start..key.token_end], "日本語");

        // No caret position, including ones inside a multibyte character or
        // beyond either end, may panic or yield a non-boundary span.
        for caret in 0..=text.len() + 3 {
            if let Some(key) = compose.recipient_request(caret) {
                assert!(text.is_char_boundary(key.token_start));
                assert!(text.is_char_boundary(key.token_end));
                let _ = &text[key.token_start..key.token_end];
            }
        }
    }

    #[test]
    fn bcc_suggestions_do_not_modify_to_or_cc() {
        let mut compose = ComposeState::new(AccountId("a".into()));
        compose.apply_field(ComposeField::To, "keep@example.com".into());
        compose.apply_field(ComposeField::Cc, "cc@example.com".into());
        compose.apply_field(ComposeField::Bcc, "gr".into());
        compose.set_recipient_caret(ComposeField::Bcc, 2);
        let key = compose
            .begin_recipient_query()
            .expect("bcc recipient token");

        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        compose.accept_suggestion(&key, 0).expect("accept");

        assert_eq!(compose.to, "keep@example.com");
        assert_eq!(compose.cc, "cc@example.com");
        assert!(
            compose
                .bcc
                .starts_with("\"Grace Hopper\" <grace@example.com>"),
            "bcc was not updated: {}",
            compose.bcc
        );
    }

    #[test]
    fn escape_dismisses_without_insertion() {
        let (mut compose, key) = compose_with(ComposeField::To, "gra");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        let before = compose.to.clone();

        assert!(compose.dismiss_suggestions(&key));

        assert_eq!(compose.to, before, "dismissal must not insert anything");
        assert!(compose.active_suggestion_key().is_none());
        assert!(
            !compose.apply_suggestions(key, vec![suggestion("Grace Hopper", "grace@example.com")]),
            "an in-flight result after dismissal is stale"
        );
    }

    #[test]
    fn arrows_and_enter_match_touch_choice() {
        let options = vec![
            suggestion("Ada Lovelace", "ada@example.com"),
            suggestion("Grace Hopper", "grace@example.com"),
            suggestion("Katherine Johnson", "katherine@example.com"),
        ];

        let (mut keyboard, keyboard_key) = compose_with(ComposeField::To, "a");
        assert!(keyboard.apply_suggestions(keyboard_key.clone(), options.clone()));
        assert_eq!(keyboard.selected_suggestion_index(), Some(0));
        assert!(keyboard.highlight_suggestion(&keyboard_key, 1));
        assert_eq!(keyboard.selected_suggestion_index(), Some(1));
        let keyboard_index = keyboard
            .selected_suggestion_index()
            .expect("highlighted row");
        let keyboard_address = keyboard
            .accept_suggestion(&keyboard_key, keyboard_index)
            .expect("keyboard accept");

        let (mut touch, touch_key) = compose_with(ComposeField::To, "a");
        assert!(touch.apply_suggestions(touch_key.clone(), options));
        let touch_address = touch
            .accept_suggestion(&touch_key, 1)
            .expect("touch accept");

        assert_eq!(keyboard_address, touch_address);
        assert_eq!(keyboard.to, touch.to);
    }

    #[test]
    fn typing_alone_does_not_record_selection() {
        let (mut compose, key) = compose_with(ComposeField::To, "gr");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        assert!(
            compose.pending_selection.is_none(),
            "receiving suggestions must never record a selection"
        );

        compose.apply_field(ComposeField::To, "gra".into());
        assert!(
            compose.pending_selection.is_none(),
            "typing must never record a selection"
        );

        compose.set_recipient_caret(ComposeField::To, 3);
        let key = compose
            .begin_recipient_query()
            .expect("recipient token after typing");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        compose.accept_suggestion(&key, 0).expect("explicit accept");

        assert_eq!(
            compose
                .take_pending_selection()
                .expect("explicit acceptance is recordable")
                .email,
            "grace@example.com"
        );
        assert!(compose.take_pending_selection().is_none());
    }

    #[test]
    fn tapping_a_suggestion_survives_pointer_events_outside_the_field() {
        use crate::components::recipient_suggestions::should_publish_caret;

        let (mut compose, key) = compose_with(ComposeField::To, "gra");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));

        // Iced delivers the press/release that lands on the suggestion row to
        // the field wrapper too. Because the pointer is outside the field's
        // bounds, the wrapper must not publish a caret (which would call
        // `begin_recipient_query` and invalidate the row's captured key).
        assert!(!should_publish_caret(true, false));

        let accepted = compose
            .accept_suggestion(&key, 0)
            .expect("the tapped row still matches the active request");
        assert_eq!(accepted.email, "grace@example.com");
        assert_eq!(compose.recipient_generation, key.generation.wrapping_add(1));
    }

    #[test]
    fn token_text_drift_is_rejected_without_a_generation_bump() {
        let (mut compose, key) = compose_with(ComposeField::To, "gra");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));

        // A paste/edit that changes the token must be rejected even if the
        // caller forgot to advance the generation; staleness also checks the
        // byte span's current text.
        compose.set_field_value(ComposeField::To, "grac".to_string());
        assert_eq!(compose.recipient_generation, key.generation);
        assert!(!compose.is_current_recipient_request(&key));
        assert!(!compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        assert!(compose.accept_suggestion(&key, 0).is_err());
    }

    #[test]
    fn focus_loss_dismisses_a_visible_list() {
        let (mut compose, key) = compose_with(ComposeField::To, "gra");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![suggestion("Grace Hopper", "grace@example.com")]
        ));
        assert!(compose.active_suggestion_key().is_some());

        compose.recipient_focus_lost();

        assert!(compose.active_suggestion_key().is_none());
        assert_eq!(compose.recipient_field, None);
        assert!(compose.accept_suggestion(&key, 0).is_err());
    }

    #[test]
    fn keyboard_choice_uses_the_highlighted_row() {
        let (mut compose, key) = compose_with(ComposeField::To, "a");
        assert!(compose.apply_suggestions(
            key.clone(),
            vec![
                suggestion("Ada Lovelace", "ada@example.com"),
                suggestion("Grace Hopper", "grace@example.com"),
            ]
        ));
        // ArrowDown and Enter can arrive in the same batch. The keyboard
        // message carries `None`, so the choice resolves the current highlight
        // after the move was applied instead of a stale view-time index.
        assert!(compose.highlight_suggestion(&key, 1));
        let accepted = compose
            .choose_suggestion(&key, None)
            .expect("keyboard accept");
        assert_eq!(accepted.email, "grace@example.com");
    }

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
    fn closing_a_compose_form_keeps_non_empty_text_to_save() {
        let mut compose = ComposeState::new(AccountId("a".into()));
        assert!(compose.is_empty(), "a fresh form closes immediately");

        compose.body = text_editor::Content::with_text("hello");
        assert!(!compose.is_empty(), "typed text must be saved, not dropped");

        compose.body = text_editor::Content::new();
        compose.subject = "   ".into();
        assert!(compose.is_empty(), "whitespace alone is still empty");
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
