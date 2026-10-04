# Bounded Model I/O Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Bound provider response collection and API-key file reads without changing successful model requests or exposing credentials.

**Architecture:** Keep `ModelProvider` and `ProviderResult` unchanged in this package. Read successful response chunks under a fixed cap, stop error-body collection at the existing excerpt cap, and isolate descriptor-based key reading in a small CLI module.

**Tech Stack:** Rust 1.98.1, reqwest 0.13.5, existing rustix, standard I/O, Docker Compose.

**Spec:** [v0.1 completion design](../specs/2026-10-04-v01-completion-design.md), section "Deadlines and bounded input"; [master plan](2026-10-04-v01-completion.md), Tasks 2–3.

## Global Constraints

- Docker Compose for build/test; offline tests make no paid API calls.
- One model send per attempt; requests possibly billed remain counted as uncertain.
- Successful HTTP response limit: 4 MiB raw bytes.
- Rejected-response excerpt limit: 4096 raw bytes; retain the known HTTP status.
- API key file limit: 4096 bytes; regular files only; no blocking FIFO/device read.
- Keep request headers, redirect refusal, no-proxy, and client retry policy unchanged.
- Preserve ASCII key validation and redacted `ApiKey`; errors never quote key contents.
- No co-author commits. Recheck latest library versions before dependency additions/updates; no new library is required here.

## Review Focus

- Missing or false `Content-Length`: enforce actual successful-body bytes (Task 1).
- Chunked success exceeds cap: stop collecting and preserve uncertain accounting (Task 1).
- Definite HTTP error with incomplete body: retain status and bounded prefix without draining (Task 1).
- FIFO/device/symlink key path: reject promptly using opened-descriptor checks and flags (Task 2).
- Key file grows after metadata inspection: cap the actual read at limit+1 (Task 2 implementation and boundary tests; add a descriptor seam if racing mutation is needed).

## Task 1: Bound provider response collection

**Files:**

- Modify: `crates/agentos-engine/src/model/anthropic.rs`.
- Modify test server: `crates/agentos-engine/tests/common/http.rs`.
- Test: `crates/agentos-engine/tests/provider.rs`, `model_executor.rs`, `model_flow.rs`, `recover_forfeit.rs`.

**Interfaces:**

- Add public `MODEL_RESPONSE_LIMIT: usize = 4 * 1024 * 1024` for documented policy/tests.
- Add private `read_success_body(reqwest::Response) -> async Result<Vec<u8>, String>` and `read_error_excerpt(reqwest::Response) -> async String`.
- Add test-only `Reply::Raw(Vec<u8>)` to the existing fake API.
- Preserve `ProviderResult::{Response,Rejected,Transport}` fields in this package. Typed retry metadata comes in master Task 4.

- [x] **Step 1: Extend the local test server to emit raw HTTP replies.**

Add the enum variant and corresponding match arm in `handle`:

```rust
Raw(Vec<u8>),
```

```rust
Reply::Raw(bytes) => {
    let _ = stream.write_all(bytes);
    let _ = stream.flush();
}
```

The request-reading/hit recording behavior remains unchanged. This enables fixed-length, chunked, and incomplete-body wire cases without a new HTTP server dependency.

- [x] **Step 2: Write red provider tests.**

Import `MODEL_RESPONSE_LIMIT` alongside `PROVIDER_TEXT_LIMIT` in `tests/provider.rs`; reuse its existing `provider`, `serve`, `Reply`, `BODY` helpers.

