# pige v1 — technical notes and measurements

**Date:** 2026-11-04
**Build:** v1, final verification pass (Task 18)
**Host:** Fedora (`bronson`), Rust edition 2024, JDK `/opt/jbr-21`, Android NDK
`28.2.13676358`
**Device:** `6bfcc911` — `iPlay60_mini_Turbo`, Android 14 (API 34), 1200×1920
physical, 320 dpi (≈ 600×960 logical points at the current density)

This file records what was actually measured and exercised for v1. Nothing here
is an estimate dressed up as a measurement; anything that could not be measured
is marked **not measured** with the reason.

---

## 1. Measurements

| Measurement | Result | Method |
| :--- | :--- | :--- |
| Release APK size | **28,753,648 bytes (27.42 MiB / 28.75 MB)** | `ls -l build/app/outputs/apk/release/app-release.apk` after `scripts/build.sh release` |
| Cold start, Android | **≈ 139 ms to first draw** (3 runs: 134, 139, 144 ms) | `adb shell am start -W -S -n bitshift.pige/.MainActivity` after `force-stop`; `am` reports `LaunchState: COLD` / `TotalTime` |
| Search latency, 1,000 conversations | **median 2.10 ms** (release), median 1.66 ms (debug) | `cargo test --release -p pige_core --test perf -- --ignored --nocapture` |
| Desktop frame time (GPU present) | **not measured** — see below | — |
| Desktop UI view build + layout (CPU proxy) | ~**7.0 ms** (expanded 1300 pt), ~**8.3 ms** (medium 900 pt), ~**7.4 ms** (compact 600 pt) median | `cargo test --release -p pige --lib shell_view_build_and_layout_time -- --ignored --nocapture` |

### Release APK

`build/app/outputs/apk/release/app-release.apk` is 27.42 MiB, of which:

- `lib/arm64-v8a/libpige.so` — 22,179,440 bytes (21.15 MiB), the stripped Rust
  core + iced + wgpu + SQLite;
- `classes.dex` — 6,467,268 bytes (6.17 MiB), mostly the Kotlin/AndroidX
  scaffolding pulled in by WorkManager and the iced Android runtime.

The release variant is signed with the debug key, so it installs directly.

### Cold start

The spec's "cold start to cached list" could not be measured: **no test account
was supplied**, so there is no cached mail to render. What was measured is the
cold start of the real release APK to the first drawn frame of the empty shell.
`am start -W` reported `LaunchState: COLD` and `TotalTime` of 134 / 139 / 144 ms
across three `force-stop` + start cycles (median 139 ms). A warm resume after
Backgrounding was `HOT` at 30 ms with the process still resident, confirming the
qlaunch window-lifecycle rule (backgrounding does not close the iced window).

### Search latency

`rust/pige_core/tests/perf.rs` (new, `#[ignore]`d) seeds 1,000 distinct
conversations in a temporary SQLite store, each with a cached body; one in ten
contains the keyword `quantum`, so a search ranks ~100 matches over 1,000 rows.
It runs five warm-ups and twenty measured searches through the same
`Store::search` path the shells call (limit 50).

Release build on this host:

```
search over 1000 conversations (100 matching threads):
min=1.93875ms median=2.103337ms mean=2.20614ms p95=2.568413ms max=3.274287ms over 20 runs
```

The debug-build run of the same test reported median 1.66 ms (min 1.61, p95
1.72, max 1.72). The two are within run-to-run noise; either way local search is
a low-single-digit-millisecond operation at this scale, comfortably inside the
spec's "well under a second".

### Desktop frame time

**Not measured — instrumentation was out of verification scope.** iced does
expose `window::frames()`, so a frame-time sampler is possible; wiring it into
the shell, attributing frames to interactions, and validating the numbers on
this host was beyond the task-18 scope. Rather than publish an
instrumentation-free estimate, no desktop frame time is claimed. What *was* done
honestly:

1. The release desktop binary was built (`cargo build --release -p pige --bin
   pige`) and run against the live Wayland session (`WAYLAND_DISPLAY=wayland-0`).
   It stayed up with no errors and `spectacle --activewindow` captured its
   window, rendering the medium/expanded two-pane layout (list + reader). So the
   desktop build does render on this machine.
2. As a focused proxy, `shell_view_build_and_layout_time` (new, `#[ignore]`d, in
   `rust/pige/src/lib.rs`) builds the view element, its widget tree, and lays out
   the real shell at 600 / 900 / 1300 pt with the headless wgpu renderer, warm-up
   + 50 timed runs each. Medians: 7.4 / 8.3 / 7.0 ms.

This CPU number excludes GPU rasterization (`Headless` cannot draw the widget
tree), so it is a **lower bound on frame time, not a frame time**. No GPU-present
figure is claimed.

---

## 2. Gates

```
cd rust && cargo test --workspace
cd rust && cargo clippy --workspace --all-targets
```

