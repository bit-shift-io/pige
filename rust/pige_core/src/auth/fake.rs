//! A scripted, in-memory authentication service and token refresher.
//!
//! Nothing here talks to a provider. The fake drives the exact lifecycle the
//! real adapters (G1/P1) must implement: begin stages a candidate secret and a
//! pending-auth reference, respond completes or rejects, cancel and expiry clean
//! the staged entry up, and a late answer is refused.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use secrecy::SecretString;
use uuid::Uuid;

use crate::account::{Account, AccountId, MailBackend, Security, ServerConfig};
use crate::providers::{
    AuthAttemptId, GMAIL_PROVIDER, OperationContext, PROTON_PROVIDER, ProviderError, ProviderId,
};
use crate::store::{PendingAuthRecord, Store};

use super::secrets::{SecretEnvelope, SecretRepository, StagedSecretRef};
use super::session::{AccountSessionManager, TokenRefresher};
use super::types::{
    AuthAnswer, AuthChallenge, AuthProgress, AuthRequest, AuthenticatedAccount, SecretPersistence,
};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// What one scripted attempt will do.
pub struct FakeAuthScript {
    challenge: AuthChallenge,
    persistence: SecretPersistence,
    secret: SecretEnvelope,
    ttl: Duration,
    provider: ProviderId,
    fail_begin: Option<ProviderError>,
    answer_delay: Option<Duration>,
}

impl FakeAuthScript {
    /// A single-step password challenge that stages a password secret.
    pub fn password(secret: impl Into<String>) -> Self {
        Self {
            challenge: AuthChallenge::Password { username: None },
            persistence: SecretPersistence::Persistent,
            secret: SecretEnvelope::password(secret),
            ttl: Duration::from_secs(300),
            provider: ProviderId::from(crate::providers::IMAP_SMTP_PROVIDER),
            fail_begin: None,
            answer_delay: None,
        }
    }

    #[must_use]
    pub fn challenge(mut self, challenge: AuthChallenge) -> Self {
        self.challenge = challenge;
        self
    }

    #[must_use]
    pub fn persistence(mut self, persistence: SecretPersistence) -> Self {
        self.persistence = persistence;
        self
    }

    #[must_use]
    pub fn secret(mut self, secret: SecretEnvelope) -> Self {
        self.secret = secret;
        self
    }

    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    #[must_use]
    pub fn provider(mut self, provider: ProviderId) -> Self {
        self.provider = provider;
        self
    }

    #[must_use]
    pub fn fail_begin(mut self, error: ProviderError) -> Self {
        self.fail_begin = Some(error);
        self
    }

