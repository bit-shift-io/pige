# Advanced settings, portable config, backup/restore, shared toast — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move accounts + settings into a portable `~/.config/bitshift/pige/pige.json`, demote SQLite to a cache with a derived account mirror, and add qlaunch-style Backup/Restore/Factory Reset on an Advanced screen with a shared toast.

**Architecture:** `pige_core::config` owns a `ConfigDocument` (accounts + settings) written atomically. `AppState` holds the document as the source of truth; the SQLite `accounts` table is re-projected selectively after every change so `ON DELETE CASCADE` still cleans the cache. Backup/restore/reset live in the core API and use a new `BackupStore` platform trait (Downloads on Linux, MediaStore on Android via the existing JNI bridge). The iced shell gains a shared `toast` channel and an Advanced screen built from the shared kit.

**Tech Stack:** Rust 2024, iced (qlaunch fork), rusqlite, serde/serde_json, `dirs` (new, non-Android), JNI/Kotlin for Android.

**Spec:** `docs/superpowers/specs/2026-11-05-advanced-settings-backup-design.md`

## Global Constraints

- Rust edition 2024; `pige_core` has no iced and no UI. All new logic is core-testable.
- Gates: `cd rust && cargo test --workspace` and `cd rust && cargo clippy --workspace --all-targets` (clean, no warnings).
- Config document path is `<config_dir>/pige.json`; on Linux `<config_dir>` is `$XDG_CONFIG_HOME/bitshift/pige` or `$HOME/.config/bitshift/pige`. Written mode `0600` on Unix, atomically.
- Backup names: `pige-backup-YYYY-MM-DD-HHMMSS.json` (UTC), strict `is_backup_name`, newest-first sort.
- No password or password-derived material in `pige.json` or any backup.
- Credentials stay in the platform store, keyed by `AccountId`; Factory Reset and restoring-away an account delete them.
- The SQLite mirror is synchronised **selectively** (`sync_accounts`), never a blind delete-all.
- New UI is composed from `theme`/`components` tokens; no hardcoded colours, radii, or spacing. Code lifted from qlaunch carries a provenance comment.
- qlaunch is the reference for naming, copy, and control flow: `/home/bronson/Projects/qlaunch` (`rust/qlaunch_core/src/platform/backup.rs`, `rust/qlaunch/src/screens/advanced.rs`, `rust/qlaunch/src/components.rs`, `rust/qlaunch_core/src/store/atomic.rs`).

## Review Focus

1. **Renaming or editing an account must not drop its cached mail.** `sync_accounts` must delete only ids absent from the document, never delete-all. Covered in Task 4 and exercised by Task 5.
2. **A restore that drops an account must delete that account's stored password.** Otherwise the secret lingers and can re-attach to a later restore of the same id. Covered in Task 8.
3. **A corrupt `pige.json` must never be overwritten or silently reset.** It is quarantined, then recovered from the SQLite mirror. Covered in Task 5.
4. **A failed document write must leave in-memory state unchanged**, so the UI cannot show an account the disk does not have. Covered in Task 5.
5. **Restore/reset must refresh the running shell's settings and background-sync schedule** (settings live outside `ModelSnapshot`). Covered in Tasks 8, 9, and 13.

---

### Task 1: `config` module — document, paths, atomic persistence

**Files:**
- Create: `rust/pige_core/src/config/mod.rs`
- Modify: `rust/pige_core/src/lib.rs` (add `pub mod config;`)
- Test: inline `#[cfg(test)] mod tests` in `config/mod.rs`

**Interfaces:**
- Produces: `config::{CURRENT_VERSION, ConfigDocument, ConfigError, config_root_from, config_root, document_path, load, save, quarantine, pre_restore_copy}`.

- [ ] **Step 1: Write failing tests** in `config/mod.rs`

