//! Account persistence.
//!
//! The configuration document (`config::ConfigDocument`) is the source of
//! truth for accounts; this table is a derived mirror kept so `ON DELETE
//! CASCADE` and account-scoped joins keep working. Every account change in the
//! API persists the document first and then calls [`Store::sync_accounts`].

use rusqlite::{Connection, Row, params};

use crate::account::{Account, AccountId, AuthKind, Security, ServerConfig};

use super::{Store, StoreError};

const COLUMNS: &str = "id, label, email, display_name, username, \
     imap_host, imap_port, imap_security, \
     smtp_host, smtp_port, smtp_security, auth_kind, sort_order";

impl Store {
    pub fn upsert_account(&self, account: &Account) -> Result<(), StoreError> {
        upsert_account_on(&self.connection(), account)
    }

    /// Bring the account mirror in line with the configuration document without
    /// touching the cached mail of accounts that survive: only ids absent from
    /// `accounts` are deleted (which cascades their cache), and every supplied
    /// account is upserted. A rename or edit keeps its id and therefore its
    /// folders, messages, threads, bodies, attachments, and outbox rows.
    pub fn sync_accounts(&self, accounts: &[Account]) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let keep: std::collections::HashSet<&str> =
            accounts.iter().map(|account| account.id.0.as_str()).collect();
        for existing in accounts_on(&tx)? {
            if !keep.contains(existing.id.0.as_str()) {
                tx.execute("DELETE FROM accounts WHERE id = ?1", [&existing.id.0])?;
            }
        }
        for account in accounts {
            upsert_account_on(&tx, account)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every account in navigation order.
    pub fn accounts(&self) -> Result<Vec<Account>, StoreError> {
        let conn = self.connection();
        accounts_on(&conn)
    }

    /// Delete an account and, through `ON DELETE CASCADE`, every folder,
    /// message, thread, cached body, attachment, and outbox row that belongs to
    /// it. The FTS triggers remove the account's search entries as messages go.
    pub fn remove_account(&self, id: &AccountId) -> Result<(), StoreError> {
        let conn = self.connection();
        conn.execute("DELETE FROM accounts WHERE id = ?1", [&id.0])?;
        Ok(())
    }
}

pub(crate) fn upsert_account_on(conn: &Connection, account: &Account) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO accounts (
             id, label, email, display_name, username,
             imap_host, imap_port, imap_security,
             smtp_host, smtp_port, smtp_security, auth_kind, sort_order
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(id) DO UPDATE SET
             label = excluded.label,
             email = excluded.email,
             display_name = excluded.display_name,
             username = excluded.username,
             imap_host = excluded.imap_host,
             imap_port = excluded.imap_port,
             imap_security = excluded.imap_security,
             smtp_host = excluded.smtp_host,
             smtp_port = excluded.smtp_port,
             smtp_security = excluded.smtp_security,
             auth_kind = excluded.auth_kind,
             sort_order = excluded.sort_order",
        params![
            account.id.0,
            account.label,
            account.email,
            account.display_name,
            account.username,
            account.imap.host,
            account.imap.port,
            account.imap.security.as_str(),
            account.smtp.host,
            account.smtp.port,
            account.smtp.security.as_str(),
            account.auth.as_str(),
            account.sort_order,
        ],
    )?;
    Ok(())
}

pub(crate) fn accounts_on(conn: &Connection) -> Result<Vec<Account>, StoreError> {
    let sql = format!("SELECT {COLUMNS} FROM accounts ORDER BY sort_order, label COLLATE NOCASE");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;

    let mut accounts = Vec::new();
    while let Some(row) = rows.next()? {
        accounts.push(account_from_row(row)?);
    }
    Ok(accounts)
}

fn account_from_row(row: &Row<'_>) -> Result<Account, StoreError> {
    let imap_security: String = row.get(7)?;
    let smtp_security: String = row.get(10)?;
    let auth_kind: String = row.get(11)?;

    Ok(Account {
        id: AccountId(row.get(0)?),
        label: row.get(1)?,
        email: row.get(2)?,
        display_name: row.get(3)?,
        username: row.get(4)?,
        imap: ServerConfig {
            host: row.get(5)?,
            port: row.get(6)?,
            security: Security::from_db(&imap_security)
                .ok_or_else(|| StoreError::corrupt("accounts.imap_security", imap_security))?,
        },
        smtp: ServerConfig {
            host: row.get(8)?,
            port: row.get(9)?,
            security: Security::from_db(&smtp_security)
                .ok_or_else(|| StoreError::corrupt("accounts.smtp_security", smtp_security))?,
        },
        auth: AuthKind::from_db(&auth_kind)
            .ok_or_else(|| StoreError::corrupt("accounts.auth_kind", auth_kind))?,
        sort_order: row.get(12)?,
    })
}
