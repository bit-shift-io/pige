# Google OAuth setup (G1)

Task G1 implements the supported Google authorization flows; live account
confirmation is deliberately BLOCKED until the registrations below exist. The
S2 decision record is `docs/provider-feasibility.md` §9; this page is the
operator checklist for the values G1 expects to be injected.

No client id, client secret, token, or account is committed to the repository.

## What Pige supplies (already implemented)

| Flow | Mechanism |
| --- | --- |
| Desktop | System browser → `https://accounts.google.com/o/oauth2/v2/auth`; loopback redirect `http://127.0.0.1:<random port>/`; PKCE S256 + exact `state`; code exchange at `https://oauth2.googleapis.com/token` with no client secret (public desktop client). |
| Android | Google Identity Services `Identity.getAuthorizationClient(activity).authorize(...)` (`play-services-auth`); consent returns through a `PendingIntent`; no redirect URI, loopback, OOB code, or WebView. |
| Refresh | Refresh tokens are persisted in the per-account F3 secret envelope and refreshed single-flight. Desktop refreshes at `https://oauth2.googleapis.com/token`. Android with the offline exchange refreshes through the configured Pige exchange endpoint (the server holds the Web client secret), so it survives process restart; without the exchange Android keeps **no device refresh token** and the host returns a fresh access token while the grant lives. |

Scopes requested:

- Mail: `https://www.googleapis.com/auth/gmail.modify` (+ `openid`, `email`,
  `profile`).
- Contacts (only when the user enables contacts):
  `https://www.googleapis.com/auth/contacts.readonly` and
  `https://www.googleapis.com/auth/contacts.other.readonly`.
  Denying contacts does not affect mail or local history learning.

## People contacts (G3)

Two independent contact sources are enabled only when the user turns them on:

| Source id | Endpoint | Scope | Read mask |
| --- | --- | --- | --- |
| `google.saved` | `GET /v1/people/me/connections` | `contacts.readonly` | `names,emailAddresses,metadata` |
| `google.other` | `GET /v1/otherContacts` | `contacts.other.readonly` | `names,emailAddresses,metadata` |

Both requests set `sources=READ_SOURCE_TYPE_CONTACT` and
`requestSyncToken=true`. The masks intentionally omit `organizations`: Other
contacts do not support it, and saved contacts do not need it for autocomplete.
Compose never queries People; recipient suggestions come from the local R2
projection. `google.other` is stored with kind `other` so it ranks below a
saved contact while the same address still merges into one suggestion.

People sync tokens expire seven days after the full sync. An HTTP 410 /
`EXPIRED_SYNC_TOKEN` response restarts that source from a fresh full snapshot;
it never uses Gmail's `historyId`. A full snapshot removes locally absent
contacts only after its final page. A completed snapshot retains its
`nextSyncToken`, so the next maintenance cycle is an incremental delta; a
completed delta updates only the changed contacts and never removes contacts it
did not mention. A 403/permission denial disables only the affected contact
source, leaving mail, local history, cached contacts and the other contact
source intact. A 403 quota/rate-limit reason (`RATE_LIMIT_EXCEEDED`,
`quotaExceeded`, `RESOURCE_EXHAUSTED`, …) is retried instead of disabling the
source. Granting contacts later is a new authorization operation
(`AuthRequest.enable_contacts = true`), not an assumption that an existing Gmail
grant already covers People.

## What the user must create in Google Cloud Console

1. Create/select a project; enable the **Gmail API** and **People API**.
2. Configure the OAuth consent screen (app name, logo, homepage, privacy policy,
   terms). Declare the scopes above. While unverified, add the disposable test
   account as a test user.
3. Create a **Desktop app** OAuth client. Supply its client id as
   `GOOGLE_DESKTOP_CLIENT_ID`. Desktop apps are public clients: PKCE is used and
   no client secret is sent, even if the console displays one.
4. Create an **Android** OAuth client for package `bitshift.pige` with the
   shared test-key SHA-1
   `7E:4B:01:BC:E7:61:75:E3:89:38:88:5D:91:A6:26:FE:D3:07:21:AD`.
   In this checkout both debug and release builds are signed with
   `android/keystore/pige-release.jks` (verified with `apksigner` on both APKs),
   so one fingerprint covers them. The host-local debug key
   `A0:4B:69:…:40:B7:67` is only used when `android/keystore.properties` is
   absent — add it as a second fingerprint only then. There is no redirect URI.
