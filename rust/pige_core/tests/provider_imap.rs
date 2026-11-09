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
    ChangePage, ErrorKind, HistoryRequest, MailChange, MailboxService, MutationAction,
    MutationOutcome, MutationRequest, OperationContext, OperationId, ProviderFactory,
    RemoteCollectionId, SyncRequest,
    imap::{ImapProviderFactory, identity, sync},
};
use pige_core::transport::{Capabilities, FakeTransport, FolderState, RemoteFolder, RemoteMessage};

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
async fn uidvalidity_change_mid_crawl_rebaselines_without_skipping_or_merging() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();

    // Epoch 1: UIDs 10..6, newest first. UID 10 and 9 are the logical messages
    // that the new epoch will renumber.
    let mut old_ten = message(10, "old ten");
    old_ten.header.message_id = Some("<old-ten@example.com>".to_string());
    let mut old_nine = message(9, "old nine");
    old_nine.header.message_id = Some("<old-nine@example.com>".to_string());

    // Epoch 2: a larger UID (20) appears, the old logical messages move to 15
    // and 5, and UID 10 now names a *different* message. Reusing the old
    // `before_uid` of 9 would skip 20/15/10; reusing the old epoch would merge
    // the new UID 10 with epoch 1's UID 10.
    let mut new_twenty = message(20, "new twenty");
    new_twenty.header.message_id = Some("<new-twenty@example.com>".to_string());
    let mut same_ten = old_ten.clone();
    same_ten.uid = Uid(15);
    let mut different_ten = message(10, "different ten");
    different_ten.header.message_id = Some("<different-ten@example.com>".to_string());
    let mut same_nine = old_nine.clone();
    same_nine.uid = Uid(5);

    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 11))]),
            messages: HashMap::from([(
                "INBOX".to_string(),
                vec![
                    old_ten,
                    old_nine,
                    message(8, "old eight"),
                    message(7, "old seven"),
                    message(6, "old six"),
                ],
            )]),
            // Rotate once, after the first page was fetched under epoch 1.
            rotate_after_history_fetches: Some(1),
            rotated_states: HashMap::from([("INBOX".to_string(), state(2, 21))]),
            rotated_messages: HashMap::from([(
                "INBOX".to_string(),
                vec![new_twenty, same_ten, different_ten, same_nine],
            )]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;
    let service = session.mailbox_service().unwrap();
    let mut cursor = None;
    let mut pages = Vec::new();
    let mut records = Vec::new();
    for _ in 0..16 {
        let page = service
            .history(
                HistoryRequest {
                    cursor,
                    page_size: 2,
                },
                &context(),
            )
            .await
            .expect("history page");
        pages.push((page.messages.len(), page.has_more));
        for record in &page.messages {
            let coords = identity::decode_message(&record.remote_id).expect("IMAP id");
            records.push((
                coords.uidvalidity,
                coords.uid.0,
                record.header.message_id.clone(),
            ));
        }
        if !page.has_more {
            break;
        }
        cursor = Some(page.checkpoint.clone());
    }

    assert_eq!(pages[0], (2, true));
    assert!(
        pages.iter().any(|(len, more)| *len == 0 && *more),
        "an epoch change must return a non-complete rebaseline page, got {pages:?}"
    );

    let old_epoch: Vec<u32> = records
        .iter()
        .filter(|(epoch, _, _)| *epoch == 1)
        .map(|(_, uid, _)| *uid)
        .collect();
    assert_eq!(old_epoch, vec![10, 9]);
    let new_epoch: Vec<u32> = records
        .iter()
        .filter(|(epoch, _, _)| *epoch == 2)
        .map(|(_, uid, _)| *uid)
        .collect();
    assert_eq!(
        new_epoch,
        vec![20, 15, 10, 5],
        "no message under the new epoch may be skipped"
    );

    let new_ten = records
        .iter()
        .find(|(epoch, uid, _)| *epoch == 2 && *uid == 10)
        .expect("new epoch UID 10");
    assert_eq!(
        new_ten.2.as_deref(),
        Some("<different-ten@example.com>"),
        "the new epoch's UID 10 is a distinct message"
    );
    assert_ne!(
        identity::message_remote_id("INBOX", 1, Uid(10)),
        identity::message_remote_id("INBOX", 2, Uid(10)),
        "a rotated epoch must never reuse the old remote id"
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

// ---------------------------------------------------------------------------
// Sync and semantic mutations (task F6).
// ---------------------------------------------------------------------------

/// Decode every upserted record in a page, as `(mailbox, uidvalidity, uid)`.
fn upserted(page: &ChangePage) -> Vec<(String, u32, u32)> {
    page.changes
        .iter()
        .filter_map(|change| match change {
            MailChange::UpsertMessage { message, .. } => {
                let coords = identity::decode_message(&message.remote_id).expect("IMAP id");
                Some((coords.mailbox, coords.uidvalidity, coords.uid.0))
            }
            _ => None,
        })
        .collect()
}

fn removed(page: &ChangePage) -> Vec<(String, u32, u32)> {
    page.changes
        .iter()
        .filter_map(|change| match change {
            MailChange::RemoveMessage(remote) => {
                let coords = identity::decode_message(remote).expect("IMAP id");
                Some((coords.mailbox, coords.uidvalidity, coords.uid.0))
            }
            _ => None,
        })
        .collect()
}

fn removed_collections(page: &ChangePage) -> Vec<String> {
    page.changes
        .iter()
        .filter_map(|change| match change {
            MailChange::RemoveCollection(remote) => Some(remote.0.clone()),
            _ => None,
        })
        .collect()
}

fn request(
    action: MutationAction,
    targets: Vec<pige_core::providers::RemoteMessageId>,
) -> MutationRequest {
    MutationRequest {
        operation: OperationId::from("op-1"),
        targets,
        source: None,
        action,
    }
}

#[tokio::test]
async fn initial_window_is_not_full_snapshot() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    let messages: Vec<RemoteMessage> = (1..=400)
        .map(|uid| message(uid, &format!("message {uid}")))
        .collect();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 401))]),
            messages: HashMap::from([("INBOX".to_string(), messages)]),
            ..FakeTransport::default()
        },
    );

    let mut session = open(transports.clone()).await;
    let page = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 300,
            },
            &context(),
        )
        .await
        .expect("sync page");

    // A bounded recent window must never retire older cached mail.
    assert!(
        removed(&page).is_empty(),
        "a bounded window must not delete older cache: {:?}",
        removed(&page)
    );

    let mut uids: Vec<u32> = upserted(&page).into_iter().map(|(_, _, uid)| uid).collect();
    uids.sort_unstable();
    assert_eq!(uids.len(), 300, "exactly the bounded window is reported");
    assert_eq!(uids.first(), Some(&101));
    assert_eq!(uids.last(), Some(&400));

    let cursor = sync::decode_sync_cursor(&page.checkpoint).expect("sync cursor");
    let inbox = cursor
        .mailboxes
        .iter()
        .find(|mailbox| mailbox.name == "INBOX")
        .expect("INBOX progress");
    assert!(
        !inbox.snapshot_complete,
        "a bounded window is not a completed snapshot"
    );
}

