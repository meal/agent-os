mod common;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::job::{HostConfig, JobDir, JobRequest, ScriptedConfig, WorkerConfig};
use agentos_engine::worker::{HostProcessWorker, ScriptedWorker, Worker, run_worker};
use agentos_engine::workspace::workspace_digest;
use common::{contract, copy_dir, fix_patch, fixtures};
use tempfile::TempDir;

struct Fx {
    dir: TempDir,
    task: TaskId,
    contract: Contract,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        Fx {
            dir,
            task: TaskId::new(),
            contract: contract(10).0,
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn host(&self, pinned: Option<Digest>, verify_timeout_secs: u64) -> HostConfig {
        HostConfig {
            snapshot_dir: self.path("snapshot"),
            profile_dir: self.path("profile"),
            work_root: self.path("work"),
            verify_timeout_secs,
            profile_digest: pinned,
        }
    }

    fn worker(&self, pinned: Option<Digest>, verify_timeout_secs: u64) -> HostProcessWorker {
        HostProcessWorker::new(
            &self.host(pinned, verify_timeout_secs),
            Some(self.path("groups")),
        )
    }

    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        EffectRequest {
            effect_id: EffectId::derive(&self.task, 0, &kind, &Digest::of(payload)),
            task_id: self.task.clone(),
            kind,
            payload: payload.to_vec(),
            contract: self.contract.clone(),
            deadline_ts: 0,
        }
    }

    fn job_request(&self, kind: EffectKind, payload: &[u8], worker: WorkerConfig) -> JobRequest {
        let req = self.request(kind, payload);
        JobRequest {
            effect_id: req.effect_id,
            task_id: req.task_id,
            kind: req.kind,
            payload: req.payload,
            contract: req.contract,
            attempt_id: AttemptId::new(),
            lease_generation: 3,
            lease_expiry_ms: i64::MAX,
            task_deadline_ms: i64::MAX,
            worker,
        }
    }

    /// Points the profile at a Python one-liner (on the copy; the fixture is untouched).
    fn script_profile(&self, script: &str) {
        let profile = serde_json::json!({ "id": "pg-test", "command": ["python3", "-c", script], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    fn groups(&self) -> Vec<i32> {
        fs::read_to_string(self.path("groups"))
            .map(|s| s.lines().map(|l| l.parse().unwrap()).collect())
            .unwrap_or_default()
    }
}

fn ctx() -> AttemptCtx {
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        worker: "test".into(),
    }
}

fn succeeded(out: &ExecOutcome) {
    assert_eq!(
        out.receipt.outcome,
        Outcome::Success,
        "{}",
        String::from_utf8_lossy(&out.output)
    );
}

fn reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!(
            "expected failure, got success: {}",
            String::from_utf8_lossy(&out.output)
        ),
    }
}

fn evidence(out: &ExecOutcome) -> serde_json::Value {
    serde_json::from_slice(&out.output).unwrap()
}

/// Snapshot, fix patch, verification through either backend, with fixed requests and contexts.
async fn full_run<F, Fut>(fx: &Fx, ctxs: &[AttemptCtx; 3], run: F) -> Vec<ExecOutcome>
where
    F: Fn(EffectRequest, AttemptCtx) -> Fut,
    Fut: std::future::Future<Output = ExecOutcome>,
{
    let snap = run(fx.request(EffectKind::ReadSnapshot, b""), ctxs[0].clone()).await;
    succeeded(&snap);
    let base = snap.new_workspace.unwrap();
    let patch = fix_patch();
    let applied = run(
        fx.request(
            EffectKind::ApplyPatch {
                expected_base: base,
            },
            patch.as_bytes(),
        ),
        ctxs[1].clone(),
    )
    .await;
    succeeded(&applied);
    let verified = run(
        fx.request(EffectKind::RunVerification, b""),
        ctxs[2].clone(),
    )
    .await;
    succeeded(&verified);
    vec![snap, applied, verified]
}

