//! Provider presets for account onboarding.
//!
//! Presets only fill in well-known IMAP/SMTP endpoints. Authentication stays
//! password-based in v1: Gmail requires an app password and iCloud/Outlook need
//! OAuth, so those providers are not offered in the account form yet.

use super::Security;

/// A recognised mail provider, or fully manual configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderPreset {
    Gmail,
    Fastmail,
    ICloud,
    Manual,
}

impl ProviderPreset {
    /// The presets the account form offers. iCloud remains in the enum for the
    /// planned OAuth path but is intentionally absent until OAuth ships.
    pub const CHOICES: [Self; 3] = [Self::Gmail, Self::Fastmail, Self::Manual];

    pub fn label(self) -> &'static str {
        match self {
            Self::Gmail => "Gmail",
            Self::Fastmail => "Fastmail",
            Self::ICloud => "iCloud",
            Self::Manual => "Manual",
        }
    }

    /// A short hint shown under the provider chips.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Gmail => "Use a Google app password, not your Google account password.",
            Self::Fastmail => "Use an app password generated in Fastmail settings.",
            Self::ICloud => "iCloud requires OAuth and is not available yet.",
            Self::Manual => "Enter the IMAP and SMTP servers from your provider.",
        }
    }
}

/// The server defaults a preset fills into the account form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountDefaults {
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub security: Security,
}

/// Resolve the defaults for one provider. Manual has empty hosts and zero
/// ports so the form does not claim a server the user has not chosen.
pub fn preset(name: ProviderPreset) -> AccountDefaults {
    match name {
        ProviderPreset::Gmail => AccountDefaults {
            imap_host: "imap.gmail.com",
            imap_port: 993,
            smtp_host: "smtp.gmail.com",
            smtp_port: 465,
            security: Security::Tls,
        },
        ProviderPreset::Fastmail => AccountDefaults {
            imap_host: "imap.fastmail.com",
            imap_port: 993,
            smtp_host: "smtp.fastmail.com",
            smtp_port: 465,
            security: Security::Tls,
        },
        ProviderPreset::ICloud => AccountDefaults {
            imap_host: "imap.mail.me.com",
            imap_port: 993,
            smtp_host: "smtp.mail.me.com",
            smtp_port: 587,
            security: Security::Tls,
        },
        ProviderPreset::Manual => AccountDefaults {
            imap_host: "",
            imap_port: 0,
            smtp_host: "",
            smtp_port: 0,
            security: Security::Tls,
        },
    }
}

/// Best-effort preset detection for editing an existing account.
pub fn detect(imap_host: &str, smtp_host: &str) -> ProviderPreset {
    let imap_host = imap_host.trim().to_ascii_lowercase();
    let smtp_host = smtp_host.trim().to_ascii_lowercase();
    for name in [
        ProviderPreset::Gmail,
        ProviderPreset::Fastmail,
        ProviderPreset::ICloud,
    ] {
        let defaults = preset(name);
        if defaults.imap_host.eq_ignore_ascii_case(&imap_host)
            && defaults.smtp_host.eq_ignore_ascii_case(&smtp_host)
        {
            return name;
        }
    }
    ProviderPreset::Manual
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_fill_known_endpoints() {
        let gmail = preset(ProviderPreset::Gmail);
        assert_eq!((gmail.imap_host, gmail.imap_port), ("imap.gmail.com", 993));
        assert_eq!((gmail.smtp_host, gmail.smtp_port), ("smtp.gmail.com", 465));

        let fastmail = preset(ProviderPreset::Fastmail);
        assert_eq!(
            (fastmail.imap_host, fastmail.imap_port),
            ("imap.fastmail.com", 993)
        );
        assert_eq!(
            (fastmail.smtp_host, fastmail.smtp_port),
            ("smtp.fastmail.com", 465)
        );
    }

    #[test]
    fn manual_preset_has_no_endpoints_and_known_hosts_are_detected() {
        let manual = preset(ProviderPreset::Manual);
        assert!(manual.imap_host.is_empty());
        assert!(manual.smtp_host.is_empty());

        assert_eq!(
            detect("IMAP.GMAIL.COM", "smtp.gmail.com"),
            ProviderPreset::Gmail
        );
        assert_eq!(
            detect("imap.example.com", "smtp.example.com"),
            ProviderPreset::Manual
        );
    }

    #[test]
    fn icloud_is_never_offered_in_the_password_only_form() {
        assert!(!ProviderPreset::CHOICES.contains(&ProviderPreset::ICloud));
    }
}
