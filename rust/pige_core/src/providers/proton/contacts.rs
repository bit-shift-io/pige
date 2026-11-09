//! Proton saved contacts: names and email addresses only.
//!
//! Proton exposes a full contact snapshot, not an incremental delta stream, so
//! the adapter advertises `incremental_sync = false`. Each new cycle restarts
//! at page zero with [`ContactPageKind::SnapshotStart`]; R6 only retires absent
//! contacts when the final page of that snapshot completes. Encrypted card
//! payloads are retained privately for the later recipient-policy work and are
//! never projected into the public read-only DTO.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::account::PROTON_CONTACTS_SOURCE_ID;
use crate::mail::Address;
use crate::providers::{
    AddressBookService, ContactChange, ContactPage, ContactPageKind, ContactRecord,
    ContactSourceId, ErrorKind, OperationContext, ProviderCursor, ProviderError, RemoteContactId,
};

use proton_core::api;
use proton_core::api::contacts::{Contact, ContactCard, ContactEmail};

use super::client::ProtonClient;

const CURSOR_VERSION: u32 = 1;
const PAGE_SIZE: u32 = 100;

/// Encrypted/clear card material for one contact. This stays inside the
/// adapter for P4's recipient-policy work; only names and emails leave through
/// [`ContactRecord`].
#[derive(Debug, Clone, Default)]
#[allow(dead_code)] // retained for P4 recipient policy; not part of this DTO
pub(crate) struct ProtonContactPrivate {
    pub uid: String,
    pub cards: Vec<ContactCard>,
    /// Full email entries, including provider type/default/policy fields that
    /// the public name/address projection must not lose.
    pub emails: Vec<ContactEmail>,
}

/// Read-only Proton saved contacts.
pub struct ProtonAddressBook {
    client: ProtonClient,
    private: HashMap<String, ProtonContactPrivate>,
}

impl ProtonAddressBook {
    pub fn new(client: ProtonClient) -> Self {
        Self {
            client,
            private: HashMap::new(),
        }
    }

