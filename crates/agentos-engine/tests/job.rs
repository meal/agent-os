mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome, Receipt};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::ExecOutcome;
use agentos_engine::job::{
    HostConfig, JobDir, JobRequest, JobState, JobStatus, KillReason, ScriptedConfig, WorkerConfig,
};
use tempfile::TempDir;

fn effect_id(n: u32) -> EffectId {
    let (_, d) = common::contract(1);
    EffectId::derive(&TaskId::new(), n, &EffectKind::ReadSnapshot, &d)
}

fn scripted() -> WorkerConfig {
    WorkerConfig::Scripted(ScriptedConfig { script: "noop".into() })
}

fn request_for(effect: &EffectId, generation: u64, worker: WorkerConfig) -> JobRequest {
    let (contract, _) = common::contract(1);
    JobRequest {
        effect_id: effect.clone(),
        task_id: TaskId::new(),
        kind: EffectKind::ReadSnapshot,
        payload: b"payload".to_vec(),
        contract,
        attempt_id: AttemptId::new(),
        lease_generation: generation,
        lease_expiry_ms: 1_000,
        task_deadline_ms: 2_000,
        worker,
    }
}

fn request() -> JobRequest {
    request_for(&effect_id(1), 1, scripted())
}

fn status(state: JobState) -> JobStatus {
    JobStatus { state, reason: None, supervisor_pid: Some(42), worker_pgid: Some(43), updated_ms: 7 }
}

fn outcome_for(req: &JobRequest, output: &[u8]) -> ExecOutcome {
    ExecOutcome {
        receipt: Receipt {
            effect_id: req.effect_id.clone(),
            attempt_id: req.attempt_id.clone(),
            lease_generation: req.lease_generation,
            outcome: Outcome::Success,
            result_digest: Some(Digest::of(output)),
        },
        output: output.to_vec(),
        new_workspace: None,
        verification: None,
    }
}

fn root() -> TempDir {
    TempDir::new().unwrap()
}

#[test]
fn request_roundtrip() {
    let root = root();
    let req = request();
    let (job, _lock) = JobDir::create(root.path(), &req).unwrap();
    assert_eq!(job.request().unwrap(), req);
    let reopened = JobDir::open(&job.path).unwrap();
    assert_eq!(reopened.request().unwrap(), req);
    assert_eq!(job.path, root.path().join(format!("{}-{}", req.effect_id, req.attempt_id)));
}

#[test]
fn status_roundtrip() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    assert_eq!(job.read_status(), None);
    let mut s = status(JobState::Killed);
    s.reason = Some(KillReason::Deadline);
    job.write_status(&s).unwrap();
    assert_eq!(job.read_status(), Some(s));
}

#[test]
fn torn_status_reads_as_none() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    fs::write(job.path.join("status.json"), b"{\"state\":\"Run").unwrap();
    assert_eq!(job.read_status(), None);
}

#[test]
fn status_write_is_atomic() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    job.write_status(&status(JobState::Starting)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let job = JobDir::open(&job.path).unwrap();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            while !stop.load(Ordering::SeqCst) {
                assert!(job.read_status().is_some(), "reader saw a partial status file");
                reads += 1;
            }
            reads
        })
    };
    for i in 0..2_000 {
        let mut s = status(if i % 2 == 0 { JobState::Running } else { JobState::Starting });
        s.updated_ms = i;
        job.write_status(&s).unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    assert!(reader.join().unwrap() > 0);
}

#[test]
fn receipt_roundtrip() {
    let root = root();
    let req = request();
    let (job, _lock) = JobDir::create(root.path(), &req).unwrap();
    assert_eq!(job.read_receipt(), None);
    let out = outcome_for(&req, b"result bytes");
    job.write_receipt(&out).unwrap();
    assert_eq!(job.read_receipt(), Some(out));
}

