//! Native Proton submission with enforced recipient encryption policy.
//!
//! Preparation creates (or adopts) one stable remote draft and persists only its
//! identity plus operation metadata. The draft body is encrypted to the sender;
//! the prepared artifact deliberately holds no unlocked keys or tokens.
//!
//! `submit` re-resolves recipient policy before any delivery. Internal
//! recipients get Proton E2E packages, external OpenPGP recipients get inline
//! OpenPGP packages, and ordinary external recipients are only classified after
//! an affirmative contact/key lookup. Any policy or lookup failure aborts the
//! whole operation before the send request.
//!
//! Proton owns the Sent copy, so [`ProtonSubmission::sent_copy_policy`] is
//! [`SentCopyPolicy::ServerManaged`] and `finalize_sent` never appends. A draft
//! is not delivery evidence: reconciliation accepts only a message the server
//! has moved to Sent, and an ambiguous send is reported as Unknown so the outbox
//! marks it Uncertain instead of resending.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::account::Account;
use crate::mail::{Address, MAX_ATTACHMENT_BYTES};
use crate::providers::{
    DeliveryClass, DeliveryClassification, ErrorKind, OperationContext, OperationId,
    PreparedSubmission, ProviderError, ProviderId, ReconcileOutcome, RemoteMessageId,
    RemoteThreadId, SentCopyPolicy, SubmissionAttachment, SubmissionOutcome, SubmissionReceipt,
    SubmissionRequest, SubmissionService,
};
use crate::send::OutgoingMessage;

use proton_core::api;
use proton_core::crypto::{SessionKeyMaterial, UploadedAttachment};
use proton_core::model::enums::{label_ids, message_flag, package_type};
use proton_core::model::message::Message;

use super::client::{ProtonClient, ProtonHttp, map_proton_error};
use super::policy::{ContactPolicyStore, ProtonRecipientPolicy};
use super::{DeliverySecurity, PROTON_PROVIDER};

/// Version of the adapter-owned prepared artifact.
const PROTON_SUBMISSION_VERSION: u32 = 1;
const LIST_PAGE_SIZE: u32 = 100;
const MAX_LIST_PAGES: u32 = 100;

/// Non-secret, durable operation evidence. It contains no key material.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedEvidence {
    v: u32,
    operation: String,
    draft_id: String,
    external_id: String,
    sender: String,
    digest: String,
}

impl PreparedEvidence {
    fn decode(reference: &str) -> Option<Self> {
        serde_json::from_str(reference).ok()
    }
}

/// Proton native submission with a server-managed Sent copy.
pub struct ProtonSubmission {
    client: ProtonClient,
    account: Account,
    sender_alias: Option<String>,
    policy: ProtonRecipientPolicy,
}

impl ProtonSubmission {
    pub fn new(client: ProtonClient, account: Account) -> Self {
        Self::with_contact_store(client, account, Arc::new(ContactPolicyStore::default()))
    }

    pub(crate) fn with_contact_store(
        client: ProtonClient,
        account: Account,
        contacts: Arc<ContactPolicyStore>,
    ) -> Self {
        let policy = ProtonRecipientPolicy::with_contact_store(client.clone(), contacts.clone());
        Self {
            client,
            account,
            sender_alias: None,
            policy,
        }
    }

    /// Send from an explicit Proton address alias. The alias is validated
    /// against the unlocked address key rings before any draft is created.
    #[must_use]
    pub fn with_sender_alias(mut self, email: impl Into<String>) -> Self {
        let email = email.into();
        self.sender_alias = Some(email.clone());
        self.policy.set_sender_alias(email);
        self
    }

    fn sender_email(&self) -> &str {
        self.sender_alias
            .as_deref()
            .unwrap_or(self.account.email.as_str())
    }

