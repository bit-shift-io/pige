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
- Shell wiring (G5) will read the client ids from the platform configuration and
  register `google_auth_service(...)` as `api::Config.auth` plus
  `google_registry(...)`. G1 adds the mechanism, not the production onboarding
  UI.

## Offline verification (no live network)

`cargo test -p pige_core --features provider-google --test google_auth` drives
all desktop token exchange and refresh logic against a local HTTP mock and
drives the Android contract with a recording fake host. Named cases cover PKCE
state mismatch, cancellation, Activity recreation binding, persistence across
restart, refresh rotation/reauth, denied contacts, expired callbacks, and HTTP
error redaction.

## Still BLOCKED (needs the registrations above)

S2 §9.7/§9.11 live cases remain BLOCKED: `desktop_pkce_cancel_and_state`,
`android_consent_return_and_recreation`, `resume_after_process_restart`,
`contacts_later_grant`, `revoked_grant_requires_login`, the Gmail/People API
probes, and `play_services_absent_or_disabled`. They must be recorded from a
real device/account; no mock result is reported as live evidence.
