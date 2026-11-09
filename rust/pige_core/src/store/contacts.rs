//! Source-address-book contacts, kept separate from mail-derived evidence.
//!
//! R1 establishes the durable tables so native contact adapters can land later
//! without reshaping the database. Deleting or denying a contact source never
//! touches `correspondence_evidence`: learned suggestions remain independent.

use rusqlite::params;

use crate::account::AccountId;
use crate::contacts::normalize::normalize_address;
use crate::mail::Address;
use crate::providers::{ContactSourceId, RemoteContactId};

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
    /// Record (or rename) one contact source for an account.
    pub fn upsert_contact_source(
        &self,
        account: &AccountId,
        source: &ContactSourceId,
        display_name: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO contact_sources (account_id, source_id, display_name)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, source_id) DO UPDATE SET
                 display_name = excluded.display_name",
            params![account.0, source.as_str(), display_name],
        )?;
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
        self.connection().execute(
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
