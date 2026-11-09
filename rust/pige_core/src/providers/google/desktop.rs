//! Desktop redirect ownership for the frozen S2 flow.
//!
//! S2 §9.3 freezes desktop Google authorization to a system browser plus a
//! loopback listener on `http://127.0.0.1:<random port>`, with PKCE (S256) and
//! `state` validated before any code is exchanged. This module owns only the
//! listener half; `GoogleAuthentication` builds the authorization URL and keeps
//! the per-attempt verifier/state.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::providers::ProviderError;

/// The redirect URI and, when the flow owns the loopback listener, the receiver
/// that waits for the browser's redirect.
pub struct DesktopRedirectFlow {
    pub redirect_uri: String,
    pub receiver: Option<Box<dyn DesktopRedirectReceiver>>,
}

/// Supplies the redirect URI for one attempt. Production uses
/// [`LoopbackRedirect`]; tests use a fixed URI.
pub trait DesktopRedirect: Send + Sync {
    fn prepare(&self) -> Result<DesktopRedirectFlow, ProviderError>;
}

/// Waits for one browser redirect and returns its full URL.
#[async_trait]
pub trait DesktopRedirectReceiver: Send + Sync {
    async fn receive(self: Box<Self>, timeout: Duration) -> Result<String, ProviderError>;
}

/// A fixed redirect URI with no listener. Used by tests and by a shell that
/// owns its own loopback listener.
pub struct FixedRedirect {
    uri: String,
}

impl FixedRedirect {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into() }
    }
}

impl DesktopRedirect for FixedRedirect {
    fn prepare(&self) -> Result<DesktopRedirectFlow, ProviderError> {
        Ok(DesktopRedirectFlow {
            redirect_uri: self.uri.clone(),
            receiver: None,
        })
    }
}

/// A loopback listener bound to `127.0.0.1` on an ephemeral port.
pub struct LoopbackRedirect;

impl LoopbackRedirect {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoopbackRedirect {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopRedirect for LoopbackRedirect {
    fn prepare(&self) -> Result<DesktopRedirectFlow, ProviderError> {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
            ProviderError::transient("could not open the local sign-in listener")
                .with_source_detail(error.to_string())
        })?;
        let address = listener.local_addr().map_err(|error| {
            ProviderError::transient("could not read the local sign-in address")
                .with_source_detail(error.to_string())
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            ProviderError::transient("could not prepare the local sign-in listener")
                .with_source_detail(error.to_string())
        })?;
        let listener = TcpListener::from_std(listener).map_err(|error| {
            ProviderError::transient("could not start the local sign-in listener")
                .with_source_detail(error.to_string())
        })?;
        Ok(DesktopRedirectFlow {
            redirect_uri: format!("http://127.0.0.1:{}/", address.port()),
            receiver: Some(Box::new(LoopbackReceiver { listener })),
        })
    }
}

struct LoopbackReceiver {
    listener: TcpListener,
}

#[async_trait]
impl DesktopRedirectReceiver for LoopbackReceiver {
    async fn receive(self: Box<Self>, timeout: Duration) -> Result<String, ProviderError> {
        let port = self
            .listener
            .local_addr()
            .map_err(|error| {
                ProviderError::transient("could not read the local sign-in address")
                    .with_source_detail(error.to_string())
            })?
            .port();
        let (mut socket, _) = tokio::time::timeout(timeout, self.listener.accept())
            .await
            .map_err(|_| ProviderError::transient("the Google sign-in timed out"))?
            .map_err(|error| {
                ProviderError::transient("could not accept the Google sign-in redirect")
                    .with_source_detail(error.to_string())
            })?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.map_err(|error| {
                ProviderError::transient("could not read the Google sign-in redirect")
                    .with_source_detail(error.to_string())
            })?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if buffer.len() > 16 * 1024 {
                return Err(ProviderError::transient(
                    "the Google sign-in redirect was too large",
                ));
            }
        }

        let request = String::from_utf8_lossy(&buffer);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .filter(|target| target.starts_with('/'))
            .ok_or_else(|| ProviderError::transient("the Google sign-in redirect was invalid"))?;

        let body = b"Pige sign-in received. You can close this window.";
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body).await;
        let _ = socket.shutdown().await;

        // `target` contains the path and query the browser sent. Rebuild the
        // full redirect URL so `GoogleAuthentication` validates the same
        // `state` and `code` it would have received from any other shell.
        Ok(format!("http://127.0.0.1:{port}{target}"))
    }
}
