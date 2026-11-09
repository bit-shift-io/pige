//! Task R1: replay-safe correspondent observation learning.
//!
//! These tests pin the durable behavior later autocomplete/ranking tasks rely
//! on: every header role is learned from provider headers, absent Bcc is never
//! invented, drafts are ignored, replaying a page or a UID epoch never
//! multiplies an address's frequency, and learned evidence survives message
//! eviction. No real addresses are used; every fixture is `example.com`.

use std::time::Duration;

use pige_core::Store;
use pige_core::StoreError;
use pige_core::account::{Account, AccountId, Security, ServerConfig};
use pige_core::contacts::{
    AddressKey, HeaderRole, LearningOrigin, ObservationTrust, normalize_address, observe_message,
    observe_outgoing,
};
use pige_core::mail::{
    Address, Flags, FolderKind, MessageHeader, MessageId, StoredMessage, ThreadId,
    parse_header_block,
};
use pige_core::providers::fake::{FakeProviderFactory, FakeProviderSession};
use pige_core::providers::{
    ChangePage, CollectionRecord, HistoryCoverage, HistoryPage, JobKey, JobKind, MailChange,
    MessageRecord, OperationContext, PreparedSubmission, ProviderCursor, ProviderId,
    RemoteCollectionId, RemoteMessageId,
};
use pige_core::providers::{ProviderFactory, SentCopyPolicy};
use pige_core::send::flush_provider_outbox;
use pige_core::send::{OutgoingMessage, queue_submission};
use pige_core::store::history_job;
use tempfile::TempDir;

fn account() -> Account {
    Account {
        id: AccountId("account-r1".to_string()),
        label: "R1".to_string(),
        email: "r1@example.com".to_string(),
        display_name: "R1 Test".to_string(),
        backend: pige_core::account::MailBackend::ImapSmtp {
            username: "r1@example.com".to_string(),
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
            auth: pige_core::AuthKind::Password,
        },
        contact_sources: Vec::new(),
        sort_order: 0,
        color: None,
    }
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(dir.path().join("pige.db")).unwrap();
    store.migrate().unwrap();
    store.upsert_account(&account()).unwrap();
    (dir, store)
}

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, Duration::from_secs(5))
}

fn address(email: &str) -> Address {
    Address::new(None, email)
}

fn key(email: &str) -> AddressKey {
    normalize_address(&address(email)).expect("fixture address must normalize")
}

fn base_header() -> MessageHeader {
    MessageHeader {
        message_id: Some("<message@example.com>".to_string()),
        in_reply_to: None,
        references: Vec::new(),
        subject: "Hello".to_string(),
        from: vec![address("sender@example.com")],
        to: vec![address("alice@example.com")],
        cc: Vec::new(),
        reply_to: Vec::new(),
        bcc: Vec::new(),
        date: 1_700_000_000,
        size: 42,
    }
}

fn record(
    remote: &str,
    header: MessageHeader,
    flags: Flags,
    collections: &[&str],
) -> MessageRecord {
    MessageRecord {
        remote_id: RemoteMessageId::new(remote),
        remote_thread: None,
        header,
        flags,
        collections: collections
            .iter()
            .map(|value| RemoteCollectionId::new(*value))
            .collect(),
        snippet: String::new(),
        has_attachments: false,
    }
}

fn page(messages: Vec<MessageRecord>, checkpoint: &str) -> HistoryPage {
    HistoryPage {
        messages,
        checkpoint: ProviderCursor::new(1, checkpoint),
        has_more: false,
        coverage: HistoryCoverage::default(),
    }
}

fn learned_key(_account: &AccountId) -> AddressKey {
    key("alice@example.com")
}

fn evidence_count(store: &Store, account: &AccountId, address: &str) -> u32 {
    store
        .learned_address(account, &key(address))
        .unwrap()
        .map(|learned| learned.evidence_count)
        .unwrap_or(0)
}