    #[must_use]
    pub fn answer_delay(mut self, delay: Duration) -> Self {
        self.answer_delay = Some(delay);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptState {
    Pending,
    Resolved,
    Cancelled,
}

struct Attempt {
    state: AttemptState,
    account: Account,
    staged: StagedSecretRef,
    expires_at_ms: i64,
}

/// A scripted [`AuthenticationService`](super::AuthenticationService).
pub struct FakeAuthService {
    secrets: Arc<dyn SecretRepository>,
    sessions: Arc<AccountSessionManager>,
    store: Arc<Store>,
    script: FakeAuthScript,
    attempts: HashMap<AuthAttemptId, Attempt>,
}

impl FakeAuthService {
    pub fn new(
        secrets: Arc<dyn SecretRepository>,
        sessions: Arc<AccountSessionManager>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            secrets,
            sessions,
            store,
            script: FakeAuthScript::password(""),
            attempts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn script(mut self, script: FakeAuthScript) -> Self {
        self.script = script;
        self
    }

    fn provisional_account(&self, id: AccountId, provider: &ProviderId) -> Account {
        let backend = match provider.as_str() {
            GMAIL_PROVIDER => MailBackend::gmail("provisional"),
            PROTON_PROVIDER => MailBackend::proton(None),
            _ => MailBackend::imap_smtp(
                "provisional",
                ServerConfig {
                    host: String::new(),
                    port: 0,
                    security: Security::Tls,
                },
                ServerConfig {
                    host: String::new(),
                    port: 0,
                    security: Security::Tls,
                },
            ),
        };
        Account {
            id,
            label: "Provisional".to_string(),
            email: String::new(),
            display_name: String::new(),
            backend,
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        }
    }

    fn discard_attempt(
        &mut self,
        attempt: &AuthAttemptId,
        staged: &StagedSecretRef,
    ) -> Result<(), ProviderError> {
        self.secrets.discard_staged(staged)?;
        self.store
            .remove_pending_auth(attempt)
            .map_err(super::secrets::store_error_to_provider)?;
        if let Some(state) = self.attempts.get_mut(attempt) {
            state.state = AttemptState::Cancelled;
        }
        Ok(())
    }

    /// Sweep every staged secret that can no longer be committed: expired
    /// pending attempts, terminal (resolved/cancelled) attempts whose commit
    /// never happened, and pending rows left by another process or a restart.
    /// Returns the attempts that were reaped.
    ///
    /// This is called at the start of every [`AuthenticationService::begin`], so
    /// a completed-but-uncommitted staged entry cannot leak.
    pub fn reap_expired(&mut self) -> Result<Vec<AuthAttemptId>, ProviderError> {
        let now = now_ms();
        let mut reaped = Vec::new();

        // Collect first so the map can be mutated while sweeping.
        let terminal: Vec<(AuthAttemptId, StagedSecretRef)> = self
            .attempts
            .iter()
            .filter(|(_, state)| state.state != AttemptState::Pending)
            .map(|(attempt, state)| (attempt.clone(), state.staged.clone()))
            .collect();
        let expired: Vec<(AuthAttemptId, StagedSecretRef)> = self
            .attempts
            .iter()
            .filter(|(_, state)| state.state == AttemptState::Pending && now >= state.expires_at_ms)
            .map(|(attempt, state)| (attempt.clone(), state.staged.clone()))
            .collect();

        for (attempt, staged) in &expired {
            self.discard_attempt(attempt, staged)?;
            reaped.push(attempt.clone());
        }
        for (attempt, staged) in &terminal {
            self.secrets.discard_staged(staged)?;
            self.store
                .remove_pending_auth(attempt)
                .map_err(super::secrets::store_error_to_provider)?;
            self.attempts.remove(attempt);
            reaped.push(attempt.clone());
        }

        // Pending rows written by another process or a previous run, whose
        // in-memory attempt state this service never held.
        for record in self
            .store
            .expired_pending_auth(now)
            .map_err(super::secrets::store_error_to_provider)?
        {
            reaped.push(record.attempt);
        }
        super::secrets::sweep_expired_pending_auth(&self.store, self.secrets.as_ref())?;

        Ok(reaped)
    }
}

#[async_trait]
impl super::AuthenticationService for FakeAuthService {
    async fn begin(&mut self, request: AuthRequest) -> Result<AuthProgress, ProviderError> {
        if let Some(error) = &self.script.fail_begin {
            return Err(error.clone());
        }
        // Sweep expired and uncommitted staged secrets before starting a new
        // attempt, so a leaked staged entry is reclaimed.
        self.reap_expired()?;
        let attempt = AuthAttemptId::new(Uuid::new_v4().to_string());
        let account_id = request.account.clone().unwrap_or_default();
        let account = self.provisional_account(account_id, &request.provider);

        // Reserve a provisional generation without creating a visible account.
        let ctx = self.sessions.context(&account.id, self.script.ttl)?;
        let staged = self.secrets.stage(&attempt, &self.script.secret)?;
        let now = now_ms();
        if let Err(error) = self.store.insert_pending_auth(&PendingAuthRecord {
            attempt: attempt.clone(),
            account: Some(account.id.clone()),
            provider: self.script.provider.clone(),
            credential_ref: staged.as_str().to_string(),
            generation: ctx.generation,
            created_at_ms: now,
            expires_at_ms: now.saturating_add(self.script.ttl.as_millis() as i64),
        }) {
            // The staged secret was written first; discard it so a failed
            // pending-row insert cannot leak it.
            let _ = self.secrets.discard_staged(&staged);
            return Err(super::secrets::store_error_to_provider(error));
        }
        self.attempts.insert(
            attempt.clone(),
            Attempt {
                state: AttemptState::Pending,
                account,
                staged,
                expires_at_ms: now.saturating_add(self.script.ttl.as_millis() as i64),
            },
        );

        Ok(AuthProgress::Challenge {
            attempt,
            challenge: self.script.challenge.clone(),
        })
    }

    async fn respond(
        &mut self,
        attempt: &AuthAttemptId,
        _answer: AuthAnswer,
    ) -> Result<AuthProgress, ProviderError> {
        if let Some(delay) = self.script.answer_delay {
            tokio::time::sleep(delay).await;
        }
        let Some(state) = self.attempts.get(attempt) else {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        };
        if state.state != AttemptState::Pending {
            return Err(ProviderError::not_found(
                "the authentication attempt is no longer active",
            ));
        }
        let account = state.account.clone();
        let staged = state.staged.clone();
        let expires_at = state.expires_at_ms;
        if now_ms() >= expires_at {
            self.discard_attempt(attempt, &staged)?;
            return Err(ProviderError::not_found(
                "the authentication attempt has expired",
            ));
        }

        if let Some(state) = self.attempts.get_mut(attempt) {
            state.state = AttemptState::Resolved;
        }
        Ok(AuthProgress::Complete(AuthenticatedAccount {
            account,
            persistence: self.script.persistence,
            credential_ref: staged,
        }))
    }

    async fn cancel(&mut self, attempt: &AuthAttemptId) -> Result<(), ProviderError> {
        let Some(state) = self.attempts.get(attempt) else {
            return Ok(());
        };
        if state.state == AttemptState::Pending {
            let staged = state.staged.clone();
            self.discard_attempt(attempt, &staged)?;
        }
        Ok(())
    }

    async fn resume(&mut self, account: &AccountId) -> Result<AuthProgress, ProviderError> {
        match self.secrets.load(account)? {
            Some(_) => Ok(AuthProgress::Complete(AuthenticatedAccount {
                account: self.provisional_account(account.clone(), &self.script.provider),
                persistence: SecretPersistence::Persistent,
                credential_ref: StagedSecretRef::for_account(account)?,
            })),
            None => Err(ProviderError::reauthenticate(
                "there is no stored session for this account",
            )),
        }
    }

    async fn revoke(&mut self, account: &AccountId) -> Result<(), ProviderError> {
        let to_discard: Vec<(AuthAttemptId, StagedSecretRef)> = self
            .attempts
            .iter()
            .filter(|(_, state)| state.account.id == *account && state.state == AttemptState::Pending)
            .map(|(attempt, state)| (attempt.clone(), state.staged.clone()))
            .collect();
        for (attempt, staged) in to_discard {
            self.discard_attempt(&attempt, &staged)?;
        }
        self.secrets.delete(account)?;
        Ok(())
    }
}

/// A counting [`TokenRefresher`](super::TokenRefresher) for single-flight tests.
pub struct FakeTokenRefresher {
    token: SecretString,
    calls: AtomicUsize,
    delay: Duration,
    fail: Option<ProviderError>,
}

impl FakeTokenRefresher {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: SecretString::from(token.into()),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: None,
        }
    }

    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    #[must_use]
    pub fn fail(mut self, error: ProviderError) -> Self {
        self.fail = Some(error);
        self
    }

    /// How many times an actual refresh ran.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenRefresher for FakeTokenRefresher {
    async fn refresh(&self, ctx: &OperationContext) -> Result<SecretString, ProviderError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        ctx.ensure_active()?;
        if let Some(error) = &self.fail {
            return Err(error.clone());
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.token.clone())
    }
}