    fn sender_keys_for(
        &self,
        email: &str,
    ) -> Result<proton_core::crypto::keys::AddressKeys, ProviderError> {
        let email = email.trim().to_string();
        if email.is_empty() {
            return Err(ProviderError::forbidden(
                "the Proton sender address is empty",
            ));
        }
        self.client.unlocked_keys().with_keys(|keys| {
            keys.address_for_email(&email)
                .filter(|address| !address.keys.is_empty())
                .cloned()
                .ok_or_else(|| {
                    ProviderError::forbidden(format!(
                        "the sender address {email} is not an unlocked Proton address"
                    ))
                })
        })?
    }

    /// Decode and authenticate the prepared artifact. The chosen sender alias is
    /// part of the durable evidence, so a restart that does not re-apply
    /// [`Self::with_sender_alias`] still verifies the digest and continues the
    /// same submission instead of wedging on a sender mismatch.
    fn evidence(&self, request: &SubmissionRequest) -> Option<PreparedEvidence> {
        let prepared = request.prepared.as_ref()?;
        if prepared.version != PROTON_SUBMISSION_VERSION
            || prepared.backend.as_str() != PROTON_PROVIDER
        {
            return None;
        }
        let evidence = PreparedEvidence::decode(prepared.reference.as_deref()?)?;
        if evidence.v != PROTON_SUBMISSION_VERSION
            || evidence.operation != request.operation.as_str()
            || evidence.sender.trim().is_empty()
            || evidence.digest != self.digest(request, &evidence.sender).ok()?
        {
            return None;
        }
        Some(evidence)
    }

    fn reusable(&self, request: &SubmissionRequest) -> Option<PreparedSubmission> {
        self.evidence(request)?;
        request.prepared.clone()
    }

