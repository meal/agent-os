//! The gate of the live model test (`tests/live_model.rs`), the same convention as
//! [`super::kvm::require`]: `let Some(live) = live::require() else { return };`.
//!
//! Without `AGENTOS_LIVE_MODEL_TESTS` the test prints [`SKIP_MESSAGE`] and returns, and the
//! default `cargo test` makes no network call. With it set, an unusable setup (no key) panics:
//! a test that was asked for must not pass by doing nothing. The key is read into an
//! [`ApiKey`] (redacted `Debug`) and never printed, logged or written down; panic texts name
//! the variables, never their values.

use std::fs;

use agentos_core::contract::Contract;
use agentos_core::ids::Digest;
use agentos_engine::model::anthropic::DEFAULT_MODEL;
use agentos_engine::model::provider::ApiKey;

pub const SKIP_MESSAGE: &str = "SKIPPED: set AGENTOS_LIVE_MODEL_TESTS=1 and ANTHROPIC_API_KEY (or AGENTOS_API_KEY_FILE) to run the live model test";

pub struct Live {
    pub key: ApiKey,
    /// `AGENTOS_ANTHROPIC_BASE_URL`, when set.
    pub base_url: Option<String>,
    /// `AGENTOS_LIVE_MODEL`, else the default model.
    pub model: String,
}

fn nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The gate. See the module documentation.
pub fn require() -> Option<Live> {
    if std::env::var_os("AGENTOS_LIVE_MODEL_TESTS").is_none() {
        println!("{SKIP_MESSAGE}");
        return None;
    }
    let raw = match nonempty("AGENTOS_API_KEY_FILE") {
        // The file is named in the panic text, its content never.
        Some(path) => fs::read_to_string(&path).unwrap_or_else(|e| panic!("AGENTOS_API_KEY_FILE={path}: {e}")),
        None => nonempty("ANTHROPIC_API_KEY").unwrap_or_else(|| {
            panic!(
                "AGENTOS_LIVE_MODEL_TESTS is set but there is no API key: set ANTHROPIC_API_KEY (or AGENTOS_API_KEY_FILE) \
                 (docker compose -p agent-os run --rm -e AGENTOS_LIVE_MODEL_TESTS=1 -e ANTHROPIC_API_KEY test cargo test -p agentos-engine --test live_model -- --nocapture --test-threads 1)"
            )
        }),
    };
    let key = ApiKey::new(&raw).unwrap_or_else(|e| panic!("AGENTOS_LIVE_MODEL_TESTS is set but the API key is unusable: {e}"));
    Some(Live {
        key,
        base_url: nonempty("AGENTOS_ANTHROPIC_BASE_URL"),
        model: nonempty("AGENTOS_LIVE_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
    })
}

/// The live contract: `MODEL_CAPS`, 12 model requests of up to 16000 output tokens, 12 tool
/// actions and a 900 second deadline over the parser fixture.
pub fn contract() -> (Contract, Digest) {
    let caps = serde_json::to_string(super::MODEL_CAPS).unwrap();
    let json = format!(
        r#"{{
        "goal": "fix the parser",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "python-stdlib-v1",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": 12,
            "max_output_tokens_per_request": 16000,
            "tool_actions": 12,
            "deadline_seconds": 900,
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }}
    }}"#
    );
    (Contract::parse(&json).unwrap(), Digest::of(json.as_bytes()))
}