```rust
#[test] fn root_is_xdg_config_home_bitshift_pige() {
    assert_eq!(config_root_from(Some(Path::new("/x")), None).unwrap(),
               PathBuf::from("/x/bitshift/pige"));
}
#[test] fn root_rejects_relative_xdg_and_falls_back_to_home() {
    assert_eq!(config_root_from(Some(Path::new("rel")), Some(Path::new("/home/me"))).unwrap(),
               PathBuf::from("/home/me/.config/bitshift/pige"));
}
#[test] fn root_errors_without_xdg_or_home() {
    assert!(config_root_from(None, None).is_err());
}
#[test] fn save_then_load_round_trips() {
    // temp dir; doc with one Account and non-default Settings
    // save(&path, &doc); assert load(&path).unwrap() == Some(doc)
}
#[test] fn load_missing_is_none() { assert_eq!(load(Path::new("/nope/pige.json")).unwrap(), None); }
#[test] fn load_corrupt_is_an_error_not_a_default() {
    // write "{" ; assert matches!(load(&path), Err(ConfigError::Json(_)))
}
#[test] fn quarantine_renames_the_bad_file() {
    // load error path is caller-driven; assert quarantine(&path) moves it to pige.json.corrupt
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core config:: -- --nocapture`
Expected: FAIL — module/functions not defined.

- [ ] **Step 3: Implement `config/mod.rs`**

Signatures (bodies per the spec §5 and qlaunch `store/atomic.rs`):

```rust
pub const CURRENT_VERSION: u32 = 1;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument { pub version: u32, #[serde(default)] pub accounts: Vec<Account>, #[serde(default)] pub settings: Settings }
impl Default for ConfigDocument { /* version CURRENT_VERSION, empty, Settings::default() */ }
#[derive(Debug, thiserror::Error)]
pub enum ConfigError { #[error("io error: {0}")] Io(#[from] std::io::Error), #[error("json error: {0}")] Json(#[from] serde_json::Error), #[error("no configuration directory is available")] NoRoot }
pub fn config_root_from(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf, ConfigError>;
pub fn config_root() -> Result<PathBuf, ConfigError>;                 // reads XDG_CONFIG_HOME, dirs::home_dir()
pub fn document_path(root: &Path) -> PathBuf;                        // root.join("pige.json")
pub fn load(path: &Path) -> Result<Option<ConfigDocument>, ConfigError>;
pub fn save(path: &Path, doc: &ConfigDocument) -> Result<(), ConfigError>;
pub fn quarantine(path: &Path) -> Result<PathBuf, ConfigError>;      // rename to "<path>.corrupt"
pub fn pre_restore_copy(path: &Path) -> Result<(), ConfigError>;     // copy to "<path>.pre-restore" if exists
```

