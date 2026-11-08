//! IMAP mailbox adapter and header-only history enumeration.
//!
//! Everything here runs against the scripted [`FakeTransport`]; no test touches
//! the network. The adapter is exercised through the provider-neutral
//! [`MailboxService`](pige_core::providers::MailboxService) boundary, so the
//! assertions are about behavior, not protocol internals.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use pige_core::account::{Account, AccountId, AuthKind, MailBackend, Security, ServerConfig};
use pige_core::auth::CredentialSecretRepository;
use pige_core::html::parse_html;
use pige_core::mail::{
    Address, Flags, FolderKind, MessageHeader, Uid, extract_attachments, parse_header_block,
};
use pige_core::platform::CredentialStore;
use pige_core::platform::fake::{FakeCredentials, FakeTransportFactory};
use pige_core::providers::{
    ErrorKind, HistoryRequest, MailboxService, OperationContext, ProviderFactory,
    imap::{ImapProviderFactory, identity},
};
use pige_core::transport::{FakeTransport, FolderState, RemoteFolder, RemoteMessage};

fn account() -> Account {
    Account {
        id: AccountId("acct-imap".to_string()),
        label: "Personal".to_string(),
        email: "ada@example.com".to_string(),
        display_name: "Ada Lovelace".to_string(),
        backend: MailBackend::ImapSmtp {
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
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn context() -> OperationContext {
    OperationContext::with_timeout(account().id, 1, Duration::from_secs(30))
}

/// The adapter with a scripted transport factory and an empty credential store.
fn factory(transports: Arc<FakeTransportFactory>) -> ImapProviderFactory {
    let credentials: Arc<dyn CredentialStore> = Arc::new(FakeCredentials::new());
    ImapProviderFactory::new(
        transports,
        Arc::new(CredentialSecretRepository::new(credentials)),
    )
}

async fn open(
    transports: Arc<FakeTransportFactory>,
) -> pige_core::providers::ProviderSession {
    factory(transports)
        .open(&account(), &context())
        .await
        .expect("open IMAP session")
}

fn folder(name: &str, kind: FolderKind) -> RemoteFolder {
    RemoteFolder {
        name: name.to_string(),
        delimiter: Some("/".to_string()),
        kind,
    }
}

fn state(uidvalidity: u32, uidnext: u32) -> FolderState {
    FolderState {
        uidvalidity,
        uidnext,
        exists: uidnext.saturating_sub(1),
    }
}

fn message(uid: u32, subject: &str) -> RemoteMessage {
    RemoteMessage {
        uid: Uid(uid),
        header: MessageHeader {
            message_id: Some(format!("<{uid}@example.com>")),
            in_reply_to: None,
            references: Vec::new(),
            subject: subject.to_string(),
            from: vec![Address::new(None, "a@example.com")],
            to: Vec::new(),
            cc: Vec::new(),
            reply_to: Vec::new(),
            bcc: Vec::new(),
            date: i64::from(uid),
            size: 64,
        },
        flags: Flags::default(),
        size: 64,
    }
}

/// Drain every history page, newest first, returning the decoded UIDs in order
/// and how many pages were needed.
async fn drain_history(
    service: &mut Box<dyn MailboxService>,
    page_size: u32,
) -> (Vec<u32>, usize) {
    let mut uids = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = service
            .history(
                HistoryRequest {
                    cursor,
                    page_size,
                },
                &context(),
            )
            .await
            .expect("history page");
        for record in &page.messages {
            let coords = identity::decode_message(&record.remote_id).expect("IMAP remote id");
            uids.push(coords.uid.0);
        }
        pages += 1;
        if !page.has_more || pages > 16 {
            break;
        }
        cursor = Some(page.checkpoint.clone());
    }
    (uids, pages)
}

#[tokio::test]
async fn explicit_identity_selects_correct_mailbox() {
    // A mailbox name with a colon, a hierarchy delimiter, an ampersand, and
    // non-ASCII text. Colon-joining any of this would corrupt the identity.
    let mailbox = "Archive: 2024/Ünïcode & Co";
    let raw = b"From: a@example.com\r\nSubject: stored\r\n\r\nhello";

    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder(mailbox, FolderKind::Other)],
            states: HashMap::from([(mailbox.to_string(), state(7, 2))]),
            messages: HashMap::from([(mailbox.to_string(), vec![message(1, "stored")])]),
            bodies: HashMap::from([(
                mailbox.to_string(),
                HashMap::from([(Uid(1), raw.to_vec())]),
            )]),
            ..FakeTransport::default()
        },
    );

    let remote = identity::message_remote_id(mailbox, 7, Uid(1));
    let decoded = identity::decode_message(&remote).expect("id round-trips");
    assert_eq!(decoded.mailbox, mailbox);
    assert_eq!(decoded.uidvalidity, 7);
    assert_eq!(decoded.uid, Uid(1));

    let mut session = open(transports.clone()).await;
    let body = session
        .mailbox_service()
        .expect("mailbox service")
        .body(&remote, &context())
        .await
        .expect("fetch body");
    assert_eq!(body.raw.as_deref(), Some(&raw[..]));

    // The exact mailbox was selected, not a truncated or re-joined name.
    let transport = transports
        .transport(&account.id)
        .await
        .expect("shared transport");
    assert_eq!(transport.select_calls, vec![mailbox.to_string()]);
}

