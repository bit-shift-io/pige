//! Rustls TLS setup for the IMAP transport.
//!
//! Builds a connector from the Mozilla root program shipped by `webpki-roots`
//! and opens a TCP connection upgraded to TLS either implicitly ([`Security::Tls`])
//! or via the IMAP `STARTTLS` command ([`Security::StartTls`]). Certificate
//! validation uses rustls's normal rules; no custom roots or bypasses.

use std::sync::Arc;

use async_imap::Client;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::account::Security;
use crate::transport::TransportError;

/// A tokio TCP stream wrapped in a rustls client session.
pub type TlsStream = tokio_rustls::client::TlsStream<TcpStream>;

/// Build a [`TlsConnector`] that validates certificates against the Mozilla
/// root program.
fn tls_connector() -> Result<TlsConnector, TransportError> {
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError(format!("tls config: {e}")))?
        .with_root_certificates(root_store)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

/// Convert `host` into a rustls [`ServerName`].
fn server_name(host: &str) -> Result<rustls::pki_types::ServerName<'static>, TransportError> {
    rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|e| TransportError(format!("invalid host {host}: {e}")))
}

/// Ceiling on TCP connect + TLS handshake + server greeting. A host that
/// accepts the connection but never completes the handshake must not hang the
/// caller; per-message reads are bounded by the operation context instead.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Open `host:port`, consume the server greeting, and upgrade to TLS using
/// implicit TLS or `STARTTLS` as requested. Returns an unauthenticated
/// [`Client`] over a TLS stream in both modes; the caller authenticates. The
/// whole sequence is bounded by [`CONNECT_TIMEOUT`].
pub async fn open(
    host: &str,
    port: u16,
    security: Security,
) -> Result<Client<TlsStream>, TransportError> {
    open_bounded(host, port, security, CONNECT_TIMEOUT).await
}

async fn open_bounded(
    host: &str,
    port: u16,
    security: Security,
    timeout: std::time::Duration,
) -> Result<Client<TlsStream>, TransportError> {
    tokio::time::timeout(timeout, open_inner(host, port, security))
        .await
        .map_err(|_| TransportError(format!("connect {host}:{port}: timed out")))?
}

async fn open_inner(
    host: &str,
    port: u16,
    security: Security,
) -> Result<Client<TlsStream>, TransportError> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|e| TransportError(format!("connect {host}:{port}: {e}")))?;

    let connector = tls_connector()?;

    match security {
        Security::Tls => {
            let tls = connector
                .connect(server_name(host)?, tcp)
                .await
                .map_err(|e| TransportError(format!("tls handshake {host}: {e}")))?;
            let mut client = Client::new(tls);
            client
                .read_response()
                .await
                .map_err(|e| TransportError(format!("greeting {host}: {e}")))?
                .ok_or_else(|| TransportError(format!("no greeting from {host}")))?;
            Ok(client)
        }
        Security::StartTls => {
            let mut client = Client::new(tcp);
            client
                .read_response()
                .await
                .map_err(|e| TransportError(format!("greeting {host}: {e}")))?
                .ok_or_else(|| TransportError(format!("no greeting from {host}")))?;
            client
                .run_command_and_check_ok("STARTTLS", None)
                .await
                .map_err(|e| TransportError(format!("STARTTLS {host}: {e}")))?;
            let tcp = client.into_inner();
            let tls = connector
                .connect(server_name(host)?, tcp)
                .await
                .map_err(|e| TransportError(format!("tls handshake {host}: {e}")))?;
            Ok(Client::new(tls))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that accepts the TCP connection but never sends a greeting or
    /// completes the handshake must time out rather than hang the caller.
    #[tokio::test]
    async fn silent_server_does_not_hang_the_connect() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind silent server");
        let port = listener.local_addr().expect("addr").port();
        let started = std::time::Instant::now();
        let result = open_bounded(
            "127.0.0.1",
            port,
            Security::Tls,
            std::time::Duration::from_millis(50),
        )
        .await;

        assert!(result.is_err(), "a silent server must time out");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the connect timeout must fire promptly"
        );
    }
}