`cargo test --workspace`: **260 passed, 0 failed, 17 ignored** (the ignored tests
are the live-network tests, the headless layout/timing tests, and the perf test).
`cargo clippy --workspace --all-targets`: clean, no warnings.

```
JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release
```

→ `BUILD SUCCESSFUL in 52s`, producing the release APK above.

---

## 3. On-device verification (Android, `6bfcc911`)

### Exercised

- **Install** of the release APK — `Success`.
- **Cold start** — three `force-stop` + `am start -W -S` cycles (COLD, 134–144 ms).
- **Compact portrait layout** (rotation 0) — app bar ("All inboxes"), search
  field, and the "No accounts yet" empty state render correctly; no clipping.
- **Landscape rotation** (rotation 1) — the layout switches to the
  **medium-breakpoint two-pane** view (list + "Conversation" reader placeholder,
  "No conversation selected"). Layout stays sane; process survives the rotation.
- **Drawer** — hamburger opens the rail ("All inboxes", "Add account",
  "Settings"); Back closes the drawer and returns to the list.
- **Settings screen** — renders sync interval, notifications, theme, and the
  account list entry.
- **Add account form** — renders label/email/username/password fields, the Gmail
  preset with `imap.gmail.com` / `smtp.gmail.com`, and TLS/STARTTLS toggles.
- **Back navigation** — from the account form → settings → list; Back at the root
  is a no-op (recorded as a deferred minor).
- **Background/foreground** — HOME then relaunch resumed HOT (30 ms) with the
  same pid and a correctly redrawn window.
- **Stability** — the process (`bitshift.pige`) stayed alive throughout, and
  `logcat` showed **no `FATAL EXCEPTION`, no `AndroidRuntime` crash, and no
  panic** (`grep -iE "FATAL EXCEPTION|AndroidRuntime|panic"` empty). The only
  `pige`-tagged warnings are routine wgpu downlevel-capability notices.

### Not run — no test account supplied

No real Gmail/Fastmail account or app password was provided, so none of the
credentialed flows could be exercised. Recorded as **not run**:

- add Gmail account / first sync;
- unified inbox + account sections with real mail;
- open an HTML marketing mail (and confirm no remote fetch);
- search over real mail;
- compose / reply / forward;
- offline send queues and retry;
- pull-to-refresh;
- background notification after the 15-minute window, and notification tap;
- attachment opens via FileProvider.

The Linux desktop credentialed flows (three-pane ≥1200 / two-pane 840–1199,
keyring-fallback behaviour, D-Bus notification) were likewise **not run** for the
same reason. The release desktop app was confirmed to launch and render the
two-pane layout, but no account was connected.

The live IMAP/SMTP smoke test (`rust/pige_core/tests/live_imap.rs`) was **not
run** against a real server for the same reason. Its missing-environment
behaviour was verified: with the variables unset it fails with
`missing required environment variable(s): PIGE_TEST_IMAP_HOST,
PIGE_TEST_IMAP_USER, PIGE_TEST_IMAP_PASSWORD` rather than silently passing.

---

## 4. Qualitative comparison to Gmail

**These are qualitative observations, not measured results.** No side-by-side
number (APK size, memory, start time, frame rate) was measured against the Gmail
Android app, so no numeric win is claimed.

Qualitatively: pige ships no WebView or browser engine, no Play services, and no
analytics, and draws its UI natively with iced/wgpu; Gmail bundles a WebView and
Google Play infrastructure and renders web content. At 27.42 MiB the pige APK is
small, but without a measured Gmail APK on the same device that is an expectation,
not a demonstrated win. Likewise, search returning in ~2 ms over 1,000 cached
conversations is fast locally, but Gmail's search is server-side over the whole
mailbox, so the two are not directly comparable.

---

## 5. Deferred

### Deferred by design (v1 scope)

- **OAuth / provider sign-in** — app passwords only; the auth layer is abstracted
  so OAuth can be added later.
- **Background IDLE / foreground service** — Android uses periodic WorkManager
  (15-minute minimum) only; no push or IMAP IDLE daemon.
- **Rich-text composition** — plain-text compose in v1.
- **Sending attachments** — attachments are view-only in v1; forwarding drops
  them.
- **Server-side search fallback** — search covers local cache only; words that
  live only in un-downloaded bodies are not found until those bodies are fetched.
- **Replayable mutation queue** — offline mutation replay is not in v1; only
  sending is queued.
- **Remote image fetching** — the spec lists a "Load images" action, but v1
  fetches no remote bytes (there is deliberately no HTTP client). Blocked images
  stay as "[image blocked]" placeholders and the misleading "Load images" bar
  and message path were removed in the final fix wave (2026-11-04). This is a
  deliberate, recorded deviation from the spec.

### Deferred minors (from `.superpowers/sdd/2026-11-03-pige-v1/progress.md`)

