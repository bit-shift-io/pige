//! Compose several interactive authentication services behind one
//! [`AuthenticationService`] object.
//!
//! [`crate::api::Config::auth`] intentionally remained a single provider slot
//! while native Google landed. Proton adds a second native provider, so the
//! shell now installs this router: `begin` dispatches by provider,
//! challenge/attempt calls return to the service that created the attempt, and
//! `resume`/`revoke` are offered to each provider. A provider's `resume` returns
//! `Reauthenticate`/`Unsupported` for an account it does not own; `revoke` is
//! ownership-safe and no-ops for a foreign credential.
//!
//! No secret, attempt token, or provider payload is exposed by this module.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::account::AccountId;
use crate::providers::{AuthAttemptId, ErrorKind, ProviderError, ProviderId};

use super::types::{AuthAnswer, AuthProgress, AuthRequest, AuthenticationService};

/// The shared handle shape [`crate::api::Config`] already uses for one provider.
pub type SharedAuthenticationService = Arc<AsyncMutex<Box<dyn AuthenticationService>>>;

struct ProviderEntry {
    id: ProviderId,
    service: SharedAuthenticationService,
}

/// One interactive service over several native providers.
pub struct AuthRouter {
    providers: Vec<ProviderEntry>,
    attempts: Mutex<HashMap<AuthAttemptId, ProviderId>>,
}

impl AuthRouter {
    /// Build a router. Duplicate provider ids are rejected so a request can
    /// never be routed to an arbitrary replacement.
    pub fn new(
        providers: Vec<(ProviderId, SharedAuthenticationService)>,
    ) -> Result<Self, ProviderError> {
        let mut entries = Vec::with_capacity(providers.len());
        for (id, service) in providers {
            if entries.iter().any(|entry: &ProviderEntry| entry.id == id) {
                return Err(ProviderError::conflict(format!(
                    "authentication provider `{id}` is already registered"
                )));
            }
            entries.push(ProviderEntry { id, service });
        }
        Ok(Self {
            providers: entries,
            attempts: Mutex::new(HashMap::new()),
        })
    }

    fn provider(&self, id: &ProviderId) -> Result<&SharedAuthenticationService, ProviderError> {
        self.providers
            .iter()
            .find(|entry| &entry.id == id)
            .map(|entry| &entry.service)
            .ok_or_else(|| ProviderError::unsupported(format!("interactive sign-in for `{id}`")))
    }

    fn remember(&self, attempt: &AuthAttemptId, provider: &ProviderId) {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(attempt.clone(), provider.clone());
    }

    fn provider_for_attempt(&self, attempt: &AuthAttemptId) -> Option<SharedAuthenticationService> {
        let provider = self
            .attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(attempt)
            .cloned()?;
        self.providers
            .iter()
            .find(|entry| entry.id == provider)
            .map(|entry| entry.service.clone())
    }

    fn forget(&self, attempt: &AuthAttemptId) {
        self.attempts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(attempt);
    }

    fn record_progress(&self, provider: &ProviderId, progress: &AuthProgress) {
        if let AuthProgress::Challenge { attempt, .. } = progress {
            self.remember(attempt, provider);
        }
    }
}

#[async_trait]
impl AuthenticationService for AuthRouter {
    async fn begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError> {
        let provider = request.provider.clone();
        let service = self.provider(&provider)?.clone();
        let progress = service.lock().await.begin(request).await?;
        self.record_progress(&provider, &progress);
        Ok(progress)
    }

    async fn begin_with_username(
        &mut self,
        provider: ProviderId,
        username: &str,
        account: Option<AccountId>,
        enable_contacts: bool,
    ) -> Result<AuthProgress, ProviderError> {
        let service = self.provider(&provider)?.clone();
        let progress = service
            .lock()
            .await
            .begin_with_username(provider.clone(), username, account, enable_contacts)
            .await?;
        self.record_progress(&provider, &progress);
        Ok(progress)
    }

    async fn respond(
        &mut self,
        attempt: &AuthAttemptId,
        answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError> {
        let service = self.provider_for_attempt(attempt).ok_or_else(|| {
            ProviderError::not_found("the authentication attempt is no longer active")
        })?;
        // Keep the attempt mapping until cancel/commit: a late cancel is
        // harmless and a completed attempt still needs routing for a platform
        // callback.
        service.lock().await.respond(attempt, answer).await
    }

    async fn cancel(&mut self, attempt: &AuthAttemptId) -> Result<(), ProviderError> {
        let Some(service) = self.provider_for_attempt(attempt) else {
            return Ok(());
        };
        let result = service.lock().await.cancel(attempt).await;
        self.forget(attempt);
        result
    }

    async fn resume(&mut self, account: &AccountId) -> Result<AuthProgress, ProviderError> {
        for entry in &self.providers {
            match entry.service.lock().await.resume(account).await {
                Ok(progress) => return Ok(progress),
                Err(error)
                    if matches!(
                        error.kind,
                        ErrorKind::Reauthenticate | ErrorKind::Unsupported
                    ) =>
                {
                    // Not this provider's account (or it has no stored session).
                    // Try the next provider; the final message below is
                    // deliberately provider-neutral and actionable.
                }
                Err(error) => return Err(error),
            }
        }
        Err(ProviderError::reauthenticate(
            "this account needs a new sign-in; open Accounts and choose Reconnect",
        ))
    }

    async fn revoke(&mut self, account: &AccountId) -> Result<(), ProviderError> {
        let mut first_error = None;
        for entry in &self.providers {
            // Each provider's revoke is ownership-safe: a foreign or already
            // removed credential is a no-op, so trying every provider cannot
            // delete the wrong session.
            if let Err(error) = entry.service.lock().await.revoke(account).await
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn await_platform_completion(
        &mut self,
        attempt: &AuthAttemptId,
    ) -> Result<AuthProgress, ProviderError> {
        let service = self.provider_for_attempt(attempt).ok_or_else(|| {
            ProviderError::not_found("the authentication attempt is no longer active")
        })?;
        service
            .lock()
            .await
            .await_platform_completion(attempt)
            .await
    }

    fn has_platform_result(&self, attempt: &AuthAttemptId) -> bool {
        self.provider_for_attempt(attempt)
            .and_then(|service| {
                service
                    .try_lock()
                    .ok()
                    .map(|guard| guard.has_platform_result(attempt))
            })
            .unwrap_or(false)
    }

    async fn deliver_platform_result(
        &mut self,
        attempt: &AuthAttemptId,
    ) -> Result<Option<AuthProgress>, ProviderError> {
        let Some(service) = self.provider_for_attempt(attempt) else {
            return Ok(None);
        };
        service.lock().await.deliver_platform_result(attempt).await
    }
}

impl fmt::Debug for AuthRouter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthRouter")
            .field(
                "providers",
                &self
                    .providers
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>(),
            )
            .field(
                "attempts",
                &self.attempts.lock().map(|map| map.len()).unwrap_or(0),
            )
            .finish()
    }
}

/// A single provider does not need the router.
pub fn shared_service(service: Box<dyn AuthenticationService>) -> SharedAuthenticationService {
    Arc::new(AsyncMutex::new(service))
}

/// Small helper used by shells: compose zero, one, or several providers into
/// the one slot the API already has.
pub fn compose(
    providers: Vec<(ProviderId, SharedAuthenticationService)>,
) -> Result<Option<SharedAuthenticationService>, ProviderError> {
    match providers.len() {
        0 => Ok(None),
        1 => Ok(providers.into_iter().next().map(|(_, service)| service)),
        _ => Ok(Some(shared_service(Box::new(AuthRouter::new(providers)?)))),
    }
}
