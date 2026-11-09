//! Local-only recipient autocomplete for compose.
//!
//! Lookup reads the indexed SQLite projection; it never opens a provider and
//! never logs the query text. A deliberate selection is the only write path
//! exposed here: arbitrary keystrokes are not evidence.

use std::collections::HashSet;
use std::time::Duration;

use crate::account::{Account, AccountId, ContactSourceConfig};
use crate::contacts::normalize::{normalize_address, normalize_email};
use crate::contacts::rank::{
    OTHER_CONTACT_KIND, SAVED_CONTACT_KIND, bound_query_text, bounded_limit, folded_query,
    query_tokens, rank_candidates,
};
use crate::contacts::types::{AddressKey, RecipientQuery, RecipientSuggestion};
use crate::mail::Address;
use crate::providers::{ContactSourceId, OperationContext};

use super::lock_state;

/// Read a few times the requested limit so blocked/self identities and
/// multi-account duplicates cannot empty an otherwise valid result page.
const CANDIDATE_MULTIPLIER: usize = 16;
const CANDIDATE_FLOOR: usize = 64;

/// Suggest recipients for the current compose field. Local-only and bounded.
pub fn suggest_recipients(query: RecipientQuery) -> Result<Vec<RecipientSuggestion>, String> {
    let text = bound_query_text(&query.text);
    let limit = bounded_limit(query.limit);
    let tokens = query_tokens(&text);
    if limit == 0 || tokens.is_empty() {
        return Ok(Vec::new());
    }
    let folded = folded_query(&text);

    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    // An unknown sending account has no compose context; never return another
    // account's suggestions for it.
    if !state
        .config_doc
        .accounts
        .iter()
        .any(|configured| configured.id == query.sending_account)
    {
        return Ok(Vec::new());
    }

    let cap = limit
        .saturating_mul(CANDIDATE_MULTIPLIER)
        .max(CANDIDATE_FLOOR);
    let candidates = state
        .store
        .recipient_candidates(&query.sending_account, &folded, &tokens, cap)
        .map_err(|error| format!("search recipients: {error}"))?;

    let blocked: HashSet<String> = state
        .config_doc
        .blocked_senders
        .iter()
        .map(|address| address.trim().to_lowercase())
        .filter(|address| !address.is_empty())
        .collect();
    let self_keys = self_identity_keys(&state.config_doc.accounts);
    Ok(rank_candidates(
        candidates,
        &query.sending_account,
        &folded,
        &blocked,
        &self_keys,
        limit,
    ))
}

/// Record that the user deliberately picked `address` in compose. Repeated
/// selections refresh recency without inflating frequency.
pub fn record_recipient_selection(account: &AccountId, address: &Address) -> Result<(), String> {
    normalize_address(address).map_err(|error| format!("invalid recipient address: {error}"))?;

    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    if !state
        .config_doc
        .accounts
        .iter()
        .any(|configured| &configured.id == account)
    {
        return Err(format!("unknown account {account}"));
    }
    let generation = state
        .store
        .generation(account)
        .map_err(|error| format!("read account generation: {error}"))?;
    let ctx = OperationContext::with_timeout(account.clone(), generation, Duration::from_secs(5));
    state
        .store
        .record_selection(&ctx, address)
        .map_err(|error| format!("record recipient selection: {error}"))
}

/// Pause or resume recipient learning for one account. Pausing keeps existing
/// suggestions but stops every ingestion path until learning is resumed.
pub fn pause_learning(account: &AccountId, paused: bool) -> Result<(), String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    ensure_configured_account(state, account)?;
    state
        .store
        .pause_learning(account, paused)
        .map_err(|error| format!("pause recipient learning: {error}"))
}

/// Forget this account's learned recipient evidence and suggestions. Synced
/// contacts are preserved. This is a logical deletion, not secure forensic
/// erasure, and automatic history/cache ingestion stays disabled until an
/// explicit rebuild.
pub fn clear_learned_recipients(account: &AccountId) -> Result<(), String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    ensure_configured_account(state, account)?;
    state
        .store
        .clear_learned_recipients(account)
        .map_err(|error| format!("clear learned recipients: {error}"))
}

