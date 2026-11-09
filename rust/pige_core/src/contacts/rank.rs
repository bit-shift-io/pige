//! Deterministic, bounded ranking for local recipient suggestions.
//!
//! Lookup is token-prefix based. Tokens are case-folded for matching only; the
//! delivered [`Address`] is never rewritten. Ranking is a total order so the
//! same store always yields the same suggestions:
//!
//! 1. deliberate selection / accepted outgoing
//! 2. saved contacts
//! 3. Other contacts
//! 4. incoming-only observations
//!
//! Within a tier an exact name/address match wins, then the current sending
//! account, the latest meaningful interaction, a capped frequency, and finally
//! the deterministic normalized address key.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::account::AccountId;
use crate::mail::Address;

use super::types::{AddressKey, RecipientSuggestion, SuggestionSource};

/// Public lookups never return more than this many rows.
pub const MAX_SUGGESTIONS: usize = 20;
/// Public query text is truncated to this many Unicode scalar values.
pub const MAX_QUERY_CHARS: usize = 256;
/// Frequency is bounded so an old, noisy correspondent cannot dominate forever.
pub const MAX_FREQUENCY: u32 = 100;

/// Contact-source kinds the adapters use to distinguish Google's saved people
/// from Other contacts. Any other kind is treated as a saved address book.
pub const SAVED_CONTACT_KIND: &str = "saved";
pub const OTHER_CONTACT_KIND: &str = "other";

/// One projection row, before exact-address rows from several accounts are
/// merged into a [`RecipientSuggestion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientCandidate {
    pub account: AccountId,
    pub key: AddressKey,
    pub address: Address,
    pub saved: bool,
    pub other: bool,
    pub outgoing: bool,
    pub selected: bool,
    pub incoming: bool,
    pub low_trust_only: bool,
    pub last_seen: i64,
    pub frequency: u32,
}

/// Clip query text to [`MAX_QUERY_CHARS`] without splitting a UTF-8 character.
pub fn bound_query_text(text: &str) -> String {
    text.chars().take(MAX_QUERY_CHARS).collect()
}

/// Cap a requested limit. `0` stays `0` (no results).
pub fn bounded_limit(limit: u16) -> usize {
    usize::from(limit.min(MAX_SUGGESTIONS as u16))
}

