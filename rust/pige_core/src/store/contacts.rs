//! Source-address-book contacts, kept separate from mail-derived evidence.
//!
//! R1 establishes the durable tables so native contact adapters can land later
//! without reshaping the database. Deleting or denying a contact source never
//! touches `correspondence_evidence`: learned suggestions remain independent.
//!
//! R2 keeps the indexed recipient projection in sync here: contact edits refresh
//! only the affected address keys, and deleting a source removes its contact
//! signals while leaving mail-derived signals intact.

use rusqlite::params;

use crate::account::AccountId;
use crate::contacts::normalize::normalize_address;
use crate::contacts::rank::SAVED_CONTACT_KIND;
use crate::contacts::types::AddressKey;
use crate::mail::Address;
use crate::providers::{ContactSourceId, RemoteContactId};

use super::correspondents::refresh_recipient_index_in;
use super::{Store, StoreError};

/// One address belonging to a remote contact source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedContactEmail {
    pub source: ContactSourceId,
    pub remote_contact: RemoteContactId,
    pub name: Option<String>,
    pub address: Address,
}

impl Store {
    /// Record (or rename) one contact source for an account. Saved address
    /// books are the default; Google-style Other contacts use
    /// [`Self::upsert_contact_source_with_kind`].
    pub fn upsert_contact_source(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        display_name: Option<&str>,
    ) -> Result<(), StoreError> {
        self.upsert_contact_source_with_kind(account, source, display_name, SAVED_CONTACT_KIND)
    }

    /// Record a contact source with an explicit kind (`saved` or `other`).
    /// Changing the kind reclassifies every address already cached for it.
    pub fn upsert_contact_source_with_kind(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        display_name: Option<&str>,
        kind: &str,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO contact_sources (account_id, source_id, display_name, kind)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, source_id) DO UPDATE SET
                 display_name = excluded.display_name,
                 kind = excluded.kind",
            params![account.0, source.as_str(), display_name, kind],
        )?;
        refresh_source_keys_in(&tx, account, source)?;
        tx.commit()?;
        Ok(())
    }

    /// Add one address to a source contact. Invalid addresses are rejected as a
    /// `Corrupt` value rather than silently stored under a bogus key.
    pub fn upsert_contact_email(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        remote_contact: &RemoteContactId,
        address: &Address,
    ) -> Result<(), StoreError> {
        let key = normalize_address(address)
            .map_err(|error| StoreError::corrupt("contacts.email", error.to_string()))?;
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO contact_emails
                 (account_id, source_id, remote_contact_id, address_key, email, display_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_id, source_id, remote_contact_id, address_key) DO UPDATE SET
                 email = excluded.email,
                 display_name = excluded.display_name",
            params![
                account.0,
                source.as_str(),
                remote_contact.as_str(),
                key.0,
                address.email.trim(),
                address.name,
            ],
        )?;
        refresh_recipient_index_in(&tx, account, &key)?;
        tx.commit()?;
        Ok(())
    }

    /// Remove a whole contact source (permission denial or deletion). Learned
    /// mail evidence is deliberately untouched; only the source's contact
    /// signals disappear from the projection.
    pub fn remove_contact_source(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
    ) -> Result<(), StoreError> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let keys = source_keys_in(&tx, account, source)?;
        tx.execute(
            "DELETE FROM contact_sources WHERE account_id = ?1 AND source_id = ?2",
            params![account.0, source.as_str()],
        )?;
        for key in keys {
            refresh_recipient_index_in(&tx, account, &key)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every cached address from every source for one account.
    pub fn contact_emails(
        &self,
        account: &AccountId,
    ) -> Result<Vec<CachedContactEmail>, StoreError> {
        let conn = self.connection();
        let mut statement = conn.prepare(
            "SELECT source_id, remote_contact_id, display_name, email
             FROM contact_emails
             WHERE account_id = ?1
             ORDER BY source_id, remote_contact_id, address_key",
        )?;
        let mut rows = statement.query([&account.0])?;
        let mut emails = Vec::new();
        while let Some(row) = rows.next()? {
            let name: Option<String> = row.get(2)?;
            emails.push(CachedContactEmail {
                source: ContactSourceId::from(row.get::<_, String>(0)?),
                remote_contact: RemoteContactId::from(row.get::<_, String>(1)?),
                address: Address::new(name.clone(), row.get::<_, String>(3)?),
                name,
            });
        }
        Ok(emails)
    }
}

fn source_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<Vec<AddressKey>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT address_key FROM contact_emails
         WHERE account_id = ?1 AND source_id = ?2
         ORDER BY address_key",
    )?;
    let mut rows = statement.query(params![account.0, source.as_str()])?;
    let mut keys = Vec::new();
    while let Some(row) = rows.next()? {
        keys.push(AddressKey(row.get(0)?));
    }
    Ok(keys)
}

fn refresh_source_keys_in(
    conn: &rusqlite::Connection,
    account: &AccountId,
    source: &ContactSourceId,
) -> Result<(), StoreError> {
    for key in source_keys_in(conn, account, source)? {
        refresh_recipient_index_in(conn, account, &key)?;
    }
    Ok(())
}
