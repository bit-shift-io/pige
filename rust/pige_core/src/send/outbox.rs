//! The durable outbox and its state machine.
//!
//! States:
//! - `Draft`: saved locally, never sent.
//! - `Queued`: ready to send, persisted before any network call.
//! - `Sending`: claimed by a flush; SMTP may or may not have accepted.
//! - `SentLocal`: SMTP accepted. Only the Sent APPEND remains.
//! - `Sent`: SMTP accepted and, when a Sent folder exists, the APPEND succeeded.
//! - `Failed`: permanent failure or an interrupted send that must not be retried
//!   blindly, because it may already have reached the server.
//!
//! Flush is deliberately conservative: a row is moved to `Sending` *before*
//! SMTP, and any `Sending` row found on a later flush is failed instead of
//! resent. The tiny window between SMTP acceptance and the `SentLocal` write can
//! therefore cause a missing Sent copy, never a duplicate delivery.

use std::str::FromStr;

use rusqlite::{Row, params};

use crate::account::{Account, AccountId};
use crate::mail::{Address, Flags};
use crate::store::{Store, StoreError};
use crate::transport::{MailTransport, SendError, SmtpSender};

use super::OutgoingMessage;
use super::compose::{ComposeError, build_mime};

/// Stable identifier for one outbox row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutboxId(pub String);

impl OutboxId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for OutboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for OutboxId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Persisted lifecycle of an outgoing message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    Draft,
    Queued,
    Sending,
    SentLocal,
    Sent,
    Failed,
}

impl OutboxState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::SentLocal => "sent_local",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "draft" => Some(Self::Draft),
            "queued" => Some(Self::Queued),
            "sending" => Some(Self::Sending),
            "sent_local" => Some(Self::SentLocal),
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// One outbox row, including the compose fields and the exact raw bytes once
/// they have been generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    pub id: OutboxId,
    pub message: OutgoingMessage,
    pub state: OutboxState,
    pub error: Option<String>,
    pub raw: Option<Vec<u8>>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Result of one flush. Per-message failures are reported here rather than
/// aborting the whole sync; only an unrecoverable store failure returns `Err`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxReport {
    /// Fully delivered and, when possible, appended to Sent.
    pub sent: u32,
    /// Accepted by SMTP, but the Sent APPEND still has to be retried.
    pub sent_local: u32,
    pub failed: u32,
    /// Still queued for a later attempt.
    pub queued: u32,
    pub errors: Vec<String>,
}

const COLUMNS: &str = "id, account_id, to_json, cc_json, bcc_json, subject, body, \
     in_reply_to, references_json, raw, state, error, created_at, updated_at";

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn encode_json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_string(value)?)
}

fn decode_json<T: serde::de::DeserializeOwned>(
    raw: &str,
    field: &'static str,
) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|_| StoreError::corrupt(field, raw))
}

impl Store {
    /// Insert a new row in `state`, returning its id.
    pub(crate) fn insert_outbox(
        &self,
        message: &OutgoingMessage,
        state: OutboxState,
    ) -> Result<OutboxId, StoreError> {
        let id = OutboxId::new();
        let now = now_unix();
        self.connection().execute(
            "INSERT INTO outbox (
                 id, account_id, to_json, cc_json, bcc_json, subject, body,
                 in_reply_to, references_json, raw, state, error, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, NULL, ?11, ?11)",
            params![
                id.0,
                message.account_id.0,
                encode_json(&message.to)?,
                encode_json(&message.cc)?,
                encode_json(&message.bcc)?,
                message.subject,
                message.body,
                message.in_reply_to,
                encode_json(&message.references)?,
                state.as_str(),
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn outbox_entry(&self, id: &OutboxId) -> Result<Option<OutboxEntry>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM outbox WHERE id = ?1");
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&id.0])?;
        match rows.next()? {
            Some(row) => Ok(Some(entry_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Every outbox row, newest first, optionally scoped to one account.
    pub fn outbox_items(
        &self,
        account: Option<&AccountId>,
    ) -> Result<Vec<OutboxEntry>, StoreError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM outbox
             WHERE (?1 IS NULL OR account_id = ?1)
             ORDER BY updated_at DESC, id"
        );
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([account.map(|account| account.0.as_str())])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(entry_from_row(row)?);
        }
        Ok(items)
    }

