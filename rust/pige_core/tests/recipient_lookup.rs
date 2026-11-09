//! Task R2: indexed local recipient lookup and ranking.
//!
//! Every suggestion is served from the local SQLite projection; no provider is
//! opened. The tests pin the ranking contract (deliberate/outgoing before saved
//! contacts, Other contacts, then incoming-only), source/account merging,
//! suppression rules, and the bounded/indexed query surface. No real addresses
//! are used; all fixtures are `example.com`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use pige_core::Store;
use pige_core::account::{Account, AccountId, MailBackend, Security, ServerConfig};
use pige_core::api::{
    self, Config, DB_FILE_NAME, RecipientQuery, RecipientSuggestion, SuggestionSource,
};
use pige_core::contacts::rank::rank_candidates;
use pige_core::contacts::{
    EvidenceId, HeaderRole, LearningOrigin, Observation, ObservationTrust, normalize_address,
};
use pige_core::mail::Address;
use pige_core::platform::fake::{
    FakeBackupStore, FakeCredentials, FakeNotifier, FakeOpener, FakeTransportFactory,
};
use pige_core::providers::fake::FakeProviderFactory;
use pige_core::providers::{ContactSourceId, OperationContext, RemoteContactId};
use rusqlite::Connection;
use tempfile::TempDir;

/// The API state is process-global; tests that initialize it must not overlap.
static API_LOCK: Mutex<()> = Mutex::new(());

fn api_lock() -> MutexGuard<'static, ()> {
    API_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn account(id: &str, email: &str) -> Account {
    Account {
        id: AccountId(id.to_string()),
        label: id.to_string(),
        email: email.to_string(),
        display_name: id.to_string(),
        backend: MailBackend::ImapSmtp {
            username: email.to_string(),
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

fn account_with_username(id: &str, email: &str, username: &str) -> Account {
    let mut account = account(id, email);
    if let MailBackend::ImapSmtp {
        username: stored, ..
    } = &mut account.backend
    {
        *stored = username.to_string();
    }
    account
}

struct Harness {
    _dir: TempDir,
    db_path: std::path::PathBuf,
    factory: Arc<FakeProviderFactory>,
}

impl Harness {
    fn new(accounts: &[Account]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let db_path = dir.path().join("data").join(DB_FILE_NAME);
        {
            let store = Store::open(&db_path).unwrap();
            store.migrate().unwrap();
            for account in accounts {
                store.upsert_account(account).unwrap();
            }
        }

        let factory = Arc::new(FakeProviderFactory::new());
        let credentials = Arc::new(FakeCredentials::default());
        api::reset_for_tests();
        api::init(Config {
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
            config_dir: dir.path().join("config"),
            credentials,
            notifier: Arc::new(FakeNotifier::default()),
            opener: Arc::new(FakeOpener::default()),
            providers: factory.clone(),
            transport: Arc::new(FakeTransportFactory::default()),
            backups: Arc::new(FakeBackupStore::new()),
            auth: None,
        })
        .expect("init api");

        Self {
            _dir: dir,
            db_path,
            factory,
        }
    }

    fn store(&self) -> Store {
        Store::open(&self.db_path).unwrap()
    }

    fn suggest(&self, sending: &AccountId, text: &str, limit: u16) -> Vec<RecipientSuggestion> {
        api::suggest_recipients(RecipientQuery {
            sending_account: sending.clone(),
            text: text.to_string(),
            limit,
        })
        .expect("suggestions")
    }
}

fn ctx(account: &AccountId) -> OperationContext {
    OperationContext::with_timeout(account.clone(), 1, std::time::Duration::from_secs(5))
}

fn address(name: Option<&str>, email: &str) -> Address {
    Address::new(name.map(str::to_string), email)
}

#[allow(clippy::too_many_arguments)]
fn observe(
    store: &Store,
    account: &AccountId,
    evidence: &str,
    origin: LearningOrigin,
    address: Address,
    role: HeaderRole,
    last_seen: i64,
    outgoing: bool,
    trust: ObservationTrust,
) {
    let key = normalize_address(&address).expect("fixture address must normalize");
    store
        .record_observations(
            &ctx(account),
            &EvidenceId::from(evidence.to_string()),
            origin,
            None,
            &[Observation {
                key,
                address,
                role,
                last_seen,
                outgoing,
                trust,
            }],
        )
        .unwrap();
}

fn incoming(
    store: &Store,
    account: &AccountId,
    evidence: &str,
    email: &str,
    name: Option<&str>,
    at: i64,
) {
    observe(
        store,
        account,
        evidence,
        LearningOrigin::History,
        address(name, email),
        HeaderRole::From,
        at,
        false,
        ObservationTrust::Ordinary,
    );
}

fn outgoing(store: &Store, account: &AccountId, evidence: &str, email: &str, at: i64) {
    observe(
        store,
        account,
        evidence,
        LearningOrigin::AcceptedSend,
        address(None, email),
        HeaderRole::To,
        at,
        true,
        ObservationTrust::Deliberate,
    );
}

fn contact(store: &Store, account: &AccountId, source: &str, kind: &str, name: &str, email: &str) {
    let source_id = ContactSourceId::from(source);
    store
        .upsert_contact_source_with_kind(account, &source_id, Some("Fixture"), kind)
        .unwrap();
    store
        .upsert_contact_email(
            account,
            &source_id,
            &RemoteContactId::from(format!("{source}-{email}")),
            &address(Some(name), email),
        )
        .unwrap();
}

fn emails(suggestions: &[RecipientSuggestion]) -> Vec<&str> {
    suggestions
        .iter()
        .map(|suggestion| suggestion.address.email.as_str())
        .collect()
}

fn suggestion<'a>(suggestions: &'a [RecipientSuggestion], email: &str) -> &'a RecipientSuggestion {
    suggestions
        .iter()
        .find(|suggestion| suggestion.address.email == email)
        .unwrap_or_else(|| panic!("missing suggestion for {email}: {:?}", emails(suggestions)))
}

#[test]
fn suggestions_work_with_network_disabled() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();
    incoming(
        &store,
        &account.id,
        "message:no-network",
        "alice@example.com",
        Some("Alice"),
        100,
    );

    let suggestions = harness.suggest(&account.id, "ali", 10);
    assert_eq!(emails(&suggestions), vec!["alice@example.com"]);
    assert_eq!(suggestions[0].sources, vec![SuggestionSource::Incoming]);
    assert_eq!(
        harness.factory.recorder().open_attempts(),
        0,
        "recipient lookup must never open a provider or touch the network"
    );
}

