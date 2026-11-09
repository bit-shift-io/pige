//! Pure header-to-observation extraction.
//!
//! None of this touches the store. A caller learns from provider
//! [`MessageRecord`]s (live sync and history), from already-cached headers
//! (`ExistingCache`), or from an accepted outgoing message. Invalid addresses
//! are skipped one by one; they never abort a page.

use std::collections::HashSet;

use serde::Serialize;

use crate::account::AccountId;
use crate::mail::{Address, MessageHeader};
use crate::providers::MessageRecord;
use crate::send::OutgoingMessage;

use super::normalize::normalize_address;
use super::types::{AddressKey, HeaderRole, LearningOrigin, Observation, ObservationTrust};

/// Observe every address role in one provider message.
///
/// The result is deterministic and deduplicated per `(address, role)`. An
/// unsent draft produces no evidence, and an absent Bcc header never produces a
/// Bcc observation.
pub fn observe_message(
    _account: &AccountId,
    message: &MessageRecord,
    origin: LearningOrigin,
) -> Vec<Observation> {
    if message.flags.draft {
        return Vec::new();
    }
    let trust = if !matches!(
        origin,
        LearningOrigin::AcceptedSend | LearningOrigin::Selection
    ) && is_low_trust_mailbox(message)
    {
        // Spam/Trash observations stay available for later corroboration but
        // are suppressed from ordinary ranking until a deliberate signal
        // exists.
        Some(ObservationTrust::SpamOrTrash)
    } else {
        None
    };
    observations_from_header(&message.header, origin, trust)
}

/// Whether the message is only known through a Spam/Trash collection. Gmail
/// exposes system label ids (`SPAM`, `TRASH`); IMAP exposes mailbox names
/// (`Junk`, `[Gmail]/Trash`); both are checked case-insensitively.
fn is_low_trust_mailbox(message: &MessageRecord) -> bool {
    message.collections.iter().any(|collection| {
        let id = collection.as_str().trim().to_ascii_lowercase();
        matches!(id.as_str(), "spam" | "trash" | "junk")
            || id.ends_with("/spam")
            || id.ends_with("/trash")
            || id.ends_with("/junk")
    })
}

/// Observe the recipients of an accepted outgoing message.
///
/// Bcc is learned here because the user authored it and it is available; it is
/// never inferred for received mail. `last_seen` is the outbox row's creation
/// time so replay is deterministic.
pub fn observe_outgoing(
    _account: &AccountId,
    message: &OutgoingMessage,
    last_seen: i64,
) -> Vec<Observation> {
    let mut observations = Vec::new();
    let mut seen = HashSet::new();
    for (role, addresses) in [
        (HeaderRole::To, &message.to),
        (HeaderRole::Cc, &message.cc),
        (HeaderRole::Bcc, &message.bcc),
    ] {
        push_role(
            &mut observations,
            &mut seen,
            role,
            addresses,
            last_seen,
            true,
            ObservationTrust::Deliberate,
        );
    }
    observations
}

/// Observe the roles available in an already-cached message.
///
/// F5's `messages` table does not persist `Reply-To` or `Bcc`, so the
/// `ExistingCache` backfill deliberately learns only From/To/Cc. Provider
/// history and live sync consume the original [`MessageRecord`] header and do
/// see those roles. This is the documented ExistingCache exclusion; no message
/// schema migration is required for R1.
pub fn observe_cached_message(
    _account: &AccountId,
    header: &MessageHeader,
    draft: bool,
) -> Vec<Observation> {
    if draft {
        return Vec::new();
    }
    let mut observations = Vec::new();
    let mut seen = HashSet::new();
    for (role, addresses) in [
        (HeaderRole::From, &header.from),
        (HeaderRole::To, &header.to),
        (HeaderRole::Cc, &header.cc),
    ] {
        push_role(
            &mut observations,
            &mut seen,
            role,
            addresses,
            header.date,
            false,
            ObservationTrust::Ordinary,
        );
    }
    observations
}

