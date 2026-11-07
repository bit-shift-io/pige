//! Migration v6: durable account generations, cross-process leases, pending
//! authentication references, and generation-fenced job checkpoints.
//!
//! These tables are deliberately small and carry **no secrets**:
//!
//! - `account_generation` is the durable fencing record for one account. It is
//!   incremented when a session is invalidated (for example the account is
//!   removed or re-authenticated) so a refresh that started earlier can never
//!   write with a stale generation. A removed account keeps its row as a
//!   tombstone; the next free number is one past the stored value.
//! - `account_lease` is the cross-process lease/fencing token. Only one holder
//!   may own an account's gate at a time; the monotonically increasing
//!   `fencing` value lets a durable write reject a writer that lost its lease.
//! - `pending_auth` records an in-flight interactive authentication attempt by
//!   referencing its protected credential entry (`credential_ref`). The secret
//!   itself never enters SQLite.
//! - `job_checkpoints` stores a resumable sync cursor together with the account
//!   generation that produced it, so a checkpoint written from a stale
//!   generation is rejected instead of silently advancing a job.
//!
//! No foreign keys point at `accounts`: a pending attempt for a brand-new
//! account exists before any visible account row, and a generation tombstone
//! must outlive the account it fences.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS account_generation (
             account_id TEXT PRIMARY KEY,
             generation INTEGER NOT NULL DEFAULT 1
         );

         CREATE TABLE IF NOT EXISTS account_lease (
             account_id TEXT PRIMARY KEY,
             holder TEXT NOT NULL,
             fencing INTEGER NOT NULL,
             expires_at INTEGER NOT NULL
         );

         CREATE TABLE IF NOT EXISTS pending_auth (
             attempt_id TEXT PRIMARY KEY,
             account_id TEXT,
             provider TEXT NOT NULL,
             credential_ref TEXT NOT NULL,
             generation INTEGER NOT NULL DEFAULT 1,
             created_at INTEGER NOT NULL,
             expires_at INTEGER NOT NULL
         );

         CREATE INDEX IF NOT EXISTS pending_auth_expires ON pending_auth(expires_at);

         CREATE TABLE IF NOT EXISTS job_checkpoints (
             job_key TEXT PRIMARY KEY,
             generation INTEGER NOT NULL,
             cursor TEXT NOT NULL
         );",
    )?;
    Ok(())
}
