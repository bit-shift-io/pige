//! Core data model for the `pige` mail client. The desktop and Android shells
//! stay thin; accounts, the SQLite cache, and mail logic live here.
//!
//! This crate must not depend on iced or any Android UI code so it can run in
//! the sync worker and under unit tests on every platform.

pub mod account;
pub mod api;
pub mod mail;
pub mod platform;
pub mod store;
pub mod sync;
pub mod transport;

pub use account::{Account, AccountId, AuthKind, Security, ServerConfig};
pub use mail::{
    Address, Conversation, Flags, Folder, FolderId, FolderKind, MessageHeader, MessageId,
    StoredMessage, ThreadId, Uid,
};
pub use store::{Store, StoreError};

/// Bumped whenever the persisted schema changes; later tasks version the store
/// against this so a mismatched database can be migrated or rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_boots() {
        // `black_box` stops clippy from folding the constant and flagging the
        // assertion as constant-valued; the check still verifies a sane version.
        assert!(std::hint::black_box(SCHEMA_VERSION) >= 1);
    }
}