    /// Adapter-private card material for one remote contact. Not part of the
    /// public provider contract.
    #[allow(dead_code)]
    pub(crate) fn private_contact(&self, id: &str) -> Option<&ProtonContactPrivate> {
        self.private.get(id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ContactsCursor {
    v: u32,
    page: u32,
    page_size: u32,
    total: Option<u32>,
    /// The previous cycle reached end-of-enumeration. A full snapshot has no
    /// delta token, so the next call starts a fresh generation at page zero.
    complete: bool,
}

impl ContactsCursor {
    fn fresh() -> Self {
        Self {
            v: CURSOR_VERSION,
            page: 0,
            page_size: PAGE_SIZE,
            total: None,
            complete: false,
        }
    }

    fn encode(&self) -> Result<ProviderCursor, ProviderError> {
        let payload = serde_json::to_string(self).map_err(|error| {
            ProviderError::transient("could not encode the Proton contacts cursor")
                .with_source_detail(error.to_string())
        })?;
        Ok(ProviderCursor::new(CURSOR_VERSION, payload))
    }

    fn decode(cursor: &ProviderCursor) -> Result<Self, ProviderError> {
        if cursor.version != CURSOR_VERSION {
            return Err(ProviderError::cursor_invalid(
                "the Proton contacts cursor version is not supported",
            ));
        }
        let state: Self = serde_json::from_str(&cursor.payload).map_err(|error| {
            ProviderError::cursor_invalid("the Proton contacts cursor is malformed")
                .with_source_detail(error.to_string())
        })?;
        if state.v != CURSOR_VERSION || state.page_size == 0 {
            return Err(ProviderError::cursor_invalid(
                "the Proton contacts cursor is not usable",
            ));
        }
        Ok(state)
    }
}

#[async_trait]
impl AddressBookService for ProtonAddressBook {
    async fn next_page(
        &mut self,
        source: &ContactSourceId,
        cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        ctx.ensure_active()?;
        if source.as_str() != PROTON_CONTACTS_SOURCE_ID {
            return Err(ProviderError::new(
                ErrorKind::Unsupported,
                format!("Proton contact source {}", source.as_str()),
            ));
        }
        let mut state = match cursor {
            Some(cursor) => ContactsCursor::decode(&cursor)?,
            None => ContactsCursor::fresh(),
        };
        // A completed full snapshot is not a resumable delta token. Start a new
        // snapshot generation rather than skipping the first page.
        if state.complete {
            state = ContactsCursor::fresh();
        }
        let first_page = state.page == 0;
        if first_page {
            // A new full snapshot supersedes the previous cycle's retained
            // card material; keep only what this enumeration reports.
            self.private.clear();
        }
        let http = self.client.request_http(ctx).await?;
        let (total, contacts) =
            http.finish(api::contacts::list(&http, state.page, state.page_size).await)?;
        ctx.ensure_active()?;

        let mut changes = Vec::with_capacity(contacts.len());
        for contact in contacts {
            let remote = RemoteContactId::new(contact.id.clone());
            self.private.insert(
                contact.id.clone(),
                ProtonContactPrivate {
                    uid: contact.uid.clone(),
                    cards: contact.cards.clone(),
                    emails: contact.emails.clone(),
                },
            );
            changes.push(ContactChange::Upsert(contact_record(remote, &contact)));
        }

        let page_end = u64::from(state.page.saturating_add(1)) * u64::from(state.page_size);
        let at_end = page_end >= u64::from(total);
        // An empty page before the reported total means the offset listing
        // shifted under this snapshot (or the server miscounted). Reporting it
        // as complete would let R6 sweep the whole address book, so retry the
        // page instead of advancing a checkpoint.
        if changes.is_empty() && !at_end {
            return Err(ProviderError::transient(
                "the Proton contacts page was empty before the reported total",
            ));
        }
        let has_more = !at_end;
        let checkpoint = ContactsCursor {
            v: CURSOR_VERSION,
            page: state.page.saturating_add(1),
            page_size: state.page_size,
            total: Some(total),
            complete: !has_more,
        };
        Ok(ContactPage {
            changes,
            checkpoint: checkpoint.encode()?,
            has_more,
            kind: if first_page {
                ContactPageKind::SnapshotStart
            } else {
                ContactPageKind::Snapshot
            },
        })
    }
}

fn contact_record(remote: RemoteContactId, contact: &Contact) -> ContactRecord {
    let mut names = Vec::new();
    if !contact.name.trim().is_empty() {
        names.push(contact.name.trim().to_string());
    }
    for email in &contact.emails {
        let name = email.name.trim();
        if !name.is_empty() && !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    let emails = contact
        .emails
        .iter()
        .filter(|email| !email.email.trim().is_empty())
        .map(|email| {
            let name = email.name.trim();
            Address::new(
                (!name.is_empty() && name != email.email).then(|| name.to_string()),
                email.email.trim(),
            )
        })
        .collect();
    ContactRecord {
        remote_id: remote,
        // Proton contact versions are not exposed as a delta token here.
        version: None,
        names,
        emails,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_cards_and_email_policy_are_retained() {
        let contact: Contact = serde_json::from_value(serde_json::json!({
            "ID": "contact-1",
            "Name": "Alice Example",
            "UID": "uid-1",
            "ContactEmails": [{
                "ID": "email-1",
                "Email": "alice@example.com",
                "Name": "Alice",
                "ContactID": "contact-1",
                "Type": [1, 2],
                "Defaults": 1
            }],
            "Cards": [{
                "Type": "ENCRYPTED",
                "Data": "synthetic-ciphertext",
                "Signature": "synthetic-signature"
            }],
            "LabelIDs": ["label-1"]
        }))
        .expect("contact fixture");

        let record = contact_record(RemoteContactId::new("contact-1"), &contact);
        assert_eq!(record.names, vec!["Alice Example", "Alice"]);
        assert_eq!(record.emails[0].email, "alice@example.com");

        let private = ProtonContactPrivate {
            uid: contact.uid.clone(),
            cards: contact.cards.clone(),
            emails: contact.emails.clone(),
        };
        assert_eq!(private.cards[0].card_type, "ENCRYPTED");
        assert_eq!(private.cards[0].data, "synthetic-ciphertext");
        assert!(private.emails[0].extra.contains_key("Type"));
        assert!(private.emails[0].extra.contains_key("Defaults"));
        assert!(contact.extra.contains_key("LabelIDs"));
    }
}