`save` writes through a private `write_atomic(path, bytes)` adapted from qlaunch's `store/atomic.rs`: unique `.<name>.<uuid>.simple().tmp`, `create_new`, `write_all`, `sync_all`, `rename`, then directory fsync; on Unix open with `OpenOptions::new().mode(0o600)` and `std::fs::set_permissions` for an existing file. Add `uuid` is already a dependency.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core config::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/config/mod.rs rust/pige_core/src/lib.rs
git commit -m "feat(core): add portable config document with atomic 0600 persistence"
```

---

### Task 2: backup naming and the `BackupStore` platform trait

**Files:**
- Create: `rust/pige_core/src/platform/backup.rs` (naming, adapted from qlaunch `platform/backup.rs`)
- Modify: `rust/pige_core/src/platform/mod.rs` (trait + `pub mod backup;`)
- Modify: `rust/pige_core/src/platform/linux.rs` (`LinuxBackupStore`)
- Modify: `rust/pige_core/src/platform/fake.rs` (`FakeBackupStore` + `FakeBackupStore::with_downloads`)
- Modify: `rust/pige_core/Cargo.toml` (add `dirs = "7.0.0"` to the non-Android target deps)
- Test: inline tests in `backup.rs`, `fake.rs`

**Interfaces:**
- Produces: `platform::backup::{BACKUP_PREFIX, BACKUP_SUFFIX, backup_name, is_backup_name, sort_backup_names}`; `platform::BackupStore` (`write`, `list`, `read`, `available`); `platform::linux::LinuxBackupStore::{new, with_downloads}`; `platform::fake::FakeBackupStore`.

- [ ] **Step 1: Write failing tests** — port qlaunch's `backup.rs` tests (`qlaunch-backup-` → `pige-backup-`): name at `1_792_591_530_000` is `pige-backup-2026-10-21-140530.json`; epoch and negative ms; `sort_backup_names` keeps only matching names newest-first; `is_backup_name` rejects the same eight bad shapes qlaunch rejects. Add a fake test: `write/list/read` round-trip and `with_downloads(None).available() == false`.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core platform::backup`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
// platform/backup.rs — constants, backup_name (civil_from_days), is_backup_name, sort_backup_names
// platform/mod.rs
pub trait BackupStore: Send + Sync {
    fn write(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn list(&self) -> Result<Vec<String>>;
    fn read(&self, name: &str) -> Result<Vec<u8>>;
    fn available(&self) -> bool;
}
```
`LinuxBackupStore { downloads: Option<PathBuf> }`: `new()` uses `dirs::download_dir()`; `with_downloads` for tests; `write` uses `config::write_atomic`-style or `std::fs::write` (backups need not be 0600); guard names with `is_backup_name`.
`FakeBackupStore` holds `Mutex<BTreeMap<String, Vec<u8>>>` and an `available: bool`.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core platform::backup platform::fake`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/platform/backup.rs rust/pige_core/src/platform/mod.rs rust/pige_core/src/platform/linux.rs rust/pige_core/src/platform/fake.rs rust/pige_core/Cargo.toml
git commit -m "feat(core): add backup naming and BackupStore platform trait"
```

---

### Task 3: `api::Config` gains `config_dir` and `backups`

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs` (`Config` struct)
- Modify: `rust/pige/src/platform.rs` (linux + android + fake branches)
- Modify: every `api::Config { .. }` literal in `rust/pige_core/tests/*.rs` and `rust/pige/src/*`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Consumes: `config`, `platform::BackupStore`, `platform::linux::LinuxBackupStore`, `platform::fake::FakeBackupStore`.
- Produces: `Config { data_dir, cache_dir, config_dir, credentials, notifier, opener, transport, backups }`.

- [ ] **Step 1: Write a failing test** in `tests/api.rs` asserting `api::init` succeeds with a temp `config_dir` and a `FakeBackupStore`, and that `api::snapshot()` is usable.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core --test api`
Expected: FAIL — `Config` has no `config_dir`.

- [ ] **Step 3: Add the fields and update every literal**

Add `pub config_dir: PathBuf` and `pub backups: Arc<dyn BackupStore>` to `Config`. Update the intro doc-comment. In `pige/src/platform.rs`: Linux uses `xdg_dir("XDG_CONFIG_HOME", ".config")` + `/bitshift/pige` (add a `config_root` helper mirroring `xdg_dir`, but with the qlaunch vendor path) and `LinuxBackupStore::new()`; the fake branch uses `root.join("config")` and `FakeBackupStore::new()`. Android uses `data_dir.join("config")` and a temporary `FakeBackupStore::new()` so the target still compiles; Task 14 replaces both with the real bridge. Update every test harness literal (grep `Config {` under `rust/`), giving each a temp `config_dir` and `FakeBackupStore::new()`.

- [ ] **Step 4: Run the workspace tests**

Run: `cd rust && cargo test --workspace`
Expected: PASS (compiles; existing tests still green).

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat(core): thread config_dir and backup store through api::Config"
```

---

### Task 4: selective `Store::sync_accounts`

**Files:**
- Modify: `rust/pige_core/src/store/accounts.rs`
- Modify: `rust/pige_core/src/store/mod.rs` (re-export if needed)
- Test: `rust/pige_core/tests/store.rs` or inline in `accounts.rs`

**Interfaces:**
- Consumes: `Account`.
- Produces: `Store::sync_accounts(&self, accounts: &[Account]) -> Result<(), StoreError>`; internal `upsert_account_on(conn, account)`.

- [ ] **Step 1: Write the failing test** — seed two accounts with folders/messages, call `sync_accounts` with one renamed (same id) and a new one, assert the renamed account's cached messages survive and the dropped account's rows are gone; assert a NEW account is inserted.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core sync_accounts`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
pub fn sync_accounts(&self, accounts: &[Account]) -> Result<(), StoreError> {
    let conn = self.connection();
    let tx = conn.unchecked_transaction()?;
    let existing = account_ids_on(&tx)?;              // Vec<String>
    let keep: HashSet<&str> = accounts.iter().map(|a| a.id.0.as_str()).collect();
    for id in existing.iter().filter(|id| !keep.contains(id.as_str())) {
        tx.execute("DELETE FROM accounts WHERE id = ?1", [id])?;   // cascades
    }
    for account in accounts { upsert_account_on(&tx, account)?; }  // existing upsert body
    tx.commit()?;
    Ok(())
}
```
Factor the existing `upsert_account` body into `pub(crate) fn upsert_account_on(conn: &Connection, account: &Account) -> Result<(), StoreError>`; `upsert_account` calls it with `self.connection()`.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core store`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/store/accounts.rs rust/pige_core/src/store/mod.rs
git commit -m "feat(core): add selective Store::sync_accounts mirror"
```

---

### Task 5: `AppState` owns the document; accounts and migration

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs` (state fields, `init`, account mutations, helpers)
- Test: `rust/pige_core/tests/api.rs`, `rust/pige_core/tests/mutations.rs`

**Interfaces:**
- Consumes: `config::{load, save, document_path, quarantine, pre_restore_copy}`, `Store::sync_accounts`.
- Produces: `AppState { config_doc, config_path, .. }`; a private `persist_accounts(state) -> Result<(), String>` that saves the document then calls `sync_accounts`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn first_run_migrates_sqlite_accounts_into_pige_json() { /* seed a store with an account + meta settings, init with a temp config_dir, assert pige.json exists and equals the account */ }
#[test] fn a_present_document_is_authoritative_over_sqlite() { /* write a doc with a distinct label, init, assert snapshot shows it */ }
#[test] fn corrupt_document_is_quarantined_and_recovered() { /* write "{" to pige.json, seed sqlite account, init, assert pige.json.corrupt exists and account survived */ }
#[test] fn rename_keeps_cached_mail() { /* add account, cache a message, rename_account, assert message still present */ }
#[test] fn a_failed_document_write_leaves_memory_unchanged() { /* point config_dir at an unwritable path, rename_account, assert Err and snapshot label unchanged */ }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core --test api`
Expected: FAIL.

- [ ] **Step 3: Implement**

Add `config_doc: ConfigDocument` and `config_path: PathBuf` to `AppState`. In `init`: resolve `config_path = config::document_path(&config.config_dir)`; `match config::load(&config_path)`: `Ok(Some(doc))` → use it and `store.sync_accounts(&doc.accounts)`; `Ok(None)` → build a document from `store.accounts()` + `settings::load_settings(&store)`, `config::save` it (warn, don't fail, if the save fails); `Err(ConfigError::Json(_))` → `config::quarantine`, rebuild from SQLite, save; `Err(ConfigError::Io(_))` → return `Err` (never overwrite). In `add_account_inner` / `update_account_inner` / `rename_account` / `remove_account`: after computing the new `Vec<Account>`, set `state.config_doc.accounts`, then `config::save(&state.config_path, &state.config_doc)` and `state.store.sync_accounts(...)`; any error returns before mutating the snapshot. Account reads in `build_snapshot` continue through `store.view` (the mirror), so no call-site churn.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/
git commit -m "feat(core): config document is the source of truth for accounts"
```

---

### Task 6: settings live in the document

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs` (`settings()`, `save_settings`)
- Modify: `rust/pige_core/src/settings.rs` (keep `load_settings` for migration; remove `save_settings`)
- Test: `rust/pige_core/tests/api.rs`, inline `settings.rs`

**Interfaces:**
- Produces: `api::settings()` and `api::save_settings(&Settings)` reading/writing `config_doc.settings`.

- [ ] **Step 1: Write failing tests** — `save_settings` then re-`init` preserves values via `pige.json` and not the `meta` table; a pre-existing `meta` settings value is migrated into the document and the `meta` key is cleared.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core settings`
Expected: FAIL.

- [ ] **Step 3: Implement** — `settings()` returns `state.config_doc.settings.clone()`; `save_settings` clamps, sets `config_doc.settings`, `config::save`, returns. Migration in Task 5 reads `settings::load_settings(&store)` and then `store.set_meta`/delete the key. Delete `settings::save_settings` and its test.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/src/settings.rs rust/pige_core/tests/
git commit -m "feat(core): persist settings in pige.json"
```

---

### Task 7: `backup_config` and `list_backups`

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Produces: `api::backup_config() -> Result<String, String>`, `api::list_backups() -> Result<Vec<String>, String>`.

- [ ] **Step 1: Write failing tests** — backup writes a `pige-backup-*.json` file through the fake store containing the document's accounts/settings but no password field; `list_backups` is newest-first; `backup_config` errors when `backups.available()` is false.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core --test api backup`
Expected: FAIL.

- [ ] **Step 3: Implement** — under the state lock, `serde_json::to_vec_pretty(&state.config_doc)`, compute `backup::backup_name(now_ms())`, `state.config.backups.write(&name, &bytes)`, return the name. `list_backups` calls `state.config.backups.list()`. Add a `now_ms()` helper.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/api.rs
git commit -m "feat(core): backup_config and list_backups"
```

---

### Task 8: `restore_config`

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Produces: `pub struct ConfigOutcome { pub snapshot: ModelSnapshot, pub settings: Settings }`; `api::restore_config(file_name: String) -> Result<ConfigOutcome, String>`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn restore_replaces_accounts_and_settings() { /* back up doc A, add account B, restore A, assert B gone and settings == A */ }
#[test] fn restore_keeps_cache_for_surviving_accounts_and_drops_the_rest() { /* cache mail for A and B; restore a doc with only A; assert A's mail present, B's gone */ }
#[test] fn restore_deletes_credentials_of_dropped_accounts() { /* fake credentials for A and B; restore doc with only A; assert B's secret is gone, A's remains */ }
#[test] fn restore_rejects_a_bad_name_and_leaves_state() { /* restore_config("evil.json") is Err; document unchanged */ }
#[test] fn restore_writes_a_pre_restore_copy() { /* pige.json.pre-restore exists after restore */ }
```

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core --test api restore`
Expected: FAIL.

- [ ] **Step 3: Implement** — validate with `backup::is_backup_name`; `backups.read`; `serde_json::from_slice`; then under the lock: compute dropped ids, `credentials.delete(id)` + `delete_session(id)` for each, `config::pre_restore_copy`, set `config_doc`, `config::save`, `store.sync_accounts`, reset `selected_folder`/`selected_conversation`/`server_more`/`conversation_limit` as `remove_account` does, build the snapshot, return `ConfigOutcome { snapshot, settings: doc.settings.clone() }`. A parse/read error returns before any mutation.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/api.rs
git commit -m "feat(core): restore_config with credential and cache reconciliation"
```

---

### Task 9: `factory_reset`

**Files:**
- Modify: `rust/pige_core/src/api/mod.rs`
- Test: `rust/pige_core/tests/api.rs`

**Interfaces:**
- Produces: `api::factory_reset() -> Result<ConfigOutcome, String>`.

- [ ] **Step 1: Write failing tests** — after seeding accounts, cache, and credentials, `factory_reset` empties accounts, restores default settings, clears cached rows, and deletes every credential; `snapshot.accounts` is empty and `settings == Settings::default()`.

- [ ] **Step 2: Run to verify failure**

Run: `cd rust && cargo test -p pige_core --test api factory`
Expected: FAIL.

- [ ] **Step 3: Implement** — under the lock: for each current account, `credentials.delete` + `delete_session`; `config_doc = ConfigDocument::default()`; `config::save`; `store.sync_accounts(&[])` (cascades all cache); reset selection; best-effort clear the attachment/raw cache directory; build snapshot; return `ConfigOutcome`.

- [ ] **Step 4: Run to verify pass**

Run: `cd rust && cargo test -p pige_core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/api/mod.rs rust/pige_core/tests/api.rs
git commit -m "feat(core): factory_reset wipes accounts, settings, cache and credentials"
```

---

### Task 10: shared `toast` replaces the snackbar

**Files:**
- Modify: `rust/pige/src/components/chrome.rs` (replace `snackbar` with `toast` + `toast_style`)
- Modify: `rust/pige/src/components/mod.rs` (re-export)
- Modify: `rust/pige/src/theme.rs` (`Metrics`: add `toast_max_width`, `toast_radius`, `toast_margin`; drop `snackbar_height`)
- Test: inline in `chrome.rs`

**Interfaces:**
- Produces: `components::toast(&str) -> Element<Message>`, `components::toast_style() -> container::Style`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn toast_style_is_a_dark_surface() { assert_eq!(toast_style().background, Some(theme::active().surface_container_highest.into())); }
#[test] #[ignore = "headless layout; run with --ignored"] fn toast_hugs_its_text_height() { /* adapt qlaunch's control_layout test */ }
```

- [ ] **Step 2: Run to verify failure** — `cd rust && cargo test -p pige toast` → FAIL.

- [ ] **Step 3: Implement** — port qlaunch `components::toast`/`toast_style` to pige's palette (`surface_container_highest`, `on_surface`, `radius.md`), `max_width(metrics.toast_max_width)`, `padding([14,16])`. Remove `snackbar` and `snackbar_height`.

- [ ] **Step 4: Run to verify pass** — `cd rust && cargo test -p pige toast` → PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige/src/components/chrome.rs rust/pige/src/components/mod.rs rust/pige/src/theme.rs
git commit -m "feat(ui): shared toast component replaces the snackbar"
```

---

### Task 11: the App toast channel

**Files:**
- Modify: `rust/pige/src/app.rs` (`error` → `toast` + generation; startup error via task)
- Modify: `rust/pige/src/message.rs` (add `Toast`, `ToastExpired`; drop `DismissError`)
- Modify: `rust/pige/src/update.rs` (route errors through `show_toast`; add arms)
- Modify: `rust/pige/src/view.rs` (toast stack instead of `error_overlay`)
- Test: `rust/pige/src/update.rs` inline, `rust/pige/src/app.rs` inline

**Interfaces:**
- Produces: `App { toast: Option<(u64, String)>, toast_generation: u64 }`; `App::show_toast(&mut self, String) -> Task<Message>`; `Message::Toast(String)`, `Message::ToastExpired(u64)`.

- [ ] **Step 1: Write failing tests** — `Message::Error("x")` puts `(gen,"x")` in `app.toast`; `ToastExpired` with the current generation clears it; an older generation does not clear a newer toast.

- [ ] **Step 2: Run to verify failure** — `cd rust && cargo test -p pige toast` → FAIL.

- [ ] **Step 3: Implement** — rename the field; `show_toast` increments the generation, stores `(generation, message)`, and returns `Task::perform(async move { sleep(4s).await; generation }, Message::ToastExpired)`. Convert every `self.error = Some(x)` to `return self.show_toast(x)` (locate with grep). `view.rs`: stack `components::toast` bottom-centre with insets + `toast_margin`; delete `error_overlay` and the dismiss button. `app.rs`: `toast: None`; if `init_error` is `Some`, push `Task::done(Message::Error(err))` onto the startup batch.

- [ ] **Step 4: Run to verify pass** — `cd rust && cargo test -p pige` → PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige/src/app.rs rust/pige/src/message.rs rust/pige/src/update.rs rust/pige/src/view.rs
git commit -m "feat(ui): single shared toast channel with generation guard"
```

---

### Task 12: `action_card`, the Advanced screen, and routing

**Files:**
- Create: `rust/pige/src/screens/advanced.rs`
- Modify: `rust/pige/src/screens/mod.rs`
- Modify: `rust/pige/src/components/mod.rs` (+ `action_card`)
- Modify: `rust/pige/src/screens/settings.rs` (Advanced entry)
- Modify: `rust/pige/src/icons.rs` (`BACKUP = '\u{e0c6}'`, `RESTORE = '\u{e534}'`, `DELETE_FOREVER = '\u{e1ba}'`)
- Modify: `rust/pige/src/app.rs` (`advanced_open`), `message.rs` (`OpenAdvanced`/`CloseAdvanced`), `update.rs`, `view.rs`
- Test: `rust/pige/src/screens/advanced.rs` inline

**Interfaces:**
- Produces: `screens::advanced::{Row, rows, view, backup_message, restore_message, NO_BACKUPS}`; `components::action_card(title, subtitle, icon, message)`; `App::advanced_open: bool`; `Message::{OpenAdvanced, CloseAdvanced}`.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn rows_are_backup_restore_reset() { assert_eq!(rows(), vec![Row::Backup, Row::Restore, Row::FactoryReset]); }
#[test] fn backup_message_names_the_downloads_file() { assert_eq!(backup_message(&Ok("pige-backup-2026.json".into())), "Backup saved to Downloads/pige-backup-2026.json."); }
#[test] fn restore_message_reports_success_and_failure() { /* Ok(ConfigOutcome) => copy names the file AND the password step; Err => "Could not restore: bad file" */ }
```

- [ ] **Step 2: Run to verify failure** — `cd rust && cargo test -p pige advanced` → FAIL.

- [ ] **Step 3: Implement** — `Row { Backup, Restore, FactoryReset }` with label/subtitle/icon; `view` composes `action_card` rows in an app-bar + scrollable scaffold like `settings.rs`, with a back button sending `CloseAdvanced`. Add `action_card` to the kit using palette tokens. Add the three icons. Add `advanced_open`, the two messages, routing in `update`/`view` (`advanced_open` takes precedence under `settings_open`, and Back returns to Settings), and an Advanced entry card in `settings.rs` sending `OpenAdvanced`.

- [ ] **Step 4: Run to verify pass** — `cd rust && cargo test -p pige` → PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/pige/src/screens/advanced.rs rust/pige/src/screens/mod.rs rust/pige/src/components/mod.rs rust/pige/src/screens/settings.rs rust/pige/src/icons.rs rust/pige/src/app.rs rust/pige/src/message.rs rust/pige/src/update.rs rust/pige/src/view.rs
git commit -m "feat(ui): Advanced screen with shared action cards"
```

---

### Task 13: Advanced flows — backup, restore picker, reset confirmation

**Files:**
- Modify: `rust/pige/src/message.rs` (`AdvancedBackup`, `AdvancedRestore`, `AdvancedFactoryReset`, `BackupDone`, `BackupsListed`, `RestoreChosen`, `RestoreCancelled`, `RestoreDone`, `ResetConfirmed`, `ResetCancelled`, `ResetDone`)
- Modify: `rust/pige/src/app.rs` (`restore_picker: Option<Vec<String>>`, `confirm_reset: bool`)
- Modify: `rust/pige/src/update.rs` (arms; apply `ConfigOutcome`)
- Modify: `rust/pige/src/view.rs` (stack the picker/confirm dialogs)
- Test: `rust/pige/src/update.rs` inline

**Interfaces:**
- Consumes: `api::{backup_config, list_backups, restore_config, factory_reset}`, `ConfigOutcome`, `components::dialog`.
- Produces: the flows described in spec §5/§6.

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn factory_reset_requires_confirmation() { /* AdvancedFactoryReset sets confirm_reset, does not call the API */ }
#[test] fn empty_backup_list_shows_a_toast_not_a_dialog() { /* BackupsListed(Ok(vec![])) => toast NO_BACKUPS, restore_picker none */ }
#[test] fn restore_success_applies_settings_and_snapshot() { /* RestoreDone(Ok(outcome)) sets app.settings, app.snapshot, closes dialogs */ }
```

- [ ] **Step 2: Run to verify failure** — `cd rust && cargo test -p pige` → FAIL.

- [ ] **Step 3: Implement** — `AdvancedBackup` → `Task::perform(api::backup_config(), Message::BackupDone)`; `BackupDone` → toast via `backup_message`. `AdvancedRestore` → `Task::perform(api::list_backups(), Message::BackupsListed)`; `BackupsListed(Ok(names))` → open `restore_picker` (or toast `NO_BACKUPS` when empty); `RestoreChosen` → `Task::perform(api::restore_config(name.clone()), move |r| Message::RestoreDone(name, r))`; `RestoreDone` → apply `ConfigOutcome` (set `self.settings`, apply the theme, `apply_background_sync`), toast `restore_message`, clear dialogs. `AdvancedFactoryReset` → set `confirm_reset`; `ResetConfirmed` → `Task::perform(api::factory_reset(), Message::ResetDone)`; `ResetDone` → apply `ConfigOutcome`, toast. Render the picker as a `components::dialog` list of names and the reset as a destructive confirm.

- [ ] **Step 4: Run to verify pass** — `cd rust && cargo test -p pige && cargo clippy --workspace --all-targets` → PASS/clean.

- [ ] **Step 5: Commit**

```bash
git add rust/pige/src/message.rs rust/pige/src/app.rs rust/pige/src/update.rs rust/pige/src/view.rs
git commit -m "feat(ui): wire backup, restore picker, and factory reset flows"
```

---

### Task 14: Android config dir and Downloads backups

**Files:**
- Modify: `rust/pige_core/src/platform/android.rs` (`AndroidPaths::{config_dir}`, `AndroidBackupStore`)
- Modify: `rust/pige/src/platform.rs` (android branch: `config_dir`, `AndroidBackupStore`)
- Modify: `android/app/src/main/kotlin/bitshift/pige/PigeBridge.kt` (`configDir`, `writeBackup`, `listBackups`, `readBackup`, `hasAllFilesAccess`, `requestAllFilesAccess`)
- Modify: `android/app/src/main/AndroidManifest.xml` (`MANAGE_EXTERNAL_STORAGE`)
- Modify: `rust/pige/src/storage.rs` (new: `has_all_files_access`, `request_all_files_access`; Android → JNI, else true/Ok)

**Interfaces:**
- Produces: `AndroidBackupStore` implementing `BackupStore`; `storage::{has_all_files_access, request_all_files_access}`.

- [ ] **Step 1: Build the Android target** to capture a failing/JNI-absent state

Run: `cd rust && cargo build -p pige --target aarch64-linux-android` (with the NDK linker env from `scripts/build.sh`)
Expected: FAIL until the JNI methods exist.

- [ ] **Step 2: Add the Kotlin methods and JNI wrappers** mirroring qlaunch's `QLaunchBridge` backup methods and `pige/src/storage.rs` mirroring qlaunch's `storage.rs`.

- [ ] **Step 3: Implement `AndroidBackupStore`** calling the bridge, parsing names with `platform::backup::sort_backup_names`.

- [ ] **Step 4: Build again**

Run: `JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release`
Expected: `BUILD SUCCESSFUL`. If the Android SDK/NDK is unavailable in this environment, record it and stop this task; the Linux feature and all core tests are unaffected.

- [ ] **Step 5: Commit**

```bash
git add rust/pige_core/src/platform/android.rs rust/pige/src/platform.rs rust/pige/src/storage.rs android/
git commit -m "feat(android): app-private config dir and shared-Downloads backups"
```

---

### Task 15: verification, technical notes, and final gates

**Files:**
- Modify: `docs/technical-notes.md` (record what was measured vs not run)
- Test: whole workspace

- [ ] **Step 1: Run the gates**

Run: `cd rust && cargo test --workspace && cargo clippy --workspace --all-targets`
Expected: all tests pass; clippy clean.

- [ ] **Step 2: Manual desktop smoke** — run the desktop binary, open Settings → Advanced, confirm Backup writes `~/Downloads/pige-backup-*.json`, Restore lists it, and Factory Reset empties accounts after confirmation. Record results.

- [ ] **Step 3: Update `docs/technical-notes.md`** with the new paths, what was exercised, and what was not (Android device steps, if unavailable).

- [ ] **Step 4: Commit**

```bash
git add docs/technical-notes.md
git commit -m "docs: record advanced settings and backup verification"
```