- **T1:** forward-looking `INTERNET` permission; `run.sh` X fallback implicit;
  `black_box` smoke assertion.
- **T2:** theme-token polish (`on_color`, scrollbar alpha, scroll-fade extent);
  component hover/state polish; pull-to-refresh not wired; thin shell-test depth.
- **T3:** folder natural-key upsert; `SEEN` bit constant; stepwise migrations;
  thread cleanup on UIDVALIDITY reset; WAL assertion; extra store tests.
- **T4:** strict plausible-header check may reject tolerated malformed headers;
  lowercase non-list tags still stripped.
- **T5:** cached-only unread undercount (documented); temporary non-UUID thread id
  until Task 10.
- **T6:** missing UIDVALIDITY sentinel; UID-set chunking; search sanitization;
  pre-release decoded folder-name migration gap.
- **T7:** residual sync/backfill race; test-hook fragility; backfilled snippets
  empty until body fetch.
- **T8:** Android Back handling (carried into T16); unified paging cached-only;
  UTC dates; fling rebound; UI-wiring test depth.
- **T9:** empty-body caching guard ineffective; async `load_images` can install
  into another conversation; "Load images" drops placeholders; attachment
  placeholder (resolved by T15); dual html5ever.
- **T10:** rethread pass only on sync and scans all headers.
- **T11:** clearing the query does not bump the generation; out-of-order tests
  exercise `SearchState` directly; no body-delete trigger test; HTML parse cost
  on index refresh.
- **T12:** no dedicated `AccountEdit` threading test.
- **T13:** mid-send failure is not retried (documented no-duplicate tradeoff);
  failed outbox rows have no UI; Sent copy skipped when no cached Sent folder.
- **T14:** `set_read` partial-failure rollback; plain EXPUNGE residual risk;
  folder unread badges stale until sync; move fallback duplication;
  mark-read-on-open still local-only.
- **T15:** pre-v3 cached bodies lack metadata; parse failure is
  indistinguishable from "no attachments"; raw body fetch uncapped; no cache
  eviction on delete.
- **T16:** JNI local refs not framed; Back at the root is a no-op; `scheduleSync`
  re-issues `UPDATE` every launch; Keystore key not deleted; Android/Linux
  transport-factory duplication; `platform.rs` comment inaccuracy.
- **T17:** closing settings without save leaves the live theme and persisted chip
  out of sync; settings uses `.spacing(0)` instead of a kit token; thin
  settings/notification test coverage; `notify-rust` scoped to Linux rather than
  "not Android"; `Cargo.lock` staged as part of the commit.

---

## 6. Advanced settings, portable config, and backups (2026-11-06)

### What changed

- Accounts and settings now live in a portable `pige.json` document that is the
  source of truth. On Linux it is `$XDG_CONFIG_HOME/bitshift/pige/pige.json`
  (`~/.config/bitshift/pige/pige.json`), written atomically with mode `0600`; on
  Android it is the app-private `config` directory. The SQLite database is
  demoted to a cache; its `accounts` table is a derived mirror rebuilt on every
  change so `ON DELETE CASCADE` keeps cleaning up.
- Backups are `pige-backup-YYYY-MM-DD-HHMMSS.json` in Downloads — Linux
  `dirs::download_dir()`, Android the shared MediaStore Downloads collection —
  and exclude passwords. The SQLite `meta` settings row is migrated once and
  cleared.
- A new **Advanced** screen (Backup config / Restore config / Factory Reset)
  built from the shared component kit, plus a shared **toast** that replaces the
  old full-width snackbar/error bar.

### Measured

| Check | Result |
| :--- | :--- |
| `cd rust && cargo test --workspace` | **19 suites green, 0 failed** (pige_core lib 68, pige lib 78, integration suites) |
| `cd rust && cargo clippy --workspace --all-targets` | **clean, no warnings** |
| Android debug build (`JAVA_HOME=/opt/jbr-21 bash scripts/build.sh debug`) | **BUILD SUCCESSFUL**; `compileDebugKotlin` and the arm64 Rust `.so` both built |

### Not run

- **Interactive desktop smoke** (Backup / Restore / Factory Reset through the
  GUI): this pass was executed non-interactively, so no window was driven. The
  pure and headless tests cover the rows, copy, dialog gating, toast generation
  guard, and toast layout.
- **Android on-device verification** (install, write a backup to Downloads,
  restore it, the "All files access" prompt, factory reset): no device
  interaction in this pass. The Kotlin bridge compiles; the Rust logic is
  identical across platforms and covered by the Linux/fake tests.

### Follow-ups (deferred)

- A guided "re-enter passwords" step after a restore is not built; the restore
  toast points at the existing account editor, and a restored account with no
  stored password fails sync with the usual message.
- Android MediaStore behaviour (scoped visibility, `IS_PENDING` handling) still
  wants one on-device pass.
