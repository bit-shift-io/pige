//! Google People saved contacts and Other contacts.
//!
//! The two sources are deliberately separate:
//!
//! * `google.saved` reads `people.connections.list` under
//!   `contacts.readonly`.
//! * `google.other` reads `otherContacts.list` under
//!   `contacts.other.readonly`.
//!
//! Neither source searches Google at compose time; R2 serves autocomplete from
//! the local projection. Read masks ask only for names, email addresses, and
//! deletion metadata. Other contacts do not support `organizations`, so that
//! field is never requested, even for saved contacts.
//!
//! The R6 snapshot path owns the destructive sweep. A full listing is emitted
//! page by page and only its final page reports `has_more == false`, after which
//! R6 retires contacts absent from the completed generation. Incremental People
//! pages report the people deleted since the previous token as `Delete` changes;
//! an expired token restarts a full sweep instead of surfacing a permanent
//! cursor error. Because the shared [`ContactPage`] contract cannot distinguish
//! "delta complete" from "full snapshot complete", an adapter-level delta is
//! followed by one verification snapshot before completion is reported. This is
//! conservative: it never sweeps on a partial delta.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::mail::Address;
use crate::providers::{
    AddressBookService, ContactChange, ContactPage, ContactRecord, ContactSourceId, ErrorKind,
    OperationContext, ProviderCursor, ProviderError, RemoteContactId,
};

use super::client::{GoogleClient, with_query};

/// The stable source id for People saved contacts.
pub const GOOGLE_SAVED_CONTACTS_SOURCE: &str = "google.saved";
/// The stable source id for People Other contacts.
pub const GOOGLE_OTHER_CONTACTS_SOURCE: &str = "google.other";

const CURSOR_VERSION: u32 = 1;
const PAGE_SIZE: u32 = 100;
const FIELD_MASK: &str = "names,emailAddresses,metadata";
const READ_SOURCE_TYPE_CONTACT: &str = "READ_SOURCE_TYPE_CONTACT";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Saved,
    Other,
}

impl SourceKind {
    fn from_source(source: &ContactSourceId) -> Result<Self, ProviderError> {
        match source.as_str() {
            GOOGLE_SAVED_CONTACTS_SOURCE => Ok(Self::Saved),
            GOOGLE_OTHER_CONTACTS_SOURCE => Ok(Self::Other),
            other => Err(ProviderError::unsupported(format!(
                "Google contact source {other}"
            ))),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Saved => GOOGLE_SAVED_CONTACTS_SOURCE,
            Self::Other => GOOGLE_OTHER_CONTACTS_SOURCE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Stage {
    /// Enumerate a complete snapshot generation.
    Snapshot,
    /// Apply incremental changes from a previous `nextSyncToken`.
    Delta,
    /// The delta completed. Run one full snapshot before reporting completion,
    /// so R6 never sweeps against a partial delta.
    Verify,
}

/// One source's opaque checkpoint. It carries everything the next request must
/// repeat: the source and its immutable read parameters, the page token, the
/// latest sync token, and the sweep generation used to diagnose a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PeopleCursor {
    v: u32,
    source: String,
    person_fields: String,
    read_source: String,
    page_size: u32,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default)]
    sync_token: Option<String>,
    sweep: u64,
    stage: Stage,
}

impl PeopleCursor {
    fn fresh(kind: SourceKind) -> Self {
        Self {
            v: CURSOR_VERSION,
            source: kind.id().to_string(),
            person_fields: FIELD_MASK.to_string(),
            read_source: READ_SOURCE_TYPE_CONTACT.to_string(),
            page_size: PAGE_SIZE,
            page_token: None,
            sync_token: None,
            sweep: 1,
            stage: Stage::Snapshot,
        }
    }

    fn restart(kind: SourceKind, sweep: u64) -> Self {
        Self {
            sweep: sweep.max(1),
            ..Self::fresh(kind)
        }
    }

    fn encode(self) -> Result<ProviderCursor, ProviderError> {
        let payload = serde_json::to_string(&self).map_err(|error| {
            ProviderError::transient("could not encode the Google contacts cursor")
                .with_source_detail(error.to_string())
        })?;
        Ok(ProviderCursor::new(CURSOR_VERSION, payload))
    }