    fn digest(&self, request: &SubmissionRequest, sender: &str) -> Result<String, ProviderError> {
        let mut hasher = Sha256::new();
        feed(&mut hasher, sender.as_bytes());
        for address in request
            .message
            .to
            .iter()
            .chain(request.message.cc.iter())
            .chain(request.message.bcc.iter())
        {
            feed(&mut hasher, b"recipient");
            feed(
                &mut hasher,
                address.name.as_deref().unwrap_or_default().as_bytes(),
            );
            feed(
                &mut hasher,
                address.email.trim().to_ascii_lowercase().as_bytes(),
            );
        }
        feed(&mut hasher, request.message.subject.as_bytes());
        feed(&mut hasher, request.message.body.as_bytes());
        feed(
            &mut hasher,
            request
                .message
                .in_reply_to
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
        for reference in &request.message.references {
            feed(&mut hasher, reference.as_bytes());
        }
        for attachment in &request.attachments {
            feed(&mut hasher, attachment.id.as_bytes());
            feed(&mut hasher, attachment.filename.as_bytes());
            feed(&mut hasher, attachment.content_type.as_bytes());
            feed(
                &mut hasher,
                attachment
                    .content_id
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            );
            feed(&mut hasher, attachment.disposition.as_str().as_bytes());
            feed(&mut hasher, &attachment.bytes);
        }
        Ok(hex(&hasher.finalize()))
    }

    fn external_id(&self, operation: &OperationId) -> String {
        let token: String = operation
            .as_str()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' {
                    character
                } else {
                    '-'
                }
            })
            .collect();
        format!("<pige-{token}@pige.invalid>")
    }

    async fn find_draft(
        &self,
        http: &ProtonHttp,
        external_id: &str,
        ctx: &OperationContext,
    ) -> Result<Option<String>, ProviderError> {
        let mut page = 0u32;
        loop {
            ctx.ensure_active()?;
            let query = api::messages::ListQuery {
                label_id: Some(label_ids::DRAFTS.to_string()),
                page: Some(page),
                page_size: Some(LIST_PAGE_SIZE),
                ..api::messages::ListQuery::default()
            };
            let (total, messages) =
                http.finish(api::messages::list_messages(http, &query).await)?;
            if let Some(message) = messages
                .iter()
                .find(|message| message.external_id.as_deref() == Some(external_id))
            {
                return Ok(Some(message.id.clone()));
            }
            let page_end = u64::from(page.saturating_add(1)) * u64::from(LIST_PAGE_SIZE);
            if page_end >= u64::from(total) {
                return Ok(None);
            }
            if messages.is_empty() {
                // Never create a second draft over an inconsistent listing.
                return Err(ProviderError::transient(
                    "the Proton draft listing was incomplete",
                ));
            }
            page = page.saturating_add(1);
            if page >= MAX_LIST_PAGES {
                return Err(ProviderError::transient(
                    "the Proton draft listing was too large",
                ));
            }
        }
    }

    async fn create_draft(
        &self,
        message: &OutgoingMessage,
        sender: &proton_core::crypto::keys::AddressKeys,
        external_id: &str,
        ctx: &OperationContext,
    ) -> Result<String, ProviderError> {
        ctx.ensure_active()?;
        let provider = proton_core::crypto::provider();
        let body = proton_core::crypto::encrypt_self_draft(&provider, sender, &message.body)
            .map_err(map_proton_error)?;
        let draft = json!({
            "ToList": recipient_list(&message.to),
            "CCList": recipient_list(&message.cc),
            "BCCList": recipient_list(&message.bcc),
            "Subject": message.subject,
            "Sender": { "Address": sender.email, "Name": "" },
            "Body": body,
            "MIMEType": "text/plain",
            "ExternalID": external_id,
        });
        let mut body = json!({ "Message": draft });
        // Proton threads a reply by its parent's remote id. A raw RFC
        // Message-ID cannot be used without an adapter-side lookup, so it is
        // carried in references only (the draft still records the operation id
        // in ExternalID for stable reconciliation).
        if let Some(parent) = message.in_reply_to.as_deref()
            && looks_like_remote_id(parent)
        {
            body["ParentID"] = json!(parent);
            body["Action"] = json!(0);
        }
        let http = self.client.request_http(ctx).await?;
        match http.finish(api::messages::create_draft(&http, body).await) {
            Ok(created) => {
                ctx.ensure_active()?;
                Ok(created.meta.id)
            }
            Err(error)
                if matches!(
                    error.kind,
                    ErrorKind::Forbidden
                        | ErrorKind::NotFound
                        | ErrorKind::Conflict
                        | ErrorKind::Unsupported
                        | ErrorKind::CursorInvalid
                ) =>
            {
                Err(error)
            }
            Err(error) => Err(ProviderError::unknown_write_outcome(
                "the Proton draft outcome could not be confirmed",
            )
            .with_source_detail(error.safe_message)),
        }
    }

    async fn fetch_message(
        &self,
        http: &ProtonHttp,
        id: &str,
        ctx: &OperationContext,
    ) -> Result<Message, ProviderError> {
        ctx.ensure_active()?;
        let message = http.finish(api::messages::get_message(http, id).await)?;
        ctx.ensure_active()?;
        Ok(message)
    }

    /// Re-resolve policy for a submit/reconcile path that may follow a process
    /// restart. The durable prepared artifact never contains key material.
    async fn resolve_materials(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<HashMap<String, super::policy::RecipientMaterial>, ProviderError> {
        // The adapter-private store is shared with the address book so raw card
        // material survives a snapshot. `resolve` reuses a completed
        // enumeration only for a short TTL, then re-lists the address book, so
        // a contact that just added a mandatory requirement is honored; stale
        // material can never authorize a send past that window.
        let deliveries = self.policy.resolve(&request.message, ctx).await?;
        let mut materials = HashMap::new();
        for delivery in deliveries {
            let email = normalize(&delivery.address.email);
            let material = self
                .policy
                .material_for(&delivery.address.email)
                .cloned()
                .ok_or_else(|| {
                    ProviderError::unsupported("recipient policy material is unavailable")
                })?;
            materials.insert(email, material);
        }
        Ok(materials)
    }
}

fn feed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn normalize(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn looks_like_remote_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.contains('@') && !value.contains('<') && !value.contains('>')
}

fn recipient_list(addresses: &[Address]) -> Value {
    Value::Array(
        addresses
            .iter()
            .map(|address| {
                json!({
                    "Address": address.email.trim(),
                    "Name": address.name.clone().unwrap_or_else(|| address.email.clone()),
                })
            })
            .collect(),
    )
}

