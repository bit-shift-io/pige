//! Read-side IMAP mailbox access behind the provider `MailboxService` trait.
//!
//! The adapter owns a low-level [`MailTransport`] and SELECTs a mailbox
//! internally for each read. Message identity is the F4 `(mailbox,
//! UIDVALIDITY, UID)` remote id (see [`super::identity`]); the adapter never
//! derives identity from a mutable selected folder.
//!
//! History is a header-only, newest-first crawl. It enumerates real UIDs
//! (`UID SEARCH ALL`) so sparse mailboxes are not truncated, pages through
//! explicit UID sets, and never fetches bodies or attachments. A resumable
//! cursor records the mailbox traversal progress, the epoch, and the UID
//! boundary reached.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mail::{FolderKind, Uid, body_parts, check_attachment_size, extract_attachments};
use crate::providers::error::ProviderError;
use crate::providers::traits::MailboxService;
use crate::providers::types::{
    AttachmentDescriptor, BodyContent, CollectionRecord, HistoryCoverage, HistoryPage,
    HistoryRequest, MessageRecord, OperationContext, RemoteAttachmentId, RemoteMessageId,
    SignatureStatus,
};
use crate::transport::{FolderState, MailTransport, RemoteFolder, RemoteMessage};

use super::{identity, transport_error};

/// Cursor payload version for [`ImapMailbox::history`]. A cursor from another
/// version is rejected as [`crate::providers::ErrorKind::CursorInvalid`].
const HISTORY_CURSOR_VERSION: u32 = 2;

/// Fallback page size when a caller asks for zero.
const HISTORY_MIN_PAGE: u32 = 1;

/// Durable progress for one history crawl.
///
/// `mailboxes` is the ordered traversal list captured when the crawl started,
/// so paging does not depend on re-listing folders in a stable order. Within
/// the current mailbox, `before_uid` is the exclusive upper bound already
/// yielded (newest-first); `None` means the mailbox has not been entered yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImapHistoryState {
    v: u32,
    mailboxes: Vec<String>,
    index: usize,
    /// UIDVALIDITY observed when the current mailbox was entered.
    uidvalidity: u32,
    /// Exclusive upper UID bound within `mailboxes[index]`.
    before_uid: Option<u32>,
    /// Cumulative messages learned across the whole crawl.
    scanned: u64,
    /// Cumulative UIDs discovered in mailboxes entered so far (progress hint).
    total_hint: u64,
}

/// A [`MailboxService`] over a live IMAP transport.
pub struct ImapMailbox {
    transport: Box<dyn MailTransport>,
}

impl ImapMailbox {
    pub fn new(transport: Box<dyn MailTransport>) -> Self {
        Self { transport }
    }

    /// SELECT `mailbox` so a subsequent fetch targets the right folder. The
    /// name is passed through unmodified; modified-UTF7 encoding to the wire
    /// stays in the transport. Returns the selected folder's state so callers
    /// can honor its `UIDVALIDITY`.
    async fn select_mailbox(&mut self, mailbox: &str) -> Result<FolderState, ProviderError> {
        let folder = RemoteFolder {
            name: mailbox.to_string(),
            delimiter: None,
            kind: FolderKind::Other,
            selectable: true,
        };
        self.transport
            .select(&folder)
            .await
            .map_err(transport_error)
    }

    /// Fetch the raw message named by `remote`, returning its mailbox and raw
    /// bytes. Shared by `body` and `attachment`.
    async fn fetch_raw(
        &mut self,
        remote: &RemoteMessageId,
    ) -> Result<(String, Vec<u8>), ProviderError> {
        let coords = identity::decode_message(remote)
            .ok_or_else(|| ProviderError::not_found("the message is not available"))?;
        let folder_state = self.select_mailbox(&coords.mailbox).await?;
        // The id names a specific UIDVALIDITY epoch. If the mailbox has been
        // re-enumerated since, the UID now refers to a different message (or
        // none), so a stale id must be reported missing rather than fetching
        // whatever currently holds that UID.
        if folder_state.uidvalidity != coords.uidvalidity {
            return Err(ProviderError::not_found("the message is not available"));
        }
        let raw = self
            .transport
            .fetch_body(coords.uid)
            .await
            .map_err(transport_error)?;
        Ok((coords.mailbox, raw))
    }

