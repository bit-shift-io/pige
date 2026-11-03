//! Email address value used in message headers.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// Display name, when the header carried one.
    pub name: Option<String>,
    pub email: String,
}

impl Address {
    pub fn new(name: Option<String>, email: impl Into<String>) -> Self {
        Self {
            name,
            email: email.into(),
        }
    }

    /// Render the address in the form a compose field expects.
    pub fn to_header_string(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() => {
                let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
                format!("\"{escaped}\" <{}>", self.email)
            }
            _ => self.email.clone(),
        }
    }

    /// The display name when present, otherwise the bare email address.
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.email.clone())
    }
}

/// The display name of the first address in a list, used for conversation rows
/// and new-mail notifications. Empty when the list has no addresses.
pub fn sender_display(addresses: &[Address]) -> String {
    addresses
        .first()
        .map(Address::display_name)
        .unwrap_or_default()
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) if !name.is_empty() => write!(f, "{name} <{}>", self.email),
            _ => f.write_str(&self.email),
        }
    }
}
