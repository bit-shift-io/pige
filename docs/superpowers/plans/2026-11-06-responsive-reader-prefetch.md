# Responsive reader, background prefetch, non-blocking account add — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Opening a conversation shows the reader instantly with a spinner, the newest tap always wins while superseded downloads still cache to disk, Inbox mail is prefetched behind a setting, and adding an account no longer blocks on its first sync.

**Architecture:** `api::open_conversation` becomes two-phase — it installs a cached-header `ConversationDetail` whose newest body is a new `MessageBody::Loading`, emits a snapshot, *then* fetches; a per-state `open_generation` makes only the newest open eligible to install, while superseded fetches still write their bodies to SQLite. A new `api::prefetch_bodies()` walks a store query for the newest message of each uncached Inbox conversation and caches bodies through the same path an open uses. The shell renders `Loading` with a new spinner and triggers prefetch after user-facing syncs.

**Tech Stack:** Rust edition 2024, iced (wgpu), tokio, rusqlite (SQLite WAL). Tests use `FakeTransport`, `FakeTransportFactory`, and the `App::for_test()` shell harness.

**Spec:** `docs/superpowers/specs/2026-11-06-responsive-reader-prefetch-design.md`

## Global Constraints

- Rust edition 2024. Gates: `cargo test --workspace` and `cargo clippy --workspace --all-targets` are clean (from `rust/`).
- `pige_core` has no iced and no UI; all logic lands there and is testable without a renderer.
- The global state mutex is held only around synchronous SQLite work; never across a network `.await`. `SYNC_LOCK` is held across network awaits; prefetch uses its own `PREFETCH_LOCK`.
- Settings live in the config **document** (`AppState.config_doc.settings`), never the legacy `meta` row (`settings::load_settings(store)`), which migration deletes.
- `MAX_BODY_BYTES = 10 * 1024 * 1024` (existing). `PREFETCH_LIMIT = 50` conversations per account. `PREFETCH_CONCURRENCY = 2` connections.
- `Settings` is `#[serde(default)]`; a new field must default without breaking an existing `pige.json`.
- Prefetch is Inbox only, unread-first then newest-first, **one body per conversation** (the newest message).
- Copy: the setting is labelled **"Download in advance"**.

## Review Focus

These are the behaviors the spec implies that a shipped mistake would most likely hit. Each has a test in the owning task.

1. **Three or more rapid taps.** Only the newest conversation installs; every superseded body is still cached. (Task 3)
2. **Leaving while a fetch is in flight** — Back, folder switch, or archive/trash/move/delete. The completion must not resurrect the old conversation. (Task 3)
3. **A body that parses to no readable text.** Fetched once, marked `body_state = 'none'`, never re-selected by prefetch. (Task 6)
4. **Size boundary.** A body exactly at `MAX_BODY_BYTES` is fetched; one byte over is never fetched or cached. (Task 6, plus the existing over-cap open test)
5. **Prefetch offline / no transport.** Gives up quietly with no panic and leaves app state intact. (Task 6)
6. **Account add with rejected credentials.** The quick check fails and the edit dialog stays open. (Task 8)

---

### Task 1: `prefetch_bodies` setting and its toggle

