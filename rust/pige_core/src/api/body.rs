//! Provider-routed body and attachment reads.
//!
//! Normal (IMAP) bodies and provider-normalized (for example Proton) bodies
//! arrive here as the same [`BodyContent`], so both go through the *same*
//! sanitizer and size limits: the raw HTML/plain parts are cached and parsed by
//! [`crate::html`] exactly as before, and the provider's attachment descriptors
//! are persisted with their remote ids so a later fetch names the part the way
//! the provider does.

use std::sync::Arc;

use crate::account::Account;
use crate::html::{MailDocument, parse_html, plain_text_document};
use crate::mail::{MessageId, snippet_from_parts};
use crate::providers::{
    BodyContent, OperationContext, ProviderFactory, RemoteAttachmentId, RemoteMessageId,
};
use crate::store::{Body, Store};

use super::mailbox::{open_session, provider_message};
use super::now_unix;

/// Fetch one normalized body through the account's provider mailbox service.
///
/// The returned [`BodyContent`] is protocol-neutral: the caller never sees an
/// IMAP UID or a Proton reference. A `raw` representation, when present, is the
/// provider's original bytes, but the cached body always comes from the decoded
/// `html`/`plain_text` halves so a native body and an IMAP body render through
/// one pipeline.
pub(crate) async fn fetch_body(
    providers: &Arc<dyn ProviderFactory>,
    account: &Account,
    ctx: &OperationContext,
    remote: &RemoteMessageId,
) -> Result<BodyContent, String> {
    let mut session = open_session(providers, account, ctx).await?;
    let service = session.mailbox_service().map_err(provider_message)?;
    service.body(remote, ctx).await.map_err(provider_message)
}

/// Fetch one attachment's bytes through the account's provider mailbox service,
/// naming the message and part by their provider identities.
pub(crate) async fn fetch_attachment(
    providers: &Arc<dyn ProviderFactory>,
    account: &Account,
    ctx: &OperationContext,
    message: &RemoteMessageId,
    attachment: &RemoteAttachmentId,
) -> Result<Vec<u8>, String> {
    let mut session = open_session(providers, account, ctx).await?;
    let service = session.mailbox_service().map_err(provider_message)?;
    service
        .attachment(message, attachment, ctx)
        .await
        .map_err(provider_message)
}

/// Persist a fetched provider body: the decoded text/HTML through the shared
/// cache path, the signature trust state, and the attachment descriptors (with
/// their remote ids). Returns the renderable document, or `None` when the body
/// carried no readable text.
pub(crate) fn cache_body_content(
    store: &Store,
    message_id: &MessageId,
    content: &BodyContent,
) -> Option<MailDocument> {
    // Signature status is part of the body, independent of whether the text
    // decoded; record it even for an Unsupported/Unsigned body.
    if let Err(error) = store.set_signature_status(message_id, content.signature) {
        log::warn!("cache signature status {message_id}: {error}");
    }
    // Attachment metadata (with remote ids) comes from the same fetch. Storing
    // it also recomputes the message's attachment flag and thread aggregate.
    if let Err(error) = store.store_provider_attachments(message_id, &content.attachments) {
        log::warn!("cache attachments {message_id}: {error}");
    }

    let body = Body {
        plain_text: content.plain_text.clone(),
        html: content.html.clone(),
        fetched_at: now_unix(),
    };
    let document = provider_document(content.plain_text.as_deref(), content.html.as_deref());
    if document.is_none()
        && let Err(error) = store.set_body_state(message_id, "none")
    {
        log::warn!("record unreadable body {message_id}: {error}");
    }
    if document.is_some()
        && let Err(error) = store.upsert_body(message_id, &body)
    {
        log::warn!("cache body {message_id}: {error}");
    }
    // A decoded body also repairs the list preview.
    if let Some(snippet) = snippet_from_parts(body.plain_text.as_deref(), body.html.as_deref())
        && let Err(error) = store.set_snippet(message_id, &snippet)
    {
        log::warn!("update preview {message_id}: {error}");
    }
    document
}

/// Build a [`MailDocument`] from a provider body's text halves, preferring HTML
/// exactly as the cached-body path does.
pub(crate) fn provider_document(
    plain_text: Option<&str>,
    html: Option<&str>,
) -> Option<MailDocument> {
    if let Some(html) = html {
        return Some(parse_html(html, false));
    }
    plain_text.map(plain_text_document)
}

/// Build a [`MailDocument`] from a cached body, preferring HTML.
pub(crate) fn document_from_body(body: &Body) -> Option<MailDocument> {
    if let Some(html) = &body.html {
        Some(parse_html(html, false))
    } else {
        body.plain_text.as_deref().map(plain_text_document)
    }
}
