use std::fmt;
use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The token usage a Messages response reports (a missing field counts 0).
pub fn usage_of(response: &serde_json::Value) -> Usage {
    let n = |k: &str| response["usage"][k].as_u64().unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
    }
}

/// What one send of a model request came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum ProviderResult {
    /// A 2xx answer: the raw response bytes and the usage they report.
    Response(Vec<u8>, Usage),
    /// A non-2xx answer; the body is bounded.
    Rejected { status: u16, body: String },
    /// A definite rejection with the provider's parsed Retry-After timestamp.
    RejectedWithRetryAfter {
        status: u16,
        body: String,
        retry_not_before_ts: i64,
    },
    /// No answer arrived (connect failure, timeout, cut connection).
    Transport(String),
}

/// An API key. Debug is redacted; there is no Display, Serialize or Deserialize.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(raw: &str) -> Result<ApiKey, String> {
        let t = raw.trim();
        if t.is_empty() {
            return Err("empty API key".into());
        }
        Ok(ApiKey(t.to_string()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(…)")
    }
}

/// Sends one serialized request exactly once. The HTTP client sets `retry::never()` and
/// `redirect::Policy::none()`, so neither reqwest nor this trait retries or re-sends.
pub trait ModelProvider: Send + Sync {
    fn complete<'a>(&'a self, body: &'a [u8]) -> BoxFuture<'a, ProviderResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", ApiKey::new("sk-ant-secret-123").unwrap()),
            "ApiKey(…)"
        );
        assert_eq!(ApiKey::new("  k \n").unwrap().expose(), "k");
        assert!(ApiKey::new(" ").is_err());
    }

    #[test]
    fn usage_of_tolerates_missing_fields() {
        assert_eq!(usage_of(&serde_json::json!({})), Usage::default());
        assert_eq!(
            usage_of(&serde_json::json!({"usage": {"input_tokens": 7}})),
            Usage {
                input_tokens: 7,
                output_tokens: 0
            }
        );
        assert_eq!(
            usage_of(&serde_json::json!({"usage": {"input_tokens": 1, "output_tokens": 2}})),
            Usage {
                input_tokens: 1,
                output_tokens: 2
            }
        );
    }
}
