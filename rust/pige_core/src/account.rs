//! Account configuration: identity, credentials kind, and server endpoints.

pub mod presets;

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub label: String,
    pub email: String,
    pub display_name: String,
    /// IMAP login name; often the full address but kept separate because some
    /// servers use a bare username.
    pub username: String,
    pub imap: ServerConfig,
    pub smtp: ServerConfig,
    pub auth: AuthKind,
    /// User-controlled ordering in the navigation rail.
    pub sort_order: i64,
    /// Accent colour as a `#rrggbb` string from [`ACCOUNT_COLORS`]. Older
    /// configuration files omit this, so a missing value falls back to the
    /// sort-order default.
    #[serde(default)]
    pub color: Option<String>,
}

impl Account {
    /// The resolved accent colour, falling back to the sort-order default for
    /// configuration written before colours existed.
    pub fn resolved_color(&self) -> &str {
        self.color
            .as_deref()
            .filter(|color| is_supported_account_color(color))
            .unwrap_or_else(|| default_account_color(self.sort_order))
    }
}
