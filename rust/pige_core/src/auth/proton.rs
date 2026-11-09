//! Pige-owned Proton token rotation and credential helpers.
//!
//! The vendored library never persists a token: Pige's F3
//! [`SecretRepository`] is the only writer. Refreshing is serialized per
//! account, generation-aware, and saves the rotated refresh token *before* it
//! returns the new access token. A refresh whose session was invalidated or
//! whose account was removed fails without writing anything.

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Mutex as AsyncMutex;

use crate::providers::{OperationContext, ProviderError};

use crate::providers::proton::client::{PROTON_APP_VERSION, ProtonEndpoints, map_proton_error};

use super::secrets::{ProtonSessionSecret, SecretEnvelope, SecretRepository, StoredCredentials};
use super::session::TokenRefresher;

/// Reads a Proton session out of a protected envelope.
pub(crate) fn proton_secret(
    envelope: SecretEnvelope,
) -> Result<ProtonSessionSecret, ProviderError> {
    match envelope.credentials {
        StoredCredentials::Proton(secret) => Ok(secret),
        _ => Err(ProviderError::unsupported(
            "this account does not use Proton sign-in",
        )),
    }
}

/// Build the credential envelope from a successful login/resume.
pub(crate) fn proton_envelope(
    uid: impl Into<String>,
    access_token: SecretString,
    refresh_token: SecretString,
    skp: SecretString,
) -> SecretEnvelope {
    SecretEnvelope::proton(ProtonSessionSecret {
        uid: Some(uid.into()),
        access_token: Some(access_token),
        refresh_token: Some(refresh_token),
        skp: Some(skp),
    })
}

/// Refreshes one account's Proton access token.
///
/// A per-account serial mutex makes concurrent refreshes single-flight even if
/// callers hold different [`crate::auth::CachedTokenSource`] instances. The
/// rotated refresh token is persisted through the generation-gated repository
/// before the new access token is returned.
pub struct ProtonTokenRefresher {
    secrets: Arc<dyn SecretRepository>,
    endpoints: ProtonEndpoints,
    serial: AsyncMutex<()>,
}

impl ProtonTokenRefresher {
    pub fn new(secrets: Arc<dyn SecretRepository>, endpoints: ProtonEndpoints) -> Self {
        Self {
            secrets,
            endpoints,
            serial: AsyncMutex::new(()),
        }
    }
}

#[derive(serde::Deserialize)]
struct RefreshResponse {
    #[serde(rename = "AccessToken")]
    access_token: String,
    #[serde(rename = "RefreshToken")]
    refresh_token: String,
}

#[async_trait]
impl TokenRefresher for ProtonTokenRefresher {
    async fn refresh(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        ctx.ensure_active()?;
        let _serial = self.serial.lock().await;
        ctx.ensure_active()?;

        let envelope = self.secrets.load(&ctx.account)?.ok_or_else(|| {
            ProviderError::reauthenticate("there is no stored Proton session; sign in again")
        })?;
        let secret = proton_secret(envelope)?;
        let uid = secret.uid.clone().ok_or_else(|| {
            ProviderError::reauthenticate("the Proton session id is missing; sign in again")
        })?;
        let refresh_token = secret.refresh_token.clone().ok_or_else(|| {
            ProviderError::reauthenticate("the Proton session has expired; sign in again")
        })?;

        use proton_core::transport::{Doer, HttpClient, Request};
        let http = HttpClient::new(self.endpoints.as_str(), PROTON_APP_VERSION);
        let response: RefreshResponse = http
            .decode(
                Request::post("/auth/v4/refresh")
                    .json(serde_json::json!({
                        "UID": uid,
                        "RefreshToken": refresh_token.expose_secret(),
                        "ResponseType": "token",
                        "GrantType": "refresh_token",
                        "RedirectURI": "https://protonmail.ch",
                        "State": "0",
                    }))
                    .no_refresh()
                    .omit_auth(),
            )
            .await
            .map_err(map_proton_error)?;

        // A removal/invalidation that landed while the network waited must not
        // be able to persist a rotated token.
        ctx.ensure_active()?;

        let rotated = SecretEnvelope::proton(ProtonSessionSecret {
            uid: Some(uid),
            access_token: Some(SecretString::from(response.access_token.clone())),
            refresh_token: Some(SecretString::from(response.refresh_token)),
            skp: secret.skp,
        });
        // Persist first; if the write is fenced, the caller gets an error and
        // never receives a token derived from a secret this account no longer
        // owns.
        self.secrets.save(ctx, &rotated)?;
        Ok(SecretString::from(response.access_token))
    }
}

impl std::fmt::Debug for ProtonTokenRefresher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProtonTokenRefresher")
            .field("endpoints", &self.endpoints)
            .field("secrets", &"<protected credential repository>")
            .finish()
    }
}
