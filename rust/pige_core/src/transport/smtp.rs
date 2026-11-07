//! SMTP submission over rustls. The transport is intentionally thin: the
//! outbox owns retries and state, so this module only maps an account's SMTP
//! settings and credentials into lettre and sends the exact raw bytes handed to
//! it.
//!
//! TLS is provided by lettre's rustls backend; native-tls/OpenSSL is never
//! linked.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::account::{Account, Security};
use crate::mail::Address;
use crate::transport::{SendError, SmtpSender};

/// A live-ish SMTP submission transport. The configuration is validated and a
/// NOOP connection test runs when the transport is built; deliveries reuse the
/// lettre connection pool.
pub struct SmtpClient {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl std::fmt::Debug for SmtpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SmtpClient").finish_non_exhaustive()
    }
}

/// Build an SMTP transport for `account` with an explicitly supplied password.
/// The connection test distinguishes an offline server (the outbox stays
/// queued) from a rejected message (the outbox row fails).
pub async fn connect(account: &Account, password: &str) -> Result<SmtpClient, SendError> {
    // Only an IMAP/SMTP backend has an SMTP endpoint. A native account returns
    // an explicit error rather than submitting to an invented server.
    let Some((username, _imap, smtp, _auth)) = account.imap_smtp() else {
        return Err(SendError(format!(
            "{} is not an IMAP/SMTP account",
            account.label
        )));
    };
    let host = smtp.host.trim();
    if host.is_empty() {
        return Err(SendError("SMTP host is empty".to_string()));
    }

    let builder = match smtp.security {
        Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        Security::StartTls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
    }
    .map_err(|error| SendError(format!("SMTP TLS setup for {host}: {error}")))?;

    let username = if username.trim().is_empty() {
        account.email.clone()
    } else {
        username.to_string()
    };
    let transport = builder
        .port(smtp.port)
        .credentials(Credentials::new(username, password.to_string()))
        .timeout(Some(Duration::from_secs(30)))
        .build();

    // Lettre connects lazily, but the outbox needs to distinguish "offline"
    // (leave queued and retry) from "the server rejected this message" (fail).
    // A cheap NOOP verifies reachability and authentication up front.
    match transport.test_connection().await {
        Ok(true) => {}
        Ok(false) => {
            return Err(SendError(format!(
                "SMTP server {} did not answer the connection test",
                smtp.host
            )));
        }
        Err(error) => {
            return Err(SendError(format!(
                "connect to {}:{}: {error}",
                smtp.host, smtp.port
            )));
        }
    }

    Ok(SmtpClient { transport })
}

fn mail_address(address: &Address) -> Result<lettre::Address, SendError> {
    lettre::Address::from_str(address.email.trim())
        .map_err(|error| SendError(format!("invalid recipient {}: {error}", address.email)))
}

fn envelope_recipients(recipients: &[Address]) -> Result<Vec<lettre::Address>, SendError> {
    recipients.iter().map(mail_address).collect()
}

#[async_trait]
impl SmtpSender for SmtpClient {
    async fn send_raw(
        &mut self,
        from: &Address,
        recipients: &[Address],
        raw: &[u8],
    ) -> Result<(), SendError> {
        if recipients.is_empty() {
            return Err(SendError("message has no recipients".to_string()));
        }
        let from = mail_address(from)?;
        let envelope = Envelope::new(Some(from), envelope_recipients(recipients)?)
            .map_err(|error| SendError(format!("SMTP envelope: {error}")))?;
        self.transport
            .send_raw(&envelope, raw)
            .await
            .map_err(|error| SendError(format!("SMTP send: {error}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AuthKind, ServerConfig};

    fn account(security: Security) -> Account {
        Account {
            id: crate::account::AccountId("a".to_string()),
            label: "Personal".to_string(),
            email: "ada@example.com".to_string(),
            display_name: "Ada".to_string(),
            backend: crate::account::MailBackend::ImapSmtp {
                username: "ada@example.com".to_string(),
                imap: ServerConfig {
                    host: "imap.example.com".to_string(),
                    port: 993,
                    security,
                },
                smtp: ServerConfig {
                    host: "smtp.example.com".to_string(),
                    port: 587,
                    security,
                },
                auth: AuthKind::Password,
            },
            contact_sources: Vec::new(),
            sort_order: 0,
            color: None,
        }
    }

    #[tokio::test]
    async fn empty_hosts_are_rejected_before_any_network_io() {
        let mut account = account(Security::Tls);
        if let crate::account::MailBackend::ImapSmtp { smtp, .. } = &mut account.backend {
            smtp.host.clear();
        } else {
            panic!("expected an IMAP/SMTP backend");
        }
        let error = connect(&account, "secret").await.expect_err("empty host");
        assert!(error.to_string().contains("SMTP host"));
    }

    #[test]
    fn invalid_recipients_are_rejected() {
        let error =
            envelope_recipients(&[Address::new(None, "not an address")]).expect_err("invalid");
        assert!(error.to_string().contains("invalid recipient"));
    }
}
