//! Migration v13: bounded top-N recipient lookup.
//!
//! v12 answered a prefix keystroke by materializing every matching token before
//! sorting, so a one-character prefix could scan a whole address book. This
//! migration makes the ranking order indexable:
//!
//! - `recipient_index.tier` stores the precomputed source tier
//!   (deliberate/outgoing, saved, Other, incoming) so the query can order by it.
//! - `recipient_index_account_rank` orders one account's rows by the complete
//!   post-tier rank, letting SQLite stop after `LIMIT` candidates instead of
//!   sorting the whole account.
//! - `recipient_index_email_folded`/`recipient_index_name_folded` make the
//!   exact-match branch an equality lookup.
//!
//! Existing projection rows are backfilled with their tier; the rank indexes are
//! rebuilt by SQLite as part of `CREATE INDEX`.

use rusqlite::Connection;

use crate::store::StoreError;

pub fn apply(conn: &Connection) -> Result<(), StoreError> {
    if !has_column(conn, "recipient_index", "tier")? {
        conn.execute_batch(
            "ALTER TABLE recipient_index ADD COLUMN tier INTEGER NOT NULL DEFAULT 3;",
        )?;
    }

    conn.execute_batch(
        "UPDATE recipient_index
         SET tier = CASE
             WHEN outgoing = 1 OR selected = 1 THEN 0
             WHEN saved_contact = 1 THEN 1
             WHEN other_contact = 1 THEN 2
             ELSE 3 END;

         CREATE INDEX IF NOT EXISTS recipient_index_account_rank
             ON recipient_index(account_id, tier, last_seen DESC, frequency DESC, address_key);

         CREATE INDEX IF NOT EXISTS recipient_index_email_folded
             ON recipient_index(account_id, email_folded);

         CREATE INDEX IF NOT EXISTS recipient_index_name_folded
             ON recipient_index(account_id, name_folded);",
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