    fn decode(cursor: &ProviderCursor, kind: SourceKind) -> Result<Self, ProviderError> {
        if cursor.version != CURSOR_VERSION {
            return Err(ProviderError::cursor_invalid(
                "the Google contacts cursor version is not supported",
            ));
        }
        let state: Self = serde_json::from_str(&cursor.payload).map_err(|error| {
            ProviderError::cursor_invalid("the Google contacts cursor is malformed")
                .with_source_detail(error.to_string())
        })?;
        if state.v != CURSOR_VERSION
            || state.source != kind.id()
            || state.person_fields != FIELD_MASK
            || state.read_source != READ_SOURCE_TYPE_CONTACT
            || state.page_size != PAGE_SIZE
            || state.sweep == 0
        {
            return Err(ProviderError::cursor_invalid(
                "the Google contacts cursor does not match this source",
            ));
        }
        Ok(state)
    }
}

/// Read-only People address book for one account.
///
/// `user` mirrors the Gmail service's user segment. The People API only accepts
/// `people/me`, which is what the application factory passes.
pub struct GoogleAddressBook {
    client: GoogleClient,
    user: String,
}

impl GoogleAddressBook {
    pub fn new(client: GoogleClient, user: impl Into<String>) -> Self {
        Self {
            client,
            user: user.into(),
        }
    }

    fn url(&self, kind: SourceKind) -> Result<Url, ProviderError> {
        let mut url = self.client.endpoints().people_base.clone();
        let mut segments = url.path_segments_mut().map_err(|_| {
            ProviderError::new(
                ErrorKind::Unsupported,
                "the Google People API base URL is unusable",
            )
        })?;
        segments.pop_if_empty();
        segments.push("v1");
        match kind {
            SourceKind::Saved => {
                segments.push("people");
                segments.push(&self.user);
                segments.push("connections");
            }
            SourceKind::Other => {
                segments.push("otherContacts");
            }
        }
        drop(segments);
        Ok(url)
    }

    async fn request(
        &self,
        kind: SourceKind,
        cursor: &PeopleCursor,
        ctx: &OperationContext,
    ) -> Result<ListResponse, ProviderError> {
        let mut query = vec![
            ("pageSize", cursor.page_size.to_string()),
            ("requestSyncToken", "true".to_string()),
            ("sources", READ_SOURCE_TYPE_CONTACT.to_string()),
        ];
        match kind {
            SourceKind::Saved => {
                query.push(("personFields", cursor.person_fields.clone()));
            }
            SourceKind::Other => {
                query.push(("readMask", cursor.person_fields.clone()));
            }
        }
        if let Some(page_token) = &cursor.page_token {
            query.push(("pageToken", page_token.clone()));
        }
        if cursor.stage == Stage::Delta
            && let Some(sync_token) = &cursor.sync_token
        {
            query.push(("syncToken", sync_token.clone()));
        }
        let url = with_query(self.url(kind)?, &query);
        self.client.get_json(ctx, url).await
    }

    async fn snapshot(
        &self,
        kind: SourceKind,
        state: &PeopleCursor,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        let response = self.request(kind, state, ctx).await?;
        let ListResponse {
            connections,
            other_contacts,
            next_page_token,
            next_sync_token,
        } = response;
        let next_sync_token = next_sync_token.or_else(|| state.sync_token.clone());
        let has_more = next_page_token.is_some();
        let checkpoint = PeopleCursor {
            page_token: next_page_token,
            sync_token: next_sync_token,
            stage: Stage::Snapshot,
            ..state.clone()
        };
        Ok(ContactPage {
            changes: contact_changes(match kind {
                SourceKind::Saved => connections,
                SourceKind::Other => other_contacts,
            }),
            checkpoint: checkpoint.encode()?,
            has_more,
        })
    }