/// Fold the whole query for exact-match comparison.
pub fn folded_query(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Split user input into case-folded prefix tokens.
///
/// Every token is kept: a query is only answered with suggestions that match the
/// full input. The SQL statement stays bounded because the public entry point
/// truncates query text to [`MAX_QUERY_CHARS`] first, so at most ~128 one-character
/// tokens (each taking two bind parameters) can reach the query builder.
pub fn query_tokens(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut tokens = Vec::new();
    for raw in text.split_whitespace() {
        let token = raw
            .trim_matches(|character: char| {
                matches!(
                    character,
                    '<' | '>' | '"' | ',' | ';' | '(' | ')' | '[' | ']'
                )
            })
            .to_lowercase();
        if token.is_empty() || !seen.insert(token.clone()) {
            continue;
        }
        tokens.push(token);
    }
    tokens
}

/// All search tokens for one suggestion: the full address, its local part and
/// its domain, plus word/section tokens from the display name.
pub fn recipient_search_tokens(address: &Address) -> Vec<String> {
    let mut tokens = BTreeSet::new();
    let email = address.email.trim().to_lowercase();
    if !email.is_empty() {
        tokens.insert(email.clone());
    }
    if let Some((local, domain)) = email.rsplit_once('@') {
        add_word(&mut tokens, local);
        add_word(&mut tokens, domain);
    }
    if let Some(name) = address.name.as_deref() {
        for word in name.split_whitespace() {
            add_word(&mut tokens, word);
        }
    }
    tokens.into_iter().collect()
}

/// Exclusive upper bound for a prefix range (`token >= prefix AND token < end`).
pub fn prefix_upper_bound(prefix: &str) -> String {
    let mut upper = String::with_capacity(prefix.len() + 4);
    upper.push_str(prefix);
    // U+10FFFF cannot appear in a user-visible address/name token, and every
    // UTF-8 string that starts with `prefix` sorts below `prefix + U+10FFFF`.
    upper.push('\u{10FFFF}');
    upper
}

/// The SQL for one account's bounded candidate page, shared by the real lookup
/// and its query-plan test.
///
/// The lookup is driven by `recipient_index_account_rank`, whose key is
/// `(account_id, tier, last_seen DESC, frequency DESC, address_key)`. After
/// fixing `account_id`, the `ORDER BY` is satisfied by that index, so SQLite
/// stops as soon as `LIMIT` candidates that also satisfy the token predicates
/// have been read. A one-character prefix therefore cannot materialize or sort
/// the whole token range.
///
/// `exact` selects the branch that also matches the whole query against the
/// folded email/name; the other branch excludes those rows. Each branch is
/// capped independently so the union always contains the account's true top
/// candidates for both groups.
///
/// Bind parameters: `?1` account id, `?2` folded query, then two parameters per
/// token (`?3` lower, `?4` upper, ...), then the cap.
pub(crate) fn recipient_account_search_sql(token_count: usize, exact: bool) -> String {
    let token_predicates = (0..token_count)
        .map(|slot| {
            format!(
                "AND EXISTS (SELECT 1 FROM recipient_tokens \
                 WHERE recipient_tokens.account_id = r.account_id \
                   AND recipient_tokens.address_key = r.address_key \
                   AND recipient_tokens.token >= ?{lower} \
                   AND recipient_tokens.token < ?{upper})",
                lower = 3 + slot * 2,
                upper = 4 + slot * 2,
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let cap = 3 + token_count * 2;

    if exact {
        // Equality lookups on the two folded-key indexes; the outer sort only
        // touches the (normally tiny) exact-match set.
        let select = |column: &str, index: &str| {
            format!(
                "SELECT r.account_id, r.address_key, r.email, r.display_name,
                        r.saved_contact, r.other_contact, r.outgoing, r.selected,
                        r.incoming, r.low_trust_only, r.last_seen, r.frequency, r.tier
                 FROM recipient_index r INDEXED BY {index}
                 WHERE r.account_id = ?1
                   AND r.low_trust_only = 0
                   AND r.{column} = ?2
                   {token_predicates}"
            )
        };
        format!(
            "SELECT * FROM (
                 {email_branch}
                 UNION
                 {name_branch}
             )
             ORDER BY tier ASC, last_seen DESC, frequency DESC, address_key ASC
             LIMIT ?{cap}",
            email_branch = select("email_folded", "recipient_index_email_folded"),
            name_branch = select("name_folded", "recipient_index_name_folded"),
        )
    } else {
        format!(
            "SELECT r.account_id, r.address_key, r.email, r.display_name,
                    r.saved_contact, r.other_contact, r.outgoing, r.selected,
                    r.incoming, r.low_trust_only, r.last_seen, r.frequency
             FROM recipient_index r INDEXED BY recipient_index_account_rank
             WHERE r.account_id = ?1
               AND r.low_trust_only = 0
               AND NOT (r.email_folded = ?2 OR r.name_folded = ?2)
               {token_predicates}
             ORDER BY r.tier ASC, r.last_seen DESC, r.frequency DESC, r.address_key ASC
             LIMIT ?{cap}"
        )
    }
}

/// Merge per-account rows into one suggestion per exact address, suppress
/// blocked/self identities, rank, and truncate.
pub fn rank_candidates(
    candidates: Vec<RecipientCandidate>,
    sending_account: &AccountId,
    folded_query: &str,
    blocked: &HashSet<String>,
    suppressed: &HashSet<AddressKey>,
    limit: usize,
) -> Vec<RecipientSuggestion> {
    if limit == 0 {
        return Vec::new();
    }

    let mut grouped: BTreeMap<AddressKey, Vec<RecipientCandidate>> = BTreeMap::new();
    for candidate in candidates {
        grouped
            .entry(candidate.key.clone())
            .or_default()
            .push(candidate);
    }

    let mut ranked = Vec::new();
    for (key, rows) in grouped {
        if blocked.contains(&key.0.to_lowercase()) || suppressed.contains(&key) {
            continue;
        }
        if rows.iter().all(|row| row.low_trust_only) {
            continue;
        }

        let saved = rows.iter().any(|row| row.saved);
        let other = rows.iter().any(|row| row.other);
        let outgoing = rows.iter().any(|row| row.outgoing);
        let selected = rows.iter().any(|row| row.selected);
        let incoming = rows.iter().any(|row| row.incoming);
        if !(saved || other || outgoing || selected || incoming) {
            continue;
        }

        let mut sources = Vec::new();
        for (flag, source) in [
            (saved, SuggestionSource::SavedContact),
            (other, SuggestionSource::OtherContact),
            (outgoing, SuggestionSource::Outgoing),
            (incoming, SuggestionSource::Incoming),
            (selected, SuggestionSource::Selected),
        ] {
            if flag {
                sources.push(source);
            }
        }

        let mut account_map = BTreeMap::new();
        for row in &rows {
            account_map.insert(row.account.0.clone(), row.account.clone());
        }
        let mut accounts: Vec<AccountId> = account_map.into_values().collect();
        if let Some(index) = accounts
            .iter()
            .position(|account| account == sending_account)
        {
            let sending = accounts.remove(index);
            accounts.insert(0, sending);
        }

        let chosen = chosen_row(&rows, sending_account);
        let chosen_name =
            chosen_name(&rows, sending_account).and_then(|row| row.address.name.clone());
        let exact = chosen.address.email.trim().to_lowercase() == folded_query
            || chosen_name
                .as_deref()
                .is_some_and(|name| name.trim().to_lowercase() == folded_query);

        let last_seen = rows.iter().map(|row| row.last_seen).max().unwrap_or(0);
        let frequency = rows
            .iter()
            .fold(0u32, |sum, row| sum.saturating_add(row.frequency))
            .min(MAX_FREQUENCY);
        let current = accounts.first() == Some(sending_account);
        let tier = if outgoing || selected {
            0
        } else if saved {
            1
        } else if other {
            2
        } else {
            3
        };

        ranked.push(Ranked {
            suggestion: RecipientSuggestion {
                address: Address::new(chosen_name, chosen.address.email.clone()),
                sources,
                accounts,
            },
            tier,
            exact,
            current,
            last_seen,
            frequency,
            key,
        });
    }

    ranked.sort_by(|left, right| {
        left.tier
            .cmp(&right.tier)
            .then_with(|| right.exact.cmp(&left.exact))
            .then_with(|| right.current.cmp(&left.current))
            .then_with(|| right.last_seen.cmp(&left.last_seen))
            .then_with(|| right.frequency.cmp(&left.frequency))
            .then_with(|| left.key.0.cmp(&right.key.0))
    });
    ranked.truncate(limit);
    ranked.into_iter().map(|ranked| ranked.suggestion).collect()
}

struct Ranked {
    suggestion: RecipientSuggestion,
    tier: u8,
    exact: bool,
    current: bool,
    last_seen: i64,
    frequency: u32,
    key: AddressKey,
}

fn source_priority(row: &RecipientCandidate) -> u8 {
    if row.saved {
        3
    } else if row.other {
        2
    } else if row.outgoing || row.selected {
        1
    } else {
        0
    }
}

fn better_row<'a>(
    candidate: &'a RecipientCandidate,
    current: &'a RecipientCandidate,
    sending_account: &AccountId,
) -> bool {
    let candidate_key = (
        source_priority(candidate),
        candidate.account == *sending_account,
        candidate.last_seen,
    );
    let current_key = (
        source_priority(current),
        current.account == *sending_account,
        current.last_seen,
    );
    match candidate_key.cmp(&current_key) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => candidate.account.0 < current.account.0,
    }
}

fn chosen_row<'a>(
    rows: &'a [RecipientCandidate],
    sending_account: &AccountId,
) -> &'a RecipientCandidate {
    let mut chosen = &rows[0];
    for candidate in &rows[1..] {
        if better_row(candidate, chosen, sending_account) {
            chosen = candidate;
        }
    }
    chosen
}