#[test]
fn outgoing_and_selected_rank_above_incoming() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    outgoing(&store, &account.id, "operation:out", "out@example.com", 100);
    incoming(
        &store,
        &account.id,
        "message:in",
        "in@example.com",
        None,
        900,
    );

    // A deliberate compose selection is learned through the public API, not by
    // treating arbitrary keystrokes as evidence.
    api::record_recipient_selection(&account.id, &address(None, "selected@example.com")).unwrap();

    let suggestions = harness.suggest(&account.id, "example.com", 10);
    let ordered = emails(&suggestions);
    let deliberate: std::collections::HashSet<&str> = ordered.iter().take(2).copied().collect();
    assert_eq!(
        deliberate,
        std::collections::HashSet::from(["out@example.com", "selected@example.com"]),
        "deliberate sources outrank a much newer incoming observation: {ordered:?}"
    );
    assert_eq!(ordered[2], "in@example.com");
    assert_eq!(
        suggestion(&suggestions, "selected@example.com").sources,
        vec![SuggestionSource::Selected]
    );
    assert_eq!(
        suggestion(&suggestions, "out@example.com").sources,
        vec![SuggestionSource::Outgoing]
    );
}

#[test]
fn saved_name_beats_incoming_spoof() {
    let _guard = api_lock();
    let primary = account("account-a", "self@example.com");
    let secondary = account("account-b", "self-b@example.com");
    let harness = Harness::new(&[primary.clone(), secondary.clone()]);
    let store = harness.store();

    contact(
        &store,
        &primary.id,
        "saved-source",
        "saved",
        "Alice Real",
        "alice@example.com",
    );
    // The same address arrives in another account with an attacker-controlled
    // display name and a much newer timestamp.
    incoming(
        &store,
        &secondary.id,
        "message:spoof",
        "alice@example.com",
        Some("Mallory Spoof"),
        9_000,
    );

    let suggestions = harness.suggest(&primary.id, "alice", 10);
    let alice = suggestion(&suggestions, "alice@example.com");
    assert_eq!(
        alice.address.name.as_deref(),
        Some("Alice Real"),
        "a trusted saved name must never be replaced by an incoming header name"
    );
    assert!(alice.sources.contains(&SuggestionSource::SavedContact));
    assert!(alice.sources.contains(&SuggestionSource::Incoming));
}

