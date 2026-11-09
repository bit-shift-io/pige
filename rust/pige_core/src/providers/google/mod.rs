//! Native Google mail and People adapters.
//!
//! G1 owns authorization, token ownership, endpoint injection, and the
//! application-facing factory. G2/G3 add the Gmail and People service
//! implementations on top of [`GoogleClient`]; nothing here exposes a Google SDK
//! type or token outside the module.

pub mod android;
pub mod auth;
pub mod client;
pub mod desktop;
mod mailbox;
mod mutations;
mod normalize;
mod people;
mod submission;
mod sync;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::Mutex as AsyncMutex;

use crate::account::{Account, AccountId, MailBackend};
use crate::auth::{AccountSessionManager, AuthenticationService, TokenSource};
use crate::providers::{
    ContactCapabilities, GMAIL_PROVIDER, MutationCapabilities, OperationContext,
    ProviderCapabilities, ProviderError, ProviderFactory, ProviderId, ProviderRegistry,
    ProviderSession,
};

pub use mailbox::GmailMailbox;
pub use mutations::GmailMutations;
pub use people::{GOOGLE_OTHER_CONTACTS_SOURCE, GOOGLE_SAVED_CONTACTS_SOURCE, GoogleAddressBook};
pub use submission::GmailSubmission;
pub use sync::{GmailSync, cursor_stage, request_collection};

#[cfg(target_os = "android")]
pub use android::AndroidPlatformHost;
pub use android::{
    AndroidAuthorizationRequest, AndroidOfflineExchange, GoogleAndroidHost, GoogleAndroidResult,
    GoogleAuthorizationOutcome, GoogleAuthorizationResult, GoogleIdentity, deliver_native_result,
    take_native_result,
};
pub use auth::{
    CONTACTS_OTHER_READONLY_SCOPE, CONTACTS_READONLY_SCOPE, GoogleAuthentication,
    GoogleOAuthConfig, GoogleTokenClient, GoogleTokenRefresher, mail_scopes, requested_scopes,
};
pub use client::{GoogleClient, GoogleEndpoints};
pub use desktop::{
    DesktopRedirect, DesktopRedirectFlow, DesktopRedirectReceiver, FixedRedirect, LoopbackRedirect,
};

/// S2 §9.1: the only requested mail scope. Permanent delete is not available
/// under it (G2 advertises `permanent_delete = false`).
pub const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

/// Resolves the per-account shared token source for the Google adapter.
pub trait GoogleTokenSourceProvider: Send + Sync {
    fn token_source(&self, account: &AccountId) -> Result<Arc<dyn TokenSource>, ProviderError>;
}

impl GoogleTokenSourceProvider for Arc<GoogleAuthentication> {
    fn token_source(&self, account: &AccountId) -> Result<Arc<dyn TokenSource>, ProviderError> {
        Ok(self.provider_token_source(account))
    }
}

struct UnavailableTokenSources;

impl GoogleTokenSourceProvider for UnavailableTokenSources {
    fn token_source(&self, _account: &AccountId) -> Result<Arc<dyn TokenSource>, ProviderError> {
        Err(ProviderError::unsupported(
            "Google authentication is not configured in this build",
        ))
    }
}

/// Opens native Gmail sessions. One token source is cached per account so the
/// Gmail and People services opened for that account share authenticated state.
pub struct GoogleProviderFactory {
    auth: Arc<AccountSessionManager>,
    endpoints: GoogleEndpoints,
    token_sources: Arc<dyn GoogleTokenSourceProvider>,
    cached: Mutex<HashMap<AccountId, Arc<dyn TokenSource>>>,
}

