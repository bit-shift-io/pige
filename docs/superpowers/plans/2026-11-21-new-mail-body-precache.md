# New-mail body precache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When a sync pass delivers new inbox mail, download those bodies immediately in the same pass (notify first, prefetch right after), so a notification tap opens an already-cached message on every platform — desktop, Android foreground, and Android's background `SyncWorker`.

**Architecture:** All sync funnels through `sync_one` in `rust/pige_core/src/api/mod.rs`, which returns the provider session alive and reports new mail as `Vec<NewMailSummary>`. We plumb the `MessageId` into that summary, then run a bounded best-effort body prefetch at the end of `sync_one`, reusing the open session and the existing `body::fetch_body_with` / `body::cache_body_content` pipeline. No shell or Kotlin changes.

**Tech Stack:** Rust, tokio, the provider abstraction (`ProviderSession`, `MailboxService`), SQLite via `Store`. Tests use the existing `FakeTransportFactory` + IMAP provider registry harness in `rust/pige_core/tests/api.rs`.

**Spec:** `docs/superpowers/specs/2026-11-21-new-mail-precache-and-icons-design.md` (Task 1 section)

## Global Constraints

- Notify first, prefetch after: the notification loop and `CoreEvent::SyncFinished` must fire before any body download starts (spec anti-goal: delaying notifications).
- Respect the `settings.prefetch_bodies` toggle (default on).
- Cap: `NEW_MAIL_PREFETCH_LIMIT: usize = 10` fetches per sync pass.
- Size gate: skip messages with `header.size > MAX_BODY_BYTES` (existing `pub(crate) const`, 10 MiB).
- Every prefetch failure is logged (warn for fetch errors, debug for gates) and swallowed — sync must never fail or error because of prefetch.
- Per-fetch gate: `scheduler.run_background` per message; `Ok(None)` (busy) stops the batch for this pass.
- Skip work for messages whose body is already cached (`store.body` is `Some`) or whose `body_state` is `'none'` (fetched before, unreadable).
- All new names in `rust/pige_core` follow the existing snake_case / doc-comment-first-line style.

## Review Focus

