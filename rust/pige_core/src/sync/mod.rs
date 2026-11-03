//! Read-path sync: turning server state into the local cache.

pub mod engine;

pub use engine::{NewMailSummary, SyncError, SyncReport, sync_account};
