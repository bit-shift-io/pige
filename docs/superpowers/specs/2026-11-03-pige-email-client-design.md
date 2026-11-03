# pige — Design

**Date:** 2026-11-03
**Status:** Draft for approval
**Author:** bronson + agent
**Repository:** `/home/bronson/Projects/pige`
**Licence:** MIT (qlaunch components copied with the owner's permission)

---

## 1. Intent

pige is a featherlight email client for Linux and Android, written in Rust with an
iced UI. On phones it should feel like the Gmail Android app: a fast conversation
list, a clean reader, full-screen compose, search, folders, and notifications. On
tablets and desktops the same app opens into a split-pane layout with navigation,
message list, and reader visible together.

The reason to build it is control: a small binary, no WebView, no browser engine,
no analytics, an offline-first local cache, and a UI that can be reshaped without
fighting a framework. The core is Rust and owns all mail state; iced is the skin.

### Success looks like

- Adding an account and seeing cached mail is quick; after the first sync, cold
  launch renders the list from SQLite without waiting on the network.
- A phone user can go drawer → folder → conversation → reply without leaving the
  single-pane flow, and the back gesture always makes sense.
- A desktop user sees navigation, list, and conversation at once.
- New mail arrives as a notification within the 15-minute background window, and
  pull-to-refresh gives an immediate manual sync.
- Search over synced mail returns in well under a second.
- The app stays materially smaller and lighter than Gmail; this is measured during
  development, not assumed.

---

## 2. Scope

### V1 must have

- Multiple IMAP/SMTP accounts with user-provided display names ("Personal", "Work").
- Canary-style navigation: unified inbox pinned at the top, then collapsible
  account sections whose folders sit underneath.
- Gmail-style conversation threading: one row per conversation, stacked messages
  in the reader.
- Folder browsing (Inbox, Sent, Drafts, Trash, Archive, Junk, and other IMAP
  folders exposed by the server). Gmail labels appear as folders; moving into one
  is the user's drag-to-folder workflow.
- Reader with a native, sanitized rendering of HTML mail and a plain-text fallback.
- Compose, reply, and forward. Plain-text composition in v1.
- View-only attachments: show chips, download/open a file, and preview nothing
  inline beyond embedded images the message already carries.
- Mark read/unread, move between folders, and delete (move to Trash; permanent
  delete only from Trash). Archive removes the source label on Gmail and moves to
  the server's Archive folder elsewhere.
- Search over synced mail with an all-accounts default and a scope chip.
- Background sync and new-mail notifications:
  - Android: WorkManager periodic work, default 15 minutes, plus pull-to-refresh
    and sync-on-open.
  - Linux: notification via D-Bus while the app runs; sync-on-open and manual
    refresh. No background daemon in v1.
- Local drafts and an offline send queue that retries.
- Light and dark themes, following the system by default.
- Thin Kotlin only where Android requires it.

### V1 will not have

- OAuth or provider-specific sign-in. App passwords only. The auth layer is
  abstracted so OAuth can be added later.
- Push mail, IMAP IDLE in the background, or a foreground service.
- Sending attachments, forwarding attachments, or rich-text composition.
- Calendar, contacts, labels management beyond IMAP folders, filters, signatures,
  snooze, stars, read receipts, PGP/S/MIME, or server-side rules.
- JMAP, POP3, Exchange, or local Maildir.
- Full HTML fidelity: no CSS layout, web fonts, scripts, forms, iframes, or
  tracking pixels.
- Message-store encryption at rest beyond the OS sandbox and platform keystores.
- Multiple windows, tabs, plugins, or a tray icon.

### Anti-goals

- A WebView or embedded browser for mail.
- Blocking the UI thread on network or SQLite work.
- A large, opinionated mail framework that dictates storage or licensing.
- Kotlin holding UI state, mail state, or sync logic.
- Storing passwords in plaintext or logging message content/credentials.

---

## 3. Constraints and inherited ground

- **Targets:** Linux desktop and Android, arm64 first (qlaunch builds
  `aarch64-linux-android` only). No iOS, no Windows in v1.
- **Toolchain:** Rust edition 2024, iced pinned to the qlaunch fork
  (`Santuzius/iced`, tag `iced_android-v0.1.1`), `iced_android` tag `v0.1.1`,
  Android NDK `28.2.13676358`, JDK 17, Gradle 9.8.1/AGP 9.4.1, minSdk 34,
  targetSdk 36.
- **Android shell:** `NativeActivity` subclass from `IcedActivity`; the native
  library must load before `super.onCreate`, the window format must be reset to
  `RGBA_8888`, and the qlaunch window-lifecycle rule applies (backgrounding must
  not close the iced window).
- **Licensing:** pige is MIT. The chosen mail dependencies — `async-imap`,
  `lettre`, `mail-parser`, `rusqlite`, `ammonia`, `html5ever` — are MIT/Apache.
  `melib` is EUPL-1.2/GPL-3.0-or-later and is therefore excluded.
- **Acceptance accounts:** Gmail (with an app password) is the primary target; a
  generic IMAP/SMTP account (for example Fastmail) is the secondary. Gmail's
  labels-as-folders model is part of v1, via its `X-GM-EXT-1` extension.
- **qlaunch reuse:** theme tokens, Material components, icon font, label,
  scroll-fade/elastic-scroll/pull-down gesture pieces, fonts, build scripts, and
  the Android scaffolding are fair game. Code lifted from qlaunch carries a
  provenance note.
- **Network:** TLS with normal certificate validation, no insecure override in v1.
- **Store accounts of any size:** the cache must bound disk and memory; bodies are
  fetched on demand and old bodies are pruned.

---

## 4. Approaches considered

### Repo shape

1. **Two-crate workspace, qlaunch-style (chosen).** `pige_core` is
   platform-independent and testable without a renderer; `pige` is the iced skin,
   desktop binary, and JNI entry; `android/` is the Gradle shell. Proven on
   qlaunch, keeps the core/UI boundary honest.
2. **Single crate.** Faster to start, but UI, JNI, and core get entangled, core
   tests need iced, and splitting later is churn.
3. **Core as a separate daemon with IPC.** Strong isolation, but process
   lifecycle, IPC, and Android background work add weight for no v1 benefit.

### Mail engine

1. **Own engine on lean crates (chosen).** `async-imap` + `lettre` +
   `mail-parser` + `rusqlite` FTS5. We own incremental sync, threading, cache,
   and the queue — exactly the behaviour that differentiates pige. Cost: the sync
   state machine and threading are the largest single chunk of work.
2. **`melib`.** Saves protocol work, but GPL/EUPL, large and opinionated, and we
   would still adapt its storage model to our cache and UI.
3. **External sync tools (isync/offlineimap) + Maildir.** Not viable on Android,
   adds a moving part, and gives up control of sync state.

### Rendering

1. **Sanitized HTML → typed document AST → native iced widgets (chosen).**
   Lightweight, private, consistent, testable.
2. **WebView overlay.** High fidelity, but reintroduces a browser, heavy memory,
   Kotlin UI, and input/z-order problems. Rejected.
3. **Plain text only.** Cheapest, but most mail is HTML-only and would read badly.

---

## 5. Architecture

### Crates and paths

```
pige/
  rust/
    Cargo.toml                # workspace, android-activity patch, release profile
    pige_core/                # no iced, no platform UI
      src/
        account/              # account config, labels, auth kinds
        mail/                 # addresses, headers, messages, conversations
        store/                # SQLite schema, migrations, FTS, body cache
        transport/            # MailTransport trait, IMAP/SMTP impls, fake
        sync/                 # scheduler, per-folder state machine, diff/reconcile
        send/                 # compose MIME, outbox, drafts
        search/               # FTS query and scopes
        html/                 # sanitizer and HTML -> MailDocument AST
        platform/             # traits + Linux/fake/Android adapters
        api/                  # snapshots, mutations, CoreEvent
    pige/                     # iced UI + desktop binary + JNI entry
      src/
        lib.rs                # App, Message, update, subscription hub
        main.rs               # desktop entry
        ui/
          theme.rs            # Material 3 light/dark tokens and styles
          components.rs       # buttons, fields, dialogs, chips, list rows
          icons.rs            # Material Icons glyphs (from qlaunch)
          fonts.rs            # platform fonts
          layout.rs           # pure breakpoint/geometry helpers
          navigation.rs       # drawer/rail: unified inbox, accounts, folders
          message_list.rs     # conversation rows, pull-to-refresh, empty states
          reader.rs           # message stack, toolbar, attachments, body blocks
          compose.rs          # new/reply/forward, drafts, recipients
          search.rs           # query field, scope chip, results
          accounts.rs         # add/edit account, connection test
          settings.rs         # accounts, sync, notifications, theme
          html.rs             # MailDocument AST -> iced elements
        jni.rs                # JNI exports, Android bridge calls
        platform.rs           # iced-side Notifier/Opener/Credentials impls
  android/
    app/src/main/kotlin/bitshift/pige/
      MainActivity.kt         # IcedActivity, lifecycle, back, permission ask
      PigeBridge.kt           # install(context), paths, credentials, open, notify
      NotificationPoster.kt   # channel, group, tap intent
      CredentialStore.kt      # Android Keystore AES-GCM
      SyncWorker.kt           # WorkManager -> native syncAll()
    app/src/main/AndroidManifest.xml
    gradle/…
  scripts/build.sh            # cargo -> jniLibs -> Gradle (adapted from qlaunch)
  run.sh / run_adb.sh         # desktop / build-install-launch
  docs/superpowers/specs/…    # this document
```

Package/application id: `bitshift.pige`. Library name: `pige`.

### Data flow

```
Kotlin (OS)  ──JNI──►  pige_core  ──snapshot()/CoreEvent──►  pige (iced)  ──render
     ▲                    │                                      │
     └──── PigeBridge ◄───┴────────── core::api::* calls ────────┘
```

- `pige_core::api` holds the live `App` (accounts, store, sync scheduler) behind a
  global lock, mirroring qlaunch's `api::state`.
- The UI reads `api::snapshot()` — a cheap, cloneable `ModelSnapshot` containing
  account/folder/thread/list state — and subscribes to `CoreEvent`s
  (`MailChanged`, `FolderChanged`, `SyncProgress`, `NewMail`, `Error`).
- Mutations are `async fn api::*(…) -> Result<ModelSnapshot, String>`, run off the
  render thread through a `work(...)` helper (`tokio::spawn_blocking` for SQLite,
  async tasks for network), then applied.
- The Android worker and Activity call the same `api::sync_*` functions. There is
  one initialisation path (`OnceLock`) for paths, credentials, and notifications.

### Core model

- **Account:** id, display label, email, display name, IMAP/SMTP settings, auth
  kind (password now, OAuth later), sort order.
- **Folder:** account-scoped; remote name, hierarchy delimiter, special kind
  (`Inbox`, `Sent`, `Drafts`, `Trash`, `Archive`, `Junk`, `Other`), unread/total,
  UIDVALIDITY/UIDNEXT sync state.
- **Message:** account/folder/UID, envelope headers, flags, size, snippet,
  attachment metadata, body state (`None`/`Headers`/`Body`), raw cache path.
- **Conversation:** account-scoped thread with aggregate date, unread count,
  message count. Mutations act on the conversation's copies in the current folder.
- **Draft/Outbox:** persisted locally, state machine
  (`Draft`/`Queued`/`Sending`/`Sent`/`Failed`), retry on next sync.
- **Snapshot:** only what the current view needs; no UI-side re-derivation of
  ordering, counts, or conversation membership.

### SQLite store

One database per installation (not per account), in the platform data directory.
Migrations are versioned; `PRAGMA journal_mode=WAL`, foreign keys on.

Tables (names indicative; exact columns are a builder call):

- `accounts`
- `folders` (with UIDVALIDITY, UIDNEXT, last sync, unread/total)
- `messages` (headers, flags, thread id, snippet, raw path, body state)
- `threads` (aggregates; membership via `messages.thread_id`)
- `bodies` (plain text, sanitized HTML, or AST cache; fetched timestamp)
- `attachments` (part id, filename, MIME, size, content id, cached path)
- `outbox` (drafts/queued sends)
- `message_fts` (FTS5: subject, sender, recipients, plain body; conversation id
  unindexed for joining)
- `meta` (schema version, settings)

Raw message bytes are stored as files in the cache directory, keyed by
account/folder/UID, and pruned by age/size. The DB stores metadata and searchable
text so the list and search never need the raw MIME.

### Transport abstraction

```rust
trait MailTransport {
    async fn list_folders(&mut self) -> Result<Vec<RemoteFolder>>;
    async fn select(&mut self, folder: &RemoteFolder) -> Result<FolderState>;
    async fn fetch_headers(&mut self, range: UidRange) -> Result<Vec<RemoteMessage>>;
    async fn fetch_body(&mut self, uid: Uid) -> Result<Vec<u8>>;
    async fn fetch_flags(&mut self, uids: &[Uid]) -> Result<Vec<(Uid, Flags)>>;
    async fn search(&mut self, query: &str) -> Result<Vec<Uid>>;
    async fn store_flags(&mut self, uids: &[Uid], flags: FlagOp) -> Result<()>;
    async fn move_messages(&mut self, uids: &[Uid], dest: &str) -> Result<()>;
    async fn archive(&mut self, uids: &[Uid], folder: &RemoteFolder) -> Result<()>;
    async fn trash(&mut self, uids: &[Uid], folder: &RemoteFolder) -> Result<()>;
    async fn append(&mut self, folder: &str, message: &[u8], flags: Flags) -> Result<()>;
}
```

`ImapTransport` uses `async-imap`; `SmtpSender` uses `lettre`; `FakeTransport`
drives tests. Sync never talks to `async-imap` types directly outside this module.

### Sync

`sync_account(account_id)` is the unit. One in-flight sync per account, guarded by
an async mutex; global sync runs accounts sequentially (or at most two at a time).

1. Connect, authenticate, `LIST` folders; upsert and classify.
2. Sync **Inbox** first, then special folders (Sent/Drafts/Trash/Archive/Junk).
   Other folders sync when opened or on explicit refresh — this keeps background
   work bounded on accounts with many labels.
3. Per folder:
   - `SELECT`, read UIDVALIDITY/UIDNEXT.
   - First sync or UIDVALIDITY change: reset folder state, fetch headers for the
     newest N messages (default 300; builder-tunable), insert.
   - Otherwise: fetch headers for UIDs above the stored high-water mark in
     batches; reconcile flags for cached UIDs; detect expunges by UID-set diff.
   - Update folder counts.
   - Older mail is reachable: a "load older" trigger at the end of the list
     backfills the next page (default 100) from the server. The initial window is
     a starting point, never the account's visible ceiling. Search covers
     everything cached, including backfilled pages; older mail becomes
     searchable as it is loaded.
4. New UIDs in Inbox produce `CoreEvent::NewMail` with account, sender, subject,
   and conversation id. The platform notifier posts; the UI refreshes.
5. Bodies are fetched on open, then cached. After the list is visible, a
   low-priority background pass fetches the first ~2 KiB of text for the newest
   ~100 messages in the visible folder to fill snippets, then a bounded number of
   full bodies (default 100) to make search useful. It never downloads a whole
   folder. Search indexes subject, sender, recipients, and every body text that
   has been fetched; opening a message indexes it immediately.
6. Outbox flush and Sent APPEND happen at the end of a sync.

Special-folder absence is handled explicitly, never silently:

- No Archive folder: on Gmail, archive is label removal (`STORE \Deleted` on the
  current folder's copies); elsewhere the archive action is hidden.
- No Trash folder: delete asks for confirmation and performs a permanent delete
  (`STORE \Deleted` + expunge) rather than pretending to trash.
- No Sent folder: sent mail is kept locally but not appended remotely.
- No Drafts folder: drafts stay local-only.
- No Junk folder: no junk action is shown.

Failure handling: network errors leave the last good state and mark the account
as offline/degraded; auth errors surface prominently and pause that account's
scheduled sync; one account failing never blocks another.

### Folders, labels, and Gmail

Gmail exposes labels as IMAP folders, and one message can appear in several of
them. pige shows them as folders because that is the user's workflow: dragging a
message into a Gmail folder is applying that label and removing the source label.
Moving a conversation therefore means COPY/MOVE into the destination folder and
removing the current-folder copies.

- Generic IMAP: `MOVE` when the server advertises it, otherwise
  `COPY` + `STORE \Deleted` + `EXPUNGE`.
- Gmail (`X-GM-EXT-1` in `CAPABILITY`): folder membership *is* label membership.
  A move is `COPY` into the destination label folder, then `STORE \Deleted` on
  the source copy — Gmail removes that label and keeps the message in All Mail.
  Archive is just `STORE \Deleted` on the source copies (remove the Inbox or
  label membership), which is why it must never be used in Trash or Spam.
  Trash is `COPY` to `[Gmail]/Trash` + `STORE \Deleted` on the source. This path
  avoids duplicate copies in All Mail and does not depend on `X-GM-LABELS`.
- All folders returned by `LIST` are browsable, including `[Gmail]/All Mail`,
  `[Gmail]/Important`, and user labels. Labels management (create/rename/delete)
  is not in v1.

### Actions and reconciliation

- **Scope:** mutations act on the conversation's messages in the **currently
  viewed folder**. Copies in other folders, including Sent, are untouched. This
  is deliberate: a thread-wide delete that removes sent mail is a data-loss
  surprise.
- **Read/unread:** optimistic local update, then server `STORE`; the next sync
  reconciles flags.
- **Move/archive/delete:** optimistic local update, then transport. On failure,
  revert the local change and surface an error.
- **Delete:** moves current-folder copies to Trash; permanent deletion is offered
  only from Trash. With no Trash folder, confirm and expunge.
- **Offline:** reading, cached bodies, drafts, and search work offline. Sending is
  queued in the outbox and retried. Other mutations (move, archive, delete,
  read-state) are disabled offline with a "reconnect to do this" message. A
  replayable mutation queue is a later extension, not v1.
- **Send:** SMTP first; on success APPEND to Sent; on failure keep the queued
  message and retry on the next sync. Drafts are local in v1.

### Threading

Conversation grouping is core-owned and deterministic:

1. Use `Message-ID`, `In-Reply-To`, and `References` to union messages into a
   thread; merge threads when a bridge message arrives.
2. Fall back to normalized subject (strip `Re:`/`Fwd:`/list tags, trim, collapse
   whitespace) plus overlapping participants within a time window for clients
   that omit references.
3. Store `thread_id` on every message; recompute conversation aggregates
   (counts, newest date, unread) on insert/flag change/move.
4. Gmail labels are just folders; a message can exist in several folders on the
   server, but the local cache keys it by folder+UID and deduplicates by
   account+Message-ID for conversation display.

### HTML rendering

Pipeline:

```
raw MIME -> mail-parser -> text/plain + text/html parts
        -> strict sanitizer (ammonia allowlist; strip style/class/scripts/forms)
        -> html5ever tree -> MailDocument AST
        -> iced elements (no HTML strings at render time)
```

`MailDocument` blocks: paragraph, heading, ordered/unordered list, blockquote,
preformatted, rule, simple table, image. Inline spans carry bold, italic,
underline, strike, monospace, and links. Unknown elements are flattened to their
children; depth and total node limits prevent pathological messages.

- Remote images are blocked and replaced by a compact placeholder; a per-message
  "Load images" action is available (builder call: remember per sender).
- `cid:` images resolve to MIME parts and render locally.
- Links are limited to `http`, `https`, and `mailto`; they open in the system
  browser/mail handler.
- Plain-text-only mail renders with preserved line breaks and auto-linked URLs.
- Quoted blocks collapse after a configurable number of lines with
  "show trimmed content".
- The AST is cached per message so re-opening is instant; large messages are
  capped.

### UI and layout

Breakpoints are in logical points, not physical pixels, with pure helpers in
`layout.rs`:

- **Compact (< 840):** one pane. Drawer overlays the list; list → reader →
  compose are pushed screens with predictable back behaviour.
- **Medium (840–1199):** list + reader side by side; drawer overlays.
- **Expanded (>= 1200):** navigation rail + list + reader in three panes.

Navigation rail/drawer, top to bottom:

1. "All inboxes" pinned entry with aggregate unread count.
2. One section per account: user label (e.g. "Work"), email as secondary text,
   collapse control, unread roll-up, then folders with icons and unread counts.
3. "Add account" and "Settings" at the bottom.

List rows: avatar with deterministic initials colour, sender(s), date/time,
subject, snippet, unread weight/dot, attachment icon, conversation count. The
latest message supplies the row preview. A floating compose button on compact;
a toolbar compose button on wide layouts. Reaching the end of the loaded pages
shows a "load older" row that backfills from the server.

Reader: top bar with back (compact), archive, delete, mark unread, and overflow;
subject; sender summary with expandable To/Cc and date; collapsed earlier
messages; latest message expanded; attachments; reply/forward actions. Opening a
conversation marks it read after a short dwell, not on a stray tap.

Compose: full-screen on compact, centered modal on wide. To/Cc/Bcc, subject, body,
send, discard/save draft. Reply prefills recipients and quoted body; forward
prefills `Fwd:` and quoted body. Sending with no network queues the message and
tells the user it will send later.

Search: a query field in the top bar, local FTS as you type, results grouped by
conversation, and a scope chip (All accounts / current account / current folder).
Empty query shows recent results. Searches cover synced content; words that only
exist in un-downloaded bodies are not found until those bodies are fetched
(builder call: server-side SEARCH fallback later).

Settings: accounts (add/edit/label/remove), sync interval (15/30/60 minutes, off),
notifications on/off, theme (system/light/dark), about. No signature editor or
per-folder sync rules in v1.

### Platform boundary

Core talks to the OS through traits:

```rust
trait Notifier { fn post(&self, mail: NewMailSummary); }
trait Opener { fn open_url(&self, url: &str); fn open_attachment(&self, path: &Path); }
trait CredentialStore { fn get(&self, id: &str) -> Result<Option<Secret>>; fn set(...); fn delete(...); }
trait Paths { fn data_dir(&self) -> PathBuf; fn cache_dir(&self) -> PathBuf; }
trait BackgroundSync { fn schedule(&self, interval: Duration); }
```

- **Linux:** D-Bus notifications (`notify-rust`), `xdg-open`/`open` for URLs and
  files, Secret Service for passwords (`keyring`), no background schedule. If no
  Secret Service is available, the account works for the session after a password
  prompt and the password is not persisted; pige never falls back to plaintext.
- **Android:** JNI to `PigeBridge`: notification poster, `Intent` openers,
  Keystore-backed credential store, WorkManager scheduling. Rust receives the
  files/cache paths at install time.
- **Fakes:** in-memory implementations keep the core testable.

### Android specifics

- `MainActivity : IcedActivity` loads `libpige.so`, installs `PigeBridge` with the
  application context, sets `RGBA_8888`, and handles Back/predictive back and the
  notification permission request.
- `POST_NOTIFICATIONS` is declared and requested at runtime; notifications are
  skipped until granted.
- WorkManager unique periodic work (minimum 15 minutes) with a network
  constraint. The worker installs the bridge and calls `nativeSyncAll()`; it does
  not create UI. Keep-alive/foreground/IDLE sync is explicitly deferred.
- Notification channels: one "Mail" channel, grouped per account. Tap opens the
  app on the conversation via intent extras.
- Credentials: AES-GCM key in Android Keystore; ciphertext file in app-private
  storage. Rust only sees plaintext briefly when connecting; never persisted to
  SQLite or logs.
- File open: write the attachment to the app cache, expose it through a
  `FileProvider`, and launch `ACTION_VIEW`; no storage permission needed.

### Build and operations

- `scripts/build.sh [debug|release]` adapted from qlaunch: NDK linker env,
  16 KiB page alignment, cargo build for `aarch64-linux-android`, copy
  `libpige.so` into `jniLibs`, resolve iced_android's Java sources from Cargo
  metadata, run Gradle.
- `run.sh` runs the desktop binary with Wayland/X fallbacks.
- `run_adb.sh` builds, installs, launches, and tails `adb logcat -s pige`.
- Release profile strips debuginfo. x86_64 emulator builds are out of scope until
  needed.

---

## 6. Security and privacy

- TLS mandatory; no certificate bypass setting.
- No remote content, scripts, forms, or tracking pixels in rendered mail.
- Sanitizer allowlist + size/depth limits; never inject HTML into a web engine.
- Links restricted to safe schemes and opened externally.
- Credentials in platform keystores, separate from the mail DB; redacted logs.
- Notification previews default to sender + subject (builder call: privacy mode).
- The mail cache lives in the app's private data directory; desktop respects
  XDG paths. At-rest encryption of cached mail is not a v1 goal.

---

## 7. Testing and verification

- `cargo test --workspace` and `cargo clippy --workspace --all-targets` are the
  gates, as in qlaunch.
- Core tests use `FakeTransport` + temporary SQLite:
  - sync: first sync, incremental UIDs, UIDVALIDITY reset, flag changes,
    expunges, auth failure isolation, outbox retry;
  - folder operations: generic `MOVE`, COPY+delete fallback, Gmail label move,
    Gmail archive/trash, missing special folders, mutation failure rollback;
  - offline: reads/search from cache, queued send retry, mutations refused;
  - threading: reference chains, bridged threads, missing references, subject
    fallback, merged conversations;
  - search: scope, ranking, flag/account filtering;
  - HTML: sanitizer fixtures (scripts, tracking pixels, cid images, nested
    tables, malformed markup) → AST snapshots;
  - compose: MIME generation, reply/forward headers, quote handling.
- Pure UI geometry/state tests for breakpoints, row layout, drawer/account
  sections, and reader collapsing — no renderer required.
- On-device: add a real account, sync inbox, open a marketing HTML mail, reply,
  send, pull-to-refresh, kill process, wait for a background notification, and
  verify tap-through. Log tag `pige`.

---

## 8. Sequencing (rough)

1. Workspace, desktop shell, Android shell, theme, breakpoint shell.
2. Store, accounts, connection test, folder sync, list and reader with plain text.
3. Conversation threading, HTML AST renderer, search.
4. Compose/reply/forward, outbox, Sent APPEND, drafts.
5. Pull-to-refresh, notifications, WorkManager, credential stores.
6. Attachments (view), settings, polish, performance pass.

The implementation plan will refine this into tasks with acceptance checks.

---

## 9. Calls I made (check these)

- Plain-text compose in v1; replies/forwards quote plain text. No rich-text editor.
- One database with account-scoped rows, not one DB per account.
- Background sync covers Inbox and special folders only; other folders sync on
  open/manual refresh.
- Conversation mutations apply only to copies in the currently viewed folder.
  Sent copies are never moved or deleted as a side effect.
- Deletes move to Trash; permanent delete only from Trash.
- Archive removes the Inbox label on Gmail, or moves to the server's Archive
  folder elsewhere; hidden when neither is available.
- Offline: reading, search, drafts, and queued sending work. Other mutations are
  disabled until reconnect.
- Local-only search for v1; no server-side SEARCH fallback yet.
- Light and dark themes, system default.
- Initial header window of 300 messages per folder, tunable, with "load older"
  backfill.
- Attachments are view-only and opened through a FileProvider.
- Drafts are local in v1; Sent APPEND is best-effort.
- Package id `bitshift.pige`.

## 10. Left to the builder

Exact schema columns and migration mechanics; sync batch sizes and expunge
strategy; threading fallback window; sanitizer allowlist details and AST cache
limits; body/raw-cache pruning policy; account presets and connection-test UX;
notification grouping and privacy default; precise theme tokens; whether "load
images" is remembered per sender; WorkManager retry/backoff parameters; Gmail
detection and auto-expunge behaviour, browsing All Mail/Important, label rename
collisions; how the "load older" trigger interacts with an in-flight sync.