#[tokio::test]
async fn epoch_reset_reconciles_only_affected_mailbox() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    let first = FakeTransport {
        folders: vec![
            folder("INBOX", FolderKind::Inbox),
            folder("Sent", FolderKind::Sent),
        ],
        states: HashMap::from([
            ("INBOX".to_string(), state(1, 4)),
            ("Sent".to_string(), state(5, 8)),
        ]),
        messages: HashMap::from([
            (
                "INBOX".to_string(),
                vec![message(1, "a"), message(2, "b"), message(3, "c")],
            ),
            ("Sent".to_string(), vec![message(7, "sent")]),
        ]),
        ..FakeTransport::default()
    };
    transports.add_transport(account.id.clone(), first.clone());

    let mut session = open(transports.clone()).await;
    let first_page = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("first sync");

    // Rotate only INBOX's UIDVALIDITY epoch; Sent is untouched.
    let mut second = first.clone();
    second.messages.insert(
        "INBOX".to_string(),
        vec![message(1, "fresh-a"), message(2, "fresh-b")],
    );
    second.states.insert("INBOX".to_string(), state(2, 3));
    transports.add_transport(account.id.clone(), second);

    let mut session = open(transports.clone()).await;
    let page = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: Some(first_page.checkpoint),
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("second sync");

    // Every removal is the rotated INBOX epoch; nothing touches Sent.
    let removals = removed(&page);
    assert_eq!(
        removals,
        vec![
            ("INBOX".to_string(), 1, 1),
            ("INBOX".to_string(), 1, 2),
            ("INBOX".to_string(), 1, 3),
        ],
        "only the affected mailbox reconciles"
    );
    let upserts = upserted(&page);
    assert!(
        upserts
            .iter()
            .all(|(mailbox, uidvalidity, _)| mailbox == "INBOX" && *uidvalidity == 2),
        "the untouched mailbox must not be re-synced: {upserts:?}"
    );
    assert_eq!(upserts.len(), 2);

    let cursor = sync::decode_sync_cursor(&page.checkpoint).expect("sync cursor");
    let sent = cursor
        .mailboxes
        .iter()
        .find(|mailbox| mailbox.name == "Sent")
        .expect("Sent progress");
    assert_eq!(sent.uidvalidity, 5, "Sent keeps its epoch");
    assert_eq!(sent.uidnext, 8, "Sent keeps its high-water");
}