#[test]
fn receipt_requires_both_files_and_a_matching_digest() {
    let root = root();
    let req = request();
    let (job, _lock) = JobDir::create(root.path(), &req).unwrap();
    let out = outcome_for(&req, b"result bytes");
    job.write_receipt(&out).unwrap();

    let bin = job.path.join("output.bin");
    let json = job.path.join("receipt.json");

    fs::write(&bin, b"tampered").unwrap();
    assert_eq!(job.read_receipt(), None, "digest mismatch");
    fs::write(&bin, b"result bytes").unwrap();
    assert_eq!(job.read_receipt(), Some(out.clone()));

    fs::remove_file(&bin).unwrap();
    assert_eq!(job.read_receipt(), None, "output.bin missing");
    fs::write(&bin, b"result bytes").unwrap();

    let saved = fs::read(&json).unwrap();
    fs::remove_file(&json).unwrap();
    assert_eq!(job.read_receipt(), None, "receipt.json missing");
    fs::write(&json, &saved[..saved.len() / 2]).unwrap();
    assert_eq!(job.read_receipt(), None, "receipt.json torn");
}

#[test]
fn outcome_roundtrip_and_validation() {
    let root = root();
    let req = request();
    let (job, _lock) = JobDir::create(root.path(), &req).unwrap();
    assert_eq!(job.read_outcome(), None);
    let out = outcome_for(&req, b"outcome bytes");
    job.write_outcome(&out).unwrap();
    assert_eq!(job.read_outcome(), Some(out));
    // receipt files are a separate pair.
    assert_eq!(job.read_receipt(), None);
    fs::write(job.path.join("outcome.bin"), b"other").unwrap();
    assert_eq!(job.read_outcome(), None);
    fs::remove_file(job.path.join("outcome.bin")).unwrap();
    assert_eq!(job.read_outcome(), None);
}

#[test]
fn create_refuses_existing_directory_and_relative_paths_and_traversal_ids() {
    let root = root();
    let req = request();
    let (_job, _lock) = JobDir::create(root.path(), &req).unwrap();
    let again = JobDir::create(root.path(), &req).err().unwrap();
    assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);

    let rel = HostConfig {
        snapshot_dir: PathBuf::from("snap"),
        profile_dir: PathBuf::from("/abs/profile"),
        work_root: PathBuf::from("/abs/work"),
        verify_timeout_secs: 5,
        profile_digest: None,
    };
    for field in 0..3 {
        let mut h = rel.clone();
        h.snapshot_dir = PathBuf::from("/abs/snap");
        match field {
            0 => h.snapshot_dir = PathBuf::from("snap"),
            1 => h.profile_dir = PathBuf::from("profile"),
            _ => h.work_root = PathBuf::from("work"),
        }
        let bad = request_for(&effect_id(2), 1, WorkerConfig::Host(h));
        let err = JobDir::create(root.path(), &bad).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "field {field}");
    }
    let mut abs = rel;
    abs.snapshot_dir = PathBuf::from("/abs/snap");
    let ok = request_for(&effect_id(2), 1, WorkerConfig::Host(abs));
    JobDir::create(root.path(), &ok).unwrap();

    for evil in ["../escape", "a/b", "..", ""] {
        let mut req = request();
        req.effect_id = serde_json::from_str(&format!("{evil:?}")).unwrap();
        let err = JobDir::create(root.path(), &req).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "effect id {evil:?}");
        let mut req = request();
        req.attempt_id = serde_json::from_str(&format!("{evil:?}")).unwrap();
        let err = JobDir::create(root.path(), &req).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "attempt id {evil:?}");
    }
    assert!(!root.path().parent().unwrap().join("escape").exists());
}

#[test]
fn create_returns_a_held_lock_and_dropping_it_frees_it() {
    let root = root();
    let (job, lock) = JobDir::create(root.path(), &request()).unwrap();
    assert!(job.lock_held());
    drop(lock);
    assert!(!job.lock_held());
    assert!(!job.lock_held(), "a probe must not leave the lock held");
    let other = fs::File::open(job.path.join("lock")).unwrap();
    other.try_lock().expect("exclusive lock is free after probes");
}

#[test]
fn concurrent_probes_of_a_dead_job_both_see_it_free() {
    let root = root();
    let (job, lock) = JobDir::create(root.path(), &request()).unwrap();
    drop(lock);
    for _ in 0..200 {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let job = JobDir::open(&job.path).unwrap();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    job.lock_held()
                })
            })
            .collect();
        for h in handles {
            assert!(!h.join().unwrap());
        }
    }
}

