//! Account lifecycle: guarded backend switching, the interactive-auth
//! wrappers, new-account commits, and crash recovery.
//!
//! The configuration document remains the source of truth for accounts, and
//! protected credentials remain in the platform credential store. A backend
//! switch therefore spans two durable stores. To make that pair recoverable, the
//! switch writes a **non-secret** [`SwitchIntent`] first, then:
//!
//! 1. copies the previous active credential to a lifecycle reference,
//! 2. records that the backup is confirmed (`BackedUp`),
//! 3. writes the new active credential and only then records `CredentialWritten`,
//! 4. publishes the new configuration and only then records `ConfigWritten`,
//! 5. clears only provider-derived cache/checkpoints (`Completed`), and
//! 6. removes the intent before discarding its temporary references.
//!
//! A crash before the target configuration is confirmed is rolled back at
//! startup; a crash after it completes forward, except for session-only
//! credentials (which cannot survive a restart) where the old backend and
//! credential are restored. The phase is a record of what was *confirmed*, never
//! of what was about to be attempted, so a phase at or above `CredentialWritten`
//! is the only evidence that the new credential is active.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use crate::account::{Account, AccountId};
use crate::auth::{
    AuthAnswer, AuthProgress, AuthRequest, AuthenticatedAccount, CredentialSecretRepository,
    SecretEnvelope, SecretPersistence, SecretRepository, StagedSecretRef,
};
use crate::config::{self, ConfigDocument};
use crate::platform::CredentialStore;
use crate::providers::{AuthAttemptId, OperationContext, ProviderError};
use crate::store::{Store, StoreError, SwitchIntent};

use super::mailbox::{INTERACTIVE_TIMEOUT, scheduler_message};
use super::{CoreEvent, ModelSnapshot, lock_state, not_initialized_message};

/// How long a lifecycle reference (rollback backup) is retained. It is removed
/// as soon as the switch completes; the constant only bounds a crashed writer.
const LIFECYCLE_REFERENCE_TTL: Duration = Duration::from_secs(30);

/// Confirmed switch progression. Every value is written only *after* the step it
/// names returned success. Recovery branches on these confirmations, never on
/// "the write was about to be attempted".
pub(crate) const PHASE_INTENT: u8 = 0;
/// The previous credential (when present) is copied to the rollback reference.
pub(crate) const PHASE_BACKED_UP: u8 = 1;
/// The new active credential write returned success.
pub(crate) const PHASE_CREDENTIAL_WRITTEN: u8 = 2;
/// The target account configuration was saved and mirrored.
pub(crate) const PHASE_CONFIG_WRITTEN: u8 = 3;
/// The provider cache/checkpoints were cleared; only reference cleanup remains.
pub(crate) const PHASE_COMPLETED: u8 = 4;

/// Test-only crash points. `1` simulates a process death immediately after the
/// new secret was activated; `2` immediately after the configuration was
/// published. Production never sets this.
static SWITCH_CRASH_POINT: AtomicU8 = AtomicU8::new(0);

#[doc(hidden)]
pub fn inject_switch_crash_after_secret() {
    SWITCH_CRASH_POINT.store(1, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn inject_switch_crash_after_config() {
    SWITCH_CRASH_POINT.store(2, Ordering::SeqCst);
}

#[doc(hidden)]
pub fn clear_switch_crash() {
    SWITCH_CRASH_POINT.store(0, Ordering::SeqCst);
}

fn simulated_crash(point: u8) -> Result<(), String> {
    if SWITCH_CRASH_POINT.load(Ordering::SeqCst) == point {
        Err("simulated backend switch crash".to_string())
    } else {
        Ok(())
    }
}

fn map_store(error: StoreError) -> String {
    format!("account lifecycle: {error}")
}

fn map_secret(error: ProviderError) -> String {
    format!("account credential: {}", error.safe_message)
}

fn switch_block_message(blockers: &[String]) -> String {
    format!(
        "Resolve {} pending or uncertain submission(s) before switching this account's backend",
        blockers.len()
    )
}

/// Refuse a backend switch while any submission for the account is not final.
///
/// `queued`, `preparing`, `submitting`, an accepted-but-unfinalized Sent copy,
/// and `uncertain` all block. Legacy `sending`/`failed` rows decode to
/// `uncertain`; `sent_local` decodes to `accepted`. A draft, a completed
/// delivery, or an evidenced rejection does not block.
pub fn can_switch_backend(account: &AccountId) -> Result<(), String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| not_initialized_message().to_string())?;
    state
        .config_doc
        .accounts
        .iter()
        .find(|candidate| &candidate.id == account)
        .ok_or_else(|| format!("unknown account {account}"))?;
    if state.store.has_switch_intent(account).map_err(map_store)? {
        return Err("a backend switch for this account is already in progress".to_string());
    }
    let blockers = state
        .store
        .outbox_switch_blockers(account)
        .map_err(map_store)?;
    if !blockers.is_empty() {
        return Err(switch_block_message(&blockers));
    }
    Ok(())
}

/// Start an interactive authentication attempt through the configured adapter.
pub async fn begin_auth(request: AuthRequest) -> Result<AuthProgress, String> {
    let service = auth_service()?;
    let mut guard = service.lock().await;
    guard
        .begin(request)
        .await
        .map_err(|error| error.safe_message)
}

/// Answer a challenge on an interactive attempt.
pub async fn answer_auth(
    attempt: &AuthAttemptId,
    answer: AuthAnswer,
) -> Result<AuthProgress, String> {
    let service = auth_service()?;
    let mut guard = service.lock().await;
    guard
        .respond(attempt, answer)
        .await
        .map_err(|error| error.safe_message)
}

