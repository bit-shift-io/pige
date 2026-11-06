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
/// The input must contain at least one plausible RFC 5322 header line; empty,
/// whitespace-only, and colon-less blocks return [`ParseError::Malformed`].
/// A blank line is appended so a block that ends at the last header parses as
/// a message with an empty body. [`mail_parser::MessageParser::parse_headers`]
/// is used because callers supply headers only.
///
/// `size` is always `0` (unknown) because a header block does not carry the
/// message size; callers with `RFC822.SIZE` set [`MessageHeader::size`] before
/// caching.
pub fn parse_header_block(bytes: &[u8]) -> Result<MessageHeader, ParseError> {
    if !has_plausible_header(bytes) {
        return Err(ParseError::Malformed);
    }

    let mut raw = Vec::with_capacity(bytes.len() + 2);
    raw.extend_from_slice(bytes);
    if !raw.ends_with(b"\n") {
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(b"\r\n");

    let message = MessageParser::new()
        .parse_headers(&raw)
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
        size: 0,
    })
}

/// Maximum number of characters kept in a list preview.
const SNIPPET_CHARS: usize = 160;
/// Bytes requested from the parser before the preview is normalized. Larger
/// than [`SNIPPET_CHARS`] so encoded words and HTML markup have room to decode.
const SNIPPET_PARSE_BYTES: usize = 1024;

/// Build a readable one-line preview from a raw message.
///
/// `bytes` may be a message prefix rather than a complete message, as fetched
/// by the IMAP snippet request. The parser decodes transfer encodings and
/// charsets; HTML-only mail is reduced to its visible text. Runs of whitespace
/// collapse to a single space and the result is capped at [`SNIPPET_CHARS`].
/// Returns `None` when the message has no readable body text.
pub fn snippet_from_message(bytes: &[u8]) -> Option<String> {
    let message = MessageParser::new().parse(bytes)?;
    let preview = message.body_preview(SNIPPET_PARSE_BYTES)?;
    let text = preview.split_whitespace().collect::<Vec<_>>().join(" ");
    let text: String = text.chars().take(SNIPPET_CHARS).collect();
    (!text.trim().is_empty()).then_some(text)
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

/// Whether the block has at least one line shaped like `field-name: value`.
///
/// This rejects empty, whitespace-only, and colon-less input before handing it
/// to `mail-parser`, whose success otherwise means only "some bytes were
/// present". Folded continuation lines are skipped, and a blank line ends the
/// header block.
fn has_plausible_header(bytes: &[u8]) -> bool {
    for line in bytes.split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        if matches!(line.first(), Some(b' ' | b'\t')) {
            continue;
        }
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            continue;
        };
        let name = &line[..colon];
        if !name.is_empty() && name.iter().all(|&byte| is_field_name_byte(byte)) {
            return true;
        }
    }
    false
}