#[tokio::test]
async fn imap_delete_is_not_api_label_removal() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: false,
                move_ext: true,
            },
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Work", FolderKind::Other),
            ],
            states: HashMap::from([
                ("INBOX".to_string(), state(1, 2)),
                ("Work".to_string(), state(3, 2)),
            ]),
            messages: HashMap::from([
                ("INBOX".to_string(), vec![message(1, "inbox")]),
                ("Work".to_string(), vec![message(1, "work")]),
            ]),
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;

    // "Remove the Work label" is not a permanent delete. On a plain IMAP server
    // a folder is the message's only home, so it cannot remove a label without
    // deleting the message: unsupported, and nothing is sent to the server.
    let label_removal = session
        .mutation_service()
        .unwrap()
        .apply(
            request(
                MutationAction::RemoveLabel(RemoteCollectionId::from("Work")),
                vec![target.clone()],
            ),
            &context(),
        )
        .await
        .expect("label removal");
    assert_eq!(label_removal.items.len(), 1);
    assert_eq!(
        label_removal.items[0].outcome,
        MutationOutcome::Rejected(pige_core::providers::ProviderError::unsupported(
            "removing a label on this mail server"
        )),
        "label removal must not be a permanent delete"
    );

    // Permanent delete does expunge the source message.
    let deleted = session
        .mutation_service()
        .unwrap()
        .apply(
            request(MutationAction::DeletePermanently, vec![target.clone()]),
            &context(),
        )
        .await
        .expect("permanent delete");
    assert_eq!(deleted.items[0].outcome, MutationOutcome::Applied);

    let transport = transports.transport(&account.id).await.expect("transport");
    assert!(
        transport.copy_calls.is_empty() && transport.move_calls.is_empty(),
        "removing a label must never copy or move"
    );
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])],
        "only the permanent delete expunged"
    );
}

#[tokio::test]
async fn gmail_imap_archive_removes_source_label_only() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: true,
                move_ext: false,
            },
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
            messages: HashMap::from([("INBOX".to_string(), vec![message(1, "kept")])]),
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;
    let result = session
        .mutation_service()
        .unwrap()
        .apply(request(MutationAction::Archive, vec![target]), &context())
        .await
        .expect("archive");

    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);
    let transport = transports.transport(&account.id).await.expect("transport");
    // Gmail archive removes the source label (a scoped delete); it does not
    // copy anywhere and does not MOVE.
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])]
    );
    assert!(transport.copy_calls.is_empty(), "archive does not copy");
    assert!(transport.move_calls.is_empty(), "archive is not a MOVE");
}

