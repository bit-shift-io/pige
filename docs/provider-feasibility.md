# Provider dependency feasibility (Task S1)

Date: 2026-11-07
Branch: `feat/mail-providers`
Task: S1 — Toolchain, dependency and licensing evidence
Task: S2 — Supported Google authorization and API probes (appended in §9)
Task: S3 — Native Proton interoperability and security gate (appended in §10)

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
| Licenses | any resolved package whose SPDX expression contains an `AGPL*` token, **or** any resolved *registry* package with no SPDX `license` field at all |
| SQLite link | more than one resolved package with `links == "sqlite3"` |

Both licensing hazards are **fatal (exit 1)** unless the package name is on
`APPROVED_LICENSE_PACKAGES`. That allow-list is deliberately tiny and matched by
package *name*: it currently holds only the vetted pure-Rust Proton crypto
family (`proton-crypto`, `proton-rpgp`, `proton-srp`), which publish no SPDX
field and ship an MIT `LICENSE` file from one repo commit (see §4.3). For those
packages the gate still prints a non-fatal **provenance warning** on stderr so
the manual review stays visible in CI logs. Every other crate lacking SPDX
metadata now fails loudly instead of passing on a warning, closing the hole
where a future AGPL crate could hide behind a `license-file`. Workspace/path
packages are exempt from the SPDX rule (they carry no registry metadata).

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
test_accepts_allow_listed_license_file_package ... ok
test_accepts_approved_rust_backend ... ok
test_rejects_go_crypto_backend ... ok
test_rejects_missing_spdx_license_for_new_registry_package ... ok
test_rejects_multiple_sqlite_links ... ok
test_rejects_non_object_metadata_json ... ok
test_rejects_unapproved_agpl_package ... ok
Ran 7 tests in 0.53s
OK
```

The four required named tests (`test_rejects_go_crypto_backend`,
`test_rejects_unapproved_agpl_package`, `test_rejects_multiple_sqlite_links`,
`test_accepts_approved_rust_backend`) assert the documented exit code `1` for the
reject cases and the exact success banner for the accept case. The three added
tests cover the missing-SPDX failure, the deliberate `proton-crypto` allow-list,
and the non-object-JSON exit `2` path. The accepted tree also carries a
`my-proton-crypto feature "gopgp"` decoy line to guard the `(?<![\w-])`
feature-line lookbehind.

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
  `proton-crypto` features `["default","gopgp"]`; the gate rejects it with six
  distinct diagnostics (exit 1), including `gopenpgp-sys`'s own missing SPDX
  field. This is the exact failure the gate exists to prevent.

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
("(The MIT License) Copyright (c) 2025 Proton AG"), all from repo commit
`853e98fc…`. These three names are the **only** entries in
`APPROVED_LICENSE_PACKAGES`, a deliberate manual-provenance decision. Any *other*
registry package whose SPDX field is missing now fails the gate (exit 1); the
approved three still emit a provenance warning on stderr. So a future crate
exhibiting the same `license-file`-only pattern is rejected until someone
deliberately adds it after reviewing the bundled license.

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
| Probe test APK (probe `.so` packaged) | **PASS** — `app-release-unsigned.apk` (604,592 bytes) containing `lib/arm64-v8a/libprovider_probe.so` |
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

#### Probe test APK (actual packaging path)

After the linked `libprovider_probe.so` above, the probe library was packaged into
a disposable `arm64-v8a` APK with the same Gradle 9.8.1 wrapper and JDK 21 that
`scripts/build.sh` uses, proving the candidate stack survives the real Android
packaging path (not just `cargo build`/`cargo check`):

```bash
# disposable app under /tmp/opencode/pige-provider-probes/test-app (not committed)
cp probe/target/aarch64-linux-android/release/libprovider_probe.so \
   test-app/app/src/main/jniLibs/arm64-v8a/
JAVA_HOME=/opt/jbr-21 ANDROID_HOME=$HOME/Android/Sdk \
  bash android/gradlew -p /tmp/opencode/pige-provider-probes/test-app \
       --no-daemon assembleRelease
```

Result: `BUILD SUCCESSFUL` (46 actionable tasks), producing
`/tmp/opencode/pige-provider-probes/test-app/app/build/outputs/apk/release/app-release-unsigned.apk`
(604,592 bytes). `unzip -l` shows the probe library inside the APK:

```
$ unzip -l app-release-unsigned.apk | grep -E 'lib/|\.so|AndroidManifest'
   587104  01-01-1981 01:01   lib/arm64-v8a/libprovider_probe.so
     1264  01-01-1981 01:01   AndroidManifest.xml
```

The APK entry is the debug-symbol-stripped form of the same probe artifact: the
unstripped `jniLibs`/`target` file hashes to
`84a4d1ef0149951e67234b1cb75c67f9a975b16ba38886a9898dbfb4b0bac2b3` and the
stripped APK entry (`= stripReleaseDebugSymbols` output) hashes to
`a3ec396549dc21d4e68dee3872d08b6c5f5e5b3f128755b85484389cb8c05f7a`.

`bash scripts/build.sh release` additionally assembled the **full Pige release
APK** for `arm64-v8a` with the existing `libpige.so`, so both the product
packaging path and the candidate-dependency probe packaging path are verified end
to end.

## 6. PASS / BLOCKED matrix

| Item | Status | Evidence |
| --- | --- | --- |
| Baseline tests / clippy / desktop bin | PASS | §2 |
| Android app release APK | PASS | §2, §5 |
| Probe test APK (probe `.so` packaged) | PASS | §5 |
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

- **Probe APK assembled (not just linked).** The candidate probe `.so` was
  packaged into a disposable `arm64-v8a` APK (`app-release-unsigned.apk`,
  604,592 bytes) with the same Gradle/JDK as `scripts/build.sh`; see §5 for the
  command and `unzip -l` evidence. The product release APK was assembled
  separately by `bash scripts/build.sh release`.
- **No runtime/API exercise.** Google and Proton crates were compiled/linked but
  not invoked; no network calls or accounts were used. This is expected for S1.
- **`proton-crypto` license metadata.** No SPDX field upstream; provenance (MIT)
  was verified from the bundled `LICENSE` and the family is now the only entry
  in `APPROVED_LICENSE_PACKAGES`. Unlike before, any *other* registry package
  missing SPDX metadata fails the gate (exit 1), so the same pattern elsewhere
  cannot pass silently.
- **`proton-core` extraction is real work.** It removes cache/keyring/session
  subsystems and re-targets SQLite; the fork must track upstream MIT changes.
  S3 owns the accept/reject decision.
- **`yup-oauth2`/`ring` weight.** Default Google features add TLS/auth machinery
  that may be unnecessary once Pige owns tokens; revisit in S2.
- **Not blocked:** every S1 deliverable is complete. The rows marked NOT RUN
  require disposable accounts/device or a later task and do not block S1.

## 9. Google authorization decision (Task S2)

Date: 2026-11-07
Branch: `feat/mail-providers`
Task: S2 — Supported Google authorization and API probes

S2 freezes a `GoogleAuthDecision`: the OAuth registrations, the per-platform
authorization flows, refresh-persistence ownership, the exact scopes, the
`gmail.modify` capability truth, the non-Play-services answer, and the chosen
mail/People SDK injection points. It writes no product code and adds no
dependency to `rust/Cargo.toml`/`Cargo.lock`. It is a stop/go gate.

**Outcome: the design is frozen and every non-live deliverable is complete; the
live authorization/API cases are BLOCKED on an unregistered OAuth client and a
disposable account; one required user distribution decision is recorded in
§9.9.** Status is reported as DONE_WITH_CONCERNS for this reason.

### 9.1 Decision summary

| Decision | Frozen value |
| --- | --- |
| Desktop client | Google Cloud **Desktop app** OAuth client; public client; authorization code + PKCE (S256) + `state`; system browser; loopback redirect `http://127.0.0.1:<random port>` |
| Android client | Google Cloud **Android** OAuth client; package `bitshift.pige`; signing SHA-1 in §9.2; **no redirect URI** |
| Android authorization **(pending §9.9 decision)** | Google Identity Services `AuthorizationClient` (`com.google.android.gms:play-services-auth`); consent returns through a `PendingIntent`/`IntentSender`; no WebView/OOB/loopback/service account. Live confirmation is parked until the §9.9 distribution decision. |
| Refresh ownership **(Android path pending §9.9 decision)** | Pige secret envelope owns credentials. Desktop: owns the refresh token. Android: on-demand access tokens while the grant is intact (grant state in the envelope); refresh token only via a server (`requestOfflineAccess`) — see §9.6; the Android choice is parked until the §9.9 decision. |
| Mail scope | `https://www.googleapis.com/auth/gmail.modify` (+ `openid email profile` for identity) |
| Contacts scopes | `https://www.googleapis.com/auth/contacts.readonly` + `https://www.googleapis.com/auth/contacts.other.readonly`, granted separately/later |
| Permanent delete | `false` under `gmail.modify`; delete maps to trash (§9.5) |
| Non-Play-services **(pending §9.9 decision)** | No supported native flow without Play services → STOP (§9.9) |
| Mail SDK | `google-gmail1` (MIT, S1 §4.1) behind `MailBackend`; Pige injects tokens |
| Contacts SDK | `google-people1` (MIT, S1 §4.1) behind the contacts capability |