- **Notification timing regression:** a slow or stalled body download must not delay `NewMail` events or `SyncFinished`. Pinned by `sync_all_emits_new_mail_before_prefetch` (Task 2).
- **Morning burst (100+ new messages):** must fetch at most 10 bodies and still report every message / notify for every message. Pinned by `sync_all_caps_new_mail_prefetch_at_ten` (Task 2).
- **Oversized message (header.size > 10 MiB):** must appear in the list and notify, but never download a body. Pinned by `sync_all_skips_prefetch_for_oversized_messages` (Task 2).
- **Message moved/trashed between sync apply and prefetch:** must be skipped, not fetched into a stale folder. Pinned by the membership re-check inside `prefetch_new_mail` (Task 2; covered by the cap test's store assertions pattern — reviewer should re-read the revalidation block).
- **Background worker cancellation:** `ctx` passed to the helper is the sync context, which already carries the pass deadline and cancellation, so a stopped WorkManager run aborts body fetches. Reviewer confirms no fresh context is created inside the helper.

---

### Task 1: Plumb `message_id` through `NewMailSummary` and add `Store::body_state`

**Files:**
- Modify: `rust/pige_core/src/sync/engine.rs:35-41` (struct) and `:395-402` (constructor)
- Modify: `rust/pige_core/src/sync/provider.rs:254-261` (constructor)
- Modify: `rust/pige_core/src/store/bodies.rs` (getter)
- Test: `rust/pige_core/tests/store.rs`

**Interfaces:**
- Consumes: existing `NewMailSummary` fields; `store.set_body_state` (writes `messages.body_state`).
- Produces:
  - `NewMailSummary { pub message_id: MessageId, ... }` (all existing fields unchanged; struct stays `Clone`).
  - `Store::body_state(&self, id: &MessageId) -> Result<Option<String>, StoreError>` — the raw `messages.body_state` column, `None` when the message does not exist.

- [ ] **Step 1: Write the failing test** in `rust/pige_core/tests/store.rs` (follow the file's existing helper style for building a store):

```rust
#[test]
fn body_state_roundtrip_and_missing_message() {
    let store = /* open a test store as this file's other tests do */;
    let id = MessageId("m-body-state".into());
    assert_eq!(store.body_state(&id).unwrap(), None);
    store.set_body_state(&id, "none").unwrap();
    assert_eq!(store.body_state(&id).unwrap(), Some("none".to_string()));
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p pige_core --test store body_state_roundtrip`
Expected: FAIL — `no method named body_state found`

- [ ] **Step 3: Add the getter** to the `impl Store` block in `rust/pige_core/src/store/bodies.rs`:

```rust
/// Read a message's body state ('headers', 'none'), if the message exists.
pub fn body_state(&self, id: &MessageId) -> Result<Option<String>, StoreError> {
    self.connection()
        .query_row(
            "SELECT body_state FROM messages WHERE id = ?1",
            [&id.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}
```

- [ ] **Step 4: Add `pub message_id: MessageId`** to `NewMailSummary` (`rust/pige_core/src/sync/engine.rs:35`), populate it as `message_id: m.id.clone()` in the engine constructor (line ~395, where `m` is the `StoredMessage`) and as `message_id: id.clone()` in the provider constructor (`rust/pige_core/src/sync/provider.rs:254`, where `id` is the resolved `MessageId`). Run `cargo check -p pige_core` and fix any other construction sites the compiler reports (there should be none).

- [ ] **Step 5: Run the full pige_core suite** — the field addition must break no behavior.

Run: `cargo test -p pige_core`
Expected: PASS (including the new store test)

- [ ] **Step 6: Commit**

```bash
git add rust/pige_core/src/sync/engine.rs rust/pige_core/src/sync/provider.rs rust/pige_core/src/store/bodies.rs rust/pige_core/tests/store.rs
git commit -m "feat: carry message id in new-mail summaries"
```

---

### Task 2: Prefetch new-mail bodies at the end of the sync pass

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs` (helper near `prefetch_bodies`, ~line 2600; call site in `sync_one` after the notify loop / `SyncFinished` send, ~line 3950)
- Test: `rust/pige_core/tests/api.rs` (next to the existing `prefetch_*` tests, ~line 1440; reuse `Harness`, `input`, `folder`, `remote_message`, `inbox_transport`)

**Interfaces:**
- Consumes: `NewMailSummary.message_id` (Task 1), `Store::body_state` (Task 1), `body::fetch_body_with(&mut ProviderSession, &OperationContext, &RemoteMessageId)`, `body::cache_body_content(&Store, &MessageId, &BodyContent)`, `AccountScheduler::run_background(&OperationContext, F) -> Result<Option<T>, SchedulerError>`, `store.remote_message(&account_id, &message_id) -> Result<RemoteMessageId>`, `store.message_memberships(&message_id) -> Result<Vec<FolderId>>`, `lock_state()`, `INTERACTIVE_TIMEOUT` (unused — see Step 3 note).
- Produces: `async fn prefetch_new_mail(store: &Store, scheduler: &AccountScheduler, account: &Account, session: &mut ProviderSession, ctx: &OperationContext, mails: &[NewMailSummary])` — private, called only from `sync_one`.

- [ ] **Step 1: Write the failing tests** in `rust/pige_core/tests/api.rs`, after `prefetch_marks_unreadable_bodies_and_does_not_refetch`:

```rust
#[tokio::test]
async fn sync_all_prefetches_new_mail_bodies() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let account = api::add_account(input("A", "a@example.com"))
        .await
        .unwrap()
        .accounts[0]
        .id
        .clone();
    harness.factory.add_transport(
        account.clone(),
        FakeTransport {
            folders: vec![RemoteFolder {
                name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Inbox,
                selectable: true,
            }],
            messages: HashMap::from([(
                "INBOX".to_string(),
                vec![remote_message(1, "hello", "a@example.com")],
            )]),
            states: HashMap::from([(
                "INBOX".to_string(),
                FolderState { uidvalidity: 1, uidnext: 2, exists: 1 },
            )]),
            bodies: HashMap::from([(
                "INBOX".to_string(),
                HashMap::from([(Uid(1), b"Subject: hello\r\n\r\nwarm body".to_vec())]),
            )]),
            ..FakeTransport::default()
        },
    );

    let snapshot = api::sync_all().await.expect("sync succeeds");

    // The notification path is unchanged: NewMail fired for the arrival.
    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(
        transport.fetch_body_calls,
        vec![("INBOX".to_string(), Uid(1))],
        "the new message's body is downloaded in the same pass"
    );
    let store = harness.open_store();
    let inbox = &store.folders(&account).unwrap()[0];
    let stored = store.messages(&inbox.id, 10, None).unwrap();
    assert_eq!(stored.len(), 1);
    let body = store.body(&stored[0].id).unwrap().expect("body cached");
    assert!(body.plain_text.unwrap_or_default().contains("warm body"));
    let _ = snapshot;

    // A second pass has no new mail and must not refetch.
    api::sync_all().await.expect("second sync");
    let transport = harness.factory.transport(&account).await.unwrap();
    assert_eq!(transport.fetch_body_calls.len(), 1, "no refetch");
}

#[tokio::test]
async fn sync_all_skips_new_mail_prefetch_when_disabled() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let account = api::add_account(input("A", "a@example.com"))
        .await
        .unwrap()
        .accounts[0]
        .id
        .clone();
    harness.factory.add_transport(
        account.clone(),
        FakeTransport {
            folders: vec![RemoteFolder {
                name: "INBOX".to_string(),
                delimiter: Some("/".to_string()),
                kind: FolderKind::Inbox,
                selectable: true,
            }],
            messages: HashMap::from([(
                "INBOX".to_string(),
                vec![remote_message(1, "hello", "a@example.com")],
            )]),
            states: HashMap::from([(
                "INBOX".to_string(),
                FolderState { uidvalidity: 1, uidnext: 2, exists: 1 },
            )]),
            ..FakeTransport::default()
        },
    );
    api::save_settings(&api::Settings { prefetch_bodies: false, ..api::Settings::default() })
        .unwrap();

    api::sync_all().await.expect("sync succeeds");

    let transport = harness.factory.transport(&account).await.unwrap();
    assert!(
        transport.fetch_body_calls.is_empty(),
        "the toggle must suppress new-mail prefetch"
    );
}