/// Abandon an interactive attempt. A late answer must be refused by the
/// adapter; this wrapper also releases the service lock.
pub async fn cancel_auth(attempt: &AuthAttemptId) -> Result<(), String> {
    let service = auth_service()?;
    let mut guard = service.lock().await;
    guard
        .cancel(attempt)
        .await
        .map_err(|error| error.safe_message)
}

/// Ask the authentication adapter to revoke accounts' provider sessions. This
/// is deliberately best-effort and runs in the background: local removal must
/// still happen, and a provider that cannot be reached must not block it.
pub(crate) fn revoke_sessions_best_effort(
    service: Option<Arc<AsyncMutex<Box<dyn crate::auth::AuthenticationService>>>>,
    accounts: Vec<AccountId>,
) {
    if accounts.is_empty() {
        return;
    }
    let Some(service) = service else {
        return;
    };
    tokio::spawn(async move {
        let mut guard = service.lock().await;
        for account in accounts {
            if let Err(error) = guard.revoke(&account).await {
                log::warn!(
                    "revoke provider session for {account}: {}",
                    error.safe_message
                );
            }
        }
    });
}

fn auth_service() -> Result<Arc<AsyncMutex<Box<dyn crate::auth::AuthenticationService>>>, String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| not_initialized_message().to_string())?;
    state
        .config
        .auth
        .clone()
        .ok_or_else(|| "interactive authentication is not available for this provider".to_string())
}

/// Commit a newly authenticated account. An already-known id is an existing
/// account re-authentication, so it takes the guarded switch path instead of
/// creating a duplicate.
pub async fn finish_auth(
    authenticated: AuthenticatedAccount,
) -> Result<(AccountId, ModelSnapshot), String> {
    let id = authenticated.account.id.clone();
    let exists = {
        let guard = lock_state();
        guard.as_ref().is_some_and(|state| {
            state
                .config_doc
                .accounts
                .iter()
                .any(|account| account.id == id)
        })
    };
    if exists {
        let snapshot = switch_backend(&id, authenticated).await?;
        return Ok((id, snapshot));
    }

    let sender;
    let snapshot;
    {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| not_initialized_message().to_string())?;
        state.ensure_writable()?;

        let mut account = authenticated.account.clone();
        if !state.config.providers.supports(&account.provider_id()) {
            return Err(format!(
                "the `{}` provider is not available in this build",
                account.provider_id()
            ));
        }
        let sort_order = state
            .config_doc
            .accounts
            .iter()
            .map(|account| account.sort_order)
            .max()
            .map_or(0, |maximum| maximum.saturating_add(1));
        account.sort_order = sort_order;
        account.color = account
            .color
            .clone()
            .or_else(|| Some(crate::account::default_account_color(sort_order).to_string()));
        if account.label.trim().is_empty() {
            account.label = if account.email.trim().is_empty() {
                "Account".to_string()
            } else {
                account.email.clone()
            };
        }

        let secrets = CredentialSecretRepository::with_gate(
            state.config.credentials.clone(),
            state.store.clone(),
        );
        let staged = secrets
            .load_staged(&authenticated.credential_ref)
            .map_err(map_secret)?
            .ok_or_else(|| {
                "the authentication candidate is no longer available; start again".to_string()
            })?;
        let ctx = state
            .sessions
            .context(&account.id, INTERACTIVE_TIMEOUT)
            .map_err(|error| error.safe_message)?;
        let write = match authenticated.persistence {
            SecretPersistence::Persistent => secrets.save(&ctx, &staged),
            SecretPersistence::SessionOnly => secrets.save_session(&ctx, &staged),
        };
        if let Err(error) = write {
            let _ = secrets.discard_staged(&authenticated.credential_ref);
            return Err(map_secret(error));
        }

        let mut next = state.config_doc.accounts.clone();
        next.push(account.clone());
        if let Err(error) = state.replace_accounts(next) {
            let _ = secrets.delete(&account.id);
            let _ = secrets.discard_staged(&authenticated.credential_ref);
            return Err(error);
        }
        let _ = secrets.discard_staged(&authenticated.credential_ref);

        state.ensure_selection();
        sender = state.events.clone();
        snapshot = state.build_snapshot();
    }

    let _ = sender.send(CoreEvent::SnapshotChanged);
    Ok((id, snapshot))
}

/// Switch an existing account to the backend represented by `authenticated`.
///
/// The operation is refused while a submission is pending, cancels every live
/// context for the account, drains the account gate, and then commits through a
/// durable switch intent. Provider-derived mail cache and checkpoints are
/// rebuilt; drafts, the account id, and other local data survive.
pub async fn switch_backend(
    account: &AccountId,
    authenticated: AuthenticatedAccount,
) -> Result<ModelSnapshot, String> {
    if authenticated.account.id != *account {
        return Err(
            "the authenticated account does not match the account being switched".to_string(),
        );
    }

    let (scheduler, ctx, previous, target) = {
        let mut guard = lock_state();
        let state = guard
            .as_mut()
            .ok_or_else(|| not_initialized_message().to_string())?;
        state.ensure_writable()?;

        let previous = state
            .config_doc
            .accounts
            .iter()
            .find(|candidate| &candidate.id == account)
            .cloned()
            .ok_or_else(|| format!("unknown account {account}"))?;

        if state.store.has_switch_intent(account).map_err(map_store)? {
            return Err("a backend switch for this account is already in progress".to_string());
        }
        let blockers = state
            .store
            .outbox_switch_blockers(account)
            .map_err(map_store)?;
        if !blockers.is_empty() {
            return Err(switch_block_message(&blockers));
        }
        if !state
            .config
            .providers
            .supports(&authenticated.account.provider_id())
        {
            return Err(format!(
                "the `{}` provider is not available in this build",
                authenticated.account.provider_id()
            ));
        }

        let target = merge_target(&previous, &authenticated.account);

        // Cancel every context minted at the old generation before waiting for
        // the account gate. The old generation is fenced durably, so even a
        // worker that ignores cancellation cannot commit.
        state
            .sessions
            .invalidate(account)
            .map_err(|error| error.safe_message)?;
        let ctx = state
            .sessions
            .context(account, INTERACTIVE_TIMEOUT)
            .map_err(|error| error.safe_message)?;

        (state.scheduler.clone(), ctx, previous, target)
    };

    // Drain the gate: any worker that was already inside its remote-read/apply
    // sequence must finish or observe cancellation before the switch commits.
    let credential_ref = authenticated.credential_ref.clone();
    let session_only = authenticated.persistence == SecretPersistence::SessionOnly;
    let account_id = account.clone();
    let commit_ctx = ctx.clone();
    let result = scheduler
        .run_interactive(&ctx, async move {
            commit_switch(
                &account_id,
                &previous,
                &target,
                &commit_ctx,
                &credential_ref,
                session_only,
            )
        })
        .await
        .map_err(scheduler_message)?;
    let (snapshot, sender) = result?;
    let _ = sender.send(CoreEvent::SnapshotChanged);

    Ok(snapshot)
}