```rust
fn raw_success(payload: &[u8], chunked: bool) -> Vec<u8> {
    let mut wire = if chunked {
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec()
    } else {
        format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", payload.len()).into_bytes()
    };
    if chunked {
        wire.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
    }
    wire.extend_from_slice(payload);
    if chunked {
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
    }
    wire
}

#[tokio::test]
async fn response_byte_limit_applies_to_fixed_length_and_chunked_bodies() {
    for chunked in [false, true] {
        for length in [MODEL_RESPONSE_LIMIT, MODEL_RESPONSE_LIMIT + 1] {
            let payload = vec![b'x'; length];
            let api = serve(Reply::Raw(raw_success(&payload, chunked)));
            let result = provider(&api, Duration::from_secs(5)).complete(BODY).await;
            if length == MODEL_RESPONSE_LIMIT {
                assert!(matches!(result, ProviderResult::Response(ref bytes, _) if bytes.len() == length));
            } else {
                assert!(matches!(result, ProviderResult::Transport(ref reason) if reason.contains("response body exceeds")), "{result:?}");
            }
            assert_eq!(api.hits(), 1);
        }
    }
}

#[tokio::test]
async fn rejected_status_does_not_require_draining_the_body() {
    let mut wire = b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 8192\r\nconnection: close\r\n\r\n".to_vec();
    wire.extend_from_slice(&vec![b'x'; PROVIDER_TEXT_LIMIT]);
    let api = serve(Reply::Raw(wire));
    let result = provider(&api, Duration::from_secs(2)).complete(BODY).await;
    assert_eq!(result, ProviderResult::Rejected {
        status: 429, body: "x".repeat(PROVIDER_TEXT_LIMIT),
    });
    assert_eq!(api.hits(), 1);
}
```

The provider collector accepts opaque bytes; JSON shape validation is separately tested by `ModelExecutor`. These tests intentionally avoid conflating JSON validation with transport size enforcement.

```sh
docker compose run --rm test cargo test -p agentos-engine --test provider --locked
```

Expected initial compile failure until the cap constant exists, then cap+1 failures under the current whole-body implementation. The incomplete 429 currently becomes transport failure instead of retaining the known status.

- [x] **Step 3: Add bounded collection helpers.**

Add the cap and these helpers to `model/anthropic.rs`:

```rust
pub const MODEL_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

async fn read_success_body(mut response: reqwest::Response) -> Result<Vec<u8>, String> {
    let exceeded = || format!("response body exceeds {MODEL_RESPONSE_LIMIT} bytes");
    if response.content_length().is_some_and(|n| n > MODEL_RESPONSE_LIMIT as u64) {
        return Err(exceeded());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await
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
```

Error-prefix collection retains the definite HTTP error even when its remaining body is incomplete. The existing client timeout and Task 1 deadline timeout still bound the time needed to receive a prefix; a byte cap alone is not a time limit.

- [x] **Step 4: Replace whole-body collection in `complete`.**

After obtaining `resp` and its status, replace the current `resp.bytes().await` branch with:

```rust
let status = resp.status().as_u16();
if resp.status().is_success() {
    match read_success_body(resp).await {
        Ok(bytes) => {
            let usage = serde_json::from_slice(&bytes).map(|v| usage_of(&v)).unwrap_or_default();
            ProviderResult::Response(bytes, usage)
        }
        Err(reason) => ProviderResult::Transport(reason),
    }
} else {
    let body = read_error_excerpt(resp).await;
    ProviderResult::Rejected { status, body }
}
```

Do not change `.send()`, no redirects, no retries, headers, key storage, or retained-outcome behavior.

- [x] **Step 5: Run collection and accounting gates, then commit.**

```sh
docker compose run --rm test cargo test -p agentos-engine --locked --test provider --test model_executor --test model_flow --test recover_forfeit --test model_crash_matrix
git diff --check
git add crates/agentos-engine/src/model/anthropic.rs crates/agentos-engine/tests/provider.rs crates/agentos-engine/tests/common/http.rs
git commit -m "fix(engine): bound provider response collection"
```

Expected: exact-boundary success; cap+1 unresolved; known errors retain bounded prefixes; one send; secrets remain excluded; no account reservation released for an unresolved response.

## Task 2: Read API keys through a bounded regular-file descriptor

**Files:**

- Create: `crates/agentos-cli/src/secrets.rs`.
- Modify: `crates/agentos-cli/src/lib.rs` to declare `mod secrets;`.
- Modify: `crates/agentos-cli/src/home.rs` to use the helper in `api_key`.
- Modify: `crates/agentos-cli/Cargo.toml` so its rustix features explicitly include `fs` in addition to `system`.
- Test: new module tests; existing `crates/agentos-cli/tests/cli.rs` secret-exclusion and pre-submit failure cases.

**Interfaces:** `pub(crate) const KEY_FILE_LIMIT: usize = 4096`; `pub(crate) fn read_key_file(path: &Path) -> io::Result<String>`. No new CLI flag or key serialization.

- [x] **Step 1: Add the behavioral test module before the helper.**

