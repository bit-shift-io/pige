//! Header-block parsing and subject normalization.
//!
//! The IMAP transport fetches raw RFC 5322 header blocks without bodies; these
//! helpers turn them (and IMAP flag strings) into the normalized domain types
//! consumed by the store, threading, and search.

use mail_parser::{HeaderForm, MessageParser};

use super::{Address, Flags, MessageHeader};

/// A header block that could not be parsed into a message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("could not parse message header block")]
    Malformed,
}

/// Parse a raw RFC 5322 header block (with no body) into a [`MessageHeader`].
///
/// A blank line is appended so a block that ends at the last header parses as
/// a message with an empty body. `size` is the length of the header bytes;
/// callers that know the real message size (for example from `RFC822.SIZE`)
/// replace it before caching.
pub fn parse_header_block(bytes: &[u8]) -> Result<MessageHeader, ParseError> {
    let mut raw = Vec::with_capacity(bytes.len() + 2);
    raw.extend_from_slice(bytes);
    if !raw.ends_with(b"\n") {
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\n");

    let message = MessageParser::new()
        .parse(&raw)
        .ok_or(ParseError::Malformed)?;

    let message_id = message.message_id().map(message_id_with_brackets);
    let in_reply_to = message
        .in_reply_to()
        .and_then(|ids| ids.last().map(message_id_with_brackets));
    let references = message
        .references()
        .map(|ids| ids.iter().map(message_id_with_brackets).collect())
        .unwrap_or_default();

    Ok(MessageHeader {
        message_id,
        in_reply_to,
        references,
        subject: message.subject().unwrap_or_default().to_string(),
        from: addresses(message.from()),
        to: addresses(message.to()),
        cc: addresses(message.cc()),
        date: message.date().map_or(0, |date| date.to_timestamp()),
        size: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
    })
}

/// Decode RFC 2047 encoded words in an unstructured header value.
pub fn decode_rfc2047(input: &str) -> String {
    HeaderForm::Text
        .parse(input.as_bytes())
        .value()
        .as_text()
        .unwrap_or(input)
        .to_string()
}

/// Map IMAP flag strings (`\Seen`, `\Answered`, ...) to [`Flags`].
///
/// Matching is ASCII case-insensitive and ignores the leading backslash so
/// servers that spell system flags without it still work. Unknown keywords,
/// including `\Recent`, are ignored.
pub fn parse_flags(raw: &[&str]) -> Flags {
    let mut flags = Flags::default();
    for flag in raw {
        let flag = flag.trim().trim_start_matches('\\');
        if flag.eq_ignore_ascii_case("seen") {
            flags.seen = true;
        } else if flag.eq_ignore_ascii_case("answered") {
            flags.answered = true;
        } else if flag.eq_ignore_ascii_case("flagged") {
            flags.flagged = true;
        } else if flag.eq_ignore_ascii_case("deleted") {
            flags.deleted = true;
        } else if flag.eq_ignore_ascii_case("draft") {
            flags.draft = true;
        }
    }
    flags
}

/// Normalize a subject for thread matching and search: strip repeated reply
/// and forward prefixes and leading list tags, collapse whitespace, and
/// lowercase.
pub fn normalized_subject(subject: &str) -> String {
    let mut text = subject.trim();

    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = strip_prefix_ascii_case(trimmed, "re:") {
            text = rest;
        } else if let Some(rest) = strip_prefix_ascii_case(trimmed, "fwd:") {
            text = rest;
        } else if let Some(rest) = strip_prefix_ascii_case(trimmed, "fw:") {
            text = rest;
        } else if let Some(rest) = strip_list_tag(trimmed) {
            text = rest;
        } else {
            break;
        }
    }

    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.to_lowercase()
}

fn addresses(list: Option<mail_parser::AddressList<'_>>) -> Vec<Address> {
    list.into_iter()
        .flat_map(|list| list.mailboxes())
        .filter_map(|mailbox| {
            let email = mailbox.address()?;
            let name = mailbox.name().filter(|name| !name.is_empty());
            Some(Address::new(name.map(str::to_string), email.to_string()))
        })
        .collect()
}

