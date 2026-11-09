//! Account configuration: identity, credentials kind, and server endpoints.

pub mod presets;

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::providers::{
    ContactSourceId, GMAIL_PROVIDER, IMAP_SMTP_PROVIDER, PROTON_PROVIDER, ProviderId,
};

/// Stable identifier for an account; never derived from the email address so
/// an account can be relabelled or re-authenticated without losing its cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }
}

impl Default for AccountId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How IMAP and SMTP connections protect their traffic. Plaintext is not
/// offered: the design requires TLS with normal certificate validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Security {
    Tls,
    StartTls,
}

impl Security {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::StartTls => "starttls",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "tls" => Some(Self::Tls),
            "starttls" => Some(Self::StartTls),
            _ => None,
        }
    }
}

/// Authentication mechanism. Only password auth ships in v1; OAuth can be
/// added as a new variant without changing the persisted account shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthKind {
    Password,
}

impl AuthKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "password" => Some(Self::Password),
            _ => None,
        }
    }
}

/// Endpoint plus the security mode used to reach it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

/// Accent colours an account can be tagged with. The UI renders the chosen
/// colour as a block behind the account's initials, which makes accounts easy
/// to tell apart in the merged inbox.
pub const ACCOUNT_COLORS: &[&str] = &[
    "#00897b", "#3949ab", "#7b1fa2", "#c2185b", "#e64a19", "#f9a825", "#2e7d32", "#546e7a",
];

/// The colour a new account receives when the caller does not choose one.
/// Cycling by sort order keeps consecutive accounts visually distinct.
pub fn default_account_color(sort_order: i64) -> &'static str {
    let index = sort_order.rem_euclid(ACCOUNT_COLORS.len() as i64) as usize;
    ACCOUNT_COLORS[index]
}

/// Whether `color` is one of the supported accent colours.
pub fn is_supported_account_color(color: &str) -> bool {
    ACCOUNT_COLORS.contains(&color)
}

/// The typed backend that owns an account's mailboxes, mail, and submission.
///
/// Accounts are versioned by backend: an IMAP/SMTP account keeps its servers,
/// while a native account carries only the provider-side user id. Provider SDK
/// types and protocol identifiers never appear here — only the neutral
/// [`ProviderId`] the registry dispatches on. A native backend deliberately has
/// no IMAP endpoint, so no fake servers are ever invented for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MailBackend {
    /// A legacy or manually configured IMAP (read) plus SMTP (submit) account.
    ImapSmtp {
        /// IMAP login name; often the full address but kept separate because
        /// some servers use a bare username.
        username: String,
        imap: ServerConfig,
        smtp: ServerConfig,
        auth: AuthKind,
    },
    /// A native Gmail account authenticated against Google's APIs.
    Gmail { user_id: String },
    /// A native Proton account. `user_id` is optional because some Proton
    /// sessions are keyed by username rather than a numeric user id.
    Proton { user_id: Option<String> },
}

impl MailBackend {
    /// Build the legacy IMAP/SMTP backend with password authentication.
    pub fn imap_smtp(username: impl Into<String>, imap: ServerConfig, smtp: ServerConfig) -> Self {
        Self::ImapSmtp {
            username: username.into(),
            imap,
            smtp,
            auth: AuthKind::Password,
        }
    }

    /// Build a native Gmail backend.
    pub fn gmail(user_id: impl Into<String>) -> Self {
        Self::Gmail {
            user_id: user_id.into(),
        }
    }

    /// Build a native Proton backend.
    pub fn proton(user_id: Option<String>) -> Self {
        Self::Proton { user_id }
    }
}

/// One contact source attached to an account. Accounts keep the sources they
/// enabled; the provider id says which adapter reads it and `kind` is an
/// adapter-defined source category (never a protocol token). A migrated v1
/// account has no sources and therefore no contacts or tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactSourceConfig {
    pub id: ContactSourceId,
    pub provider: ProviderId,
    pub kind: String,
    pub enabled: bool,
}

/// Native Google People saved contacts. The adapter owns the same stable id;
/// it lives here so account setup can register the source without depending on
/// the optional Google adapter module.
pub const GOOGLE_SAVED_CONTACTS_SOURCE_ID: &str = "google.saved";
/// Native Google People Other contacts.
pub const GOOGLE_OTHER_CONTACTS_SOURCE_ID: &str = "google.other";

/// The two R6 contact sources a native Google account gets when the user
/// enables contacts. Order is deliberate: saved contacts rank above Other.
pub fn google_contact_sources() -> Vec<ContactSourceConfig> {
    [
        GOOGLE_SAVED_CONTACTS_SOURCE_ID,
        GOOGLE_OTHER_CONTACTS_SOURCE_ID,
    ]
    .into_iter()
    .map(|id| ContactSourceConfig {
        id: ContactSourceId::from(id),
        provider: ProviderId::from(GMAIL_PROVIDER),
        kind: if id == GOOGLE_OTHER_CONTACTS_SOURCE_ID {
            "other".to_string()
        } else {
            "saved".to_string()
        },
        enabled: true,
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub label: String,
    pub email: String,
    pub display_name: String,
    /// Which backend serves this account. Versioned: the configuration document
    /// stores the backend shape directly, so a native account needs no IMAP
    /// endpoint and a Gmail-over-IMAP account is never silently converted.
    pub backend: MailBackend,
    /// Contact sources the user enabled for this account. Empty for accounts
    /// written before contacts existed and for IMAP-only accounts.
    #[serde(default)]
    pub contact_sources: Vec<ContactSourceConfig>,
    /// User-controlled ordering in the navigation rail.
    pub sort_order: i64,
    /// Accent colour as a `#rrggbb` string from [`ACCOUNT_COLORS`]. Older
    /// configuration files omit this, so a missing value falls back to the
    /// sort-order default.
    #[serde(default)]
    pub color: Option<String>,
}

impl Account {
    /// The backend that serves this account, as the registry's dispatch key.
    pub fn provider_id(&self) -> ProviderId {
        match &self.backend {
            MailBackend::ImapSmtp { .. } => ProviderId::from(IMAP_SMTP_PROVIDER),
            MailBackend::Gmail { .. } => ProviderId::from(GMAIL_PROVIDER),
            MailBackend::Proton { .. } => ProviderId::from(PROTON_PROVIDER),
        }
    }

    /// The IMAP/SMTP connection details, or `None` for a native backend. This
    /// is the only way protocol code reads the legacy fields, so a native
    /// account can never be mistaken for an IMAP account with empty servers.
    pub fn imap_smtp(&self) -> Option<(&str, &ServerConfig, &ServerConfig, AuthKind)> {
        match &self.backend {
            MailBackend::ImapSmtp {
                username,
                imap,
                smtp,
                auth,
            } => Some((username.as_str(), imap, smtp, *auth)),
            MailBackend::Gmail { .. } | MailBackend::Proton { .. } => None,
        }
    }

    /// The resolved accent colour, falling back to the sort-order default for
    /// configuration written before colours existed.
    pub fn resolved_color(&self) -> &str {
        self.color
            .as_deref()
            .filter(|color| is_supported_account_color(color))
            .unwrap_or_else(|| default_account_color(self.sort_order))
    }
}