/// Discard learned recipient data and re-enable automatic history under a new
/// learning epoch. Synced contacts are preserved.
pub fn rebuild_learned_recipients(account: &AccountId) -> Result<(), String> {
    let guard = lock_state();
    let state = guard
        .as_ref()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    ensure_configured_account(state, account)?;
    state
        .store
        .rebuild_learned_recipients(account)
        .map_err(|error| format!("rebuild learned recipients: {error}"))
}

/// Enable or disable synchronization for one contact source.
///
/// Disabling cancels the source's resumable job and leaves every cached contact
/// in place; only an explicit source/account removal deletes local contact rows.
/// Learned mail-derived evidence is never affected.
pub fn set_contact_sync(
    account: &AccountId,
    source: &ContactSourceId,
    enabled: bool,
) -> Result<(), String> {
    let mut guard = lock_state();
    let state = guard
        .as_mut()
        .ok_or_else(|| "pige api is not initialized".to_string())?;
    state.ensure_writable()?;
    let current = state
        .config_doc
        .accounts
        .iter()
        .find(|configured| &configured.id == account)
        .cloned()
        .ok_or_else(|| format!("unknown account {account}"))?;

    let mut updated = current.clone();
    let kind = match updated
        .contact_sources
        .iter_mut()
        .find(|configured| &configured.id == source)
    {
        Some(existing) => {
            existing.enabled = enabled;
            existing.kind.clone()
        }
        None => {
            // Google's auto-populated Other contacts rank below saved contacts.
            // The legacy upsert defaults to `saved`, so the source kind has to
            // be explicit here as well as in the durable store.
            let kind = contact_source_kind(source).to_string();
            updated.contact_sources.push(ContactSourceConfig {
                id: source.clone(),
                provider: current.provider_id(),
                kind: kind.clone(),
                enabled,
            });
            kind
        }
    };
    let mut accounts = state.config_doc.accounts.clone();
    if let Some(slot) = accounts
        .iter_mut()
        .find(|configured| &configured.id == account)
    {
        *slot = updated;
    }
    state.replace_accounts(accounts)?;

    // The configuration document is authoritative for user intent. The store's
    // state is the durable execution fence; a failure is reported rather than
    // silently leaving a source believing it is still enabled. Registering the
    // kind before enabling prevents `set_contact_source_enabled` from creating
    // an Other-contacts source with the default saved ranking.
    if enabled {
        state
            .store
            .upsert_contact_source_with_kind(account, source, None, &kind)
            .map_err(|error| format!("update contact source kind: {error}"))?;
    }
    state
        .store
        .set_contact_source_enabled(account, source, enabled)
        .map_err(|error| format!("update contact source state: {error}"))?;
    let sender = state.events.clone();
    drop(guard);
    let _ = sender.send(super::CoreEvent::SnapshotChanged);
    Ok(())
}

/// Default ranking kind for a built-in contact source id. Unknown sources keep
/// the saved-address-book behavior established before source kinds existed.
fn contact_source_kind(source: &ContactSourceId) -> &'static str {
    if source.as_str() == "google.other" {
        OTHER_CONTACT_KIND
    } else {
        SAVED_CONTACT_KIND
    }
}

fn ensure_configured_account(state: &super::AppState, account: &AccountId) -> Result<(), String> {
    if state
        .config_doc
        .accounts
        .iter()
        .any(|configured| &configured.id == account)
    {
        Ok(())
    } else {
        Err(format!("unknown account {account}"))
    }
}

/// Every configured account address and IMAP login alias. These are never
/// default suggestions.
fn self_identity_keys(accounts: &[Account]) -> HashSet<AddressKey> {
    let mut keys = HashSet::new();
    for account in accounts {
        add_identity(&mut keys, &account.email);
        if let Some((username, _, _, _)) = account.imap_smtp() {
            add_identity(&mut keys, username);
        }
    }
    keys
}

fn add_identity(keys: &mut HashSet<AddressKey>, value: &str) {
    let value = value.trim();
    if !value.contains('@') {
        return;
    }
    if let Ok(key) = normalize_email(value) {
        keys.insert(key);
    }
}
