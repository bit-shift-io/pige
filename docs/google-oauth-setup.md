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
| Refresh | Desktop stores the rotated refresh token in the per-account F3 secret envelope and refreshes single-flight. Android keeps **no device refresh token**; the host returns a fresh access token while the grant lives. |

Scopes requested:

- Mail: `https://www.googleapis.com/auth/gmail.modify` (+ `openid`, `email`,
  `profile`).
- Contacts (only when the user enables contacts):
  `https://www.googleapis.com/auth/contacts.readonly` and
  `https://www.googleapis.com/auth/contacts.other.readonly`.
  Denying contacts does not affect mail or local history learning.

## What the user must create in Google Cloud Console

1. Create/select a project; enable the **Gmail API** and **People API**.
2. Configure the OAuth consent screen (app name, logo, homepage, privacy policy,
   terms). Declare the scopes above. While unverified, add the disposable test
   account as a test user.
3. Create a **Desktop app** OAuth client. Supply its client id as
   `GOOGLE_DESKTOP_CLIENT_ID`. Desktop apps are public clients: PKCE is used and
   no client secret is sent, even if the console displays one.
4. Create an **Android** OAuth client for package `bitshift.pige` with both
   signing SHA-1s from `docs/provider-feasibility.md` §9.2:
   - release/shared test key `7E:4B:01:BC:E7:61:75:E3:89:38:88:5D:91:A6:26:FE:D3:07:21:AD`
   - local debug key `A0:4B:69:80:86:C4:44:A4:B3:6A:43:6D:98:11:CA:EE:64:40:B7:67`
   There is no redirect URI for this client.
5. Optional, only for Android offline/background refresh after process restart:
   create a **Web application** client and supply its id/secret as
   `GOOGLE_WEB_SERVER_CLIENT_ID` plus a Pige auth-server exchange URL whose
   server holds `GOOGLE_WEB_SERVER_CLIENT_SECRET`. The app passes the client id
   to `requestOfflineAccess`, receives a `serverAuthCode`, and sends it to that
   server endpoint (`AndroidOfflineExchange { server_client_id, exchange_url }`);
   the device never carries the Google client secret. Without this, Android uses
   on-demand `authorize()` and keeps no refresh token on device. This choice is
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