#[tokio::test]
async fn sync_all_skips_prefetch_for_oversized_messages() {
    // Same seeding as sync_all_prefetches_new_mail_bodies but with
    // `remote_message(1, "big", "a@example.com")` mutated:
    // `msg.header.size = 11 * 1024 * 1024;` (bodies seeded normally).
    // After sync_all: fetch_body_calls is empty, the message is stored,
    // NewMail still fired (assert via `api::subscribe()` before sync),
    // and store.body is None.
}

#[tokio::test]
async fn sync_all_caps_new_mail_prefetch_at_ten() {
    // Seed 12 messages (uid 1..=12) with bodies for all of them; one sync.
    // Assert fetch_body_calls.len() == 10 and that the two unfetched
    // messages have store.body() == None while 10 have Some.
}

#[tokio::test]
async fn sync_all_prefetch_survives_unreadable_body() {
    // Seed one new message with NO entry in `bodies` (fetch returns empty
    // bytes). sync_all must succeed; the message's body_state becomes
    // "none" (assert with the Task 1 getter) and store.body() is None.
    // A second sync_all must not call fetch_body again.
}

#[tokio::test]
async fn sync_all_emits_new_mail_before_prefetch() {
    // Seed one new message with its body. Set `fetch_body_pause` to a
    // semaphore with zero permits, so the body fetch parks forever once it
    // starts. Subscribe before sync, run `api::sync_all()` on a spawned
    // task, and poll `events.try_recv()` (with short sleeps and a timeout)
    // until a `CoreEvent::NewMail` arrives. That proves the notification
    // fired while the body download was still parked. Then add one permit
    // to the pause semaphore, await the task to completion, and assert
    // `fetch_body_calls` contains the message (the prefetch did run).
}
```

Write the last two tests out fully following the first test's seeding pattern; the comments above fix their assertions exactly.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p pige_core --test api sync_all_prefetches_new_mail_bodies sync_all_skips_new_mail_prefetch_when_disabled sync_all_skips_prefetch_for_oversized_messages sync_all_caps_new_mail_prefetch_at_ten sync_all_prefetch_survives_unreadable_body sync_all_emits_new_mail_before_prefetch`
Expected: the first four FAIL (no body fetch happens today); `sync_all_prefetch_survives_unreadable_body` may PASS already (the on-demand path is not involved; empty body simply never caches) — its second-sync no-refetch assertion is the part that pins the new skip logic, so if it passes, verify it still passes after Step 4.