#[tokio::test]
async fn uid_epoch_is_part_of_identity() {
    // The same mailbox and UID under two epochs must never collapse.
    let first = identity::message_remote_id("INBOX", 1, Uid(5));
    let second = identity::message_remote_id("INBOX", 2, Uid(5));
    assert_ne!(first, second, "UIDVALIDITY is part of the identity");
    assert_eq!(identity::decode_message(&first).unwrap().uidvalidity, 1);
    assert_eq!(identity::decode_message(&second).unwrap().uidvalidity, 2);
    assert_eq!(identity::decode_message(&first).unwrap().uid, Uid(5));
    assert_eq!(identity::decode_message(&first).unwrap().mailbox, "INBOX");

    // A real crawl names the same UID differently once the epoch rotates.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    let script = |uidvalidity: u32| FakeTransport {
        folders: vec![folder("INBOX", FolderKind::Inbox)],
        states: HashMap::from([("INBOX".to_string(), state(uidvalidity, 6))]),
        messages: HashMap::from([("INBOX".to_string(), vec![message(5, "hello")])]),
        ..FakeTransport::default()
    };

    transports.add_transport(account.id.clone(), script(1));
    let mut session = open(transports.clone()).await;
    let (uids, _) = drain_history(session.mailbox_service().unwrap(), 10).await;
    assert_eq!(uids, vec![5]);
    let first = identity::message_remote_id("INBOX", 1, Uid(5));

    transports.add_transport(account.id.clone(), script(2));
    let mut session = open(transports.clone()).await;
    let (uids, _) = drain_history(session.mailbox_service().unwrap(), 10).await;
    assert_eq!(uids, vec![5]);
    let second = identity::message_remote_id("INBOX", 2, Uid(5));

    assert_ne!(
        first, second,
        "a rotated epoch must not reuse the old remote id"
    );
}

