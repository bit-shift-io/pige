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

/// Open `host:port`, consume the server greeting, and upgrade to TLS using
/// implicit TLS or `STARTTLS` as requested. Returns an unauthenticated
/// [`Client`] over a TLS stream in both modes; the caller authenticates.
pub async fn open(
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