/// Synchronous commit performed while the account gate is held. No await may
/// appear in this function: the state lock is only held around SQLite and
/// credential-store writes.
fn commit_switch(
    account: &AccountId,
    previous: &Account,
    target: &Account,
    ctx: &OperationContext,
    credential_ref: &StagedSecretRef,
    session_only: bool,
) -> Result<(ModelSnapshot, tokio::sync::broadcast::Sender<CoreEvent>), String> {
    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| not_initialized_message().to_string())?;

    // Another edit/removal may have landed while we waited for the gate.
    let current = state
        .config_doc
        .accounts
        .iter()
        .find(|candidate| &candidate.id == account)
        .cloned()
        .ok_or_else(|| format!("unknown account {account}"))?;
    if !same_account_config(&current, previous) {
        return Err("the account changed while the backend switch was waiting".to_string());
    }
    if state.store.generation(account).map_err(map_store)? != ctx.generation {
        return Err("the account changed while the backend switch was waiting".to_string());
    }
    let blockers = state
        .store
        .outbox_switch_blockers(account)
        .map_err(map_store)?;
    if !blockers.is_empty() {
        return Err(switch_block_message(&blockers));
    }
    if state.store.has_switch_intent(account).map_err(map_store)? {
        return Err("a backend switch for this account is already in progress".to_string());
    }

    let secrets = CredentialSecretRepository::with_gate(
        state.config.credentials.clone(),
        state.store.clone(),
    );
    let staged = secrets
        .load_staged(credential_ref)
        .map_err(map_secret)?
        .ok_or_else(|| {
            "the authentication candidate is no longer available; start again".to_string()
        })?;

    let previous_envelope = secrets.load(account).map_err(map_secret)?;
    let previous_persistent = secrets
        .load_persistent(account)
        .map_err(map_secret)?
        .is_some();
    let previous_present = previous_envelope.is_some();
    let backup_ref = StagedSecretRef::from_stored_key(&format!("switch_backup_{}", account.0))
        .map_err(map_secret)?;

    state
        .store
        .insert_switch_intent(&SwitchIntent {
            account: account.clone(),
            target: target.clone(),
            previous: previous.clone(),
            credential_ref: credential_ref.as_str().to_string(),
            backup_ref: previous_present.then(|| backup_ref.as_str().to_string()),
            session_only,
            previous_present,
            previous_persistent,
            phase: PHASE_INTENT,
            created_at_ms: now_ms(),
        })
        .map_err(map_store)?;

    if previous_present {
        let previous_secret = previous_envelope
            .as_ref()
            .ok_or_else(|| "the previous credential disappeared".to_string())?;
        if let Err(error) =
            secrets.write_reference(&backup_ref, previous_secret, previous_persistent)
        {
            // The active credential was not touched. Discard the recovery
            // record; if that cleanup fails, keep the intent so the (idempotent)
            // startup sweep can remove it rather than orphaning a backup.
            if let Err(cleanup) =
                cleanup_switch_references(&secrets, &backup_ref, previous_present, credential_ref)
            {
                log::warn!(
                    "keep switch intent after backup failure for {account}: {}",
                    cleanup.safe_message
                );
            } else if let Err(remove) = state.store.remove_switch_intent(account) {
                log::warn!("keep switch intent after backup failure for {account}: {remove}");
            }
            return Err(map_secret(error));
        }
    }

    // The backup stage is confirmed. The active credential has not been touched
    // yet; this state only says a reachable rollback copy exists (or that the
    // account had no credential to copy).
    state
        .store
        .set_switch_intent_phase(account, PHASE_BACKED_UP)
        .map_err(map_store)?;

    let write = if session_only {
        // `get` prefers a persistent entry, so a session-only commit must remove
        // the previous OS secret or the new backend would keep reading it. The
        // rollback backup was written above, so the old credential is not lost
        // if this fails or the process dies between the two writes.
        let cleared = if previous_persistent {
            secrets.delete(account)
        } else {
            Ok(())
        };
        cleared.and_then(|()| secrets.save_session(ctx, &staged))
    } else {
        secrets.save(ctx, &staged)
    };
    if let Err(error) = write {
        return Err(rollback_switch(
            &state.store,
            &secrets,
            account,
            ctx,
            previous_envelope.as_ref(),
            previous_persistent,
            &backup_ref,
            previous_present,
            credential_ref,
            map_secret(error),
        ));
    }

    // The new active credential write returned success. Only now may recovery
    // treat the new credential as active. If the confirmation cannot be
    // recorded, roll back instead of continuing with an unconfirmed write.
    if let Err(error) = state
        .store
        .set_switch_intent_phase(account, PHASE_CREDENTIAL_WRITTEN)
    {
        return Err(rollback_switch(
            &state.store,
            &secrets,
            account,
            ctx,
            previous_envelope.as_ref(),
            previous_persistent,
            &backup_ref,
            previous_present,
            credential_ref,
            map_store(error),
        ));
    }

    // Test-only process death between secret activation and config publication.
    simulated_crash(1)?;

    let mut next = state.config_doc.accounts.clone();
    if let Some(slot) = next.iter_mut().find(|candidate| &candidate.id == account) {
        *slot = target.clone();
    } else {
        return Err(format!("unknown account {account}"));
    }
    if let Err(error) = state.replace_accounts(next) {
        return Err(rollback_switch(
            &state.store,
            &secrets,
            account,
            ctx,
            previous_envelope.as_ref(),
            previous_persistent,
            &backup_ref,
            previous_present,
            credential_ref,
            error,
        ));
    }
    state
        .store
        .set_switch_intent_phase(account, PHASE_CONFIG_WRITTEN)
        .map_err(map_store)?;

    // Test-only process death after the config was published.
    simulated_crash(2)?;

    let backend_changed = previous.backend != target.backend;
    if backend_changed {
        match state.store.clear_provider_cache(account) {
            Ok(files) => {
                for path in files {
                    if let Err(error) = std::fs::remove_file(&path) {
                        log::warn!("remove switched cache file {}: {error}", path.display());
                    }
                }
            }
            Err(error) => {
                // The config and credential pair is already consistent. Leave
                // the intent so startup retries the cache clear instead of
                // pretending the stale provider cache is gone.
                return Err(map_store(error));
            }
        }
    } else {
        for source in replaced_contact_sources(previous, target) {
            state
                .store
                .clear_contact_source_cache(account, &source)
                .map_err(map_store)?;
        }
    }

    // Cache clearing is idempotent; a failure keeps the intent so startup
    // retries it. Record `Completed` only once it returned success.
    if let Err(error) = state
        .store
        .set_switch_intent_phase(account, PHASE_COMPLETED)
    {
        return Err(map_store(error));
    }

    complete_switch_intent(
        &state.store,
        &secrets,
        account,
        &backup_ref,
        previous_present,
        credential_ref,
    )
    .map_err(map_store)?;

    state.capabilities.remove(account);
    state.selected_folder = None;
    state.invalidate_open();
    state.selected_conversation = None;
    state.server_more = None;
    state.conversation_limit = super::INITIAL_PAGE;
    state.ensure_selection();
    let sender = state.events.clone();
    Ok((state.build_snapshot(), sender))
}