#[tokio::test]
async fn host_worker_matches_fixture_executor_on_a_full_patch_and_verify_run() {
    let fx = Fx::new();
    let ctxs = [ctx(), ctx(), ctx()];
    let exec = FixtureExecutor::new(fx.path("snapshot"), fx.path("profile"), fx.path("work"));
    let direct = full_run(&fx, &ctxs, |req, ctx| {
        let exec = &exec;
        async move { exec.run(&req, &ctx).await }
    })
    .await;
    let worker = fx.worker(None, 60);
    let via_worker = full_run(&fx, &ctxs, |req, ctx| {
        let worker = &worker;
        async move { worker.run(&req, &ctx).await }
    })
    .await;

    assert_eq!(direct, via_worker);
    assert!(via_worker[2].verification.as_ref().unwrap().passed);
    assert_eq!(
        worker.current_workspace(&fx.task),
        exec.current_workspace(&fx.task)
    );
    let applied = fx.request(
        EffectKind::ApplyPatch {
            expected_base: direct[0].new_workspace.unwrap(),
        },
        fix_patch().as_bytes(),
    );
    let c = ctx();
    assert_eq!(
        worker.reconcile(&applied, &c).await,
        exec.reconcile(&applied, &c).await
    );
    // Only the worker records the verification's process group.
    assert_eq!(fx.groups().len(), 1);
}

#[tokio::test]
async fn verification_with_a_wrong_pinned_digest_fails_before_running_the_profile() {
    let fx = Fx::new();
    let marker = fx.path("marker");
    fx.script_profile(&format!(
        "open({:?}, 'w').write('ran')",
        marker.to_str().unwrap()
    ));
    let actual = workspace_digest(&fx.path("profile")).unwrap();
    let pinned = Digest::of(b"some other profile");
    let worker = fx.worker(Some(pinned), 60);
    succeeded(
        &worker
            .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
            .await,
    );

    let out = worker
        .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
        .await;
    assert_eq!(
        reason(&out),
        format!("profile digest mismatch: pinned {pinned}, found {actual}")
    );
    assert!(out.verification.is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !marker.exists(),
        "the profile ran despite the digest mismatch"
    );
    assert!(fx.groups().is_empty(), "a process group was started");
}

#[tokio::test]
async fn profile_bytes_changed_after_hostconfig_is_built_voids_the_evidence() {
    let fx = Fx::new();
    let pinned = workspace_digest(&fx.path("profile")).unwrap();
    let worker = fx.worker(Some(pinned), 60);
    // The registry copy is swapped for a check that passes everything, after the pin.
    fx.script_profile("print('always passes')");
    let changed = workspace_digest(&fx.path("profile")).unwrap();
    succeeded(
        &worker
            .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
            .await,
    );

    let out = worker
        .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
        .await;
    assert_eq!(
        reason(&out),
        format!("profile digest mismatch: pinned {pinned}, found {changed}")
    );
    assert!(
        out.verification.is_none(),
        "no evidence, so nothing can pass"
    );
}

#[tokio::test]
async fn verification_with_the_right_pinned_digest_passes() {
    let fx = Fx::new();
    let pinned = workspace_digest(&fx.path("profile")).unwrap();
    let worker = fx.worker(Some(pinned), 60);
    let outs = full_run(&fx, &[ctx(), ctx(), ctx()], |req, ctx| {
        let worker = &worker;
        async move { worker.run(&req, &ctx).await }
    })
    .await;
    assert!(outs[2].verification.as_ref().unwrap().passed);
    assert_eq!(
        evidence(&outs[2])["profile_digest"],
        serde_json::json!(pinned)
    );
}

/// Replaces `path` atomically, so a reader sees one whole version or the other.
fn swap_in(path: &std::path::Path, bytes: &[u8]) {
    // The temp file lives outside the profile directory, so staging never copies it.
    let tmp = path.parent().unwrap().with_extension("swap");
    fs::write(&tmp, bytes).unwrap();
    fs::rename(&tmp, path).unwrap();
}

