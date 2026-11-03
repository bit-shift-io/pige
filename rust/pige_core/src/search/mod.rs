//! Local full-text search over synced headers and cached body text.
//!
//! Search is intentionally synchronous and cheap; shells run it off their UI
//! thread. The FTS query is built from quoted terms so user punctuation can
//! never turn into an FTS5 operator or SQL fragment.

use crate::account::AccountId;
use crate::mail::{FolderId, ThreadId};
use crate::store::{Store, StoreError};

/// Which part of the cache a search covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    AllAccounts,
    Account(AccountId),
    Folder(FolderId),
}

/// One conversation-level search result. A thread appears at most once, using
/// its newest matching message for display fields and ordering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub thread_id: ThreadId,
    pub account_id: AccountId,
    pub subject: String,
    pub sender: String,
    pub date: i64,
}

/// Search cached mail and return one hit per conversation, newest first.
pub fn search(
    store: &Store,
    query: &str,
    scope: &SearchScope,
    limit: u32,
) -> Result<Vec<SearchHit>, StoreError> {
    store.search(query, scope, limit)
}

/// Turn a user query into a safe FTS5 MATCH expression.
///
/// Terms split on whitespace, FTS operator punctuation is removed, and each
/// surviving term is wrapped in double quotes. FTS5 treats adjacent phrases as
/// an implicit AND, so the terms combine conjunctively without any user text
/// escaping into the query grammar. Quoting also makes words such as `OR`
/// literal text rather than boolean operators. An input with no usable term
/// produces an empty string, which callers treat as "no hits".
pub fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .filter_map(|term| {
            let cleaned: String = term
                .chars()
                .filter(|character| !matches!(character, '"' | '*' | '(' | ')' | ':' | '^'))
                .collect();
            (!cleaned.is_empty()).then(|| format!("\"{cleaned}\""))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_terms_are_quoted_and_joined() {
        assert_eq!(fts_query("foo\" OR *"), "\"foo\" \"OR\"");
        assert_eq!(fts_query("  alpha   beta "), "\"alpha\" \"beta\"");
        assert_eq!(fts_query(""), "");
        assert_eq!(fts_query("* ( ) ^"), "");
    }
}