### 9.2 Required Google Cloud registrations (exact)

Everything below is created by the user in the Google Cloud Console; no secret is
committed to the repository.

Project and consent screen:

1. Create/select a Cloud project and enable the **Gmail API** and **People API**.
2. Configure the OAuth consent screen (Branding page): app name **Pige**, logo,
   homepage, privacy-policy URL, terms-of-service URL.
3. Declare scopes: `gmail.modify` (restricted), `contacts.readonly` and
   `contacts.other.readonly` (sensitive), plus `openid`, `email`, `profile`.

OAuth clients:

| Client | Type | Identity | Redirect |
| --- | --- | --- | --- |
| Pige desktop | Desktop app | generated id (a `client_secret` may be issued, but Desktop apps are **public clients**: PKCE is used and **no secret is sent** — see §9.3) | loopback `http://127.0.0.1:<port>` (by client type) |
| Pige Android | Android | package `bitshift.pige`, SHA-1 `7E:4B:01:BC:E7:61:75:E3:89:38:88:5D:91:A6:26:FE:D3:07:21:AD` | none |
| Pige Android (debug) | Android | package `bitshift.pige`, SHA-1 `A0:4B:69:80:86:C4:44:A4:B3:6A:43:6D:98:11:CA:EE:64:40:B7:67` | none |
| Pige server client | Web application | generated id | only if the offline/`serverAuthCode` path is chosen |

The Android fingerprints are the real, verifiable identities of this branch's
signing keys (evidence: `s2-registration-and-device.txt`): the committed shared
test-phone key `android/keystore/pige-release.jks` (alias `qlaunch`) and the
local debug key. Both must be registered because debug and release builds use
different keys when `android/keystore.properties` is absent.

The desktop and Android clients are **separate registrations**. There is no
cross-platform callback URI, and the Android client has none at all — Android
authorization never receives a redirect URI.

### 9.3 Desktop flow (frozen)

System browser → `https://accounts.google.com/o/oauth2/v2/auth` with
`response_type=code`, `code_challenge` (S256), `state`, `access_type=offline` →
loopback listener on `http://127.0.0.1:<random port>` → validate `state` →
exchange at `https://oauth2.googleapis.com/token` with `code_verifier` (no
client secret; Desktop apps are public clients). Cancellation
(`error=access_denied`) and `state` mismatch are surfaced as typed auth errors;
tokens are persisted in Pige's versioned secret envelope only.

### 9.4 Android flow (proposed; live confirmation BLOCKED)

`Identity.getAuthorizationClient(activity).authorize(request)` with an
`AuthorizationRequest` carrying the scopes. If `result.hasResolution()`, launch
`result.pendingIntent` through `StartIntentSenderForResult`; otherwise the grant
already exists and `result.accessToken` is available. The result is read back
with `getAuthorizationResultFromIntent`. The access token is passed to
`google-gmail1`/`google-people1` through Pige's token boundary.

This is the only Google-supported Android authorization API, and it ships in
Google Play services. Review Focus #2 (a desktop/browser success is not Android
success) is honoured: the desktop flow is never reused on Android and no
desktop app is impersonated. The live behaviour that must be *observed* (consent
return, Activity recreation, silent renewal, revocation) is BLOCKED; see §9.7.

### 9.5 `gmail.modify` capability truth

