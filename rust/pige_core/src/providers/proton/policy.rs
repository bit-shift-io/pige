//! Proton recipient security policy.
//!
//! This module is the single place that decides whether a message may be
//! submitted. It resolves every recipient through both the Proton key API and
//! the account's (possibly encrypted) contact cards, then classifies the
//! delivery as Proton E2E, external OpenPGP, or explicitly ordinary external.
//!
//! The critical invariant is fail-closed: a failed contact or key lookup, an
//! unreadable encrypted card, an unavailable mandatory key, or an unsupported
//! key-verification requirement blocks the *whole* operation. There is no
//! catch-all branch that turns an error into ordinary external delivery.
//!
//! Verified public keys and the chosen delivery class are retained privately in
//! [`ProtonRecipientPolicy`]; only [`RecipientDelivery`] values leave the
//! adapter. Nothing here is persisted, and nothing here holds a lock across an
//! await.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use secrecy::ExposeSecret;

use crate::mail::Address;
use crate::providers::{ErrorKind, OperationContext, ProviderError};
use crate::send::OutgoingMessage;

use proton_core::api;
use proton_core::api::contacts::{Contact, ContactCard, ContactEmail};
use proton_core::api::keys::{ApiPublicKey, RecipientKeys, pick_encryption_key};
use proton_core::crypto::keys::AddressKeys;
use proton_core::model::enums::{key_flag, recipient_type};
use proton_crypto::crypto::{DataEncoding, PGPProviderSync, Verifier, VerifierSync};

use super::client::ProtonClient;

/// The decrypted contact-card bytes we are willing to parse.
const MAX_CONTACT_CARD_BYTES: usize = 512 * 1024;
/// Contact enumeration is bounded so a broken server cannot loop forever.
const CONTACT_PAGE_SIZE: u32 = 100;
const MAX_CONTACT_PAGES: u32 = 1_000;

/// How a recipient's copy will be protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeliverySecurity {
    /// Proton-to-Proton end-to-end encryption.
    ProtonEncrypted,
    /// OpenPGP to an external recipient's public key.
    ExternalOpenPgp,
    /// Proton's ordinary external delivery. This is a deliberate,
    /// policy-resolved class, never an error fallback.
    ExternalOrdinary,
}

/// A provider-neutral recipient classification. It intentionally carries no
/// key material; verified keys stay inside [`ProtonRecipientPolicy`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecipientDelivery {
    pub address: Address,
    pub security: DeliverySecurity,
}

/// One contact's raw card/email material. Retained adapter-privately so the
/// address-book snapshot and the submission policy share one cache.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContactMaterial {
    pub cards: Vec<ContactCard>,
    pub emails: Vec<ContactEmail>,
}

impl ContactMaterial {
    pub(crate) fn from_contact(contact: &Contact) -> Self {
        Self {
            cards: contact.cards.clone(),
            emails: contact.emails.clone(),
        }
    }
}

#[derive(Default)]
struct StoreState {
    materials: Vec<ContactMaterial>,
    snapshot: Vec<ContactMaterial>,
    snapshot_active: bool,
    complete: bool,
}

/// Adapter-private cache shared by [`super::ProtonAddressBook`] and the
/// submission policy. A new snapshot is accumulated separately and only
/// replaces the previous material when the full snapshot completes, so an
/// interrupted sweep never erases a still-valid mandatory policy.
#[derive(Default)]
pub(crate) struct ContactPolicyStore {
    state: Mutex<StoreState>,
}

impl ContactPolicyStore {
    pub(crate) fn begin_snapshot(&self) {
        let mut state = self.lock();
        state.snapshot.clear();
        state.snapshot_active = true;
    }

    pub(crate) fn push_snapshot(&self, material: ContactMaterial) {
        let mut state = self.lock();
        if !state.snapshot_active {
            state.snapshot.clear();
            state.snapshot_active = true;
        }
        state.snapshot.push(material);
    }

    pub(crate) fn finish_snapshot(&self, complete: bool) {
        let mut state = self.lock();
        if complete {
            state.materials = std::mem::take(&mut state.snapshot);
            state.complete = true;
            state.snapshot_active = false;
        }
        // An incomplete snapshot keeps accumulating: the next page must not
        // start a fresh snapshot that would drop the already-seen pages (and
        // with them a mandatory policy).
    }

