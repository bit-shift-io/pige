//! Contact identity, normalization, and replay-safe correspondent learning.
//!
//! The module is deliberately independent of any contact provider. Learned
//! evidence comes from mail headers and accepted sends; remote address books
//! are a separate source merged later.

pub mod learn;
pub mod normalize;
pub mod rank;
pub mod sync;
pub mod types;

pub use learn::{observe_cached_message, observe_message, observe_outgoing};
pub use normalize::{normalize_address, normalize_email};
pub use rank::{MAX_QUERY_CHARS, MAX_SUGGESTIONS, RecipientCandidate};
pub use types::{
    AddressError, AddressKey, EvidenceId, HeaderRole, LearningOrigin, Observation,
    ObservationTrust, RecipientQuery, RecipientSuggestion, SuggestionSource,
};