/// RFC 5322 `ftext`: printable US-ASCII other than `:`.
fn is_field_name_byte(byte: u8) -> bool {
    matches!(byte, 33..=57 | 59..=126)
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

/// Strip one leading lowercase list marker such as `[list]` or `[pige-dev]`.
///
/// Mixed-case or uppercase tags such as `[PATCH]`, `[ANN]`, and `[URGENT]`
/// are deliberately preserved: treating every bracketed prefix as a list tag
/// would collapse distinct announcement or urgent threads.
fn strip_list_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('[')?;
    let close = rest.find(']')?;
    let tag = &rest[..close];
    let recognizable = !tag.is_empty()
        && tag.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || byte == b'-'
                || byte == b'_'
                || byte == b'.'
        });
    recognizable.then(|| &rest[close + 1..])
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
        assert_eq!(header.size, 0);
    }

    #[test]
    fn parse_header_rejects_empty_whitespace_and_colonless_garbage() {
        assert_eq!(parse_header_block(b""), Err(ParseError::Malformed));
        assert_eq!(
            parse_header_block(b" \r\n\t\r\n"),
            Err(ParseError::Malformed)
        );
        assert_eq!(
            parse_header_block(b"this is not a header\r\nnor is this\r\n"),
            Err(ParseError::Malformed)
        );
    }

    #[test]
    fn parse_header_uses_zero_for_missing_or_invalid_dates() {
        let missing = parse_header_block(b"Subject: No date\r\n\r\n").expect("header");
        assert_eq!(missing.date, 0);

        let invalid =
            parse_header_block(b"Subject: Bad date\r\nDate: not a date\r\n\r\n").expect("header");
        assert_eq!(invalid.date, 0);
    }

    #[test]
    fn parse_header_replaces_invalid_utf8_and_keeps_bad_encoded_words() {
        let invalid_utf8 = parse_header_block(b"Subject: caf\xe9\r\n\r\n").expect("header");
        assert_eq!(invalid_utf8.subject, "caf\u{fffd}");

        assert_eq!(
            decode_rfc2047("=?UTF-8?B?not-base64?="),
            "=?UTF-8?B?not-base64?="
        );
    }

    #[test]
    fn parse_header_decodes_mixed_plain_and_encoded_subject() {
        let header =
            parse_header_block(b"Subject: Hello =?UTF-8?B?R3LDvMOfZQ==?=\r\n\r\n").expect("header");

        assert_eq!(header.subject, "Hello Grüße");
    }

    #[test]
    fn parse_header_takes_the_last_of_multiple_in_reply_to_ids() {
        let header = parse_header_block(b"In-Reply-To: <one@x> <two@x>\r\n\r\n").expect("header");

        assert_eq!(header.in_reply_to.as_deref(), Some("<two@x>"));
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
        assert_eq!(
            parse_flags(&["Seen", "DRAFT", "flagged", "answered", "deleted"]),
            Flags {
                seen: true,
                answered: true,
                flagged: true,
                deleted: true,
                draft: true,
            }
        );
    }

    #[test]
    fn normalized_subject_strips_only_recognizable_list_tags() {
        assert_eq!(normalized_subject("Re: [list] Hello  World"), "hello world");
        assert_eq!(normalized_subject("[pige-dev] Release 1.0"), "release 1.0");
        assert_eq!(
            normalized_subject("Fwd: Re: [PATCH] Café  Release"),
            "[patch] café release"
        );
        assert_eq!(normalized_subject("[URGENT] Outage"), "[urgent] outage");
        assert_eq!(normalized_subject("[ANN] New release"), "[ann] new release");
        assert_eq!(normalized_subject("   "), "");
    }

    #[test]
    fn snippet_decodes_quoted_printable_and_collapses_whitespace() {
        let raw = b"From: a@example.com\r\nContent-Type: text/plain\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHello=20world=21\r\n\r\nSecond   line";
        assert_eq!(
            snippet_from_message(raw).as_deref(),
            Some("Hello world! Second line")
        );
    }

    #[test]
    fn snippet_decodes_base64_text() {
        let raw = b"From: a@example.com\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\nSGVsbG8gd29ybGQh\r\n";
        assert_eq!(snippet_from_message(raw).as_deref(), Some("Hello world!"));
    }

    #[test]
    fn snippet_extracts_visible_text_from_html_only_mail() {
        let raw = b"From: a@example.com\r\nContent-Type: text/html\r\n\r\n<p>Hello <b>world</b></p><script>bad()</script>";
        assert_eq!(snippet_from_message(raw).as_deref(), Some("Hello world"));
    }

    #[test]
    fn snippet_is_none_without_a_body() {
        let raw = b"From: a@example.com\r\nSubject: Empty\r\n\r\n";
        assert_eq!(snippet_from_message(raw), None);
    }

    #[test]
    fn snippet_caps_long_previews() {
        let body = "word ".repeat(200);
        let raw = format!("From: a@example.com\r\nContent-Type: text/plain\r\n\r\n{body}");
        let snippet = snippet_from_message(raw.as_bytes()).expect("snippet");
        assert!(snippet.chars().count() <= 160);
        assert!(snippet.starts_with("word word"));
    }
}
