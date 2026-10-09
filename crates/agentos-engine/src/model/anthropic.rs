use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::provider::{ApiKey, BoxFuture, ModelProvider, ProviderResult, usage_of};

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const API_VERSION: &str = "2023-06-01";
pub const MODEL_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest rejected-response body kept (bytes).
pub const PROVIDER_TEXT_LIMIT: usize = 4096;
/// Maximum raw bytes retained from a successful provider response.
pub const MODEL_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

async fn read_success_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let exceeded = || format!("response body exceeds {MODEL_RESPONSE_LIMIT} bytes");
    if response
        .content_length()
        .is_some_and(|n| n > MODEL_RESPONSE_LIMIT as u64)
    {
        return Err(exceeded());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("response body: {}", e.without_url()))?
    {
        if chunk.len() > MODEL_RESPONSE_LIMIT.saturating_sub(bytes.len()) {
            return Err(exceeded());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_error_excerpt(mut response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    while bytes.len() < PROVIDER_TEXT_LIMIT {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let count = chunk.len().min(PROVIDER_TEXT_LIMIT - bytes.len());
                bytes.extend_from_slice(&chunk[..count]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    key: ApiKey,
}

fn client_builder(timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
}

fn build_client(timeout: Duration) -> reqwest::Client {
    client_builder(timeout)
        .build()
        .expect("build the HTTP client")
}

/// Whether the provider's HTTP client can be built here: it verifies the server with the
/// system's CA certificates, which a minimal host may lack. Call before
/// [`AnthropicProvider::new`], which cannot fail.
pub fn check_http_client() -> Result<(), String> {
    client_builder(MODEL_TIMEOUT)
        .build()
        .map(drop)
        .map_err(|e| {
            let cause =
                std::error::Error::source(&e).map_or_else(|| e.to_string(), ToString::to_string);
            format!("cannot build a TLS client for the model provider: {cause}")
        })
}

impl AnthropicProvider {
    pub fn new(key: ApiKey) -> AnthropicProvider {
        AnthropicProvider {
            client: build_client(MODEL_TIMEOUT),
            base_url: ANTHROPIC_BASE_URL.into(),
            key,
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.client = build_client(d);
        self
    }
}

impl fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnthropicProvider({})", self.base_url)
    }
}

impl ModelProvider for AnthropicProvider {
    fn complete<'a>(&'a self, body: &'a [u8]) -> BoxFuture<'a, ProviderResult> {
        Box::pin(async move {
            let sent = self
                .client
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", self.key.expose())
                .header("anthropic-version", API_VERSION)
                .header("content-type", "application/json")
                .body(body.to_vec())
                .send()
                .await;
            let resp = match sent {
                Ok(r) => r,
                Err(e) => return ProviderResult::Transport(e.without_url().to_string()),
            };
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                match read_success_body(resp).await {
                    Ok(bytes) => {
                        let usage = serde_json::from_slice(&bytes)
                            .map(|v| usage_of(&v))
                            .unwrap_or_default();
                        ProviderResult::Response(bytes, usage)
                    }
                    Err(reason) => ProviderResult::Transport(reason),
                }
            } else {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                let retry_not_before_ts = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| super::policy::parse_retry_after(h, now));
                let body = read_error_excerpt(resp).await;
                match retry_not_before_ts {
                    Some(retry_not_before_ts) => ProviderResult::RejectedWithRetryAfter {
                        status,
                        body,
                        retry_not_before_ts,
                    },
                    None => ProviderResult::Rejected { status, body },
                }
            }
        })
    }
}