#[tokio::test]
async fn sparse_uids_do_not_truncate_history() {
    // UIDs 1, 900, 50000: a dense-range assumption would lose the gaps.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 50_001))]),
            messages: HashMap::from([(
                "INBOX".to_string(),
                vec![
                    message(1, "one"),
                    message(900, "nine hundred"),
                    message(50_000, "fifty thousand"),
                ],
            )]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;
    // A page size smaller than the gaps forces several pages; every sparse UID
    // must still be learned, newest first.
    let (uids, pages) = drain_history(session.mailbox_service().unwrap(), 1).await;

    assert_eq!(uids, vec![50_000, 900, 1]);
    assert!(pages <= 5, "paging must terminate, got {pages} pages");
}

#[test]
fn reply_to_and_available_bcc_are_parsed() {
    let raw = b"From: ada@example.com\r\n\
Reply-To: replies@example.com\r\n\
Bcc: hidden@example.com\r\n\
Subject: hi\r\n\r\n";
    let header = parse_header_block(raw).expect("parse header");

    assert_eq!(header.reply_to.len(), 1);
    assert_eq!(header.reply_to[0].email, "replies@example.com");
    assert_eq!(header.bcc.len(), 1);
    assert_eq!(header.bcc[0].email, "hidden@example.com");

    // Received mail normally omits Bcc; it must not be invented.
    let without =
        parse_header_block(b"From: ada@example.com\r\nSubject: hi\r\n\r\n").expect("parse");
    assert!(without.reply_to.is_empty());
    assert!(without.bcc.is_empty());
}

#[tokio::test]
async fn header_history_never_fetches_body() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 50_001))]),
            messages: HashMap::from([(
                "INBOX".to_string(),
                vec![message(1, "one"), message(900, "nine"), message(50_000, "big")],
            )]),
            // A body is scripted; history must not ask for it.
            bodies: HashMap::from([(
                "INBOX".to_string(),
                HashMap::from([(Uid(1), b"body".to_vec())]),
            )]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;
    let (uids, _) = drain_history(session.mailbox_service().unwrap(), 2).await;
    assert_eq!(uids, vec![50_000, 900, 1]);

    let transport = transports
        .transport(&account.id)
        .await
        .expect("shared transport");
    assert!(
        transport.fetch_body_calls.is_empty(),
        "history must never fetch a body"
    );
    assert!(
        transport.fetch_texts_calls.is_empty(),
        "history must never fetch snippet text"
    );
    assert!(
        !transport.fetch_learning_headers_calls.is_empty(),
        "history learns headers"
    );
}

#[tokio::test]
async fn raw_body_uses_existing_sanitizer_pipeline() {
    const MAILBOX: &str = "INBOX";
    const RAW: &[u8] = b"From: a@example.com\r\n\
Subject: hi\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p onclick=\"steal()\">Hello <b>world</b></p><script>alert(1)</script>\r\n\
--b\r\n\
Content-Type: application/octet-stream\r\n\
Content-Disposition: attachment; filename=\"report.bin\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--b--\r\n";

    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder(MAILBOX, FolderKind::Inbox)],
            states: HashMap::from([(MAILBOX.to_string(), state(1, 2))]),
            messages: HashMap::from([(MAILBOX.to_string(), vec![message(1, "hi")])]),
            bodies: HashMap::from([(MAILBOX.to_string(), HashMap::from([(Uid(1), RAW.to_vec())]))]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;
    let body = session
        .mailbox_service()
        .expect("mailbox service")
        .body(&identity::message_remote_id(MAILBOX, 1, Uid(1)), &context())
        .await
        .expect("fetch body");

    // The raw bytes are preserved for callers that need them.
    assert_eq!(body.raw.as_deref(), Some(RAW));

    // Text/HTML come from the same MIME decoding the reader uses.
    let html = body.html.as_deref().expect("html body");
    assert!(html.contains("Hello <b>world</b>"), "html: {html}");

    // Attachment descriptors use the existing extractor.
    let expected = extract_attachments(RAW);
    assert_eq!(body.attachments.len(), 1);
    assert_eq!(body.attachments[0].filename, "report.bin");
    assert_eq!(body.attachments[0].part_id, expected[0].part_id);
    assert_eq!(body.attachments[0].content_type, "application/octet-stream");

    // Routing the HTML through the existing sanitizer drops scripts and event
    // handlers, so the adapter did not bypass the sanitizer pipeline.
    let document = parse_html(html, false);
    assert_eq!(document.visible_text(), "Hello world");
}

