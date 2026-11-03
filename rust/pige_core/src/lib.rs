//! Core data model for the `pige` mail client. The desktop and Android shells
//! stay thin; the storage schema and mail logic will live here.

/// Bumped whenever the persisted schema changes; later tasks version the store
/// against this so a mismatched database can be migrated or rebuilt.
pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_boots() {
        assert!(SCHEMA_VERSION >= 1);
    }
}
