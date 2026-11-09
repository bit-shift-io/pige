//! Migration v11: contacts and replay-safe correspondent evidence.
//!
//! Learned correspondents are stored as *evidence*: one row per native
//! message/operation, with the roles in which each address appeared. Aggregated
//! suggestions in `contact_addresses` are recomputed from that evidence, so a
//! repeated history page, an IMAP UID reset, a label copy, or a Sent replay
//! cannot multiply a frequency.
//!
//! Correspondence evidence deliberately has **no foreign key to `messages`**.
//! Remote deletion or local cache eviction removes content but must never erase
//! what the user has already learned. Removing an account still cascades all of
//! its contact and evidence rows through `accounts`.
//!
//! `contact_sources`/`contact_emails` are the durable home for later remote
//! address-book syncing. They are independent of learned evidence: denying or
//! deleting a contact source must not delete mail-derived suggestions.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contact_sources (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             source_id TEXT NOT NULL,
             display_name TEXT,
             PRIMARY KEY (account_id, source_id)
         );

         CREATE TABLE IF NOT EXISTS contact_emails (
             account_id TEXT NOT NULL,
             source_id TEXT NOT NULL,
             remote_contact_id TEXT NOT NULL,
             address_key TEXT NOT NULL,
             email TEXT NOT NULL,
             display_name TEXT,
             PRIMARY KEY (account_id, source_id, remote_contact_id, address_key),
             FOREIGN KEY (account_id, source_id)
                 REFERENCES contact_sources(account_id, source_id) ON DELETE CASCADE
         );

         CREATE INDEX IF NOT EXISTS contact_emails_address
             ON contact_emails(account_id, address_key);

         CREATE TABLE IF NOT EXISTS correspondence_evidence (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             evidence_id TEXT NOT NULL,
             origin TEXT NOT NULL,
             fingerprint TEXT,
             observed_at INTEGER NOT NULL DEFAULT 0,
             last_seen INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (account_id, evidence_id)
         );

         CREATE INDEX IF NOT EXISTS correspondence_evidence_fingerprint
             ON correspondence_evidence(account_id, fingerprint);

         CREATE TABLE IF NOT EXISTS evidence_addresses (
             account_id TEXT NOT NULL,
             evidence_id TEXT NOT NULL,
             address_key TEXT NOT NULL,
             role TEXT NOT NULL,
             email TEXT NOT NULL,
             display_name TEXT,
             outgoing INTEGER NOT NULL DEFAULT 0,
             trust TEXT NOT NULL,
             last_seen INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (account_id, evidence_id, address_key, role),
             FOREIGN KEY (account_id, evidence_id)
                 REFERENCES correspondence_evidence(account_id, evidence_id)
                 ON DELETE CASCADE
         );

         CREATE INDEX IF NOT EXISTS evidence_addresses_lookup
             ON evidence_addresses(account_id, address_key);

         CREATE TABLE IF NOT EXISTS contact_addresses (
             account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
             address_key TEXT NOT NULL,
             email TEXT NOT NULL,
             display_name TEXT,
             trust TEXT NOT NULL,
             outgoing INTEGER NOT NULL DEFAULT 0,
             last_seen INTEGER NOT NULL DEFAULT 0,
             evidence_count INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (account_id, address_key)
         );

         CREATE INDEX IF NOT EXISTS contact_addresses_rank
             ON contact_addresses(account_id, last_seen DESC, evidence_count DESC);

         CREATE TABLE IF NOT EXISTS contact_learning_state (
             account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
             existing_cache_cursor TEXT,
             updated_at INTEGER NOT NULL DEFAULT 0
         );",
    )?;
    Ok(())
}
