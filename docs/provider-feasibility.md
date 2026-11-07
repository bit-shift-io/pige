# Provider dependency feasibility (Task S1)

Date: 2026-11-07
Branch: `feat/mail-providers`
Task: S1 — Toolchain, dependency and licensing evidence

This document freezes the toolchain/dependency/licensing evidence that later
provider tasks build on. It records what was measured on this branch before any
product refactor. It does **not** decide live authentication flows (S2/S3) and
does **not** accept product dependencies: no candidate crate was added to
`rust/Cargo.toml`/`Cargo.lock`.

## 1. Scope and method

S1's job is to prove, on the real Android toolchain, which candidate dependency
shapes are buildable and license-compatible, and to install a reusable gate that
catches the three known dependency hazards:

1. **Go crypto backend.** `proton-crypto` defaults to `gopgp`, which builds the
   Go `gopenpgp-sys` backend. Pige requires the pure-Rust backend
   (`rustpgp`) on Android (no Bridge/Go service constraint).
2. **AGPL-3.0-only code.** Official Proton mail code (`ProtonMail/clients`) is
   AGPL-3.0-only and is incompatible with Pige's MIT direction.
3. **Native SQLite link collision.** `rusqlite` already owns
   `links = "sqlite3"` (via bundled `libsqlite3-sys`). A second crate claiming
   that slot breaks the native build.

Everything was measured with disposable probes under
`/tmp/opencode/pige-provider-probes/`; nothing there is committed.

Commands were run from `rust/` for Cargo, and from the repository root for
`python3`/`scripts/build.sh`, unless noted.

## 2. Pre-change baseline

| Command | Result |
| --- | --- |
| `cargo test --workspace` | **PASS** — exit 0; 19 suites, 354 passed, 0 failed, 22 ignored |
| `cargo clippy --workspace --all-targets` | **PASS** — exit 0 |
| `cargo build --locked -p pige --bin pige` | **PASS** — exit 0 |
| `bash scripts/build.sh release` (root) | **PASS** — Gradle `BUILD SUCCESSFUL`; `build/app/outputs/apk/release/app-release.apk` (29,212,840 bytes) |

The `cargo test` counts match the controller's independent pre-change measurement
(19 suites / 354 passed / 0 failed / 22 ignored), so the baseline is stable and
reproducible. No unrelated code was changed; S1 changes no workspace Rust code.

Toolchain observed:

- `cargo 1.98.1 (797e8a9bc 2026-08-05)`, `rustc 1.98.1 (48a229cea 2026-09-01)`
- Python `3.14.8`
- Android NDK `28.2.13676358`; `aarch64-linux-android` target installed
- JDK: `/opt/jbr-21` (auto-detected by `scripts/build.sh`)
- Gradle 9.8.1 via the committed wrapper

## 3. The boundary gate

### Deliverables

- `scripts/check-provider-boundaries.py`
- `scripts/tests/test_provider_boundaries.py`
- this report

### CLI

```bash
# generate inputs from the committed workspace
cargo metadata --locked --format-version 1     > /tmp/cargo-metadata.json  # (from rust/)
cargo tree --locked -e features                > /tmp/cargo-tree.txt      # (from rust/)

python3 scripts/check-provider-boundaries.py \
    --metadata /tmp/cargo-metadata.json --tree /tmp/cargo-tree.txt
```

Exit codes: `0` = approved, `1` = boundary violation, `2` = malformed input.

### Rules and matching strategy

The gate keys on Cargo package identity (`name`/`version`/`source`, i.e. the
metadata `id`) and on structured fields, never on bare name substrings. Look-alike
names such as `gopenpgp-sys-stub` or `sqlite3-sys-fake` are ignored.

| Rule | Escalation condition |
| --- | --- |
| Go crypto backend | a resolved package named exactly `gopenpgp-sys`, **or** `proton-crypto` with any of `default`/`gopgp`/`multi_be` enabled, **or** the feature-resolved tree shows the same |
| Licenses | any resolved package whose SPDX expression contains an `AGPL*` token and is not explicitly allow-listed (allow-list is empty today) |
| SQLite link | more than one resolved package with `links == "sqlite3"` |

