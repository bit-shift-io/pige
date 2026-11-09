//! The adapter-internal Google API client and its endpoint configuration.
//!
//! Only `providers/google` may name a Google API host. The mail and People
//! adapters (G2/G3) share one [`GoogleClient`] per account so one [`TokenSource`]
//! serves both; no token, SDK type, or wire error escapes this module.

use std::sync::Arc;

use secrecy::SecretString;
use url::Url;

use crate::auth::TokenSource;
use crate::providers::{OperationContext, ProviderError};

/// The API roots used by the Google adapters. Production callers use
/// [`GoogleEndpoints::production`]; tests inject a local mock root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoogleEndpoints {
    pub gmail_base: Url,
    pub people_base: Url,
}

impl GoogleEndpoints {
    pub fn production() -> Self {
        Self {
            gmail_base: Url::parse("https://gmail.googleapis.com")
                .expect("the Gmail API base is a valid URL"),
            people_base: Url::parse("https://people.googleapis.com")
                .expect("the People API base is a valid URL"),
        }
    }
}

/// One account's authenticated Google API client.
///
/// `tokens` is shared by the Gmail and People services opened for the account;
/// it performs single-flight, generation-safe refresh through the F3
/// [`TokenSource`] contract.
#[derive(Clone)]
pub struct GoogleClient {
    tokens: Arc<dyn TokenSource>,
    endpoints: GoogleEndpoints,
}

impl GoogleClient {
    pub fn new(tokens: Arc<dyn TokenSource>, endpoints: GoogleEndpoints) -> Self {
        Self { tokens, endpoints }
    }

    pub fn endpoints(&self) -> &GoogleEndpoints {
        &self.endpoints
    }

    pub fn token_source(&self) -> Arc<dyn TokenSource> {
        self.tokens.clone()
    }

    /// Fetch a current access token, bounded by the caller's operation context.
    pub async fn access_token(
        &self,
        ctx: &OperationContext,
    ) -> Result<SecretString, ProviderError> {
        self.tokens.access_token(ctx).await
    }
}

impl std::fmt::Debug for GoogleClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GoogleClient")
            .field("endpoints", &self.endpoints)
            .field("tokens", &"<authenticated token source>")
            .finish()
    }
}