#[test]
fn list_does_not_confuse_effect_a_with_effect_a_dash_b() {
    let root = root();
    let a: EffectId = serde_json::from_str("\"a\"").unwrap();
    let ab: EffectId = serde_json::from_str("\"a-b\"").unwrap();
    let (ja, _l1) = JobDir::create(root.path(), &request_for(&a, 1, scripted())).unwrap();
    let (jab, _l2) = JobDir::create(root.path(), &request_for(&ab, 1, scripted())).unwrap();
    let la: Vec<_> = JobDir::list(root.path(), &a).into_iter().map(|j| j.path).collect();
    assert_eq!(la, vec![ja.path]);
    let lab: Vec<_> = JobDir::list(root.path(), &ab).into_iter().map(|j| j.path).collect();
    assert_eq!(lab, vec![jab.path]);
}

#[test]
fn a_job_with_only_request_json_and_a_held_lock_is_alive() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    assert_eq!(job.read_status(), None);
    assert!(!job.is_dead());
}

#[test]
fn a_job_with_a_free_lock_and_no_status_is_dead() {
    let root = root();
    let (job, lock) = JobDir::create(root.path(), &request()).unwrap();
    drop(lock);
    assert!(job.is_dead());
    // a missing lock file also reads as not held.
    fs::remove_file(job.path.join("lock")).unwrap();
    assert!(!job.lock_held());
}

#[test]
fn terminal_status_or_receipt_means_dead_even_if_the_lock_is_held() {
    let root = root();
    let req = request();
    let (job, _lock) = JobDir::create(root.path(), &req).unwrap();
    job.write_status(&status(JobState::Running)).unwrap();
    assert!(!job.is_dead());
    job.write_status(&status(JobState::Exited)).unwrap();
    assert!(job.is_dead());
    job.write_status(&status(JobState::Killed)).unwrap();
    assert!(job.is_dead());

    let root2 = TempDir::new().unwrap();
    let req2 = request();
    let (job2, _lock2) = JobDir::create(root2.path(), &req2).unwrap();
    assert!(!job2.is_dead());
    job2.write_receipt(&outcome_for(&req2, b"x")).unwrap();
    assert!(job2.lock_held());
    assert!(job2.is_dead());
}

#[test]
fn list_orders_attempts_by_lease_generation_and_ignores_tmp() {
    let root = root();
    let effect = effect_id(1);
    let other = effect_id(2);
    let mut locks = Vec::new();
    let mut made = Vec::new();
    for generation in [3u64, 1, 2] {
        let req = request_for(&effect, generation, scripted());
        let (job, lock) = JobDir::create(root.path(), &req).unwrap();
        made.push((generation, job.path.clone()));
        locks.push(lock);
    }
    let (_o, l) = JobDir::create(root.path(), &request_for(&other, 9, scripted())).unwrap();
    locks.push(l);
    fs::create_dir(root.path().join(format!("{effect}-deadbeef.tmp"))).unwrap();
    fs::create_dir(root.path().join(format!(".{effect}-x.tmp"))).unwrap();
    // an unreadable request.json sorts first.
    let broken = root.path().join(format!("{effect}-broken"));
    fs::create_dir(&broken).unwrap();
    fs::write(broken.join("request.json"), b"{").unwrap();

    let listed: Vec<PathBuf> = JobDir::list(root.path(), &effect).into_iter().map(|j| j.path).collect();
    made.sort();
    let mut expected = vec![broken];
    expected.extend(made.into_iter().map(|(_, p)| p));
    assert_eq!(listed, expected);
    assert!(JobDir::list(Path::new("/nonexistent/jobs"), &effect).is_empty());
}

#[test]
fn cancel_marker_roundtrip() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    assert!(!job.cancel_requested());
    job.drop_cancel().unwrap();
    assert!(job.cancel_requested());
}

#[test]
fn groups_file_append_and_read() {
    let root = root();
    let (job, _lock) = JobDir::create(root.path(), &request()).unwrap();
    assert!(job.groups().is_empty());
    job.record_group(100).unwrap();
    job.record_group(200).unwrap();
    assert_eq!(job.groups(), vec![100, 200]);
    let mut f = fs::OpenOptions::new().append(true).open(job.path.join("groups")).unwrap();
    std::io::Write::write_all(&mut f, b"garbage\n300\n").unwrap();
    assert_eq!(job.groups(), vec![100, 200, 300]);
}