#[test]
fn same_exact_address_combines_sources() {
    let _guard = api_lock();
    let primary = account("account-a", "self@example.com");
    let secondary = account("account-b", "self-b@example.com");
    let harness = Harness::new(&[primary.clone(), secondary.clone()]);
    let store = harness.store();

    contact(
        &store,
        &primary.id,
        "saved-source",
        "saved",
        "Alice",
        "alice@example.com",
    );
    outgoing(
        &store,
        &primary.id,
        "operation:alice",
        "alice@example.com",
        200,
    );
    incoming(
        &store,
        &secondary.id,
        "message:alice",
        "alice@example.com",
        Some("Alice"),
        300,
    );

    let suggestions = harness.suggest(&primary.id, "alice@example.com", 10);
    assert_eq!(suggestions.len(), 1, "exact addresses are deduplicated");
    let alice = &suggestions[0];
    assert_eq!(
        alice.address.email, "alice@example.com",
        "the returned deliverable address must stay unchanged"
    );
    assert_eq!(
        alice.sources,
        vec![
            SuggestionSource::SavedContact,
            SuggestionSource::Outgoing,
            SuggestionSource::Incoming,
        ]
    );
    assert_eq!(
        alice.accounts,
        vec![primary.id.clone(), secondary.id.clone()],
        "the current sending account is listed first"
    );
}

#[test]
fn case_distinct_local_parts_stay_distinct() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    incoming(
        &store,
        &account.id,
        "message:upper",
        "Alice@example.com",
        None,
        100,
    );
    incoming(
        &store,
        &account.id,
        "message:lower",
        "alice@example.com",
        None,
        101,
    );

    let suggestions = harness.suggest(&account.id, "alice", 10);
    let returned = emails(&suggestions);
    assert_eq!(
        returned.len(),
        2,
        "case-distinct local parts are distinct: {returned:?}"
    );
    assert!(returned.contains(&"Alice@example.com"));
    assert!(returned.contains(&"alice@example.com"));
}

#[test]
fn blocked_and_spam_only_are_suppressed() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    incoming(
        &store,
        &account.id,
        "message:ordinary",
        "person@example.com",
        Some("Person"),
        100,
    );
    observe(
        &store,
        &account.id,
        "message:spam",
        LearningOrigin::History,
        address(Some("Spam Trap"), "spam-only@example.com"),
        HeaderRole::From,
        200,
        false,
        ObservationTrust::SpamOrTrash,
    );
    api::block_sender("Blocked@Example.com").unwrap();
    incoming(
        &store,
        &account.id,
        "message:blocked",
        "Blocked@example.com",
        Some("Blocked"),
        300,
    );

    let suggestions = harness.suggest(&account.id, "example.com", 10);
    let returned = emails(&suggestions);
    assert!(returned.contains(&"person@example.com"));
    assert!(
        !returned.contains(&"spam-only@example.com"),
        "spam-only evidence must stay hidden: {returned:?}"
    );
    assert!(
        !returned.contains(&"Blocked@example.com"),
        "blocked senders are suppressed globally: {returned:?}"
    );

    // Independently trusted evidence makes a previously spam-only address
    // visible again.
    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Now Trusted",
        "spam-only@example.com",
    );
    let suggestions = harness.suggest(&account.id, "spam-only", 10);
    assert_eq!(emails(&suggestions), vec!["spam-only@example.com"]);
    assert_eq!(
        suggestions[0].address.name.as_deref(),
        Some("Now Trusted"),
        "saved contact metadata wins once the address is trusted"
    );
}

