//! Protected authentication: secret envelopes, staged pending credentials,
//! generation-safe account sessions, and single-flight token refresh.
//!
//! Redaction is structural: secrets are the `secrecy` crate's [`SecretString`]
//! (re-exported as [`secrecy`]), every secret-bearing type has a redacting
//! `Debug`, and the envelope serialization is explicit rather than derived, so a
//! secret cannot reach a log through a `derive`.
//!
//! Adapters implement [`AuthenticationService`] and [`TokenSource`]; the
//! application only ever sees [`AuthProgress`] and a staged
//! [`StagedSecretRef`], never the secret itself.
//!
//! [`SecretString`]: secrecy::SecretString

pub mod fake;
pub mod secrets;
pub mod session;
pub mod types;

pub use fake::{FakeAuthScript, FakeAuthService, FakeTokenRefresher};
pub use secrets::{
    CredentialSecretRepository, GoogleSessionSecret, MAX_ENVELOPE_BYTES, ProtonSessionSecret,
    SECRET_ENVELOPE_VERSION, SecretEnvelope, SecretRepository, StagedSecretRef, StoredCredentials,
    credential_error, store_error_to_provider, sweep_expired_pending_auth,
};
pub use session::{AccountSessionManager, CachedTokenSource, TokenRefresher, TokenSource};
pub use types::{
    AuthAnswer, AuthChallenge, AuthProgress, AuthRequest, AuthenticatedAccount, AuthenticationService,
    BrowserAnswer, BrowserChallenge, HumanChallenge, HumanVerificationAnswer, SecretPersistence,
};

/// Re-exported so callers can name the secret wrapper without adding the crate
/// themselves.
pub use secrecy;