/// Roll the active credential back after a commit step failed and only discard
/// the recovery record once the old credential is confirmed active again. If
/// the restore fails, the intent, its rollback backup, and the staged candidate
/// are deliberately kept: startup recovery is the only remaining path back to a
/// consistent config/credential pair.
#[allow(clippy::too_many_arguments)]
fn rollback_switch(
    store: &Store,
    secrets: &dyn SecretRepository,
    account: &AccountId,
    ctx: &OperationContext,
    previous: Option<&SecretEnvelope>,
    previous_persistent: bool,
    backup_ref: &StagedSecretRef,
    backup_present: bool,
    credential_ref: &StagedSecretRef,
    original: String,
) -> String {
    if let Err(error) = restore_secret(secrets, ctx, account, previous, previous_persistent) {
        return format!(
            "{original}; the previous credential could not be restored ({}); the switch will be recovered at startup",
            error.safe_message
        );
    }
    // The previous credential is active again. Remove the recovery record
    // *before* discarding the backup: a crash in between can then only orphan a
    // temporary reference for the startup sweep, never leave an intent whose
    // rollback data is already gone.
    match store.remove_switch_intent(account) {
        Ok(()) => {
            if let Err(error) =
                cleanup_switch_references(secrets, backup_ref, backup_present, credential_ref)
            {
                log::warn!(
                    "discard switch references for {account}: {}; the startup sweep will retry",
                    error.safe_message
                );
            }
            original
        }
        Err(error) => format!("{original}; could not clear the switch intent: {error}"),
    }
}

/// Retire a switch whose config/credential pair is already consistent. The
/// intent is removed first; reference cleanup is best effort and retried by the
/// startup orphan sweep, so a crash can never leave an intent without the
/// rollback data a later recovery might need.
fn complete_switch_intent(
    store: &Store,
    secrets: &dyn SecretRepository,
    account: &AccountId,
    backup_ref: &StagedSecretRef,
    backup_present: bool,
    credential_ref: &StagedSecretRef,
) -> Result<(), StoreError> {
    store.remove_switch_intent(account)?;
    if let Err(error) =
        cleanup_switch_references(secrets, backup_ref, backup_present, credential_ref)
    {
        log::warn!(
            "discard switch references for {account}: {}; the startup sweep will retry",
            error.safe_message
        );
    }
    Ok(())
}

fn restore_secret(
    secrets: &dyn SecretRepository,
    ctx: &OperationContext,
    account: &AccountId,
    previous: Option<&SecretEnvelope>,
    persistent: bool,
) -> Result<(), ProviderError> {
    match previous {
        Some(envelope) => {
            if persistent {
                secrets.save(ctx, envelope)
            } else {
                secrets.save_session(ctx, envelope)
            }
        }
        None => secrets.delete(account),
    }
}