#[tokio::test]
async fn history_resume_reselects_the_cursors_mailbox() {
    // A durable cursor that stops mid-mailbox. A fresh session resumes it while
    // a *different* mailbox is selected (as an interleaved body/attachment
    // read would leave it); the resume must re-SELECT the cursor's mailbox
    // before enumerating, or `list_uids` reads the wrong folder.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Other", FolderKind::Other),
            ],
            states: HashMap::from([
                ("INBOX".to_string(), state(7, 4)),
                ("Other".to_string(), state(9, 51)),
            ]),
            messages: HashMap::from([
                (
                    "INBOX".to_string(),
                    vec![message(1, "one"), message(2, "two"), message(3, "three")],
                ),
                ("Other".to_string(), vec![message(50, "other")]),
            ]),
            bodies: HashMap::from([(
                "Other".to_string(),
                HashMap::from([(
                    Uid(50),
                    b"From: a@example.com\r\nSubject: other\r\n\r\nbody".to_vec(),
                )]),
            )]),
            ..FakeTransport::default()
        },
    );

    // First page of an INBOX crawl with page_size 1 leaves a mid-mailbox
    // checkpoint: `before_uid` is set (UID 2 remains unread).
    let mut session = open(transports.clone()).await;
    let first = session
        .mailbox_service()
        .unwrap()
        .history(
            HistoryRequest {
                cursor: None,
                page_size: 1,
            },
            &context(),
        )
        .await
        .expect("first page");
    assert_eq!(first.messages.len(), 1);
    let frontier = identity::decode_message(&first.messages[0].remote_id).expect("IMAP id");
    assert_eq!(frontier.mailbox, "INBOX");
    assert_eq!(frontier.uid, Uid(3));
    assert!(first.has_more, "the crawl continues");
    let cursor = first.checkpoint.clone();

    // A fresh session whose selected mailbox is now something else.
    let mut fresh = open(transports.clone()).await;
    fresh
        .mailbox_service()
        .unwrap()
        .body(
            &identity::message_remote_id("Other", 9, Uid(50)),
            &context(),
        )
        .await
        .expect("read from Other");
    {
        let transport = transports.transport(&account.id).await.expect("transport");
        assert_eq!(transport.current.as_deref(), Some("Other"));
    }

    let resumed = fresh
        .mailbox_service()
        .unwrap()
        .history(
            HistoryRequest {
                cursor: Some(cursor),
                page_size: 1,
            },
            &context(),
        )
        .await
        .expect("resume page");

    assert_eq!(resumed.messages.len(), 1);
    let next = identity::decode_message(&resumed.messages[0].remote_id).expect("IMAP id");
    assert_eq!(
        next.mailbox, "INBOX",
        "a resumed page must read the cursor's mailbox, not the selected one"
    );
    assert_eq!(next.uid, Uid(2), "the next newest INBOX UID");

    let transport = transports.transport(&account.id).await.expect("transport");
    assert_eq!(
        transport.select_calls.last().map(String::as_str),
        Some("INBOX"),
        "the resume re-selected the cursor's mailbox"
    );
}

#[tokio::test]
async fn read_with_stale_uidvalidity_is_not_found() {
    // After an epoch rotation the same UID names a different message. A stale
    // remote id must resolve to NotFound, never to whatever now holds that UID.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    const MAILBOX: &str = "INBOX";
    const RAW: &[u8] = b"From: a@example.com\r\nSubject: hi\r\n\r\nbody";
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder(MAILBOX, FolderKind::Inbox)],
            states: HashMap::from([(MAILBOX.to_string(), state(7, 2))]),
            messages: HashMap::from([(MAILBOX.to_string(), vec![message(1, "hi")])]),
            bodies: HashMap::from([(MAILBOX.to_string(), HashMap::from([(Uid(1), RAW.to_vec())]))]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;

    let stale = identity::message_remote_id(MAILBOX, 99, Uid(1));
    let error = session
        .mailbox_service()
        .unwrap()
        .body(&stale, &context())
        .await
        .expect_err("a stale epoch must not resolve");
    assert_eq!(error.kind, ErrorKind::NotFound);

    // The same UID under the live epoch still reads.
    let live = identity::message_remote_id(MAILBOX, 7, Uid(1));
    let body = session
        .mailbox_service()
        .unwrap()
        .body(&live, &context())
        .await
        .expect("live epoch reads");
    assert_eq!(body.raw.as_deref(), Some(RAW));
}