impl GoogleProviderFactory {
    /// The brief's construction point. A shell that owns a
    /// [`GoogleAuthentication`] additionally calls
    /// [`Self::with_token_source_provider`]; without one, opening an account is a
    /// typed `Unsupported` error rather than an unauthenticated request.
    pub fn new(auth: Arc<AccountSessionManager>, endpoints: GoogleEndpoints) -> Self {
        Self {
            auth,
            endpoints,
            token_sources: Arc::new(UnavailableTokenSources),
            cached: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_authentication(self, authentication: Arc<GoogleAuthentication>) -> Self {
        self.with_token_source_provider(Arc::new(authentication))
    }

    /// The account session manager used for generations and cancellation. G2/G3
    /// services take their contexts from the caller; this accessor keeps the
    /// registration point explicit.
    pub fn sessions(&self) -> &Arc<AccountSessionManager> {
        &self.auth
    }

    pub fn with_token_source_provider(
        mut self,
        provider: Arc<dyn GoogleTokenSourceProvider>,
    ) -> Self {
        self.token_sources = provider;
        self
    }

    /// The shared token source for one account. Repeated calls and repeated
    /// session opens return the same cache, so a concurrent Gmail and People
    /// fetch performs one refresh.
    pub fn token_source(&self, account: &AccountId) -> Result<Arc<dyn TokenSource>, ProviderError> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(source) = cached.get(account) {
            return Ok(source.clone());
        }
        let source = self.token_sources.token_source(account)?;
        cached.insert(account.clone(), source.clone());
        Ok(source)
    }

    /// Build an authenticated client. G2/G3 use this from `open`.
    pub fn client(&self, account: &AccountId) -> Result<GoogleClient, ProviderError> {
        Ok(GoogleClient::new(
            self.token_source(account)?,
            self.endpoints.clone(),
        ))
    }
}

impl std::fmt::Debug for GoogleProviderFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleProviderFactory")
            .field("endpoints", &self.endpoints)
            .finish()
    }
}

#[async_trait]
impl ProviderFactory for GoogleProviderFactory {
    fn supports(&self, id: &ProviderId) -> bool {
        id.as_str() == GMAIL_PROVIDER
    }

    async fn open(
        &self,
        account: &Account,
        ctx: &OperationContext,
    ) -> Result<ProviderSession, ProviderError> {
        ctx.ensure_active()?;
        if !matches!(account.backend, MailBackend::Gmail { .. }) {
            return Err(ProviderError::unsupported("Gmail for this account"));
        }
        // Build the client now so every capability of this session shares the
        // same per-account token source. This does not prove a credential
        // exists; the first authenticated call reports reauthentication.
        let client = self.client(&account.id)?;
        // Gmail accepts `me` for the authenticated user; the account's Google
        // user id is not the Gmail resource id.
        let user = "me";

        let mut session = ProviderSession::new(ProviderCapabilities {
            mailbox: true,
            sync: true,
            mutations: MutationCapabilities {
                read: true,
                star: true,
                labels: true,
                move_messages: true,
                archive: true,
                trash: true,
                // v1 holds gmail.modify, which cannot permanently delete.
                permanent_delete: false,
            },
            // Gmail owns the Sent copy. Contacts are read-only in v1: People
            // lists are full snapshots with deletion metadata, and compose
            // remains local.
            submission: true,
            contacts: ContactCapabilities {
                read: true,
                incremental_sync: true,
                remote_search: false,
                write: false,
                groups: false,
            },
            server_search: false,
        })
        .with_provider(ProviderId::from(GMAIL_PROVIDER));

        session.mailbox = Some(Box::new(GmailMailbox::new(client.clone(), user)));
        session.sync = Some(Box::new(GmailSync::new(client.clone(), user)));
        session.address_book = Some(Box::new(GoogleAddressBook::new(client.clone(), user)));
        session.mutations = Some(Box::new(GmailMutations::new(client.clone(), user)));
        session.submission = Some(Box::new(GmailSubmission::new(client, account.clone())));
        Ok(session)
    }
}

/// Register the Gmail adapter in an application-owned registry.
pub fn register_google(
    registry: &mut ProviderRegistry,
    auth: Arc<AccountSessionManager>,
    endpoints: GoogleEndpoints,
    authentication: Arc<GoogleAuthentication>,
) -> Result<(), ProviderError> {
    registry.register(
        ProviderId::from(GMAIL_PROVIDER),
        Arc::new(GoogleProviderFactory::new(auth, endpoints).with_authentication(authentication)),
    )
}

/// A standalone registry containing only Gmail. The application shell composes
/// this with its IMAP registry through [`ProviderRegistry::merge`].
pub fn google_registry(
    auth: Arc<AccountSessionManager>,
    endpoints: GoogleEndpoints,
    authentication: Arc<GoogleAuthentication>,
) -> Result<ProviderRegistry, ProviderError> {
    let mut registry = ProviderRegistry::new();
    register_google(&mut registry, auth, endpoints, authentication)?;
    Ok(registry)
}

/// Wrap a configured Google authentication service for [`crate::api::Config`].
pub fn google_auth_service(
    authentication: GoogleAuthentication,
) -> Arc<AsyncMutex<Box<dyn AuthenticationService>>> {
    Arc::new(AsyncMutex::new(Box::new(authentication)))
}
