# Proton Mail support and limitations

Pige signs in directly to Proton's Mail API on Linux and Android. There is no
Proton Bridge, hosted proxy, Go service, or imitated official Proton client.
This page records what the adapter promises, what it deliberately refuses, and
which security properties are *not* claimed. The live checklist is at the end.

## Supported account and sign-in flow

| Step | Pige behaviour |
| --- | --- |
| New account | The account pane asks for the Proton email, then drives `ProtonAuthentication::begin_with_username`. `AuthRequest` has no email field, so the username-first entry point is required; IMAP and OAuth are never offered as a Proton fallback. |
| Password | SRP-6a with the server-signed group; server proof and modulus are verified before a credential is staged. |
| Two-step | A TOTP code is requested as a separate attempt-bound challenge. A FIDO2-only account is refused with an actionable message (enable TOTP or use a Proton app password); there is no bypass. |
| Two-password accounts | The mailbox password is requested only when the server reports `PasswordMode = 2`; user and address keys are unlocked from that password and the derived key passphrase. |
| Human verification | The provider's human-verification page is shown as a URL plus a token field. Raw challenge JSON is never rendered or logged. |
| Reconnect / unlock | `resume_auth` first tries the protected stored session and the mailbox-derived key passphrase. If the session is gone, the interactive flow starts again. A locked mailbox shows an unlock/reconnect prompt, not an empty inbox. |
| Remove / logout | Local generation invalidation, key wipe and credential deletion happen before a bounded best-effort server logout. A late login for a removed account is discarded. |

Contacts are optional and read-only. Enabling them attaches the
`proton.contacts` source, which performs a full snapshot (Proton exposes no
incremental contact cursor). Disabling keeps cached contacts and stops sync.

## Cached mail is decrypted in app-private storage

Pige caches message bodies, snippets, attachments and the local SQLite mirror so
the reader and autocomplete work offline. **These cached Proton messages are
decrypted at rest.** Pige does *not* add a second encryption layer around its
cache:

- The cache lives in the app's private data/cache directory
  (`$XDG_DATA_HOME/pige` on Linux, app-private storage on Android) and relies on
  the OS account/app sandbox and full-disk encryption.
- Android backup rules and the existing backup exclusion apply; Pige's JSON
  configuration backup never contains mail bodies, credentials or keys.
- Unlocked Proton private keys and mailbox passphrases stay in process memory
  only. They are wiped on logout/removal and never written to the cache or
  configuration document.
- A device backup or rooted-device extraction of the app directory can read
  cached decrypted mail. Do not treat Pige's cache as an encrypted mailbox
  archive.

Historical recipient learning stores sender/recipient addresses and snippets
derived from mail; it is local metadata, not a second copy of Proton's address
book.

## Encryption and signature limits

- **Verified** means the message signature verified against a Proton-internal
  key with usable trust flags. **Unsigned**, **Unverified**, **Invalid** and
  **Unsupported** are distinct reader states; a decrypted body is never
  implicitly shown as verified.
- External/WKD-signed mail is decrypted but shown as `Unverified`, never
  `Verified`.
- Unsupported body formats (encrypted MIME, password/EO forms, other PGP armor,
  an `E2E` flag without a supported packet) fail closed as `Unsupported`. Pige
  does not fall back to rendering cleartext.
- Compose resolves recipient policy before creating a draft. Internal Proton
  recipients get E2E packages, external OpenPGP recipients get OpenPGP, and an
  ordinary external recipient is labelled as *not end-to-end encrypted*. A
  contact that requires encryption or verified key material but has no usable
  key blocks the whole send; no lookup failure ever becomes cleartext.
- **Key Transparency is not implemented.** Pige trusts Proton's published key
  API plus authenticated TLS, and (for mandatory `X-PM-VERIFY`) a detached
  contact-card signature made by an account key. It cannot prove that a
  published key is the one Proton's Key Transparency log would show.