#[tokio::test]
async fn a_source_profile_swapped_during_staging_cannot_pass_a_pinned_verification() {
    let fx = Fx::new();
    // The pinned profile always fails; the swapped-in one would always pass.
    fx.script_profile("import sys; sys.exit(1)");
    let source = fx.path("profile/profile.json");
    let pinned_bytes = fs::read(&source).unwrap();
    let pinned = workspace_digest(&fx.path("profile")).unwrap();
    let tampered =
        serde_json::json!({ "id": "pg-test", "command": ["true"], "protected": true }).to_string();
    let worker = fx.worker(Some(pinned), 60);
    succeeded(
        &worker
            .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
            .await,
    );

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let swapper = {
        let (stop, source, pinned_bytes) = (stop.clone(), source.clone(), pinned_bytes.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                swap_in(&source, tampered.as_bytes());
                swap_in(&source, &pinned_bytes);
            }
        })
    };
    let mut ran = 0;
    for _ in 0..60 {
        let out = worker
            .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
            .await;
        if let Some(report) = &out.verification {
            ran += 1;
            assert!(
                !report.passed,
                "a swapped source produced a passing pinned verification: {}",
                String::from_utf8_lossy(&out.output)
            );
            assert_eq!(evidence(&out)["profile_digest"], serde_json::json!(pinned));
            assert_eq!(
                evidence(&out)["command"],
                serde_json::json!(["python3", "-c", "import sys; sys.exit(1)"])
            );
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    swapper.join().unwrap();
    assert!(
        ran > 0,
        "no verification ran to completion; the race was not exercised"
    );
}

#[tokio::test]
async fn run_worker_rejects_relative_host_paths() {
    let fx = Fx::new();
    let req = fx.job_request(
        EffectKind::ReadSnapshot,
        b"",
        WorkerConfig::Host(fx.host(None, 60)),
    );
    let (job, lock) = JobDir::create(&fx.path("jobs"), &req).unwrap();
    drop(lock);
    for field in ["snapshot_dir", "profile_dir", "work_root"] {
        // A hand-written (or older) request.json, never checked by `JobDir::create`.
        let mut raw: serde_json::Value =
            serde_json::from_slice(&fs::read(job.path.join("request.json")).unwrap()).unwrap();
        let original = raw["worker"]["Host"][field].clone();
        raw["worker"]["Host"][field] = serde_json::json!("relative/dir");
        fs::write(job.path.join("request.json"), raw.to_string()).unwrap();
        let err = run_worker(&job).await.unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{field}: {err}"
        );
        assert!(err.to_string().contains(field), "{err}");
        assert!(job.read_outcome().is_none());
        assert!(!fx.path("work").exists() && !std::path::Path::new("relative").exists());
        raw["worker"]["Host"][field] = original;
        fs::write(job.path.join("request.json"), raw.to_string()).unwrap();
    }
}

#[tokio::test]
async fn scripted_worker_is_refused_without_the_env_guard() {
    assert_ne!(
        std::env::var("AGENTOS_TEST_WORKERS").as_deref(),
        Ok("1"),
        "this test checks the refusal path; run it without AGENTOS_TEST_WORKERS=1"
    );
    let fx = Fx::new();
    let marker = fx.path("marker");
    let worker = ScriptedWorker::new(format!("touch {}", marker.display()));
    let out = worker
        .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
        .await;
    assert_eq!(reason(&out), "scripted workers are disabled");
    assert!(!marker.exists());
}

#[tokio::test]
async fn run_worker_writes_outcome_and_not_receipt() {
    let fx = Fx::new();
    let jobs = fx.path("jobs");

    let snap_req = fx.job_request(
        EffectKind::ReadSnapshot,
        b"",
        WorkerConfig::Host(fx.host(None, 60)),
    );
    let (snap_job, lock) = JobDir::create(&jobs, &snap_req).unwrap();
    drop(lock);
    run_worker(&snap_job).await.unwrap();
    let out = snap_job.read_outcome().expect("outcome written");
    succeeded(&out);
    assert_eq!(out.receipt.attempt_id, snap_req.attempt_id);
    assert_eq!(out.receipt.lease_generation, 3);
    assert_eq!(
        out.new_workspace,
        Some(workspace_digest(&fx.path("work").join(fx.task.as_str()).join("ws")).unwrap())
    );

    let pinned = workspace_digest(&fx.path("profile")).unwrap();
    let verify_req = fx.job_request(
        EffectKind::RunVerification,
        b"",
        WorkerConfig::Host(fx.host(Some(pinned), 60)),
    );
    let (verify_job, lock) = JobDir::create(&jobs, &verify_req).unwrap();
    drop(lock);
    run_worker(&verify_job).await.unwrap();
    let out = verify_job.read_outcome().expect("outcome written");
    succeeded(&out);
    assert!(out.verification.is_some());
    assert_eq!(
        verify_job.groups().len(),
        1,
        "the check's group is recorded in the job's groups file"
    );

    let scripted = WorkerConfig::Scripted(ScriptedConfig {
        script: "echo hi".into(),
    });
    let script_req = fx.job_request(EffectKind::ReadSnapshot, b"", scripted);
    let (script_job, lock) = JobDir::create(&jobs, &script_req).unwrap();
    drop(lock);
    run_worker(&script_job).await.unwrap();
    assert_eq!(
        reason(&script_job.read_outcome().unwrap()),
        "scripted workers are disabled"
    );

    for job in [&snap_job, &verify_job, &script_job] {
        assert!(job.path.join("outcome.json").exists() && job.path.join("outcome.bin").exists());
        assert!(
            !job.path.join("receipt.json").exists(),
            "the worker never writes the receipt"
        );
        assert!(!job.path.join("output.bin").exists());
        assert!(job.read_receipt().is_none());
    }
}