fn is_sent(message: &Message) -> bool {
    message_flag::has(message.meta.flags, message_flag::SENT)
        || message
            .meta
            .label_ids
            .iter()
            .any(|label| label == label_ids::SENT || label == label_ids::ALL_SENT)
}

fn is_draft(message: &Message) -> bool {
    message
        .meta
        .label_ids
        .iter()
        .any(|label| label == label_ids::DRAFTS || label == label_ids::ALL_DRAFTS)
}

fn same_recipients(
    expected: &[Address],
    actual: &[proton_core::model::message::Recipient],
) -> bool {
    let mut expected: Vec<String> = expected
        .iter()
        .map(|address| normalize(&address.email))
        .collect();
    let mut actual: Vec<String> = actual
        .iter()
        .map(|recipient| normalize(&recipient.address))
        .collect();
    expected.sort();
    actual.sort();
    expected == actual
}

struct Uploaded {
    id: String,
    material: SessionKeyMaterial,
}

#[async_trait]
impl SubmissionService for ProtonSubmission {
    fn sent_copy_policy(&self) -> SentCopyPolicy {
        SentCopyPolicy::ServerManaged
    }

    async fn classify(
        &mut self,
        message: &OutgoingMessage,
        ctx: &OperationContext,
    ) -> Result<Vec<DeliveryClassification>, ProviderError> {
        let sender = self.sender_email().to_string();
        self.policy.set_sender_alias(sender);
        let deliveries = self.policy.resolve(message, ctx).await?;
        Ok(deliveries
            .into_iter()
            .map(|delivery| DeliveryClassification {
                address: delivery.address,
                class: match delivery.security {
                    DeliverySecurity::ProtonEncrypted => DeliveryClass::EndToEndEncrypted,
                    DeliverySecurity::ExternalOpenPgp => DeliveryClass::ExternalOpenPgp,
                    DeliverySecurity::ExternalOrdinary => DeliveryClass::ExternalOrdinary,
                },
            })
            .collect())
    }