#[test]
fn all_header_roles_and_available_bcc() {
    let mut header = base_header();
    header.reply_to = vec![address("reply@example.com")];
    header.to = vec![address("alice@example.com"), address("bob@example.com")];
    header.cc = vec![address("carol@example.com")];
    header.bcc = vec![address("dave@example.com")];

    let message = record("remote-all", header, Flags::default(), &["INBOX"]);
    let observations = observe_message(&account().id, &message, LearningOrigin::History);

    assert_eq!(
        observations.len(),
        6,
        "one observation per address and role"
    );
    for role in [
        HeaderRole::From,
        HeaderRole::ReplyTo,
        HeaderRole::To,
        HeaderRole::Cc,
        HeaderRole::Bcc,
    ] {
        assert!(
            observations
                .iter()
                .any(|observation| observation.role == role),
            "missing header role {role:?}"
        );
    }
    assert_eq!(
        observations
            .iter()
            .filter(|observation| observation.role == HeaderRole::To)
            .count(),
        2
    );
    assert!(
        observations
            .iter()
            .all(|observation| observation.trust == ObservationTrust::Ordinary)
    );
    assert!(observations.iter().all(|observation| !observation.outgoing));
    assert!(
        observations
            .iter()
            .all(|observation| observation.last_seen == 1_700_000_000)
    );
}

#[test]
fn absent_bcc_is_not_invented() {
    let message = record("remote-no-bcc", base_header(), Flags::default(), &["INBOX"]);
    let observations = observe_message(&account().id, &message, LearningOrigin::History);
    assert!(
        observations
            .iter()
            .all(|observation| observation.role != HeaderRole::Bcc),
        "received mail without a Bcc header must not invent one"
    );
}

#[test]
fn unsent_draft_has_no_evidence() {
    let mut header = base_header();
    header.to = vec![address("alice@example.com")];
    header.bcc = vec![address("dave@example.com")];
    let message = record(
        "remote-draft",
        header,
        Flags {
            draft: true,
            ..Flags::default()
        },
        &["Drafts"],
    );

    assert!(
        observe_message(&account().id, &message, LearningOrigin::History).is_empty(),
        "an unsent draft must never train recipients"
    );
    assert!(
        observe_message(&account().id, &message, LearningOrigin::LiveArrival).is_empty(),
        "a draft replayed by sync must never train recipients"
    );
}

#[test]
fn duplicate_page_is_idempotent() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let page = page(
        vec![record(
            "remote-dup",
            base_header(),
            Flags::default(),
            &["INBOX"],
        )],
        "checkpoint-1",
    );

    store.apply_history_page(&context, None, &page).unwrap();
    // A replayed page is applied against the checkpoint it produced, so the
    // idempotent replay is still accepted and leaves the cursor in place.
    store
        .apply_history_page(&context, Some(&page.checkpoint), &page)
        .unwrap();

    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(
        store
            .learned_roles(&account.id, &learned_key(&account.id))
            .unwrap(),
        vec![HeaderRole::To]
    );
    assert_eq!(
        store
            .job_checkpoint(&history_job(&account.id))
            .unwrap()
            .map(|cursor| cursor.payload),
        Some("checkpoint-1".to_string())
    );
}

#[test]
fn labels_count_one_message() {
    let (_dir, store) = open_store();
    let account = account();
    let header = base_header();
    // The same message appears under two labels with different remote ids, as
    // an adapter may report a Gmail message once per label.
    let page = page(
        vec![
            record(
                "remote-label-a",
                header.clone(),
                Flags::default(),
                &["INBOX"],
            ),
            record("remote-label-b", header, Flags::default(), &["Label"]),
        ],
        "labels",
    );

    store
        .apply_history_page(&ctx(&account.id), None, &page)
        .unwrap();

    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1,
        "one message in two labels must count once"
    );
    assert_eq!(evidence_count(&store, &account.id, "sender@example.com"), 1);
}

#[test]
fn uid_reset_fingerprint_keeps_frequency_bounded() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let header = base_header();

    store
        .apply_history_page(
            &context,
            None,
            &page(
                vec![record(
                    "epoch-1-uid-10",
                    header.clone(),
                    Flags::default(),
                    &["INBOX"],
                )],
                "epoch-1",
            ),
        )
        .unwrap();
    // A UIDVALIDITY reset renames the native identity; the same normalized
    // full header must not multiply the observed frequency.
    store
        .apply_history_page(
            &context,
            Some(&ProviderCursor::new(1, "epoch-1")),
            &page(
                vec![record(
                    "epoch-2-uid-10",
                    header,
                    Flags::default(),
                    &["INBOX"],
                )],
                "epoch-2",
            ),
        )
        .unwrap();

    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1,
        "a UID reset copy must stay bounded at one observation"
    );
}