- **FIDO2/WebAuthn hardware sign-in is not implemented.** TOTP and
  password/mailbox-password flows are supported; a FIDO-only account is refused.
- Attachment detached signatures are retained but not independently verified;
  OpenPGP packet integrity (SEIPD/MDC) is enforced and corrupted packets fail.
- Contact cards are decrypted for recipient policy only. Full encrypted-contact
  editing, groups and writes are not implemented.
- No claim of Proton-client security parity is made. Pige is an independent
  client using an unsupported/private API surface.

## Honest client identity and API stability

Pige sends the vendored adapter's app version token `Other`; it does not
impersonate Proton Mail Web, Bridge, or an official mobile app. The Proton API
is a third-party, non-contractual interface:

- endpoints, SRP parameters, key formats and package types can change without
  notice; live tests are the only proof of current compatibility;
- Proton may rate-limit, require additional human verification, or reject
  clients it does not recognise;
- a format change fails closed (unsupported/reauth error) rather than silently
  downgrading to cleartext;
- the vendored library is a narrow maintained fork. Update and rollback rules
  live in [`proton-dependency-updates.md`](proton-dependency-updates.md).

## Factory capabilities

The Proton factory advertises only what P2–P4 implement:

| Capability | Value |
| --- | --- |
| mailbox read, body, attachment | yes |
| incremental mail sync and checkpointed reconciliation | yes |
| read/star/labels/move/archive/trash/permanent delete | yes |
| native submission | yes, server-managed Sent copy |
| contacts read | yes |
| incremental contacts, remote search, contact write, groups | no |
| server-side search | no |

## Live verification checklist

These steps need an explicitly authorised disposable Proton account. They are
**not** run by the normal test suite; `tests/live_proton.rs` is ignored and
fails clearly when its env is missing.

1. `mask`/`export` inside the shell:
   ```sh
   export PIGE_TEST_PROTON_EMAIL='disposable@proton.me'
   read -rsp 'account password: ' PIGE_TEST_PROTON_ACCOUNT_PASSWORD; export PIGE_TEST_PROTON_ACCOUNT_PASSWORD
   # only if the account uses a separate mailbox password:
   read -rsp 'mailbox password: ' PIGE_TEST_PROTON_MAILBOX_PASSWORD; export PIGE_TEST_PROTON_MAILBOX_PASSWORD
   # only if two-step is enabled:
   export PIGE_TEST_PROTON_TOTP='123456'
   # only if Proton asks for human verification:
   export PIGE_TEST_PROTON_HV_TOKEN='...'
   ```
2. `cargo test -p pige_core --features provider-proton --test live_proton -- --ignored --nocapture live_proton_read_and_sync`
3. `cargo test -p pige_core --features provider-proton --test live_proton -- --ignored --nocapture live_proton_logout_locks_keys`
4. Send only after verifying the recipient is the test account itself:
   `PIGE_TEST_ALLOW_SEND=1 cargo test -p pige_core --features provider-proton --test live_proton -- --ignored --nocapture live_proton_send_requires_explicit_opt_in`
5. In the app on Linux and Android: cold login, cancel while at TOTP/human
   verification (no account is created), kill and reopen (unlock prompt or
   resume), background delta, offline autocomplete, attachment open, a tampered
   signature, contact enable/disable, one internal and one external send, then
   logout/removal (keys locked, no late account resurrection).

Missing prerequisites are BLOCKED evidence, not a success. Do not use a
personal or production mailbox for these checks.

The deterministic release gate is `bash scripts/verify-providers.sh all`
(formatting, tests, clippy, every provider feature, dependency/license gate); it
never reads Proton credentials and never sends mail. Current live and on-device
status is recorded in
[`provider-release-verification.md`](provider-release-verification.md). The
extension/SDK-replacement recipe, including the Proton fork upgrade checks, is
[`adding-a-mail-provider.md`](adding-a-mail-provider.md).