It also emits a **non-fatal warning** for any registry package with no SPDX
`license` field (see the `proton-crypto` caveat in §4.3), so an AGPL package that
hides behind a `license-file` is surfaced for manual provenance review.

### Test evidence (TDD)

RED — tests written first, before the script existed:

```
$ python3 -m unittest discover -s scripts/tests -p 'test_provider_boundaries.py'
FFFF
... AssertionError: can't open file '.../scripts/check-provider-boundaries.py'
Ran 4 tests ... FAILED (failures=4)
```

GREEN — after implementation:

```
$ python3 -m unittest discover -s scripts/tests -p 'test_provider_boundaries.py' -v
test_accepts_approved_rust_backend ... ok
test_rejects_go_crypto_backend ... ok
test_rejects_multiple_sqlite_links ... ok
test_rejects_unapproved_agpl_package ... ok
Ran 4 tests in 0.29s
OK
```

Applied to the committed workspace, the gate passes:

```
$ python3 scripts/check-provider-boundaries.py --metadata ... --tree ...
provider boundary check: OK (618 resolved packages checked)
```

A negative control that injects one `AGPL-3.0-only` package into the real metadata
fails as expected (`exit 1`, naming the exact package id).

## 4. Candidate dependency decisions

### 4.1 Google clients (mail + contacts)

Both are generated by `Byron/google-apis-rs`, MIT, and are **approved for
probe/product use pending S2 auth decisions**.

| Crate | Version | License | Source commit (`google-apis-rs`) | `.crate` sha256 |
| --- | --- | --- | --- | --- |
| `google-gmail1` | `7.0.0+20251215` | MIT | `961d1c290b3df52be63a5369cfd3c766af6064af` | `7c3a243f…301ab2` |
| `google-people1` | `7.0.0+20251221` | MIT | `961d1c290b3df52be63a5369cfd3c766af6064af` | `37426832…c96bf` |

Notable transitive behaviour: default features enable `yup-oauth2` + `ring`
(`hyper-rustls`/`ring` TLS). Both crates compiled and linked for
`aarch64-linux-android` with default features. If S2 supplies tokens through
Pige's own boundary, `default-features = false` can drop `yup-oauth2`; that
decision is deferred to S2. Contacts may alternatively use a narrow People REST
adapter; no party was selected here.

### 4.2 Proton crypto (pure Rust)

`proton-crypto` with the Go backend disabled is **approved for probe/product use
pending S3 interoperability decisions**.

| Crate | Version | Backend features | License | Source commit (`proton-crypto-rs`) | `.crate` sha256 |
| --- | --- | --- | --- | --- | --- |
| `proton-crypto` | `0.14.1` | `default-features = false, features = ["rustpgp"]` | MIT (`license-file`) | `853e98fc18c202ed41d3c1abb9dcc56b68494f5e` | `8f9f04ca…c81c5` |
| `proton-rpgp` | `0.9.0` | transitive of `rustpgp` | MIT (`license-file`) | `853e98fc…` (same repo) | `5cc147eb…bc528` |
| `proton-srp` | `0.8.4` | transitive | MIT (`license-file`) | `853e98fc…` (same repo) | `5360dae8…b7d1` |

Verified behaviour:

- Approved probe metadata shows `proton-crypto` enabled features `["rustpgp"]`,
  `gopenpgp-sys` **absent**, `proton-rpgp` present.
- A hazard probe using default features resolves `gopenpgp-sys 0.3.9` and
  `proton-crypto` features `["default","gopgp"]`; the gate rejects it with five
  distinct diagnostics (exit 1). This is the exact failure the gate exists to
  prevent.

### 4.3 Licenses and native links

