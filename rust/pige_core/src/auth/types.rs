//! Provider-neutral interactive authentication vocabulary.
//!
//! These types are the *only* thing the application sees from an authentication
//! attempt: challenges a user can act on, redacted answers, and a completed
//! account that references a protected credential instead of carrying one.
//! Concrete provider SDK types, wire payloads, and credentials never appear
//! here.

use std::fmt;

use async_trait::async_trait;
use secrecy::SecretString;

use crate::account::{Account, AccountId};
use crate::providers::{AuthAttemptId, ProviderError, ProviderId};

use super::secrets::StagedSecretRef;

/// Where a completed credential must live: persisted in the OS store (desktop
/// with a refresh token) or kept in process memory only (a keychain-less host or
/// Google's on-device access-token model).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretPersistence {
    Persistent,
    SessionOnly,
}

/// What an authentication attempt is asking the user (or the platform) for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthChallenge {
    /// Continue in the system browser (desktop PKCE or Google's hosted flow).
    Browser(BrowserChallenge),
    /// Supply the account password for an IMAP/SMTP or mailbox login.
    Password { username: Option<String> },
    /// Supply a time-based one-time code.
    Totp,
    /// Supply the mailbox (IMAP) password after a provider login.
    MailboxPassword,
    /// Complete a provider-run human verification step (for example a Proton
    /// CAPTCHA token).
    HumanVerification(HumanChallenge),
    /// The provider has no supported flow for this platform.
    Unsupported { reason: String },
}

/// A browser authorization request. `url` is provider-validated and `expires_at`
/// bounds how long the attempt may wait; the attempt id in [`AuthProgress`]
/// binds the challenge to one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserChallenge {
    pub url: String,
    pub expires_at_ms: i64,
}

/// A provider-run human verification step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanChallenge {
    pub instructions: String,
    pub expires_at_ms: i64,
}

/// The user's (or platform's) answer to an [`AuthChallenge`]. Every variant is
/// redacted in `Debug`; secret answers never appear in logs.
///
/// Not `PartialEq`: comparing two answers would compare secret bytes, and a
/// caller should never branch on the contents of a secret.
#[derive(Clone)]
pub enum AuthAnswer {
    Browser(BrowserAnswer),
    Password(SecretString),
    Totp(SecretString),
    MailboxPassword(SecretString),
    HumanVerification(HumanVerificationAnswer),
}

/// The result of a browser redirect. The URL is not a secret, but it is redacted
/// alongside the rest of the enum so an authorization code query parameter can
/// never leak through a debug log.
#[derive(Clone, PartialEq, Eq)]
pub struct BrowserAnswer {
    pub redirect_url: String,
}

/// A provider-issued human-verification token.
#[derive(Clone, PartialEq, Eq)]
pub struct HumanVerificationAnswer {
    pub token: String,
}

impl fmt::Debug for AuthAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Browser(_) => "Browser",
            Self::Password(_) => "Password",
            Self::Totp(_) => "Totp",
            Self::MailboxPassword(_) => "MailboxPassword",
            Self::HumanVerification(_) => "HumanVerification",
        };
        write!(formatter, "AuthAnswer::{variant}(<redacted>)")
    }
}

impl fmt::Debug for BrowserAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BrowserAnswer(<redacted>)")
    }
}

impl fmt::Debug for HumanVerificationAnswer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HumanVerificationAnswer(<redacted>)")
    }
}

/// What to authenticate. A `None` account means this is a brand-new account: the
/// attempt reserves a provisional account id and generation without creating a
/// visible account or an active credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub provider: ProviderId,
    pub account: Option<AccountId>,
    pub enable_contacts: bool,
}

/// One step of an authentication attempt.
///
/// The `Complete` variant carries a whole [`Account`], so the enum is larger
/// than its other variants. The shape is the cross-plan contract and auth
/// progress is not a hot path, so it is left unboxed deliberately.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum AuthProgress {
    Challenge {
        attempt: AuthAttemptId,
        challenge: AuthChallenge,
    },
    Complete(AuthenticatedAccount),
    Cancelled,
}

/// A completed authentication: the account to create or attach, how its
/// credential must be persisted, and a reference to the staged protected secret.
///
/// The secret is *not* committed here; the account-creation or recoverable-switch
/// path (F10) commits the staged reference so an existing backend's credentials
/// are never overwritten while an in-flight submission still needs them.
#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub account: Account,
    pub persistence: SecretPersistence,
    pub credential_ref: StagedSecretRef,
}

/// Interactive authentication for one provider.
///
/// `begin` starts an attempt, `respond` advances it, `cancel` abandons it,
/// `resume` re-opens a stored session, and `revoke` removes an account's
/// credentials. Concrete provider logins live with their adapters (G1/P1); this
/// trait is the boundary they implement.
#[async_trait]
pub trait AuthenticationService: Send {
    async fn begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError>;
    async fn respond(
        &mut self,
        attempt: &AuthAttemptId,
        answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError>;
    async fn cancel(&mut self, attempt: &AuthAttemptId) -> Result<(), ProviderError>;
    async fn resume(&mut self, account: &AccountId) -> Result<AuthProgress, ProviderError>;
    async fn revoke(&mut self, account: &AccountId) -> Result<(), ProviderError>;

    /// Wait for a platform-owned interactive result (desktop loopback or an
    /// Android consent return). Providers with no platform callback keep the
    /// default `Unsupported` answer.
    async fn await_platform_completion(
        &mut self,
        _attempt: &AuthAttemptId,
    ) -> Result<AuthProgress, ProviderError> {
        Err(ProviderError::unsupported(
            "a platform-owned authentication callback",
        ))
    }

    /// Whether a validated platform result is already queued for `attempt`.
    /// This is checked without holding the service across a wait so the UI can
    /// poll an Android JNI result while Cancel remains responsive.
    fn has_platform_result(&self, _attempt: &AuthAttemptId) -> bool {
        false
    }

    /// Consume one queued platform result. `Ok(None)` means nothing arrived
    /// yet; it is not a failure and must never create an account.
    async fn deliver_platform_result(
        &mut self,
        _attempt: &AuthAttemptId,
    ) -> Result<Option<AuthProgress>, ProviderError> {
        Ok(None)
    }
}