fn cleanup_switch_references(
    secrets: &dyn SecretRepository,
    backup_ref: &StagedSecretRef,
    backup_present: bool,
    credential_ref: &StagedSecretRef,
) -> Result<(), ProviderError> {
    let mut first_error = None;
    if backup_present && let Err(error) = secrets.discard_reference(backup_ref) {
        first_error = Some(error);
    }
    if let Err(error) = secrets.discard_staged(credential_ref)
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn merge_target(previous: &Account, supplied: &Account) -> Account {
    let same_backend = previous.backend == supplied.backend;
    let mut target = supplied.clone();
    target.id = previous.id.clone();
    target.sort_order = previous.sort_order;
    target.color = previous.color.clone().or_else(|| supplied.color.clone());
    if target.label.trim().is_empty() {
        target.label = previous.label.clone();
    }
    if target.email.trim().is_empty() {
        target.email = previous.email.clone();
    }
    if target.display_name.trim().is_empty() {
        target.display_name = previous.display_name.clone();
    }
    if same_backend {
        // An authentication service may return a provisional account shape for
        // a re-authentication. Never replace working endpoints with placeholders.
        target.backend = previous.backend.clone();
        if target.contact_sources.is_empty() {
            target.contact_sources = previous.contact_sources.clone();
        }
    }
    target
}

fn same_account_config(left: &Account, right: &Account) -> bool {
    left.id == right.id
        && left.label == right.label
        && left.email == right.email
        && left.display_name == right.display_name
        && left.backend == right.backend
        && left.contact_sources == right.contact_sources
        && left.sort_order == right.sort_order
        && left.color == right.color
}

fn replaced_contact_sources(previous: &Account, target: &Account) -> Vec<String> {
    previous
        .contact_sources
        .iter()
        .filter(|old| {
            !target
                .contact_sources
                .iter()
                .any(|new| new.id == old.id && new.provider == old.provider && new.kind == old.kind)
        })
        .map(|source| source.id.as_str().to_string())
        .collect()
}

fn intent_target_applied(current: &Account, intent: &SwitchIntent) -> bool {
    current.label == intent.target.label
        && current.email == intent.target.email
        && current.display_name == intent.target.display_name
        && current.backend == intent.target.backend
        && current.contact_sources == intent.target.contact_sources
}

fn intent_previous_applied(current: &Account, intent: &SwitchIntent) -> bool {
    current.label == intent.previous.label
        && current.email == intent.previous.email
        && current.display_name == intent.previous.display_name
        && current.backend == intent.previous.backend
        && current.contact_sources == intent.previous.contact_sources
}

/// Clear the same provider-derived state as the live commit. A backend change
/// clears the whole provider cache; a same-backend change clears only the
/// contact sources that disappeared.
fn clear_switch_cache(store: &Store, intent: &SwitchIntent) -> Result<(), String> {
    if intent.previous.backend != intent.target.backend {
        match store.clear_provider_cache(&intent.account) {
            Ok(files) => {
                for path in files {
                    if let Err(error) = std::fs::remove_file(&path) {
                        log::warn!("remove switched cache file {}: {error}", path.display());
                    }
                }
                Ok(())
            }
            Err(error) => Err(format!(
                "Could not finish clearing the previous provider cache: {error}"
            )),
        }
    } else {
        for source in replaced_contact_sources(&intent.previous, &intent.target) {
            store
                .clear_contact_source_cache(&intent.account, &source)
                .map_err(|error| {
                    format!("Could not finish clearing a replaced contact source cache: {error}")
                })?;
        }
        Ok(())
    }
}

enum Activation {
    Activated,
    Missing,
}

/// Write the staged authentication candidate to the active entry. `Missing`
/// means the process (or its restart) has already discarded the candidate and it
/// can never be activated again; a store failure is returned so the caller
/// retries instead of guessing.
fn activate_staged_credential(
    secrets: &dyn SecretRepository,
    store: &Store,
    intent: &SwitchIntent,
) -> Result<Activation, ProviderError> {
    let Ok(reference) = StagedSecretRef::from_stored_key(&intent.credential_ref) else {
        return Ok(Activation::Missing);
    };
    let Some(staged) = secrets.load_staged(&reference)? else {
        return Ok(Activation::Missing);
    };
    let ctx = recovery_context(store, &intent.account);
    if intent.session_only {
        secrets.save_session(&ctx, &staged)?;
    } else {
        secrets.save(&ctx, &staged)?;
    }
    Ok(Activation::Activated)
}

enum RestoredCredential {
    Restored,
    BackupMissing,
}

fn restore_intent_credential(
    secrets: &dyn SecretRepository,
    store: &Store,
    intent: &SwitchIntent,
) -> Result<RestoredCredential, ProviderError> {
    if !intent.previous_present {
        secrets.delete(&intent.account)?;
        return Ok(RestoredCredential::Restored);
    }
    let Some(raw) = intent.backup_ref.as_deref() else {
        return Ok(RestoredCredential::BackupMissing);
    };
    let Ok(reference) = StagedSecretRef::from_stored_key(raw) else {
        return Ok(RestoredCredential::BackupMissing);
    };
    let Some(envelope) = secrets.load_staged(&reference)? else {
        return Ok(RestoredCredential::BackupMissing);
    };
    let ctx = recovery_context(store, &intent.account);
    restore_secret(
        secrets,
        &ctx,
        &intent.account,
        Some(&envelope),
        intent.previous_persistent,
    )?;
    Ok(RestoredCredential::Restored)
}

/// Confirm that the target backend has its new credential active. Only a
/// recorded `CredentialWritten` transition (or a successful activation of a
/// still-available staged candidate) counts as confirmation.
fn confirm_target_credential(
    secrets: &dyn SecretRepository,
    store: &Store,
    intent: &SwitchIntent,
) -> Result<bool, ProviderError> {
    if intent.phase >= PHASE_CREDENTIAL_WRITTEN && secrets.load(&intent.account)?.is_some() {
        return Ok(true);
    }
    match activate_staged_credential(secrets, store, intent)? {
        Activation::Activated => {
            store
                .set_switch_intent_phase(&intent.account, PHASE_CREDENTIAL_WRITTEN)
                .map_err(crate::auth::store_error_to_provider)?;
            Ok(true)
        }
        Activation::Missing => Ok(false),
    }
}

/// Put the previous credential back for a config that is known to be the
/// previous shape. A failed restore or delete keeps the intent and (when it
/// exists) the backup so the next start can retry; nothing is silently dropped.
fn settle_previous_credential(
    secrets: &dyn SecretRepository,
    store: &Store,
    intent: &SwitchIntent,
    warnings: &mut Vec<String>,
) -> bool {
    if intent.phase < PHASE_BACKED_UP {
        // No credential write or backup attempt ever started: the active
        // credential is already the previous one.
        return true;
    }
    match restore_intent_credential(secrets, store, intent) {
        Ok(RestoredCredential::Restored) => true,
        Ok(RestoredCredential::BackupMissing) => {
            // No usable backup: remove whatever may be active. Only a successful
            // delete (or a verified absence) proves the old backend is not left
            // paired with a newer credential.
            match secrets.delete(&intent.account) {
                Ok(()) => {
                    warnings.push(
                        "A previous credential backup was no longer available after an interrupted backend switch; sign in again."
                            .to_string(),
                    );
                    true
                }
                Err(error) => {
                    warnings.push(format!(
                        "The credential of an interrupted backend switch could not be cleared ({}); it will be retried at the next start.",
                        error.safe_message
                    ));
                    false
                }
            }
        }
        Err(error) => {
            warnings.push(format!(
                "The previous credential could not be restored ({}); the switch will be retried at the next start.",
                error.safe_message
            ));
            false
        }
    }
}

/// Replace the account shape with the pre-switch one, but only when that write
/// succeeds. `false` keeps the intent so the next start retries.
fn rollback_config_to_previous(
    store: &Store,
    config_path: &Path,
    doc: &mut ConfigDocument,
    index: usize,
    intent: &SwitchIntent,
    warnings: &mut Vec<String>,
) -> bool {
    let mut candidate = doc.clone();
    candidate.accounts[index] = intent.previous.clone();
    if let Err(error) = config::save(config_path, &candidate) {
        warnings.push(format!(
            "Could not restore the previous account setup: {error}"
        ));
        return false;
    }
    *doc = candidate;
    if let Err(error) = store.sync_accounts(&doc.accounts) {
        warnings.push(format!(
            "Could not restore the previous account cache: {error}"
        ));
        return false;
    }
    true
}

/// Complete or roll back every interrupted backend switch. Called by
/// [`super::init`] after the configuration document is loaded and before the
/// process-wide state is installed.
pub(crate) fn recover_switch_intents(
    store: &Arc<Store>,
    credentials: &Arc<dyn CredentialStore>,
    config_path: &Path,
    doc: &mut ConfigDocument,
) -> Result<Vec<String>, String> {
    let intents = store.switch_intents().map_err(map_store)?;
    if intents.is_empty() {
        return Ok(Vec::new());
    }
    let secrets = CredentialSecretRepository::with_gate(credentials.clone(), store.clone());
    let mut warnings = Vec::new();

    for intent in intents {
        let Some(index) = doc
            .accounts
            .iter()
            .position(|account| account.id == intent.account)
        else {
            // The account is gone; any credential left by the switch must go
            // too. A failed delete keeps the intent so the next start retries.
            match secrets.delete(&intent.account) {
                Ok(()) => finish_recovered_intent(store, &secrets, &intent, &mut warnings),
                Err(error) => warnings.push(format!(
                    "Could not clear the credential of an account removed during an interrupted backend switch ({}); the switch will be retried at the next start.",
                    error.safe_message
                )),
            }
            continue;
        };

        let target_applied;
        let previous_applied;
        let backend_is_target;
        let backend_is_previous;
        {
            let current = &doc.accounts[index];
            target_applied = intent_target_applied(current, &intent);
            previous_applied = intent_previous_applied(current, &intent);
            backend_is_target = current.backend == intent.target.backend;
            backend_is_previous = current.backend == intent.previous.backend;
        }

        if !target_applied && !previous_applied {
            // An unrelated edit landed while the switch was interrupted. Do not
            // overwrite it, but never leave a credential that might belong to
            // the other side of the switch attached to it either.
            warnings.push(
                "The account changed outside an interrupted backend switch; its configuration was left as edited."
                    .to_string(),
            );
            let settle = if backend_is_target {
                match confirm_target_credential(&secrets, store, &intent) {
                    Ok(true) => match clear_switch_cache(store, &intent) {
                        Ok(()) => true,
                        Err(warning) => {
                            warnings.push(warning);
                            false
                        }
                    },
                    Ok(false) => match secrets.delete(&intent.account) {
                        Ok(()) => {
                            warnings.push(
                                "An interrupted switch's credential could not be matched to the edited account; sign in again."
                                    .to_string(),
                            );
                            true
                        }
                        Err(error) => {
                            warnings.push(format!(
                                "An interrupted switch's credential could not be cleared ({}); it will be retried at the next start.",
                                error.safe_message
                            ));
                            false
                        }
                    },
                    Err(error) => {
                        warnings.push(format!(
                            "The interrupted switch's credential could not be verified ({}); it will be retried at the next start.",
                            error.safe_message
                        ));
                        false
                    }
                }
            } else if backend_is_previous {
                settle_previous_credential(&secrets, store, &intent, &mut warnings)
            } else {
                match secrets.delete(&intent.account) {
                    Ok(()) => true,
                    Err(error) => {
                        warnings.push(format!(
                            "An interrupted switch's credential could not be cleared ({}); it will be retried at the next start.",
                            error.safe_message
                        ));
                        false
                    }
                }
            };
            if settle {
                finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            }
            continue;
        }

        // A session-only credential cannot survive a restart, so a published
        // target must be rolled back even though the phase says it was written.
        if target_applied && intent.session_only {
            if !rollback_config_to_previous(store, config_path, doc, index, &intent, &mut warnings)
            {
                continue;
            }
            if !settle_previous_credential(&secrets, store, &intent, &mut warnings) {
                continue;
            }
            finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            continue;
        }

        if target_applied {
            let ready = match confirm_target_credential(&secrets, store, &intent) {
                Ok(ready) => ready,
                Err(error) => {
                    warnings.push(format!(
                        "The new backend credential could not be verified ({}); the switch will be retried at the next start.",
                        error.safe_message
                    ));
                    continue;
                }
            };
            if !ready {
                // The staged candidate is gone and the new credential is not
                // confirmed. Roll the account back rather than pair the target
                // backend with the old credential.
                if !rollback_config_to_previous(
                    store,
                    config_path,
                    doc,
                    index,
                    &intent,
                    &mut warnings,
                ) {
                    continue;
                }
                if !settle_previous_credential(&secrets, store, &intent, &mut warnings) {
                    continue;
                }
                warnings.push(
                    "An interrupted backend switch could not activate its new credential; the previous account setup was restored."
                        .to_string(),
                );
                finish_recovered_intent(store, &secrets, &intent, &mut warnings);
                continue;
            }

            // The config and credential are a matching pair; finish the same
            // cleanup the live commit would have done before dropping the intent.
            if let Err(warning) = clear_switch_cache(store, &intent) {
                warnings.push(warning);
                continue;
            }
            if let Err(error) = store.set_switch_intent_phase(&intent.account, PHASE_COMPLETED) {
                warnings.push(format!(
                    "Could not record a completed interrupted backend switch: {error}"
                ));
                continue;
            }
            finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            continue;
        }

        // The pre-switch configuration is still published: restore its credential.
        if settle_previous_credential(&secrets, store, &intent, &mut warnings) {
            finish_recovered_intent(store, &secrets, &intent, &mut warnings);
        }
    }

    Ok(warnings)
}

fn recovery_context(store: &Store, account: &AccountId) -> OperationContext {
    let generation = store
        .generation(account)
        .unwrap_or(crate::store::INITIAL_GENERATION);
    OperationContext::with_timeout(account.clone(), generation, LIFECYCLE_REFERENCE_TTL)
}

fn cleanup_intent_references(
    secrets: &dyn SecretRepository,
    intent: &SwitchIntent,
) -> Result<(), ProviderError> {
    let mut first_error = None;
    if let Some(raw) = &intent.backup_ref
        && let Ok(reference) = StagedSecretRef::from_stored_key(raw)
        && let Err(error) = secrets.discard_reference(&reference)
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    if let Ok(reference) = StagedSecretRef::from_stored_key(&intent.credential_ref)
        && let Err(error) = secrets.discard_staged(&reference)
        && first_error.is_none()
    {
        first_error = Some(error);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Remove a recovered intent whose pair is already consistent, *then* discard
/// its temporary references. A crash between the two can only leave an orphaned
/// reference for the startup sweep; it can never leave an intent that needs a
/// backup which was already discarded.
fn finish_recovered_intent(
    store: &Store,
    secrets: &dyn SecretRepository,
    intent: &SwitchIntent,
    warnings: &mut Vec<String>,
) {
    if let Err(error) = store.remove_switch_intent(&intent.account) {
        warnings.push(format!(
            "Could not clear an interrupted backend switch: {error}"
        ));
        return;
    }
    if let Err(error) = cleanup_intent_references(secrets, intent) {
        warnings.push(format!(
            "Temporary switch credentials could not be discarded ({}); the startup orphan sweep will retry.",
            error.safe_message
        ));
    }
}

/// Reconcile every unfinished switch against the account list a restore just
/// installed, then drop the recovery record once the restored backend and the
/// active credential are a pair again.
///
/// This runs *after* the restored document is durable. A crash before this
/// completes still leaves the intent for startup recovery, which makes the same
/// choice. If a credential cannot be made to match, the active credential is
/// removed (forcing re-authentication) rather than pairing a new backend with an
/// old secret; a failed delete keeps the intent for the next start.
pub(crate) fn reconcile_restored_switches(
    store: &Arc<Store>,
    credentials: &Arc<dyn CredentialStore>,
    accounts: &[Account],
) -> Vec<String> {
    let secrets = CredentialSecretRepository::with_gate(credentials.clone(), store.clone());
    let mut warnings = Vec::new();
    let intents = match store.switch_intents() {
        Ok(intents) => intents,
        Err(error) => {
            warnings.push(format!(
                "Could not inspect interrupted backend switches after the restore: {error}"
            ));
            return warnings;
        }
    };
    for intent in intents {
        let Some(restored) = accounts
            .iter()
            .find(|candidate| candidate.id == intent.account)
        else {
            // The restore dropped the account; clear any credential the switch
            // may have left, but only finish once the delete succeeded.
            match secrets.delete(&intent.account) {
                Ok(()) => finish_recovered_intent(store, &secrets, &intent, &mut warnings),
                Err(error) => warnings.push(format!(
                    "A restored-away account's switch credential could not be cleared ({}); it will be retried at the next start.",
                    error.safe_message
                )),
            }
            continue;
        };

        if intent.previous.backend == intent.target.backend {
            // Either credential is semantically valid for the restored account.
            // Still apply the forward cleanup when the restored shape is the
            // target so a replaced contact source is not replayed.
            if intent_target_applied(restored, &intent)
                && let Err(warning) = clear_switch_cache(store, &intent)
            {
                warnings.push(warning);
                continue;
            }
            finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            continue;
        }

        if restored.backend == intent.target.backend {
            // The target backend needs the *new* credential. Never keep the old
            // one merely because a phase says a write was attempted.
            let ready = match confirm_target_credential(&secrets, store, &intent) {
                Ok(ready) => ready,
                Err(error) => {
                    warnings.push(format!(
                        "A restored account's new credential could not be verified ({}); it will be retried at the next start.",
                        error.safe_message
                    ));
                    continue;
                }
            };
            if !ready {
                match secrets.delete(&intent.account) {
                    Ok(()) => {
                        warnings.push(
                            "A restored account could not be paired with its new credential; sign in again."
                                .to_string(),
                        );
                        finish_recovered_intent(store, &secrets, &intent, &mut warnings);
                    }
                    Err(error) => warnings.push(format!(
                        "A restored account's mismatched credential could not be cleared ({}); it will be retried at the next start.",
                        error.safe_message
                    )),
                }
                continue;
            }
            if let Err(warning) = clear_switch_cache(store, &intent) {
                warnings.push(warning);
                continue;
            }
            finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            continue;
        }

        if restored.backend == intent.previous.backend {
            if settle_previous_credential(&secrets, store, &intent, &mut warnings) {
                finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            }
            continue;
        }

        // A third backend cannot be proven to pair with either credential.
        match secrets.delete(&intent.account) {
            Ok(()) => {
                warnings.push(
                    "The restored account uses a different backend than the interrupted switch; sign in again."
                        .to_string(),
                );
                finish_recovered_intent(store, &secrets, &intent, &mut warnings);
            }
            Err(error) => warnings.push(format!(
                "A restored account's switch credential could not be cleared ({}); it will be retried at the next start.",
                error.safe_message
            )),
        }
    }
    warnings
}

fn switch_backup_ref(account: &AccountId) -> Option<StagedSecretRef> {
    StagedSecretRef::from_stored_key(&format!("switch_backup_{}", account.0)).ok()
}

/// Delete a `switch_backup_<account>` reference even when its intent row was
/// already lost. Used by explicit removal/reset paths and the startup sweep.
pub(crate) fn discard_switch_backup(credentials: &Arc<dyn CredentialStore>, account: &AccountId) {
    if let Err(error) = discard_switch_backup_reporting(credentials, account) {
        log::warn!("discard switch rollback credential for {account}: {error}");
    }
}

/// Fallible form used where the caller reports cleanup warnings to the user.
pub(crate) fn discard_switch_backup_reporting(
    credentials: &Arc<dyn CredentialStore>,
    account: &AccountId,
) -> Result<(), String> {
    let Some(reference) = switch_backup_ref(account) else {
        return Ok(());
    };
    let secrets = CredentialSecretRepository::new(credentials.clone());
    secrets
        .discard_reference(&reference)
        .map_err(|error| error.safe_message)
}

/// Remove rollback backups that have no matching switch intent. The secure
/// store cannot be enumerated, so this covers every account id known from the
/// configuration document and the SQLite mirror; an intent that still exists is
/// always left untouched for recovery.
pub(crate) fn sweep_orphaned_switch_references(
    store: &Arc<Store>,
    credentials: &Arc<dyn CredentialStore>,
    accounts: &[Account],
) {
    let mut ids: Vec<AccountId> = accounts.iter().map(|account| account.id.clone()).collect();
    match store.accounts() {
        Ok(stored) => ids.extend(stored.into_iter().map(|account| account.id)),
        Err(error) => log::warn!("list accounts for switch reference sweep: {error}"),
    }
    ids.sort_by(|left, right| left.0.cmp(&right.0));
    ids.dedup();
    for id in ids {
        // On an unknown intent state, keep the reference: a missing backup is
        // worse than a delayed sweep.
        if store.has_switch_intent(&id).unwrap_or(true) {
            continue;
        }
        discard_switch_backup(credentials, &id);
    }
}

/// Discard every unfinished switch and its temporary credentials. Factory reset
/// is an explicit user action that supersedes a crash remnant; leaving a
/// rollback backup behind would leak a protected secret. Cleanup failures are
/// returned as warnings and retried by the normal startup sweep while the
/// account id is still known.
pub(crate) fn discard_pending_switches(
    store: &Arc<Store>,
    credentials: &Arc<dyn CredentialStore>,
) -> Result<Vec<String>, String> {
    let secrets = CredentialSecretRepository::with_gate(credentials.clone(), store.clone());
    let mut warnings = Vec::new();
    for intent in store.switch_intents().map_err(map_store)? {
        if let Err(error) = cleanup_intent_references(&secrets, &intent) {
            warnings.push(format!(
                "A temporary credential from an interrupted backend switch could not be discarded ({}); it will be retried by the startup sweep.",
                error.safe_message
            ));
        }
        store
            .remove_switch_intent(&intent.account)
            .map_err(map_store)?;
    }
    Ok(warnings)
}

/// Discard one account's unfinished switch before an explicit removal.
pub(crate) fn discard_switch_intent(
    store: &Arc<Store>,
    credentials: &Arc<dyn CredentialStore>,
    account: &AccountId,
) -> Result<(), String> {
    let Some(intent) = store.switch_intent(account).map_err(map_store)? else {
        return Ok(());
    };
    let secrets = CredentialSecretRepository::with_gate(credentials.clone(), store.clone());
    if let Err(error) = cleanup_intent_references(&secrets, &intent) {
        log::warn!(
            "discard switch references for {account}: {}",
            error.safe_message
        );
    }
    store.remove_switch_intent(account).map_err(map_store)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
