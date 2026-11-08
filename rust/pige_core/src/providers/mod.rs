//! Provider-neutral contracts: identifiers, DTOs, errors, capability traits,
//! the application-owned registry, and a scripted fake.
//!
//! Adapters (IMAP/SMTP first, native Gmail/Proton later) implement the service
//! traits in [`traits`]. Everything above them — sync, mutations, submission,
//! contacts, and the UI — depends only on this module, so no SDK type, protocol
//! identifier, crypto, or wire error crosses the boundary.

pub mod error;
pub mod fake;
pub mod imap;
pub mod registry;
pub mod traits;
pub mod types;

pub use error::{ErrorKind, ProviderError};
pub use registry::ProviderRegistry;
pub use traits::{
    AddressBookService, MailboxService, MutationService, ProviderFactory, ProviderSession,
    SubmissionService, SyncService,
};
pub use types::*;