#[test]
fn domain_normalization_preserves_local_case_dots_and_tags() {
    let address = Address::new(
        Some("Alice Smith".to_string()),
        "First.Last+News@Example.COM",
    );
    assert_eq!(
        normalize_address(&address).unwrap(),
        AddressKey("First.Last+News@example.com".to_string())
    );
    assert_eq!(
        normalize_address(&Address::new(None, "  Bob@EXAMPLE.com  ")).unwrap(),
        AddressKey("Bob@example.com".to_string())
    );
    assert_ne!(
        normalize_address(&Address::new(None, "Alice@example.com")).unwrap(),
        normalize_address(&Address::new(None, "alice@example.com")).unwrap(),
        "local parts are case-sensitive and must not be folded"
    );
    assert!(normalize_address(&Address::new(None, "not an address")).is_err());
    assert!(normalize_address(&Address::new(None, "bad@@example.com")).is_err());
    assert!(normalize_address(&Address::new(None, "")).is_err());
}

#[test]
fn invalid_address_is_skipped_without_losing_page() {
    let (_dir, store) = open_store();
    let account = account();
    let mut header = base_header();
    header.from = vec![address("not an address")];
    header.to = vec![
        address("broken@@example.com"),
        address("alice@example.com"),
        address("  "),
    ];
    header.cc = vec![address("carol@example.com")];

    let page = page(
        vec![record(
            "remote-invalid",
            header,
            Flags::default(),
            &["INBOX"],
        )],
        "invalid",
    );
    store
        .apply_history_page(&ctx(&account.id), None, &page)
        .unwrap();

    assert_eq!(
        store
            .job_checkpoint(&history_job(&account.id))
            .unwrap()
            .map(|cursor| cursor.payload),
        Some("invalid".to_string()),
        "an invalid address must not lose the page checkpoint"
    );
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(evidence_count(&store, &account.id, "carol@example.com"), 1);
    let learned = store.learned_addresses(&account.id).unwrap();
    assert_eq!(learned.len(), 2, "only valid addresses are projected");
    assert!(
        learned
            .iter()
            .all(|entry| entry.address.email == "alice@example.com"
                || entry.address.email == "carol@example.com")
    );
}

#[test]
fn message_eviction_keeps_learned_address() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    let mut header = base_header();
    header.reply_to = vec![address("reply@example.com")];
    header.bcc = vec![address("dave@example.com")];
    let cached = StoredMessage {
        id: MessageId("cached-message".to_string()),
        account_id: account.id.clone(),
        folder_id: None,
        uid: None,
        uidvalidity: None,
        header,
        flags: Flags::default(),
        thread_id: ThreadId("thread-r1".to_string()),
        snippet: "cached".to_string(),
        has_attachments: false,
    };
    store.upsert_message(&cached).unwrap();

    let report = store.learn_existing_cache_page(&context, 10).unwrap();
    assert_eq!(report.scanned, 1);
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(
        store
            .learned_roles(&account.id, &learned_key(&account.id))
            .unwrap(),
        vec![HeaderRole::To],
        "ExistingCache reads cached headers only, so Reply-To/Bcc are excluded"
    );

    store
        .delete_messages(&[MessageId("cached-message".to_string())])
        .unwrap();
    assert!(store.message(&cached.id).unwrap().is_none());

    let learned = store
        .learned_address(&account.id, &learned_key(&account.id))
        .unwrap()
        .expect("learned evidence survives message eviction");
    assert_eq!(learned.evidence_count, 1);
    assert_eq!(
        store
            .learned_roles(&account.id, &learned_key(&account.id))
            .unwrap(),
        vec![HeaderRole::To],
        "eviction must not delete the evidence-address role rows"
    );
}