    pub fn outbox_drafts(
        &self,
        account: Option<&AccountId>,
    ) -> Result<Vec<OutboxEntry>, StoreError> {
        Ok(self
            .outbox_items(account)?
            .into_iter()
            .filter(|entry| entry.state == OutboxState::Draft)
            .collect())
    }

    /// Rows a flush should inspect: queued, already accepted, and interrupted.
    pub fn outbox_pending(&self, account: &AccountId) -> Result<Vec<OutboxEntry>, StoreError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM outbox
             WHERE account_id = ?1 AND state IN ('queued', 'sent_local', 'sending')
             ORDER BY created_at, id"
        );
        let conn = self.connection();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([&account.0])?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(entry_from_row(row)?);
        }
        Ok(items)
    }

    pub fn set_outbox_state(
        &self,
        id: &OutboxId,
        state: OutboxState,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![state.as_str(), error, now_unix(), id.0],
        )?;
        Ok(())
    }

    pub fn set_outbox_raw(&self, id: &OutboxId, raw: &[u8]) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET raw = ?1, updated_at = ?2 WHERE id = ?3",
            params![raw, now_unix(), id.0],
        )?;
        Ok(())
    }

    /// Persist the generated bytes and claim the row atomically. A row can only
    /// leave `queued` once, so overlapping flushes cannot both send it.
    pub fn claim_outbox(&self, id: &OutboxId, raw: &[u8]) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE outbox
             SET state = 'sending', raw = ?1, error = NULL, updated_at = ?2
             WHERE id = ?3 AND state = 'queued'",
            params![raw, now_unix(), id.0],
        )?;
        Ok(changed == 1)
    }

    pub fn set_outbox_error(&self, id: &OutboxId, error: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE outbox SET error = ?1, updated_at = ?2 WHERE id = ?3",
            params![error, now_unix(), id.0],
        )?;
        Ok(())
    }

    pub fn delete_outbox(&self, id: &OutboxId) -> Result<bool, StoreError> {
        let changed = self
            .connection()
            .execute("DELETE FROM outbox WHERE id = ?1", [&id.0])?;
        Ok(changed == 1)
    }

    pub fn count_outbox_state(
        &self,
        account: &AccountId,
        state: OutboxState,
    ) -> Result<u32, StoreError> {
        Ok(self.connection().query_row(
            "SELECT COUNT(*) FROM outbox WHERE account_id = ?1 AND state = ?2",
            params![account.0, state.as_str()],
            |row| row.get(0),
        )?)
    }
}