5. Optional, only for Android offline/background refresh after process restart:
   create a **Web application** client and supply its id/secret as
   `GOOGLE_WEB_SERVER_CLIENT_ID` plus a Pige auth-server exchange URL whose
   server holds `GOOGLE_WEB_SERVER_CLIENT_SECRET`. The app passes the server
   client id to `requestOfflineAccess` (via
   `AndroidAuthorizationRequest::server_client_id`), receives a
   `serverAuthCode`, and sends it to that server endpoint
   (`AndroidOfflineExchange { server_client_id, exchange_url }`); the device
   never carries the Google client secret. The endpoint must also accept a
   `grant_type=refresh_token` request (client id, refresh token) and return a
   fresh access token, optionally rotating the refresh token; that is the path
   `provider_token_source` prefers whenever a refresh token is stored. Without
   this configuration Android uses on-demand `authorize()`, keeps no refresh
   token on device, and falls back to the host `acquire` path. This choice is
   the user distribution decision recorded in S2 §9.9.
6. Use a **disposable Google account** for live testing. Do not use a personal
   account, and separately authorize the single test send when G4 is verified.

## Injection points

- Rust: `GoogleOAuthConfig` (`client_id`, optional `client_secret`,
  `authorization_url`, `token_url`) and `GoogleEndpoints`. Desktop:
  `GoogleAuthentication::desktop(...)` with a `DesktopRedirect`
  (`LoopbackRedirect` in production, a fixed URI in tests). Android:
  `GoogleAuthentication::android(...)` with a `GoogleAndroidHost` and optional
  `AndroidOfflineExchange`.
- `GoogleProviderFactory::new(sessions, endpoints)` requires a token source
  provider; the shell attaches the configured authentication with
  `.with_authentication(authentication)` (or
  `.with_token_source_provider(...)`).
- Shell wiring (G5) lives in `rust/pige/src/platform.rs`. It composes the IMAP
  registry with `register_google(...)`, and installs
  `google_auth_service(...)` as `api::Config.auth`.

### Host values (no secrets in the repository)

Client ids are configuration, never credentials. The shell reads, in order of
precedence:

| Value | Environment | Local JSON key |
| --- | --- | --- |
| Desktop OAuth client id | `PIGE_GOOGLE_DESKTOP_CLIENT_ID` (or legacy `GOOGLE_DESKTOP_CLIENT_ID`) | `desktop_client_id` |
| Optional Web/Android server client id | `PIGE_GOOGLE_SERVER_CLIENT_ID` (or `GOOGLE_WEB_SERVER_CLIENT_ID`) | `server_client_id` |
| Optional server exchange URL | `PIGE_GOOGLE_SERVER_EXCHANGE_URL` | `server_exchange_url` |

The local file is `<config_dir>/google-oauth.json` (Linux:
`$XDG_CONFIG_HOME/bitshift/pige/google-oauth.json`; Android:
`<app files>/config/google-oauth.json`). It is optional and contains only those
public identifiers. Never put a refresh token, access token, client secret, or
account password in it. When the desktop client id is absent the Google provider
is not registered; any native Gmail account already in `pige.json` remains
visible as an "unavailable provider" account instead of being quarantined or
treated as a corrupt document.

Android does **not** need the desktop id: Play services identifies the Android
OAuth client by package name plus signing certificate. The server client id and
exchange URL are only for the optional offline-refresh path. Without them, the
Android app uses on-demand `authorize()`: it keeps an in-memory access token,
stores no device refresh token, and asks the user to sign in again after expiry
or process restart. That is the distribution choice for this build.

### Onboarding, contacts, and switching (G5)

- The account screen offers **Google account** separately from the manual
  **IMAP/SMTP** form. A new Google account has a label/display-name field and an
  optional "Sync Google contacts" choice; it never asks for a password or IMAP
  servers.
- An existing Gmail-over-IMAP account keeps editing its IMAP/SMTP fields. A
  separate "Switch to native Google" action is offered for `gmail.com` /
  `googlemail.com` addresses and calls `can_switch_backend` first. A pending,
  accepted-but-unfinalized, or uncertain submission refuses the switch; the
  account stays IMAP and no credential is overwritten.
- Enabling Google contacts is a new `AuthRequest { enable_contacts: true }`, not
  an assumption that the mail grant already covers People. On completion the two
  R6 sources (`google.saved`, `google.other`) are registered and enabled.
  Disabling contacts keeps cached contacts and only stops synchronization.
- The Android path is wired through `PigeBridge.launchGoogleAuthorization`
  (scopes JSON + optional `serverClientId`) and
  `GoogleAuthorization.nativeResult`. The Rust host forwards the request and
  the validated result is polled from the JNI inbox by
  `api::poll_platform_auth`; the attempt id binds every result, so a late answer
  after cancellation or Activity recreation cannot create an account.

## Release setup: console, consent, and signing

