//! Portable configuration: accounts and settings as one readable JSON document.
//!
//! This is the source of truth for account and preference data; the SQLite
//! cache is rebuilt from the servers and only mirrors the accounts so foreign
//! keys keep cleaning up. Nothing here touches the network or a UI.
//!
//! The document is written atomically (temp file, fsync, rename, directory
//! fsync) and, on Unix, with mode `0600`: it holds email addresses, usernames,
//! and server hostnames, never a password.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::account::Account;
use crate::settings::Settings;

/// Format version written into the document. Bumped only for an incompatible
/// change to the document shape.
pub const CURRENT_VERSION: u32 = 1;

/// Everything pige keeps in `pige.json`: the accounts and the settings. The
/// mail cache is deliberately absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigDocument {
    pub version: u32,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub settings: Settings,
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            accounts: Vec::new(),
            settings: Settings::default(),
        }
    }
}

/// Failures reading or writing the configuration document.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no configuration directory is available")]
    NoRoot,
}

/// The configuration root for the environment values that decide it: the XDG
/// Base Directory specification uses `xdg_config_home` only when it is an
/// absolute path, otherwise `<home>/.config`, and otherwise there is no place
/// to keep anything. The vendor path is `<base>/bitshift/pige`.
pub fn config_root_from(
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let base = match xdg_config_home.filter(|path| path.is_absolute()) {
        Some(xdg) => xdg.to_path_buf(),
        None => home
            .filter(|path| path.is_absolute())
            .ok_or(ConfigError::NoRoot)?
            .join(".config"),
    };
    Ok(base.join("bitshift").join("pige"))
}

/// The configuration root for this process.
pub fn config_root() -> Result<PathBuf, ConfigError> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    config_root_from(xdg.as_deref(), home.as_deref())
}

/// The document file inside a configuration root.
pub fn document_path(root: &Path) -> PathBuf {
    root.join("pige.json")
}

/// Read the document. `Ok(None)` means the file does not exist; a file that
/// exists but cannot be read or parsed is an error so the caller can quarantine
/// or report it instead of silently replacing the user's configuration.
pub fn load(path: &Path) -> Result<Option<ConfigDocument>, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

/// Write the document atomically and durably.
pub fn save(path: &Path, doc: &ConfigDocument) -> Result<(), ConfigError> {
    let bytes = serde_json::to_vec_pretty(doc)?;
    write_atomic(path, &bytes)?;
    Ok(())
}

/// Move an unreadable document aside so a later save cannot destroy it, and
/// return where it went.
pub fn quarantine(path: &Path) -> Result<PathBuf, ConfigError> {
    let mut target = path.as_os_str().to_owned();
    target.push(".corrupt");
    let target = PathBuf::from(target);
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Keep a copy of the current document before a restore replaces it, when one
/// exists.
pub fn pre_restore_copy(path: &Path) -> Result<(), ConfigError> {
    if path.exists() {
        let mut target = path.as_os_str().to_owned();
        target.push(".pre-restore");
        std::fs::copy(path, PathBuf::from(target))?;
    }
    Ok(())
}

/// Replace `path` with `bytes` so a crash or a concurrent reader sees either
/// the old contents or the new ones, never a mixture: a unique temp file in the
/// same directory, synced, renamed over the target, then the directory synced.
/// Adapted from qlaunch's `store/atomic.rs` (bitshift, MIT), with mode `0600`.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let (dir, name) = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => (dir, name),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a file path",
            ));
        }
    };
    if !dir.as_os_str().is_empty() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let written = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok::<(), std::io::Error>(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    sync_dir(dir)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, AuthKind, Security, ServerConfig};

    fn account(label: &str) -> Account {
        Account {
            id: AccountId::new(),
            label: label.to_string(),
            email: "ada@example.com".to_string(),
            display_name: "Ada".to_string(),
            username: "ada@example.com".to_string(),
            imap: ServerConfig {
                host: "imap.example.com".to_string(),
                port: 993,
                security: Security::Tls,
            },
            smtp: ServerConfig {
                host: "smtp.example.com".to_string(),
                port: 465,
                security: Security::Tls,
            },
            auth: AuthKind::Password,
            sort_order: 0,
        }
    }

    #[test]
    fn root_is_xdg_config_home_bitshift_pige() {
        assert_eq!(
            config_root_from(Some(Path::new("/x")), None).unwrap(),
            PathBuf::from("/x/bitshift/pige")
        );
    }

    #[test]
    fn root_rejects_relative_xdg_and_falls_back_to_home() {
        assert_eq!(
            config_root_from(Some(Path::new("rel")), Some(Path::new("/home/me"))).unwrap(),
            PathBuf::from("/home/me/.config/bitshift/pige")
        );
    }

    #[test]
    fn root_errors_without_xdg_or_home() {
        assert!(config_root_from(None, None).is_err());
        assert!(config_root_from(Some(Path::new("rel")), Some(Path::new("rel"))).is_err());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = document_path(dir.path());
        let mut doc = ConfigDocument::default();
        doc.accounts.push(account("Personal"));
        doc.settings.notifications = false;
        save(&path, &doc).unwrap();

        assert_eq!(load(&path).unwrap(), Some(doc));
    }

    #[test]
    fn load_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&document_path(dir.path())).unwrap(), None);
    }

    #[test]
    fn load_corrupt_is_an_error_not_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = document_path(dir.path());
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(matches!(load(&path), Err(ConfigError::Json(_))));
    }

    #[test]
    fn quarantine_renames_the_bad_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = document_path(dir.path());
        std::fs::write(&path, b"broken").unwrap();
        let moved = quarantine(&path).unwrap();
        assert_eq!(moved, PathBuf::from(format!("{}.corrupt", path.display())));
        assert!(!path.exists());
        assert!(moved.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = document_path(dir.path());
        save(&path, &ConfigDocument::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
