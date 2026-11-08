//! Read-path sync: turning server state into the local cache.
//!
//! [`engine`] is the legacy IMAP transport-driven sync still used by the
//! application API. [`provider`] is the provider-neutral transactional
//! orchestrator, and [`scheduler`] serializes per-account work.

pub mod engine;
pub mod provider;
pub mod scheduler;

pub use engine::{NewMailSummary, SyncError, SyncReport, sync_account};