    /// Record a complete snapshot fetched outside the incremental address-book
    /// sweep. A sweep that is still accumulating is left untouched so this
    /// write cannot erase pages (and mandatory policy) it has already seen.
    pub(crate) fn replace_completed(&self, materials: Vec<ContactMaterial>) {
        let mut state = self.lock();
        state.materials = materials;
        state.complete = true;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RecipientMaterial {
    pub security: DeliverySecurity,
    pub public_key: Option<String>,
}

/// Resolves one outgoing message's recipient security before any draft or send
/// request is made.
///
/// Callers get only [`RecipientDelivery`] values. The verified key selected for
/// each address stays private and is consumed by the submission pipeline.
pub struct ProtonRecipientPolicy {
    client: ProtonClient,
    contacts: Arc<ContactPolicyStore>,
    sender_alias: Option<String>,
    resolved: HashMap<String, RecipientMaterial>,
}

impl ProtonRecipientPolicy {
    pub fn new(client: ProtonClient) -> Self {
        Self::with_contact_store(client, Arc::new(ContactPolicyStore::default()))
    }

    pub(crate) fn with_contact_store(
        client: ProtonClient,
        contacts: Arc<ContactPolicyStore>,
    ) -> Self {
        Self {
            client,
            contacts,
            sender_alias: None,
            resolved: HashMap::new(),
        }
    }

    /// Validate that a message claims to send from one of this account's
    /// unlocked Proton addresses. The sender is not part of
    /// [`OutgoingMessage`], so callers that expose an alias set it here.
    #[must_use]
    pub fn with_sender_alias(mut self, email: impl Into<String>) -> Self {
        self.sender_alias = Some(email.into());
        self
    }

    pub(crate) fn set_sender_alias(&mut self, email: impl Into<String>) {
        self.sender_alias = Some(email.into());
    }

    fn validate_sender(&self) -> Result<(), ProviderError> {
        let Some(sender) = self.sender_alias.as_deref() else {
            return Ok(());
        };
        let sender = sender.trim();
        self.client.unlocked_keys().with_keys(|keys| {
            keys.address_for_email(sender)
                .filter(|address| !address.keys.is_empty())
                .map(|_| ())
                .ok_or_else(|| {
                    ProviderError::forbidden(format!(
                        "the sender address {sender} is not an unlocked Proton address"
                    ))
                })
        })?
    }

    /// Resolve every recipient, fail closed on any missing/invalid policy, and
    /// retain the selected keys privately for the submission step.
    pub async fn resolve(
        &mut self,
        message: &OutgoingMessage,
        ctx: &OperationContext,
    ) -> Result<Vec<RecipientDelivery>, ProviderError> {
        ctx.ensure_active()?;
        self.validate_sender()?;
        self.resolved.clear();

        if message.recipients().is_empty() {
            return Err(ProviderError::forbidden("the message has no recipients"));
        }

        // A failed contact snapshot means we cannot affirm ordinary external
        // delivery, so it blocks the operation rather than degrading.
        let materials = self.fetch_contact_materials(ctx).await?;
        self.contacts.replace_completed(materials.clone());

        let mut seen = HashSet::new();
        let mut deliveries = Vec::new();
        for address in message.recipients() {
            let email = address.email.trim();
            if email.is_empty() {
                return Err(ProviderError::forbidden("a recipient address is empty"));
            }
            let key = normalize_email(email);
            if !seen.insert(key.clone()) {
                continue;
            }
            // Key lookup failures are never permission to send cleartext.
            let keys = self.fetch_recipient_keys(email, ctx).await?;
            let contact = contact_policy(&self.client, &materials, email)?;
            let (security, public_key) = classify(&keys, contact.as_ref())?;
            self.resolved.insert(
                key,
                RecipientMaterial {
                    security,
                    public_key,
                },
            );
            deliveries.push(RecipientDelivery {
                address: address.clone(),
                security,
            });
        }
        ctx.ensure_active()?;
        Ok(deliveries)
    }

    /// Adapter-private material for a resolved address.
    pub(crate) fn material_for(&self, email: &str) -> Option<&RecipientMaterial> {
        self.resolved.get(&normalize_email(email))
    }

    async fn fetch_recipient_keys(
        &self,
        email: &str,
        ctx: &OperationContext,
    ) -> Result<RecipientKeys, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let keys = http.finish(api::keys::get_all_public_keys(&http, email).await)?;
        ctx.ensure_active()?;
        Ok(keys)
    }

    async fn fetch_contact_materials(
        &self,
        ctx: &OperationContext,
    ) -> Result<Vec<ContactMaterial>, ProviderError> {
        ctx.ensure_active()?;
        let http = self.client.request_http(ctx).await?;
        let mut materials = Vec::new();
        let mut page = 0u32;
        loop {
            ctx.ensure_active()?;
            let (total, contacts) =
                http.finish(api::contacts::list(&http, page, CONTACT_PAGE_SIZE).await)?;
            let page_end = u64::from(page.saturating_add(1)) * u64::from(CONTACT_PAGE_SIZE);
            let at_end = page_end >= u64::from(total);
            if contacts.is_empty() && !at_end {
                // An empty page before the reported total is an inconsistent
                // snapshot; treating it as complete could hide a mandatory
                // policy.
                return Err(ProviderError::transient(
                    "the Proton contact policy snapshot was incomplete",
                ));
            }
            materials.extend(contacts.iter().map(ContactMaterial::from_contact));
            if at_end {
                break;
            }
            page = page.saturating_add(1);
            if page >= MAX_CONTACT_PAGES {
                return Err(ProviderError::transient(
                    "the Proton contact policy snapshot was too large",
                ));
            }
        }
        Ok(materials)
    }
}

/// A public key pinned by a contact card plus the provenance of the card that
/// supplied it. Only a card with a detached signature verified against one of
/// the account's own keys is authenticated; an unsigned or invalid card can
/// still provide a key for optional encryption but never satisfies a mandatory
/// verification requirement.
#[derive(Debug, Clone, Default)]
struct ContactKey {
    armored: String,
    authenticated: bool,
}

#[derive(Debug, Clone, Default)]
struct ContactPolicy {
    /// `Some(true)` means encryption is mandatory; `Some(false)` is an explicit
    /// ordinary preference; `None` means the card said nothing.
    encrypt: Option<bool>,
    sign: Option<bool>,
    verify: Option<bool>,
    keys: Vec<ContactKey>,
}

impl ContactPolicy {
    fn merge(&mut self, other: ContactPolicy) {
        self.encrypt = merge_required(self.encrypt, other.encrypt);
        self.sign = merge_required(self.sign, other.sign);
        self.verify = merge_required(self.verify, other.verify);
        for key in other.keys {
            if key.armored.trim().is_empty() {
                continue;
            }
            if let Some(existing) = self.keys.iter_mut().find(|k| k.armored == key.armored) {
                // The same key pinned by an authenticated card is authenticated
                // even if another card that mentions it was not.
                existing.authenticated |= key.authenticated;
            } else {
                self.keys.push(key);
            }
        }
    }
}

fn merge_required(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    }
}