#[test]
fn self_aliases_are_suppressed() {
    let _guard = api_lock();
    let account = account_with_username("account-a", "self@example.com", "alias@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    incoming(
        &store,
        &account.id,
        "message:self",
        "self@example.com",
        None,
        100,
    );
    incoming(
        &store,
        &account.id,
        "message:alias",
        "alias@example.com",
        None,
        100,
    );
    incoming(
        &store,
        &account.id,
        "message:other",
        "colleague@example.com",
        None,
        50,
    );

    let suggestions = harness.suggest(&account.id, "example.com", 10);
    assert_eq!(
        emails(&suggestions),
        vec!["colleague@example.com"],
        "the account's own addresses and aliases are never default suggestions"
    );
}

#[test]
fn saved_contacts_rank_above_other_contacts_and_incoming() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Saved Person",
        "saved@example.com",
    );
    contact(
        &store,
        &account.id,
        "other-source",
        "other",
        "Other Person",
        "other@example.com",
    );
    incoming(
        &store,
        &account.id,
        "message:incoming",
        "incoming@example.com",
        Some("Incoming Person"),
        9_000,
    );

    let suggestions = harness.suggest(&account.id, "example.com", 10);
    assert_eq!(
        emails(&suggestions),
        vec![
            "saved@example.com",
            "other@example.com",
            "incoming@example.com"
        ],
        "saved contacts then Other contacts then incoming-only, even when incoming is newest"
    );
    assert_eq!(
        suggestion(&suggestions, "saved@example.com").sources,
        vec![SuggestionSource::SavedContact]
    );
    assert_eq!(
        suggestion(&suggestions, "other@example.com").sources,
        vec![SuggestionSource::OtherContact]
    );
}

#[test]
fn recency_and_capped_frequency_break_ties() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    incoming(
        &store,
        &account.id,
        "message:newer",
        "newer@example.com",
        None,
        500,
    );
    incoming(
        &store,
        &account.id,
        "message:older",
        "older@example.com",
        None,
        100,
    );
    let suggestions = harness.suggest(&account.id, "example.com", 10);
    assert_eq!(
        emails(&suggestions),
        vec!["newer@example.com", "older@example.com"]
    );

    // Same recency: capped frequency outranks the deterministic key, which
    // would otherwise put `a-rare` before `z-frequent`.
    for index in 0..3 {
        incoming(
            &store,
            &account.id,
            &format!("message:frequent-{index}"),
            "z-frequent@example.com",
            None,
            300,
        );
    }
    incoming(
        &store,
        &account.id,
        "message:rare",
        "a-rare@example.com",
        None,
        300,
    );
    let suggestions = harness.suggest(&account.id, "example.com", 10);
    let ordered = emails(&suggestions);
    assert!(
        ordered
            .iter()
            .position(|email| *email == "z-frequent@example.com")
            < ordered
                .iter()
                .position(|email| *email == "a-rare@example.com"),
        "higher bounded frequency wins: {ordered:?}"
    );
}

#[test]
fn active_account_breaks_ties() {
    let _guard = api_lock();
    let primary = account("account-a", "self@example.com");
    let secondary = account("account-b", "self-b@example.com");
    let harness = Harness::new(&[primary.clone(), secondary.clone()]);
    let store = harness.store();

    incoming(
        &store,
        &primary.id,
        "message:active",
        "active@example.com",
        None,
        100,
    );
    incoming(
        &store,
        &secondary.id,
        "message:other",
        "other@example.com",
        None,
        100,
    );

    let suggestions = harness.suggest(&secondary.id, "example.com", 10);
    assert_eq!(
        emails(&suggestions),
        vec!["other@example.com", "active@example.com"],
        "the sending account wins otherwise equal rows"
    );
}

