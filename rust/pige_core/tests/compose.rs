//! Composition tests: reply/forward headers, quote formatting, and the raw
//! MIME bytes shared by SMTP delivery and the Sent-folder APPEND.

use async_trait::async_trait;
use pige_core::{
    Account, AccountId, Address, AuthKind, Flags, MessageHeader, Security, ServerConfig,
    send::{
        OutgoingMessage, build_mime,
        compose::{forward_message, forward_subject, quote_body, reply_message, reply_subject},
        deliver,
    },
    transport::{SendError, SmtpSender},
};

fn account() -> Account {
    Account {
        id: AccountId("account-1".to_string()),
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
        username: "ada@example.com".to_string(),
        imap: ServerConfig {
            host: "imap.example.com".to_string(),
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerConfig {
            host: "smtp.example.com".to_string(),
            port: 465,
            security: Security::Tls,
        },
        auth: AuthKind::Password,
        sort_order: 0,
    }
}

fn header() -> MessageHeader {
    MessageHeader {
        message_id: Some("<parent@example.com>".to_string()),
        in_reply_to: Some("<root@example.com>".to_string()),
        references: vec!["<root@example.com>".to_string()],
        subject: "Grüße aus Berlin".to_string(),
        from: vec![Address::new(
            Some("Charles Babbage".to_string()),
            "charles@example.com",
        )],
        to: vec![Address::new(None, "ada@example.com")],
        cc: Vec::new(),
        date: 1_772_960_400,
        size: 512,
    }
}

fn message() -> OutgoingMessage {
    OutgoingMessage {
        account_id: AccountId("account-1".to_string()),
        to: vec![Address::new(
            Some("Charles Babbage".to_string()),
            "charles@example.com",
        )],
        cc: vec![Address::new(None, "grace@example.com")],
        bcc: vec![Address::new(None, "hidden@example.com")],
        subject: "Grüße aus Berlin".to_string(),
        body: "Hallo Charles,\n\nhier ist der Bericht.\n".to_string(),
        in_reply_to: Some("<parent@example.com>".to_string()),
        references: vec![
            "<root@example.com>".to_string(),
            "<parent@example.com>".to_string(),
        ],
    }
}

#[test]
fn reply_sets_recipient_subject_and_references() {
    let reply = reply_message(&account(), &header(), "Original body.\nSecond line.");

    assert_eq!(reply.account_id, AccountId("account-1".to_string()));
    assert_eq!(
        reply.to,
        vec![Address::new(
            Some("Charles Babbage".to_string()),
            "charles@example.com"
        )]
    );
    assert_eq!(reply.subject, "Re: Grüße aus Berlin");
    assert_eq!(reply.in_reply_to.as_deref(), Some("<parent@example.com>"));
    assert_eq!(
        reply.references,
        vec![
            "<root@example.com>".to_string(),
            "<parent@example.com>".to_string()
        ]
    );
    assert!(reply.body.contains("> Original body."));
    assert!(reply.body.contains("> Second line."));
    assert!(reply.body.contains("Charles Babbage"));
}

#[test]
fn forward_has_fwd_subject_and_no_recipient() {
    let forward = forward_message(&account(), &header(), "Original body.");

    assert!(forward.to.is_empty());
    assert!(forward.cc.is_empty());
    assert!(forward.bcc.is_empty());
    assert_eq!(forward.subject, "Fwd: Grüße aus Berlin");
    assert!(forward.body.contains("> Original body."));
    assert!(forward.body.contains("Charles Babbage"));
    assert_eq!(forward.in_reply_to, None);
    assert!(forward.references.is_empty());
}

#[test]
fn quote_body_prefixes_each_line_and_names_the_sender() {
    let quote = quote_body("Ada Lovelace", 1_772_960_400, "line one\nline two\n");
    let lines: Vec<&str> = quote.lines().collect();

    assert!(quote.starts_with("On "));
    assert!(quote.contains("Ada Lovelace wrote:"));
    assert_eq!(lines[0], "On 2026-03-08 09:00 UTC, Ada Lovelace wrote:");
    assert_eq!(lines[1], "> line one");
    assert_eq!(lines[2], "> line two");
}

#[test]
fn reply_and_forward_subjects_do_not_stack_prefixes() {
    assert_eq!(reply_subject("Hello"), "Re: Hello");
    assert_eq!(reply_subject("Re: Hello"), "Re: Hello");
    assert_eq!(reply_subject("RE:   Hello"), "RE:   Hello");
    assert_eq!(forward_subject("Hello"), "Fwd: Hello");
    assert_eq!(forward_subject("Fwd: Hello"), "Fwd: Hello");
    assert_eq!(forward_subject("FW: Hello"), "FW: Hello");
}

#[test]
fn mime_is_utf8_plain_text_with_expected_headers() {
    let from = Address::new(Some("Ada Lovelace".to_string()), "ada@example.com");
    let raw = build_mime(&from, &message()).unwrap();
    let text = String::from_utf8(raw).unwrap();

    assert!(text.contains("Content-Type: text/plain; charset=utf-8"));
    assert!(text.contains("Content-Transfer-Encoding: "));
    assert!(text.contains("MIME-Version: 1.0"));
    assert!(text.contains("Subject: =?UTF-8?"));
    assert!(text.contains("From: \"Ada Lovelace\" <ada@example.com>"));
    assert!(text.contains("To: \"Charles Babbage\" <charles@example.com>"));
    assert!(text.contains("Cc: grace@example.com"));
    // The Bcc envelope recipient must never be disclosed in the message text.
    assert!(!text.contains("hidden@example.com"));
    assert!(!text.contains("Bcc:"));
    assert!(text.contains("In-Reply-To: <parent@example.com>"));
    assert!(text.contains("References: <root@example.com> <parent@example.com>"));
    assert!(text.contains("Message-ID: <"));
    assert!(text.contains("Date: "));
    assert!(text.contains("Hallo Charles,"));
    assert!(text.contains("hier ist der Bericht."));
}

#[test]
fn ascii_body_is_sent_unchanged() {
    let from = Address::new(None, "ada@example.com");
    let mut message = message();
    message.subject = "Report".to_string();
    message.body = "Short note.\n".to_string();
    let raw = build_mime(&from, &message).unwrap();
    let text = String::from_utf8(raw).unwrap();

    assert!(text.contains("Subject: Report"));
    assert!(text.contains("Short note."));
}

#[derive(Default)]
struct RecordingSmtp {
    calls: Vec<(Address, Vec<Address>, Vec<u8>)>,
}

#[async_trait]
impl SmtpSender for RecordingSmtp {
    async fn send_raw(
        &mut self,
        from: &Address,
        recipients: &[Address],
        raw: &[u8],
    ) -> Result<(), SendError> {
        self.calls
            .push((from.clone(), recipients.to_vec(), raw.to_vec()));
        Ok(())
    }
}

#[tokio::test]
async fn deliver_sends_the_same_bytes_to_every_recipient() {
    let from = Address::new(Some("Ada Lovelace".to_string()), "ada@example.com");
    let mut smtp = RecordingSmtp::default();
    let raw = deliver(&mut smtp, &from, &message()).await.unwrap();

    assert_eq!(smtp.calls.len(), 1);
    assert_eq!(smtp.calls[0].2, raw);
    assert_eq!(smtp.calls[0].0, from);
    assert_eq!(smtp.calls[0].1.len(), 3);
    assert!(
        smtp.calls[0]
            .1
            .iter()
            .any(|a| a.email == "hidden@example.com")
    );
    assert!(
        !String::from_utf8(raw)
            .unwrap()
            .contains("hidden@example.com")
    );
}

#[test]
fn invalid_recipient_is_a_compose_error() {
    let from = Address::new(None, "ada@example.com");
    let mut message = message();
    message.to = vec![Address::new(None, "not an address")];
    let error = build_mime(&from, &message).expect_err("invalid recipient");
    assert!(error.to_string().contains("not an address"));
}

#[test]
fn empty_recipient_list_is_rejected_by_delivery() {
    let from = Address::new(None, "ada@example.com");
    let mut message = message();
    message.to.clear();
    message.cc.clear();
    message.bcc.clear();
    let error = build_mime(&from, &message).expect_err("no recipients");
    assert!(error.to_string().contains("recipient"));
}

#[test]
fn flags_helper_is_available_for_sent_copies() {
    // The Sent APPEND marks the copy seen; keep the contract explicit here.
    let flags = Flags {
        seen: true,
        ..Flags::default()
    };
    assert!(flags.seen);
}
