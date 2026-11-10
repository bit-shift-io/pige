# Provider performance evidence (Task H2)

**Date:** 2026-11-10
**Branch:** `feat/mail-providers`
**Base:** `eedb01c` (H1 complete)
**Fixtures:** synthetic `example.com` addresses only. No live account, network
provider, credential, or real mail was used. The attached device holds the
user's real mail; the measurements below use only a temporary SQLite file under
`/data/local/tmp` and never read or modify the Pige app database.

This document records what was measured, how, and what remains a gap. The 50 ms
p95 recipient-lookup target is an **Android reference-device** target. Desktop
numbers are reported for comparison only and are not the target. The raw
sanitized device transcript (build command, `adb` push/run commands, and exact
captured output) is committed alongside this file as
[`provider-performance-device.log`](provider-performance-device.log).

## 1. Reference environment

| Item | Value |
| :--- | :--- |
| Device | `6bfcc911`, iPlay60 mini Turbo, `iPlay60_mini_Turbo` |
| Android | 14 (API 34), build type `user`, ABI `arm64-v8a` |
| Device state | battery 100%, USB powered, status full, 24.0 °C; airplane mode off; Wi-Fi `spud_mlo` connected/validated, 5 GHz, −42 dBm, 433 Mbps |
| Desktop host | Fedora Linux, x86_64, Rust edition 2024, release profile |
| Android test runner | native aarch64 test binary, built with NDK 28.2.13676358, pushed to `/data/local/tmp` |
| Android app build | `JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release` |

The Android measurement is a **native fixture runner**: the integration-test
binary containing the real `pige_core` store, recipient SQL, and history
learning code is cross-compiled for `aarch64-linux-android`, pushed to the
device, and executed there. It is not a desktop number and not an estimate.
It is not an `androidTest`/instrumentation APK, and it does not measure the iced
UI frame time.

## 2. Deterministic bounds (gate for the measurements)

`rust/pige_core/tests/recipient_perf.rs`:

| Test | Bound asserted |
| :--- | :--- |
| `lookup_uses_bounded_projection` | Exactly 100,000 unique mixed addresses; public cap 20; zero limit empty; every source kind reachable; broad prefix <5,000 SQLite VM steps; selective prefixes <20,000 VM steps, including a **rare two-character name prefix** (`zq`, three matching rows) that must be driven by `recipient_tokens` rather than scanning the rank order; query plan uses `recipient_tokens`/`recipient_index_account_rank`, never the evidence ledger; no provider opened. |
| `history_memory_scales_with_page_not_mailbox` | 1,205 lazy messages spread over 1,204,001 UID positions; max page 100; every page fetched; zero body/attachment calls; visible mail cache stays empty; 1,206 learned addresses; peak heap delta <64 MiB. |
| `cancellation_releases_background_slot` | `HISTORY_MAX_CONCURRENT_JOBS == 2`; a third account issues no provider work while both slots are held; cancelling one in-flight job returns `Cancelled` and immediately admits the third account; the second job still completes. |
| `index_restart_eventually_reaches_oldest_header` | 1,205-message fixture; first two pages commit `e1:200`; a fresh `Store` reopen resumes from that cursor, reaches the oldest header, learns all 1,206 addresses, and never fetches a body/attachment. |

These four tests pass on the desktop **and on the reference device**
(`4 passed, 0 failed, 3 ignored` on both).

### Optimization justified by a failing functional test

The first selective-prefix probe of the 100k fixture failed the new bound:
`person099999` consumed **2,000,081 VM steps** because the rank-index scan paid
a token `EXISTS` probe on every preceding row before finding a selective match
(the first 100k benchmark run measured p50 ≈335 ms on desktop). The fix:

- probe at most a bounded token prefix (2 rows, then up to 4,097 rows for
  longer tokens or 257 rows for one/two-character tokens) to find a selective
  token or prove a token has no match;
- drive that account's prefix branch from `recipient_tokens_token` with a
  forced `CROSS JOIN` join order, sort only the small matched set, and preserve
  the exact same tier/last_seen/frequency/address rank and cap;
- keep the historical rank-index scan for broad prefixes, including broad
  one- and two-character prefixes.