/// Mail headers store identifiers with their angle brackets; `mail-parser`
/// returns the bare id, so put them back for consistent threading lookups.
fn message_id_with_brackets(id: &str) -> String {
    let id = id.trim();
    if id.starts_with('<') && id.ends_with('>') {
        id.to_string()
    } else {
        format!("<{id}>")
    }
}

fn strip_prefix_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// Strip one leading `[tag]` list marker, if present.
fn strip_list_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('[')?;
    let close = rest.find(']')?;
    Some(&rest[close + 1..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Flags;

    const ENCODED: &[u8] = include_bytes!("../../tests/fixtures/headers/encoded_subject.eml");
    const MISSING_MESSAGE_ID: &[u8] =
        include_bytes!("../../tests/fixtures/headers/missing_message_id.eml");

    #[test]
    fn parse_header_decodes_encoded_words_and_addresses() {
        let header = parse_header_block(ENCODED).expect("header should parse");

        assert_eq!(header.subject, "Grüße aus Berlin");
        assert_eq!(header.from.len(), 1);
        assert_eq!(header.from[0].email, "ada@example.com");
        assert_eq!(header.from[0].name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(header.to.len(), 2);
        assert_eq!(header.to[0].email, "grace@example.com");
        assert_eq!(header.to[1].email, "jose@example.com");
        assert_eq!(header.to[1].name.as_deref(), Some("José García"));
        assert_eq!(header.cc.len(), 1);
        assert_eq!(header.cc[0].name.as_deref(), Some("Alan Turing"));
        assert_eq!(header.date, 1_700_000_000);
        assert_eq!(header.message_id.as_deref(), Some("<m1@example.com>"));
        assert_eq!(header.in_reply_to.as_deref(), Some("<parent@x>"));
        assert_eq!(header.references, vec!["<a@x>", "<b@x>"]);
    }

    #[test]
    fn parse_header_tolerates_missing_message_id_and_body() {
        let header = parse_header_block(MISSING_MESSAGE_ID).expect("header should parse");

        assert_eq!(header.message_id, None);
        assert!(header.references.is_empty());
        assert!(header.in_reply_to.is_none());
        assert_eq!(header.from[0].email, "noreply@example.com");
        assert_eq!(header.from[0].name, None);
        assert_eq!(header.date, 1_700_000_000);
        assert_eq!(header.size, MISSING_MESSAGE_ID.len() as u32);
    }

    #[test]
    fn parse_header_appends_missing_body_separator() {
        let header = parse_header_block(b"From: ada@example.com\r\nSubject: Bare header")
            .expect("header should parse");

        assert_eq!(header.subject, "Bare header");
        assert_eq!(header.from[0].email, "ada@example.com");
    }

    #[test]
    fn decode_rfc2047_joins_adjacent_encoded_words() {
        assert_eq!(
            decode_rfc2047("=?UTF-8?Q?Gr=C3=BC=C3=9Fe?= =?UTF-8?Q?_aus_Berlin?="),
            "Grüße aus Berlin"
        );
        assert_eq!(decode_rfc2047("plain subject"), "plain subject");
    }

    #[test]
    fn parse_flags_is_case_insensitive_and_ignores_unknown() {
        let flags = parse_flags(&[
            "\\Seen",
            "\\ANSWERED",
            "\\Flagged",
            "\\Deleted",
            "\\Draft",
            "\\Recent",
        ]);

        assert!(flags.seen);
        assert!(flags.answered);
        assert!(flags.flagged);
        assert!(flags.deleted);
        assert!(flags.draft);
        assert_eq!(parse_flags(&[]), Flags::default());
        assert_eq!(parse_flags(&["\\Unknown"]), Flags::default());
    }

    #[test]
    fn normalized_subject_strips_prefixes_and_collapses_space() {
        assert_eq!(normalized_subject("Re: [list] Hello  World"), "hello world");
        assert_eq!(
            normalized_subject("Fwd: Re: [PATCH] Café  Release"),
            "café release"
        );
        assert_eq!(normalized_subject("   "), "");
    }
}
