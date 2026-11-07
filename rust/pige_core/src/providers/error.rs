//! The provider-neutral error surface.
//!
//! Every capability returns [`ProviderError`] instead of an SDK or protocol
//! error. Adapters map their failures to an [`ErrorKind`] and a safe, already
//! redacted message; wire payloads and credentials never cross the boundary.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A coarse, provider-neutral classification of a failure. Callers branch on
/// this instead of parsing messages: retries, reauthentication, and "the write
/// may have happened" are all decided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// No usable network.
    Offline,
    /// A temporary failure; retrying later may succeed.
    Transient,
    /// The provider rate-limited the request.
    RateLimited,
    /// Credentials are expired or rejected; interactive auth is required.
    Reauthenticate,
    /// Authenticated, but not permitted to perform the operation.
    Forbidden,
    /// The provider or session genuinely cannot do this.
    Unsupported,
    /// A stored cursor could not be interpreted; reconcile from scratch.
    CursorInvalid,
    /// A concurrent change conflicts with this write; re-read and retry.
    Conflict,
    /// The addressed remote object does not exist.
    NotFound,
    /// The operation was cancelled or its deadline passed.
    Cancelled,
    /// A non-idempotent write's outcome is unknown; never retry blindly.
    UnknownWriteOutcome,
}

/// A redacted provider failure.
///
/// `safe_message` is presentation-ready and must not contain wire data.
/// `source_detail` is adapter-local diagnostic context (which may echo a
/// protocol payload); it is deliberately excluded from both `Display` and
/// `Debug` so a wire payload cannot leak through ordinary logging.
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
#[error("{safe_message}")]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub safe_message: String,
    pub retry_after: Option<Duration>,
    source_detail: Option<String>,
}

impl ProviderError {
    pub fn new(kind: ErrorKind, safe_message: impl Into<String>) -> Self {
        Self {
            kind,
            safe_message: safe_message.into(),
            retry_after: None,
            source_detail: None,
        }
    }

    /// Suggest waiting this long before retrying a transient/rate-limited call.
    #[must_use]
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// Attach adapter-local diagnostic detail. It is redacted from `Display`
    /// and `Debug`; never place credentials in it.
    #[must_use]
    pub fn with_source_detail(mut self, detail: impl Into<String>) -> Self {
        self.source_detail = Some(detail.into());
        self
    }

    /// The adapter-local detail, for adapter-side structured logging only.
    pub fn source_detail(&self) -> Option<&str> {
        self.source_detail.as_deref()
    }

    pub fn offline(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Offline, message)
    }

    pub fn transient(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transient, message)
    }

    pub fn rate_limited(retry_after: Duration) -> Self {
        Self::new(ErrorKind::RateLimited, "provider rate limit reached")
            .with_retry_after(retry_after)
    }

    pub fn reauthenticate(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Reauthenticate, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Forbidden, message)
    }

    /// The session/capability is absent. `what` names the missing capability.
    pub fn unsupported(what: impl AsRef<str>) -> Self {
        Self::new(
            ErrorKind::Unsupported,
            format!("provider does not support {}", what.as_ref()),
        )
    }

    pub fn cursor_invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::CursorInvalid, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Cancelled, message)
    }

    pub fn unknown_write_outcome(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::UnknownWriteOutcome, message)
    }
}

impl fmt::Debug for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderError")
            .field("kind", &self.kind)
            .field("safe_message", &self.safe_message)
            .field("retry_after", &self.retry_after)
            .field(
                "source_detail",
                &self.source_detail.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}
