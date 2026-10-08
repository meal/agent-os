//! The committed acceptance evidence (`docs/evidence/`) and recordings
//! (`fixtures/transcripts/`) carry no key, no authenticated request and no full capability
//! handle, and each promoted live run's files agree with one another. Runs in the default
//! tier: it reads only committed files.

mod common;

use std::path::{Path, PathBuf};

use agentos_core::broker::Handle;
use agentos_core::ids::Digest;
use agentos_engine::model::fake::{AttemptRecording, RecordedAttempt};
use agentos_engine::model::provider::{ProviderResult, Usage};
use common::evidence::{check_live_run, file_findings, findings, tree_findings};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `docs/evidence/<date>/<worker>/<stem>.evidence.json`, as (worker dir, stem).
fn promoted_runs() -> Vec<(PathBuf, String)> {
    let mut runs = Vec::new();
    for date in std::fs::read_dir(repo().join("docs/evidence")).unwrap() {
        let date = date.unwrap().path();
        if !date.is_dir() {
            continue;
        }
        for worker in std::fs::read_dir(&date).unwrap() {
            let worker = worker.unwrap().path();
            if !worker.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&worker).unwrap() {
                let name = file.unwrap().file_name().into_string().unwrap();
                if let Some(stem) = name.strip_suffix(".evidence.json") {
                    runs.push((worker.clone(), stem.to_string()));
                }
            }
        }
    }
    runs.sort();
    runs
}

fn recording_for(worker_dir: &Path, stem: &str) -> PathBuf {
    let worker = worker_dir.file_name().unwrap().to_str().unwrap();
    repo().join(format!("fixtures/transcripts/live/{worker}-{stem}.json"))
}

fn recording_with_response(body: &[u8]) -> Vec<u8> {
    serde_json::to_vec_pretty(&AttemptRecording {
        schema_version: 2,
        attempts: vec![RecordedAttempt {
            request_digest: Digest::of(b"request"),
            outcome: ProviderResult::Response(body.to_vec(), Usage::default()),
        }],
    })
    .unwrap()
}

#[test]
fn a_key_inside_an_encoded_response_body_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");
    let body = br#"{"content":[{"type":"text","text":"echo sk-ant-api03-not-a-real-key"}]}"#;
    std::fs::write(&path, recording_with_response(body)).unwrap();

    // The blind spot: the body is a byte array, so the file's text never contains the key.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("sk-ant"),
        "the recording stores bodies as bytes"
    );
    let found = file_findings(&path, &[]);
    assert!(
        found
            .iter()
            .any(|f| f.contains("decoded bytes") && f.contains("sk-ant-")),
        "{found:?}"
    );
}

#[test]
fn exact_secret_bytes_are_found_in_decoded_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");
    std::fs::write(
        &path,
        recording_with_response(b"leaked: correct-horse-battery"),
    )
    .unwrap();
    assert!(file_findings(&path, &[]).is_empty());
    let found = file_findings(&path, &[b"correct-horse-battery"]);
    assert!(
        found.iter().any(|f| f.contains("a secret, verbatim")),
        "{found:?}"
    );
}

#[test]
fn a_full_handle_is_found_but_prefixes_and_digests_are_not() {
    let handle = Handle::generate();
    let digest = Digest::of(b"x").to_string();
    let quoted = |s: &str| format!("{{\"value\":\"{s}\"}}").into_bytes();
    assert!(
        findings(&quoted(&handle.to_string()), &[])
            .iter()
            .any(|f| f.contains("handle"))
    );
    assert!(findings(&quoted(handle.prefix()), &[]).is_empty());
    assert!(findings(&quoted(&digest), &[]).is_empty());
    assert!(findings(b"Authorization: Bearer abc", &[]).len() == 2);
    assert!(findings(b"X-Api-Key: abc", &[]).len() == 1);
}

#[test]
fn committed_evidence_and_recordings_contain_no_secrets() {
    let mut found = tree_findings(&repo().join("docs/evidence"), &[]);
    found.extend(tree_findings(&repo().join("fixtures/transcripts"), &[]));
    assert!(
        found.is_empty(),
        "secret-like content in committed evidence:\n{}",
        found.join("\n")
    );
}

#[test]
fn every_promoted_live_run_agrees_with_its_files() {
    let runs = promoted_runs();
    assert!(
        !runs.is_empty(),
        "no promoted live runs under docs/evidence"
    );
    for (dir, stem) in runs {
        let setup = dir.parent().unwrap().join("live-setup.json");
        check_live_run(&dir, &stem, &recording_for(&dir, &stem), &setup)
            .unwrap_or_else(|why| panic!("{}/{stem}: {why}", dir.display()));
    }
}

/// A copy of the first promoted run with `edit` applied to it fails the check.
fn tampered(edit: impl FnOnce(&Path)) -> Result<(), String> {
    let (dir, stem) = promoted_runs().remove(0);
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join(dir.file_name().unwrap());
    common::copy_dir(&dir, &copy);
    let setup = tmp.path().join("live-setup.json");
    std::fs::copy(dir.parent().unwrap().join("live-setup.json"), &setup).unwrap();
    let recording = tmp.path().join("recording.json");
    std::fs::copy(recording_for(&dir, &stem), &recording).unwrap();
    edit(tmp.path());
    check_live_run(&copy, &stem, &recording, &setup)
}

#[test]
fn a_tampered_run_fails_the_check() {
    assert_eq!(tampered(|_| {}), Ok(()), "an untouched copy passes");

    let patch = tampered(|root| {
        let worker = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.is_dir())
            .unwrap();
        let mut diff = std::fs::read(worker.join("patch.diff")).unwrap();
        diff.extend_from_slice(b"\n");
        std::fs::write(worker.join("patch.diff"), diff).unwrap();
    });
    assert!(patch.unwrap_err().contains("patch.diff"));

    let commit = tampered(|root| {
        std::fs::write(
            root.join("live-setup.json"),
            br#"{"schema_version":1,"tier":"live","commit":"0000000","image":"python-stdlib-v1"}"#,
        )
        .unwrap();
    });
    assert!(commit.unwrap_err().contains("setup commit"));

    let recording = tampered(|root| {
        std::fs::write(root.join("recording.json"), recording_with_response(b"{}")).unwrap();
    });
    assert!(recording.unwrap_err().contains("recording"));
}

#[test]
fn the_scan_reads_the_response_bodies_of_the_real_recordings() {
    // `stop_reason` occurs only inside the byte-encoded response bodies, so finding it proves
    // the clean result above covered them (a format change would make it vacuous).
    for (dir, stem) in promoted_runs() {
        let recording = recording_for(&dir, &stem);
        let text = std::fs::read_to_string(&recording).unwrap();
        assert!(!text.contains("stop_reason"), "{}", recording.display());
        let found = file_findings(&recording, &[b"stop_reason"]);
        assert!(
            found.iter().any(|f| f.contains("decoded bytes")),
            "{}: response bodies were not decoded",
            recording.display()
        );
    }
}
