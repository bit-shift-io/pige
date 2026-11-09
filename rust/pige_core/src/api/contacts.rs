//! Local-only recipient autocomplete for compose.
//!
//! Lookup reads the indexed SQLite projection; it never opens a provider and
//! never logs the query text. A deliberate selection is the only write path
//! exposed here: arbitrary keystrokes are not evidence.

use std::collections::HashSet;
use std::time::Duration;

use crate::account::{Account, AccountId};
use crate::contacts::normalize::{normalize_address, normalize_email};
use crate::contacts::rank::{
    bound_query_text, bounded_limit, folded_query, query_tokens, rank_candidates,
};
use crate::contacts::types::{AddressKey, RecipientQuery, RecipientSuggestion};
use crate::mail::Address;
use crate::providers::OperationContext;

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