#[test]
fn denied_contacts_still_returns_history() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Alice Saved",
        "alice@example.com",
    );
    incoming(
        &store,
        &account.id,
        "message:alice",
        "alice@example.com",
        Some("Alice Header"),
        100,
    );
    assert_eq!(
        suggestion(
            &harness.suggest(&account.id, "alice", 10),
            "alice@example.com"
        )
        .sources,
        vec![SuggestionSource::SavedContact, SuggestionSource::Incoming]
    );

    // Simulate contacts permission denial / source deletion.
    store
        .remove_contact_source(&account.id, &ContactSourceId::from("saved-source"))
        .unwrap();

    let suggestions = harness.suggest(&account.id, "alice", 10);
    let alice = suggestion(&suggestions, "alice@example.com");
    assert_eq!(
        alice.sources,
        vec![SuggestionSource::Incoming],
        "deleting a contact source must not delete independent learned evidence"
    );
    assert_eq!(
        alice.address.name.as_deref(),
        Some("Alice Header"),
        "learned history remains available after contact denial"
    );
}

#[test]
fn name_or_address_prefix_matches() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Alice Smith",
        "alice@example.com",
    );

    for query in [
        "ali",
        "Ali",
        "smi",
        "smith",
        "alice@exa",
        "example.c",
        "alice smi",
    ] {
        let suggestions = harness.suggest(&account.id, query, 10);
        assert_eq!(
            emails(&suggestions),
            vec!["alice@example.com"],
            "prefix query {query:?} must match the case-folded name/address tokens"
        );
    }
    assert!(
        harness.suggest(&account.id, "zzz", 10).is_empty(),
        "unmatched prefixes return no rows"
    );
}

#[test]
fn query_and_limit_are_bounded() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    for index in 0..25 {
        contact(
            &store,
            &account.id,
            "saved-source",
            "saved",
            &format!("Bound Person {index:02}"),
            &format!("bound{index:02}@example.com"),
        );
    }

    let capped = harness.suggest(&account.id, "bound", 10_000);
    assert_eq!(capped.len(), 20, "the public lookup caps the limit at 20");

    let empty = harness.suggest(&account.id, "bound", 0);
    assert!(empty.is_empty(), "a zero limit returns no suggestions");

    // 300 multi-byte characters plus a matching suffix: truncation must happen
    // on a character boundary and must not panic or split UTF-8.
    let long = format!("{}bound00", "é".repeat(300));
    let bounded = harness.suggest(&account.id, &long, 20);
    assert!(
        bounded.len() <= 20,
        "an over-long query is bounded without panicking"
    );

    let whitespace = harness.suggest(&account.id, "   ", 10);
    assert!(whitespace.is_empty());
}

#[test]
fn v11_database_backfills_the_projection_on_upgrade() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pige.db");
    let store = Store::open(&db_path).unwrap();
    store.migrate_to(11).unwrap();
    let account = account("account-a", "self@example.com");
    store.upsert_account(&account).unwrap();

    {
        let conn = Connection::open(&db_path).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO contact_sources (account_id, source_id, display_name)
             VALUES (?1, 'legacy-source', 'Legacy')",
            [&account.id.0],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO contact_emails
                 (account_id, source_id, remote_contact_id, address_key, email, display_name)
             VALUES (?1, 'legacy-source', 'legacy-1', 'contact@example.com',
                     'contact@example.com', 'Legacy Contact')",
            [&account.id.0],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO correspondence_evidence
                 (account_id, evidence_id, origin, fingerprint, observed_at, last_seen)
             VALUES (?1, 'message:legacy', 'history', NULL, 50, 50)",
            [&account.id.0],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO evidence_addresses
                 (account_id, evidence_id, address_key, role, email, display_name,
                  outgoing, trust, last_seen)
             VALUES (?1, 'message:legacy', 'history@example.com', 'from',
                     'history@example.com', 'History Person', 0, 'ordinary', 50)",
            [&account.id.0],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO contact_addresses
                 (account_id, address_key, email, display_name, trust, outgoing,
                  last_seen, evidence_count)
             VALUES (?1, 'history@example.com', 'history@example.com', 'History Person',
                     'ordinary', 0, 50, 1)",
            [&account.id.0],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), pige_core::SCHEMA_VERSION);

    let candidates = store
        .recipient_candidates(&account.id, "example.com", &["example.com".to_string()], 20)
        .unwrap();
    let mut found: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.address.email.as_str())
        .collect();
    found.sort();
    assert_eq!(
        found,
        vec!["contact@example.com", "history@example.com"],
        "the v12 backfill indexes both existing contacts and learned evidence"
    );
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.saved && candidate.address.email == "contact@example.com"),
        "pre-v12 contact sources default to the saved kind"
    );
}