Create `secrets.rs` with the following tests, and declare it in `lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn regular_key_files_are_bounded_by_actual_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        fs::write(&path, vec![b'k'; KEY_FILE_LIMIT]).unwrap();
        assert_eq!(read_key_file(&path).unwrap().len(), KEY_FILE_LIMIT);
        fs::write(&path, vec![b'k'; KEY_FILE_LIMIT + 1]).unwrap();
        assert!(read_key_file(&path).is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(read_key_file(&path).is_err());
    }

    #[test]
    fn nonregular_and_symlink_key_paths_are_refused_promptly() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        fs::write(&key, b"sk-ant-test-not-a-real-key\n").unwrap();
        let link = dir.path().join("link");
        symlink(&key, &link).unwrap();
        let fifo = dir.path().join("fifo");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        for path in [link.as_path(), fifo.as_path(), dir.path(), std::path::Path::new("/dev/null")] {
            let started = Instant::now();
            assert!(read_key_file(path).is_err(), "{}", path.display());
            assert!(started.elapsed() < Duration::from_secs(1));
        }
        assert!(read_key_file(&key).unwrap().starts_with("sk-ant-test"));
    }
}
```

```sh
docker compose run --rm test cargo test -p agentos-cli --lib secrets --locked
```

Expected compile failure: the helper/constants are absent. Do not introduce an unbounded helper as a runnable baseline for FIFO tests.

- [x] **Step 2: Implement opened-descriptor checks and actual-read cap.**

Add above the test module:

```rust
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub(crate) const KEY_FILE_LIMIT: usize = 4096;

pub(crate) fn read_key_file(path: &Path) -> io::Result<String> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let file = OpenOptions::new().read(true).custom_flags(flags.bits() as i32).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "API key path must be a regular file"));
    }
    if metadata.len() > KEY_FILE_LIMIT as u64 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "API key file exceeds 4096 bytes"));
    }
    let mut bytes = Vec::new();
    file.take((KEY_FILE_LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > KEY_FILE_LIMIT {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "API key file exceeds 4096 bytes"));
    }
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "API key file must be UTF-8")
    })
}
```

Linux is the supported platform. `NONBLOCK` prevents a FIFO open from waiting before the descriptor type check; `NOFOLLOW` refuses a symlink final component. Do not claim protection against every ancestor-directory race; the local owner controls key-file configuration.

- [x] **Step 3: Delegate `Home::api_key` to the helper.**

Replace only the file-reading match arm:

```rust
Some(path) => crate::secrets::read_key_file(path)
    .map_err(|e| CliError::usage(format!("cannot read {}: {e}", path.display())))?,
```

Keep the environment-key branch, trimming, `ApiKey::new`, printable ASCII check, and redaction. Add a CLI integration case using an oversized key file; assert exit 2, no submitted task, and no key content in stderr. Reuse the existing CLI setup and `an_unreadable_key_file_exits_2` assertions rather than inventing a separate harness.

- [x] **Step 4: Run CLI tests, document the regular-file limit, then commit.**

```sh
docker compose run --rm test cargo test -p agentos-cli --lib secrets --locked
docker compose run --rm test cargo test -p agentos-cli --test cli --locked
git diff --check
git add crates/agentos-cli/src/secrets.rs crates/agentos-cli/src/lib.rs crates/agentos-cli/src/home.rs crates/agentos-cli/Cargo.toml crates/agentos-cli/tests/cli.rs README.md
git commit -m "fix(cli): bound API key file reads and refuse nonregular paths"
```

Do not remove unrelated key-file documentation. Update the known-limit statements only after the new behavior passes.

## Package verification and self-review

Format the changed Rust files using the pinned container toolchain. Run the host and fake-jail workspace tests after both tasks. Once the independent CI-baseline package lands, require fmt/Clippy as well. Inspect retained model failure output to confirm oversized success follows transport uncertainty and definite rejection remains settled.

The implementation adds no provider retry, journal schema, API key serialization, or ambient guest secret access. Actual byte collection is bounded even without length headers; actual descriptor reads are bounded even if metadata becomes stale. Deterministic mutation-after-open testing can use a descriptor/read seam if needed; do not rely on a racy concurrent-file test as the only proof of the read cap.

Execution status: implemented and verified offline in `codex/v01-completion`; see the
[reviewed milestone report](../../reviews/2026-10-04-offline-milestone-review.md).
