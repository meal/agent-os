use serde::{Deserialize, Serialize};
use std::time::UNIX_EPOCH;

pub const POLICY_VERSION: u32 = 1;
/// The model limits version new submissions record.
pub const LIMITS_VERSION: u32 = 1;
pub const REQUEST_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelFailureClass {
    Permanent,
    Transient,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFailure {
    pub class: ModelFailureClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_not_before_ts: Option<i64>,
}

pub fn classify(status: u16) -> ModelFailureClass {
    if matches!(status, 408 | 429 | 500..=599) {
        ModelFailureClass::Transient
    } else {
        ModelFailureClass::Permanent
    }
}

pub fn backoff_seconds(consecutive_failures: u32) -> i64 {
    match consecutive_failures {
        0..=1 => 2,
        2..=5 => 2_i64 << (consecutive_failures - 1),
        _ => 60,
    }
}

pub fn retry_at(now: i64, consecutive_failures: u32, provider_not_before: Option<i64>) -> i64 {
    now.saturating_add(backoff_seconds(consecutive_failures))
        .max(provider_not_before.unwrap_or(now))
}

pub fn parse_retry_after(header: &str, now: i64) -> Option<i64> {
    let value = header.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let seconds: u64 = value.parse().ok()?;
        return Some(now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX)));
    }
    let date = httpdate::parse_http_date(value).ok()?;
    let seconds = date.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(i64::try_from(seconds).unwrap_or(i64::MAX))
}

pub fn check_request_size(size: usize) -> Result<(), &'static str> {
    if size > REQUEST_LIMIT {
        Err("model context size exceeds 8388608 bytes")
    } else {
        Ok(())
    }
}

/// Missing versions denote the legacy policy. Unknown versions are never guessed.
pub fn versions(
    db: &agentos_store::db::Db,
    task: &agentos_core::ids::TaskId,
) -> Result<(u32, u32), agentos_store::db::DbError> {
    let submitted = db
        .events(task)?
        .into_iter()
        .find(|e| e.event_type == "Submitted");
    let version = |name: &str| {
        let value = submitted.as_ref().and_then(|e| e.payload.get(name));
        match value {
            None => Ok(0),
            Some(v) if v.as_u64() == Some(0) => Ok(0),
            Some(v) if v.as_u64() == Some(1) => Ok(1),
            _ => Err(agentos_store::db::DbError::Corrupt(format!(
                "unsupported {name}"
            ))),
        }
    };
    Ok((
        version("model_policy_version")?,
        version("model_limits_version")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicitly_transient_statuses_retry() {
        for status in [408, 429, 500, 503, 529, 599] {
            assert_eq!(classify(status), ModelFailureClass::Transient);
        }
        for status in [301, 302, 400, 401, 403, 404, 422, 499] {
            assert_eq!(classify(status), ModelFailureClass::Permanent);
        }
    }

    #[test]
    fn retry_backoff_saturates_and_provider_can_only_extend_it() {
        assert_eq!(
            (1..=8).map(backoff_seconds).collect::<Vec<_>>(),
            [2, 4, 8, 16, 32, 60, 60, 60]
        );
        assert!((6..=70).all(|n| backoff_seconds(n) == 60));
        assert_eq!(retry_at(100, 1, None), 102);
        assert_eq!(retry_at(100, 1, Some(101)), 102);
        assert_eq!(retry_at(100, 1, Some(120)), 120);
        assert_eq!(retry_at(i64::MAX, u32::MAX, None), i64::MAX);
    }

    #[test]
    fn retry_after_accepts_delta_and_http_date_but_ignores_invalid_input() {
        assert_eq!(parse_retry_after(" 12 ", 100), Some(112));
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:02:00 GMT", 100),
            Some(120)
        );
        for value in ["nonsense", "-1", "1.5", "184467440737095516160"] {
            assert_eq!(parse_retry_after(value, 100), None, "{value}");
        }
    }

    #[test]
    fn request_bytes_accept_exact_limit_and_reject_the_next_byte() {
        assert!(check_request_size(8 * 1024 * 1024).is_ok());
        assert!(
            check_request_size(8 * 1024 * 1024 + 1)
                .unwrap_err()
                .contains("context size")
        );
    }
}
