//! The adapter-internal Proton HTTP client and endpoint configuration.
//!
//! [`ProtonEndpoints`] is the only place in Pige that names a Proton API root.
//! Production callers use [`ProtonEndpoints::production`]; tests and approved
//! deployments may inject a mock/alternate base URL. No token, SDK wire type, or
//! protocol error escapes `providers::proton`.
//!
//! [`ProtonClient`] shares one generation-aware [`TokenSource`] and one
//! [`UnlockedKeyStore`] per account. It does not own persistence: the F3
//! `SecretRepository` is the only credential writer.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;

use crate::auth::TokenSource;
use crate::providers::{ErrorKind, OperationContext, ProviderError};

use super::keys::UnlockedKeyStore;

/// The app version and headers accepted by S3. No user-agent impersonation.
pub const PROTON_APP_VERSION: &str = "Other";

/// Proton API roots used by the adapter.
#[derive(Clone, PartialEq, Eq)]
pub struct ProtonEndpoints {
    base_url: String,
}

impl ProtonEndpoints {
    /// The production Proton Mail API root.
    pub fn production() -> Self {
        Self {
            base_url: "https://mail.proton.me/api".to_string(),
        }
    }

    /// Inject an approved alternate HTTPS API root. The trailing slash is
    /// removed so request paths append cleanly. Cleartext URLs are rejected:
    /// bearer tokens and key material must never leave the process unencrypted.
    pub fn new(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let base_url = trim_base_url(base_url.into());
        if !base_url.starts_with("https://") {
            return Err(ProviderError::new(
                ErrorKind::Unsupported,
                "the Proton API base URL must be an https URL",
            ));
        }
        Ok(Self { base_url })
    }

    /// Inject a local mock API root for this crate's tests only.
    ///
    /// The constructor is compiled only for `cfg(test)` or when the
    /// `provider-proton-test-support` feature is enabled (the package's own
    /// dev-dependency does that for integration tests). Product builds, debug or
    /// release, cannot be pointed at a cleartext endpoint that would expose
    /// tokens.
    #[cfg(any(test, feature = "provider-proton-test-support"))]
    pub fn new_insecure_for_tests(base_url: impl Into<String>) -> Result<Self, ProviderError> {
        let base_url = trim_base_url(base_url.into());
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(ProviderError::new(
                ErrorKind::Unsupported,
                "the Proton API base URL must be an http(s) URL",
            ));
        }
        Ok(Self { base_url })
    }

    /// The API root without a trailing slash.
    pub fn as_str(&self) -> &str {
        &self.base_url
    }
}

fn trim_base_url(mut base_url: String) -> String {
    while base_url.ends_with('/') {
        base_url.pop();
    }
    base_url
}

impl fmt::Debug for ProtonEndpoints {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtonEndpoints")
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// One account's authenticated Proton client.
///
/// Cloning is cheap and shares the same token and key state. The client may be
/// constructed before a credential exists; the first token fetch then reports
/// reauthentication rather than making an unauthenticated request.
#[derive(Clone)]
pub struct ProtonClient {
    tokens: Arc<dyn TokenSource>,
    keys: Arc<UnlockedKeyStore>,
    endpoints: ProtonEndpoints,
}

impl ProtonClient {
    pub fn new(
        tokens: Arc<dyn TokenSource>,
        keys: Arc<UnlockedKeyStore>,
        endpoints: ProtonEndpoints,
    ) -> Self {
        Self {
            tokens,
            keys,
            endpoints,
        }
    }

    pub fn endpoints(&self) -> &ProtonEndpoints {
        &self.endpoints
    }

    pub fn token_source(&self) -> Arc<dyn TokenSource> {
        self.tokens.clone()
    }

    pub fn unlocked_keys(&self) -> &Arc<UnlockedKeyStore> {
        &self.keys
    }

    /// Fetch a current access token, bounded by the caller's operation context.
    pub async fn access_token(
        &self,
        ctx: &OperationContext,
    ) -> Result<SecretString, ProviderError> {
        self.tokens.access_token(ctx).await
    }

    /// Build a transport rooted at the configured API host and carrying the
    /// current access token and session UID.
    ///
    /// Automatic refresh is intentionally not installed on this transport: token
    /// rotation is owned by the F3 [`TokenSource`], so a request that sees a 401
    /// must re-enter [`Self::access_token`] instead of letting the vendor client
    /// persist tokens behind Pige's back.
    pub async fn http(
        &self,
        ctx: &OperationContext,
    ) -> Result<proton_core::transport::HttpClient, ProviderError> {
        ctx.ensure_active()?;
        let access = self.access_token(ctx).await?;
        ctx.ensure_active()?;
        let uid = self.keys.session_uid().ok_or_else(|| {
            ProviderError::reauthenticate("unlock the Proton mailbox to continue")
        })?;
        let http =
            proton_core::transport::HttpClient::new(self.endpoints.as_str(), PROTON_APP_VERSION);
        http.set_tokens(uid, access, SecretString::from(String::new()))
            .await;
        Ok(http)
    }
}

impl fmt::Debug for ProtonClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtonClient")
            .field("endpoints", &self.endpoints)
            .field("tokens", &"<authenticated token source>")
            .field("keys", &self.keys)
            .finish()
    }
}

/// Map a vendored-library failure onto the provider-neutral surface.
///
/// Wire bodies and raw library messages stay in `source_detail` so they cannot
/// reach the UI or ordinary logs. Authentication failures are already
/// redacted by construction.
pub(crate) fn map_proton_error(error: proton_core::Error) -> ProviderError {
    use proton_core::Error;

    match &error {
        Error::Unauthorized => {
            ProviderError::reauthenticate("the Proton session is no longer valid; sign in again")
        }
        Error::HumanVerification(_) => {
            ProviderError::reauthenticate("Proton requires human verification")
        }
        Error::Api(api) => match api.http_status {
            401 => ProviderError::reauthenticate(
                "the Proton session is no longer valid; sign in again",
            ),
            403 => ProviderError::forbidden("Proton did not allow this operation"),
            404 => ProviderError::not_found("the Proton item no longer exists"),
            409 | 422 => ProviderError::conflict("Proton rejected the request"),
            429 => ProviderError::rate_limited(Duration::from_secs(60)),
            status if status >= 500 => {
                ProviderError::transient("Proton is temporarily unavailable")
            }
            _ => ProviderError::transient("Proton rejected the request"),
        },
        Error::Http(_) => ProviderError::offline("could not reach Proton"),
        Error::Json(_) => ProviderError::transient("Proton returned an unreadable response"),
        Error::Srp(_) => ProviderError::reauthenticate(
            "Proton could not verify the server's SRP challenge; sign in again",
        ),
        Error::Crypto(_) => ProviderError::reauthenticate(
            "the Proton mailbox password could not unlock this account",
        ),
        Error::Session(_) | Error::Cache(_) | Error::Io(_) => {
            ProviderError::transient("the Proton session could not be read")
        }
        Error::NotFound { .. } => ProviderError::not_found("the Proton item no longer exists"),
        Error::Ambiguous(_) => ProviderError::conflict("the Proton item is ambiguous"),
        Error::Other(_) => ProviderError::transient("Proton could not complete the request"),
    }
    .with_source_detail(error.to_string())
}
