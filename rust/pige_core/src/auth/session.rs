//! Generation-safe, cancellation-aware account sessions and single-flight
//! token refresh.
//!
//! [`AccountSessionManager::context`] hands out an [`OperationContext`] bound to
//! the account's durable generation and to a shared cancellation token.
//! [`AccountSessionManager::invalidate`] cancels every live context for that
//! account and durably increments the generation, so a network call or a
//! refresh that started earlier can neither continue nor persist its result.
//! The durable generation and cross-process lease live in the SQLite store, so
//! the fence survives a restart and holds across processes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use secrecy::SecretString;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::account::AccountId;
use crate::providers::{OperationContext, ProviderError};
use crate::store::{AccountLease, Store};

use super::secrets::store_error_to_provider;

/// Per-account in-process lease state: the durable generation and the token
/// every live context descends from.
struct SessionState {
    generation: u64,
    cancellation: CancellationToken,
}

/// Owns the in-process half of the account gate and the durable operations on
/// top of it.
pub struct AccountSessionManager {
    store: Arc<Store>,
    /// Identifies this process for the durable lease/fencing token.
    holder: String,
    sessions: Mutex<HashMap<AccountId, SessionState>>,
}

impl AccountSessionManager {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            holder: format!("pige-{}", Uuid::new_v4()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, SessionState>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// This process's lease holder name.
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// The account's current durable generation.
    pub fn generation(&self, account: &AccountId) -> Result<u64, ProviderError> {
        self.store
            .generation(account)
            .map_err(store_error_to_provider)
    }

    /// Build a context bound to the account's durable generation and to the
    /// shared cancellation token.
    ///
    /// If another process (or a restart) has advanced the durable generation,
    /// the in-process token is rotated here so every older context is cancelled
    /// before this one is handed out.
    pub fn context(
        &self,
        account: &AccountId,
        timeout: Duration,
    ) -> Result<OperationContext, ProviderError> {
        let durable = self.generation(account)?;
        let mut sessions = self.sessions();
        let state = sessions.entry(account.clone()).or_insert_with(|| SessionState {
            generation: durable,
            cancellation: CancellationToken::new(),
        });
        if state.generation != durable {
            state.cancellation.cancel();
            state.cancellation = CancellationToken::new();
            state.generation = durable;
        }
        let parent = state.cancellation.clone();
        drop(sessions);

        Ok(OperationContext {
            account: account.clone(),
            generation: durable,
            cancellation: parent.child_token(),
            deadline: Instant::now() + timeout,
        })
    }

    /// Cancel every live context for `account` and durably increment its
    /// generation. Called when an account is removed or re-authenticated.
    pub fn invalidate(&self, account: &AccountId) -> Result<(), ProviderError> {
        let generation = self
            .store
            .bump_generation(account)
            .map_err(store_error_to_provider)?;
        let mut sessions = self.sessions();
        let state = sessions.entry(account.clone()).or_insert_with(|| SessionState {
            generation,
            cancellation: CancellationToken::new(),
        });
        state.cancellation.cancel();
        state.cancellation = CancellationToken::new();
        state.generation = generation;
        Ok(())
    }

    /// Take the account's cross-process lease.
    pub fn acquire_lease(
        &self,
        account: &AccountId,
        ttl: Duration,
    ) -> Result<AccountLease, ProviderError> {
        self.store
            .acquire_lease(account, &self.holder, ttl)
            .map_err(store_error_to_provider)
    }

    /// Extend a held lease.
    pub fn renew_lease(
        &self,
        lease: &AccountLease,
        ttl: Duration,
    ) -> Result<AccountLease, ProviderError> {
        self.store
            .renew_lease(lease, ttl)
            .map_err(store_error_to_provider)
    }

    /// Release a held lease.
    pub fn release_lease(&self, lease: &AccountLease) -> Result<(), ProviderError> {
        self.store.release_lease(lease).map_err(store_error_to_provider)
    }

    /// Verify a lease is still ours before a durable write.
    pub fn ensure_lease(&self, lease: &AccountLease) -> Result<(), ProviderError> {
        self.store.ensure_lease(lease).map_err(store_error_to_provider)
    }
}

/// A source of a provider access token.
///
/// `access_token` may return a cached value or refresh it, but it must be
/// cancellation- and generation-aware: a context that was invalidated must yield
/// no token.
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn access_token(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError>;
}

/// Performs one token refresh. Implemented by an adapter (or the fake) so the
/// single-flight cache stays provider-neutral.
#[async_trait]
pub trait TokenRefresher: Send + Sync {
    async fn refresh(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError>;
}

struct CachedToken {
    token: SecretString,
    expires_at: Instant,
}

/// A [`TokenSource`] that refreshes at most once for concurrent callers.
///
/// The cache lock is held across the refresh await, so the first caller performs
/// the refresh and every caller that arrived meanwhile observes the fresh token
/// instead of starting a second refresh.
pub struct CachedTokenSource<R: TokenRefresher> {
    refresher: Arc<R>,
    cache: Arc<tokio::sync::Mutex<Option<CachedToken>>>,
    ttl: Duration,
}

impl<R: TokenRefresher> Clone for CachedTokenSource<R> {
    fn clone(&self) -> Self {
        Self {
            refresher: self.refresher.clone(),
            cache: self.cache.clone(),
            ttl: self.ttl,
        }
    }
}

impl<R: TokenRefresher> CachedTokenSource<R> {
    pub fn new(refresher: Arc<R>, ttl: Duration) -> Self {
        Self {
            refresher,
            cache: Arc::new(tokio::sync::Mutex::new(None)),
            ttl,
        }
    }
}

#[async_trait]
impl<R: TokenRefresher> TokenSource for CachedTokenSource<R> {
    async fn access_token(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        ctx.ensure_active()?;

        let mut cache = self.cache.lock().await;
        if let Some(cached) = cache.as_ref()
            && Instant::now() < cached.expires_at
        {
            return Ok(cached.token.clone());
        }

        let token = self.refresher.refresh(ctx).await?;
        *cache = Some(CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + self.ttl,
        });
        Ok(token)
    }
}