fn normalize_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

/// Combine every card for every contact that mentions `email`. An encrypted
/// card that cannot be decrypted fails closed; it is never skipped as if the
/// contact had no policy.
fn contact_policy(
    client: &ProtonClient,
    materials: &[ContactMaterial],
    email: &str,
) -> Result<Option<ContactPolicy>, ProviderError> {
    let target = normalize_email(email);
    let mut combined: Option<ContactPolicy> = None;
    let mut matched = false;
    for material in materials {
        let contact_emails: HashSet<String> = material
            .emails
            .iter()
            .map(|entry| normalize_email(&entry.email))
            .filter(|entry| !entry.is_empty())
            .collect();
        if !contact_emails.contains(&target) {
            continue;
        }
        matched = true;
        for card in &material.cards {
            let text = card_text(client, card)?;
            let authenticated = card_signature_verified(client, card);
            let parsed = parse_card_text(&text, authenticated);
            // Global card properties apply to every email in this contact.
            let mut policy = parsed.global;
            for (card_email, per_email) in parsed.per_email {
                if normalize_email(&card_email) == target {
                    policy.merge(per_email);
                }
            }
            let card_mentions_target = parsed
                .emails
                .iter()
                .map(|value| normalize_email(value))
                .any(|value| value == target);
            if !parsed.emails.is_empty() && !card_mentions_target {
                // A card scoped to another address must not impose its policy
                // on this recipient, but a pinned key is harmless to retain.
                let keys_only = ContactPolicy {
                    keys: policy.keys,
                    ..ContactPolicy::default()
                };
                combined
                    .get_or_insert_with(ContactPolicy::default)
                    .merge(keys_only);
                continue;
            }
            combined
                .get_or_insert_with(ContactPolicy::default)
                .merge(policy);
        }
    }
    Ok(matched.then(|| combined.unwrap_or_default()))
}