#[test]
fn existing_cache_backfill_resumes_from_durable_cursor() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    for (id, recipient) in [
        ("cached-a", "alice@example.com"),
        ("cached-b", "bob@example.com"),
    ] {
        let mut header = base_header();
        header.to = vec![address(recipient)];
        store
            .upsert_message(&StoredMessage {
                id: MessageId(id.to_string()),
                account_id: account.id.clone(),
                folder_id: None,
                uid: None,
                uidvalidity: None,
                header,
                flags: Flags::default(),
                thread_id: ThreadId(format!("thread-{id}")),
                snippet: String::new(),
                has_attachments: false,
            })
            .unwrap();
    }

    let first = store.learn_existing_cache_page(&context, 1).unwrap();
    assert_eq!(first.scanned, 1);
    assert!(first.has_more);
    let second = store.learn_existing_cache_page(&context, 1).unwrap();
    assert_eq!(second.scanned, 1);
    assert!(!second.has_more);
    let third = store.learn_existing_cache_page(&context, 1).unwrap();
    assert_eq!(
        third.scanned, 0,
        "the cursor must not restart from the beginning"
    );

    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(evidence_count(&store, &account.id, "bob@example.com"), 1);
}

#[test]
fn existing_cache_and_history_converge_reply_to_and_bcc() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    // The local cache cannot persist Reply-To/Bcc, so ExistingCache reads them
    // empty while History sees the original header. The same physical message
    // must still produce exactly one evidence row, not two.
    let mut reply_header = base_header();
    reply_header.message_id = Some("<reply-to@example.com>".to_string());
    reply_header.subject = "Reply-To path".to_string();
    reply_header.to = vec![address("alice@example.com")];
    reply_header.reply_to = vec![address("reply@example.com")];

    let mut bcc_header = base_header();
    bcc_header.message_id = Some("<bcc@example.com>".to_string());
    bcc_header.subject = "Bcc path".to_string();
    bcc_header.to = vec![address("bob@example.com")];
    bcc_header.bcc = vec![address("dave@example.com")];

    for (id, header) in [
        ("cached-reply-to", reply_header.clone()),
        ("cached-bcc", bcc_header.clone()),
    ] {
        store
            .upsert_message(&StoredMessage {
                id: MessageId(id.to_string()),
                account_id: account.id.clone(),
                folder_id: None,
                uid: None,
                uidvalidity: None,
                header,
                flags: Flags::default(),
                thread_id: ThreadId(format!("thread-{id}")),
                snippet: String::new(),
                has_attachments: false,
            })
            .unwrap();
    }

    let backfill = store.learn_existing_cache_page(&context, 10).unwrap();
    assert_eq!(backfill.scanned, 2);
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(evidence_count(&store, &account.id, "bob@example.com"), 1);

    // Provider history sees the same messages with Reply-To/Bcc populated.
    let history = page(
        vec![
            record(
                "history-reply-to",
                reply_header,
                Flags::default(),
                &["INBOX"],
            ),
            record("history-bcc", bcc_header, Flags::default(), &["INBOX"]),
        ],
        "history-after-cache",
    );
    store.apply_history_page(&context, None, &history).unwrap();

    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1,
        "ExistingCache and History must converge on one evidence row"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "bob@example.com"),
        1,
        "a Bcc that the cache cannot see must not fork the evidence"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "dave@example.com"),
        1,
        "History still learns the Bcc role from the observed header"
    );
    assert_eq!(
        store
            .learned_roles(&account.id, &key("dave@example.com"))
            .unwrap(),
        vec![HeaderRole::Bcc]
    );
}