A rare one- or two-character prefix is the same hazard: the three-row `zq`
name token originally fell into the short-prefix rank scan and consumed
**2,000,138 VM steps**. The deterministic test now pins it; the bounded short
probe drives it from the token index in **306 VM steps** (desktop and device).
The same selective probes consume **220 / 208 / 246 VM steps** (`person099999`,
`person099999@example.com`, `Saved Person 000120`). No result is truncated:
every learned address stays in the projection and the full ranking contract is
unchanged; all existing `recipient_lookup` tests remain green.

## 3. Recipient lookup, 100,000 mixed addresses

Fixture: 40k saved contacts, 20k Other contacts, 20k incoming-only, 10k
outgoing, 10k deliberately selected. 250 representative queries (email local
parts, full addresses, saved/Other/learned name prefixes, one broad domain).
Warm-up: 10 queries × 3 runs. The background-crawl pass runs the same 250
queries while a 12,050-header history crawl writes evidence on a second SQLite
connection.

### Device (reference target)

```
recipient lookup 100k (idle): queries=250 min=563.073µs p50=599.844µs mean=787.667µs p95=1.730677ms max=2.6275ms (p95 target 50ms: PASS)
recipient lookup 100k (background crawl): queries=250 min=559.01µs p50=707.448µs mean=937.349µs p95=1.98224ms max=4.629218ms (p95 target 50ms: PASS)
data: 100000 unique mixed addresses; seed=1.225643646s; projection rebuild=6.111932758s (16361 addresses/s); sqlite=396727568 bytes; rss before=25056 KiB after=27880 KiB peak=67428 KiB; background_crawl_active_during_queries=true; build=release android, target=aarch64/unix, debug_assertions=false
```

Exact command and captured output:
[`provider-performance-device.log`](provider-performance-device.log). The
ignored benchmark now **asserts** `p95 <= 50 ms` on Android and asserts that the
background crawl was still running during the second pass, so a regression
exits non-zero rather than changing a printed `PASS`/`MISS` note.

**Device p95: 1.73 ms idle, 1.98 ms during background crawl — inside the 50 ms
target.** Peak RSS was 67,428 KiB (~65.9 MiB) including seeding and the 396.7 MB
temporary database.

### Desktop (comparison only, not the target)

```
recipient lookup 100k (idle): queries=250 min=292.175µs p50=309.236µs mean=431.434µs p95=1.073062ms max=1.438914ms (target 50ms is the Android reference device; this run is linux and is not the target)
recipient lookup 100k (background crawl): queries=250 min=285.482µs p50=309.387µs mean=466.388µs p95=1.063133ms max=1.663393ms (target 50ms is the Android reference device; this run is linux and is not the target)
data: 100000 unique mixed addresses; seed=630.135567ms; projection rebuild=3.582937965s (27910 addresses/s); sqlite=401519888 bytes; rss before=58868 KiB after=59016 KiB peak=61040 KiB; background_crawl_active_during_queries=true; build=release linux, target=x86_64/unix, debug_assertions=false
```

An earlier desktop debug run of the same fixture (not quoted as a target)
measured p50 ≈0.82 ms and p95 ≈4.35 ms, confirming the fix also holds without
optimisation.

## 4. Header-history indexing and restart progress

12,050 headers with 100 sparse UID positions between each: the newest UID is
1,204,901, yet only 12,050 records are ever materialized. A warm-up slice learns
500; the process-style restart reopens SQLite and resumes from the durable
checkpoint before timing the remaining 11,550.

### Device

```
history indexing: 12,050 sparse headers (max UID > 1,000,000); warm-up=500 messages; resumed=39.494663631s for 11550 messages (292 messages/s); sqlite=34720512 bytes; peak rss=15728 KiB; build=release android, target=aarch64/unix, debug_assertions=false
quota backoff: configured 2s, durable retry in 2000 ms, checkpoint=e1:100
```

### Desktop

```
history indexing: 12,050 sparse headers (max UID > 1,000,000); warm-up=500 messages; resumed=23.406577195s for 11550 messages (493 messages/s); sqlite=34695912 bytes; peak rss=14304 KiB; build=release linux, target=x86_64/unix, debug_assertions=false
quota backoff: configured 2s, durable retry in 2000 ms, checkpoint=e1:100
```

Restart progress survives: the resumable checkpoint is `e1:500` after the
warm-up, and the resumed run completes to UID 1. The deterministic restart test
separately pins `e1:200` → reopen → oldest header at UID 1.