#[tokio::test]
async fn run_worker_reports_an_unreadable_request_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let job = JobDir::open(dir.path()).unwrap();
    assert!(run_worker(&job).await.is_err());
    fs::write(dir.path().join("request.json"), b"{not json").unwrap();
    assert!(run_worker(&job).await.is_err());
    assert!(job.read_outcome().is_none());
}

/// Live (non-zombie) processes whose process group is `pgid`. Parses `/proc/<pid>/stat`
/// after the last `)`, because the command name may contain spaces and parentheses.
fn live_members(pgid: i32) -> Vec<i32> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // fields: state ppid pgrp ...
        if fields.len() > 2 && fields[0] != "Z" && fields[2].parse() == Ok(pgid) {
            found.push(pid);
        }
    }
    found
}

fn own_pgid() -> i32 {
    let stat = fs::read_to_string("/proc/self/stat").unwrap();
    let rest = &stat[stat.rfind(')').unwrap() + 1..];
    rest.split_whitespace().nth(2).unwrap().parse().unwrap()
}

async fn wait_until_empty(pgid: i32) -> Vec<i32> {
    let started = Instant::now();
    loop {
        let live = live_members(pgid);
        if live.is_empty() || started.elapsed() > Duration::from_secs(3) {
            return live;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn check_children_still_get_their_own_process_group_and_are_recorded() {
    let fx = Fx::new();
    fx.script_profile(
        "import os, subprocess\n\
         p = subprocess.Popen(['sleep', '30'])\n\
         print(os.getpid(), os.getpgid(0), p.pid, os.getpgid(p.pid))\n",
    );
    let worker = fx.worker(None, 60);
    succeeded(
        &worker
            .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
            .await,
    );
    let out = worker
        .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
        .await;
    succeeded(&out);

    let stdout = evidence(&out)["stdout"].as_str().unwrap().to_string();
    let ids: Vec<i32> = stdout
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect();
    let [check_pid, check_pgid, sleep_pid, sleep_pgid] = ids[..] else {
        panic!("unexpected stdout {stdout:?}")
    };
    assert_eq!(check_pgid, check_pid, "the check leads its own group");
    assert_ne!(
        check_pgid,
        own_pgid(),
        "the check is not in the worker's group"
    );
    assert_eq!(
        sleep_pgid, check_pgid,
        "the check's child is in the check's group"
    );
    assert_ne!(sleep_pid, check_pid);
    assert_eq!(
        fx.groups(),
        vec![check_pgid],
        "the check's group is recorded"
    );
    assert_eq!(
        wait_until_empty(check_pgid).await,
        Vec::<i32>::new(),
        "members of the check's group survived"
    );
}

#[tokio::test]
async fn a_grandchild_holding_stdout_open_does_not_block_verification_past_the_check_timeout() {
    let fx = Fx::new();
    // The check exits at once; its background child keeps stdout and stderr open.
    fx.script_profile(
        "import subprocess\nsubprocess.Popen(['sleep', '30'])\nprint('checks done')\n",
    );
    let worker = fx.worker(None, 5);
    succeeded(
        &worker
            .run(&fx.request(EffectKind::ReadSnapshot, b""), &ctx())
            .await,
    );
    let started = Instant::now();
    let out = worker
        .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
        .await;
    succeeded(&out);
    assert!(
        evidence(&out)["stdout"]
            .as_str()
            .unwrap()
            .contains("checks done")
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "waited for the grandchild: {:?}",
        started.elapsed()
    );

    // The check itself hangs too: the timeout still ends the run on time.
    fx.script_profile(
        "import subprocess, time\nsubprocess.Popen(['sleep', '30'])\ntime.sleep(30)\n",
    );
    let worker = fx.worker(None, 1);
    let started = Instant::now();
    let out = worker
        .run(&fx.request(EffectKind::RunVerification, b""), &ctx())
        .await;
    assert_eq!(reason(&out), "timeout");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "ran past the timeout: {:?}",
        started.elapsed()
    );

    let groups = fx.groups();
    assert_eq!(groups.len(), 2);
    for pgid in groups {
        assert_eq!(
            wait_until_empty(pgid).await,
            Vec::<i32>::new(),
            "group {pgid} survived"
        );
    }
}