- [ ] **Step 3: Implement `prefetch_new_mail`** in `rust/pige_core/src/api/mod.rs`, directly after `prefetch_bodies`:

```rust
/// How many newly-arrived bodies one sync pass downloads. A burst still
/// notifies and lists every message; only the warm-open guarantee is capped.
const NEW_MAIL_PREFETCH_LIMIT: usize = 10;

/// Download the bodies the sync pass just delivered, so a notification tap
/// opens a cached message. Notify-first: the caller has already emitted
/// `NewMail` and `SyncFinished`. Best-effort — every failure is logged and
/// swallowed, and the sync `ctx` (deadline + cancellation) bounds each read.
async fn prefetch_new_mail(
    store: &Store,
    scheduler: &AccountScheduler,
    account: &Account,
    session: &mut ProviderSession,
    ctx: &OperationContext,
    mails: &[NewMailSummary],
) {
    // Same tolerance as the notify loop: a headless worker without global
    // state has no toggle to read, so it proceeds.
    let enabled = lock_state()
        .as_ref()
        .is_none_or(|state| state.config_doc.settings.prefetch_bodies);
    if !enabled || mails.is_empty() {
        return;
    }
    for mail in mails.iter().take(NEW_MAIL_PREFETCH_LIMIT) {
        // Revalidate against the store: the message may have been moved,
        // trashed, or already opened (and thus cached) while the pass ran.
        let Ok(Some(stored)) = store.message(&mail.message_id) else { continue };
        if stored.header.size > MAX_BODY_BYTES {
            continue;
        }
        let Ok(memberships) = store.message_memberships(&mail.message_id) else { continue };
        if !memberships.contains(&mail.folder_id) {
            continue;
        }
        match store.body(&mail.message_id) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(error) => {
                log::warn!("new-mail prefetch cache check: {error}");
                continue;
            }
        }
        if store.body_state(&mail.message_id).ok().flatten().as_deref() == Some("none") {
            continue;
        }
        // A message with no provider identity cannot be prefetched.
        let Ok(remote) = store.remote_message(&account.id, &mail.message_id) else {
            continue;
        };
        // Reborrow: the `&mut` must survive across loop iterations.
        let operation = async { body::fetch_body_with(&mut *session, ctx, &remote).await };
        let content = match scheduler.run_background(ctx, operation).await {
            Ok(Some(Ok(content))) => content,
            // Busy gate: the user is interacting; stop this pass's batch.
            Ok(Some(Err(error))) => {
                log::warn!("new-mail prefetch {}: {error}", mail.message_id);
                continue;
            }
            Ok(None) => break,
            Err(error) => {
                log::debug!("new-mail prefetch deferred: {error}");
                break;
            }
        };
        body::cache_body_content(store, &mail.message_id, &content);
    }
}
```

Implementation notes the signature and tests leave open, decided here:
- Iterate `mails` in report order (the engine already sorts newest-first) and let `take(NEW_MAIL_PREFETCH_LIMIT)` cap *fetch attempts*, not summaries.
- Reuse the sync `ctx` — it carries the pass deadline and WorkManager cancellation; do NOT create a fresh context.
- `cache_body_content` already rewrites the snippet and stores attachment descriptors; no extra store writes.

- [ ] **Step 4: Call it from `sync_one`** in `rust/pige_core/src/api/mod.rs`, after the `CoreEvent::SyncFinished` send (the last statement of the function), while `session` is still alive:

```rust
if !report.new_inbox.is_empty() {
    prefetch_new_mail(
        store,
        scheduler,
        account,
        &mut session,
        &ctx,
        &report.new_inbox,
    )
    .await;
}
```

- [ ] **Step 5: Run the new tests**

Run: `cargo test -p pige_core --test api sync_all_`
Expected: PASS, including the pre-existing `sync_all_*` tests.

- [ ] **Step 6: Run the whole workspace suite and the linters the repo gates on**

Run: `cargo test -p pige_core && cargo clippy -p pige_core --all-targets -- -D warnings && cargo fmt --check`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/api.rs
git commit -m "feat: prefetch bodies for newly-arrived inbox mail"
```