Gmail API reference: `users.messages.delete` ("immediately and permanently
deletes ... cannot be undone") requires `https://mail.google.com/`. The Gmail
API scope documentation states `gmail.modify` "does not allow immediate,
permanent deletion of threads and messages, bypassing the trash." Pige therefore
exposes `permanent_delete = false` and maps its delete command to
`users.messages.trash` (recoverable), never `users.messages.delete`. Full-mail
access is not requested silently.

This is the documented API contract, confirmed from the Gmail API scopes and the
`users.messages.delete` reference on 2026-11-07. The disposable-account runtime
confirmation is BLOCKED with the rest of the live cases.

### 9.6 Refresh persistence ownership

- **Desktop.** Pige owns the refresh token and persists it in the per-account
  versioned secret envelope; refresh is serialized per account with atomic
  rotated-token saves, and secrets never enter the mail cache or config.
- **Android.** Google's authorization guide directs that long-lived refresh
  tokens be held by a backend server, not the device: `requestOfflineAccess`
  returns a `serverAuthCode` that must be exchanged server-side. Without a
  backend, Pige must not store a refresh token on device. The supported
  device-only model is to call `authorize()` on demand: while the user's grant is
  intact, Play services returns a fresh access token with no user interaction;
  Pige persists only account identity and granted-scope state. This satisfies
  "resume after process restart" without retaining a password or a device-side
  refresh token, at the cost of periodic on-demand token calls and re-consent
  after revocation.
- The choice between "no device refresh token + on-demand `authorize()`" and
  "add a minimal auth server" is the distribution decision in §9.9. Confirming
  the actual token/refresh ownership on a real device is **BLOCKED**; the host
  authorization bridge is not specified until it is confirmed.

### 9.7 Checklist: cases

| Case | Status | Why / evidence |
| --- | --- | --- |
| `desktop_pkce_cancel_and_state` | **BLOCKED** | Needs a registered Desktop app client + disposable account. Offline mechanics self-test PASSes (`s2-desktop-pkce-selftest.log`), but that is not Google proof. |
| `android_consent_return_and_recreation` | **BLOCKED** | Needs the Android client (package+SHA-1) registered, a disposable account, and the probe APK. Device is present. A Web `serverClientId` is **only** needed for the offline/`serverAuthCode` path, not for the device-only on-demand `authorize()` flow. |
| `mail_without_contacts_scope` | **BLOCKED** | Needs a live `gmail.modify` token from the disposable account. |
| `resume_after_process_restart` | **BLOCKED** | Needs a live Android grant + probe APK. |
| `contacts_later_grant` | **BLOCKED** | Needs a live grant + `contacts.readonly`/`contacts.other.readonly` consent. |
| `revoked_grant_requires_login` | **BLOCKED** | Needs a live Android grant, then revocation (account permissions page / `revokeAccess`). |
| `api_profile_and_message_read` | **BLOCKED** | Brief line 33: needs a live `gmail.modify` token for `users.getProfile`, `users.messages.list`, `users.messages.get`, `users.history.list`. |
| `api_authorized_test_send` | **BLOCKED** | Brief line 33: needs a live `gmail.modify` token and explicit user authorization for one `users.messages.send` (to the disposable account itself). |
| `api_saved_contacts` | **BLOCKED** | Brief line 33: needs a live `contacts.readonly` token for `people.connections.list` (saved contacts). |
| `api_other_contacts` | **BLOCKED** | Brief line 33: needs a live `contacts.other.readonly` token for `otherContacts.list`/`search` (Other contacts). |
| `gmail_modify_permanent_delete_false` | **BLOCKED** | Brief line 33: needs a live `gmail.modify` token; `users.messages.delete` must be rejected and delete must map to `users.messages.trash` (§9.5), i.e. `permanent_delete = false`. |
| `play_services_absent_or_disabled` | **BLOCKED** | Brief line 34: needs a device/emulator with Play services disabled or absent; expectation is the §9.9 STOP (no supported native auth flow). |

Brief line 34's "cancellation and lost activity" are covered by
`desktop_pkce_cancel_and_state` (unknown/denied cancel) and
`android_consent_return_and_recreation` (Activity recreation / lost Activity)
respectively; these two new groups complete the recordable checklist.

No case is marked PASS without having been run; no output is fabricated.

Non-live evidence that *was* collected is real:

- Android application identity and both signing SHA-1s (keytool).
- Attached device: `iPlay60 mini Turbo`, Android 14 / API 34, `arm64-v8a`,
  Google Play services `26.43.30`, Chrome and Play Store present
  (`s2-registration-and-device.txt`).
- Desktop PKCE mechanics self-test: `state` validation, cancellation, S256
  challenge, and exchange-to-verifier binding all PASS offline
  (`s2-desktop-pkce-selftest.log`).

### 9.8 Exact blocked prerequisites and commands

The non-live deliverables (this section, the registration enumeration, the flow,
the scopes, the checklist, the disposable probe scaffold, and the device/cert
evidence) are complete. Missing for the live runs:

1. A Google Cloud project with a registered **Desktop app** OAuth client
   (`GOOGLE_DESKTOP_CLIENT_ID`).
2. A registered **Android** OAuth client for `bitshift.pige` with the two SHA-1s
   in §9.2.
3. A **Web application** client id (`GOOGLE_WEB_SERVER_CLIENT_ID`) if the
   offline/`serverAuthCode` path is chosen.
4. A **disposable Google account** (placeholder `disposable-account@example.com`;
   the attached device is signed into a personal account that must **not** be
   used for these tests) and explicit authorization for one test send.

#### 9.8.1 Reproducible commands (in-repo; no secrets)

These are the exact commands that turn each BLOCKED case into a live run once the
prerequisites above exist. They are self-contained: no disposable probe
directory is required. Substitute an unregistered value only for the
placeholders in angle brackets (client ids, port, codes, tokens); never commit a
real client id, secret, token, or account.

Desktop PKCE / cancellation / state (`desktop_pkce_cancel_and_state`):

```bash
# 1. Build the PKCE (S256) request and open it in the system browser:
#    https://accounts.google.com/o/oauth2/v2/auth
#      ?client_id=<desktop-client-id>
#      &redirect_uri=http://127.0.0.1:<random-port>
#      &response_type=code
#      &scope=https://www.googleapis.com/auth/gmail.modify
#      &code_challenge=<S256-of-verifier>&code_challenge_method=S256
#      &state=<random-state>&access_type=offline
# 2. A loopback listener on 127.0.0.1:<random-port> captures ?code= (or ?error=).
#    Validate state exactly; on cancel expect error=access_denied.
# 3. Exchange the code at the token endpoint (public client; no secret, PKCE):
curl -s https://oauth2.googleapis.com/token \
  -d client_id=<desktop-client-id> \
  -d code=<authorization-code> \
  -d code_verifier=<verifier> \
  -d grant_type=authorization_code \
  -d redirect_uri=http://127.0.0.1:<random-port>
#    PASS: token JSON. Cancellation -> {"error":"access_denied"} (exit 1).
#    Forged/mismatched state -> rejected before exchange.
```

Android consent return / Activity recreation / resume / revoke
(`android_consent_return_and_recreation`, `resume_after_process_restart`,
`contacts_later_grant`, `revoked_grant_requires_login`):

```bash
# Probe app (its own disposable project) uses
# com.google.android.gms:play-services-auth and calls
# Identity.getAuthorizationClient(activity).authorize(request); it launches
# result.pendingIntent via StartIntentSenderForResult and reads the result back
# with getAuthorizationResultFromIntent. Build with the repo Gradle/JDK:
JAVA_HOME=/opt/jbr-21 ANDROID_HOME=$HOME/Android/Sdk \
  bash android/gradlew -p <probe-app-dir> --no-daemon assembleDebug
adb install -r <probe-app-dir>/app/build/outputs/apk/debug/app-debug.apk
# consent return + Activity recreation (rotate the device on the consent screen):
adb shell am start -n bitshift.pige.probe/.AuthProbeActivity \
  --es scope https://www.googleapis.com/auth/gmail.modify
# resume after process restart (kill, relaunch, no user interaction expected):
adb shell am force-stop bitshift.pige.probe
adb shell am start -n bitshift.pige.probe/.AuthProbeActivity \
  --es scope https://www.googleapis.com/auth/gmail.modify
# contacts_later_grant (add the contacts scopes; a new consent is required):
adb shell am start -n bitshift.pige.probe/.AuthProbeActivity \
  --es scope "https://www.googleapis.com/auth/contacts.readonly https://www.googleapis.com/auth/contacts.other.readonly"
# revoked_grant_requires_login (revoke, then expect hasResolution() again):
adb shell am start -n bitshift.pige.probe/.RevokeActivity
#   or revoke at https://myaccount.google.com/permissions
```

API contract probes (`api_profile_and_message_read`, `api_authorized_test_send`,
`api_saved_contacts`, `api_other_contacts`,
`gmail_modify_permanent_delete_false`), once any flow yields a token:

```bash
TOKEN=<access-token>
# profile / list / detail / history:
curl -s -H "Authorization: Bearer $TOKEN" \
  https://gmail.googleapis.com/gmail/v1/users/me/profile
curl -s -H "Authorization: Bearer $TOKEN" \
  'https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=5'
curl -s -H "Authorization: Bearer $TOKEN" \
  'https://gmail.googleapis.com/gmail/v1/users/me/messages/<id>?format=full'
curl -s -H "Authorization: Bearer $TOKEN" \
  'https://gmail.googleapis.com/gmail/v1/users/me/history?startHistoryId=<id>'
# one authorized test send (to the disposable account itself):
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  https://gmail.googleapis.com/gmail/v1/users/me/messages/send \
  -d '{"raw":"<base64url RFC822 message>"}'
# saved contacts + Other contacts:
curl -s -H "Authorization: Bearer $TOKEN" \
  'https://people.googleapis.com/v1/people/me/connections?personFields=names,emailAddresses'
curl -s -H "Authorization: Bearer $TOKEN" \
  'https://people.googleapis.com/v1/otherContacts?readMask=names,emailAddresses'
# permanent-delete truth: must be rejected under gmail.modify; trash succeeds:
curl -s -o /dev/null -w '%{http_code}\n' -X DELETE -H "Authorization: Bearer $TOKEN" \
  https://gmail.googleapis.com/gmail/v1/users/me/messages/<id>
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
  https://gmail.googleapis.com/gmail/v1/users/me/messages/<id>/trash
```

Play-services availability (`play_services_absent_or_disabled`):

```bash
# On a device/emulator with Play services disabled or absent, run the Android
# probe above; expectation is the §9.9 STOP (no supported native auth flow),
# not a silent fallback.
adb shell pm disable-user --user 0 com.google.android.gms   # test device only
```

The historical probe scaffold and raw output live under
`/tmp/opencode/pige-provider-probes/` (disposable, never committed); the
commands above do not depend on it.

### 9.9 Required user distribution decision (STOP)

The S2 brief: "If supported Google auth requires Play services or an auth
server, stop for the user's distribution decision; do not proceed by
impersonating a desktop app on Android."

Finding: Google's supported Android authorization (GIS `AuthorizationClient`)
requires Google Play services. Google no longer supports custom URI schemes on
Android and has deprecated/disallowed loopback redirects for Android; OOB is
removed and embedded WebViews are disallowed. There is therefore **no supported
Google OAuth mechanism for a native Android app without Play services**; the only
alternative (a Web client with an app-claimed HTTPS/App-Links redirect) is not a
documented supported native-app flow and would require owning and verifying an
HTTPS domain. In addition, Google directs that Android refresh tokens be held by
a backend server, so true offline background refresh implies either an auth
server or the on-demand `authorize()` model of §9.6.

**Decision requested from the user** (engineering recommendation in parentheses):

1. Accept a mandatory Google Play services dependency for Gmail/Contacts on
   Android (recommended: yes — it is the only supported flow); and
2. Choose between (a) device-only on-demand `authorize()` access tokens with no
   on-device refresh token (recommended for v1; no backend), or (b) add a minimal
   Pige auth server to hold refresh tokens; or
3. Reconsider Android distribution for native Gmail if neither is acceptable.

Until this decision is recorded, live Android authorization work stays parked.

### 9.10 Selected SDK injection points

- Mail: `google-gmail1` `7.0.0+20251215` (MIT, approved in S1 §4.1), used behind
  Pige's provider-neutral `MailBackend`; Pige supplies the OAuth2 access token per
  request. Prefer `default-features = false` to drop `yup-oauth2` so a single
  token path is used on both platforms (S1 §4.1, §8); final feature wiring is
  G-task work using S1's build evidence.
- Contacts: `google-people1` `7.0.0+20251221` (MIT) behind the contacts
  capability, independent of the mail scope grant.
- No `google-gmail1`/`google-people1` dependency is added to the committed
  workspace in S2.

### 9.11 S2 PASS / FAIL / BLOCKED matrix

| Item | Status | Evidence |
| --- | --- | --- |
| `GoogleAuthDecision` section frozen | PASS | §9.1–§9.10 |
| Registration enumeration (clients, scopes, consent screen) | PASS | §9.2 |
| Proposed platform flows + Play-services answer | PASS | §9.3–§9.4, §9.9 |
| Case checklist present with PASS/FAIL/BLOCKED | PASS | §9.7 |
| Android app identity + signing SHA-1s | PASS | §9.2, `s2-registration-and-device.txt` |
| Device/Play-services/browser capability | PASS | §9.7, `s2-registration-and-device.txt` |
| Desktop PKCE mechanics self-test (offline) | PASS (supplementary) | `s2-desktop-pkce-selftest.log` |
| `desktop_pkce_cancel_and_state` (live Google) | BLOCKED | §9.7, §9.8 |
| `android_consent_return_and_recreation` | BLOCKED | §9.7, §9.8 |
| `mail_without_contacts_scope` | BLOCKED | §9.7, §9.8 |
| `resume_after_process_restart` | BLOCKED | §9.7, §9.8 |
| `contacts_later_grant` | BLOCKED | §9.7, §9.8 |
| `revoked_grant_requires_login` | BLOCKED | §9.7, §9.8 |
| `api_profile_and_message_read` | BLOCKED | §9.7, §9.8 |
| `api_authorized_test_send` | BLOCKED | §9.7, §9.8 |
| `api_saved_contacts` | BLOCKED | §9.7, §9.8 |
| `api_other_contacts` | BLOCKED | §9.7, §9.8 |
| `gmail_modify_permanent_delete_false` | BLOCKED | §9.7, §9.8 |
| `play_services_absent_or_disabled` | BLOCKED | §9.7, §9.8 |

## 10. Proton dependency decision (Task S3)

Date: 2026-11-08
Branch: `feat/mail-providers`
Task: S3 — Native Proton interoperability and security gate

S3 freezes a `ProtonDependencyDecision`: the accepted source/version/features,
honest application identification and API-access expectations, the key-policy
and format matrix, the protected session fields, the fork/upstream patch
procedure and upkeep budget, and the live/device test evidence. It writes no
product code and adds no dependency to `rust/Cargo.toml`/`Cargo.lock`. Every
build/link/gate outcome below is reproduced with exact commands and captured
output in the committed transcript **`docs/s3-command-transcript.md`** (the
earlier raw logs lived only in disposable `/tmp` storage).

**Outcome: the non-live deliverables are complete and a real extraction build
of the candidate library against the approved Rust crypto backend was
reproduced for `aarch64-linux-android`; every live, device, CAPTCHA and
send case is honestly BLOCKED on a disposable Proton account, a device
interaction and explicit send authorization. One candidate behaviour — silent
cleartext downgrade for keyless external recipients — is rejected as a blocker
for downstream P4.**

### 10.1 Decision summary

| Decision | Frozen value |
| --- | --- |
| Accepted crypto | `proton-crypto 0.14.1` (`default-features = false, features = ["rustpgp"]`), `proton-rpgp 0.9.0`, `proton-srp 0.8.4`; MIT (`license-file`); repo commit `853e98fc18c202ed41d3c1abb9dcc56b68494f5e` (S1 §4.2) |
| High-level library | Extract `proton-core 0.1.2` (`filippofinke/protonmail-rs`, MIT, commit `67044505aa2931f387b262f37e3c8ad64a6a126c`) into a small maintained MIT fork. Do **not** consume unchanged. |
| Crypto backend | Go (`gopgp`/`gopenpgp-sys`) stays disabled; the S1 gate enforces this. |
| Local persistence | Pige owns SQLite/secrets/sync. The fork deletes `cache.rs` and `mail/sync.rs` and drops `rusqlite`/`directories`. |
| Secret/session ownership | Pige owns the versioned secret envelope. The fork drops the in-library `keyring` store and on-disk session persistence; the host injects a token/secret boundary. |
| App identification | Honest external header only. The candidate default `x-pm-appversion: Other` is acceptable; **no masquerading `User-Agent`**. Live API acceptance is UNVERIFIED. |
| Recipient encryption policy | FAIL CLOSED. The candidate's silent `Type 4` cleartext fallback is rejected; P4 must resolve recipient policy before any delivery and block on missing policy. |
| Key Transparency | Not implemented by the candidate or by `proton-crypto 0.14.1`; disclosed limitation, never equated with a valid signature. |
| FIDO2/WebAuthn | Unsupported → typed, actionable error; never bypassed or downgraded. |
| Status | DONE_WITH_CONCERNS — non-live work complete; live/device/send cases BLOCKED (§10.8, §10.9). |

### 10.2 Accepted sources, versions and features (exact)

The accepted dependency set is the S1-approved MIT crypto family, plus an
**extracted** high-level layer:

| Component | Version | Features | License | Provenance |
| --- | --- | --- | --- | --- |
| `proton-crypto` | `0.14.1` | `default-features = false, features = ["rustpgp"]` | MIT (`license-file`) | `proton-crypto-rs` `853e98fc…` |
| `proton-rpgp` | `0.9.0` | transitive of `rustpgp` | MIT (`license-file`) | same repo/commit |
| `proton-srp` | `0.8.4` | transitive | MIT (`license-file`) | same repo/commit |
| `proton-core` (forked) | upstream `0.1.2` + patch series | cache/keyring/session removed; `proton-crypto` bumped to `0.14.1` | MIT | `filippofinke/protonmail-rs` `67044505…` |

Rejected outright (unchanged from the spec/S1):

- Official `ProtonMail/clients` `mail-api` — AGPL-3.0-only (license-direction conflict).
- `proton-api-rs` — AGPL-3.0-only plus a Go SRP/FFI build dependency.
- Any Bridge, Go service, hosted proxy, or Bridge/MCP wrapper.

The upstream `proton-core 0.1.2` already disables the Go backend
(`proton-crypto = { default-features = false, features = ["rustpgp"] }`), so the
extraction is about **cache/keyring/session/SQLite ownership and the crypto
version bump**, not about selecting a crypto backend.

### 10.3 Honest application identification and API-access expectations

Facts established by source inspection of `proton-core 0.1.2`:

- `src/mail/mod.rs`: `DEFAULT_APP_VERSION = "Other"`.
- `src/transport/mod.rs`: `AuthState { app_version, user_agent: Option<String> }`
  and a setter whose doc-comment reads "Optional `User-Agent` (to present as a
  real client)".