#[test]
fn hundred_thousand_address_fixture_uses_projection_indexes() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pige.db");
    let store = Store::open(&db_path).unwrap();
    store.migrate().unwrap();
    let account = account("account-a", "self@example.com");
    store.upsert_account(&account).unwrap();
    seed_hundred_thousand_addresses(&db_path, &account);

    store.rebuild_recipient_index().unwrap();

    let matches = store
        .recipient_candidates(
            &account.id,
            "person099999@example.com",
            &["person099999@example.com".to_string()],
            20,
        )
        .unwrap();
    assert_eq!(matches.len(), 1, "the projection finds the seeded address");

    let plan = store
        .explain_recipient_search(
            &account.id,
            "person099999@example.com",
            &["person099999@example.com".to_string()],
            20,
        )
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("recipient_tokens"),
        "token prefix lookup must use the projection: {plan}"
    );
    assert!(
        plan.contains("SEARCH recipient_tokens"),
        "token lookup must be an indexed SEARCH, not a scan: {plan}"
    );
    assert!(
        plan.contains("recipient_index"),
        "ranking must read the projection table: {plan}"
    );
    assert!(
        !plan.contains("correspondence_evidence") && !plan.contains("evidence_addresses"),
        "a keystroke must never scan the evidence ledger: {plan}"
    );
}

#[test]
fn hundred_thousand_address_short_prefix_is_bounded() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pige.db");
    let store = Store::open(&db_path).unwrap();
    store.migrate().unwrap();
    let account = account("account-a", "self@example.com");
    store.upsert_account(&account).unwrap();
    seed_hundred_thousand_addresses(&db_path, &account);
    store.rebuild_recipient_index().unwrap();

    // Every fixture address starts with `p`, so this prefix matches all 100k
    // rows. The lookup must read an indexed top-N, not materialize the range.
    let cap = 20usize;
    let tokens = vec!["p".to_string()];
    let steps = store
        .recipient_search_vm_steps(&account.id, "p", &tokens, cap)
        .unwrap();
    assert!(
        steps < 5_000,
        "a one-character prefix must be bounded work, but consumed {steps} VM steps"
    );

    let candidates = store
        .recipient_candidates(&account.id, "p", &tokens, cap)
        .unwrap();
    assert_eq!(
        candidates.len(),
        cap,
        "the bounded page still fills the cap"
    );
    let ranked = rank_candidates(
        candidates,
        &account.id,
        "p",
        &HashSet::new(),
        &HashSet::new(),
        cap,
    );
    let emails: Vec<&str> = ranked
        .iter()
        .map(|suggestion| suggestion.address.email.as_str())
        .collect();
    assert_eq!(
        emails,
        (0..cap)
            .map(|index| format!("person{index:06}@example.com"))
            .collect::<Vec<_>>(),
        "the indexed top-N keeps the correct deterministic rank order"
    );

    let plan_lines = store
        .explain_recipient_search(&account.id, "p", &tokens, cap)
        .unwrap();
    let plan = plan_lines.join("\n");
    assert!(
        plan.contains("recipient_index_account_rank"),
        "the top-N must be driven by the rank index: {plan}"
    );
    assert!(
        !plan.contains("SCAN recipient_index"),
        "a short prefix must not scan the whole projection: {plan}"
    );
    assert!(
        plan_lines
            .iter()
            .filter(|line| line.starts_with("account-a prefix:"))
            .all(|line| !line.contains("USE TEMP B-TREE FOR ORDER BY")),
        "the prefix branch must not sort the whole range: {plan}"
    );
    assert!(
        !plan.contains("correspondence_evidence") && !plan.contains("evidence_addresses"),
        "a keystroke must never scan the evidence ledger: {plan}"
    );
}