#[tokio::test]
async fn gmail_permanent_delete_is_not_label_removal() {
    // A Gmail message is named per mailbox: the source label's UID does not
    // address `[Gmail]/All Mail`, so expunging the source would remove only the
    // source label — exactly what `Archive` does. The adapter cannot address a
    // true permanent delete with a source UID, so it must fail closed rather
    // than silently degrade to a label removal.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: true,
                move_ext: false,
            },
            folders: vec![folder("INBOX", FolderKind::Inbox)],
            states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
            messages: HashMap::from([("INBOX".to_string(), vec![message(1, "kept")])]),
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;
    let deleted = session
        .mutation_service()
        .unwrap()
        .apply(
            request(MutationAction::DeletePermanently, vec![target]),
            &context(),
        )
        .await
        .expect("delete request");

    assert!(
        matches!(
            &deleted.items[0].outcome,
            MutationOutcome::Rejected(error) if error.kind == ErrorKind::Unsupported
        ),
        "Gmail permanent delete must fail closed, not equal archive/label removal: {:?}",
        deleted.items[0].outcome
    );

    let transport = transports.transport(&account.id).await.expect("transport");
    assert!(
        transport.delete_calls.is_empty(),
        "Gmail permanent delete must not expunge the source label: {:?}",
        transport.delete_calls
    );
    // The capability must not claim a permanent delete the adapter cannot do.
    assert!(!session.capabilities.mutations.permanent_delete);
}

#[tokio::test]
async fn gmail_remove_label_never_uses_source_uid() {
    // INBOX UID 1 and Work UID 900 name the same Gmail message. IMAP UIDs are
    // per-mailbox, so deleting INBOX UID 1 after selecting Work would target a
    // different message (or nothing). The adapter must never act on a UID from
    // another mailbox's UID space.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: true,
                move_ext: false,
            },
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Work", FolderKind::Other),
            ],
            states: HashMap::from([
                ("INBOX".to_string(), state(1, 2)),
                ("Work".to_string(), state(7, 901)),
            ]),
            messages: HashMap::from([
                ("INBOX".to_string(), vec![message(1, "work mail")]),
                ("Work".to_string(), vec![message(900, "work mail")]),
            ]),
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;
    let result = session
        .mutation_service()
        .unwrap()
        .apply(
            request(
                MutationAction::RemoveLabel(RemoteCollectionId::from("Work")),
                vec![target],
            ),
            &context(),
        )
        .await
        .expect("remove label request");

    assert!(
        matches!(
            &result.items[0].outcome,
            MutationOutcome::Rejected(error) if error.kind == ErrorKind::Unsupported
        ),
        "Gmail label removal needs the per-label UID and must fail closed: {:?}",
        result.items[0].outcome
    );

    let transport = transports.transport(&account.id).await.expect("transport");
    assert!(
        transport.delete_calls.is_empty(),
        "must never delete INBOX UID 1 from the Work mailbox: {:?}",
        transport.delete_calls
    );
    assert!(
        transport.select_calls.iter().all(|name| name != "Work"),
        "must not even select a foreign mailbox to act on its UID space: {:?}",
        transport.select_calls
    );
    // Label *addition* is implemented (COPY into the label folder), so label
    // management is advertised even though removal stays unsupported.
    assert!(session.capabilities.mutations.labels);
}

#[tokio::test]
async fn gmail_add_label_copies_source_uid_into_label_folder() {
    // Adding a Gmail label is a COPY of the *source* mailbox's own UID into the
    // label folder — the same UID space (Gmail COPY = add label), so it is
    // correct and must succeed. It never deletes or moves the source message.
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: true,
                move_ext: false,
            },
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Work", FolderKind::Other),
            ],
            states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
            messages: HashMap::from([("INBOX".to_string(), vec![message(1, "labeled mail")])]),
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;
    assert!(
        session.capabilities.mutations.labels,
        "Gmail add-label is implemented, so label management is advertised"
    );

    let result = session
        .mutation_service()
        .unwrap()
        .apply(
            request(
                MutationAction::AddLabel(RemoteCollectionId::from("Work")),
                vec![target],
            ),
            &context(),
        )
        .await
        .expect("add label request");

    assert_eq!(result.items[0].outcome, MutationOutcome::Applied);

    let transport = transports.transport(&account.id).await.expect("transport");
    assert_eq!(
        transport.copy_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Work".to_string())],
        "add-label copies the source mailbox's own UID into the label folder"
    );
    assert!(
        transport.delete_calls.is_empty(),
        "adding a label must not delete the source: {:?}",
        transport.delete_calls
    );
    assert!(
        transport.move_calls.is_empty(),
        "adding a label is a COPY, not a MOVE: {:?}",
        transport.move_calls
    );
}

