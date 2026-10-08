//! Checks for acceptance evidence before it is committed: secret exclusion and agreement
//! between the files one live run leaves behind.
//!
//! Recordings keep provider response bodies as JSON arrays of byte values, so a text search
//! of a recording never sees them. [`file_findings`] decodes every such array and scans the
//! bytes as well as the raw file.

use std::path::Path;

use agentos_core::ids::Digest;
use agentos_engine::export::Manifest;
use agentos_engine::model::anthropic::ANTHROPIC_BASE_URL;
use agentos_engine::model::fake::load_recording;
use agentos_engine::model::provider::ProviderResult;
use serde_json::Value;

/// Case-insensitive markers of a key or of an authenticated request in evidence.
pub const PATTERNS: &[&str] = &["sk-ant-", "x-api-key", "authorization", "bearer "];

/// A capability handle is 16 random bytes in hex (`Handle::generate`); evidence carries
/// 8-digit prefixes and 64-digit digests, never a run of exactly 32 hex digits.
const HANDLE_HEX_DIGITS: usize = 32;

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// What in `bytes` looks like a secret: a [`PATTERNS`] marker, any of `secrets` verbatim,
/// or a full capability handle.
pub fn findings(bytes: &[u8], secrets: &[&[u8]]) -> Vec<String> {
    let lower = bytes.to_ascii_lowercase();
    let mut out: Vec<String> = PATTERNS
        .iter()
        .filter(|p| contains(&lower, p.as_bytes()))
        .map(|p| format!("marker {p:?}"))
        .collect();
    if secrets.iter().any(|s| contains(bytes, s)) {
        out.push("a secret, verbatim".into());
    }
    let mut run = 0;
    for b in bytes.iter().chain(std::iter::once(&b' ')) {
        if b.is_ascii_hexdigit() {
            run += 1;
            continue;
        }
        if run == HANDLE_HEX_DIGITS {
            out.push("a full capability handle (32 hex digits)".into());
        }
        run = 0;
    }
    out
}

/// A non-empty array of integers in 0..=255, as bytes.
fn as_bytes(items: &[Value]) -> Option<Vec<u8>> {
    if items.is_empty() {
        return None;
    }
    items
        .iter()
        .map(|v| v.as_u64().filter(|n| *n <= 255).map(|n| n as u8))
        .collect()
}

fn walk(value: &Value, at: &str, secrets: &[&[u8]], out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.extend(
            findings(s.as_bytes(), secrets)
                .into_iter()
                .map(|f| format!("{at}: {f}")),
        ),
        Value::Array(items) => match as_bytes(items) {
            Some(bytes) => out.extend(
                findings(&bytes, secrets)
                    .into_iter()
                    .map(|f| format!("{at} (decoded bytes): {f}")),
            ),
            None => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{at}[{i}]"), secrets, out);
                }
            }
        },
        Value::Object(map) => {
            for (key, item) in map {
                walk(item, &format!("{at}.{key}"), secrets, out);
            }
        }
        _ => {}
    }
}

/// Findings in the file at `path`: its raw bytes, and when it is JSON, every string and
/// every decoded byte array in it.
pub fn file_findings(path: &Path, secrets: &[&[u8]]) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out: Vec<String> = findings(&bytes, secrets);
    if let Ok(json) = serde_json::from_slice::<Value>(&bytes) {
        walk(&json, "$", secrets, &mut out);
    }
    out.into_iter()
        .map(|f| format!("{}: {f}", path.display()))
        .collect()
}

/// Findings in every regular file under `dir`, recursively, except prose (`*.md`).
pub fn tree_findings(dir: &Path, secrets: &[&[u8]]) -> Vec<String> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        let meta = std::fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            out.extend(tree_findings(&path, secrets));
        } else if meta.is_file() && path.extension().is_none_or(|e| e != "md") {
            out.extend(file_findings(&path, secrets));
        }
    }
    out
}

fn json(path: &Path) -> Result<Value, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn digest(value: &Value, what: &str) -> Result<Digest, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("{what}: not a digest ({e})"))
}

fn digests(value: &Value, what: &str) -> Result<Vec<Digest>, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("{what}: not digests ({e})"))
}

fn expect(ok: bool, why: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(why()) }
}