- The candidate ships `--client web|ios|android` presets and a configurable
  `--app-version` (README "Client masquerade"). That is a deliberate
  impersonation affordance for third-party compatibility.

S3 decision: Pige sends an honest `x-pm-appversion` that identifies Pige (or the
neutral `Other`) and never sets a masquerading `User-Agent`. The exact header
string is a small open question that must be checked live, because the Proton
API is unofficial/undocumented for third parties and may gate on `appversion`.

**Stop condition (brief):** "Stop if only official-client impersonation works."
Whether Proton accepts an honest external `appversion`/`User-Agent` **cannot be
determined without a live account**; it is recorded BLOCKED (§10.9), and no
maintainer clarification from Proton AG has been obtained. Pige must not ship a
masquerading identity as a workaround.

### 10.4 Extraction: fork / upstream patch procedure and upkeep budget

**Fork identity.** A small MIT-preserving patch series over the pinned upstream
revision of `proton-core` (`0.1.2`, commit
`67044505aa2931f387b262f37e3c8ad64a6a126c`). Plan 04 locates it at
`rust/vendor/proton-mail-api/` with license/provenance and update instructions;
it is added to the workspace only when P1 needs it. No CLI/cache/keyring crate
is vendored.

**Exact patch inventory** (verified against the published source; the file set
is reproduced in `s3-extraction-diff.txt`):