1. **Registrations.** Create one Google Cloud project. Enable the Gmail API and
   People API. Create a Desktop OAuth client for Linux and an Android OAuth client
   for package `bitshift.pige`. If a server-side exchange is ever added, create a
   Web application client as well; it is not required for this on-demand build.
2. **Minimal scopes.** Mail: `gmail.modify` plus `openid email profile`.
   Contacts: `contacts.readonly` and `contacts.other.readonly`, requested only by
   the contacts opt-in. Do not request broader Gmail scopes; permanent delete is
   deliberately unavailable under `gmail.modify`.
3. **Android identity.** The Android OAuth client must match:
   - package: `bitshift.pige`
   - debug and release SHA-1:
     `7E:4B:01:BC:E7:61:75:E3:89:38:88:5D:91:A6:26:FE:D3:07:21:AD`
     (shared key committed at `android/keystore/pige-release.jks`; selected by
     `android/keystore.properties`). Both build types use it, so one fingerprint
     covers local debug and release APKs.
   - If `android/keystore.properties` is absent, the host-local debug key is used
     instead; add that debug SHA-1 as a second Android client fingerprint only in
     that case. Changing the signing key requires a new Android OAuth client (or
     another registered fingerprint) or consent returns `DEVELOPER_ERROR`.
   There is no redirect URI, App Link, loopback, OOB code, or WebView on Android.
4. **Consent screen and verification.** While the OAuth consent screen is in
   **Testing**:
   - only accounts listed as test users can authorize (limit: 100 test users);
   - refresh tokens for external/test-user grants expire after **7 days**;
   - the user must be re-added if the project changes.
   `gmail.modify` is a restricted Gmail scope. Distributing to users outside the
   test list requires publishing the app and completing Google's OAuth
   verification (and, for restricted scopes, the applicable security assessment).
   Plan for a review lead time before a public release.
5. **Testing-user limits.** Desktop and Android each consume the same project's
   consent screen and test-user list. A disposable account should be a test user;
   do not use a personal account for send testing.
6. **Release signing.** For a public release, replace the shared test key with a
   dedicated release keystore held outside the repository, update
   `android/keystore.properties` (or the CI secret injection), sign with
   `scripts/build.sh release`, and register the release SHA-1 as the Android OAuth
   fingerprint. Keep the release keystore backed up: losing it prevents in-place
   upgrades, and rotating it requires a new OAuth client registration. The
   release scripts build both desktop and Android with `--features
   provider-google`.

## Offline verification (no live network)

`cargo test -p pige_core --features provider-google --test google_auth` drives
all desktop token exchange and refresh logic against a local HTTP mock and
drives the Android contract with a recording fake host. Named cases cover PKCE
state mismatch, cancellation, Activity recreation binding, persistence across
restart, refresh rotation/reauth, denied contacts, expired callbacks, and HTTP
error redaction.

G5 adds `cargo test -p pige_core --features provider-google --test
provider_lifecycle`, including the native onboarding/switch/contacts cases, and
`tests/live_google.rs`. Every live case is `#[ignore]` and requires its own
environment. An explicitly requested live run with missing values fails with a
clear message; it never reports a silent pass. Send additionally requires
`PIGE_TEST_ALLOW_SEND=1`. Example:

```text
PIGE_TEST_GOOGLE_CLIENT_ID=... \
PIGE_TEST_GOOGLE_REFRESH_TOKEN=... \
PIGE_TEST_GOOGLE_ACCOUNT_EMAIL=person@example.com \
cargo test -p pige_core --features provider-google --test live_google \
  -- --ignored --nocapture live_gmail_read_and_sync
```

The contacts-denial case also requires `PIGE_TEST_GOOGLE_CONTACTS_DENIED=1` and
a token without People scopes. Tokens and response bodies are never printed.

The ordinary deterministic gate is `bash scripts/verify-providers.sh all`; it
never reads these credentials, never runs an ignored live test, and never sends
mail. It checks formatting, tests, clippy, every provider feature, and the
dependency/license boundary. Release status and prerequisites are recorded in
[`provider-release-verification.md`](provider-release-verification.md); live
Google rows stay BLOCKED until the console registrations and disposable account
above are actually used.

## Recorded coverage still required from a live account

Cold resume, sync/delta, contacts denial, offline autocomplete, send, revoke, and
switch recovery must be recorded on Linux and Android against the disposable
account. Fixtures and mocks prove the wiring, not Google's live behavior. The
S2 §9.7/§9.11 cases remain BLOCKED until the console registrations above exist:
`desktop_pkce_cancel_and_state`, `android_consent_return_and_recreation`,
`resume_after_process_restart`, `contacts_later_grant`,
`revoked_grant_requires_login`, the Gmail/People API probes, and
`play_services_absent_or_disabled`. They must be recorded from a real
device/account; no mock result is reported as live evidence.