fn seed_hundred_thousand_addresses(db_path: &std::path::Path, account: &Account) {
    // Seed the durable contact rows directly so the fixture stays fast; the
    // production rebuild then populates the search projection in one pass.
    let conn = Connection::open(db_path).unwrap();
    let tx = conn.unchecked_transaction().unwrap();
    tx.execute(
        "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
         VALUES (?1, 'fixture', 'Fixture', 'saved')",
        [&account.id.0],
    )
    .unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO contact_emails
                     (account_id, source_id, remote_contact_id, address_key, email, display_name)
                 VALUES (?1, 'fixture', ?2, ?3, ?4, ?5)",
            )
            .unwrap();
        for index in 0..100_000 {
            let email = format!("person{index:06}@example.com");
            insert
                .execute(rusqlite::params![
                    account.id.0,
                    format!("remote-{index}"),
                    format!("person{index:06}@example.com"),
                    email,
                    format!("Person {index:06}"),
                ])
                .unwrap();
        }
    }
    tx.commit().unwrap();
}

#[test]
fn every_query_token_is_required() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();

    // The first contact matches all nine query words; the second matches only
    // the first eight. A silently truncated token list would suggest both.
    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India",
        "nine@example.com",
    );
    contact(
        &store,
        &account.id,
        "saved-source",
        "saved",
        "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel",
        "eight@example.com",
    );

    let query = "alpha bravo charlie delta echo foxtrot golf hotel india";
    assert_eq!(
        emails(&harness.suggest(&account.id, query, 10)),
        vec!["nine@example.com"],
        "the ninth token must be part of the match, not silently ignored"
    );
}

#[test]
fn unknown_sending_account_returns_no_suggestions() {
    let _guard = api_lock();
    let account = account("account-a", "self@example.com");
    let harness = Harness::new(std::slice::from_ref(&account));
    let store = harness.store();
    incoming(
        &store,
        &account.id,
        "message:alice",
        "alice@example.com",
        Some("Alice"),
        100,
    );

    let unknown = api::suggest_recipients(RecipientQuery {
        sending_account: AccountId("not-configured".to_string()),
        text: "ali".to_string(),
        limit: 10,
    })
    .unwrap();
    assert!(
        unknown.is_empty(),
        "an unknown sending account must not receive another account's suggestions"
    );

    // The configured account still works.
    assert_eq!(
        emails(&harness.suggest(&account.id, "ali", 10)),
        vec!["alice@example.com"]
    );
}

#[test]
fn v12_database_gains_the_rank_index_on_upgrade() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("pige.db");
    let store = Store::open(&db_path).unwrap();
    store.migrate_to(12).unwrap();
    let account = account("account-a", "self@example.com");
    store.upsert_account(&account).unwrap();

    // Recreate the pre-v13 v12 shape: no `tier` column and no rank indexes.
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("ALTER TABLE recipient_index DROP COLUMN tier;")
            .unwrap();
        conn.execute(
            "INSERT INTO recipient_index
                 (account_id, address_key, email, display_name, email_folded, name_folded,
                  saved_contact, other_contact, outgoing, selected, incoming, low_trust_only,
                  last_seen, frequency)
             VALUES (?1, 'alice@example.com', 'alice@example.com', 'Alice',
                     'alice@example.com', 'alice', 1, 0, 0, 0, 0, 0, 100, 1)",
            [&account.id.0],
        )
        .unwrap();
    }

    store.migrate().unwrap();
    assert_eq!(
        store.schema_version().unwrap(),
        pige_core::SCHEMA_VERSION,
        "the database upgrades to the current schema"
    );

    let conn = Connection::open(&db_path).unwrap();
    let tier: i64 = conn
        .query_row(
            "SELECT tier FROM recipient_index WHERE address_key = 'alice@example.com'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        tier, 1,
        "existing saved rows are backfilled with their tier"
    );
    let rank_index: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'recipient_index_account_rank'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rank_index, 1, "the bounded rank index is created");
}