| File | Upstream role | Patch action | Reason |
| --- | --- | --- | --- |
| `src/cache.rs` (264 lines) | SQLite cache + FTS | **delete** | owns `rusqlite 0.32` (bundled) → second `links = "sqlite3"`; Pige owns persistence |
| `src/mail/sync.rs` (190 lines) | event stream → SQLite | **delete** (remove `pub mod sync;`) | Pige owns sync/cursors (P2) |
| `src/lib.rs` | module root + tracing init | drop `pub mod cache;`, drop `init_tracing`, drop `SyncReport` re-export | removes `tracing-subscriber` |
| `src/session/secret_store.rs` | OS keychain store | drop `KeyringStore` (keep the `SecretStore` trait + in-memory impl) | removes `keyring`; Pige injects its own store |
| `src/session/mod.rs` | on-disk session metadata | drop `directories`-based `Paths::system`; host supplies the base dir | removes `directories` |
| `src/mail/mod.rs` | `Client::login/resume/logout` persistence | replace `KeyringStore`/`Paths::system` with the injected store; drop `pub mod sync;` | no in-library credential/persistence ownership |
| `Cargo.toml` | manifests | remove `directories`, `keyring`, `rusqlite`, `tracing-subscriber`; bump `proton-crypto 0.12 → 0.14.1`, `proton-srp 0.8 → 0.8.4`; drop upstream `[[test]]` live/wiremock targets | aligns with the accepted set |

Everything else is kept and reused by P1–P4: SRP login / TOTP / human
verification (`auth/`), key-hierarchy unlock and the crypto format matrix
(`crypto/`), the typed API wrappers (`api/`), read/send/drafts/attachments/
organize/filters/contacts (`mail/`), the HTTP transport with 401-refresh /
429-retry / 9001-human-verification handling (`transport/`), and the wire models
(`model/`).

**Reproduced this task (not inherited from a README).** The probe applied
exactly the 7-file patch above and built it with the S1-approved
`proton-crypto 0.14.1` for the real Android target:

- Host build + unit tests: `48 passed; 0 failed` (`docs/s3-command-transcript.md`
  §A1; disposable `s3-fork-host-tests.log`).
- `aarch64-linux-android` release build: PASS (`docs/s3-command-transcript.md`
  §A2; `libproton_core.rlib` extracts to ELF64 `AArch64` objects).
- Cdylib link into a real Android shared object: `libproton_fork_probe.so`
  (460,712 bytes), `ELF64 AArch64 DYN`, `LOAD` align `0x4000`, `NEEDED libdl.so
  libc.so` (`docs/s3-command-transcript.md` §A3; disposable
  `s3-fork-cdylib-build.log`).
- Boundary gate on the extracted graph: PASS, `OK (338 resolved packages
  checked)`, `rustpgp` only; `gopenpgp-sys`, `rusqlite`, `sqlite3`, `keyring`
  absent. Reproduce with `cargo metadata --format-version 1 --filter-platform
  aarch64-linux-android` plus the feature-resolved `cargo tree`; see
  `docs/s3-command-transcript.md` §A4. (An *unfiltered* `cargo metadata` also
  lists inactive *optional* deps such as `gopenpgp-sys` and would
  false-positive the gate, so it must be run per-target.)

These build, link and gate outcomes are reproduced with exact commands in the
committed transcript **`docs/s3-command-transcript.md`** (§A); the underlying
raw logs previously lived only in disposable `/tmp` storage.

**Upkeep budget (frozen).** On each upstream release: (1) re-apply the ~450
deleted lines / 7-file removal patch above; (2) keep the `proton-crypto` pin on
the S1-approved version until a checked bump; (3) re-run the extraction
`aarch64` build, the boundary gate, and the synthetic crypto fixtures before
accepting; (4) keep a rollback to the last green pinned revision. Any upstream
change that expands the patch surface substantially (e.g. re-introducing
persistence ownership or a second native `links`) is a stop-and-return-to-user
trigger per the spec.

### 10.5 Protected session fields

The candidate splits non-secret metadata (0600 JSON file) from secrets (OS
keychain). S3 freezes what Pige must protect and where it lives:

| Field | Candidate location | Sensitivity | Pige (S3) decision |
| --- | --- | --- | --- |
| `uid` | session JSON | non-secret identifier | envelope metadata |
| `app_version`, `base_url`, `password_mode`, `user_agent` | session JSON | non-secret | typed account config |
| `access_token` | OS keychain | **secret** | Pige secret envelope |
| `refresh_token` | OS keychain | **secret** (rotated) | Pige secret envelope; serialize rotation, atomic save |
| `skp` (primary user-key passphrase) | OS keychain | **secret** — unlocks every address key | protected unlock material; envelope-held |
| unlocked keys (armored key + passphrase) | in-memory `KeyStore`/`StoredKey` only | **secret** | memory only; wipe on lock/logout |
| login password | **not persisted** | — | never retained |