#[tokio::test]
async fn imap_advertises_only_implemented_mutations() {
    // Plain IMAP: an expunge permanently deletes from the source mailbox, and
    // labels do not exist.
    let plain = Arc::new(FakeTransportFactory::default());
    plain.add_transport(
        account().id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: false,
                move_ext: true,
            },
            ..FakeTransport::default()
        },
    );
    let plain = open(plain).await;
    assert!(plain.capabilities.mutations.permanent_delete);
    assert!(!plain.capabilities.mutations.labels);

    // Gmail: add-label (COPY into the label folder) is implemented, so label
    // management is advertised; permanent delete cannot be performed correctly
    // with a per-label UID, so it is not.
    let gmail = Arc::new(FakeTransportFactory::default());
    gmail.add_transport(
        account().id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: true,
                move_ext: false,
            },
            ..FakeTransport::default()
        },
    );
    let gmail = open(gmail).await;
    assert!(gmail.capabilities.mutations.labels);
    assert!(!gmail.capabilities.mutations.permanent_delete);
}

#[tokio::test]
async fn new_other_folder_can_be_adopted_on_demand() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Work", FolderKind::Other),
            ],
            states: HashMap::from([
                ("INBOX".to_string(), state(1, 2)),
                ("Work".to_string(), state(9, 51)),
            ]),
            messages: HashMap::from([
                ("INBOX".to_string(), vec![message(1, "inbox")]),
                ("Work".to_string(), vec![message(50, "work")]),
            ]),
            ..FakeTransport::default()
        },
    );

    // A first page auto-syncs only special folders: a brand-new Other folder is
    // announced as a collection, but its messages are not fetched and no
    // progress is recorded for it.
    let mut session = open(transports.clone()).await;
    let first = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("first sync");
    assert!(
        upserted(&first)
            .iter()
            .all(|(mailbox, _, _)| mailbox != "Work"),
        "an Other folder is not auto-synced before it is requested"
    );
    let cursor = sync::decode_sync_cursor(&first.checkpoint).expect("sync cursor");
    assert!(
        cursor
            .mailboxes
            .iter()
            .all(|mailbox| mailbox.name != "Work"),
        "no progress exists for the untouched Other folder"
    );

    // F7 can name the folder through the cursor: `request_mailbox` folds it into
    // the checkpoint, and the next page bootstraps it under its own identity.
    let seeded = sync::request_mailbox(Some(&first.checkpoint), "Work").expect("seed request");
    let mut session = open(transports.clone()).await;
    let adopted = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: Some(seeded),
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("adopting sync");
    assert_eq!(
        upserted(&adopted),
        vec![("Work".to_string(), 9, 50)],
        "the requested Other folder is bootstrapped"
    );
    let adopted_cursor = adopted.checkpoint.clone();
    let cursor = sync::decode_sync_cursor(&adopted_cursor).expect("sync cursor");
    assert!(
        cursor.requested.is_empty(),
        "a spent adoption request is not carried forward"
    );
    assert!(
        cursor
            .mailboxes
            .iter()
            .any(|mailbox| mailbox.name == "Work"),
        "the adopted folder is now tracked by its own progress"
    );

    // It keeps syncing without another request: a newer Work UID is reported by
    // the ordinary delta path.
    let after = FakeTransport {
        folders: vec![
            folder("INBOX", FolderKind::Inbox),
            folder("Work", FolderKind::Other),
        ],
        states: HashMap::from([
            ("INBOX".to_string(), state(1, 2)),
            ("Work".to_string(), state(9, 52)),
        ]),
        messages: HashMap::from([
            ("INBOX".to_string(), vec![message(1, "inbox")]),
            (
                "Work".to_string(),
                vec![message(50, "work"), message(51, "new work")],
            ),
        ]),
        ..FakeTransport::default()
    };
    transports.add_transport(account.id.clone(), after);

    let mut session = open(transports.clone()).await;
    let delta = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: Some(adopted_cursor),
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("delta sync");
    assert!(
        upserted(&delta)
            .iter()
            .any(|(mailbox, _, uid)| mailbox == "Work" && *uid == 51),
        "the adopted folder keeps syncing on its own: {:?}",
        upserted(&delta)
    );
}