    async fn delta(
        &self,
        kind: SourceKind,
        state: &PeopleCursor,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        let response = self.request(kind, state, ctx).await?;
        let ListResponse {
            connections,
            other_contacts,
            next_page_token,
            next_sync_token,
        } = response;
        let next_sync_token = next_sync_token.or_else(|| state.sync_token.clone());
        // A delta page is never "complete" for R6: completion would sweep every
        // contact the delta did not mention. Continue delta paging, then run one
        // verification snapshot before the final page can retire old rows.
        let (stage, has_more) = if next_page_token.is_some() {
            (Stage::Delta, true)
        } else {
            (Stage::Verify, true)
        };
        let checkpoint = PeopleCursor {
            page_token: next_page_token,
            sync_token: next_sync_token,
            stage,
            ..state.clone()
        };
        Ok(ContactPage {
            changes: contact_changes(match kind {
                SourceKind::Saved => connections,
                SourceKind::Other => other_contacts,
            }),
            checkpoint: checkpoint.encode()?,
            has_more,
        })
    }
}

#[async_trait]
impl AddressBookService for GoogleAddressBook {
    async fn next_page(
        &mut self,
        source: &ContactSourceId,
        cursor: Option<ProviderCursor>,
        ctx: &OperationContext,
    ) -> Result<ContactPage, ProviderError> {
        ctx.ensure_active()?;
        let kind = SourceKind::from_source(source)?;
        let mut state = match cursor {
            Some(cursor) => PeopleCursor::decode(&cursor, kind)?,
            None => PeopleCursor::fresh(kind),
        };

        match state.stage {
            // A completed full snapshot's cursor names its sync token. Resuming
            // it is an incremental request, exactly as the People API intends.
            Stage::Snapshot if state.page_token.is_none() && state.sync_token.is_some() => {
                state.stage = Stage::Delta;
            }
            Stage::Verify => {
                state = PeopleCursor::restart(kind, state.sweep.saturating_add(1));
            }
            Stage::Snapshot | Stage::Delta => {}
        }

        if state.stage == Stage::Delta {
            match self.delta(kind, &state, ctx).await {
                Ok(page) => return Ok(page),
                // People returns HTTP 410 / EXPIRED_SYNC_TOKEN after seven days.
                // Restart the source from a new full snapshot generation.
                Err(error) if error.kind == ErrorKind::CursorInvalid => {
                    state = PeopleCursor::restart(kind, state.sweep.saturating_add(1));
                }
                Err(error) => return Err(error),
            }
        }

        self.snapshot(kind, &state, ctx).await
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    #[serde(default)]
    connections: Vec<Person>,
    #[serde(default)]
    other_contacts: Vec<Person>,
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    next_sync_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Person {
    #[serde(default)]
    resource_name: String,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    names: Vec<PersonName>,
    #[serde(default)]
    email_addresses: Vec<PersonEmail>,
    #[serde(default)]
    metadata: Option<PersonMetadata>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersonName {
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersonEmail {
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PersonMetadata {
    #[serde(default)]
    deleted: bool,
}

fn contact_changes(people: Vec<Person>) -> Vec<ContactChange> {
    people
        .into_iter()
        .filter_map(|person| {
            let resource = person.resource_name.trim().to_string();
            if resource.is_empty() {
                return None;
            }
            let remote_id = RemoteContactId::new(resource);
            if person
                .metadata
                .as_ref()
                .is_some_and(|metadata| metadata.deleted)
            {
                return Some(ContactChange::Delete(remote_id));
            }

            let names: Vec<String> = person
                .names
                .iter()
                .filter_map(|name| name.display_name.as_deref())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .collect();
            let default_name = names.first().cloned();
            let emails: Vec<Address> = person
                .email_addresses
                .into_iter()
                .filter_map(|email| {
                    let value = email.value?.trim().to_string();
                    if value.is_empty() {
                        return None;
                    }
                    let name = email
                        .display_name
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .or_else(|| default_name.clone());
                    Some(Address::new(name, value))
                })
                .collect();

            Some(ContactChange::Upsert(ContactRecord {
                remote_id,
                version: person.etag,
                names,
                emails,
            }))
        })
        .collect()
}