    /// Begin a crawl: capture the ordered mailbox list and an empty cursor.
    async fn start_history(&mut self) -> Result<ImapHistoryState, ProviderError> {
        let folders = self
            .transport
            .list_folders()
            .await
            .map_err(transport_error)?;
        Ok(ImapHistoryState {
            v: HISTORY_CURSOR_VERSION,
            // A `\NoSelect` container holds no messages; never crawl into it.
            mailboxes: folders
                .into_iter()
                .filter(|folder| folder.selectable)
                .map(|folder| folder.name)
                .collect(),
            index: 0,
            uidvalidity: 0,
            before_uid: None,
            scanned: 0,
            total_hint: 0,
        })
    }
}

#[async_trait]
impl MailboxService for ImapMailbox {
    async fn collections(
        &mut self,
        ctx: &OperationContext,
    ) -> Result<Vec<CollectionRecord>, ProviderError> {
        ctx.ensure_active()?;
        let folders = self
            .transport
            .list_folders()
            .await
            .map_err(transport_error)?;
        Ok(folders
            .into_iter()
            // A `\NoSelect` container is not a mailbox and is never offered as
            // a collection.
            .filter(|folder| folder.selectable)
            .map(|folder| CollectionRecord {
                remote_id: identity::collection_remote_id(&folder.name),
                name: folder.name,
                kind: folder.kind,
                delimiter: folder.delimiter,
                total: None,
                unread: None,
            })
            .collect())
    }

    async fn body(
        &mut self,
        id: &RemoteMessageId,
        ctx: &OperationContext,
    ) -> Result<BodyContent, ProviderError> {
        ctx.ensure_active()?;
        let (_mailbox, raw) = self.fetch_raw(id).await?;
        Ok(body_content(&raw))
    }

    async fn attachment(
        &mut self,
        message: &RemoteMessageId,
        id: &RemoteAttachmentId,
        ctx: &OperationContext,
    ) -> Result<Vec<u8>, ProviderError> {
        ctx.ensure_active()?;
        let (_mailbox, raw) = self.fetch_raw(message).await?;
        let bytes = crate::mail::attachment_bytes(&raw, id.as_str())
            .ok_or_else(|| ProviderError::not_found("the attachment is not in this message"))?;
        check_attachment_size(bytes.len() as u64).map_err(|_| {
            ProviderError::unsupported("the attachment is larger than the client accepts")
        })?;
        Ok(bytes)
    }

