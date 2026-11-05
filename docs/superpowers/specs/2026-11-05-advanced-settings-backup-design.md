# pige — portable config, Advanced settings, backup/restore, shared toast

**Date:** 2026-11-05
**Status:** Approved (author pre-approved; design produced autonomously)
**Author:** bronson + agent
**Repository:** `/home/bronson/Projects/pige`
**Licence:** MIT (qlaunch components adapted with the owner's permission)

---

## 1. Intent

pige keeps everything — accounts, settings, and the whole mail cache — in one
SQLite database under the XDG *data* directory. That is right for the cache,
but wrong for the parts a person should be able to see, copy, and move:
their accounts and preferences are trapped inside a binary file next to
gigabytes of disposable mail.

This change gives pige a real, portable configuration the way its sibling
qlaunch has one:

- a config document, `pige.json`, in `$XDG_CONFIG_HOME/bitshift/pige`
  (`~/.config/bitshift/pige` on Linux), holding **accounts + settings** and
  nothing else;
- SQLite demoted to a cache that can be rebuilt from the servers;
- an **Advanced** screen in Settings with **Backup config**, **Restore config**,
  and **Factory Reset**, behaving like qlaunch's: dated backups in Downloads,
  a picker to restore one, and a destructive reset behind a confirmation;
- the shared **toast** so operation results appear as a small dark card at the
  bottom instead of the current full-width error bar.

The purpose is easy, safe transfer of a mail setup between phone and desktop
without dragging the mail cache or any secret along.

### Success looks like

- After first launch, `~/.config/bitshift/pige/pige.json` contains the accounts
  and settings in readable JSON.
- Backup writes `~/Downloads/pige-backup-YYYY-MM-DD-HHMMSS.json`; the Advanced
  screen reports it with a toast and the screen behind stays visible.
- Restore lists those backups newest-first and, after confirmation, replaces
  accounts and settings exactly.
- Factory Reset, after confirmation, leaves a working first-run app.
- Passwords never appear in `pige.json`, a backup, or any log; after a restore
  on a new device the user re-enters each password once.

---

## 2. Goals

1. A `pige_core::config` module owning a `ConfigDocument` (accounts + settings),
   serialised to JSON, written atomically to the platform config directory.
2. The config document is the **source of truth** for accounts and settings.
3. The SQLite `accounts` table is kept as a **derived mirror** so
   `ON DELETE CASCADE` keeps cleaning up folders, messages, threads, bodies,
   attachments, outbox rows, and the FTS index when an account goes away.
4. `api::Config` gains a `config_dir` and a platform **backup store**; the
   desktop shell and Android bridge supply them.
5. `api::backup_config`, `api::list_backups`, `api::restore_config`, and
   `api::factory_reset`, plus backup-file naming and validation, live in the
   core and are covered by tests.
6. An Advanced screen composed from the shared component kit, with a Settings
   entry point; a confirmation dialog for Factory Reset; a picker for Restore.
7. A shared `toast` component and a single toast channel in `App` (with a
   generation guard and a 4-second expiry), replacing the snackbar/error bar.
8. A one-time migration: an existing installation with accounts/settings in
   SQLite writes them into `pige.json` on first launch of the new version.

### Non-goals

- Backing up the mail cache, synced mail, or raw bodies.
- Backing up passwords (see §4, option A).
- Rich backup formats, compression, encryption, incremental backups, or cloud
  sync.
- Moving the SQLite cache into the config directory.
- Swipe-to-dismiss toasts, toast action buttons, or a toast queue.
- Changing sync, threading, search, or send behaviour.

### Anti-goals

- Any secret or password-derived material in `pige.json` or a backup file.
- Two live sources of truth for accounts that can drift at rest.
- A per-account database split (see §4).
- A second, parallel message channel beside the toast.
- Blocking the UI thread on config file or backup IO.

---

## 3. Constraints and inherited ground

- Rust edition 2024; `cargo test --workspace` and
  `cargo clippy --workspace --all-targets` are the gates, as in qlaunch.
- The core (`pige_core`) has no iced and no UI; all new logic lands there and is
  testable without a renderer.
- `pige_core` currently has no `dirs` dependency; the desktop shell computes XDG
  directories from the environment in `pige/src/platform.rs`. The config root
  follows the same rule so behaviour is testable via the process environment.
- Credentials stay in the platform store, keyed by `AccountId`: Linux Secret
  Service (`service = "pige"`, `username = "account:<id>"`); Android Keystore.
- Android has no XDG home; the config directory is app-private and supplied by
  the Kotlin bridge (mirroring qlaunch's `Context.filesDir`).
- qlaunch reuse is in scope: `platform/backup.rs` naming, the `BackupStore`
  shape, `components::toast`, `components::action_card`, and the Advanced rows.
  Code lifted carries a provenance note.
- `pige.json` may contain email addresses, usernames, and server hostnames —
  not secrets, but still private. On Linux it is written with mode `0600`.

---

## 4. Approaches considered

### Where accounts live

1. **Config document owns accounts + settings; SQLite is cache-only (chosen).**
   `pige.json` is portable and authoritative. Backup is a copy of the file.
   Cost: accounts leave the DB, so cascade integrity needs care (below).
2. **Settings in `pige.json`; accounts stay in SQLite; backup combines both.**
   Lower risk and a real config file, but the "config" is split, and backup
   serialises accounts from the DB rather than copying the config. Rejected:
   the request is a portable *configuration*, and a split one is not.

### Keeping referential integrity with Approach 1

1. **Derived mirror (chosen).** The config file is authoritative; the SQLite
   `accounts` table is rebuilt from it on startup and after every account
   change, purely so `ON DELETE CASCADE` and account-scoped joins keep working.
   A crash between the config write and the mirror write is repaired on next
   startup. No drift at rest.
2. **Drop the accounts table; explicit purge.** Cleaner conceptually, but every
   account-scoped table must be purged by hand forever; a missed table in a
   future migration silently orphans mail. Rejected.
3. **Keep accounts in SQLite and treat the file as a backup only.** Two live
   sources of truth. Rejected.

### One database or one per account

**One shared, account-scoped database (chosen).** Per-account databases make
deletion a file drop, but turn the unified inbox and all-accounts search into
cross-database queries (`ATTACH`, capped at ~10, or manual merge), and multiply
migrations and connections across a codebase whose API is a single `Store`.
The cache is disposable, so the only hard part is deletion, which the mirror
handles. Per-account cache files can be added later behind the same API if sync
contention ever matters.

### Passwords in the backup

1. **No passwords (chosen).** The JSON carries accounts + settings only; the
   user re-enters each password once after a restore. No new crypto, no
   passphrase flow, and nothing crackable ever lands in Downloads.
2. **Passphrase-encrypted password block.** Portable and within the
   "never plaintext" rule, but adds KDF + AEAD dependencies, passphrase entry
   at both ends, and a weak passphrase defeats it. Deferred; the JSON shape is
   additive, so a `secrets` section can be added without a format break.

### The message channel

1. **Replace the snackbar/error bar with the shared toast (chosen).** One
   channel, generation guard, 4-second expiry; every brief result flows through
   it, matching qlaunch.
2. **Keep the error bar and add a toast beside it.** Two overlapping message
   concepts. Rejected.

---

## 5. Architecture

### New crate module: `pige_core::config`

```
pige_core/src/config/
  mod.rs      # ConfigDocument, load/save, config paths, migration helpers
```

`ConfigDocument`:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument {
    /// Format version; bumped if the document shape changes incompatibly.
    pub version: u32,
    pub accounts: Vec<Account>,
    pub settings: Settings,
}
```

- `Account` and `Settings` already derive `Serialize`/`Deserialize`
  (`Settings` has `#[serde(default)]`); `Account` gains the derive if missing.
- `CURRENT_VERSION: u32 = 1`.
- `load(path) -> Result<ConfigDocument, ConfigError>`: missing file →
  `Ok(None)`-style default handled by caller; corrupt file → an error the shell
  can surface, **not** a silent reset.
- `save(path, &doc) -> Result<(), ConfigError>`: atomic write
  (`write_atomic`, adapted from qlaunch) with mode `0600` on Unix and a
  directory fsync.
- `account()`/`settings()` accessors, and `replace_accounts`,
  `replace_settings`, `remove_account`, `upsert_account` helpers that keep the
  document in memory and persist it.

Root resolution (testable from values, as qlaunch does):

```rust
pub fn config_root_from(xdg_config_home: Option<&Path>, home: Option<&Path>)
    -> Result<PathBuf, ConfigError>; // <base>/bitshift/pige
pub fn config_root() -> Result<PathBuf, ConfigError>; // reads env, uses ~
pub fn document_path(root: &Path) -> PathBuf; // root/pige.json
```

Rules: use `XDG_CONFIG_HOME` only when absolute; else `$HOME/.config`; else
error. On Android the shell passes the app-private config directory through
`api::Config`, so `config_root()` is only the Linux/default path.

`pige/src/platform.rs` grows a `config_dir` computed the same way the existing
`xdg_dir` computes data/cache: `$XDG_CONFIG_HOME/bitshift/pige` or
`$HOME/.config/bitshift/pige`. Android's `PigeBridge` supplies a `configDir`
(app-private).

### `api::Config`

```rust
pub struct Config {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub config_dir: PathBuf,
    pub credentials: Arc<dyn CredentialStore>,
    pub notifier: Arc<dyn Notifier>,
    pub opener: Arc<dyn Opener>,
    pub transport: Arc<dyn TransportFactory>,
    pub backups: Arc<dyn BackupStore>,
}
```

`BackupStore` is a new platform trait:

```rust
pub trait BackupStore: Send + Sync {
    fn write(&self, name: &str, bytes: &[u8]) -> platform::Result<()>;
    fn list(&self) -> platform::Result<Vec<String>>;
    fn read(&self, name: &str) -> platform::Result<Vec<u8>>;
    fn available(&self) -> bool;           // false when there is no Downloads dir
}
```

- `LinuxBackupStore` uses `dirs::download_dir()` (add `dirs` to the non-Android
  deps, mirroring qlaunch) and `platform::backup::{is_backup_name,
  sort_backup_names}`.
- `FakeBackupStore` is an in-memory map for tests.
- Android implementation crosses the existing `PigeBridge` JNI (MediaStore
  `Downloads`, mirroring qlaunch's `writeBackup`/`listBackups`/`readBackup`
  plus `hasAllFilesAccess`/`requestAllFilesAccess`). See §6 and §11.

### Account and settings flow

`AppState` keeps the document in memory:

```rust
struct AppState {
    config_doc: ConfigDocument,   // source of truth (accounts + settings)
    ...
}
```

- **Startup migration.** `init` loads `pige.json`. If it exists, it is
  authoritative. If it does not, accounts and settings are read from SQLite
  (the existing `meta` settings key and `accounts` table), written to
  `pige.json`, and the app continues from the document. A missing document on a
  genuinely fresh install writes a default document.
- **Writes.** Every account mutation (`add_account`, `update_account`,
  `rename_account`, `remove_account`) and `save_settings`:
  1. updates `config_doc` in memory;
  2. persists the document atomically;
  3. synchronises the SQLite mirror (`store.sync_accounts(&document.accounts)`,
     below). A failure to persist leaves memory unchanged and returns an error;
     the mirror runs only after the document is durable.
- **Reads.** `store.accounts()` continues to serve joins and the mirror;
  account data used to build snapshots comes from `config_doc`, so the
  document is the source of truth even if the mirror were somehow stale.
- `Settings` load/save moves out of the `meta` table. `load_settings` /
  `save_settings` become document operations; the old `meta` value is read only
  by the one-time migration and is then cleared, so a later downgrade cannot
  resurrect stale preferences.
- **Corrupt or unreadable document.** `init` never bricks the app and never
  overwrites a file it could not read. If `pige.json` exists but fails to
  parse, it is renamed to `pige.json.corrupt` (quarantine), accounts are then
  recovered from the SQLite mirror if it still holds any (the upgrade path),
  otherwise the document starts empty, and a warning is surfaced through the
  snapshot/events. An I/O or permission failure reading a file that *does*
  exist is treated differently: the document is left untouched and the failure
  is reported, because overwriting it could destroy the user's real config. A
  config root that cannot be resolved at all is an `init` error, consistent
  with the existing empty-`data_dir`/`cache_dir` check.
- **Pre-restore safety copy.** Before a restore replaces the document,
  the current `pige.json` is copied beside it as `pige.json.pre-restore`, so a
  surprising restore is always recoverable by hand.

### SQLite changes

- No schema change required for the mirror: the `accounts` table stays exactly
  as it is. `store.sync_accounts(&[Account])` is a new transactional method
  that is **selective**, never a blind delete-all:
  1. `DELETE FROM accounts WHERE id NOT IN (current ids)` — cascading away only
     the accounts the document no longer contains, and their cache;
  2. upsert each current account, so an id that survives a rename, edit, or
     restore keeps its folders, messages, threads, bodies, attachments, and
     outbox rows.
  This is what makes "rename keeps the cache" and "restore purges only removed
  accounts" both true.
- `upsert_account` / `remove_account` remain for internal use but are driven by
  the document. Factory Reset needs no special purge: writing a document with
  no accounts and calling `sync_accounts(&[])` cascades everything via
  `ON DELETE CASCADE`. Any non-account cache (raw body/attachment files already
  on disk) is cleared separately, as today.

### Backup, restore, factory reset (core API)

Naming, adapted from qlaunch:

- `BACKUP_PREFIX = "pige-backup-"`, suffix `.json`,
  `pige-backup-YYYY-MM-DD-HHMMSS.json` (UTC), `is_backup_name` strict,
  `sort_backup_names` newest-first. Lives in `pige_core::platform::backup`
  (new module), unit-tested with the same edge cases qlaunch uses.

API:

- `backup_config() -> Result<String, String>`: serialise `config_doc` (no
  passwords) and `backups.write(&name, &bytes)`; return the name.
- `list_backups() -> Result<Vec<String>, String>`.
- `restore_config(name) -> Result<ConfigOutcome, String>`: validate the name,
  read + parse, **then**:
  1. copy the current document to `pige.json.pre-restore`;
  2. replace `config_doc` (accounts + settings) and persist it;
  3. **delete the stored credential of every account the backup dropped**
     (`credentials.delete(id)` and `delete_session(id)`, exactly as
     `remove_account` does), so no orphaned secret lingers and cannot re-attach
     if the same account id is restored later;
  4. `store.sync_accounts(&new accounts)`, cascading only the dropped accounts'
     cache;
  5. reset core selection state (`selected_folder`, `selected_conversation`,
     `server_more`, `conversation_limit`) exactly as `remove_account` does, so
     a reader cannot point at a removed account.
  A parse or IO failure leaves the current setup untouched.
- `factory_reset() -> Result<ConfigOutcome, String>`: delete every stored
  credential (via `CredentialStore`), write a default document (no accounts,
  default settings), `sync_accounts(&[])` (cascading all cache rows), clear
  remaining cache/attachment directories, reset core selection state.

`ConfigOutcome` is new and deliberately small:

```rust
pub struct ConfigOutcome {
    pub snapshot: ModelSnapshot,
    pub settings: Settings,
}
```

`ModelSnapshot` carries no settings, so restore/reset must return them for the
shell to refresh `App.settings`, re-apply the theme, and re-run
`apply_background_sync` (Android WorkManager). The shell routes this through
the existing `SettingsLoaded` path plus `apply_background_sync`, not a new
settings channel.

**Post-restore passwords.** Restoring does not add a guided password flow.
Accounts come back without secrets, and a restored account that needs a
password already fails sync with the existing "no password is stored for this
account; edit the account and enter it again" message. The restore toast names
this plainly ("Config restored from <name>. Re-enter passwords for any accounts
that need them.") and the existing Accounts screen's editor is the entry point.
Building a dedicated multi-account credential wizard is out of scope.

The user-facing copy (`backup_message`, `restore_message`, the "no backups"
and "needs All files access" strings) lives beside the screen, as in qlaunch.

### UI

- **Shared components (`pige/src/components/`).**
  - New `action_card` builder in the kit (title, subtitle, leading glyph,
    message), adapted from qlaunch's `components.rs`, using pige's
    `Palette`/`TextRole` tokens.
  - Replace `chrome::snackbar` with `chrome::toast` (content-hugging container,
    `max_width`, `SURFACE_CONTAINER_HIGHEST`, `ON_SURFACE`, rounded, subtle
    shadow). New `theme.metrics` tokens: `toast_max_width`, `toast_radius`,
    `toast_margin`; drop `snackbar_height`.
- **Wait — `chrome::toast` returns an element; the dark styling is a new
  `toast_style()`.** Keep `inverse_surface` tokens (still part of the scheme)
  but stop using them for the message channel.
- **Advanced screen (`pige/src/screens/advanced.rs`).** `Row` enum with
  `Backup`, `Restore`, `FactoryReset`; label/subtitle/icon helpers; `view`
  composes `action_card` rows inside the standard app-bar + scrollable
  scaffold, mirroring `screens/settings.rs`.
- **Settings entry.** An "Advanced" card/button in `screens/settings.rs` that
  opens the Advanced screen.
- **Dialogs.** A confirmation dialog for Factory Reset (destructive) and a
  Restore picker listing backup names. Reuse/replace `components::dialog` with
  the shape pige needs; the picker lists names, selecting one opens the
  confirmation, confirming calls `restore_config`.
- **Toast channel (`App`).** Replace `error: Option<String>` with
  `toast: Option<(u64, String)>` + `toast_generation: u64`; add
  `Message::Toast(String)` and `Message::ToastExpired(u64)`; `Message::Error`
  routes into `Toast`. `view` stacks the toast bottom-centre with insets +
  margin. The existing `DismissError`/close button goes away (toasts
  auto-expire, as in qlaunch).
- **App routing.** Add `advanced_open: bool` alongside `settings_open` /
  `accounts_open`, plus `Message::OpenAdvanced`/`CloseAdvanced`. Back navigation
  from Advanced returns to Settings.

### Android

- Config directory: app-private, supplied by `PigeBridge` (a `configDir`
  helper, or `filesDir/config`).
- Backups: shared Downloads via MediaStore, mirroring qlaunch — Kotlin
  `writeBackup`/`listBackups`/`readBackup`, `hasAllFilesAccess`, and
  `requestAllFilesAccess`, wired through `pige/src/platform.rs` (a
  `storage`-style helper selecting the Android or always-true path) exactly as
  qlaunch's `storage.rs` does. The `MANAGE_EXTERNAL_STORAGE` permission and the
  system settings intent are added to the manifest/bridge.
- Device verification (install, backup, restore, permission prompt, reset) is
  recorded separately; the Rust logic is identical across platforms and is
  covered by Linux/fake tests.

---

## 6. Behaviour

| Concern | Linux | Android |
| :--- | :--- | :--- |
| Config document | `$XDG_CONFIG_HOME/bitshift/pige/pige.json` (`~/.config/bitshift/pige/pige.json`), mode 0600 | app-private `configDir/pige.json` |
| Cache | unchanged: `$XDG_DATA_HOME/pige/pige.db` | unchanged: app data dir |
| Backups | `dirs::download_dir()` (`~/Downloads`) | shared Downloads via MediaStore |
| Backup name | `pige-backup-YYYY-MM-DD-HHMMSS.json` | same |
| Missing Downloads | Backup/Restore show a toast; other rows still work | permission request, then retry |
| Restore failure | toast with the error; setup unchanged | same |
| Factory Reset | confirmation, then wipe accounts/settings/cache/credentials | same |
| Passwords | Secret Service, never in the document/backup | Keystore, never in the document/backup |

---

## 7. Security and privacy

- No secret or password-derived material in `pige.json` or backups (option A).
- The document is written atomically and, on Unix, with mode `0600`. The
  containing directory is created with owner-only permissions.
- Backup files land in shared Downloads and therefore hold account config in
  the clear; this is the accepted tradeoff for portability and is stated in the
  Advanced screen's subtitles. It is not a secret, but it is private.
- Credentials continue to live only in the platform store; Factory Reset
  deletes them explicitly.
- Restore validates the file name strictly before reading, so a listing cannot
  name a file outside Downloads.
- A corrupt or unknown `pige.json` is surfaced as an error rather than silently
  resetting a working installation.

---

## 8. Testing and verification

Core (no renderer):

- `config`: root resolution (absolute vs relative vs missing XDG/home), atomic
  save/load round-trip, corrupt document error, unknown-field tolerance and
  version handling.
- `platform::backup`: name generation at and before the epoch, strict
  validation (the qlaunch edge cases), newest-first sorting.
- account flow: add/update/rename/remove persist the document first and rebuild
  the mirror; removing an account cascades its cache rows; a failed document
  write leaves state unchanged.
- settings flow: load/save via the document; migration from the old `meta`
  value.
- migration: a store with accounts + settings and no document produces
  `pige.json` and preserves both; a subsequent run reads the document.
- backup/restore/reset: backup writes a correctly named file; list is
  newest-first; restore replaces accounts+settings and purges removed accounts'
  cache; a bad name/bad file leaves state untouched; factory reset empties
  accounts, settings, cache, and calls credential deletion.
- `BackupStore` fake tests cover "no Downloads" (`available() == false`).

UI:

- `advanced`: row order and copy; `backup_message`/`restore_message`; factory
  reset requires confirmation; empty backup list yields a toast, not a dialog.
- toast: `toast_style` is `SURFACE_CONTAINER_HIGHEST`/`ON_SURFACE`; a headless
  layout test asserts the toast hugs its text height (regression guard for the
  `center_y(Length::Fill)` stretch, as qlaunch added).
- message routing: `Message::Error` shows a toast; expiry only clears its own
  generation.

Gates: `cargo test --workspace`, `cargo clippy --workspace --all-targets`.
Android: `JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release`; device steps
recorded in the technical notes when a device is available.

---

## 9. Sequencing (rough)

1. `config` module + `platform::backup` + `BackupStore` trait/impls + tests.
2. `api::Config` wiring (config_dir, backups) in the desktop and fake shells.
3. Document-owned accounts/settings + SQLite mirror + migration + tests.
4. `api` backup/restore/factory-reset + tests.
5. Shared `toast` (component + App channel + view) and `action_card`.
6. Advanced screen, Settings entry, confirmation and picker dialogs, routing.
7. Android bridge (config dir + MediaStore backups) and manifest.
8. Gates, technical notes, commit.

---

## 10. Calls I made (check these)

- One shared SQLite cache database; accounts owned by `pige.json`; the SQLite
  `accounts` table kept as a derived mirror for cascade.
- No passwords in the config or backups.
- Config path `$XDG_CONFIG_HOME/bitshift/pige/pige.json`, mode 0600; backups in
  Downloads (`dirs::download_dir()`), matching qlaunch.
- Factory Reset wipes accounts, stored credentials, cached mail, and settings.
- The shared toast **replaces** the existing error bar; errors auto-expire
  after 4 seconds, as in qlaunch, losing the manual dismiss button.
- The Advanced screen carries only Backup, Restore, and Factory Reset (pige has
  no restart/calendar/shortcut rows to port).
- Restore keeps cache for accounts whose ids survive and purges cache for
  accounts that do not; it does not purge everything.
- The SQLite mirror is synchronised **selectively** (`sync_accounts`), never by
  a blind delete-all, so renaming or editing an account cannot drop its cache.
- Restore deletes the stored credentials of accounts the backup dropped, so no
  orphaned secret remains.
- `restore_config`/`factory_reset` return `{ snapshot, settings }`
  (`ConfigOutcome`) so the shell can refresh settings and reschedule background
  sync; they also reset core selection state.
- A corrupt `pige.json` is quarantined to `pige.json.corrupt` and recovered (or
  reported), never silently overwritten or silently reset.
- No guided multi-account password wizard; the restore toast points at the
  existing account editor.
- A pre-restore copy (`pige.json.pre-restore`) is kept beside the document.

## 11. Left to the builder

Exact `ConfigDocument` field naming and version-migration mechanics; whether
the old `meta` settings key is cleared after migration; the precise
`action_card`/dialog geometry within pige's existing metrics; the restore
picker's presentation (modal vs inline) if a modal proves awkward on compact
widths; Android MediaStore details (`RELATIVE_PATH`, MIME type, permission
prompt UX) and the exact `PigeBridge` method names; the final toast
`max_width` on phone vs tablet; whether the Advanced screen also shows the
config path for transparency.
