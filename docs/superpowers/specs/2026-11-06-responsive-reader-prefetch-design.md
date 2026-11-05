# pige — responsive reader, background body prefetch, non-blocking account add

**Date:** 2026-11-06
**Status:** Approved (2026-11-06)
**Author:** bronson + agent
**Repository:** `/home/bronson/Projects/pige`
**Licence:** MIT (qlaunch components adapted with the owner's permission)

---

## 1. Intent

Right now opening a conversation feels broken. `api::open_conversation` reads
the cached header, then — if the body is not cached — dials IMAP, selects the
folder, and downloads the whole message *before* it tells the reader anything.
On compact/mobile the reader pane does not even open until then (the layout
keys off `selected_conversation.is_some()`), so a tap looks like it did
nothing. There is no spinner anywhere in the app. And because each open writes
`state.selected_conversation` when it finishes with no idea which message the
user actually last picked, a slow earlier tap lands *after* a later fast one and
the wrong mail pops in — exactly the reported "click another, the first one
eventually appears".

This change makes opening a conversation feel instant, makes the newest tap
win, and quietly downloads the mail the user is most likely to read next so
that most opens are cache hits. It also stops adding an account from freezing
the dialog while the whole first mailbox sync runs.

### Success looks like

- Tapping a conversation shows the reader immediately — subject, sender, and a
  spinner where the body will be — with no stale content left on screen.
- The body replaces the spinner when it arrives. If the body is already cached,
  there is no spinner at all.
- Tapping a second conversation while the first is still downloading shows the
  second; the first never pops back over it, and its body is still cached to
  disk for later.
- With prefetch on, conversations the user is likely to open (unread and recent
  Inbox mail) usually open from cache with no network wait.
- With prefetch off, nothing is downloaded beyond what the user opens.
- Adding an account validates and saves without waiting for the first sync; the
  account appears immediately and the first sync runs in the background.

### Non-goals

- Server-side search, push/IDLE, or a background *service* (unchanged).
- Fetching remote images or any bytes other than message bodies and their
  attachments' metadata.
- A general connection pool or keep-alive between operations.
- Reworking sync, threading, search ranking, or send.
- Prefetching attachments' payloads (metadata only, as today).
- A user-facing prefetch progress UI.

### Anti-goals

- Any stale conversation overwriting the newest selection.
- Blocking the UI thread on network or SQLite.
- Downloading anything when the prefetch setting is off.
- Two live sources of truth for "what is selected".
- Silent bandwidth use on low-bandwidth connections (hence the setting gate).

---

## 2. Constraints and inherited ground

- Rust edition 2024. Gates remain `cargo test --workspace` and
  `cargo clippy --workspace --all-targets`.
- `pige_core` has no iced and no UI; all new logic lands there and is testable
  without a renderer. The shell only renders state and schedules tasks.
- Locks: the global state mutex is held only around synchronous SQLite work;
  network waits happen outside it. `SYNC_LOCK` serializes syncs and is held
  across network awaits.
- Bodies are cached in the `bodies` table; `Store::upsert_body` also refreshes
  the message's search entry. Attachments' metadata is stored by
  `Store::store_attachments`, called from the existing `resolve_body` path.
- `MAX_BODY_BYTES` (10 MiB) caps what the reader will download; bodies over it
  are shown as `MessageBody::TooLarge` and are never fetched.
- Settings live in the portable `pige.json` document
  (`pige_core::settings::Settings`), rendered by the Settings screen and
  persisted through `api::save_settings`. A new field rides along automatically.
- The shell reaches the core through async API functions and a broadcast
  `CoreEvent` channel; the shell's subscription turns most events into a fresh
  `api::snapshot()`.
- The Android JNI notification-tap path calls `api::open_conversation` and only
  logs the outcome; it does not depend on the body being present in the return
  value.
- There is no connectivity or metered-network detection in the codebase, and
  adding one is out of scope; the setting is the control users get.

---

## 3. Approaches considered

### A. Reader loading state

1. **Loading state in the core (chosen).** `open_conversation` builds the reader
   from the cached header first, marks the newest body `Loading`, installs it,
   and emits a snapshot; it then fetches and, only if its request is still the
   newest, resolves and installs the real body. Superseded fetches still cache
   their bytes. Everything the UI needs falls out of the snapshot it already
   reads, on both mobile (pane switch) and desktop.
2. **Shell-only loading flag.** Keep the core as is; track a loading flag and a
   request token in the shell and filter stale snapshots. Smaller core change,
   but two sources of truth: the core still writes the stale selection, every
   background snapshot must be filtered, and the instant header is awkward.
   Rejected.
3. **Do nothing but prefetch.** Rely on cache hits and accept the wait on a
   miss. Rejected: misses still look broken and the ordering bug remains.

### B. Where prefetched bodies come from

1. **After each sync, bounded and prioritized (chosen).** Fetch bodies for
   unread then recent Inbox messages, newest first, up to a cap, reusing one
   connection per folder and caching through the existing body path.
2. **Prefetch only the message after the open one.** Cheap, but does not make
   the *first* open fast and adds a network round trip per open. Rejected.
3. **Scheduled separate fetch loop.** A new timer independent of sync. More
   moving parts and no better result. Rejected.

### C. Account add

1. **Quick check + save, then sync in the background (chosen).** Keep the
   connection test as the quick gate; save and dismiss the dialog immediately;
   run the first sync as a background task that reports through the toast.
2. **Keep awaiting the first sync, add a spinner.** Does not address the wait.
   Rejected.

---

## 4. Design

### 4.1 Reader loading and newest-wins ordering

**Data model.** Add a third `MessageBody` variant:

```rust
pub enum MessageBody {
    Document(MailDocument),
    TooLarge,
    /// A body fetch is in flight for this message.
    Loading,
}
```

**Open request identity.** `AppState` gains `open_generation: u64`. Every
`open_conversation` call increments it under the state lock and keeps its own
value. Only a completion whose generation still equals `state.open_generation`
may install a body or change the selection.

**Invalidation.** Any code that clears or replaces the selection must also
invalidate in-flight opens, or a slow fetch will resurrect a conversation the
user already backed out of or removed. Add `AppState::invalidate_open()` (bump
`open_generation`) and call it wherever `selected_conversation` is cleared or
changed outside an open: `select_folder`, `select_unified`, `load_older` when
the folder changes, `remove_account`, `restore_config`, `factory_reset`, the
delete mutation, and the archive/trash/move mutations that change the open
conversation. The shell's Back path already routes through
`select_folder`/`select_unified`, so it inherits this.

**`open_conversation(thread_id)` becomes two-phase.**

- Phase 1 (state lock): read the thread's messages, account, folder, cached
  body, and the too-large flag, exactly as today; increment and capture the
  generation. Build a `ConversationDetail` from cached data where the newest
  body is: the cached `Document` if present, else `TooLarge` if over the cap,
  else `Loading`. Install it as `selected_conversation`. Build and return the
  snapshot, and emit `CoreEvent::SnapshotChanged` after dropping the lock.
- Phase 2 (no lock held across the await): only when the body was neither
  cached nor too large, fetch the raw bytes through the account's transport, as
  today.

**Completion.**

- Re-acquire the state lock.
- **Always** run the existing extraction + `store_attachments` + `upsert_body`
  caching on a successfully fetched body, so a superseded download is not
  wasted.
- If the generation is still current, resolve the body and rebuild the detail
  (newest body now `Document`, or `None` if it had no readable text), install it,
  build the snapshot, and emit `CoreEvent::SnapshotChanged`.
- If the generation is stale, do **not** touch `selected_conversation` and do
  **not** emit a selection change; return the current snapshot (the shell will
  apply the same authoritative state it already has).
- A fetch error logs and leaves the `Loading` body replaced by `None` (the
  reader's existing "no readable body" state) when current.

**Why awaiting rather than a detached task.** Keeping the fetch inside
`open_conversation` (as today) preserves one entry point, works for the JNI
caller, and is directly testable. The instant feedback comes from the phase-1
`SnapshotChanged` event, which the shell turns into a fresh snapshot while the
task is still running; concurrent opens are independent tasks in iced, so a
newer tap is never queued behind an older fetch. The task's final return value
is applied too, and is always built from the authoritative state, so it cannot
regress the view.

**Priority, precisely.** Display priority is absolute: the newest open is the
only one allowed to install. Network priority is best-effort: a superseded
fetch keeps running and sharing bandwidth rather than being cancelled, which is
what caches the message for later. This is a deliberate trade — true
cancellation would throw away the bytes we were asked to keep caching.

### 4.2 Reader rendering

- `screens/reader.rs`: `conversation_body` matches the new variant and renders a
  spinner (see below) centred in the body area when the newest body is
  `Loading`. The header (subject, sender, avatar) already renders from cached
  data, so it appears instantly.
- Earlier messages inside the expanded section are unaffected: they only ever
  hold cached bodies or nothing.
- The layout already treats `selected_conversation.is_some()` as "reading", so
  compact shows the reader pane immediately once phase 1 installs the loading
  detail.

**Spinner component.** Add `components/spinner.rs`: a small circular
indeterminate progress indicator. iced's `canvas` widget is not currently
compiled in — `pige/Cargo.toml` pins iced with `["wgpu", "tokio", "image",
"wayland", "advanced", "web-colors"]` — so enable the additive `"canvas"`
feature (it maps to `iced_widget/canvas`) and draw a rotating arc sized from
the palette, coloured `on_surface_variant`. It animates from a `Message::Spin`
driven by a new `iced::time::every` subscription that exists only while at
least one `Loading` body is present, so an idle app schedules no animation
frames. If the feature change proves troublesome on the pinned fork, fall back
to a pulsing ring built from a styled `container` (no canvas). Either way it is
one new component with no per-screen logic.

### 4.3 Background body prefetch

**Setting.** Add to `Settings`:

```rust
/// Download message bodies ahead of time so Inbox mail opens instantly.
pub prefetch_bodies: bool,   // serde default = true
```

- Default **on**, because it is the point of the feature; users on constrained
  connections turn it off.
- The Settings screen gains a "Download in advance" section with On/Off chips,
  mirroring the existing Background sync section, wired through a new
  `Message::SettingsPrefetchToggled(bool)` and persisted on Save like the other
  toggles.
- When off, no prefetch runs at all; the reader still shows its instant spinner
  and loads on open.

**Core entry point.** `api::prefetch_bodies() -> ()` (no snapshot change):

1. Read `state.config_doc.settings.prefetch_bodies` under the state lock — from
   the in-memory document, **not** `settings::load_settings(store)`, which reads
   the legacy `meta` row that migration deletes and therefore always returns the
   default. Return immediately if off.
2. Under a new `PREFETCH_LOCK` (a `try_lock`; if a prefetch is already running,
   skip this round), select candidates from the store:
   - For each account, its Inbox folder(s).
   - **One body per conversation**: the newest message of each Inbox thread,
     which is the only body the reader instantiates on open. Older replies in a
     thread are not prefetched.
   - That newest message's body is not cached, and its cached `size` is at or
     under `MAX_BODY_BYTES`.
   - **Unread conversations first, then newest-first**, capped at
     `PREFETCH_LIMIT = 50` conversations per account.
3. Group candidates by `(account, folder)`. Process folders with a small,
   bounded concurrency (`PREFETCH_CONCURRENCY = 2` connections total), one
   transport connection per folder reused across that folder's messages.
4. For each message, fetch raw bytes and cache through the same path as a
   reader open (extract parts, `store_attachments`, `upsert_body`). Re-check the
   cache immediately before fetching to avoid racing a user open. A successful
   fetch that yields no readable text is recorded so it is not retried forever
   (below). Any error is logged and skipped; one bad message never aborts the
   batch.
5. Emit nothing. Bodies do not change the list view, and the open path already
   observes the cache.

**Messages with no readable body.** `resolve_body` deliberately does not cache
a body when parsing yields no document, so a plain "body is not cached"
candidate query would re-download such mail on every sync and eat the cap. Use
the existing, currently unused `messages.body_state` column: set it to `'none'`
when a successful fetch has no readable body, and have the candidate query skip
any message whose `body_state` is not `'headers'`. No schema change — the column
already exists. `resolve_body` learns the same update so a reader open is not
retried by prefetch either.

**Trigger.** Prefetch is deliberately **not** called inside core `sync_all`/
`sync_account`. `nativeSyncAll` runs `sync_all` on a **current-thread** runtime
(`pige/src/jni.rs::runtime`), so a `tokio::spawn`ed prefetch would never run,
while `await`ing it there would hold the Android WorkManager worker past its
execution budget. Instead the **shell** triggers `api::prefetch_bodies()` after
each user-facing sync — startup sync-on-open, Sync Now, pull-to-refresh, and the
background first sync from account add — as its own `Task`. The Android
WorkManager background sync therefore does not prefetch; the foreground app
prefetches on its next sync-on-open, so opens are still fast when the user
returns. `PREFETCH_LOCK` keeps overlapping triggers from doubling up.

**Concurrency honesty.** Bodies within one folder are fetched sequentially over
one connection; parallelism is across folders/accounts, capped at two. This is
not a per-message thread pool — it is not needed for responsiveness once the
reader is instant and the common cases are cached — and it avoids hammering a
single IMAP session. "Multi-threaded downloads" here means the work proceeds in
the background, the newest open is never blocked behind an older one, and
across accounts the fetches run in parallel.

### 4.4 Non-blocking account add

Today `save_account` runs `test_account` (connect + login), then creates or
updates the account, then **awaits `sync_account`** — the whole first sync —
before the dialog closes.

Change the shell's `save_account` to:

1. Run `api::test_account` (the quick check). On failure, return the error; the
   existing `AccountSaved` error arm keeps the form open with the message, so
   the user can fix settings and retry. **No dialog change needed.**
2. Create or update the account (unchanged, including the
   session-only-credential fallback).
3. Return `AccountSaveResult` immediately — the form closes and the account
   shows in the list.
4. Schedule the first sync as a separate task via `Task::perform` from the
   `AccountSaved` success arm. On success, refresh the snapshot (the new mail
   appears). On failure, show a warning toast; the account stays and the user
   can retry with Sync Now.

The dialog's "Working…" state now covers only the quick check and the save,
which are fast.

**Decision (my call, flagged for review):** a first-sync failure after a
successful save is a **toast**, not a reopened dialog. The quick check already
proved the credentials and endpoints work, so a first-sync failure is almost
always transient or server-side; reopening the edit dialog would imply the
settings are wrong when they are not. If the user wants the dialog back on any
sync failure, that is a small change to the `AccountSaved`/sync-error arm.

---

## 5. Data and API surface

| Item | Change |
| :--- | :--- |
| `MessageBody` | add `Loading` |
| `AppState` | add `open_generation: u64` and `invalidate_open()` |
| `api::open_conversation` | two-phase; generation-guarded completion; caches superseded fetches |
| `api::prefetch_bodies` | new; bounded, prioritized, setting-gated |
| `PREFETCH_LOCK`, `PREFETCH_LIMIT`, `PREFETCH_CONCURRENCY` | new constants in `api` |
| `Settings` | add `prefetch_bodies: bool` (default true) |
| `Store` | new query: newest message of each uncached, under-cap Inbox conversation, unread-first then newest; a `body_state = 'none'` setter |
| `messages.body_state` | start using the existing column (`'headers'` → `'none'`) |
| `Message` | add `SettingsPrefetchToggled(bool)`; a spinner tick message |
| `components/spinner.rs` | new component; enable iced `"canvas"` feature |
| `screens/reader.rs` | render `Loading` |
| `screens/settings.rs` | "Download in advance" section |
| `app::save_account` (shell) | quick check + save only; background first sync |
| `app` / `update.rs` (shell) | trigger `prefetch_bodies()` after user-facing syncs |
| `api::sync_one` | read `notifications` from the config document, not the legacy `meta` row |

No schema migration: `Settings` is JSON in `pige.json` with `#[serde(default)]`;
`bodies`, `attachments`, and `messages.body_state` already exist. The `canvas`
feature addition changes `pige/Cargo.toml` and the lockfile.

---

## 6. Testing

Core (`pige_core`):

- Opening an uncached conversation returns a snapshot whose newest body is
  `Loading` before any fetch completes (use `FakeTransport` with a controllable
  body fetch).
- A superseded open does not overwrite the newer selection, and its fetched
  body is still present in `Store::body` afterwards.
- Backing out (or changing folders, or removing the account) while an open is
  in flight invalidates it: the completion neither installs nor resurrects the
  selection.
- Cached and too-large conversations open directly into `Document`/`TooLarge`
  with no `Loading`.
- Prefetch, with the setting on, caches the newest message of the expected Inbox
  conversations (unread-first, capped, under-cap only) and never fetches
  over-cap ones or older replies; with the setting off, it fetches nothing.
- A successful fetch with no readable body marks `body_state = 'none'` and is
  not re-selected by the next prefetch.
- Prefetch re-checks the cache, so a message opened by the user during a
  prefetch is not fetched twice.

Shell (`pige`):

- The reader renders a spinner for `Loading` and the document when resolved
  (pure view/state test; no renderer needed beyond the existing headless
  helpers).
- The compact layout shows the reader for a `Loading` detail.
- `Settings` round-trips `prefetch_bodies` through save/load.
- Account save returns before sync and schedules a background first sync (test
  the `update` transition with a fake/stub sync result).

Gates: `cargo test --workspace` and `cargo clippy --workspace --all-targets`
both clean.

---

## 7. Risks and notes

- **Extra snapshots.** Each open now emits up to two snapshots. Snapshot
  building is a cheap in-process read (measured single-digit ms for the list),
  and phase-1 snapshots are what make the app feel instant.
- **Prefetch bandwidth.** Bounded and gated. Worst case is `50 × accounts`
  conversations per sync, each one body capped at 10 MiB but in practice far
  smaller; only uncached ones are fetched.
- **Two connections at once.** A prefetch after a sync opens its own
  connections (cap 2). SQLite is WAL and each `Store` serializes its own
  writes; bodies and headers are different tables. `PREFETCH_LOCK` prevents
  duplicate prefetches.
- **Spinner animation cost.** The tick subscription only exists while a body is
  loading, so there is no idle animation.
- **Headless renderer.** If the canvas spinner does not render under the
  headless test path, the component still builds and lays out; the assertion is
  on the element tree, not pixels.
- **Adjacent bug folded into scope.** `api::sync_one`
  (`pige_core/src/api/mod.rs:2454`) reads `settings::load_settings(store)
  .notifications`, i.e. the legacy `meta` row that `init` deletes on migration,
  so it always sees the default and the Notifications toggle is effectively
  ignored during sync. Fix it here: read
  `config_doc.settings.notifications` the same way `prefetch_bodies` reads its
  setting. One-line correctness fix in the same area, sharing the reasoning
  above. Add a regression test: with notifications off in the document, a sync
  that finds new mail must not notify.

### My calls (please correct any)

1. Prefetch defaults **on**, capped at **50 conversations per account**, **2**
   concurrent connections.
2. Prefetch scope is **Inbox only**, **unread first then newest**, and **one
   body per conversation** (the newest message) — older replies are not
   prefetched because the reader never shows them on open.
3. Prefetch is triggered by the **shell** after user-facing syncs, not inside
   core `sync_all`, so the Android WorkManager worker is not held open (§4.3).
4. A first-sync failure opens **no** dialog — a toast and a retry path (§4.4).
5. The reader shows subject/sender immediately and only animates a spinner for
   the body; no skeleton lines.
6. Enable iced's `"canvas"` feature for the spinner, with a pulsing-ring
   fallback if the pinned fork makes that awkward.