    async fn history(
        &mut self,
        request: HistoryRequest,
        ctx: &OperationContext,
    ) -> Result<HistoryPage, ProviderError> {
        ctx.ensure_active()?;
        let page_size = request.page_size.max(HISTORY_MIN_PAGE) as usize;

        let mut state = match &request.cursor {
            Some(cursor) => decode_history_cursor(cursor)?,
            None => self.start_history().await?,
        };
        if state.v != HISTORY_CURSOR_VERSION {
            return Err(ProviderError::cursor_invalid(
                "the history cursor was written by a different version",
            ));
        }

        let mut messages = Vec::new();
        let mut produced = false;
        let mut more_in_mailbox = false;
        let mut rebaselined = false;

        while state.index < state.mailboxes.len() {
            // A long run of unselectable mailboxes must still honor a cancel.
            ctx.ensure_active()?;
            let mailbox = state.mailboxes[state.index].clone();

            // (Re-)select the current mailbox on *every* page. The cursor is
            // durable, so a fresh session (or an interleaved `body`/
            // `attachment`/`collections` call) may leave a different mailbox
            // selected; `list_uids` and `fetch_learning_headers` act on the
            // currently selected folder, so we must re-establish the cursor's.
            // A genuine SELECT failure aborts the page: a `\NoSelect` container
            // was already excluded from the crawl list, so a failure here is an
            // auth/TLS/transient error that must surface, not silently drop the
            // mailbox's mail from coverage.
            let folder_state = self.select_mailbox(&mailbox).await?;

            // Compare the live epoch with the cursor's on every page. A UID
            // only names a message inside its UIDVALIDITY epoch: after a reset
            // the same UID may be gone or may name a different message, so the
            // old `before_uid` bound must never be reused. Restart this
            // mailbox from its newest UID under the new epoch and return a
            // non-complete page so the caller commits the rebaselined cursor
            // before any further paging (and can safely replay it).
            if state.uidvalidity != 0 && state.uidvalidity != folder_state.uidvalidity {
                state.uidvalidity = folder_state.uidvalidity;
                state.before_uid = None;
                rebaselined = true;
                break;
            }
            // Entering a mailbox records the epoch that names every id yielded
            // from it.
            state.uidvalidity = folder_state.uidvalidity;

            let mut uids = self.transport.list_uids().await.map_err(transport_error)?;
            // Newest first, from real UID metadata. Sparse UIDs are kept as-is.
            uids.sort_unstable_by(|a, b| b.cmp(a));
            if state.before_uid.is_none() {
                state.total_hint += uids.len() as u64;
            }

            let bound = state.before_uid;
            let batch: Vec<Uid> = uids
                .into_iter()
                .filter(|uid| bound.is_none_or(|bound| uid.0 < bound))
                .take(page_size)
                .collect();

            if batch.is_empty() {
                // Mailbox exhausted: advance to the next one.
                state.index += 1;
                state.uidvalidity = 0;
                state.before_uid = None;
                continue;
            }

            more_in_mailbox = batch.len() == page_size;
            let mut fetched = self
                .transport
                .fetch_learning_headers(&batch)
                .await
                .map_err(transport_error)?;
            fetched.sort_unstable_by_key(|message| std::cmp::Reverse(message.uid));

            state.scanned += fetched.len() as u64;
            state.before_uid = batch.last().map(|uid| uid.0);
            for message in fetched {
                messages.push(history_record(&mailbox, state.uidvalidity, message));
            }
            produced = true;
            break;
        }

        let has_more = rebaselined
            || (produced && (more_in_mailbox || state.index + 1 < state.mailboxes.len()));
        let checkpoint = encode_history_cursor(&state)?;

        Ok(HistoryPage {
            messages,
            checkpoint,
            has_more,
            coverage: HistoryCoverage {
                scanned: state.scanned,
                total_hint: Some(state.total_hint),
            },
        })
    }
}

/// Build a provider [`MessageRecord`] from one fetched header. History records
/// are metadata-only: no snippet, no attachment flag, no thread mapping.
fn history_record(mailbox: &str, uidvalidity: u32, message: RemoteMessage) -> MessageRecord {
    MessageRecord {
        remote_id: identity::message_remote_id(mailbox, uidvalidity, message.uid),
        remote_thread: None,
        header: message.header,
        flags: message.flags,
        collections: vec![identity::collection_remote_id(mailbox)],
        snippet: String::new(),
        has_attachments: false,
    }
}

/// Normalize a raw RFC 5322 message into a [`BodyContent`] using the same MIME
/// pipeline as the reader: `mail-parser` for text/HTML and
/// [`extract_attachments`] for descriptors. The HTML is raw; the allowlist
/// sanitizer runs at render/index time. `raw` carries the original bytes.
fn body_content(raw: &[u8]) -> BodyContent {
    let (html, plain_text) = body_parts(raw);
    let attachments = extract_attachments(raw)
        .into_iter()
        .map(|part| AttachmentDescriptor {
            remote_id: RemoteAttachmentId::from(part.part_id.clone()),
            part_id: part.part_id,
            filename: part.filename,
            content_type: part.content_type,
            size: part.size,
            content_id: part.content_id,
            disposition: part.disposition,
        })
        .collect();
    BodyContent {
        plain_text,
        html,
        raw: Some(raw.to_vec()),
        attachments,
        // IMAP carries no signature trust state; F8 persists the real status.
        signature: SignatureStatus::NotApplicable,
    }
}

fn encode_history_cursor(
    state: &ImapHistoryState,
) -> Result<crate::providers::ProviderCursor, ProviderError> {
    let payload = serde_json::to_string(state).map_err(|error| {
        ProviderError::transient("could not encode the history cursor")
            .with_source_detail(error.to_string())
    })?;
    Ok(crate::providers::ProviderCursor::new(
        HISTORY_CURSOR_VERSION,
        payload,
    ))
}

fn decode_history_cursor(
    cursor: &crate::providers::ProviderCursor,
) -> Result<ImapHistoryState, ProviderError> {
    serde_json::from_str(&cursor.payload)
        .map_err(|_| ProviderError::cursor_invalid("the history cursor could not be read"))
}