    async fn prepare(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<PreparedSubmission, ProviderError> {
        ctx.ensure_active()?;
        if request.message.recipients().is_empty() {
            return Err(ProviderError::forbidden("the message has no recipients"));
        }
        // A durable prepared artifact is authoritative for the chosen sender
        // alias after a restart; otherwise fall back to the configured alias or
        // the account address.
        let sender_email = match self.evidence(request) {
            Some(evidence) => evidence.sender,
            None => self.sender_email().trim().to_string(),
        };
        let sender = self.sender_keys_for(&sender_email)?;
        self.policy.set_sender_alias(sender_email);
        // Resolve policy before any draft request. A failure preserves the
        // unsent operation and never creates a remote draft.
        self.policy.resolve(&request.message, ctx).await?;
        ctx.ensure_active()?;

        if let Some(prepared) = self.reusable(request) {
            return Ok(prepared);
        }

        let external_id = self.external_id(&request.operation);
        let http = self.client.request_http(ctx).await?;
        let draft_id = match self.find_draft(&http, &external_id, ctx).await? {
            Some(id) => id,
            None => {
                self.create_draft(&request.message, &sender, &external_id, ctx)
                    .await?
            }
        };
        let evidence = PreparedEvidence {
            v: PROTON_SUBMISSION_VERSION,
            operation: request.operation.as_str().to_string(),
            draft_id,
            external_id,
            sender: sender.email.clone(),
            digest: self.digest(request, &sender.email)?,
        };
        let reference = serde_json::to_string(&evidence).map_err(|error| {
            ProviderError::transient("could not encode the Proton submission evidence")
                .with_source_detail(error.to_string())
        })?;
        Ok(PreparedSubmission {
            version: PROTON_SUBMISSION_VERSION,
            backend: ProviderId::from(PROTON_PROVIDER),
            // Draft/reference/operation metadata only; no keys or tokens.
            raw: None,
            reference: Some(reference),
        })
    }

    async fn submit(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<SubmissionOutcome, ProviderError> {
        ctx.ensure_active()?;
        let Some(evidence) = self.evidence(request) else {
            return Ok(SubmissionOutcome::Rejected(ProviderError::forbidden(
                "the prepared Proton submission is invalid for this operation",
            )));
        };
        // The prepared evidence carries the sender alias, so a restart without
        // `with_sender_alias` still uses and validates the original address.
        let sender = self.sender_keys_for(&evidence.sender)?;
        self.policy.set_sender_alias(evidence.sender.clone());
        // Policy is resolved again immediately before delivery; a changed or
        // unreadable policy blocks now rather than using stale preparation.
        let materials = self.resolve_materials(request, ctx).await?;
        ctx.ensure_active()?;

        let http = self.client.request_http(ctx).await?;
        let draft = match self.fetch_message(&http, &evidence.draft_id, ctx).await {
            Ok(message) => message,
            Err(error) if error.kind == ErrorKind::NotFound => {
                return Ok(SubmissionOutcome::Rejected(error));
            }
            Err(error) => return Err(error),
        };
        if is_sent(&draft) {
            return Ok(SubmissionOutcome::Accepted(receipt(
                request,
                evidence.draft_id.clone(),
            )));
        }
        if !is_draft(&draft) {
            // A message in an unknown state cannot be proven unsent; never
            // send over it.
            return Ok(SubmissionOutcome::Unknown);
        }
        if draft.meta.external_id.as_deref() != Some(evidence.external_id.as_str())
            || !same_recipients(&request.message.to, &draft.meta.to_list)
            || !same_recipients(&request.message.cc, &draft.meta.cc_list)
            || !same_recipients(&request.message.bcc, &draft.meta.bcc_list)
        {
            return Ok(SubmissionOutcome::Rejected(ProviderError::forbidden(
                "the prepared Proton draft does not match this operation",
            )));
        }

        // Encrypt and upload every structured attachment to the prepared draft.
        let mut uploaded = Vec::with_capacity(request.attachments.len());
        let mut total = 0u64;
        for attachment in &request.attachments {
            let size = attachment.bytes.len() as u64;
            if size > MAX_ATTACHMENT_BYTES {
                return Ok(SubmissionOutcome::Rejected(ProviderError::unsupported(
                    "a submission attachment is larger than the client accepts",
                )));
            }
            total = total.saturating_add(size);
            if total > MAX_ATTACHMENT_BYTES {
                return Ok(SubmissionOutcome::Rejected(ProviderError::unsupported(
                    "the submission attachments exceed the client limit",
                )));
            }
            let up = encrypt_attachment(&sender, attachment)?;
            let id = http.finish(
                api::attachments::upload_attachment(
                    &http,
                    &attachment.filename,
                    &evidence.draft_id,
                    attachment.content_id.as_deref().unwrap_or_default(),
                    &attachment.content_type,
                    &up.key_packet,
                    &up.data_packet,
                    &up.signature,
                )
                .await,
            )?;
            uploaded.push(Uploaded {
                id,
                material: up.material,
            });
        }

        let packages = match build_packages(&sender, &request.message, &materials, &uploaded) {
            Ok(packages) => packages,
            Err(error) => return Ok(SubmissionOutcome::Rejected(error)),
        };
        let body = json!({
            "ExpirationTime": Value::Null,
            "AutoSaveContacts": 0,
            "Packages": packages,
        });
        match http.finish(api::messages::send_message(&http, &evidence.draft_id, body).await) {
            Ok(()) => Ok(SubmissionOutcome::Accepted(receipt(
                request,
                evidence.draft_id,
            ))),
            Err(error)
                if matches!(
                    error.kind,
                    ErrorKind::Forbidden
                        | ErrorKind::NotFound
                        | ErrorKind::Conflict
                        | ErrorKind::Unsupported
                        | ErrorKind::CursorInvalid
                ) =>
            {
                Ok(SubmissionOutcome::Rejected(error))
            }
            // A timeout, dropped connection, or server-side failure after the
            // request may still have been accepted. Never resend automatically.
            Err(_) => Ok(SubmissionOutcome::Unknown),
        }
    }

    async fn reconcile(
        &mut self,
        request: &SubmissionRequest,
        ctx: &OperationContext,
    ) -> Result<ReconcileOutcome, ProviderError> {
        ctx.ensure_active()?;
        let evidence = self.evidence(request);
        if request.prepared.is_some() && evidence.is_none() {
            // A prepared artifact we cannot authenticate is not proof either
            // way. Stay conservative.
            return Ok(ReconcileOutcome::Unknown);
        }
        let Some(evidence) = evidence else {
            // Preparation without a durable artifact can only have created a
            // draft. Finding it (or not) never proves delivery, but it prevents
            // a blind duplicate on the next prepare.
            let http = self.client.request_http(ctx).await?;
            let external_id = self.external_id(&request.operation);
            let _ = self.find_draft(&http, &external_id, ctx).await?;
            return Ok(ReconcileOutcome::NotAccepted);
        };

        let http = self.client.request_http(ctx).await?;
        match self.fetch_message(&http, &evidence.draft_id, ctx).await {
            Ok(message) if is_sent(&message) => Ok(ReconcileOutcome::Accepted(receipt(
                request,
                evidence.draft_id,
            ))),
            Ok(message) if is_draft(&message) => Ok(ReconcileOutcome::NotAccepted),
            Ok(_) => Ok(ReconcileOutcome::Unknown),
            // The draft may have been moved to Sent and its id no longer be
            // addressable; that is not evidence that delivery failed.
            Err(error) if error.kind == ErrorKind::NotFound => Ok(ReconcileOutcome::Unknown),
            Err(error) => Err(error),
        }
    }

    async fn finalize_sent(
        &mut self,
        _request: &SubmissionRequest,
        _receipt: &SubmissionReceipt,
        _ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        // Proton stores the Sent copy itself. Appending would duplicate it.
        Ok(())
    }
}

fn receipt(request: &SubmissionRequest, id: String) -> SubmissionReceipt {
    SubmissionReceipt {
        operation: request.operation.clone(),
        remote_message: Some(RemoteMessageId::new(id)),
        remote_thread: None::<RemoteThreadId>,
    }
}

fn encrypt_attachment(
    sender: &proton_core::crypto::keys::AddressKeys,
    attachment: &SubmissionAttachment,
) -> Result<UploadedAttachment, ProviderError> {
    let provider = proton_core::crypto::provider();
    proton_core::crypto::encrypt_attachment(&provider, sender, &attachment.bytes)
        .map_err(map_proton_error)
}

fn build_packages(
    sender: &proton_core::crypto::keys::AddressKeys,
    message: &OutgoingMessage,
    materials: &HashMap<String, super::policy::RecipientMaterial>,
    uploaded: &[Uploaded],
) -> Result<Vec<Value>, ProviderError> {
    let provider = proton_core::crypto::provider();
    let (material, data_packet) =
        proton_core::crypto::encrypt_for_transport(&provider, sender, &message.body)
            .map_err(map_proton_error)?;

    let entry_for = |public_key: &str, package: i64| -> Result<Value, ProviderError> {
        let body_key = proton_core::crypto::wrap_session_key(&provider, public_key, &material)
            .map_err(map_proton_error)?;
        let mut entry = json!({
            "Type": package,
            "BodyKeyPacket": STANDARD.encode(body_key),
            "Signature": 0,
        });
        if !uploaded.is_empty() {
            let mut attachment_keys = Map::new();
            for attachment in uploaded {
                let key = proton_core::crypto::wrap_session_key(
                    &provider,
                    public_key,
                    &attachment.material,
                )
                .map_err(map_proton_error)?;
                attachment_keys.insert(attachment.id.clone(), json!(STANDARD.encode(key)));
            }
            entry["AttachmentKeyPackets"] = Value::Object(attachment_keys);
        }
        Ok(entry)
    };

    let mut seen = HashSet::new();
    let mut internal = Map::new();
    let mut pgp = Map::new();
    let mut ordinary = Map::new();
    for address in message
        .to
        .iter()
        .chain(message.cc.iter())
        .chain(message.bcc.iter())
    {
        let email = normalize(&address.email);
        if email.is_empty() || !seen.insert(email.clone()) {
            continue;
        }
        let selected = materials.get(&email).ok_or_else(|| {
            ProviderError::unsupported("recipient policy material is unavailable")
        })?;
        match selected.security {
            DeliverySecurity::ProtonEncrypted => {
                let key = selected.public_key.as_deref().ok_or_else(|| {
                    ProviderError::unsupported("a Proton recipient key is unavailable")
                })?;
                internal.insert(email, entry_for(key, package_type::PM)?);
            }
            DeliverySecurity::ExternalOpenPgp => {
                let key = selected.public_key.as_deref().ok_or_else(|| {
                    ProviderError::unsupported("an external OpenPGP key is unavailable")
                })?;
                pgp.insert(email, entry_for(key, package_type::PGP_INLINE)?);
            }
            DeliverySecurity::ExternalOrdinary => {
                ordinary.insert(
                    email,
                    json!({ "Type": package_type::CLEAR, "Signature": 0 }),
                );
            }
        }
    }

    let self_key = proton_core::crypto::wrap_session_key_to_self(&provider, sender, &material)
        .map_err(map_proton_error)?;
    let mut packages = Vec::new();
    if !internal.is_empty() {
        packages.push(json!({
            "Addresses": Value::Object(internal),
            "MIMEType": "text/plain",
            "Type": package_type::PM,
            "Body": STANDARD.encode(&data_packet),
            "BodyKeyPacket": STANDARD.encode(&self_key),
        }));
    }
    if !pgp.is_empty() {
        packages.push(json!({
            "Addresses": Value::Object(pgp),
            "MIMEType": "text/plain",
            "Type": package_type::PGP_INLINE,
            "Body": STANDARD.encode(&data_packet),
            "BodyKeyPacket": STANDARD.encode(&self_key),
        }));
    }
    if !ordinary.is_empty() {
        let mut package = json!({
            "Addresses": Value::Object(ordinary),
            "MIMEType": "text/plain",
            "Type": package_type::CLEAR,
            "Body": STANDARD.encode(&data_packet),
            "BodyKey": {
                "Key": STANDARD.encode(material.key.as_slice()),
                "Algorithm": proton_core::crypto::algo_name(material.algo),
            },
        });
        if !uploaded.is_empty() {
            let mut keys = Map::new();
            for attachment in uploaded {
                keys.insert(
                    attachment.id.clone(),
                    json!({
                        "Key": STANDARD.encode(attachment.material.key.as_slice()),
                        "Algorithm": proton_core::crypto::algo_name(attachment.material.algo),
                    }),
                );
            }
            package["AttachmentKeys"] = Value::Object(keys);
        }
        packages.push(package);
    }
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_external_id_is_scoped_and_sanitized() {
        let submission = |operation: &str| {
            let account = Account {
                id: crate::account::AccountId("a".to_string()),
                label: "A".to_string(),
                email: "a@example.com".to_string(),
                display_name: "A".to_string(),
                backend: crate::account::MailBackend::proton(None),
                contact_sources: Vec::new(),
                sort_order: 0,
                color: None,
            };
            let store = std::sync::Arc::new(super::super::keys::UnlockedKeyStore::new());
            let client = ProtonClient::new(
                std::sync::Arc::new(NoTokens),
                store,
                super::super::client::ProtonEndpoints::production(),
            );
            ProtonSubmission::new(client, account)
                .external_id(&crate::providers::OperationId::new(operation))
        };
        assert_eq!(submission("abc/def"), "<pige-abc-def@pige.invalid>");
    }

    struct NoTokens;

    #[async_trait]
    impl crate::auth::TokenSource for NoTokens {
        async fn access_token(
            &self,
            _ctx: &OperationContext,
        ) -> Result<secrecy::SecretString, ProviderError> {
            Err(ProviderError::reauthenticate("synthetic"))
        }
    }
}