fn chosen_name<'a>(
    rows: &'a [RecipientCandidate],
    sending_account: &AccountId,
) -> Option<&'a RecipientCandidate> {
    let mut chosen: Option<&RecipientCandidate> = None;
    for candidate in rows {
        if candidate
            .address
            .name
            .as_deref()
            .is_none_or(|name| name.trim().is_empty())
        {
            continue;
        }
        match chosen {
            Some(current) if !better_row(candidate, current, sending_account) => {}
            _ => chosen = Some(candidate),
        }
    }
    chosen
}

fn add_word(tokens: &mut BTreeSet<String>, word: &str) {
    let folded = word.trim().to_lowercase();
    let trimmed = folded.trim_matches(|character: char| !character.is_alphanumeric());
    if trimmed.is_empty() {
        return;
    }
    tokens.insert(trimmed.to_string());
    for part in trimmed.split(|character: char| !character.is_alphanumeric()) {
        if !part.is_empty() {
            tokens.insert(part.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_is_bounded_on_character_boundaries() {
        let long = "é".repeat(300);
        let bounded = bound_query_text(&long);
        assert_eq!(bounded.chars().count(), MAX_QUERY_CHARS);
        assert_eq!(bounded, "é".repeat(MAX_QUERY_CHARS));
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }

    #[test]
    fn tokens_include_full_address_local_domain_and_name_parts() {
        let tokens = recipient_search_tokens(&Address::new(
            Some("Alice Smith".to_string()),
            "First.Last+News@Example.COM",
        ));
        for expected in [
            "first.last+news@example.com",
            "first.last+news",
            "first",
            "last",
            "news",
            "example.com",
            "example",
            "com",
            "alice",
            "smith",
        ] {
            assert!(tokens.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[test]
    fn prefix_bound_covers_every_continuation() {
        for token in ["a", "alice", "person099999@example.com"] {
            assert!(format!("{token}zzz") < prefix_upper_bound(token));
        }
    }

    #[test]
    fn exact_name_and_address_rank_above_prefixes_in_the_same_tier() {
        let sending = AccountId("sending".to_string());
        let candidate = |email: &str, name: Option<&str>, last_seen: i64| RecipientCandidate {
            account: sending.clone(),
            key: AddressKey(format!("{email}@example.com")),
            address: Address::new(name.map(str::to_string), format!("{email}@example.com")),
            saved: false,
            other: false,
            outgoing: false,
            selected: false,
            incoming: true,
            low_trust_only: false,
            last_seen,
            frequency: 1,
        };
        let ranked = rank_candidates(
            vec![
                candidate("alice2", None, 500),
                candidate("alice", Some("Alice"), 100),
                candidate("alice3", None, 400),
            ],
            &sending,
            "alice",
            &HashSet::new(),
            &HashSet::new(),
            10,
        );
        assert_eq!(ranked[0].address.email, "alice@example.com");
    }
}
