//! Migration v10: recoverable backend-switch intents.
//!
//! A backend switch must survive a crash between activating the new credential
//! and publishing the new account configuration. The intent is deliberately
//! **non-secret**: it records the account id, the old and target account shapes
//! (which contain no passwords or tokens), the account generation, and the
//! *references* to the protected credential-store entries involved. The
//! credentials themselves stay in the platform credential store.
//!
//! One row per account: starting a switch replaces (or is refused while) a prior
//! unfinished intent. `ON DELETE CASCADE` removes the intent if the account is
//! removed or dropped by a restore.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS switch_intents (
             account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
             target_account TEXT NOT NULL,
             previous_account TEXT NOT NULL,
             credential_ref TEXT NOT NULL,
             backup_ref TEXT,
             persistence TEXT NOT NULL,
             previous_present INTEGER NOT NULL,
             previous_persistent INTEGER NOT NULL,
             phase INTEGER NOT NULL DEFAULT 0,
             created_at INTEGER NOT NULL
         );",
    )?;
    Ok(())
}