fn entry_from_row(row: &Row<'_>) -> Result<OutboxEntry, StoreError> {
    let to_json: String = row.get(2)?;
    let cc_json: String = row.get(3)?;
    let bcc_json: String = row.get(4)?;
    let references_json: String = row.get(8)?;
    let state: String = row.get(10)?;
    Ok(OutboxEntry {
        id: OutboxId(row.get(0)?),
        message: OutgoingMessage {
            account_id: AccountId(row.get(1)?),
            to: decode_json(&to_json, "outbox.to_json")?,
            cc: decode_json(&cc_json, "outbox.cc_json")?,
            bcc: decode_json(&bcc_json, "outbox.bcc_json")?,
            subject: row.get(5)?,
            body: row.get(6)?,
            in_reply_to: row.get(7)?,
            references: decode_json(&references_json, "outbox.references_json")?,
        },
        state: OutboxState::from_db(&state)
            .ok_or_else(|| StoreError::corrupt("outbox.state", state))?,
        error: row.get(11)?,
        raw: row.get(9)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

/// Save a new draft locally. Drafts never touch the network.
pub fn save_draft(store: &Store, message: OutgoingMessage) -> Result<OutboxId, StoreError> {
    store.insert_outbox(&message, OutboxState::Draft)
}

/// Replace an existing draft's fields. The row keeps its id so the Drafts list
/// stays stable while the user edits.
pub fn update_draft(
    store: &Store,
    id: &OutboxId,
    message: &OutgoingMessage,
) -> Result<(), StoreError> {
    store.connection().execute(
        "UPDATE outbox SET
             account_id = ?1, to_json = ?2, cc_json = ?3, bcc_json = ?4,
             subject = ?5, body = ?6, in_reply_to = ?7, references_json = ?8,
             updated_at = ?9
         WHERE id = ?10",
        params![
            message.account_id.0,
            encode_json(&message.to)?,
            encode_json(&message.cc)?,
            encode_json(&message.bcc)?,
            message.subject,
            message.body,
            message.in_reply_to,
            encode_json(&message.references)?,
            now_unix(),
            id.0,
        ],
    )?;
    Ok(())
}

pub fn discard_draft(store: &Store, id: &OutboxId) -> Result<bool, StoreError> {
    store.delete_outbox(id)
}

/// Drafts, newest first, optionally for one account.
pub fn drafts(store: &Store, account: Option<&AccountId>) -> Result<Vec<OutboxEntry>, StoreError> {
    store.outbox_drafts(account)
}

/// Every outbox entry, used by the UI for queued/failed status and tests.
pub fn list_outbox(
    store: &Store,
    account: Option<&AccountId>,
) -> Result<Vec<OutboxEntry>, StoreError> {
    store.outbox_items(account)
}

pub fn outbox_entry(store: &Store, id: &OutboxId) -> Result<Option<OutboxEntry>, StoreError> {
    store.outbox_entry(id)
}

/// Persist a message as `Queued` before any network work. Callers then invoke
/// [`flush_outbox`]; if the network is unavailable the row simply stays queued.
pub async fn queue_send(store: &Store, message: OutgoingMessage) -> Result<OutboxId, StoreError> {
    store.insert_outbox(&message, OutboxState::Queued)
}

/// Deliver every pending row for one account.
pub async fn flush_outbox(
    store: &Store,
    account: &Account,
    smtp: &mut dyn SmtpSender,
    imap: &mut dyn MailTransport,
) -> Result<OutboxReport, SendError> {
    let from = Address::new(
        Some(account.display_name.clone()).filter(|name| !name.trim().is_empty()),
        account.email.clone(),
    );
    let pending = store.outbox_pending(&account.id).map_err(store_error)?;
    let mut report = OutboxReport::default();

    for entry in pending {
        match entry.state {
            OutboxState::Queued => {
                match flush_queued(store, account, &from, smtp, imap, &entry).await {
                    Ok(Outcome::Sent) => report.sent += 1,
                    Ok(Outcome::SentLocal(error)) => {
                        report.sent_local += 1;
                        report.errors.push(error);
                    }
                    Ok(Outcome::Failed(error)) => {
                        report.failed += 1;
                        report.errors.push(error);
                    }
                    Ok(Outcome::Skipped) => {}
                    Err(error) => return Err(error),
                }
            }
            OutboxState::SentLocal => match entry.raw.as_deref() {
                Some(raw) => match append_sent(store, account, imap, &entry.id, raw).await {
                    Ok(()) => report.sent += 1,
                    Err(message) => {
                        report.sent_local += 1;
                        report.errors.push(message);
                    }
                },
                None => {
                    let message = "sent message has no raw bytes to append".to_string();
                    store
                        .set_outbox_error(&entry.id, &message)
                        .map_err(store_error)?;
                    report.sent_local += 1;
                    report.errors.push(message);
                }
            },
            OutboxState::Sending => {
                // The process may have died after SMTP accepted this message.
                // Resending would risk a duplicate delivery, so fail it loudly
                // and leave the row for the user to inspect.
                let message = "send was interrupted; not retried to avoid a duplicate".to_string();
                store
                    .set_outbox_state(&entry.id, OutboxState::Failed, Some(&message))
                    .map_err(store_error)?;
                report.failed += 1;
                report.errors.push(message);
            }
            OutboxState::Draft | OutboxState::Sent | OutboxState::Failed => {}
        }
    }

    report.queued = store
        .count_outbox_state(&account.id, OutboxState::Queued)
        .map_err(store_error)?;
    Ok(report)
}

enum Outcome {
    Sent,
    SentLocal(String),
    Failed(String),
    Skipped,
}

async fn flush_queued(
    store: &Store,
    account: &Account,
    from: &Address,
    smtp: &mut dyn SmtpSender,
    imap: &mut dyn MailTransport,
    entry: &OutboxEntry,
) -> Result<Outcome, SendError> {
    let raw = match &entry.raw {
        Some(raw) => raw.clone(),
        None => build_mime(from, &entry.message).map_err(compose_error)?,
    };

    let recipients = entry
        .message
        .to
        .iter()
        .chain(entry.message.cc.iter())
        .chain(entry.message.bcc.iter())
        .cloned()
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        let message = "message has no recipients".to_string();
        store
            .set_outbox_state(&entry.id, OutboxState::Failed, Some(&message))
            .map_err(store_error)?;
        return Ok(Outcome::Failed(message));
    }

    if !store.claim_outbox(&entry.id, &raw).map_err(store_error)? {
        // Another flush claimed it first; leave it alone.
        return Ok(Outcome::Skipped);
    }

    if let Err(error) = smtp.send_raw(from, &recipients, &raw).await {
        let message = error.to_string();
        store
            .set_outbox_state(&entry.id, OutboxState::Failed, Some(&message))
            .map_err(store_error)?;
        return Ok(Outcome::Failed(message));
    }

    // SMTP accepted. Record that before touching the Sent folder; from here on
    // a failure can only retry the APPEND, never the send.
    store
        .set_outbox_state(&entry.id, OutboxState::SentLocal, None)
        .map_err(store_error)?;

    match append_sent(store, account, imap, &entry.id, &raw).await {
        Ok(()) => Ok(Outcome::Sent),
        Err(message) => Ok(Outcome::SentLocal(message)),
    }
}

/// Best-effort APPEND to the account's Sent folder. No Sent folder means there
/// is nothing to retry, so the row is considered sent. A failure leaves the row
/// in `SentLocal` and stores the error for the next flush.
async fn append_sent(
    store: &Store,
    account: &Account,
    imap: &mut dyn MailTransport,
    id: &OutboxId,
    raw: &[u8],
) -> Result<(), String> {
    let folders = store.folders(&account.id).map_err(|e| e.to_string())?;
    let Some(folder) = folders
        .iter()
        .find(|folder| folder.kind == crate::mail::FolderKind::Sent)
    else {
        store
            .set_outbox_state(id, OutboxState::Sent, None)
            .map_err(|e| e.to_string())?;
        return Ok(());
    };

    let flags = Flags {
        seen: true,
        ..Flags::default()
    };
    match imap.append(&folder.name, raw, flags).await {
        Ok(()) => {
            store
                .set_outbox_state(id, OutboxState::Sent, None)
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(error) => {
            let message = format!("append to {}: {error}", folder.name);
            store
                .set_outbox_error(id, &message)
                .map_err(|e| e.to_string())?;
            Err(message)
        }
    }
}

fn compose_error(error: ComposeError) -> SendError {
    SendError(error.to_string())
}

fn store_error(error: StoreError) -> SendError {
    SendError(format!("outbox store: {error}"))
}

impl FromStr for OutboxState {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_db(value).ok_or(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_send_is_not_a_retryable_outbox_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path().join("pige.db")).unwrap();
        store.migrate().unwrap();
        let account = Account {
            id: AccountId("a".to_string()),
            label: "A".to_string(),
            email: "a@example.com".to_string(),
            display_name: "A".to_string(),
            username: "a@example.com".to_string(),
            imap: crate::account::ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: crate::account::Security::Tls,
            },
            smtp: crate::account::ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: crate::account::Security::Tls,
            },
            auth: crate::account::AuthKind::Password,
            sort_order: 0,
        };
        store.upsert_account(&account).unwrap();
        let id = save_draft(
            &store,
            OutgoingMessage {
                account_id: account.id.clone(),
                to: vec![Address::new(None, "b@example.com")],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "s".to_string(),
                body: "b".to_string(),
                in_reply_to: None,
                references: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            store.outbox_entry(&id).unwrap().unwrap().state,
            OutboxState::Draft
        );
        assert_eq!(store.outbox_pending(&account.id).unwrap().len(), 0);
    }
}
