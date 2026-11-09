//! Native Proton Mail adapter.
//!
//! P1 owns authentication, token rotation, and protected key state. Mailbox,
//! sync, contacts, and submission services arrive with P2-P4; the factory below
//! already shares one per-account token source and key store so those services
//! cannot open independent, divergent sessions.
//!
//! Nothing in this module owns persistence: the F3 `SecretRepository` stores the
//! session envelope, and the vendored library's file/keyring `session` module is
//! intentionally unused.

pub mod auth;
pub mod client;
pub mod keys;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::account::{Account, AccountId, MailBackend};
use crate::auth::{
    AccountSessionManager, AuthenticationService, CachedTokenSource, SecretRepository, TokenSource,
};
use crate::providers::{
    OperationContext, PROTON_PROVIDER, ProviderCapabilities, ProviderError, ProviderFactory,
    ProviderId, ProviderRegistry, ProviderSession,
};

pub use auth::ProtonAuthentication;
pub use client::{PROTON_APP_VERSION, ProtonClient, ProtonEndpoints};
pub use keys::{ProtonAddressKeys, ProtonKeyStores, UnlockedKeyStore};

// Re-exported for adapter code and synthetic tests. These are still private to
// `providers::proton` from the application's point of view.
pub use proton_core;
pub use proton_crypto;
pub use proton_srp;

/// The per-account token source builder used by the factory.
pub struct ProtonProviderFactory {
    sessions: Arc<AccountSessionManager>,
    secrets: Arc<dyn SecretRepository>,
    endpoints: ProtonEndpoints,
    keys: Arc<ProtonKeyStores>,
    token_sources: Mutex<HashMap<AccountId, Arc<dyn TokenSource>>>,
}

impl ProtonProviderFactory {
    pub fn new(
        sessions: Arc<AccountSessionManager>,
        secrets: Arc<dyn SecretRepository>,
        endpoints: ProtonEndpoints,
        keys: Arc<ProtonKeyStores>,
    ) -> Self {
        Self {
            sessions,
            secrets,
            endpoints,
            keys,
            token_sources: Mutex::new(HashMap::new()),
        }
    }

    pub fn sessions(&self) -> &Arc<AccountSessionManager> {
        &self.sessions
    }

    pub fn endpoints(&self) -> &ProtonEndpoints {
        &self.endpoints
    }

    pub fn key_stores(&self) -> &Arc<ProtonKeyStores> {
        &self.keys
    }

    /// The shared token source for one account. Repeated calls return the same
    /// single-flight cache, so concurrent Proton services perform one refresh.
    pub fn token_source(&self, account: &AccountId) -> Arc<dyn TokenSource> {
        let mut cached = self
            .token_sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(source) = cached.get(account) {
            return source.clone();
        }
        let refresher = Arc::new(crate::auth::proton::ProtonTokenRefresher::new(
            self.secrets.clone(),
            self.endpoints.clone(),
        ));
        let source: Arc<dyn TokenSource> =
            Arc::new(CachedTokenSource::new(refresher, Duration::from_secs(60)));
        cached.insert(account.clone(), source.clone());
        source
    }

    /// Build an authenticated client sharing this account's token and key state.
    pub fn client(&self, account: &AccountId) -> ProtonClient {
        ProtonClient::new(
            self.token_source(account),
            self.keys.get_or_create(account),
            self.endpoints.clone(),
        )
    }
}

impl fmt::Debug for ProtonProviderFactory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtonProviderFactory")
            .field("endpoints", &self.endpoints)
            .field("keys", &self.keys)
            .finish()
    }
}

#[async_trait]
impl ProviderFactory for ProtonProviderFactory {
    fn supports(&self, id: &ProviderId) -> bool {
        id.as_str() == PROTON_PROVIDER
    }

    fn registered(&self) -> Vec<ProviderId> {
        vec![ProviderId::from(PROTON_PROVIDER)]
    }

    async fn open(
        &self,
        account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        ctx.ensure_active()?;
        if !matches!(account.backend, MailBackend::Proton { .. }) {
            return Err(ProviderError::unsupported("Proton for this account"));
        }
        // Build the shared client eagerly so every capability opened for this
        // account uses the same token source and key store. P2-P4 attach their
        // services to this session; P1 advertises no capability it cannot serve.
        let _client = self.client(&account.id);
        Ok(ProviderSession::new(ProviderCapabilities::default())
            .with_provider(ProviderId::from(PROTON_PROVIDER)))
    }
}

/// Register the Proton factory in an application-owned registry.
pub fn register_proton(
    registry: &mut ProviderRegistry,
    factory: ProtonProviderFactory,
) -> Result<(), ProviderError> {
    registry.register(ProviderId::from(PROTON_PROVIDER), Arc::new(factory))
}

/// Register the Proton factory with a standalone registry.
pub fn proton_registry(factory: ProtonProviderFactory) -> Result<ProviderRegistry, ProviderError> {
    let mut registry = ProviderRegistry::new();
    register_proton(&mut registry, factory)?;
    Ok(registry)
}

/// The core-level Proton registration: provider dispatch plus the interactive
/// authentication service that produces its credentials.
///
/// P5 still owns the application shell: [`crate::api::Config::auth`] is a single
/// provider slot, so composing Proton with Google/IMAP onboarding and exposing
/// the account UI is a shell decision. This type is the wiring the shell will
/// consume, not a second UI path.
pub struct ProtonRegistration {
    pub registry: ProviderRegistry,
    pub authentication: Arc<AsyncMutex<Box<dyn AuthenticationService>>>,
}

/// Build the registered Proton provider and authentication service together.
pub fn proton_registration(
    factory: ProtonProviderFactory,
    authentication: ProtonAuthentication,
) -> Result<ProtonRegistration, ProviderError> {
    Ok(ProtonRegistration {
        registry: proton_registry(factory)?,
        authentication: proton_auth_service(authentication),
    })
}

/// Wrap a configured Proton authentication service for [`crate::api::Config`].
pub fn proton_auth_service(
    authentication: ProtonAuthentication,
) -> Arc<AsyncMutex<Box<dyn AuthenticationService>>> {
    Arc::new(AsyncMutex::new(Box::new(authentication)))
}
