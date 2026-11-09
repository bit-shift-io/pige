//! Contacts API (`/contacts/v4/contacts`) — read access.

use crate::error::Result;
use crate::transport::{Doer, Request};
use serde::{Deserialize, Serialize};

/// A contact email entry (plaintext metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContactEmail {
    /// Contact-email entry ID.
    #[serde(rename = "ID")]
    pub id: String,
    /// Email address.
    #[serde(default)]
    pub email: String,
    /// Display name for this email entry.
    #[serde(default)]
    pub name: String,
    /// ID of the contact this email belongs to.
    #[serde(rename = "ContactID", default)]
    pub contact_id: String,
    /// Policy/type/default fields this read path does not interpret. Retained
    /// for the later recipient-policy work rather than discarded at parse time.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One vCard/card entry as stored by Proton. The encrypted card data is only
/// retained by the adapter for future recipient-policy work; it is never part
/// of a public read-only contact DTO.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContactCard {
    /// Card type (for example `CLEAR` or `ENCRYPTED`).
    #[serde(rename = "Type", default)]
    pub card_type: String,
    /// Card data (may be encrypted).
    #[serde(default)]
    pub data: String,
    /// Detached signature over the card data, if present.
    #[serde(default)]
    pub signature: String,
}

/// A contact (name + its email entries).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Contact {
    /// Contact ID.
    #[serde(rename = "ID")]
    pub id: String,
    /// Contact display name.
    #[serde(default)]
    pub name: String,
    /// Email entries belonging to this contact.
    #[serde(rename = "ContactEmails", default)]
    pub emails: Vec<ContactEmail>,
    /// Stable contact UID, if exposed by the API.
    #[serde(rename = "UID", default)]
    pub uid: String,
    /// Encrypted/clear card payloads. Kept adapter-private.
    #[serde(default)]
    pub cards: Vec<ContactCard>,
    /// Remaining provider fields (labels, sizes, timestamps, policy hints).
    /// Retained adapter-private so P4 can read policy without a re-fetch.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct ContactsResp {
    #[serde(rename = "Contacts", default)]
    contacts: Vec<Contact>,
    #[serde(rename = "Total", default)]
    total: u32,
}

#[derive(Deserialize)]
struct EmailsResp {
    #[serde(rename = "ContactEmails", default)]
    contact_emails: Vec<ContactEmail>,
}

/// List contacts (paged).
pub async fn list<D: Doer>(d: &D, page: u32, page_size: u32) -> Result<(u32, Vec<Contact>)> {
    let req = Request::get("/contacts/v4/contacts")
        .query("Page", page.to_string())
        .query("PageSize", page_size.to_string());
    let r: ContactsResp = d.decode(req).await?;
    Ok((r.total, r.contacts))
}

/// List contact email entries, optionally filtered by an email address.
pub async fn list_emails<D: Doer>(
    d: &D,
    page: u32,
    page_size: u32,
    email: Option<&str>,
) -> Result<Vec<ContactEmail>> {
    let mut req = Request::get("/contacts/v4/contacts/emails")
        .query("Page", page.to_string())
        .query("PageSize", page_size.to_string());
    if let Some(e) = email {
        req = req.query("Email", e);
    }
    let r: EmailsResp = d.decode(req).await?;
    Ok(r.contact_emails)
}