Full current-workspace license inventory (618 packages) contains **no AGPL**.
Two license expressions carry a GPL branch but are acceptable OR-choices:
`self_cell` (`Apache-2.0 OR GPL-2.0-only`) and `r-efi`
(`MIT OR Apache-2.0 OR GPL-2.0-only`). These are permissive-branch selections,
not copyleft obligations, and are pre-existing.

Native `links` owners in the current workspace (all distinct, exactly one
`sqlite3`):

| Package | `links` | License |
| --- | --- | --- |
| `libsqlite3-sys 0.39.1` (bundled, via `rusqlite 0.41.1`) | `sqlite3` | MIT |
| `libdbus-sys 0.2.7` | `dbus` | Apache-2.0/MIT |
| `objc-sys 0.3.5` | `objc_0_3` | MIT |
| `rayon-core 1.13.0` | `rayon-core` | MIT OR Apache-2.0 |
| `ring 0.17.14` | `ring_core_0_17_14_` | Apache-2.0 AND ISC |
| `sqlite-wasm-rs 0.6.2` | `wsqlite3` | MIT |
| `wasm-bindgen-shared 0.2.130` | `wasm_bindgen` | MIT OR Apache-2.0 |

**License caveat (important).** `proton-crypto`, `proton-rpgp`, and `proton-srp`
publish only a `license-file`; their `cargo metadata` license field is empty
(crates.io normalizes it to "non-standard"). The bundled `LICENSE` files are MIT
("(The MIT License) Copyright (c) 2025 Proton AG"). The gate therefore warns
rather than silently trusting an absent SPDX field; a manual provenance check is
required for any new package exhibiting this pattern.

### 4.4 `proton-core` (`filippofinke/protonmail-rs`) — extraction required

Unmodified `proton-core 0.1.2` is MIT (source commit
`67044505aa2931f387b262f37e3c8ad64a6a126c`, `.crate` sha256 `2e25ec68…fc58c`)
and is the closest high-level Proton candidate. It **cannot be consumed
unchanged**: Cargo refuses to resolve it alongside Pige's SQLite stack because it
pulls `rusqlite 0.32` (bundled) and thus a second `links = "sqlite3"` provider:

```
error: failed to select a version for `libsqlite3-sys` ...
package `libsqlite3-sys` links to the native library `sqlite3`, but it conflicts
with a previous package which links to `sqlite3` as well:
package `rusqlite v0.32.0` ... required by package `proton-core v0.1.2`
```

This is a resolution-time failure, exactly the hazard in Review Focus #1.

**Extraction patch inventory** (from the published `proton-core 0.1.2` source):

| Concern | Location | Planned action in an MIT-preserving fork |
| --- | --- | --- |
| SQLite cache + FTS | `src/cache.rs`, `src/lib.rs` (`pub mod cache`) | remove; Pige owns persistence |
| OS keychain | `src/session/secret_store.rs` | replace with Pige's platform credential boundary |
| On-disk session/`directories` | `src/session/mod.rs`, `src/cache.rs` | replace with Pige's secret envelope |
| stderr tracing init | `src/lib.rs::init_tracing` | drop/adapt (`tracing-subscriber` dep) |
| Heavy HTTP/auth deps | `reqwest 0.13`, `chrono` (`mail/export.rs`, `mail/read.rs`) | keep or re-target deliberately |
| `proton-crypto` pin | `proton-crypto = "0.12"` (`default-features=false, rustpgp`) | bump to approved `0.14.1` |

The upstream crate already disables the Go backend
(`proton-crypto = { default-features = false, features = ["rustpgp"] }`), so the
extraction is about **cache/keyring/persistence ownership and the SQLite link**,
not about crypto backend selection. Building an actual extracted fork was **not**
performed in S1 and is deferred; the inventory above is the patch surface to
freeze in S3.

## 5. Android build evidence

All Rust probe builds used the real NDK flags from `scripts/build.sh`:

```bash
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/28.2.13676358"
TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/aarch64-linux-android34-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"
cargo build --release --target aarch64-linux-android
```