fn card_text(client: &ProtonClient, card: &ContactCard) -> Result<String, ProviderError> {
    let kind = card.card_type.trim().to_ascii_uppercase();
    let data = card.data.trim();
    if data.is_empty() {
        return Ok(String::new());
    }
    if kind.contains("ENCRYPTED") {
        return decrypt_card(client, data);
    }
    if kind.contains("CLEAR") || kind.contains("SIGNED") || kind.contains("CERTIFICATE") {
        return Ok(unescape_card(data));
    }
    // Unknown card kinds are only safe to parse when they are visibly plain
    // text. Anything else is unsupported policy, not an empty policy.
    if data.contains("BEGIN:VCARD") || data.contains("BEGIN PGP PUBLIC KEY BLOCK") {
        return Ok(unescape_card(data));
    }
    Err(policy_error(
        "this Proton contact card uses an unsupported format",
    ))
}

/// Verify the card's detached signature against the account's own user/address
/// keys. A missing, unreadable, or invalid signature is unauthenticated
/// provenance, not a transient failure: the card can still describe optional
/// policy, but cannot satisfy a mandatory key-verification requirement.
fn card_signature_verified(client: &ProtonClient, card: &ContactCard) -> bool {
    let signature = card.signature.trim();
    if signature.is_empty() || card.data.trim().is_empty() {
        return false;
    }
    let provider = proton_core::crypto::provider();
    let data = card.data.as_bytes();
    // A card signature may be armored or raw binary. Proton stores armored
    // detached signatures; accepting base64-wrapped binary as a fallback keeps
    // this robust without guessing which key signed it.
    let binary = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        signature.as_bytes(),
    )
    .ok();
    client
        .unlocked_keys()
        .with_keys(|keys| {
            let mut public_keys = Vec::new();
            for stored in keys
                .user_keys
                .iter()
                .chain(keys.addresses.iter().flat_map(|address| address.keys.iter()))
            {
                let Ok(private) = provider.private_key_import(
                    stored.armored.as_bytes(),
                    stored.passphrase.expose_secret().as_bytes(),
                    DataEncoding::Armor,
                ) else {
                    continue;
                };
                if let Ok(public) = provider.private_key_to_public_key(&private) {
                    public_keys.push(public);
                }
            }
            if public_keys.is_empty() {
                return false;
            }
            let armored = provider
                .new_verifier()
                .with_verification_keys(public_keys.iter())
                .verify_detached(data, signature.as_bytes(), DataEncoding::Armor)
                .is_ok();
            if armored {
                return true;
            }
            match binary {
                Some(binary) => provider
                    .new_verifier()
                    .with_verification_keys(public_keys.iter())
                    .verify_detached(data, binary, DataEncoding::Bytes)
                    .is_ok(),
                None => false,
            }
        })
        .unwrap_or(false)
}