#[tokio::test]
async fn copy_delete_fallback_reports_partial_outcome() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    transports.add_transport(
        account.id.clone(),
        FakeTransport {
            capabilities: Capabilities {
                gmail: false,
                move_ext: false,
            },
            folders: vec![
                folder("INBOX", FolderKind::Inbox),
                folder("Archive", FolderKind::Archive),
            ],
            states: HashMap::from([("INBOX".to_string(), state(1, 2))]),
            messages: HashMap::from([("INBOX".to_string(), vec![message(1, "moved")])]),
            // The COPY succeeds but the source delete fails: the message is now
            // in both mailboxes, so its outcome is genuinely unknown.
            fail_delete: true,
            ..FakeTransport::default()
        },
    );

    let target = identity::message_remote_id("INBOX", 1, Uid(1));
    let mut session = open(transports.clone()).await;
    let result = session
        .mutation_service()
        .unwrap()
        .apply(
            request(
                MutationAction::Move(RemoteCollectionId::from("Archive")),
                vec![target],
            ),
            &context(),
        )
        .await
        .expect("move");

    assert_eq!(
        result.items[0].outcome,
        MutationOutcome::Unknown,
        "a partially-applied COPY + delete is not a definite success"
    );

    let transport = transports.transport(&account.id).await.expect("transport");
    assert_eq!(
        transport.copy_calls,
        vec![("INBOX".to_string(), vec![Uid(1)], "Archive".to_string())]
    );
    assert_eq!(
        transport.delete_calls,
        vec![("INBOX".to_string(), vec![Uid(1)])],
        "the fallback tried to remove the source"
    );
}

#[tokio::test]
async fn folder_rename_does_not_merge_unrelated_uid_identity() {
    let transports = Arc::new(FakeTransportFactory::default());
    let account = account();
    let before = FakeTransport {
        folders: vec![
            folder("INBOX", FolderKind::Inbox),
            folder("Sent", FolderKind::Sent),
        ],
        states: HashMap::from([
            ("INBOX".to_string(), state(1, 1)),
            ("Sent".to_string(), state(9, 8)),
        ]),
        messages: HashMap::from([("Sent".to_string(), vec![message(7, "outgoing")])]),
        ..FakeTransport::default()
    };
    transports.add_transport(account.id.clone(), before);

    let mut session = open(transports.clone()).await;
    let first_page = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: None,
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("first sync");
    assert_eq!(
        upserted(&first_page),
        vec![("Sent".to_string(), 9, 7)],
        "the message starts under the Sent identity"
    );

    // The mailbox is renamed: the same server-side UIDs now live under a new
    // name, with the same UIDVALIDITY. A UID must never be matched across names.
    let after = FakeTransport {
        folders: vec![
            folder("INBOX", FolderKind::Inbox),
            folder("Outbox", FolderKind::Sent),
        ],
        states: HashMap::from([
            ("INBOX".to_string(), state(1, 1)),
            ("Outbox".to_string(), state(9, 8)),
        ]),
        messages: HashMap::from([("Outbox".to_string(), vec![message(7, "outgoing")])]),
        ..FakeTransport::default()
    };
    transports.add_transport(account.id.clone(), after);

    let mut session = open(transports.clone()).await;
    let page = session
        .sync_service()
        .unwrap()
        .next_page(
            SyncRequest {
                cursor: Some(first_page.checkpoint),
                initial_limit: 100,
            },
            &context(),
        )
        .await
        .expect("second sync");

    // The vanished mailbox is retired: its collection and every message under
    // its old identity.
    assert_eq!(removed_collections(&page), vec!["Sent".to_string()]);
    assert_eq!(
        removed(&page),
        vec![("Sent".to_string(), 9, 7)],
        "the old identity is retired, not reused"
    );

    // The new mailbox is bootstrapped under its own name.
    let upserts = upserted(&page);
    assert_eq!(upserts, vec![("Outbox".to_string(), 9, 7)]);
    assert!(
        !page.changes.iter().any(|change| match change {
            MailChange::UpsertMessage { message, .. } => {
                identity::decode_message(&message.remote_id)
                    .is_some_and(|coords| coords.mailbox == "Sent")
            }
            _ => false,
        }),
        "no change may re-use the retired Sent identity"
    );

    // The new identity is genuinely distinct from the old one.
    assert_ne!(
        identity::message_remote_id("Outbox", 9, Uid(7)),
        identity::message_remote_id("Sent", 9, Uid(7)),
        "a rename must not collapse two identities"
    );
}