/// A deterministic, normalized fingerprint of the header fields that identify a
/// message across sources.
///
/// This is a *secondary* dedup hint only: native message/operation identity is
/// primary. It exists so an IMAP UID reset, a label copy, an ExistingCache
/// backfill, or a Sent-copy replay with a different native id does not multiply
/// observed frequency. It deliberately excludes provider ids, collections,
/// mailbox names, and size.
///
/// It also excludes `Reply-To` and `Bcc`, because not every source can observe
/// them: the local cache does not persist those columns (so `ExistingCache`
/// reads them empty), a server may strip `Bcc` from its Sent copy, and only the
/// prepared submit bytes may carry it. Including them would give the *same*
/// message two different fingerprints on those paths and double-count it.
/// Roles still come from the header that was actually observed; excluding them
/// here can only undercount genuinely distinct messages that differ solely by
/// `Reply-To`/`Bcc`, which the plan explicitly prefers.
pub(crate) fn header_fingerprint(header: &MessageHeader) -> String {
    #[derive(Serialize)]
    struct FingerprintParts {
        message_id: Option<String>,
        in_reply_to: Option<String>,
        references: Vec<String>,
        subject: String,
        from: Vec<String>,
        to: Vec<String>,
        cc: Vec<String>,
        date: i64,
    }

    let parts = FingerprintParts {
        message_id: header.message_id.clone(),
        in_reply_to: header.in_reply_to.clone(),
        references: header.references.clone(),
        subject: header.subject.clone(),
        from: fingerprint_addresses(&header.from),
        to: fingerprint_addresses(&header.to),
        cc: fingerprint_addresses(&header.cc),
        date: header.date,
    };
    serde_json::to_string(&parts).unwrap_or_default()
}

fn fingerprint_addresses(addresses: &[Address]) -> Vec<String> {
    addresses
        .iter()
        .map(|address| match normalize_address(address) {
            Ok(key) => key.0,
            // An invalid address is included verbatim so two different broken
            // headers do not collide merely because both were skipped.
            Err(_) => format!("invalid:{}", address.email.trim()),
        })
        .collect()
}

fn observations_from_header(
    header: &MessageHeader,
    origin: LearningOrigin,
    trust_override: Option<ObservationTrust>,
) -> Vec<Observation> {
    let outgoing = matches!(
        origin,
        LearningOrigin::AcceptedSend | LearningOrigin::Selection
    );
    let trust = trust_override.unwrap_or(match origin {
        LearningOrigin::AcceptedSend | LearningOrigin::Selection => ObservationTrust::Deliberate,
        LearningOrigin::ExistingCache | LearningOrigin::History | LearningOrigin::LiveArrival => {
            ObservationTrust::Ordinary
        }
    });
    let mut observations = Vec::new();
    let mut seen = HashSet::new();
    for (role, addresses) in [
        (HeaderRole::From, &header.from),
        (HeaderRole::ReplyTo, &header.reply_to),
        (HeaderRole::To, &header.to),
        (HeaderRole::Cc, &header.cc),
        (HeaderRole::Bcc, &header.bcc),
    ] {
        push_role(
            &mut observations,
            &mut seen,
            role,
            addresses,
            header.date,
            outgoing,
            trust,
        );
    }
    observations
}

#[allow(clippy::too_many_arguments)]
fn push_role(
    observations: &mut Vec<Observation>,
    seen: &mut HashSet<(AddressKey, HeaderRole)>,
    role: HeaderRole,
    addresses: &[Address],
    last_seen: i64,
    outgoing: bool,
    trust: ObservationTrust,
) {
    for address in addresses {
        // Skip one invalid address without losing the rest of the header/page.
        let Ok(key) = normalize_address(address) else {
            continue;
        };
        if !seen.insert((key.clone(), role)) {
            continue;
        }
        observations.push(Observation {
            key,
            address: Address::new(address.name.clone(), address.email.trim()),
            role,
            last_seen,
            outgoing,
            trust,
        });
    }
}
