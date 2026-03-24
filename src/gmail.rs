use crate::auth::Authenticator;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use reqwest::{
    Client, StatusCode,
    header::{HeaderMap, RETRY_AFTER},
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use url::Url;

const MAX_TRANSIENT_RETRIES: usize = 5;
const INITIAL_RETRY_DELAY_MS: u64 = 500;
const MAX_RETRY_DELAY_MS: u64 = 8_000;

#[derive(Clone)]
pub struct GmailClient {
    client: Client,
    auth: Arc<Mutex<Authenticator>>,
    include_spam_trash: bool,
}

impl GmailClient {
    pub fn new(client: Client, auth: Authenticator, include_spam_trash: bool) -> Self {
        Self {
            client,
            auth: Arc::new(Mutex::new(auth)),
            include_spam_trash,
        }
    }

    pub async fn list_message_ids(&self, query: &str, page_size: u32) -> Result<Vec<String>> {
        let mut next_page_token: Option<String> = None;
        let mut ids = Vec::new();

        loop {
            let mut url = Url::parse("https://gmail.googleapis.com/gmail/v1/users/me/messages")
                .context("Failed to build the Gmail messages.list URL")?;
            {
                let mut query_pairs = url.query_pairs_mut();
                query_pairs
                    .append_pair("includeSpamTrash", bool_as_google(self.include_spam_trash));
                query_pairs.append_pair("maxResults", &page_size.clamp(1, 500).to_string());
                query_pairs.append_pair("q", query);

                if let Some(page_token) = next_page_token.as_deref() {
                    query_pairs.append_pair("pageToken", page_token);
                }
            }

            let page: ListMessagesResponse = self.get_json(url.clone()).await?;
            let page_count = page.messages.as_ref().map_or(0, Vec::len);
            let page_estimate = page
                .result_size_estimate
                .map(|estimate| format!(" / estimated {estimate}"))
                .unwrap_or_default();

            if let Some(messages) = page.messages {
                ids.extend(messages.into_iter().map(|message| message.id));
            }

            println!(
                "Listed {page_count} messages this page, {} total{page_estimate}",
                ids.len()
            );

            next_page_token = page.next_page_token;
            if next_page_token.is_none() {
                break;
            }
        }

        Ok(ids)
    }

    pub async fn get_raw_message(&self, message_id: &str) -> Result<RawMessage> {
        let mut url = Url::parse(&format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{message_id}"
        ))
        .context("Failed to build the Gmail messages.get URL")?;
        url.query_pairs_mut().append_pair("format", "raw");

        let message: RawMessageResponse = self.get_json(url).await?;
        let raw = decode_gmail_base64(
            message
                .raw
                .as_deref()
                .context("The Gmail API response did not contain a raw message body")?,
        )?;

        Ok(RawMessage { raw })
    }

    pub async fn trash_message(&self, message_id: &str) -> Result<()> {
        let url = Url::parse(&format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{message_id}/trash"
        ))
        .context("Failed to build the Gmail messages.trash URL")?;

        self.post_empty(url).await
    }

    pub async fn get_message_labels(&self, message_id: &str) -> Result<Vec<String>> {
        let mut url = Url::parse(&format!(
            "https://gmail.googleapis.com/gmail/v1/users/me/messages/{message_id}"
        ))
        .context("Failed to build the Gmail messages.get URL")?;
        url.query_pairs_mut().append_pair("format", "minimal");

        let message: MinimalMessageResponse = self.get_json(url).await?;
        Ok(message.label_ids.unwrap_or_default())
    }

    async fn get_json<T>(&self, url: Url) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut refreshed_access_token = false;
        let mut transient_retries = 0usize;

        loop {
            let access_token = self.bearer_token().await?;
            let response = match self
                .client
                .get(url.clone())
                .bearer_auth(access_token)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    if should_retry_transport_error(&error)
                        && transient_retries < MAX_TRANSIENT_RETRIES
                    {
                        transient_retries += 1;
                        sleep(retry_delay(transient_retries, None)).await;
                        continue;
                    }

                    return Err(error)
                        .with_context(|| format!("Request to the Gmail API failed: {url}"));
                }
            };

            if response.status() == StatusCode::UNAUTHORIZED && !refreshed_access_token {
                self.invalidate_access_token().await;
                refreshed_access_token = true;
                continue;
            }

            if should_retry_status(response.status()) && transient_retries < MAX_TRANSIENT_RETRIES {
                transient_retries += 1;
                sleep(retry_delay(transient_retries, Some(response.headers()))).await;
                continue;
            }

            let response = response
                .error_for_status()
                .with_context(|| format!("The Gmail API returned an error: {url}"))?;

            return response
                .json::<T>()
                .await
                .with_context(|| format!("Failed to parse the Gmail API JSON: {url}"));
        }
    }

    async fn post_empty(&self, url: Url) -> Result<()> {
        let mut refreshed_access_token = false;
        let mut transient_retries = 0usize;

        loop {
            let access_token = self.bearer_token().await?;
            let response = match self
                .client
                .post(url.clone())
                .bearer_auth(access_token)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    if should_retry_transport_error(&error)
                        && transient_retries < MAX_TRANSIENT_RETRIES
                    {
                        transient_retries += 1;
                        sleep(retry_delay(transient_retries, None)).await;
                        continue;
                    }

                    return Err(error)
                        .with_context(|| format!("Request to the Gmail API failed: {url}"));
                }
            };

            if response.status() == StatusCode::UNAUTHORIZED && !refreshed_access_token {
                self.invalidate_access_token().await;
                refreshed_access_token = true;
                continue;
            }

            if should_retry_status(response.status()) && transient_retries < MAX_TRANSIENT_RETRIES {
                transient_retries += 1;
                sleep(retry_delay(transient_retries, Some(response.headers()))).await;
                continue;
            }

            response
                .error_for_status()
                .with_context(|| format!("The Gmail API returned an error: {url}"))?;
            return Ok(());
        }
    }

    async fn bearer_token(&self) -> Result<String> {
        let mut auth = self.auth.lock().await;
        auth.bearer_token().await
    }

    async fn invalidate_access_token(&self) {
        let mut auth = self.auth.lock().await;
        auth.invalidate_access_token();
    }
}

pub struct RawMessage {
    pub raw: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct ListMessagesResponse {
    messages: Option<Vec<MessageId>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "resultSizeEstimate")]
    result_size_estimate: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct MessageId {
    id: String,
}

#[derive(Debug, Deserialize)]
struct RawMessageResponse {
    raw: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MinimalMessageResponse {
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
}

fn bool_as_google(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

fn should_retry_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn should_retry_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect()
}

fn retry_delay(attempt: usize, headers: Option<&HeaderMap>) -> Duration {
    retry_after_delay(headers).unwrap_or_else(|| exponential_backoff(attempt))
}

fn retry_after_delay(headers: Option<&HeaderMap>) -> Option<Duration> {
    let headers = headers?;
    let retry_after = headers.get(RETRY_AFTER)?;
    let seconds = retry_after.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

fn exponential_backoff(attempt: usize) -> Duration {
    let shift = attempt.saturating_sub(1).min(6) as u32;
    let multiplier = 1_u64 << shift;
    Duration::from_millis((INITIAL_RETRY_DELAY_MS * multiplier).min(MAX_RETRY_DELAY_MS))
}

fn decode_gmail_base64(encoded: &str) -> Result<Vec<u8>> {
    URL_SAFE
        .decode(encoded)
        .context("Failed to decode the Gmail raw message from Base64URL")
}
