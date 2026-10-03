use std::fmt;
use std::time::Duration;

use super::provider::{ApiKey, BoxFuture, ModelProvider, ProviderResult, usage_of};

pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const API_VERSION: &str = "2023-06-01";
pub const MODEL_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest rejected-response body kept (bytes).
pub const PROVIDER_TEXT_LIMIT: usize = 4096;

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    key: ApiKey,
}

fn build_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build().expect("build the HTTP client")
}

impl AnthropicProvider {
    pub fn new(key: ApiKey) -> AnthropicProvider {
        AnthropicProvider { client: build_client(MODEL_TIMEOUT), base_url: ANTHROPIC_BASE_URL.into(), key }
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
            let ok = resp.status().is_success();
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return ProviderResult::Transport(format!("response body: {}", e.without_url())),
            };
            if ok {
                let usage = serde_json::from_slice(&bytes).map(|v| usage_of(&v)).unwrap_or_default();
                ProviderResult::Response(bytes.to_vec(), usage)
            } else {
                let n = bytes.len().min(PROVIDER_TEXT_LIMIT);
                ProviderResult::Rejected { status, body: String::from_utf8_lossy(&bytes[..n]).into_owned() }
            }
        })
    }
}