The candidate derives `skp` with bcrypt (`crypto/kdf.rs`) and re-imports keys
per operation, so it does not persist the account password — this satisfies the
"no retained login password" part of Review Focus #4. Token refresh uses
`POST /auth/v4/refresh` with a rotating `RefreshToken` and a `save_tokens`
callback. Residual concern: the in-memory `StoredKey` passphrase is a
`secrecy::SecretString` but there is no explicit wipe-on-lock in the candidate;
P1 must add `UnlockedKeyStore::lock()` wiping/releasing material.

### 10.6 Key-policy and format matrix

Reading (`mail/read.rs` + `crypto/message.rs`):

| Input form | Candidate handling | Verdict / result |
| --- | --- | --- |
| Proton internal E2E (armored body) | `decrypt_body` | `Verified` / `Unsigned` / `Unverified` / `Invalid` |
| MIME `multipart/*` body | `mail-parser` extraction | displayable text/html |
| HTML body | `html::sanitize` | active content stripped |
| Attachments | key packet → session key → data packet (`decrypt_attachment`) | plaintext bytes |
| External PGP-signed mail | only **verified internal** sender keys are fetched (`keys/all` `Address`) | external/WKD keys ignored → `Unverified`, never `Verified` |
| `PGP_MIME (16)` / `CLEAR_MIME (32)` | constants defined but not produced/parsed distinctly | unsupported/indistinct |
| Unknown/unsupported encrypted form | decrypt returns `Err` | surfaced as error, not a fake `Verified` |

Sending (`mail/send.rs`), per recipient after `keys/all` lookup:

| Recipient | Candidate package | S3 verdict |
| --- | --- | --- |
| Proton internal (verified key) | `Type 1` E2E | acceptable |
| External with WKD/unverified key | `Type 8` PGP-inline | `Type 8` is **OpenPGP-inline encrypted to the recipient's key**: the payload is E2E-encrypted to the contact and is *not* readable by Proton's servers. It is **not** Proton-internal E2E (so Proton cannot verify/decrypt it), but it is *not* plaintext — do not conflate it with the genuinely plaintext `Type 4` case below. Policy must still confirm inline PGP is the contact's expressed preference |
| External keyless + EO password | `Type 2` encrypted-outside | not required in v1; acceptable but out of scope |
| External keyless, no EO password | **`Type 4` cleartext, `tracing::warn!` only** | **BLOCKER — silent downgrade** |

**Sender alias and recipient packaging.** `SendOptions.from` selects an alias by
case-insensitive email (`address_for_email`); with no `from`, the primary
address (lowest API `Order`) is used. An explicit `from` that matches no unlocked
address yields `NotFound { kind: "sender address" }`, so a wrong alias is
rejected rather than silently sent from the primary. Recipients are packaged as
separate `Addresses` entries per recipient (deduped case-insensitively), so a
Bcc-only recipient is not folded into a visible To/Cc entry in the client
payload; final Bcc privacy still depends on Proton server packaging (unverified,
BLOCKED).

Signature policy (synthetic evidence): `Unverified` is distinct from `Verified`;
a body signed by an unknown key surfaces as `Unverified` (not `Verified`); the
provider rejects a bad detached signature. `Invalid` is reachable only for a
same-key-id tampered signature, which the public integrated API did not let us
synthesize.

### 10.7 Security and limitation findings