**Files:**
- Modify: `rust/pige_core/src/settings.rs`
- Modify: `rust/pige/src/message.rs`
- Modify: `rust/pige/src/update.rs`
- Modify: `rust/pige/src/screens/settings.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `Settings.prefetch_bodies: bool` (default `true`); `Message::SettingsPrefetchToggled(bool)`.

- [ ] **Step 1: Write the failing core test** in `settings.rs`'s `mod tests`:

```rust
#[test]
fn prefetch_defaults_on_and_an_old_document_still_loads() {
    assert!(Settings::default().prefetch_bodies);
    // A pre-upgrade document has no `prefetch_bodies` key.
    let old = r#"{"sync_minutes":15,"notifications":true,"background_sync":true,"theme":"system"}"#;
    let parsed: Settings = serde_json::from_str(old).unwrap();
    assert!(parsed.prefetch_bodies, "missing key must default to on");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p pige_core settings::tests::prefetch_defaults_on_and_an_old_document_still_loads`
Expected: FAIL — no field `prefetch_bodies`.

- [ ] **Step 3: Add the field**

Add `pub prefetch_bodies: bool,` to `Settings` with a doc comment, and `prefetch_bodies: true` in `Settings::default`.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p pige_core settings::tests`
Expected: PASS.

- [ ] **Step 5: Write the failing shell test** in `pige/src/update.rs`'s `mod tests`:

```rust
#[test]
fn prefetch_toggle_updates_settings() {
    let mut app = App::for_test();
    let _ = app.update(Message::SettingsPrefetchToggled(false));
    assert!(!app.settings.prefetch_bodies);
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p pige prefetch_toggle_updates_settings`
Expected: FAIL — no variant `SettingsPrefetchToggled`.

- [ ] **Step 7: Implement the variant, arm, and UI**

- `message.rs`: add `SettingsPrefetchToggled(bool)` beside `SettingsBackgroundSyncToggled`.
- `update.rs`: add `Message::SettingsPrefetchToggled(on) => self.settings.prefetch_bodies = on,` beside the background-sync arm.
- `screens/settings.rs`: add a `prefetch_section(app)` modelled on `background_sync_section` (On/Off chips wired to `SettingsPrefetchToggled`), with the label "Download in advance" and supporting text "Prefetch Inbox message bodies so mail opens instantly. Turn off on metered or slow connections." Insert it after `background_sync_section(app)` in the screen body.

- [ ] **Step 8: Run it to verify it passes**

Run: `cargo test -p pige prefetch_toggle_updates_settings`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add rust/pige_core/src/settings.rs rust/pige/src/message.rs rust/pige/src/update.rs rust/pige/src/screens/settings.rs
git commit -m "feat: add Download in advance setting for body prefetch"
```

---

### Task 2: `MessageBody::Loading` and the two-phase open

**Files:**
- Modify: `rust/pige_core/src/api/snapshot.rs`
- Modify: `rust/pige_core/src/api/mod.rs`
- Modify: `rust/pige_core/src/transport/fake.rs`
- Modify: `rust/pige/src/screens/reader.rs` (minimal arm so the workspace compiles; upgraded in Task 5)
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `MessageBody::Loading`; `FakeTransport.fetch_body_started: Option<Arc<Semaphore>>` and `fetch_body_pause: Option<Arc<Semaphore>>`; helper `fn cache_fetched_body(store: &Store, message_id: &MessageId, raw: &[u8]) -> Option<MailDocument>`.

- [ ] **Step 1: Add the variant and the fake hooks**

- `snapshot.rs`: add `Loading,` to `MessageBody` with doc comment "A body fetch is in flight for this message.".
- `fake.rs`: add `pub fetch_body_started: Option<Arc<Semaphore>>,` and `pub fetch_body_pause: Option<Arc<Semaphore>>,` to `FakeTransport`, and in `fetch_body` (after recording the call, before returning bytes) mirror the `fetch_headers` hooks: `if let Some(started) = &self.fetch_body_started { started.add_permits(1); }` then `if let Some(pause) = &self.fetch_body_pause { let _ = pause.acquire().await; }`.
- `reader.rs`: add a temporary `Some(MessageBody::Loading) => {}` arm to the `match &latest.body` in `conversation_body` so the workspace builds. (Task 5 replaces it.)

Verify: `cargo build --workspace` — expected: succeeds.

- [ ] **Step 2: Write the failing test** in `rust/pige_core/tests/api.rs`:

```rust
#[tokio::test]
async fn open_conversation_shows_loading_before_the_body_arrives() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let added = api::add_account(input("Personal", "ada@example.com")).await.unwrap();
    let account_id = added.accounts[0].id.clone();

    let store = harness.open_store();
    let inbox = folder("inbox", &account_id, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox).unwrap();
    let mut message = stored_message(&inbox, 1);
    message.thread_id = ThreadId("thread-loading".to_string());
    store.upsert_message(&message).unwrap();

    let started = Arc::new(Semaphore::new(0));
    let pause = Arc::new(Semaphore::new(0));
    let raw = b"Content-Type: text/plain\r\n\r\nhello body\r\n".to_vec();
    let transport = FakeTransport {
        folders: vec![RemoteFolder { name: "INBOX".into(), delimiter: Some("/".into()), kind: FolderKind::Inbox }],
        bodies: HashMap::from([("INBOX".to_string(), HashMap::from([(Uid(1), raw)]))]),
        fetch_body_started: Some(started.clone()),
        fetch_body_pause: Some(pause.clone()),
        ..FakeTransport::default()
    };
    harness.factory.add_transport(account_id.clone(), transport);

    let thread = ThreadId("thread-loading".to_string());
    let open = tokio::spawn(async move { api::open_conversation(&thread).await });

    // Wait until the fetch is in flight, then read the installed snapshot.
    let _ = started.acquire().await.unwrap();
    let body = api::snapshot()
        .selected_conversation
        .and_then(|detail| detail.messages.into_iter().next())
        .and_then(|message| message.body);
    assert_eq!(body, Some(MessageBody::Loading));

    // Release the fetch; the resolved body replaces it.
    pause.add_permits(1);
    let snapshot = open.await.unwrap().unwrap();
    assert!(matches!(
        snapshot.selected_conversation.unwrap().messages[0].body,
        Some(MessageBody::Document(_))
    ));
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p pige_core --test api open_conversation_shows_loading_before_the_body_arrives`
Expected: FAIL — the snapshot has `None` (or the assertion on `Loading` fails) because the current code installs nothing until the fetch returns.

- [ ] **Step 4: Implement the two-phase open**

In `api/mod.rs`:

- Extract `fn cache_fetched_body(store: &Store, message_id: &MessageId, raw: &[u8]) -> Option<MailDocument>`: run `extract_attachments` + `store.store_attachments`, `extract_body_parts`, build `Body { .., fetched_at: now_unix() }`, `document_from_body`, `store.upsert_body` when a document exists, and return the document. Rewrite `resolve_body` to call it (keeping its `too_large`/cached precedence).
- Rewrite `open_conversation`:
  - Phase 1 (under the state lock): read messages/account/folder/cached/too_large as today; build `ConversationDetail` via `build_conversation_detail` with the newest body set to the cached `Document` if present, else `TooLarge` if over cap, else `Loading`; set `state.selected_conversation`; clone `config`; `sender`. Drop the lock, `sender.send(CoreEvent::SnapshotChanged)`, and keep the phase-1 snapshot.
  - Phase 2: only when `cached.is_none() && !too_large`, `fetch_body_bytes(...).await`. On success, cache under a fresh lock via `cache_fetched_body`; rebuild the detail with the resolved body (`Document`, or `None`); install it; emit `SnapshotChanged`. On fetch error, log and install `None` for the newest body. Return the final `state.build_snapshot()`.

- [ ] **Step 5: Run it to verify it passes**

Run: `cargo test -p pige_core --test api open_conversation_shows_loading_before_the_body_arrives`
Expected: PASS.

- [ ] **Step 6: Run the whole suite and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, clean. (Existing `open_conversation_fetches_and_caches_the_latest_body` and `oversized_body_is_not_fetched_or_cached` must still pass unchanged.)

```bash
git add rust/pige_core/src/api rust/pige_core/src/transport/fake.rs rust/pige/src/screens/reader.rs rust/pige_core/tests/api.rs
git commit -m "feat(core): open conversations into a Loading body, then resolve"
```

---

### Task 3: Newest-wins open generation and invalidation

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs`
- Modify: `rust/pige_core/src/api/mutations.rs`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Consumes: `MessageBody::Loading`, the two-phase `open_conversation` (Task 2).
- Produces: `AppState.open_generation: u64`; `AppState::invalidate_open(&mut self)`.

- [ ] **Step 1: Write the failing ordering test** in `tests/api.rs`:

```rust
#[tokio::test]
async fn a_slow_open_does_not_overwrite_a_newer_one() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    let a = api::add_account(input("A", "a@example.com")).await.unwrap().accounts[0].id.clone();
    let b = api::add_account(input("B", "b@example.com")).await.unwrap().accounts[0].id.clone();

    let store = harness.open_store();
    let inbox_a = folder("a-inbox", &a, "INBOX", FolderKind::Inbox, 0);
    let inbox_b = folder("b-inbox", &b, "INBOX", FolderKind::Inbox, 0);
    store.upsert_folder(&inbox_a).unwrap();
    store.upsert_folder(&inbox_b).unwrap();
    // `seed_conversation` gives each row a folder-scoped id so the two uid-1
    // messages do not collide on the messages primary key.
    seed_conversation(&store, &inbox_a, 1, 1, "A", "thread-a");
    seed_conversation(&store, &inbox_b, 1, 1, "B", "thread-b");

    // Account A's fetch blocks; account B's returns at once.
    let started = Arc::new(Semaphore::new(0));
    let pause = Arc::new(Semaphore::new(0));
    harness.factory.add_transport(a.clone(), FakeTransport {
        folders: vec![RemoteFolder { name: "INBOX".into(), delimiter: Some("/".into()), kind: FolderKind::Inbox }],
        bodies: HashMap::from([("INBOX".into(), HashMap::from([(Uid(1), b"Subject: A\r\n\r\nalpha".to_vec())]))]),
        fetch_body_started: Some(started.clone()),
        fetch_body_pause: Some(pause.clone()),
        ..FakeTransport::default()
    });
    harness.factory.add_transport(b.clone(), FakeTransport {
        folders: vec![RemoteFolder { name: "INBOX".into(), delimiter: Some("/".into()), kind: FolderKind::Inbox }],
        bodies: HashMap::from([("INBOX".into(), HashMap::from([(Uid(1), b"Subject: B\r\n\r\nbravo".to_vec())]))]),
        ..FakeTransport::default()
    });

    let thread_a = ThreadId("thread-a".into());
    let slow = tokio::spawn(async move { api::open_conversation(&thread_a).await });
    let _ = started.acquire().await.unwrap();

    // The newer open finishes first and wins.
    let fast = api::open_conversation(&ThreadId("thread-b".into())).await.unwrap();
    assert_eq!(fast.selected_conversation.unwrap().thread_id, ThreadId("thread-b".into()));

    // The slow one lands late and must not clobber the newer selection.
    pause.add_permits(1);
    let late = slow.await.unwrap().unwrap();
    assert_eq!(late.selected_conversation.unwrap().thread_id, ThreadId("thread-b".into()));

    // ...but its body was still cached.
    let store = harness.open_store();
    assert!(store.body(&MessageId("a-inbox-message-1".into())).unwrap().is_some());
}
```

(`seed_conversation` sets `id = format!("{}-message-{uid}", folder.id.0)`, so the assertion above uses `a-inbox-message-1`.)

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p pige_core --test api a_slow_open_does_not_overwrite_a_newer_one`
Expected: FAIL — the late completion installs thread A.

- [ ] **Step 3: Write the failing invalidation test**

```rust
#[tokio::test]
async fn leaving_invalidates_an_in_flight_open() {
    // Same setup as the ordering test, single account with A's transport paused.
    // Start `open_conversation(thread-a)`, wait for `started`, then:
    api::select_unified().await.unwrap();
    // Release and await the open.
    pause.add_permits(1);
    let late = slow.await.unwrap().unwrap();
    assert!(late.selected_conversation.is_none(), "a stale open must not resurrect the selection");
}
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test -p pige_core --test api leaving_invalidates_an_in_flight_open`
Expected: FAIL — the late completion reinstalls thread A.

- [ ] **Step 5: Implement the generation guard**

- Add `open_generation: u64` to `AppState`, initialised `0` in `init`.
- Add `fn invalidate_open(&mut self) { self.open_generation = self.open_generation.wrapping_add(1); }`.
- In `open_conversation` phase 1, increment and capture `let generation = { state.invalidate_open(); state.open_generation };`.
- In phase 2's completion, capture `state.open_generation` and only install the resolved body/rebuild the detail/emit `SnapshotChanged` when `generation == current`. On a stale completion, still `cache_fetched_body`, then return `state.build_snapshot()` without touching `selected_conversation`.

- [ ] **Step 6: Implement the invalidation call sites**

Call `state.invalidate_open()` immediately before each `state.selected_conversation = None;` and wherever the selection is replaced outside an open:

- `api/mod.rs`: `select_folder`, `select_unified`, `load_older` (folder-change branch), `remove_account`, `restore_config`, `factory_reset`.
- `api/mutations.rs`: `remove_optimistically` (covers archive/trash/move/delete).

- [ ] **Step 7: Run both tests to verify they pass**

Run: `cargo test -p pige_core --test api open`
Expected: PASS, including all earlier open tests.

- [ ] **Step 8: Run the workspace and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, clean.

```bash
git add rust/pige_core/src/api rust/pige_core/tests/api.rs
git commit -m "fix(core): newest open wins and leaving invalidates an in-flight fetch"
```

---

### Task 4: `sync_one` reads notifications from the config document

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Consumes: `AppState.config_doc` (existing).
- Produces: nothing new.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn sync_does_not_notify_when_notifications_are_off() {
    let _guard = TEST_LOCK.lock().await;
    let harness = Harness::new();
    api::save_settings(&Settings { notifications: false, ..Settings::default() }).unwrap();

    let added = api::add_account(input("Personal", "ada@example.com")).await.unwrap();
    let account_id = added.accounts[0].id.clone();
    // Seed one cached inbox message, then script a transport that reports a new
    // message so `report.new_inbox` is non-empty. Use `inbox_transport` with a
    // `states` uidnext beyond the cached UID, or the same seams the existing
    // sync tests use to produce new mail.
    // (See `sync.rs`/`api.rs` tests that already assert on `FakeNotifier`.)

    let _ = api::sync_account(&account_id).await;
    assert!(harness.notifier.notifications().is_empty(), "notifications were off");
}
```

Reuse the new-mail seeding from an existing notifier test in `tests/api.rs` rather than inventing one; the point is the assertion.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p pige_core --test api sync_does_not_notify_when_notifications_are_off`
Expected: FAIL — a notification is recorded because `sync_one` read the deleted `meta` row.

- [ ] **Step 3: Fix the read**

In `sync_one`, replace `let notify_new_mail = crate::settings::load_settings(store).notifications;` with a one-time read of the live document under the state lock (default true if uninitialised):

```rust
let notify_new_mail = {
    let guard = lock_state();
    guard.as_ref().is_none_or(|state| state.config_doc.settings.notifications)
};
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p pige_core --test api sync_does_not_notify_when_notifications_are_off`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/api.rs
git commit -m "fix(core): honor the notifications setting during sync"
```

---

### Task 5: Spinner component and reader rendering

**Files:**
- Create: `rust/pige/src/components/spinner.rs`
- Modify: `rust/pige/src/components/mod.rs`
- Modify: `rust/pige/src/screens/reader.rs`
- Modify: `rust/pige/src/message.rs`
- Modify: `rust/pige/src/update.rs`
- Modify: `rust/pige/src/app.rs`
- Modify: `rust/pige/Cargo.toml`

**Interfaces:**
- Consumes: `MessageBody::Loading` (Task 2).
- Produces: `pub fn spinner(rotation: f32) -> iced::Element<'static, Message>`; `reader::ReaderBody` and `pub fn reader_body_kind(message: &MessageSummary) -> ReaderBody`; `Message::Spin`; `App.spin_phase: u32`.

- [ ] **Step 1: Enable the canvas feature**

In `rust/pige/Cargo.toml`, add `"canvas"` to the iced `features` list. Run `cargo check -p pige` — expected: succeeds and `Cargo.lock` updates.

- [ ] **Step 2: Write the failing pure test** in `screens/reader.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn with_body(body: Option<MessageBody>) -> MessageSummary { /* build a MessageSummary with that body */ }

    #[test]
    fn body_kind_distinguishes_loading_from_missing() {
        assert_eq!(reader_body_kind(&with_body(Some(MessageBody::Loading))), ReaderBody::Loading);
        assert_eq!(reader_body_kind(&with_body(None)), ReaderBody::Missing);
        assert_eq!(reader_body_kind(&with_body(Some(MessageBody::TooLarge))), ReaderBody::TooLarge);
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p pige body_kind_distinguishes_loading_from_missing`
Expected: FAIL — no `ReaderBody`/`reader_body_kind`.

- [ ] **Step 4: Implement the enum and matcher**

Add to `screens/reader.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderBody { Document, TooLarge, Loading, Missing }

pub fn reader_body_kind(message: &MessageSummary) -> ReaderBody { /* match message.body */ }
```

Rewire `conversation_body`'s match to switch on `reader_body_kind(latest)`: `Loading` pushes a centred `components::spinner(app.spin_phase as f32)`, the others keep their current rendering.

- [ ] **Step 5: Implement the spinner**

`components/spinner.rs`: a `Spinner { rotation: f32 }` implementing `iced::widget::canvas::Program<Message>` that strokes a ~270° arc in `theme::active().on_surface_variant` at a palette-derived size; `pub fn spinner<'a>(rotation: f32) -> iced::Element<'a, Message>` wraps `canvas::Canvas::new(Spinner { rotation })`. Export it from `components/mod.rs`.

- [ ] **Step 6: Add the spin tick**

- `app.rs`: add `pub(crate) spin_phase: u32` to `App`, initialised `0` in both `App::new` and `App::for_test`.
- `message.rs`: add `Spin`.
- `update.rs`: `Message::Spin => self.spin_phase = self.spin_phase.wrapping_add(1),` and in `subscription()` batch in `iced::time::every(Duration::from_millis(80)).map(|_| Message::Spin)` only when a `Loading` body is present (`self.snapshot.selected_conversation.as_ref().and_then(|d| d.messages.first()).and_then(|m| m.body.as_ref()) == Some(&MessageBody::Loading)`), else `Subscription::none()`.

- [ ] **Step 7: Run the tests and commit**

Run: `cargo test -p pige body_kind_distinguishes_loading_from_missing && cargo check --workspace`
Expected: PASS.

```bash
git add rust/pige/Cargo.toml rust/pige/Cargo.lock rust/pige/src/components rust/pige/src/screens/reader.rs rust/pige/src/message.rs rust/pige/src/update.rs rust/pige/src/app.rs
git commit -m "feat(ui): show a spinner while a message body loads"
```

---

### Task 6: Prefetch candidate query, body marker, and `prefetch_bodies`

**Files:**
- Modify: `rust/pige_core/src/store/messages.rs`
- Modify: `rust/pige_core/src/store/bodies.rs`
- Modify: `rust/pige_core/src/store/mod.rs` (re-export `PrefetchCandidate`)
- Modify: `rust/pige_core/src/api/mod.rs`
- Test: `rust/pige_core/tests/store.rs`, `rust/pige_core/tests/api.rs`

**Interfaces:**
- Consumes: `cache_fetched_body` (Task 2), `Settings.prefetch_bodies` (Task 1).
- Produces: `store::PrefetchCandidate { account_id: AccountId, folder_id: FolderId, message_id: MessageId, uid: Uid }`; `Store::prefetch_candidates(&self, max_bytes: u32, per_account: u32) -> Result<Vec<PrefetchCandidate>, StoreError>`; `Store::set_body_state(&self, id: &MessageId, state: &str) -> Result<(), StoreError>`; `api::prefetch_bodies() -> ()`; constants `PREFETCH_LIMIT`, `PREFETCH_CONCURRENCY`, `static PREFETCH_LOCK`.

- [ ] **Step 1: Write the failing store test** in `tests/store.rs`:

Seed one account with several Inbox conversations and one Sent conversation. For conversation P: newest message unread, no body, size 100. Conversation Q: newest message read, no body, size 100. Conversation V (Inbox): newest message size exactly `MAX_BODY_BYTES` (boundary, must be included). Conversation R (Inbox): newest message size `MAX_BODY_BYTES + 1`. Conversation S (Inbox): newest has a stored body. Conversation T (Inbox): newest `body_state = 'none'`. Conversation U (Inbox): two messages, newest already cached, older not.

```rust
#[test]
fn prefetch_candidates_are_inbox_newest_uncached_unread_first() {
    let candidates = store.prefetch_candidates(10 * 1024 * 1024, 50).unwrap();
    let uids: Vec<u32> = candidates.iter().map(|c| c.uid.0).collect();
    // Only P (unread), V (boundary), and Q (read); P before V before Q.
    assert_eq!(uids, vec![/* P uid */, /* V uid */, /* Q uid */]);
}
```

Also assert: over-cap (R), cached (S), `body_state='none'` (T), older replies (U's older uid), and Sent are all absent.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p pige_core --test store prefetch_candidates_are_inbox_newest_uncached_unread_first`
Expected: FAIL — no method.

- [ ] **Step 3: Implement `prefetch_candidates` and `set_body_state`**

`messages.rs`: add the struct and the query — rank messages per `(account_id, thread_id)` over `folder_id IN (SELECT id FROM folders WHERE kind='inbox')`, keep `thread_rank = 1`, filter `size <= ?max AND body_state = 'headers' AND id NOT IN (SELECT message_id FROM bodies)`, then `ROW_NUMBER() OVER (PARTITION BY account_id ORDER BY unread DESC, date DESC, uid DESC) <= ?per_account`, ordered `account_id, unread DESC, date DESC`.

`bodies.rs`: add `set_body_state` (`UPDATE messages SET body_state = ?1 WHERE id = ?2`) and call it from `resolve_body`/`cache_fetched_body` when a successful fetch yields no document.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test -p pige_core --test store prefetch_candidates`
Expected: PASS.

- [ ] **Step 5: Write the failing API tests** in `tests/api.rs`:

```rust
#[tokio::test]
async fn prefetch_caches_inbox_bodies_when_enabled() { /* setting on; one uncached inbox message; call api::prefetch_bodies().await; assert store.body(id).is_some() and factory transport.fetch_body_calls has it once */ }

#[tokio::test]
async fn prefetch_fetches_nothing_when_disabled() { /* api::save_settings(prefetch_bodies:false); call prefetch; assert fetch_body_calls empty */ }

#[tokio::test]
async fn prefetch_marks_unreadable_bodies_and_does_not_refetch() { /* body bytes parse to no text; first prefetch sets body_state 'none'; second prefetch makes no fetch_body call */ }

#[tokio::test]
async fn prefetch_gives_up_quietly_without_a_transport() { /* no transport registered; prefetch_bodies().await returns without panic; snapshot still readable */ }
```

- [ ] **Step 6: Run them to verify they fail**

Run: `cargo test -p pige_core --test api prefetch_`
Expected: FAIL — no `api::prefetch_bodies`.

- [ ] **Step 7: Implement `api::prefetch_bodies`**

In `api/mod.rs`: add the constants and lock. Implementation — read `config_doc.settings.prefetch_bodies` and call `store.prefetch_candidates(MAX_BODY_BYTES, PREFETCH_LIMIT)` in a short state-lock scope, then drop the lock; return if the setting is off. Take `PREFETCH_LOCK.try_lock()` (return if held). Open a dedicated `Store::open(database_path(&config.data_dir))` for the whole prefetch (as `sync_folder_inner` does) so no state lock is held across a fetch. Group candidates by `(account_id, folder_id)` and process groups with `futures::stream::iter(groups).map(|group| async { ... }).buffer_unordered(PREFETCH_CONCURRENCY).collect::<Vec<_>>().await`. Per group: locate the account and folder, `config.transport.imap(&account).await`, `select`, then for each candidate re-check the dedicated store's `body(&message_id)` is still `None`, `fetch_body(uid).await`, and `cache_fetched_body`. Log and skip per-message/per-group errors; emit nothing.

- [ ] **Step 8: Run them to verify they pass**

Run: `cargo test -p pige_core --test api prefetch_ && cargo test -p pige_core --test store prefetch_`
Expected: PASS.

- [ ] **Step 9: Run the workspace and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, clean.

```bash
git add rust/pige_core/src/store rust/pige_core/src/api/mod.rs rust/pige_core/tests
git commit -m "feat(core): prefetch Inbox message bodies behind a setting"
```

---

### Task 7: Trigger prefetch after user-facing syncs

**Files:**
- Modify: `rust/pige/src/update.rs` (helper + sync arms)
- Modify: `rust/pige/src/app.rs` (startup sync)
- Test: none new — this is thin wiring over the already-tested `api::prefetch_bodies`; covered indirectly by the core suite

**Interfaces:**
- Consumes: `api::prefetch_bodies()` (Task 6).
- Produces: `async fn synced_with_prefetch<F>(future: F) -> Result<ModelSnapshot, String> where F: Future<Output = Result<ModelSnapshot, String>>` in `update.rs`.

- [ ] **Step 1: Implement the helper**

```rust
/// Run a sync, then prefetch recent Inbox bodies when it succeeded. A failed
/// sync (offline) skips prefetch rather than attempting pointless network work.
pub(crate) async fn synced_with_prefetch<F>(future: F) -> Result<ModelSnapshot, String>
where
    F: Future<Output = Result<ModelSnapshot, String>>,
{
    let result = future.await;
    if result.is_ok() {
        api::prefetch_bodies().await;
    }
    result
}
```

- [ ] **Step 2: Route every user-facing sync through it**

Replace the direct `api::sync_all()`/`api::sync_account(...)` calls in these paths with `synced_with_prefetch(...)`:
- `App::new` startup sync (`app.rs`) — wrap `api::sync_all()`.
- `update.rs`: `Message::SyncNow | Message::Refresh`.
- The account-save background sync added in Task 8.

Leave `select_folder`'s internal `sync_folder` and the other-folder refresh inside `sync_all` alone (they are not user "sync now" paths and prefetch runs after the top-level sync anyway).

- [ ] **Step 3: Verify build and existing tests**

Run: `cargo test -p pige && cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add rust/pige/src/update.rs rust/pige/src/app.rs
git commit -m "feat: prefetch recent Inbox bodies after a successful sync"
```

---

### Task 8: Non-blocking account add

**Files:**
- Modify: `rust/pige/src/message.rs`
- Modify: `rust/pige/src/update.rs`
- Modify: `rust/pige/src/screens/accounts.rs` (test only)
- Test: `rust/pige/src/update.rs`, `rust/pige/src/screens/accounts.rs`

**Interfaces:**
- Consumes: `synced_with_prefetch` (Task 7).
- Produces: `AccountSaveResult { snapshot: ModelSnapshot, warning: Option<String>, account_id: AccountId }`; `Message::AccountSynced(AccountId, Result<ModelSnapshot, String>)`.

- [ ] **Step 1: Write the failing test** in `update.rs`:

```rust
#[test]
fn a_saved_account_closes_the_dialog_and_schedules_the_first_sync() {
    let mut app = App::for_test();
    app.accounts_open = true;
    app.accounts.start_new();
    let result = AccountSaveResult {
        snapshot: ModelSnapshot::default(),
        warning: None,
        account_id: AccountId("acc".into()),
    };
    let _ = app.update(Message::AccountSaved(Ok(result)));
    assert!(app.accounts.form.is_none(), "the dialog closes immediately");
    assert!(!app.accounts_open);
}

#[test]
fn a_failed_first_sync_reports_a_toast_and_keeps_the_account() {
    let mut app = App::for_test();
    let _ = app.update(Message::AccountSynced(
        AccountId("acc".into()),
        Err("offline".to_string()),
    ));
    assert!(app.toast.current.is_some());
}

// Regression guard for the Review Focus item: a rejected quick check must not
// dismiss the dialog, so the settings can be corrected.
#[test]
fn a_rejected_account_keeps_the_dialog_open_for_editing() {
    let mut app = App::for_test();
    app.accounts_open = true;
    app.accounts.start_new();
    let _ = app.update(Message::AccountSaved(Err("bad credentials".to_string())));
    assert!(app.accounts.form.is_some());
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p pige a_saved_account_closes_the_dialog_and_schedules_the_first_sync`
Expected: FAIL — `AccountSaveResult` has no `account_id`; no `AccountSynced`.

- [ ] **Step 3: Implement**

- `message.rs`: add `account_id: AccountId` to `AccountSaveResult`; add `AccountSynced(AccountId, Result<ModelSnapshot, String>)`.
- `update.rs` `save_account`: run `api::test_account`, create/update the account (unchanged fallback), and **return immediately** with `AccountSaveResult { snapshot, warning, account_id: id }`. Delete the `api::sync_account(&id).await` block and its warning merge.
- `update.rs` `Message::AccountSaved(Ok(result))`: apply snapshot, close the form, close the screen, show any warning toast, then return `Task::perform(synced_with_prefetch(async move { api::sync_account(&id).await }), move |r| Message::AccountSynced(id, r))`.
- `update.rs`: add `Message::AccountSynced(id, result)` — `Ok(snapshot) => self.apply_snapshot(snapshot)`, `Err(error) => return self.show_toast(format!("First sync failed: {error}"))`.
- `screens/accounts.rs`: update its `AccountSaveResult` construction in tests to include `account_id`.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo test -p pige a_saved_account_closes_the_dialog && cargo test -p pige a_failed_first_sync_reports_a_toast && cargo test -p pige a_rejected_account_keeps_the_dialog_open`
Expected: PASS.

- [ ] **Step 5: Run the workspace and commit**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: PASS, clean.

```bash
git add rust/pige/src/message.rs rust/pige/src/update.rs rust/pige/src/screens/accounts.rs
git commit -m "feat: save accounts immediately and sync them in the background"
```

---

## Verification (after all tasks)

- [ ] `cd rust && cargo test --workspace` — all green.
- [ ] `cd rust && cargo clippy --workspace --all-targets` — no warnings.
- [ ] Manual, desktop: with prefetch on, open a large inbox; the first list renders, then tap several messages quickly — each shows a spinner instantly, the newest stays, and previously tapped bodies open instantly on a second tap.
- [ ] Manual, desktop: tap a message, then immediately Back — the reader does not reappear when the fetch lands.
- [ ] Manual: Settings → "Download in advance" off, restart, sync — confirm no bodies are fetched for unopened mail (watch logs / `bodies` table).
- [ ] Manual: add an account with a wrong password — the dialog stays open with the error. Correct it and save — the dialog closes at once and mail appears as the first sync lands.