/// One promoted live run: `<dir>/<stem>.evidence.json`, `<dir>/<stem>.replay.json`,
/// `<dir>/manifest.json` and `<dir>/patch.diff`, its recording and the setup record of
/// the acceptance invocation. `dir`'s name is the worker. Every file must agree with the
/// success report.
pub fn check_live_run(
    dir: &Path,
    stem: &str,
    recording: &Path,
    setup: &Path,
) -> Result<(), String> {
    let worker = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let ev = json(&dir.join(format!("{stem}.evidence.json")))?;
    expect(ev["schema_version"] == 1, || {
        format!("evidence schema_version {}", ev["schema_version"])
    })?;
    expect(ev["kind"] == "live-acceptance", || {
        format!("evidence kind {}", ev["kind"])
    })?;
    expect(ev["worker"] == worker, || {
        format!("evidence worker {} in {worker}/", ev["worker"])
    })?;
    expect(ev["model"].as_str().is_some_and(|m| !m.is_empty()), || {
        "evidence has no model".into()
    })?;
    expect(ev["endpoint"] == ANTHROPIC_BASE_URL, || {
        format!("evidence endpoint {}", ev["endpoint"])
    })?;
    expect(ev["single_dispatch_verified"] == true, || {
        "single dispatch not verified".into()
    })?;
    expect(ev["uncertain_model_requests"] == 0, || {
        "uncertain model requests".into()
    })?;
    let commit = ev["commit"].as_str().ok_or("evidence has no commit")?;

    let run = &ev["run"];
    let requests = digests(&run["requests"], "run.requests")?;
    let (workspace, patch, profile) = (
        digest(&run["workspace"], "run.workspace")?,
        digest(&run["patch"], "run.patch")?,
        digest(&run["profile"], "run.profile")?,
    );
    expect(run["model_calls"] == requests.len(), || {
        format!(
            "{} model calls, {} request digests",
            run["model_calls"],
            requests.len()
        )
    })?;
    expect(run["event_types"]["VerifyPassed"] == 1, || {
        "no single VerifyPassed".into()
    })?;

    let replay = json(&dir.join(format!("{stem}.replay.json")))?;
    expect(replay["schema_version"] == 1, || {
        "replay schema_version".into()
    })?;
    expect(replay["kind"] == "offline-replay", || {
        format!("replay kind {}", replay["kind"])
    })?;
    expect(replay["worker"] == worker, || {
        format!("replay worker {}", replay["worker"])
    })?;
    expect(
        replay["request_patch_workspace_profile_match"] == true,
        || "replay did not match".into(),
    )?;
    for field in ["requests", "model_calls", "workspace", "patch", "profile"] {
        expect(replay["run"][field] == run[field], || {
            format!("replay {field} differs from the live run")
        })?;
    }

    let manifest: Manifest = serde_json::from_value(json(&dir.join("manifest.json"))?)
        .map_err(|e| format!("manifest.json: {e}"))?;
    let diff = std::fs::read(dir.join("patch.diff")).map_err(|e| format!("patch.diff: {e}"))?;
    expect(manifest.state == "SUCCEEDED", || {
        format!("manifest state {}", manifest.state)
    })?;
    expect(Digest::of(&diff) == patch, || {
        "patch.diff does not hash to the run's patch".into()
    })?;
    expect(manifest.patch_digest == patch, || {
        "manifest patch_digest differs".into()
    })?;
    expect(manifest.final_workspace_digest == Some(workspace), || {
        "manifest final workspace differs".into()
    })?;
    expect(manifest.verified_digest == Some(workspace), || {
        "manifest verified digest differs".into()
    })?;
    expect(
        manifest.verification_profile_digest == Some(profile),
        || "manifest profile differs".into(),
    )?;
    let called: Vec<Digest> = manifest
        .model_calls
        .iter()
        .map(|c| c.request_digest)
        .collect();
    expect(called == requests, || {
        "manifest model calls differ from the run's requests".into()
    })?;
    let accepted: Vec<_> = manifest
        .verification_results
        .iter()
        .filter(|r| r.accepted_for_final_workspace)
        .collect();
    expect(
        accepted.len() == 1
            && accepted[0].passed
            && accepted[0].workspace_digest == Some(workspace),
        || "no single passing verification accepted for the final workspace".into(),
    )?;
    expect(
        manifest
            .capabilities
            .iter()
            .all(|c| c.handle_prefix.len() == 8),
        || "the manifest carries more than a handle prefix".into(),
    )?;

    let recorded =
        load_recording(recording).map_err(|e| format!("{}: {e}", recording.display()))?;
    let answered: Vec<Digest> = recorded
        .attempts
        .iter()
        .filter(|a| matches!(a.outcome, ProviderResult::Response(..)))
        .map(|a| a.request_digest)
        .collect();
    expect(answered == requests, || {
        "the recording's answered requests differ from the run's".into()
    })?;

    let setup = json(setup)?;
    expect(setup["tier"] == "live", || {
        format!("setup tier {}", setup["tier"])
    })?;
    expect(setup["commit"] == commit, || {
        format!(
            "setup commit {} != evidence commit {commit}",
            setup["commit"]
        )
    })?;
    Ok(())
}
