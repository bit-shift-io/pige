//! The adapter-internal Google API client and its endpoint configuration.
//!
//! Only `providers/google` may name a Google API host. The mail and People
//! adapters (G2/G3) share one [`GoogleClient`] per account so one [`TokenSource`]
//! serves both; no token, SDK type, or wire error escapes this module.

use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use url::Url;

use crate::auth::TokenSource;
use crate::providers::{ErrorKind, OperationContext, ProviderError};

use super::normalize::METADATA_HEADERS;

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
    http: reqwest::Client,
}

impl GoogleClient {
    pub fn new(tokens: Arc<dyn TokenSource>, endpoints: GoogleEndpoints) -> Self {
        Self {
            tokens,
            endpoints,
            http: reqwest::Client::new(),
        }
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

/// One Gmail API user-scoped client. It stays `pub(crate)` so no Gmail wire
/// type or endpoint is visible outside `providers/google`.
#[derive(Clone)]
pub(crate) struct GmailApi {
    client: GoogleClient,
    user: String,
}

impl GmailApi {
    pub(crate) fn new(client: GoogleClient, user: impl Into<String>) -> Self {
        Self {
            client,
            user: user.into(),
        }
    }

    fn url(&self, tail: &[&str]) -> Result<Url, ProviderError> {
        let mut url = self.client.endpoints().gmail_base.clone();
        let mut segments = url.path_segments_mut().map_err(|_| {
            ProviderError::new(ErrorKind::Unsupported, "the Gmail API base URL is unusable")
        })?;
        segments.pop_if_empty();
        segments
            .push("gmail")
            .push("v1")
            .push("users")
            .push(&self.user);
        for segment in tail {
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }

    pub(crate) async fn profile(
        &self,
        ctx: &OperationContext,
    ) -> Result<ProfileResponse, ProviderError> {
        self.client.get_json(ctx, self.url(&["profile"])?).await
    }

    pub(crate) async fn labels(
        &self,
        ctx: &OperationContext,
    ) -> Result<LabelsResponse, ProviderError> {
        self.client.get_json(ctx, self.url(&["labels"])?).await
    }

    pub(crate) async fn list_messages(
        &self,
        label: Option<&str>,
        page_token: Option<&str>,
        max_results: u32,
        include_spam_trash: bool,
        ctx: &OperationContext,
    ) -> Result<MessageListResponse, ProviderError> {
        let mut query = vec![("maxResults", max_results.to_string())];
        if let Some(label) = label {
            query.push(("labelIds", label.to_string()));
        }
        if let Some(token) = page_token {
            query.push(("pageToken", token.to_string()));
        }
        if include_spam_trash {
            query.push(("includeSpamTrash", "true".to_string()));
        }
        let url = with_query(self.url(&["messages"])?, &query);
        self.client.get_json(ctx, url).await
    }

    /// Fetch header-only metadata. The field mask always includes the address
    /// and threading headers the shared model persists.
    pub(crate) async fn message_metadata(
        &self,
        id: &str,
        ctx: &OperationContext,
    ) -> Result<Message, ProviderError> {
        let mut query = vec![("format", "metadata".to_string())];
        for header in METADATA_HEADERS {
            query.push(("metadataHeaders", (*header).to_string()));
        }
        let url = with_query(self.url(&["messages", id])?, &query);
        self.client.get_json(ctx, url).await
    }

    pub(crate) async fn message_full(
        &self,
        id: &str,
        ctx: &OperationContext,
    ) -> Result<Message, ProviderError> {
        let url = with_query(
            self.url(&["messages", id])?,
            &[("format", "full".to_string())],
        );
        self.client.get_json(ctx, url).await
    }

    pub(crate) async fn attachment(
        &self,
        message: &str,
        attachment: &str,
        ctx: &OperationContext,
    ) -> Result<AttachmentResponse, ProviderError> {
        let url = self.url(&["messages", message, "attachments", attachment])?;
        self.client.get_json(ctx, url).await
    }

    pub(crate) async fn history(
        &self,
        start_history_id: &str,
        page_token: Option<&str>,
        ctx: &OperationContext,
    ) -> Result<HistoryResponse, ProviderError> {
        let mut query = vec![
            ("startHistoryId", start_history_id.to_string()),
            ("historyTypes", "messageAdded".to_string()),
            ("historyTypes", "messageDeleted".to_string()),
            ("historyTypes", "labelAdded".to_string()),
            ("historyTypes", "labelRemoved".to_string()),
        ];
        if let Some(token) = page_token {
            query.push(("pageToken", token.to_string()));
        }
        let url = with_query(self.url(&["history"])?, &query);
        self.client.get_json(ctx, url).await
    }

    pub(crate) async fn modify(
        &self,
        id: &str,
        add_labels: &[String],
        remove_labels: &[String],
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        let body = serde_json::json!({
            "addLabelIds": add_labels,
            "removeLabelIds": remove_labels,
        });
        self.client
            .post_json::<_, Value>(ctx, self.url(&["messages", id, "modify"])?, &body)
            .await
            .map(|_| ())
    }

    pub(crate) async fn trash(
        &self,
        id: &str,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.client
            .post_empty(ctx, self.url(&["messages", id, "trash"])?)
            .await
    }

    pub(crate) async fn delete(
        &self,
        id: &str,
        ctx: &OperationContext,
    ) -> Result<(), ProviderError> {
        self.client.delete(ctx, self.url(&["messages", id])?).await
    }
}

pub(crate) fn with_query(mut url: Url, query: &[(&str, String)]) -> Url {
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query {
            pairs.append_pair(name, value);
        }
    }
    url
}

impl GoogleClient {
    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        ctx: &OperationContext,
        url: Url,
    ) -> Result<T, ProviderError> {
        let mut attempt = 0u32;
        loop {
            ctx.ensure_active()?;
            match self
                .send(ctx, reqwest::Method::GET, url.clone(), None)
                .await
            {
                Ok(bytes) => return decode_json(&bytes),
                Err(error)
                    if attempt < 2
                        && matches!(error.kind, ErrorKind::Transient | ErrorKind::RateLimited) =>
                {
                    let delay = error
                        .retry_after
                        .unwrap_or_else(|| Duration::from_millis(100 * u64::from(attempt + 1)))
                        .min(Duration::from_millis(500));
                    if tokio::time::Instant::now() + delay >= ctx.deadline {
                        return Err(error);
                    }
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        ctx: &OperationContext,
        url: Url,
        body: &B,
    ) -> Result<T, ProviderError> {
        let bytes = self
            .send(
                ctx,
                reqwest::Method::POST,
                url,
                Some(serde_json::to_value(body).map_err(|error| {
                    ProviderError::transient("could not encode a Gmail request")
                        .with_source_detail(error.to_string())
                })?),
            )
            .await?;
        decode_json(&bytes)
    }

    async fn post_empty(&self, ctx: &OperationContext, url: Url) -> Result<(), ProviderError> {
        self.send(ctx, reqwest::Method::POST, url, None)
            .await
            .map(|_| ())
    }

    async fn delete(&self, ctx: &OperationContext, url: Url) -> Result<(), ProviderError> {
        self.send(ctx, reqwest::Method::DELETE, url, None)
            .await
            .map(|_| ())
    }

    async fn send(
        &self,
        ctx: &OperationContext,
        method: reqwest::Method,
        url: Url,
        body: Option<Value>,
    ) -> Result<Vec<u8>, ProviderError> {
        ctx.ensure_active()?;
        let token = self.access_token(ctx).await?;
        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(token.expose_secret());
        if let Some(body) = body {
            request = request.json(&body);
        }
        // `request.send()` has no inherent bound: a stalled connect or a peer
        // that never finishes the response could outlive the operation. Apply
        // the caller's remaining deadline to the whole request and race the
        // future against cancellation so every capability call honors the
        // master invariant.
        let remaining = ctx
            .deadline
            .saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(ProviderError::transient("operation deadline exceeded"));
        }
        let request = request.timeout(remaining);
        let response = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                return Err(ProviderError::cancelled("operation cancelled"));
            }
            result = request.send() => result.map_err(map_transport_error)?,
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let bytes = tokio::select! {
            biased;
            _ = ctx.cancellation.cancelled() => {
                return Err(ProviderError::cancelled("operation cancelled"));
            }
            result = tokio::time::timeout_at(ctx.deadline, response.bytes()) => {
                match result {
                    Ok(result) => result.map_err(map_transport_error)?,
                    Err(_) => return Err(ProviderError::transient("operation deadline exceeded")),
                }
            }
        };
        if status.is_success() {
            Ok(bytes.to_vec())
        } else {
            Err(map_status(status, retry_after, &bytes))
        }
    }
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(bytes).map_err(|error| {
        ProviderError::transient("unexpected response from Google")
            .with_source_detail(error.to_string())
    })
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    let detail = error.to_string();
    if error.is_connect() {
        ProviderError::offline("Google could not be reached").with_source_detail(detail)
    } else if error.is_timeout() {
        ProviderError::transient("Google did not respond in time").with_source_detail(detail)
    } else {
        ProviderError::transient("Google could not be reached").with_source_detail(detail)
    }
}

fn map_status(
    status: reqwest::StatusCode,
    retry_after: Option<Duration>,
    body: &[u8],
) -> ProviderError {
    let detail = String::from_utf8_lossy(body).to_string();
    let reason = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("errors"))
                .and_then(|errors| errors.as_array())
                .and_then(|errors| errors.first())
                .and_then(|first| first.get("reason"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    // Google's People sync APIs return HTTP 410 (or an ErrorInfo reason) when a
    // seven-day sync token has expired. Clients must restart from a full sync;
    // this is a cursor problem, not an authentication or transport failure.
    if status == reqwest::StatusCode::GONE
        || reason == "EXPIRED_SYNC_TOKEN"
        || detail.contains("EXPIRED_SYNC_TOKEN")
    {
        return ProviderError::cursor_invalid("the Google sync cursor expired")
            .with_source_detail(detail);
    }

    let error = match status {
        reqwest::StatusCode::UNAUTHORIZED => {
            ProviderError::reauthenticate("Google sign-in has expired")
        }
        reqwest::StatusCode::FORBIDDEN => {
            if reason.contains("ateLimit") || reason.contains("uota") {
                ProviderError::rate_limited(retry_after.unwrap_or(Duration::from_secs(1)))
            } else {
                ProviderError::forbidden("Google did not permit this request")
            }
        }
        reqwest::StatusCode::NOT_FOUND => ProviderError::not_found("the Google item was not found"),
        reqwest::StatusCode::CONFLICT | reqwest::StatusCode::PRECONDITION_FAILED => {
            ProviderError::conflict("the Google item changed")
        }
        reqwest::StatusCode::TOO_MANY_REQUESTS => {
            ProviderError::rate_limited(retry_after.unwrap_or(Duration::from_secs(1)))
        }
        status if status.is_server_error() => {
            ProviderError::transient("Google had a temporary problem")
        }
        _ => ProviderError::transient("Google rejected the request"),
    };
    error.with_source_detail(detail)
}

/// Wire shapes used only inside the Gmail adapter. They are deliberately
/// private: nothing outside `providers/google` ever sees a Gmail resource.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Label {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) messages_total: Option<u32>,
    #[serde(default)]
    pub(crate) messages_unread: Option<u32>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub(crate) struct LabelsResponse {
    #[serde(default)]
    pub(crate) labels: Vec<Label>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProfileResponse {
    #[serde(default)]
    pub(crate) history_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageRef {
    pub(crate) id: String,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageListResponse {
    #[serde(default)]
    pub(crate) messages: Vec<MessageRef>,
    #[serde(default)]
    pub(crate) next_page_token: Option<String>,
    #[serde(default)]
    pub(crate) result_size_estimate: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Message {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) label_ids: Vec<String>,
    #[serde(default)]
    pub(crate) snippet: String,
    #[serde(default)]
    pub(crate) internal_date: Option<String>,
    #[serde(default)]
    pub(crate) size_estimate: Option<u64>,
    #[serde(default)]
    pub(crate) payload: Option<Payload>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Payload {
    #[serde(default)]
    pub(crate) part_id: Option<String>,
    #[serde(default)]
    pub(crate) mime_type: Option<String>,
    #[serde(default)]
    pub(crate) filename: Option<String>,
    #[serde(default)]
    pub(crate) headers: Vec<Header>,
    #[serde(default)]
    pub(crate) body: Option<Body>,
    #[serde(default)]
    pub(crate) parts: Vec<Payload>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct Header {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Body {
    #[serde(default)]
    pub(crate) attachment_id: Option<String>,
    #[serde(default)]
    pub(crate) size: Option<u64>,
    #[serde(default)]
    pub(crate) data: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AttachmentResponse {
    #[serde(default)]
    pub(crate) data: Option<String>,
    #[serde(default)]
    pub(crate) size: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryResponse {
    #[serde(default)]
    pub(crate) history: Vec<HistoryRecord>,
    #[serde(default)]
    pub(crate) next_page_token: Option<String>,
    #[serde(default)]
    pub(crate) history_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryRecord {
    #[serde(default)]
    pub(crate) messages_added: Vec<HistoryMessage>,
    #[serde(default)]
    pub(crate) messages_deleted: Vec<HistoryMessage>,
    #[serde(default)]
    pub(crate) labels_added: Vec<HistoryLabel>,
    #[serde(default)]
    pub(crate) labels_removed: Vec<HistoryLabel>,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
pub(crate) struct HistoryMessage {
    #[serde(default)]
    pub(crate) message: MessageRef,
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryLabel {
    #[serde(default)]
    pub(crate) message: MessageRef,
    #[serde(default)]
    pub(crate) label_ids: Vec<String>,
}