1. **Silent cleartext downgrade — BLOCKER (Review Focus #3).**
   `mail/send.rs` (`finish_send`) classifies each recipient as internal (Type 1),
   external-with-key (Type 8), EO (Type 2), or **else `Type 4` cleartext with
   only a log warning**. It never consults the contact's encryption/signing
   preference, and it sends anyway. This violates the spec ("Never silently
   downgrade to plaintext") and the brief ("missing policy visibility is a
   blocker, not permission to send cleartext"). P4 must resolve policy *before*
   delivery and fail closed; the candidate's `send.rs` is **not frozen as-is**.
2. **Encrypted contact policy is not readable.** `api/contacts.rs::Contact`
   exposes only `ID`, `Name` and `ContactEmails` (plaintext). Proton's encrypted
   contact `Cards` — which carry the per-contact `x-pm-encrypt`/`x-pm-sign`
   preferences — are neither fetched nor decoded. A "require encryption" contact
   therefore cannot be honored through the candidate; this is a prerequisite for
   safe P4 submission.
3. **Silent address-key fallback (Review Focus #1).** `mail/read.rs::decrypt_message`
   resolves the address as `keys().address(msg.address_id).or_else(primary_address)`.
   On a multi-address / two-password account this can silently use the primary
   key when the correct address key is missing instead of surfacing a key
   problem. Decryption will usually error rather than leak plaintext, but the
   fallback is a correctness/policy hazard that P1/P3 must remove.
4. **Unknown signer is `Unverified`, not `Invalid`** (synthetic finding). This is
   fail-safe (never `Verified`), but it means external/non-Proton signatures can
   never reach `Verified` through this read path, and `Invalid` is only reachable
   for same-key-id tampering. The UI must present `Unverified` honestly.
5. **Key Transparency absent.** `proton-crypto 0.14.1` `keytransparency.rs`
   defines only `KTFailure`/`VerificationError` types; `proton-core` never calls
   KT. This is a disclosed limitation, never a substitute for signature validity.
6. **FIDO2/WebAuthn unsupported.** `auth/mod.rs` returns a typed, actionable
   error (`Error::Other`) when the 2FA bitmask indicates a security key and no
   TOTP. Correct behaviour: explicit unsupported, not bypassed or downgraded.
7. **Human verification is a host callback, not a CLI scrape.**
   `transport::HvResolver` lets the host satisfy the `9001` challenge (the
   candidate's own synthetic test proves the retry path). This means the design
   *can* satisfy the brief's "CAPTCHA solvable through an actual Android-host
   interaction" — but that on-device interaction is unverified (BLOCKED).
8. **API instability / anti-abuse.** The Proton API is unofficial; the candidate
   handles `429` (Retry-After, one retry) and `9001` (HV) but this remains an
   operational risk. Honest app identification is the mitigation (10.3).
9. **App-identification affordance** (10.3) — the default is honest (`Other`)
   but the library invites impersonation; Pige must constrain it.
10. **Bcc handling.** Packaging is per-recipient and `dedupe` is case-insensitive
    (`mail/send.rs`), so Bcc is not folded into a visible recipient entry in the
    client payload; final Bcc privacy still depends on Proton server packaging
    (unverified, BLOCKED).

**Stop-condition status (brief: "Stop if … policies cannot be honored").**
This stop condition is **triggered by the candidate as extracted**: it cannot
read the encrypted contact policy (§10.7 #2) and it silently degrades keyless
external recipients to cleartext (§10.7 #1), so the recipient's policy cannot be
honored. S3 therefore does **not** accept the candidate unchanged and does not
proceed to integration on it. It is being handled as an explicit
**user-reviewed exception** — on 2026-11-08 the user authorized continuing the
feasibility work without blocking on this trigger, on the conditions that (a) the
candidate is *not* frozen as-is, (b) P4 must resolve recipient policy before
delivery and fail closed, and (c) restoring policy visibility + a fail-closed
send path is a hard prerequisite before any real send. This is a logged decision,
not a silent deferral; if the user withholds that exception, the correct outcome
is BLOCKED. (The other two brief stop triggers — only-official-client
impersonation, and an unmaintainable fork — are untriggered as far as known:
honest identification is *required* and live acceptance UNVERIFIED per §10.3, and
the patch surface is the small 7-file set in §10.4.)

### 10.8 Case checklist (brief line 31)

Status is per-case. "synthetic" means exercised off-line on this host with no
account (reproduced here); "static" means source inspection only, explicitly
**not** a reproduced test; "live" cases were not run.

| # | Case | Status | Evidence |
| --- | --- | --- | --- |
| 1 | one/two-password accounts | **BLOCKED** (synthetic partial) | `password_mode == 2` requires `mailbox_password` (static, `mail/mod.rs`); real 1-pw and 2-pw accounts needed. |
| 2 | TOTP | **BLOCKED** | Candidate implements TOTP bit 0 (static, `auth/mod.rs`); no synthetic TOTP test run; live code required. |
| 3 | human verification / CAPTCHA | **PASS (synthetic) / BLOCKED (Android)** | `resolves_human_verification_then_retries_with_headers` (host, from the fork's upstream tests) proves the `9001` callback + retry; on-device CAPTCHA is BLOCKED. |
| 4 | invalid SRP proof/modulus | **BLOCKED** | `proton-srp` verifies the signed modulus and the server proof (static, `auth/mod.rs`); a tampered modulus/proof run needs a live account. |
| 5 | resumed rotating session | **PASS (synthetic) / BLOCKED (process death)** | `session::save_load_clear_roundtrip` and `transport::refreshes_then_retries_on_401` (host) show persistence + rotation logic; killed-process resume on device is BLOCKED. |
| 6 | multiple address keys | **PASS (synthetic partial) / BLOCKED (live)** | `primary_address_is_first`, `addresses_by_order_sorts_ascending_stably` (host); real multi-key decrypt of old mail is BLOCKED. |
| 7 | internal/external read | **BLOCKED** | Needs a disposable account with internal and external mail. |
| 8 | MIME/attachments | **PASS (synthetic) / BLOCKED (live)** | `attachment_roundtrip`, `transport_body_wraps_and_decrypts`, `eml_has_headers_and_body`, `html` sanitizer tests (host); live fetch BLOCKED. |
| 9 | mixed-recipient send | **FAIL (static) / BLOCKED (live)** | `mail/send.rs` silently emits `Type 4` cleartext for keyless external recipients (§10.7 #1). No authorized send was performed. |
| 10 | encrypted contact policy | **BLOCKED / not representable** | `Contact` has no encrypted `Cards`; policy cannot be read (§10.7 #2). |
| 11 | unsupported FIDO2-only login | **PASS (static inspection only — not a live pass) / BLOCKED (live)** | `auth/mod.rs` returns a typed actionable error for a FIDO2-only 2FA bitmask; a real FIDO2-only account was not available. |
| 12 | event expiry/reset | **BLOCKED** | Event sync is **deleted** by the frozen patch (§10.4 removes `mail/sync.rs`); **P2 must re-own and reimplement** event stream/cursor handling and treat a server `Refresh` event by re-snapshotting rather than fabricating deltas. Real event reset/pagination needs a live account. |
| 13 | contact metadata | **BLOCKED** | Candidate reads only name + emails (static); live contact list needed; encrypted fields/editing are out of v1 scope. |
| 14 | signed/unsigned/invalid signature | **PASS (synthetic, partial)** | `self_draft_roundtrip_verifies` (Verified), `unverified_when_no_sender_key` (Unverified), `unknown_signer_is_unverified_not_verified`, `unsigned_body_is_unsigned_not_verified`, `provider_rejects_bad_detached_signature` (host). Integrated `Invalid` path not reproducible. |

No case is marked PASS without having been run; no output is fabricated.
"static" findings are labelled as inspection, not as reproduced tests.

### 10.9 Exact blocked prerequisites and the commands that would run them

Missing for every live/device/send case:

1. A **disposable Proton account** (placeholder `proton-disposable@proton.me`)
   with a known one-password mode; a **second disposable two-password account**;
   TOTP enabled on both.
2. A **human-verification/CAPTCHA interaction** performed on the device.
3. The **Android device** already attached in S2 (iPlay60 mini Turbo, API 34,
   `arm64-v8a`); a **plaintext external recipient** mailbox to observe the
   external delivery class; an **external PGP/WKD contact** and a
   **keyless contact marked require-encryption** for the policy cases.
4. **Explicit user authorization for test sends** — which S3 does not have, so no
   message was sent.

Build the extracted library + probe APK (disposable, `/tmp`):

```bash
# extracted fork at /tmp/opencode/pige-provider-probes/proton-core-fork
export ANDROID_NDK_HOME="$HOME/Android/Sdk/ndk/28.2.13676358"
TOOLCHAIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TOOLCHAIN/aarch64-linux-android34-clang"
export CC_aarch64_linux_android="$CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
export AR_aarch64_linux_android="$TOOLCHAIN/llvm-ar"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"
cd /tmp/opencode/pige-provider-probes/proton-fork-cdylib
cargo build --release --target aarch64-linux-android   # -> libproton_fork_probe.so
# package into a disposable arm64-v8a probe app (same Gradle/JDK as scripts/build.sh)
JAVA_HOME=/opt/jbr-21 ANDROID_HOME="$HOME/Android/Sdk" \
  bash android/gradlew -p <probe-app-dir> --no-daemon assembleDebug
adb install -r <probe-app-dir>/app/build/outputs/apk/debug/app-debug.apk
```

Login / TOTP / two-password / delayed logout / cold resume (candidate live tests,
once credentials + send authorization exist):

```bash
PROTON_TEST_USER=<disposable> PROTON_TEST_PASSWORD='<pw>' \
PROTON_TEST_TOTP=<6-digit> \
PROTON_TEST_MAILBOX_PASSWORD='<mailbox-pw>' \
cargo test -p proton-core --test live -- --nocapture --test-threads=1
# process-death resume: kill and relaunch the probe, expect no user interaction
adb shell am force-stop bitshift.pige.protonprobe
adb shell am start -n bitshift.pige.protonprobe/.ResumeProbeActivity
```

Human verification through an Android-host interaction (not console scraping):

```bash
# Probe surfaces the 9001 challenge (WebUrl + token) in the Activity, the user
# solves the CAPTCHA in the in-app WebView/browser, the token is returned to the
# HvResolver; assert the bound attempt resumes. Expect a WebView/CAPTCHA
# Activity, never `--captcha-token` shell scraping.
adb shell am start -n bitshift.pige.protonprobe/.HvChallengeActivity
```

Honest-identification and API-access probe (no secrets committed):

```bash
# create an unauthenticated session with an honest external identity only:
curl -s -X POST https://mail.proton.me/api/auth/v4/sessions \
  -H 'x-pm-appversion: Other' -d '{}'
# fetch the SRP modulus and verify the server proof in the client (proton-srp);
# expected failure modes: rejected appversion, or 9001 human verification.
curl -s -X POST https://mail.proton.me/api/core/v4/auth/info \
  -H 'x-pm-appversion: Other' -d '{"Username":"<disposable>"}'
```

Mixed-recipient / policy send (requires explicit authorization; not run):

```bash
# Probe composes one message to: an internal address, an external plain
# address, an external PGP/WKD contact, and a keyless contact flagged
# require-encryption. Expected (required) behaviour:
#   - internal + external-PGP get their E2E/PGP package;
#   - the keyless *require-encryption* recipient BLOCKS the whole send with an
#     actionable unsupported-policy error (never Type 4 cleartext);
#   - a plain external recipient is explicitly classified and shown as
#     "ordinary external (unencrypted to recipient)".
# Current candidate behaviour FAILS this (Type 4 clear + warning); see §10.7 #1.
adb shell am start -n bitshift.pige.protonprobe/.SendPolicyProbeActivity
```

Event expiry/reset (live): list, mutate remotely, force an event `Refresh`, and
assert the client re-snapshots rather than inventing deltas. This must be tested
against **P2's re-owned event sync**, not the candidate: the frozen patch deletes
`mail/sync.rs` (§10.4), so the candidate has no event-sync code post-patch.
Needs a live account.

**Environment note (downstream builds).** Building the full workspace with
`CARGO_TARGET_DIR` on the small 16 GB `/tmp` tmpfs fails to link the `pige` GUI
once the tmpfs fills (`No space left on device`, surfaced as a link error). It is
a space artifact, not a code defect; keep the target directory on the larger
`/home` filesystem. Recorded in `docs/s3-command-transcript.md` ("Capture note").

The historical probe scaffold and raw output lived under a disposable scratch
directory (never committed); the durable, sanitized evidence is now
`docs/s3-command-transcript.md`.

### 10.10 Dependency and extraction decision

**Accepted (with conditions):** extract `proton-core 0.1.2` into a small
MIT-preserving fork and build it against the S1-approved `proton-crypto 0.14.1`
Rust (`rustpgp`) backend. This decision is grounded in a **reproduced** build
(host tests + real `aarch64-linux-android` `.so` + boundary gate, §10.4), not an
upstream README claim; exact commands and outcomes are committed in
`docs/s3-command-transcript.md`.

This acceptance is **not clean**. The brief's stop condition "Stop if …
policies cannot be honored" **is triggered** by the candidate as extracted (§10.7
stop-condition status) and is handled as an explicit **user-reviewed exception**
the user granted on 2026-11-08, on the conditions listed in §10.7. Without that
exception the correct decision is BLOCKED; the exception does not waive the P4
prerequisites below.

**Explicitly not done here (owned by later tasks):**

- No `rust/vendor/proton-mail-api/` fork repository is created, and no
  dependency is added to the committed workspace (S3 is a gate, not an
  integration; plan 04 owns the vendored fork).
- Live interop, CAPTCHA-on-device, cold resume, event reset, and any send remain
  BLOCKED (§10.8, §10.9).
- The cleartext-downgrade fix and the encrypted-contact-policy read are
  **required** before P4 and are not present in the candidate; the fork's
  `mail/send.rs`/`api/contacts.rs` are therefore not frozen as-is.
- Key Transparency and FIDO2 remain unsupported and documented.

### 10.11 S3 PASS / FAIL / BLOCKED matrix

| Item | Status | Evidence |
| --- | --- | --- |
| `ProtonDependencyDecision` section | PASS | §10.1–§10.10 |
| Accepted source/version/features frozen | PASS | §10.2 |
| Honest app identification / access expectations | PASS (static inspection only — not a live pass) / BLOCKED (live acceptance) | §10.3, §10.9 |
| Fork / extraction patch inventory + upkeep budget | PASS | §10.4 |
| Extracted fork host build + tests | PASS | §10.4; `docs/s3-command-transcript.md` §A1 (48/0) |
| Extracted fork `aarch64-linux-android` build/link | PASS | §10.4; `docs/s3-command-transcript.md` §A2–§A3 |
| Boundary gate on extracted graph | PASS | §10.4; `docs/s3-command-transcript.md` §A4 (`OK (338 resolved packages checked)`) |
| Protected session fields frozen | PASS | §10.5 |
| Key-policy / format matrix | PASS | §10.6 |
| Security/limitation findings recorded | PASS | §10.7 |
| Stop condition "policies cannot be honored" | **TRIGGERED — user-reviewed exception** | §10.7 stop-condition status; §10.10 |
| Mixed-recipient cleartext downgrade | **FAIL (static inspection)** | §10.7 #1 |
| Encrypted contact policy readable | **FAIL (static inspection, missing)** | §10.7 #2 |
| Case checklist present (14 cases) | PASS | §10.8 |
| Live one/two-password, TOTP, read, events, contacts | BLOCKED | §10.8, §10.9 |
| Human verification on Android host | BLOCKED | §10.8, §10.9 |
| Cold-resume after process death | BLOCKED | §10.8, §10.9 |
| Any authorized test send | BLOCKED (no authorization) | §10.9 |
| Key Transparency | Not implemented (documented) | §10.7 #5 |
| FIDO2/WebAuthn | Unsupported (typed error) | §10.7 #6 |
| Baseline `cargo test --workspace` at the S3 prior commit `64db57c` | PASS | 19 suites, 354 passed, 0 failed, 22 ignored (disposable-log outcome, restated) |
| `cargo test --workspace` at the branch HEAD when S3 committed (`b75c977`, includes a concurrent F1 commit) | PASS | 20 suites, 360 passed, 0 failed, 22 ignored (`s3-head-test.log`, disposable) |
| Full `cargo test --workspace` at branch HEAD `8589d8a` (fix re-run) | PASS | 20 suites, 360 passed, 0 failed, 22 ignored (`docs/s3-command-transcript.md` §B2) |
| Boundary gate on committed workspace | PASS | `OK (619 resolved packages)`; `docs/s3-command-transcript.md` §B1 |

### 10.12 Review-focus answers

- **#1 multi-address / wrong key.** Not satisfied as-is: `mail/read.rs` silently
  falls back to the primary address key (§10.7 #3). Must be removed in P1/P3.
- **#2 invalid/unsupported/untrusted must not look verified.** Satisfied in the
  read path: `Unverified`/`Unsigned`/errors are distinct from `Verified`, and
  external signatures cannot reach `Verified` (§10.6, §10.7 #4–#5).
- **#3 mixed recipients must not lose encryption policy.** **Not satisfied** —
  the candidate silently downgrades keyless external recipients to cleartext
  (§10.7 #1). Blocker for P4.
- **#4 refreshed session after process death without a retained password.**
  Partly satisfied: the password is not persisted (`skp` is stored instead) and
  rotation logic is tested synthetically; process-death resume is BLOCKED.
- **#5 CLI CAPTCHA helper unusable on Android.** Satisfied by design: the
  candidate exposes a `HvResolver` host callback, so an Android-host CAPTCHA
  interaction is possible without console-token scraping; the on-device run is
  BLOCKED (§10.9).

### 10.13 Evidence index

**Committed (durable, sanitized):**

- `docs/s3-command-transcript.md` — exact commands + captured outcomes for the
  extracted-fork host tests (§A1), `aarch64-linux-android` build/link (§A2–§A3),
  the extracted-graph boundary gate (§A4), the committed-workspace boundary gate
  (§B1) and the full `cargo test --workspace` (§B2), plus the tmpfs link-failure
  capture note. This is what §10.4 and §10.11 link to.

**Disposable scratch (`/tmp/opencode/pige-provider-probes/`, never committed) —
retained only as the historical run that the committed transcript restates:**

- `proton-core-fork/` — the extracted fork built this task.
- `proton-fork-cdylib/` — wrapper that links the fork into an Android `.so`.
- `evidence/s3-fork-host-tests.log` — host unit tests (48 passed). §10.11; restated §A1.
- `evidence/s3-fork-arm64-build.log` — `aarch64-linux-android` release build. §10.11; restated §A2.
- `evidence/s3-fork-cdylib-build.log` — Android `.so` link. §10.11; restated §A3.
- `evidence/s3-fork-metadata.json` / `s3-fork-tree.txt` (0.12.1) and
  `evidence/s3-fork-0141-metadata.json` / `s3-fork-0141-tree.txt` (0.14.1) —
  extracted-graph gate inputs. §10.11 (0.14.1); restated §A4.
- `evidence/s3-extraction-diff.txt` / `s3-extraction-cargo.diff` — patch surface (§10.4).
- `evidence/s3-baseline-64db57c-test.log` — baseline at the prior commit. §10.11.
- `evidence/s3-head-test.log` — workspace HEAD when S3 committed. §10.11.

No secret, token, key, address or message plaintext is committed.
