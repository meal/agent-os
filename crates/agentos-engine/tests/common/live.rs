//! The gate of the live model test (`tests/live_model.rs`), the same convention as
//! [`super::kvm::require`]: `let Some(live) = live::require() else { return };`.
//!
//! Without `AGENTOS_LIVE_MODEL_TESTS` the test prints [`SKIP_MESSAGE`] and returns, and the
//! default `cargo test` makes no network call. With it set, an unusable setup (no key) panics:
//! a test that was asked for must not pass by doing nothing. The key is read into an
//! [`ApiKey`] (redacted `Debug`) and never printed, logged or written down; panic texts name
//! the variables, never their values.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

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
    pub worker: String,
}

fn nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub fn enabled(value: Option<&str>) -> bool {
    match value {
        None | Some("") | Some("0") => false,
        Some("1") => true,
        Some(_) => panic!("AGENTOS_LIVE_MODEL_TESTS must be 0 or 1"),
    }
}

pub fn read_key_file(path: &Path) -> io::Result<String> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "key must be a regular file of at most 4096 bytes",
        ));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "key exceeds 4096 bytes",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "key must be UTF-8"))
}

/// The gate. See the module documentation.
pub fn require() -> Option<Live> {
    if !enabled(std::env::var("AGENTOS_LIVE_MODEL_TESTS").ok().as_deref()) {
        println!("{SKIP_MESSAGE}");
        return None;
    }
    let raw = match nonempty("AGENTOS_API_KEY_FILE") {
        // The file is named in the panic text, its content never.
        Some(path) => read_key_file(Path::new(&path)).unwrap_or_else(|e| panic!("AGENTOS_API_KEY_FILE={path}: {e}")),
        None => nonempty("ANTHROPIC_API_KEY").unwrap_or_else(|| {
            panic!(
                "AGENTOS_LIVE_MODEL_TESTS is set but there is no API key: set ANTHROPIC_API_KEY (or AGENTOS_API_KEY_FILE) \
                 (docker compose -p agent-os run --rm -e AGENTOS_LIVE_MODEL_TESTS=1 -e ANTHROPIC_API_KEY test cargo test -p agentos-engine --test live_model -- --nocapture --test-threads 1)"
            )
        }),
    };
    let key = ApiKey::new(&raw).unwrap_or_else(|e| {
        panic!("AGENTOS_LIVE_MODEL_TESTS is set but the API key is unusable: {e}")
    });
    let worker = nonempty("AGENTOS_LIVE_WORKER").unwrap_or_else(|| "host".into());
    assert!(
        matches!(worker.as_str(), "host" | "firecracker"),
        "AGENTOS_LIVE_WORKER must be host or firecracker"
    );
    Some(Live {
        key,
        worker,
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
    let contract = Contract::parse(&json).unwrap();
    let digest = Digest::of(&serde_json::to_vec(&contract).unwrap());
    (contract, digest)
}
