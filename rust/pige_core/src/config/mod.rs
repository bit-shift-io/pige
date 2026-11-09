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

use crate::account::{Account, AccountId, AuthKind, MailBackend, ServerConfig};
use crate::settings::Settings;

/// Format version written into the document. Bumped only for an incompatible
/// change to the document shape.
///
/// - v1: an account carried `username`/`imap`/`smtp`/`auth` inline.
/// - v2: an account carries a typed `backend` and `contact_sources`. A native
///   backend needs no IMAP endpoint, so no fake servers are invented for it.
pub const CURRENT_VERSION: u32 = 2;

/// Everything pige keeps in `pige.json`: the accounts and the settings. The
/// mail cache is deliberately absent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigDocument {
    pub version: u32,
    pub accounts: Vec<Account>,
    pub settings: Settings,
    /// Bare sender addresses the user has blocked. Excluded from local
    /// recipient suggestions; no inbox filtering reads this yet.
    pub blocked_senders: Vec<String>,
}

impl<'de> Deserialize<'de> for ConfigDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Single choke point: every deserialization goes through the version
        // reader, so a v1 document can never be read back as a v2 document and
        // a newer format is rejected here too.
        let value = serde_json::Value::deserialize(deserializer)?;
        let bytes = serde_json::to_vec(&value).map_err(serde::de::Error::custom)?;
        parse_document(&bytes).map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            accounts: Vec::new(),
            settings: Settings::default(),
            blocked_senders: Vec::new(),
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
    /// The file was written by a newer build (format `found`, this build
    /// supports `supported`). It must never be overwritten, and callers treat
    /// it like an unreadable file: keep it, start from the cache, block saves.
    #[error("configuration format {found} is newer than supported format {supported}")]
    TooNew { found: u32, supported: u32 },
}

/// The v1 wire shape of one account: protocol fields inline, no backend.
#[derive(Debug, Deserialize)]
struct AccountV1 {
    id: AccountId,
    label: String,
    email: String,
    display_name: String,
    username: String,
    imap: ServerConfig,
    smtp: ServerConfig,
    auth: AuthKind,
    sort_order: i64,
    #[serde(default)]
    color: Option<String>,
}

impl AccountV1 {
    /// Migrate a v1 account into the v2 shape. The IMAP/SMTP backend is kept
    /// exactly, so a Gmail-over-IMAP account stays an IMAP account and no
    /// contacts or tokens are invented.
    fn into_account(self) -> Account {
        Account {
            id: self.id,
            label: self.label,
            email: self.email,
            display_name: self.display_name,
            backend: MailBackend::ImapSmtp {
                username: self.username,
                imap: self.imap,
                smtp: self.smtp,
                auth: self.auth,
            },
            contact_sources: Vec::new(),
            sort_order: self.sort_order,
            color: self.color,
        }
    }
}

fn default_version_one() -> u32 {
    1
}

/// The v1 document wire shape, read explicitly rather than being coerced into
/// the v2 shape. The `version` field is validated by [`VersionProbe`] before
/// this reader runs, so it is deliberately not part of the struct.
#[derive(Debug, Deserialize)]
struct DocumentV1 {
    #[serde(default)]
    accounts: Vec<AccountV1>,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    blocked_senders: Vec<String>,
}

/// The v2 document wire shape. `Account` deserializes its typed `backend`
/// directly; there is no "missing endpoint" fallback here.
#[derive(Debug, Deserialize)]
struct DocumentV2 {
    #[serde(default)]
    accounts: Vec<Account>,
    #[serde(default)]
    settings: Settings,
    #[serde(default)]
    blocked_senders: Vec<String>,
}

/// Only the version field, read first so an unknown newer format is rejected
/// before the rest of the document is interpreted.
#[derive(Debug, Deserialize)]
struct VersionProbe {
    #[serde(default = "default_version_one")]
    version: u32,
}

/// Parse a document of any supported version into the current shape, migrating
/// v1 accounts to typed backends. A format newer than [`CURRENT_VERSION`] is
/// rejected with [`ConfigError::TooNew`] so callers never overwrite it.
pub fn parse_document(bytes: &[u8]) -> Result<ConfigDocument, ConfigError> {
    let probe: VersionProbe = serde_json::from_slice(bytes)?;
    if probe.version > CURRENT_VERSION {
        return Err(ConfigError::TooNew {
            found: probe.version,
            supported: CURRENT_VERSION,
        });
    }

    if probe.version >= 2 {
        let wire: DocumentV2 = serde_json::from_slice(bytes)?;
        Ok(ConfigDocument {
            version: CURRENT_VERSION,
            accounts: wire.accounts,
            settings: wire.settings,
            blocked_senders: wire.blocked_senders,
        })
    } else {
        let wire: DocumentV1 = serde_json::from_slice(bytes)?;
        Ok(ConfigDocument {
            version: CURRENT_VERSION,
            accounts: wire
                .accounts
                .into_iter()
                .map(AccountV1::into_account)
                .collect(),
            settings: wire.settings,
            blocked_senders: wire.blocked_senders,
        })
    }
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
        // Any supported older version is read through its own wire reader and
        // migrated in memory; a newer format is rejected, not reinterpreted.
        Ok(bytes) => Ok(Some(parse_document(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

/// Write the document atomically and durably. The recorded version is always
/// [`CURRENT_VERSION`], so a document constructed with a stale version field
/// cannot be written back out as an older format.
pub fn save(path: &Path, doc: &ConfigDocument) -> Result<(), ConfigError> {
    let normalized = ConfigDocument {
        version: CURRENT_VERSION,
        ..doc.clone()
    };
    let bytes = serde_json::to_vec_pretty(&normalized)?;
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
    use crate::account::{AccountId, AuthKind, MailBackend, Security, ServerConfig};

    fn account(label: &str) -> Account {
        Account {
            id: AccountId::new(),
            label: label.to_string(),
            email: "ada@example.com".to_string(),
            display_name: "Ada".to_string(),
            backend: MailBackend::ImapSmtp {
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
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
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
    fn blocked_senders_round_trip_and_an_old_document_still_loads() {
        let mut doc = ConfigDocument::default();
        doc.blocked_senders.push("spam@example.com".to_string());
        let json = serde_json::to_string(&doc).unwrap();
        let parsed: ConfigDocument = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.blocked_senders, vec!["spam@example.com".to_string()]);

        let old = r#"{"version":1,"accounts":[],"settings":{"sync_minutes":15}}"#;
        let parsed: ConfigDocument = serde_json::from_str(old).unwrap();
        assert!(parsed.blocked_senders.is_empty());
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
