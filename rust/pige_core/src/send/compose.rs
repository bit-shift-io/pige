//! Reply/forward helpers and raw MIME generation.
//!
//! v1 sends plain text only. The same bytes are used for SMTP submission and
//! the best-effort IMAP APPEND to Sent, so the stored copy is byte-identical to
//! what went over the wire.

use std::str::FromStr;

use lettre::Message;
use lettre::message::Mailbox;
use lettre::message::header::{ContentType, MIME_VERSION_1_0};

use crate::account::Account;
use crate::mail::{Address, MessageHeader, sender_display};

use super::OutgoingMessage;

/// A failure while turning a compose form into an RFC 5322 message.
#[derive(Debug, thiserror::Error)]
pub enum ComposeError {
    #[error("invalid address {address:?}: {reason}")]
    Address { address: String, reason: String },
    #[error("message has no recipients")]
    NoRecipients,
    #[error("could not build message: {0}")]
    Build(String),
}

/// `Re: <subject>`, without stacking a second prefix on an existing reply.
pub fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if has_prefix(trimmed, &["re:"]) {
        trimmed.to_string()
    } else {
        format!("Re: {trimmed}")
    }
}

/// `Fwd: <subject>`, accepting the common `Fw:` spelling as already forwarded.
pub fn forward_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if has_prefix(trimmed, &["fwd:", "fw:"]) {
        trimmed.to_string()
    } else {
        format!("Fwd: {trimmed}")
    }
}

fn has_prefix(value: &str, prefixes: &[&str]) -> bool {
    let lower = value.to_ascii_lowercase();
    prefixes.iter().any(|prefix| lower.starts_with(prefix))
}

/// Quote every line of an earlier message under a standard attribution line.
///
/// The timestamp is deterministic UTC (`YYYY-MM-DD HH:MM UTC`) so tests and
/// stored drafts do not depend on the machine's locale or time zone.
pub fn quote_body(sender: &str, date: i64, body: &str) -> String {
    let sender = sender.trim();
    let sender = if sender.is_empty() {
        "Unknown sender"
    } else {
        sender
    };
    let mut quote = format!("On {}, {sender} wrote:\n", format_quote_date(date));

    let mut any = false;
    for line in body.lines() {
        quote.push_str("> ");
        quote.push_str(line);
        quote.push('\n');
        any = true;
    }
    if !any {
        quote.push_str("> \n");
    }
    // Callers append the quote after their own text; drop the final newline so
    // an empty compose body gets one blank separator, not two.
    if quote.ends_with('\n') {
        quote.pop();
    }
    quote
}