#[test]
fn stale_history_page_is_rejected_without_regressing_checkpoint() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);

    let first = page(
        vec![record(
            "history-first",
            base_header(),
            Flags::default(),
            &["INBOX"],
        )],
        "cp-1",
    );
    store.apply_history_page(&context, None, &first).unwrap();

    let mut second_header = base_header();
    second_header.message_id = Some("<second@example.com>".to_string());
    let second = page(
        vec![record(
            "history-second",
            second_header,
            Flags::default(),
            &["INBOX"],
        )],
        "cp-2",
    );
    store
        .apply_history_page(&context, Some(&ProviderCursor::new(1, "cp-1")), &second)
        .unwrap();

    // A late page produced against cp-1 is now stale: another page already
    // advanced the checkpoint to cp-2. It must be rejected as a whole.
    let mut stale_header = base_header();
    stale_header.message_id = Some("<stale@example.com>".to_string());
    stale_header.to = vec![address("stale@example.com")];
    let stale = page(
        vec![record(
            "history-stale",
            stale_header,
            Flags::default(),
            &["INBOX"],
        )],
        "cp-1",
    );
    let error = store
        .apply_history_page(&context, Some(&ProviderCursor::new(1, "cp-1")), &stale)
        .unwrap_err();
    assert!(
        matches!(error, StoreError::StalePage),
        "expected StalePage, got {error:?}"
    );

    assert_eq!(
        store
            .job_checkpoint(&history_job(&account.id))
            .unwrap()
            .map(|cursor| cursor.payload),
        Some("cp-2".to_string()),
        "a rejected page must not regress the checkpoint"
    );
    assert_eq!(
        evidence_count(&store, &account.id, "stale@example.com"),
        0,
        "a rejected page must not apply its observations"
    );
}

#[test]
fn only_confirmed_new_arrivals_train_live() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let key = JobKey {
        account: account.id.clone(),
        kind: JobKind::MailSync,
        source: "account".to_string(),
    };
    let collection = CollectionRecord {
        remote_id: RemoteCollectionId::new("INBOX"),
        name: "INBOX".to_string(),
        kind: FolderKind::Inbox,
        delimiter: Some("/".to_string()),
        total: None,
        unread: None,
    };

    let mut replay_header = base_header();
    replay_header.to = vec![address("replay@example.com")];
    let replay = ChangePage {
        changes: vec![
            MailChange::UpsertCollection(collection.clone()),
            MailChange::UpsertMessage {
                message: record("remote-replay", replay_header, Flags::default(), &["INBOX"]),
                is_new_arrival: false,
            },
        ],
        checkpoint: ProviderCursor::new(1, "cp-1"),
        has_more: false,
    };
    store
        .apply_change_page(&context, &key, "imap-smtp", None, &replay)
        .unwrap();
    assert_eq!(
        evidence_count(&store, &account.id, "replay@example.com"),
        0,
        "a snapshot/metadata replay must not train live recipients"
    );

    let arrival = ChangePage {
        changes: vec![MailChange::UpsertMessage {
            message: record(
                "remote-arrival",
                base_header(),
                Flags::default(),
                &["INBOX"],
            ),
            is_new_arrival: true,
        }],
        checkpoint: ProviderCursor::new(1, "cp-2"),
        has_more: false,
    };
    store
        .apply_change_page(
            &context,
            &key,
            "imap-smtp",
            Some(&ProviderCursor::new(1, "cp-1")),
            &arrival,
        )
        .unwrap();
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
}

#[tokio::test]
async fn accepted_send_and_receipt_reconcile_once() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let message = OutgoingMessage {
        account_id: account.id.clone(),
        to: vec![address("alice@example.com")],
        cc: vec![address("bob@example.com")],
        bcc: vec![address("dave@example.com")],
        subject: "Accepted".to_string(),
        body: "Body".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    };
    let id = queue_submission(&store, message.clone(), Vec::new()).unwrap();

    let factory = FakeProviderFactory::submission_only()
        .session(FakeProviderSession::new().sent_copy_policy(SentCopyPolicy::ServerManaged));
    let mut session = factory.open(&account, &context).await.unwrap();
    let report = flush_provider_outbox(
        &store,
        &account,
        session.submission.as_mut().unwrap().as_mut(),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(report.sent, 1);

    for (recipient, role) in [
        ("alice@example.com", HeaderRole::To),
        ("bob@example.com", HeaderRole::Cc),
        ("dave@example.com", HeaderRole::Bcc),
    ] {
        let learned = store
            .learned_address(&account.id, &key(recipient))
            .unwrap()
            .expect("accepted send must learn every envelope recipient");
        assert_eq!(learned.evidence_count, 1);
        assert!(learned.outgoing);
        assert_eq!(learned.trust, ObservationTrust::Deliberate);
        assert_eq!(
            store.learned_roles(&account.id, &key(recipient)).unwrap(),
            vec![role]
        );
    }

    // Replaying the receipt after the row is already accepted must not learn a
    // second time; the acceptance transaction is the once-only gate.
    let entry = store.outbox_entry(&id).unwrap().unwrap();
    let receipt = entry.receipt.clone().unwrap();
    let observations = observe_outgoing(&account.id, &entry.message, entry.created_at);
    let first = store
        .record_outbox_acceptance_with_learning(&id, &receipt, &context, None, &observations)
        .unwrap();
    assert!(!first, "the acceptance transition already happened");
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(
        store
            .learned_address(&account.id, &key("alice@example.com"))
            .unwrap()
            .unwrap()
            .trust,
        ObservationTrust::Deliberate
    );
}

