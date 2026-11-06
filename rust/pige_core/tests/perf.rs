//! Repeatable local performance measurements.
//!
//! These tests are `#[ignore]`d so the normal `cargo test` run stays fast and
//! quiet. Run them explicitly when recording numbers:
//!
//! ```text
//! cd rust && cargo test -p pige_core --test perf -- --ignored --nocapture
//! ```
//!
//! The numbers printed here are what `docs/technical-notes.md` records. They are
//! wall-clock times on the machine that runs the test, measured through the same
//! `Store::search` path the shells call.

use std::time::{Duration, Instant};

use pige_core::{
    Account, AccountId, Address, AuthKind, Body, Flags, Folder, FolderId, FolderKind,
    MessageHeader, MessageId, SearchScope, Security, ServerConfig, Store, StoredMessage, ThreadId,
    Uid,
};
use tempfile::TempDir;

const CONVERSATIONS: u32 = 1_000;
const WARMUP_RUNS: usize = 5;
const MEASURED_RUNS: usize = 20;

fn account() -> Account {
    Account {
        id: AccountId("perf".to_string()),
        label: "Performance".to_string(),
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
        color: None,
    }
}

fn open_store(dir: &TempDir) -> Store {
    let store = Store::open(dir.path().join("pige.db")).expect("open store");
    store.migrate().expect("migrate store");
    store
}

/// Seed `CONVERSATIONS` distinct conversations. One in ten contains the
/// keyword `quantum`, so a search has ~100 real hits to rank over 1,000 rows.
fn seed(store: &Store) {
    let account = account();
    store.upsert_account(&account).unwrap();
    let inbox = Folder {
        id: FolderId("inbox".to_string()),
        account_id: account.id.clone(),
        name: "INBOX".to_string(),
        delimiter: Some("/".to_string()),
        kind: FolderKind::Inbox,
        unread: 0,
        total: CONVERSATIONS,
        uidvalidity: Some(1),
        uidnext: Some(CONVERSATIONS + 1),
    };
    store.upsert_folder(&inbox).unwrap();

    for index in 0..CONVERSATIONS {
        let keyword = if index % 10 == 0 {
            "quantum"
        } else {
            "ordinary"
        };
        let subject = format!("Conversation {index:04} about {keyword} matters");
        let message = StoredMessage {
            id: MessageId(format!("message-{index}")),
            account_id: account.id.clone(),
            folder_id: inbox.id.clone(),
            uid: Uid(index + 1),
            uidvalidity: 1,
            header: MessageHeader {
                message_id: Some(format!("<{index}@example.com>")),
                in_reply_to: None,
                references: Vec::new(),
                subject: subject.clone(),
                from: vec![Address::new(
                    Some(format!("Sender {index}")),
                    "ada@example.com",
                )],
                to: vec![Address::new(None, "inbox@example.com")],
                cc: Vec::new(),
                date: 1_700_000_000 + i64::from(index),
                size: 512,
            },
            flags: Flags::default(),
            thread_id: ThreadId(format!("thread-{index}")),
            snippet: String::new(),
            has_attachments: false,
        };
        store.upsert_message(&message).unwrap();
        store
            .upsert_body(
                &message.id,
                &Body {
                    plain_text: Some(format!(
                        "{keyword} payload for conversation {index}; the body is cached locally."
                    )),
                    html: None,
                    fetched_at: 1,
                },
            )
            .unwrap();
    }
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    let index = ((samples.len() - 1) * percent) / 100;
    samples[index]
}

#[test]
#[ignore = "performance measurement; run explicitly with --ignored --nocapture"]
fn search_latency_on_1000_conversations() {
    let dir = TempDir::new().unwrap();
    let store = open_store(&dir);
    seed(&store);

    assert_eq!(store.schema_version().unwrap(), pige_core::SCHEMA_VERSION);

    let query = "quantum";
    let scope = SearchScope::AllAccounts;

    // Sanity-check the fixture before timing it.
    let hits = store.search(query, &scope, 50).unwrap();
    assert_eq!(hits.len(), 50, "expected the limit to be filled");
    assert_eq!(
        store.search(query, &scope, 1_000).unwrap().len(),
        CONVERSATIONS as usize / 10,
        "expected one hit per matching conversation"
    );

    for _ in 0..WARMUP_RUNS {
        let _ = store.search(query, &scope, 50).unwrap();
    }

    let mut samples = Vec::with_capacity(MEASURED_RUNS);
    for _ in 0..MEASURED_RUNS {
        let started = Instant::now();
        let hits = store.search(query, &scope, 50).unwrap();
        samples.push(started.elapsed());
        assert!(!hits.is_empty());
    }

    let min = *samples.iter().min().unwrap();
    let max = *samples.iter().max().unwrap();
    let median = percentile(&mut samples, 50);
    let p95 = percentile(&mut samples, 95);
    let mean: Duration = samples.iter().sum::<Duration>() / MEASURED_RUNS as u32;

    println!(
        "search over {CONVERSATIONS} conversations ({} matching threads): \
         min={:?} median={:?} mean={:?} p95={:?} max={:?} over {MEASURED_RUNS} runs",
        CONVERSATIONS / 10,
        min,
        median,
        mean,
        p95,
        max,
    );
    assert!(
        median < Duration::from_millis(500),
        "search is unexpectedly slow: median {median:?}"
    );
}