fn format_quote_date(date: i64) -> String {
    let days = date.max(0).div_euclid(86_400);
    let seconds = date.max(0).rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        seconds / 3_600,
        (seconds % 3_600) / 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

/// Build a reply to the newest message in a conversation. The sender is the
/// recipient unless the message came from this account, in which case the
/// original recipients are used so replying to one's own Sent copy still works.
pub fn reply_message(account: &Account, header: &MessageHeader, body: &str) -> OutgoingMessage {
    let self_email = account.email.trim().to_ascii_lowercase();
    let mut to = header
        .from
        .iter()
        .filter(|address| address.email.trim().to_ascii_lowercase() != self_email)
        .cloned()
        .collect::<Vec<_>>();
    if to.is_empty() {
        to = header
            .to
            .iter()
            .filter(|address| address.email.trim().to_ascii_lowercase() != self_email)
            .cloned()
            .collect();
    }

    let mut references = header.references.clone();
    if let Some(message_id) = &header.message_id
        && !references.iter().any(|reference| reference == message_id)
    {
        references.push(message_id.clone());
    }

    OutgoingMessage {
        account_id: account.id.clone(),
        to,
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: reply_subject(&header.subject),
        body: format!(
            "\n\n{}",
            quote_body(&sender_display(&header.from), header.date, body)
        ),
        in_reply_to: header.message_id.clone(),
        references,
    }
}

/// Build a forward: no recipient is guessed, the subject is prefixed once, and
/// the original text is quoted beneath a blank line for the user to add context.
pub fn forward_message(account: &Account, header: &MessageHeader, body: &str) -> OutgoingMessage {
    OutgoingMessage {
        account_id: account.id.clone(),
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: forward_subject(&header.subject),
        body: format!(
            "\n\n{}",
            quote_body(&sender_display(&header.from), header.date, body)
        ),
        in_reply_to: None,
        references: Vec::new(),
    }
}

/// Render a compose form into RFC 5322 bytes with UTF-8 plain text, encoded
/// headers, a Date and Message-ID, and reply headers when present.
///
/// Bcc recipients are added to the SMTP envelope but deliberately *not* to the
/// formatted headers: the same bytes go to every recipient, so a Bcc header
/// would disclose the blind recipients to To/Cc.
pub fn build_mime(from: &Address, message: &OutgoingMessage) -> Result<Vec<u8>, ComposeError> {
    let recipient_count = message.to.len() + message.cc.len() + message.bcc.len();
    if recipient_count == 0 {
        return Err(ComposeError::NoRecipients);
    }

    let from_mailbox = mailbox(from)?;
    let domain = from
        .email
        .rsplit_once('@')
        .map(|(_, domain)| domain)
        .filter(|domain| !domain.is_empty())
        .unwrap_or("localhost");

    let mut builder = Message::builder()
        .from(from_mailbox)
        .date_now()
        .message_id(Some(format!("<{}@{domain}>", uuid::Uuid::new_v4())))
        .header(MIME_VERSION_1_0)
        .header(ContentType::TEXT_PLAIN)
        .subject(message.subject.clone());

    for address in &message.to {
        builder = builder.to(mailbox(address)?);
    }
    for address in &message.cc {
        builder = builder.cc(mailbox(address)?);
    }
    for address in &message.bcc {
        builder = builder.bcc(mailbox(address)?);
    }

    if let Some(in_reply_to) = &message.in_reply_to {
        builder = builder.in_reply_to(in_reply_to.clone());
    }
    if !message.references.is_empty() {
        builder = builder.references(message.references.join(" "));
    }

    let formatted = builder
        .body(message.body.clone())
        .map_err(|error| ComposeError::Build(error.to_string()))?;
    Ok(normalize_encoded_subject(formatted.formatted()))
}

/// RFC 2047 charset names are case-insensitive, but `=?UTF-8?` is the spelling
/// most clients emit and the one interoperability tests expect. lettre emits
/// lowercase here; normalize only the token so the encoded words stay intact.
fn normalize_encoded_subject(mut raw: Vec<u8>) -> Vec<u8> {
    const PREFIX: &[u8] = b"Subject: =?";
    let mut offset = 0;
    while let Some(found) = raw[offset..]
        .windows(PREFIX.len())
        .position(|window| window.eq_ignore_ascii_case(PREFIX))
    {
        let start = offset + found + b"Subject: =?".len();
        let end = raw[start..]
            .iter()
            .position(|byte| *byte == b'?')
            .map(|length| start + length);
        if let Some(end) = end
            && raw[start..end].eq_ignore_ascii_case(b"utf-8")
        {
            raw[start..end].copy_from_slice(b"UTF-8");
            break;
        }
        offset = start;
    }
    raw
}

fn mailbox(address: &Address) -> Result<Mailbox, ComposeError> {
    let email =
        lettre::Address::from_str(address.email.trim()).map_err(|error| ComposeError::Address {
            address: address.email.clone(),
            reason: error.to_string(),
        })?;
    let name = address
        .name
        .as_ref()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    Ok(Mailbox::new(name, email))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_of_empty_body_still_has_a_quoted_line() {
        let quote = quote_body("Ada", 0, "");
        assert!(quote.ends_with("> "));
        assert!(quote.contains("Ada wrote:"));
    }

    #[test]
    fn quote_keeps_blank_lines_quoted() {
        let quote = quote_body("Ada", 0, "one\n\ntwo");
        assert!(quote.contains("> one\n> \n> two"));
    }

    #[test]
    fn prefix_checks_ignore_case_and_whitespace() {
        assert_eq!(reply_subject("  re: hello  "), "re: hello");
        assert_eq!(forward_subject("Fw: hello"), "Fw: hello");
        assert_eq!(forward_subject("hello"), "Fwd: hello");
    }
}