#[tokio::test]
async fn accepted_send_and_sent_copy_do_not_double_count() {
    let (_dir, store) = open_store();
    let account = account();
    let context = ctx(&account.id);
    let message = OutgoingMessage {
        account_id: account.id.clone(),
        to: vec![address("alice@example.com")],
        cc: vec![address("bob@example.com")],
        bcc: vec![address("dave@example.com")],
        subject: "Accepted".to_string(),
        body: "Body".to_string(),
        in_reply_to: None,
        references: Vec::new(),
    };

    // The exact prepared bytes a provider might accept. They carry the Bcc
    // header; the provider's later Sent copy may strip it.
    let raw = concat!(
        "From: R1 Test <r1@example.com>\r\n",
        "To: alice@example.com\r\n",
        "Cc: bob@example.com\r\n",
        "Bcc: dave@example.com\r\n",
        "Subject: Accepted\r\n",
        "Date: Tue, 07 Nov 2023 22:13:20 +0000\r\n",
        "Message-ID: <accepted@example.com>\r\n",
        "\r\n",
        "Body\r\n",
    )
    .as_bytes()
    .to_vec();
    let sent_header = parse_header_block(&raw).unwrap();
    assert_eq!(
        sent_header.bcc,
        vec![address("dave@example.com")],
        "fixture prepared bytes must exercise the Bcc mismatch"
    );

    let prepared = PreparedSubmission {
        version: 1,
        backend: ProviderId::from("fake"),
        raw: Some(raw),
        reference: None,
    };
    queue_submission(&store, message, Vec::new()).unwrap();
    let factory = FakeProviderFactory::submission_only().session(
        FakeProviderSession::new()
            .sent_copy_policy(SentCopyPolicy::ServerManaged)
            .prepared(prepared),
    );
    let mut session = factory.open(&account, &context).await.unwrap();
    let report = flush_provider_outbox(
        &store,
        &account,
        session.submission.as_mut().unwrap().as_mut(),
        &context,
    )
    .await
    .unwrap();
    assert_eq!(report.sent, 1);
    assert_eq!(evidence_count(&store, &account.id, "alice@example.com"), 1);
    assert_eq!(evidence_count(&store, &account.id, "bob@example.com"), 1);
    assert_eq!(evidence_count(&store, &account.id, "dave@example.com"), 1);

    // A later History/Sent copy of the same logical message: the server strips
    // Bcc, so the header is otherwise identical but not byte-for-byte equal.
    let mut copy_header = sent_header;
    copy_header.bcc = Vec::new();
    let sent_copy = record(
        "sent-copy-message",
        copy_header,
        Flags::default(),
        &["Sent"],
    );
    store
        .apply_history_page(&context, None, &page(vec![sent_copy], "sent-copy"))
        .unwrap();

    assert_eq!(
        evidence_count(&store, &account.id, "alice@example.com"),
        1,
        "the Sent copy must reconcile onto the accepted-send evidence"
    );
    assert_eq!(evidence_count(&store, &account.id, "bob@example.com"), 1);
    assert_eq!(
        evidence_count(&store, &account.id, "dave@example.com"),
        1,
        "the stripped Bcc must not add or erase the accepted Bcc evidence"
    );
    assert_eq!(
        store
            .learned_roles(&account.id, &key("dave@example.com"))
            .unwrap(),
        vec![HeaderRole::Bcc]
    );
}
