//! Address canonicalization for correspondent identity.
//!
//! Only the domain is case-folded. The local part is deliberately left exactly
//! as the header delivered it: dots and `+` tags are routing decisions, and two
//! local parts that differ only in case may be different mailboxes on a
//! case-sensitive server.

use crate::mail::Address;

use super::types::{AddressError, AddressKey};

/// Canonicalize one address for storage and lookup.
pub fn normalize_address(address: &Address) -> Result<AddressKey, AddressError> {
    normalize_email(&address.email)
}

/// Canonicalize a raw `local@domain` string.
pub fn normalize_email(email: &str) -> Result<AddressKey, AddressError> {
    let trimmed = email.trim();
    if trimmed.is_empty() {
        return Err(AddressError::Empty);
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return Err(AddressError::MissingAt(trimmed.to_string()));
    };
    if local.is_empty() {
        return Err(AddressError::EmptyLocal(trimmed.to_string()));
    }
    if domain.is_empty() {
        return Err(AddressError::EmptyDomain(trimmed.to_string()));
    }
    if local.contains('@') {
        return Err(AddressError::MultipleAt(trimmed.to_string()));
    }
    if local
        .chars()
        .chain(domain.chars())
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(AddressError::Whitespace(trimmed.to_string()));
    }
    if domain.starts_with('.')
        || domain.ends_with('.')
        || domain.contains("..")
        || domain.contains('/')
        || domain.contains('@')
    {
        return Err(AddressError::InvalidDomain(trimmed.to_string()));
    }

    Ok(AddressKey(format!("{local}@{}", domain.to_lowercase())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_part_is_preserved_and_domain_is_folded() {
        let address = Address::new(None, "First.Last+Tag@Example.COM");
        assert_eq!(
            normalize_address(&address).unwrap(),
            AddressKey("First.Last+Tag@example.com".to_string())
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_but_inner_whitespace_is_rejected() {
        assert_eq!(
            normalize_email("  Bob@Example.com ").unwrap(),
            AddressKey("Bob@example.com".to_string())
        );
        assert!(normalize_email("Bob @example.com").is_err());
    }

    #[test]
    fn malformed_addresses_are_rejected() {
        assert!(normalize_email("").is_err());
        assert!(normalize_email("no-at-sign").is_err());
        assert!(normalize_email("@example.com").is_err());
        assert!(normalize_email("a@").is_err());
        assert!(normalize_email("a@@example.com").is_err());
        assert!(normalize_email("a@example..com").is_err());
    }
}