fn decrypt_card(client: &ProtonClient, data: &str) -> Result<String, ProviderError> {
    let provider = proton_core::crypto::provider();
    client.unlocked_keys().with_keys(|keys| {
        let mut candidates: Vec<AddressKeys> = keys.addresses.clone();
        if !keys.user_keys.is_empty() {
            candidates.push(AddressKeys {
                address_id: String::new(),
                email: String::new(),
                keys: keys.user_keys.clone(),
            });
        }
        if candidates.is_empty() {
            return Err(ProviderError::reauthenticate(
                "unlock the Proton keys to read contact policy",
            ));
        }
        let mut last_error = None;
        for address in &candidates {
            match proton_core::crypto::decrypt_body_bounded(
                &provider,
                address,
                &[],
                data,
                MAX_CONTACT_CARD_BYTES,
            ) {
                Ok((bytes, _)) => return Ok(String::from_utf8_lossy(&bytes).into_owned()),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        Err(
            policy_error("an encrypted Proton contact policy could not be read")
                .with_source_detail(last_error.unwrap_or_default()),
        )
    })?
}

fn unescape_card(data: &str) -> String {
    data.replace("\\n", "\n").replace("\\r", "\r")
}

#[derive(Debug, Default)]
struct ParsedCard {
    emails: Vec<String>,
    global: ContactPolicy,
    per_email: Vec<(String, ContactPolicy)>,
}

fn parse_card_text(text: &str, authenticated: bool) -> ParsedCard {
    let mut parsed = ParsedCard::default();
    // A bare public key card carries no vCard policy, but its key is usable.
    if text
        .trim_start()
        .starts_with("-----BEGIN PGP PUBLIC KEY BLOCK-----")
    {
        if let Some(key) = extract_key(text) {
            parsed.global.keys.push(ContactKey {
                armored: key,
                authenticated,
            });
        }
        return parsed;
    }

    let unfolded = unfold_lines(text);
    for line in unfolded {
        let Some((left, value)) = line.split_once(':') else {
            continue;
        };
        let mut parts = left.split(';');
        let name = parts.next().unwrap_or("").trim().to_ascii_uppercase();
        let value = value.trim();
        if name.is_empty() {
            continue;
        }
        let params: HashMap<String, String> = parts
            .filter_map(|part| part.split_once('='))
            .map(|(key, value)| {
                (
                    key.trim().to_ascii_uppercase(),
                    value.trim().trim_matches('"').to_string(),
                )
            })
            .collect();
        match name.as_str() {
            "EMAIL" => {
                let email = normalize_email(value.trim_start_matches("mailto:"));
                if email.is_empty() {
                    continue;
                }
                parsed.emails.push(email.clone());
                let scoped = ContactPolicy {
                    encrypt: params
                        .get("X-PM-ENCRYPT")
                        .and_then(|value| parse_bool(value)),
                    sign: params.get("X-PM-SIGN").and_then(|value| parse_bool(value)),
                    verify: params
                        .get("X-PM-VERIFY")
                        .or_else(|| params.get("X-PM-KEY-VERIFY"))
                        .and_then(|value| parse_bool(value)),
                    keys: Vec::new(),
                };
                if scoped.encrypt.is_some() || scoped.sign.is_some() || scoped.verify.is_some() {
                    parsed.per_email.push((email, scoped));
                }
            }
            "X-PM-ENCRYPT" | "X-PM-ENCRYPTION" => {
                parsed.global.encrypt = parse_bool(value);
            }
            "X-PM-SIGN" => {
                parsed.global.sign = parse_bool(value);
            }
            "X-PM-VERIFY" | "X-PM-KEY-VERIFY" | "X-PM-VERIFY-KEY" => {
                parsed.global.verify = parse_bool(value);
            }
            "KEY" | "X-PM-PUBLIC-KEY" | "X-PM-PGP-KEY" | "X-PM-ENCRYPT-KEY" => {
                if let Some(key) = extract_key(value) {
                    parsed.global.keys.push(ContactKey {
                        armored: key,
                        authenticated,
                    });
                }
            }
            _ => {}
        }
    }
    parsed
}

fn unfold_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.replace("\r\n", "\n").lines() {
        if (line.starts_with(' ') || line.starts_with('\t'))
            && let Some(previous) = lines.last_mut()
        {
            previous.push_str(&line[1..]);
            continue;
        }
        lines.push(line.to_string());
    }
    lines
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn extract_key(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let decoded = if let Some((_, encoded)) = value.split_once("base64,") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    } else {
        Some(unescape_card(value))
    }?;
    let begin = decoded.find("-----BEGIN PGP PUBLIC KEY BLOCK-----")?;
    let end = decoded.find("-----END PGP PUBLIC KEY BLOCK-----")?;
    let end = end + "-----END PGP PUBLIC KEY BLOCK-----".len();
    Some(decoded[begin..end].to_string())
}

fn classify(
    keys: &RecipientKeys,
    contact: Option<&ContactPolicy>,
) -> Result<(DeliverySecurity, Option<String>), ProviderError> {
    let internal_key = keys
        .keys
        .iter()
        .find(|key| usable_internal_key(key))
        .or_else(|| pick_encryption_key(&keys.keys).filter(|key| usable_internal_key(key)));
    if let Some(key) = internal_key {
        // Proton recipients are always E2E; a contact's ordinary preference
        // cannot turn internal mail into cleartext.
        return Ok((
            DeliverySecurity::ProtonEncrypted,
            Some(key.public_key.clone()),
        ));
    }
    if keys.recipient_type == recipient_type::INTERNAL || !keys.keys.is_empty() {
        return Err(policy_error(
            "this Proton recipient has no usable, trusted encryption key",
        ));
    }

    let contact_keys = contact.map(|policy| policy.keys.as_slice()).unwrap_or(&[]);
    let contact_key = contact_keys
        .iter()
        .find(|key| !key.armored.trim().is_empty())
        .map(|key| key.armored.clone());
    let authenticated_contact_key = contact_keys
        .iter()
        .find(|key| key.authenticated && !key.armored.trim().is_empty())
        .map(|key| key.armored.clone());
    let api_key = keys
        .unverified
        .iter()
        .find(|key| usable_external_key(key))
        .map(|key| key.public_key.clone());

    let encryption_required = contact.and_then(|policy| policy.encrypt) == Some(true);
    // `X-PM-SIGN` asks for message signing, not proof that the pinned key was
    // authenticated by the contact-card signature. Only `X-PM-VERIFY` imposes
    // verified-key provenance; folding sign into it over-blocked sign-only
    // contacts.
    let verification_required = contact.and_then(|policy| policy.verify) == Some(true);

    if verification_required {
        // A mandatory verification requirement may only be satisfied by a key
        // from a card whose detached signature verifies against an account key.
        // A published API key or a key from an unsigned card is unauthenticated
        // provenance and must block the whole submission, even though the key
        // could technically encrypt.
        let key = authenticated_contact_key.ok_or_else(|| {
            policy_error(
                "this recipient requires verified OpenPGP key material from an authenticated contact card",
            )
        })?;
        return Ok((DeliverySecurity::ExternalOpenPgp, Some(key)));
    }

    if encryption_required {
        let key = contact_key.or(api_key).ok_or_else(|| {
            policy_error("this recipient requires encryption but no usable key is available")
        })?;
        return Ok((DeliverySecurity::ExternalOpenPgp, Some(key)));
    }

    if let Some(key) = contact_key.or(api_key) {
        // A published/contact key is honored even when encryption is optional.
        return Ok((DeliverySecurity::ExternalOpenPgp, Some(key)));
    }

    // Both lookups succeeded and the contact policy does not require
    // encryption, so ordinary external delivery is an explicit classification.
    Ok((DeliverySecurity::ExternalOrdinary, None))
}

fn usable_internal_key(key: &ApiPublicKey) -> bool {
    key.flags & key_flag::NOT_OBSOLETE != 0
        && key.flags & key_flag::NOT_COMPROMISED != 0
        && key.flags & key_flag::EMAIL_NO_ENCRYPT == 0
        && !key.public_key.trim().is_empty()
}

fn usable_external_key(key: &ApiPublicKey) -> bool {
    key_flag::can_encrypt(key.flags)
        && key.flags & key_flag::NOT_COMPROMISED != 0
        && key.flags & key_flag::EMAIL_NO_ENCRYPT == 0
        && !key.public_key.trim().is_empty()
}

fn policy_error(message: &str) -> ProviderError {
    ProviderError::new(
        ErrorKind::Unsupported,
        format!("recipient policy: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_mandatory_policy_over_optional() {
        let mut policy = ContactPolicy {
            encrypt: Some(false),
            ..ContactPolicy::default()
        };
        policy.merge(ContactPolicy {
            encrypt: Some(true),
            ..ContactPolicy::default()
        });
        assert_eq!(policy.encrypt, Some(true));
    }

    #[test]
    fn parses_encrypt_and_sign_properties() {
        let parsed = parse_card_text(
            "BEGIN:VCARD\nVERSION:4.0\nEMAIL:alice@example.com\nX-PM-ENCRYPT:true\nX-PM-SIGN:false\nEND:VCARD",
            false,
        );
        assert_eq!(parsed.emails, vec!["alice@example.com"]);
        assert_eq!(parsed.global.encrypt, Some(true));
        assert_eq!(parsed.global.sign, Some(false));
    }

    #[test]
    fn incomplete_snapshot_keeps_the_previous_policy() {
        let store = ContactPolicyStore::default();
        let previous = ContactMaterial {
            cards: vec![ContactCard {
                card_type: "CLEAR".into(),
                data: "BEGIN:VCARD\nX-PM-ENCRYPT:true\nEND:VCARD".into(),
                signature: String::new(),
            }],
            emails: vec![ContactEmail {
                id: "e1".into(),
                email: "alice@example.com".into(),
                name: String::new(),
                contact_id: "c1".into(),
                extra: Default::default(),
            }],
        };
        store.replace_completed(vec![previous.clone()]);
        store.begin_snapshot();
        store.push_snapshot(previous);
        store.finish_snapshot(false);
        assert_eq!(store.lock().materials.len(), 1, "old policy is retained");

        // A retried later page continues the same snapshot rather than
        // clearing the pages already collected.
        store.push_snapshot(ContactMaterial::default());
        store.finish_snapshot(true);
        assert_eq!(store.lock().materials.len(), 2);

        store.begin_snapshot();
        store.finish_snapshot(true);
        assert!(store.lock().materials.is_empty());
    }

    #[test]
    fn sign_only_policy_does_not_demand_verified_keys() {
        let policy = ContactPolicy {
            sign: Some(true),
            ..ContactPolicy::default()
        };
        let keys = RecipientKeys {
            recipient_type: recipient_type::EXTERNAL,
            keys: Vec::new(),
            unverified: Vec::new(),
        };
        let (security, key) = classify(&keys, Some(&policy)).expect("sign-only is not verification");
        assert_eq!(security, DeliverySecurity::ExternalOrdinary);
        assert!(key.is_none());
    }

    #[test]
    fn verification_requires_authenticated_contact_key_provenance() {
        let keys = RecipientKeys {
            recipient_type: recipient_type::EXTERNAL,
            keys: Vec::new(),
            unverified: Vec::new(),
        };
        let unauthenticated = ContactPolicy {
            verify: Some(true),
            keys: vec![ContactKey {
                armored: "-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n-----END PGP PUBLIC KEY BLOCK-----".into(),
                authenticated: false,
            }],
            ..ContactPolicy::default()
        };
        assert!(classify(&keys, Some(&unauthenticated)).is_err());

        let authenticated = ContactPolicy {
            verify: Some(true),
            keys: vec![ContactKey {
                armored: "-----BEGIN PGP PUBLIC KEY BLOCK-----\nkey\n-----END PGP PUBLIC KEY BLOCK-----".into(),
                authenticated: true,
            }],
            ..ContactPolicy::default()
        };
        let (security, key) =
            classify(&keys, Some(&authenticated)).expect("authenticated provenance");
        assert_eq!(security, DeliverySecurity::ExternalOpenPgp);
        assert!(key.is_some());
    }

    #[test]
    fn external_key_usability_honors_not_compromised() {
        let base = ApiPublicKey {
            public_key: "key".into(),
            flags: key_flag::NOT_OBSOLETE | key_flag::NOT_COMPROMISED,
            primary: 1,
            source: 0,
        };
        assert!(usable_external_key(&base));
        let compromised = ApiPublicKey {
            flags: key_flag::NOT_OBSOLETE,
            ..base
        };
        assert!(!usable_external_key(&compromised));
    }

    #[test]
    fn classifies_internal_only_with_a_trusted_key() {
        let trusted = ApiPublicKey {
            public_key: "key".into(),
            flags: key_flag::NOT_OBSOLETE | key_flag::NOT_COMPROMISED,
            primary: 1,
            source: 0,
        };
        let keys = RecipientKeys {
            recipient_type: recipient_type::INTERNAL,
            keys: vec![trusted],
            unverified: Vec::new(),
        };
        let (security, _) = classify(&keys, None).unwrap();
        assert_eq!(security, DeliverySecurity::ProtonEncrypted);

        let compromised = ApiPublicKey {
            public_key: "key".into(),
            flags: key_flag::NOT_OBSOLETE,
            primary: 1,
            source: 0,
        };
        let keys = RecipientKeys {
            recipient_type: recipient_type::INTERNAL,
            keys: vec![compromised],
            unverified: Vec::new(),
        };
        assert!(classify(&keys, None).is_err());
    }
}