Quota backoff is durable: a rate-limited second page leaves the first page's
checkpoint (`e1:100`) untouched and records a retry ~2 s in the future; a
subsequent slice issues no provider request until that delay elapses. The same
behaviour is covered deterministically by the existing
`transient_429_preserves_checkpoint_and_retry_after` test.

## 5. Cold cached inbox and body-on-open

1,000 cached messages and bodies, 300-conversation first page. “Cold” means the
first snapshot after API initialisation in a fresh test process; the OS page
cache is already warm, so this is not a cold-boot measurement.

| Measurement | Device | Desktop release |
| :--- | :--- | :--- |
| First cached snapshot | 27.31 ms | 5.01 ms |
| Warm snapshot p50 / p95 / max | 6.32 / 10.41 / 12.86 ms | 4.66 / 4.79 / 4.90 ms |
| Cached body on open p50 / p95 / max | 6.59 / 6.71 / 6.72 ms | 4.86 / 4.97 / 5.00 ms |

A cached body-open makes **zero provider opens**; the test asserts the fake
provider recorder stayed at 0.

## 6. Release APK size

`JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release` → `BUILD SUCCESSFUL`.

| Item | Bytes |
| :--- | ---: |
| `app-release.apk` | 45,003,832 (42.92 MiB) |
| `lib/arm64-v8a/libpige.so` | 36,858,496 |
| `classes.dex` | 7,860,988 |

This is larger than the v1 28,753,648-byte APK because the release build now
links the native Google and Proton provider code. It is a measured size, not a
size target; no claim is made that it is minimal.

## 7. Commands

```text
cd rust
cargo test -p pige_core --test recipient_perf
cargo test --release -p pige_core --test recipient_perf -- --ignored --nocapture --test-threads=1

# Android native fixture runner (NDK 28.2 environment as in scripts/build.sh)
cargo test --release --target aarch64-linux-android -p pige_core --test recipient_perf --no-run
adb push target/aarch64-linux-android/release/deps/recipient_perf-<hash> /data/local/tmp/recipient_perf-h2
adb shell chmod 755 /data/local/tmp/recipient_perf-h2
adb shell 'cd /data/local/tmp && TMPDIR=/data/local/tmp ./recipient_perf-h2 --nocapture --test-threads=1'
adb shell 'cd /data/local/tmp && TMPDIR=/data/local/tmp ./recipient_perf-h2 --ignored --nocapture --test-threads=1'

# Release APK
JAVA_HOME=/opt/jbr-21 bash scripts/build.sh release
```

The exact build/push/run transcript and captured output for the reference device
is committed as [`provider-performance-device.log`](provider-performance-device.log).

## 8. Gaps and honest limits

- **Not measured:** UI frame time and on-device iced compose keystroke rendering.
  The compose measurement is the public R2 suggestion query under background
  indexing, not the widget/input pipeline.
- **Not measured:** a truly cold OS page cache or cold process start of the
  shipping APK with a large real mailbox.
- **Not measured:** live-provider indexing rates. No account or network provider
  was used; history pages come from a deterministic lazy fixture.
- The temporary 100k fixture occupies ~397 MB because every address stores a
  full-email token, an email local-part token, a domain token, and
  display-name word tokens (plus alphanumeric subparts of each). Real user
  caches will vary with address/name shape; this is synthetic worst-case
  density, not a storage requirement.
- **Tracked limitation:** a one- or two-character prefix matching 257–4,096
  rows still takes the rank-index scan. The short probe stops at 257 so a broad
  short prefix stays cheap; a rare prefix (≤256 rows, including the deterministic
  three-row `zq` case) is token-driven at 306 VM steps. The rank scan measured
  2,000,138 VM steps for a three-match short prefix, so a pathological
  late-ranked prefix in that residual band remains the slow path. Raising the
  short probe to 4,097 rows cost ~29,483 VM steps for the broad `p` prefix and
  was rejected.
- Device conditions are recorded, not controlled: the device was on USB, 100%
  battery, Wi-Fi, 24 °C. Results are from one run; no thermal or low-battery
  stress test was performed.
- The native runner links the same `pige_core` code as the app but does not
  exercise Android `WorkManager`, JNI, or the iced event loop.