| Build | Result |
| --- | --- |
| Approved probe, host (`cargo build`) | **PASS** — `provider-probe` + `proton-rpgp`/`pgp`/`rsa`/Google clients compile |
| Approved probe, `aarch64-linux-android` release | **PASS** — `libprovider_probe.so` (1,047,912 bytes) |
| Boundary gate on approved probe metadata/tree | **PASS** — 285 resolved packages, one `sqlite3` owner, `rustpgp` only |
| Hazard probe (default Go features) gate | **PASS (rejected as intended)** — exit 1 |
| App release APK (`bash scripts/build.sh release`) | **PASS** — `app-release.apk` 29,212,840 bytes |

The probe shared object is a genuine Android artifact:

```
ELF 64-bit LSB shared object, ARM aarch64, ... dynamically linked, for Android 34,
built by NDK r28c (13676358)
LOAD ... align 0x4000   (16 KiB max-page-size flag applied)
NEEDED libdl.so, libc.so
```

`bash scripts/build.sh release` additionally assembled the **full Pige release
APK** for `arm64-v8a` with the existing `libpige.so`, i.e. the Android packaging
path itself is verified end to end.

## 6. PASS / BLOCKED matrix

| Item | Status | Evidence |
| --- | --- | --- |
| Baseline tests / clippy / desktop bin | PASS | §2 |
| Android app release APK | PASS | §2, §5 |
| Boundary gate unit tests (RED→GREEN) | PASS | §3 |
| Gate on committed workspace | PASS | §3 |
| `google-gmail1` + `google-people1` arm64 link | PASS | §5 |
| `proton-crypto` `rustpgp` arm64 link | PASS | §5 |
| Go backend rejected | PASS | §4.2 |
| `proton-core` extraction path understood | PASS (inventory) | §4.4 |
| Extracted `proton-core` arm64 build | NOT RUN (deferred to S3) | §4.4 |
| Live Google auth / mail | NOT RUN — needs disposable account + S2 | later task |
| Live Proton login/read/send on device | NOT RUN — needs disposable account/device + S3 | later task |

## 7. Decisions frozen for later tasks

1. Use `proton-crypto` (and its `rustpgp`/`proton-rpgp` backend) only with
   `default-features = false`; the gate enforces this.
2. Treat `google-gmail1`/`google-people1` as approved MIT candidates; final auth
   wiring (`yup-oauth2` vs Pige token boundary) is an S2 decision.
3. Do not add `proton-core` unchanged; fork/extract with the §4.4 inventory and
   re-target the SQLite/keyring/persistence surfaces.
4. Keep one native SQLite owner (`libsqlite3-sys` via `rusqlite 0.41`).
5. Run `scripts/check-provider-boundaries.py` in CI and locally over
   `cargo metadata` + `cargo tree`.

## 8. Issues, concerns, and blockers

- **Full probe APK not assembled.** The Pige release APK was assembled and the
  probe libraries were linked for `arm64-v8a`, but a separate APK bundling the
  probe `.so` was not produced. Per the S1 brief this is called out explicitly;
  the Rust link proof and the app APK assembly together cover the requirement.
- **No runtime/API exercise.** Google and Proton crates were compiled/linked but
  not invoked; no network calls or accounts were used. This is expected for S1.
- **`proton-crypto` license metadata.** No SPDX field upstream; the gate warns and
  provenance (MIT) was verified from the bundled `LICENSE`. A new candidate with
  the same pattern needs manual review.
- **`proton-core` extraction is real work.** It removes cache/keyring/session
  subsystems and re-targets SQLite; the fork must track upstream MIT changes.
  S3 owns the accept/reject decision.
- **`yup-oauth2`/`ring` weight.** Default Google features add TLS/auth machinery
  that may be unnecessary once Pige owns tokens; revisit in S2.
- **Not blocked:** every S1 deliverable is complete. The rows marked NOT RUN
  require disposable accounts/device or a later task and do not block S1.
