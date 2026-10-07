//! Collection uses actual journal/effect/blob state and real file locks.
mod common;
use agentos_core::{broker::Resource, effect::EffectKind, ids::Digest, state::TaskEvent};
use agentos_engine::{
    executor::ExecOutcome,
    gc::{CollectionStage, HeldDriverLock, Options, Report, collect_with_hook},
    job::{JobDir, JobRequest, WorkerConfig},
    steps,
};
use common::Env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

/// One collection pass over `root` under its driver lock, as `agentos gc` runs it.
fn gc_hook(
    root: &Path,
    env: &Env,
    dry_run: bool,
    hook: &dyn Fn(CollectionStage, &Path) -> std::io::Result<()>,
) -> Report {
    gc_with(root, env, Options::new(dry_run), hook)
}
fn gc_with(
    root: &Path,
    env: &Env,
    opts: Options,
    hook: &dyn Fn(CollectionStage, &Path) -> std::io::Result<()>,
) -> Report {
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    let held = HeldDriverLock::verify(root, &lock).unwrap();
    collect_with_hook(&held, &env.db, &env.blobs, opts, hook).unwrap()
}
fn gc(root: &Path, env: &Env, dry_run: bool) -> Report {
    gc_hook(root, env, dry_run, &|_, _| Ok(()))
}

fn settled() -> (Env, JobDir, PathBuf) {
    settled_with(|_| {})
}

/// `settled()`, running `before_failed` just before the task fails.
fn settled_with(before_failed: impl Fn(&Env)) -> (Env, JobDir, PathBuf) {
    let env = Env::with_model(3, 10);
    env.db.append(&env.task, &TaskEvent::Started).unwrap();
    let base = env.db.task(&env.task).unwrap().workspace_digest;
    let rec = steps::intend(
        &env.db,
        &env.task,
        EffectKind::ReadSnapshot,
        Digest::of(b""),
        &base,
        &Resource::Task,
    )
    .unwrap();
    let ctx = steps::dispatch(&env.db, &rec).unwrap();
    let req = steps::request(&rec, Vec::new(), &env.contract, 0);
    let out = ExecOutcome::success(&req, &ctx, b"published snapshot".to_vec());
    let request = JobRequest {
        effect_id: rec.effect_id.clone(),
        task_id: env.task.clone(),
        kind: rec.kind.clone(),
        payload: Vec::new(),
        contract: env.contract.clone(),
        attempt_id: ctx.attempt_id.clone(),
        lease_generation: ctx.lease_generation,
        lease_expiry_ms: 0,
        task_deadline_ms: 0,
        worker: WorkerConfig::Host(common::host_config(env.dir.path())),
    };
    let (job, lock) = JobDir::create(&env.dir.path().join("jobs"), &request).unwrap();
    job.write_receipt(&out).unwrap();
    job.write_outcome(&out).unwrap();
    drop(lock);
    let digest = steps::store_result(&env.blobs, &out).unwrap();
    steps::register_result(&env.db, &rec, &out, &digest).unwrap();
    env.db
        .complete_effect(&rec.effect_id, &out.receipt, Some(&digest), None)
        .unwrap();
    let model = steps::intend(
        &env.db,
        &env.task,
        EffectKind::ModelCall {
            model: "fake".into(),
            turn: 1,
        },
        Digest::of(b"{}"),
        &base,
        &Resource::Task,
    )
    .unwrap();
    let ctx = steps::dispatch(&env.db, &model).unwrap();
    let out = ExecOutcome::success(
        &steps::request(&model, Vec::new(), &env.contract, 0),
        &ctx,
        b"model response".to_vec(),
    );
    let retention = env
        .dir
        .path()
        .join("model")
        .join(format!("{}-{}", model.effect_id, ctx.attempt_id));
    fs::create_dir_all(&retention).unwrap();
    fs::write(
        retention.join("response.json"),
        serde_json::to_vec(&out).unwrap(),
    )
    .unwrap();
    let digest = steps::store_result(&env.blobs, &out).unwrap();
    steps::register_result(&env.db, &model, &out, &digest).unwrap();
    env.db
        .complete_effect(&model.effect_id, &out.receipt, Some(&digest), None)
        .unwrap();
    before_failed(&env);
    env.db
        .append(
            &env.task,
            &TaskEvent::Failed {
                reason: "finished fixture".into(),
            },
        )
        .unwrap();
    let work = env.dir.path().join("work").join(env.task.as_str());
    fs::create_dir_all(work.join("ws")).unwrap();
    fs::write(work.join("ws/file"), b"transient").unwrap();
    fs::write(work.join("ws.img"), b"transient image").unwrap();
    (env, job, retention)
}

#[test]
fn settled_copies_are_deleted_and_collection_is_idempotent() {
    let (env, job, retention) = settled();
    let before = env.events();
    let report = gc(env.dir.path(), &env, true);
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|e| e.status == "candidate")
            .count(),
        4,
        "{report:?}"
    );
    assert!(job.path.exists() && retention.exists());
    let report = gc(env.dir.path(), &env, false);
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|e| e.status == "deleted")
            .count(),
        4,
        "{report:?}"
    );
    assert!(job.path.is_dir() && !job.path.join("output.bin").exists());
    assert!(!retention.exists());
    assert!(
        !env.dir
            .path()
            .join("work")
            .join(env.task.as_str())
            .join("ws")
            .exists()
    );
    assert!(
        env.dir
            .path()
            .join("work")
            .join(env.task.as_str())
            .join("ws.lock")
            .exists()
    );
    let again = gc(env.dir.path(), &env, false);
    assert_eq!(again.summary.collected, 1, "{again:?}");
    assert_eq!(again.entries.len(), 1, "{again:?}");
    assert!(!again.failed());
    assert_eq!(env.events(), before);
    for digest in env.db.referenced_blobs().unwrap() {
        env.blobs.get(&digest).unwrap();
    }
}

#[test]
fn a_live_job_retains_the_whole_task() {
    let (env, job, retention) = settled();
    let held = File::options()
        .write(true)
        .open(job.path.join("lock"))
        .unwrap();
    held.lock().unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(
        !report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(job.path.join("output.bin").exists() && retention.exists());
    assert!(
        report
            .entries
            .iter()
            .all(|e| e.status == "retained" && e.reason.contains("the job is live")),
        "{report:?}"
    );
    assert!(!report.failed());
}

#[test]
fn a_missing_receipt_keeps_the_attempt_but_not_its_settled_siblings() {
    let (env, job, retention) = settled();
    fs::remove_file(job.path.join("receipt.json")).unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(job.path.join("output.bin").exists(), "{report:?}");
    assert!(!retention.exists(), "{report:?}");
    assert_eq!(status_of(&report, &rel(&env, &job.path)), ["retained"]);
}

#[test]
fn busy_workspace_inspection_or_jail_retains_all_task_copies() {
    for condition in ["lock", "inspection", "jail"] {
        let (env, job, retention) = settled();
        let work = env.dir.path().join("work").join(env.task.as_str());
        let held = match condition {
            "lock" => {
                let f = File::create(work.join("ws.lock")).unwrap();
                f.lock().unwrap();
                Some(f)
            }
            "inspection" => {
                fs::create_dir_all(
                    env.dir
                        .path()
                        .join("inspect")
                        .join(env.task.as_str())
                        .join("boot"),
                )
                .unwrap();
                None
            }
            _ => {
                fs::create_dir(job.path.join("jail")).unwrap();
                None
            }
        };
        let report = gc(env.dir.path(), &env, false);
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{condition}: {report:?}"
        );
        assert!(job.path.exists() && retention.exists() && work.join("ws.img").exists());
        drop(held);
    }
}

#[test]
fn terminal_outstanding_and_pending_cancel_tasks_are_retained() {
    for cancel in [false, true] {
        let env = Env::new(10);
        env.db.append(&env.task, &TaskEvent::Started).unwrap();
        let base = env.db.task(&env.task).unwrap().workspace_digest;
        steps::intend(
            &env.db,
            &env.task,
            EffectKind::ReadSnapshot,
            Digest::of(b""),
            &base,
            &Resource::Task,
        )
        .unwrap();
        env.db
            .append(
                &env.task,
                &if cancel {
                    TaskEvent::CancelRequested
                } else {
                    TaskEvent::Failed {
                        reason: "terminal with intended effect".into(),
                    }
                },
            )
            .unwrap();
        let work = env
            .dir
            .path()
            .join("work")
            .join(env.task.as_str())
            .join("ws");
        fs::create_dir_all(&work).unwrap();
        let report = gc(env.dir.path(), &env, false);
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{report:?}"
        );
        assert!(work.exists());
    }
}

#[test]
fn symlink_roots_children_and_hardlinks_are_refused() {
    for condition in ["root", "child", "hardlink"] {
        let (env, job, _) = settled();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, b"keep").unwrap();
        if condition == "root" {
            fs::rename(
                env.dir.path().join("jobs"),
                env.dir.path().join("saved-jobs"),
            )
            .unwrap();
            std::os::unix::fs::symlink(outside.path(), env.dir.path().join("jobs")).unwrap();
        } else if condition == "child" {
            std::os::unix::fs::symlink(&secret, job.path.join("redirect")).unwrap();
        } else {
            fs::hard_link(&secret, job.path.join("linked")).unwrap();
        }
        let report = gc(env.dir.path(), &env, false);
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{report:?}"
        );
        assert_eq!(fs::read(&secret).unwrap(), b"keep");
    }
}

#[test]
fn corrupt_blob_or_response_is_retained() {
    for blob in [false, true] {
        let (env, job, retention) = settled();
        if blob {
            let digest = env
                .db
                .referenced_blobs()
                .unwrap()
                .into_iter()
                .next()
                .unwrap()
                .to_string();
            fs::write(
                env.dir
                    .path()
                    .join("blobs/objects")
                    .join(&digest[..2])
                    .join(&digest[2..]),
                b"corrupt",
            )
            .unwrap();
        } else {
            fs::write(retention.join("response.json"), b"malformed").unwrap();
        }
        let report = gc(env.dir.path(), &env, false);
        if blob {
            assert!(
                !report.entries.iter().any(|e| e.status == "deleted"),
                "{report:?}"
            );
            assert!(report.failed());
            assert!(job.path.join("output.bin").exists() && retention.exists());
        } else {
            assert_eq!(
                status_of(&report, &rel(&env, &retention)),
                ["retained"],
                "{report:?}"
            );
            assert!(retention.join("response.json").exists());
            assert!(!job.path.join("output.bin").exists(), "{report:?}");
        }
    }
}

#[test]
fn unknown_names_stop_the_pass_before_any_deletion() {
    let (env, job, retention) = settled();
    for n in 0..1001 {
        fs::create_dir(env.dir.path().join("model").join(format!("unknown-{n}"))).unwrap();
    }
    let report = gc(env.dir.path(), &env, false);
    assert!(report.entries.len() <= 1000);
    assert!(!report.entries.iter().any(|e| e.status == "deleted"));
    assert!(report.failed(), "{report:?}");
    assert!(job.path.join("output.bin").exists() && retention.exists());
}

#[test]
fn socket_files_are_retained_outside_the_firecracker_job_socket() {
    for name in ["v.sock", "unrecognized.sock"] {
        let (env, job, retention) = settled();
        let (path, _held_dir) =
            agentos_engine::guestlink::socket_path(&job.path.join(name)).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(path).unwrap();
        drop(socket);
        let report = gc(env.dir.path(), &env, false);
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{report:?}"
        );
        assert!(job.path.exists() && retention.exists());
    }
}

#[test]
fn a_nonterminal_task_with_settled_effects_keeps_its_workspace() {
    let env = Env::new(10);
    let work = env
        .dir
        .path()
        .join("work")
        .join(env.task.as_str())
        .join("ws");
    fs::create_dir_all(&work).unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert_eq!(report.entries[0].status, "retained");
    assert!(!report.failed());
    assert!(work.exists());
}

#[test]
fn an_overlarge_candidate_tree_is_never_partially_removed() {
    let (env, job, retention) = settled();
    for n in 0..10001 {
        fs::write(job.path.join(format!("entry-{n}")), b"").unwrap();
    }
    let report = gc(env.dir.path(), &env, false);
    assert!(!report.entries.iter().any(|e| e.status == "deleted"));
    assert!(job.path.exists() && retention.exists());
}

#[test]
fn a_job_request_for_a_different_effect_is_retained() {
    let (env, job, retention) = settled();
    let mut req = job.request().unwrap();
    req.effect_id = serde_json::from_value(serde_json::Value::String("a".repeat(64))).unwrap();
    fs::write(
        job.path.join("request.json"),
        serde_json::to_vec(&req).unwrap(),
    )
    .unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(job.path.join("output.bin").exists(), "{report:?}");
    assert_eq!(
        status_of(&report, &rel(&env, &job.path)),
        ["retained"],
        "{report:?}"
    );
    assert!(!retention.exists(), "the mismatch keeps only that attempt");
}

#[test]
fn historical_empty_workspace_parents_do_not_disable_new_collection() {
    let (env, job, retention) = settled();
    for n in 0..1001 {
        let name = format!("00000000-0000-0000-0000-{n:012x}");
        let old = env.dir.path().join("work").join(name);
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("ws.lock"), b"").unwrap();
    }
    let report = gc(env.dir.path(), &env, false);
    assert!(
        report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(!job.path.join("output.bin").exists() && !retention.exists());
}

#[test]
fn ancestor_substitution_cannot_redirect_workspace_deletion() {
    let (env, _, _) = settled();
    let outside = tempfile::tempdir().unwrap();
    let external_ws = outside.path().join(env.task.as_str()).join("ws");
    fs::create_dir_all(&external_ws).unwrap();
    fs::write(external_ws.join("secret"), b"must survive").unwrap();
    let root = env.dir.path();
    let report = gc_hook(root, &env, false, &|stage, _| {
        if stage == CollectionStage::Validated {
            fs::rename(root.join("work"), root.join("original-work"))?;
            std::os::unix::fs::symlink(outside.path(), root.join("work"))?;
        }
        Ok(())
    });
    assert_eq!(
        fs::read(external_ws.join("secret")).unwrap(),
        b"must survive",
        "{report:?}"
    );
}

#[test]
fn interrupted_deletion_retries_after_job_receipt_has_been_removed() {
    let (env, job, retention) = settled();
    let root = env.dir.path();
    let receipt = job.path.join("receipt.json");
    let report = gc_hook(root, &env, false, &|stage, original| {
        if stage == CollectionStage::Staged && original.starts_with("jobs") {
            fs::remove_file(&receipt)?;
            return Err(std::io::Error::from_raw_os_error(28));
        }
        Ok(())
    });
    assert!(report.failed(), "{report:?}");
    assert!(retention.exists(), "the rest of the task stops: {report:?}");
    let retry = gc(root, &env, false);
    assert!(!retry.failed(), "{retry:?}");
    assert_eq!(
        status_of(&retry, &rel(&env, &job.path.join("output.bin"))),
        ["deleted"],
        "{retry:?}"
    );
    assert!(!job.path.join("output.bin").exists());
    assert!(!retention.exists());
    assert_eq!(fs::read_dir(root.join("gc-trash")).unwrap().count(), 0);
    let third = gc(root, &env, false);
    assert!(
        third
            .entries
            .iter()
            .all(|e| e.status == "retained" || e.status == "collected"),
        "{third:?}"
    );
}

#[test]
fn candidate_replacement_after_validation_is_retained() {
    let (env, job, retention) = settled();
    let original = job.path.clone();
    let moved = env.dir.path().join("original-job");
    let report = gc_hook(env.dir.path(), &env, false, &|stage, _| {
        if stage == CollectionStage::Validated {
            fs::rename(&original, &moved)?;
            fs::create_dir(&original)?;
            fs::write(original.join("foreign"), b"retain replacement")?;
        }
        Ok(())
    });
    let mut foreign = original.join("foreign");
    if !foreign.exists() {
        for entry in fs::read_dir(env.dir.path().join("gc-trash")).unwrap() {
            let maybe = entry.unwrap().path().join("data/foreign");
            if maybe.exists() {
                foreign = maybe;
                break;
            }
        }
    }
    assert_eq!(
        fs::read(foreign).unwrap(),
        b"retain replacement",
        "{report:?}"
    );
    assert!(moved.exists() && retention.exists());
}

#[test]
fn malformed_receipt_errors_keep_the_json_report_bounded() {
    let (env, _, retention) = settled();
    let mut response: serde_json::Value =
        serde_json::from_slice(&fs::read(retention.join("response.json")).unwrap()).unwrap();
    response["receipt"]["outcome"] = serde_json::Value::String("x".repeat(8192));
    fs::write(
        retention.join("response.json"),
        serde_json::to_vec(&response).unwrap(),
    )
    .unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(serde_json::to_vec(&report).unwrap().len() < 4096);
    assert!(retention.exists());
}

// ---- Review findings: kept evidence, per-task decisions and bounded descriptors ----

use agentos_core::ids::TaskId;

/// A second approved, started task in `env`'s home.
fn second_task(env: &Env) -> TaskId {
    let (contract, digest) = common::contract_with(2000, common::ALL_CAPS);
    let task = env.db.create_task(&contract, &digest).unwrap();
    env.db.approve_task(&task).unwrap();
    env.db.append(&task, &TaskEvent::Started).unwrap();
    task
}

/// Settles one more `ReadSnapshot` effect of `task`, leaving a job directory with a
/// published receipt for `output`.
fn settle_job(env: &Env, task: &TaskId, output: &[u8]) -> JobDir {
    let base = env.db.task(task).unwrap().workspace_digest;
    let rec = steps::intend(
        &env.db,
        task,
        EffectKind::ReadSnapshot,
        Digest::of(output),
        &base,
        &Resource::Task,
    )
    .unwrap();
    let ctx = steps::dispatch(&env.db, &rec).unwrap();
    let req = steps::request(&rec, Vec::new(), &env.contract, 0);
    let out = ExecOutcome::success(&req, &ctx, output.to_vec());
    let request = JobRequest {
        effect_id: rec.effect_id.clone(),
        task_id: task.clone(),
        kind: rec.kind.clone(),
        payload: Vec::new(),
        contract: env.contract.clone(),
        attempt_id: ctx.attempt_id.clone(),
        lease_generation: ctx.lease_generation,
        lease_expiry_ms: 0,
        task_deadline_ms: 0,
        worker: WorkerConfig::Host(common::host_config(env.dir.path())),
    };
    let (job, lock) = JobDir::create(&env.dir.path().join("jobs"), &request).unwrap();
    job.write_receipt(&out).unwrap();
    job.write_outcome(&out).unwrap();
    drop(lock);
    let digest = steps::store_result(&env.blobs, &out).unwrap();
    steps::register_result(&env.db, &rec, &out, &digest).unwrap();
    env.db
        .complete_effect(&rec.effect_id, &out.receipt, Some(&digest), None)
        .unwrap();
    job
}

fn finish(env: &Env, task: &TaskId) {
    env.db
        .append(
            task,
            &TaskEvent::Failed {
                reason: "finished fixture".into(),
            },
        )
        .unwrap();
}

fn status_of<'a>(report: &'a agentos_engine::gc::Report, path: &str) -> Vec<&'a str> {
    report
        .entries
        .iter()
        .filter(|e| e.path == path)
        .map(|e| e.status.as_str())
        .collect()
}

fn rel(env: &Env, path: &std::path::Path) -> String {
    path.strip_prefix(env.dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

const LOGS: [&str; 6] = [
    "console.log",
    "stderr.log",
    "firecracker.log",
    "supervisor.log",
    "status.json",
    "groups",
];

#[test]
fn collection_keeps_job_logs_status_and_receipt_and_removes_only_redundant_copies() {
    let (env, job, retention) = settled();
    for name in LOGS {
        fs::write(job.path.join(name), format!("evidence {name}")).unwrap();
    }
    fs::write(job.path.join("scratch.img"), b"scratch drive").unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(job.path.is_dir(), "{report:?}");
    for name in LOGS {
        assert_eq!(
            fs::read(job.path.join(name)).unwrap(),
            format!("evidence {name}").as_bytes(),
            "{name} must be kept: {report:?}"
        );
    }
    for name in [
        "receipt.json",
        "request.json",
        "outcome.json",
        "outcome.bin",
        "lock",
    ] {
        assert!(job.path.join(name).exists(), "{name} must be kept");
    }
    for name in ["output.bin", "scratch.img"] {
        assert!(!job.path.join(name).exists(), "{name} is redundant");
    }
    assert!(!retention.exists());
    let again = gc(env.dir.path(), &env, false);
    assert_eq!(
        status_of(&again, &rel(&env, &job.path)),
        ["collected"],
        "{again:?}"
    );
    assert!(
        !again
            .entries
            .iter()
            .any(|e| e.status == "candidate" || e.status == "refused"),
        "{again:?}"
    );
}

#[test]
fn a_nonterminal_task_does_not_block_a_terminal_one() {
    let (env, job, retention) = settled();
    let other = second_task(&env);
    let other_ws = env.dir.path().join("work").join(other.as_str()).join("ws");
    fs::create_dir_all(&other_ws).unwrap();
    fs::write(other_ws.join("file"), b"live work").unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(!job.path.join("output.bin").exists(), "{report:?}");
    assert!(!retention.exists(), "{report:?}");
    assert_eq!(fs::read(other_ws.join("file")).unwrap(), b"live work");
    assert_eq!(
        status_of(&report, &rel(&env, &other_ws)),
        ["retained"],
        "{report:?}"
    );
}

#[test]
fn a_receipt_less_older_attempt_is_kept_without_blocking_its_task() {
    let (env, job, retention) = settled();
    let mut req = job.request().unwrap();
    req.attempt_id = agentos_core::effect::AttemptId::new();
    req.lease_generation = 0;
    let (old, lock) = JobDir::create(&env.dir.path().join("jobs"), &req).unwrap();
    drop(lock);
    fs::write(old.path.join("scratch.img"), b"killed job scratch").unwrap();
    fs::write(old.path.join("console.log"), b"what the killed job said").unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(!job.path.join("output.bin").exists(), "{report:?}");
    assert!(!retention.exists(), "{report:?}");
    assert_eq!(
        fs::read(old.path.join("scratch.img")).unwrap(),
        b"killed job scratch",
        "a killed job's scratch.img stays while it has no receipt"
    );
    assert!(old.path.join("console.log").exists());
    assert_eq!(
        status_of(&report, &rel(&env, &old.path)),
        ["retained"],
        "{report:?}"
    );
}

#[test]
fn an_oversized_host_workspace_retains_only_its_own_task() {
    let (env, job, retention) = settled();
    let other = second_task(&env);
    finish(&env, &other);
    let big = env.dir.path().join("work").join(other.as_str()).join("ws");
    fs::create_dir_all(&big).unwrap();
    for n in 0..10001 {
        fs::write(big.join(format!("f{n}")), b"").unwrap();
    }
    let report = gc(env.dir.path(), &env, false);
    assert!(!job.path.join("output.bin").exists(), "{report:?}");
    assert!(!retention.exists());
    assert!(big.join("f10000").exists());
    assert_eq!(
        status_of(&report, &rel(&env, &big)),
        ["retained"],
        "{report:?}"
    );
}

#[test]
fn a_home_reached_through_a_symlink_is_collected() {
    let (env, job, retention) = settled();
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("home");
    std::os::unix::fs::symlink(env.dir.path(), &link).unwrap();
    let report = gc(&link, &env, false);
    assert!(!job.path.join("output.bin").exists(), "{report:?}");
    assert!(!retention.exists(), "{report:?}");
}

#[test]
fn an_output_larger_than_the_json_limit_is_collected() {
    let (env, _, _) = settled();
    let other = second_task(&env);
    let big = settle_job(&env, &other, &vec![7u8; 33 * 1024 * 1024]);
    finish(&env, &other);
    let report = gc(env.dir.path(), &env, false);
    assert!(!big.path.join("output.bin").exists(), "{report:?}");
    assert!(big.path.join("receipt.json").exists());
}

const FD_CHILD: &str = "AGENTOS_GC_TEST_FD_CHILD";

/// Hundreds of settled job directories in one task, plus other tasks, collected in a
/// process whose descriptor limit is far below the number of entries.
/// Re-executes this test binary for exactly `test` under `ulimit -n 256` and asserts the
/// child printed `marker`.
fn run_in_low_descriptor_child(test: &str, marker: &str) {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -n 256 && exec \"$0\" \"$@\"")
        .arg(exe)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(FD_CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains(marker));
}

fn assert_descriptor_limit_256() {
    let limit = std::fs::read_to_string("/proc/self/limits").unwrap();
    let line = limit
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap()
        .to_string();
    assert!(line.split_whitespace().nth(3) == Some("256"), "{line}");
}

/// Hundreds of settled job directories in one task, plus other tasks, collected in a
/// process whose descriptor limit is far below the number of entries.
#[test]
fn many_settled_jobs_are_collected_under_a_low_descriptor_limit() {
    if std::env::var_os(FD_CHILD).is_some() {
        return low_descriptor_child();
    }
    run_in_low_descriptor_child(
        "many_settled_jobs_are_collected_under_a_low_descriptor_limit",
        "low-descriptor child collected",
    );
}

/// Hundreds of terminal tasks, each with a nested workspace, at the largest allowed batch
/// size: the held workspace locks per batch stay bounded whatever the flag says.
#[test]
fn many_tasks_are_collected_at_the_largest_batch_under_a_low_descriptor_limit() {
    if std::env::var_os(FD_CHILD).is_some() {
        return many_tasks_child();
    }
    run_in_low_descriptor_child(
        "many_tasks_are_collected_at_the_largest_batch_under_a_low_descriptor_limit",
        "many-tasks child collected",
    );
}

fn many_tasks_child() {
    assert_descriptor_limit_256();
    let (env, _, _) = settled();
    let mut workspaces = Vec::new();
    for t in 0..300 {
        let task = second_task(&env);
        finish(&env, &task);
        let ws = env.dir.path().join("work").join(task.as_str()).join("ws");
        let mut deep = ws.clone();
        for level in 0..20 {
            deep = deep.join(format!("d{level}"));
        }
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("file"), format!("task {t}")).unwrap();
        workspaces.push(ws);
    }
    let report = gc_with(
        env.dir.path(),
        &env,
        Options {
            dry_run: false,
            batch_size: 4096,
        },
        &|_, _| Ok(()),
    );
    let bad: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.status != "deleted" && e.status != "collected")
        .take(3)
        .collect();
    assert!(
        bad.is_empty() && !report.failed(),
        "{:?} {bad:?}",
        report.summary
    );
    for ws in &workspaces {
        assert!(!ws.exists(), "{}", ws.display());
    }
    assert_eq!(
        fs::read_dir(env.dir.path().join("gc-trash"))
            .unwrap()
            .count(),
        0
    );
    println!("many-tasks child collected {} workspaces", workspaces.len());
}

fn low_descriptor_child() {
    assert_descriptor_limit_256();
    let (env, job, retention) = settled();
    let other = second_task(&env);
    let jobs: Vec<JobDir> = (0..1100)
        .map(|n| settle_job(&env, &other, format!("output {n}").as_bytes()))
        .collect();
    finish(&env, &other);
    let report = gc(env.dir.path(), &env, false);
    let refused: Vec<_> = report
        .entries
        .iter()
        .filter(|e| e.status == "refused")
        .take(3)
        .collect();
    assert!(refused.is_empty(), "{refused:?}");
    for j in jobs.iter().chain([&job]) {
        assert!(!j.path.join("output.bin").exists(), "{}", j.path.display());
        assert!(j.path.join("receipt.json").exists());
    }
    assert!(!retention.exists());
    println!("low-descriptor child collected {} jobs", jobs.len() + 1);
}

// ---- Hostile cases: forged tickets, redirected paths, unsettled effects, crash points ----

fn ticket_key(relative: &str) -> String {
    Digest::of(relative.as_bytes()).to_string()
}

fn ws_ticket(relative: &str, task: &TaskId, kind: &str, version: u64) -> serde_json::Value {
    serde_json::json!({
        "version": version,
        "relative": relative,
        "kind": kind,
        "proof": {
            "task": task, "effect": null, "attempt": null, "result": null,
            "lease": 0, "firecracker_socket": false
        },
        "device": 0,
        "inode": 0
    })
}

/// Everything `settled()` made is still there and `secret` is untouched.
fn nothing_deleted(env: &Env, job: &JobDir, retention: &Path, secret: &Path, report: &Report) {
    assert!(report.failed(), "{report:?}");
    assert!(
        !report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(job.path.join("output.bin").exists(), "{report:?}");
    assert!(retention.exists());
    let work = env.dir.path().join("work").join(env.task.as_str());
    assert!(work.join("ws.img").exists());
    assert_eq!(fs::read(secret).unwrap(), b"keep", "{report:?}");
}

#[test]
fn forged_or_foreign_deletion_tickets_never_delete_anything() {
    let cases = [
        "traversal",
        "absolute",
        "dotdot-inside",
        "kind-mismatch",
        "key-mismatch",
        "data-symlink",
        "unowned-dir",
        "version-1",
        "garbage",
        "foreign-tmp",
    ];
    for case in cases {
        let (env, job, retention) = settled();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, b"keep").unwrap();
        let outside_name = outside.path().file_name().unwrap().to_str().unwrap();
        let trash = env.dir.path().join("gc-trash");
        fs::create_dir_all(&trash).unwrap();
        let ws = format!("work/{}/ws", env.task);
        let forge = |relative: &str, key: &str, kind: &str, version: u64| {
            fs::write(
                trash.join(format!("{key}.json")),
                serde_json::to_vec(&ws_ticket(relative, &env.task, kind, version)).unwrap(),
            )
            .unwrap();
        };
        match case {
            "traversal" => {
                let rel = format!("../{outside_name}/secret");
                forge(&rel, &ticket_key(&rel), "Workspace", 2)
            }
            "absolute" => {
                let rel = secret.to_str().unwrap().to_string();
                forge(&rel, &ticket_key(&rel), "Workspace", 2)
            }
            "dotdot-inside" => {
                let rel = format!("work/../../{outside_name}/secret");
                forge(&rel, &ticket_key(&rel), "Workspace", 2)
            }
            "kind-mismatch" => forge(&ws, &ticket_key(&ws), "Model", 2),
            "key-mismatch" => forge(&ws, &ticket_key("work/other/ws"), "Workspace", 2),
            "data-symlink" => {
                forge(&ws, &ticket_key(&ws), "Workspace", 2);
                fs::create_dir(trash.join(ticket_key(&ws))).unwrap();
                std::os::unix::fs::symlink(
                    outside.path(),
                    trash.join(ticket_key(&ws)).join("data"),
                )
                .unwrap();
            }
            "unowned-dir" => fs::create_dir(trash.join(ticket_key(&ws))).unwrap(),
            "version-1" => forge(&ws, &ticket_key(&ws), "Workspace", 1),
            "garbage" => fs::write(trash.join(format!("{}.json", ticket_key(&ws))), b"{").unwrap(),
            _ => fs::write(trash.join(".tmpABCDEF"), b"not ours").unwrap(),
        }
        let before: Vec<_> = fs::read_dir(&trash)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let report = gc(env.dir.path(), &env, false);
        nothing_deleted(&env, &job, &retention, &secret, &report);
        assert_eq!(report.entries[0].status, "refused", "{case}: {report:?}");
        let after: Vec<_> = fs::read_dir(&trash)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after, "{case}: unknown entries are never removed");
        assert!(outside.path().join("secret").exists());
    }
}

#[test]
fn symlinked_workspaces_model_dirs_and_task_dirs_stop_the_pass() {
    for case in ["ws", "model", "task-dir"] {
        let (env, job, retention) = settled();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        fs::write(&secret, b"keep").unwrap();
        let root = env.dir.path();
        match case {
            "ws" => {
                let ws = root.join("work").join(env.task.as_str()).join("ws");
                fs::remove_dir_all(&ws).unwrap();
                std::os::unix::fs::symlink(outside.path(), &ws).unwrap();
            }
            "model" => {
                let name = format!("{}-{}", "a".repeat(64), uuid_like());
                std::os::unix::fs::symlink(outside.path(), root.join("model").join(name)).unwrap();
            }
            _ => {
                let other = second_task(&env);
                finish(&env, &other);
                std::os::unix::fs::symlink(outside.path(), root.join("work").join(other.as_str()))
                    .unwrap();
                fs::create_dir(outside.path().join("ws")).unwrap();
            }
        }
        let report = gc(root, &env, false);
        nothing_deleted(&env, &job, &retention, &secret, &report);
        assert_eq!(report.entries[0].status, "refused", "{case}: {report:?}");
    }
}

fn uuid_like() -> String {
    agentos_core::effect::AttemptId::new().to_string()
}

#[test]
fn a_superseded_attempt_with_its_own_receipt_is_kept() {
    let (env, job, retention) = settled();
    let mut req = job.request().unwrap();
    req.attempt_id = agentos_core::effect::AttemptId::new();
    req.lease_generation = 0;
    let (old, lock) = JobDir::create(&env.dir.path().join("jobs"), &req).unwrap();
    drop(lock);
    let mut out: ExecOutcome =
        serde_json::from_slice(&fs::read(job.path.join("receipt.json")).unwrap()).unwrap();
    out.output = fs::read(job.path.join("output.bin")).unwrap();
    out.receipt.attempt_id = req.attempt_id.clone();
    out.receipt.lease_generation = 0;
    old.write_receipt(&out).unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(!report.failed(), "{report:?}");
    assert!(old.path.join("output.bin").exists(), "{report:?}");
    assert_eq!(status_of(&report, &rel(&env, &old.path)), ["retained"]);
    assert!(!job.path.join("output.bin").exists() && !retention.exists());
}

#[test]
fn a_dispatched_or_unknown_model_call_keeps_its_retained_response() {
    for unknown in [false, true] {
        let env = Env::with_model(3, 10);
        env.db.append(&env.task, &TaskEvent::Started).unwrap();
        let base = env.db.task(&env.task).unwrap().workspace_digest;
        let model = steps::intend(
            &env.db,
            &env.task,
            EffectKind::ModelCall {
                model: "fake".into(),
                turn: 1,
            },
            Digest::of(b"{}"),
            &base,
            &Resource::Task,
        )
        .unwrap();
        let ctx = steps::dispatch(&env.db, &model).unwrap();
        let out = ExecOutcome::success(
            &steps::request(&model, Vec::new(), &env.contract, 0),
            &ctx,
            b"model response".to_vec(),
        );
        let retention = env
            .dir
            .path()
            .join("model")
            .join(format!("{}-{}", model.effect_id, ctx.attempt_id));
        fs::create_dir_all(&retention).unwrap();
        fs::write(
            retention.join("response.json"),
            serde_json::to_vec(&out).unwrap(),
        )
        .unwrap();
        if unknown {
            env.db.mark_unknown(&model.effect_id).unwrap();
        }
        finish(&env, &env.task);
        let report = gc(env.dir.path(), &env, false);
        assert!(retention.join("response.json").exists(), "{report:?}");
        assert_eq!(
            status_of(&report, &rel(&env, &retention)),
            ["retained"],
            "{report:?}"
        );
        assert!(report.entries[0].reason.contains("unsettled"), "{report:?}");
    }
}

/// Crash points of a deletion: after the ticket is durable (before staging), after the
/// move (before removal), after removal (before the ticket goes). A rerun finishes and
/// nothing outside the home changes.
#[test]
fn every_crash_point_of_a_deletion_is_finished_by_a_rerun() {
    for at in [
        CollectionStage::TicketPublished,
        CollectionStage::Staged,
        CollectionStage::Removed,
    ] {
        for target in ["jobs", "model", "work"] {
            let (env, job, retention) = settled();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), b"keep").unwrap();
            let root = env.dir.path();
            let before = env.events();
            let report = gc_hook(root, &env, false, &|stage, path| {
                if stage == at && path.starts_with(target) {
                    return Err(std::io::Error::other("injected crash"));
                }
                Ok(())
            });
            assert!(report.failed(), "{at:?} {target}: {report:?}");
            let retry = gc(root, &env, false);
            assert!(!retry.failed(), "{at:?} {target}: {retry:?}");
            assert!(!job.path.join("output.bin").exists(), "{retry:?}");
            assert!(job.path.join("receipt.json").exists());
            assert!(!retention.exists(), "{at:?} {target}: {retry:?}");
            let work = root.join("work").join(env.task.as_str());
            assert!(!work.join("ws").exists() && !work.join("ws.img").exists());
            assert_eq!(
                fs::read_dir(root.join("gc-trash")).unwrap().count(),
                0,
                "{at:?} {target}: {retry:?}"
            );
            assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"keep");
            assert_eq!(env.events(), before);
            let third = gc(root, &env, false);
            assert!(
                third.entries.iter().all(|e| e.status == "collected"),
                "{third:?}"
            );
        }
    }
}

#[test]
fn an_entry_swapped_before_staging_is_moved_back_and_kept() {
    let (env, _, _) = settled();
    let work = env.dir.path().join("work").join(env.task.as_str());
    let report = gc_hook(env.dir.path(), &env, false, &|stage, path| {
        if stage == CollectionStage::TicketPublished && path.ends_with("ws") {
            fs::rename(work.join("ws"), work.join("ws-validated"))?;
            fs::create_dir(work.join("ws"))?;
            fs::write(work.join("ws/foreign"), b"not validated")?;
        }
        Ok(())
    });
    assert!(report.failed(), "{report:?}");
    assert_eq!(fs::read(work.join("ws/foreign")).unwrap(), b"not validated");
    assert!(work.join("ws-validated/file").exists());
    assert_eq!(
        fs::read_dir(env.dir.path().join("gc-trash"))
            .unwrap()
            .count(),
        0,
        "the unexecuted ticket is dropped, so later passes are not wedged"
    );
}

#[test]
fn unpublished_ticket_temp_files_are_cleaned() {
    let (env, _, _) = settled();
    let trash = env.dir.path().join("gc-trash");
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join(".tmp-ticket-a1B2c3"), b"{\"half\":").unwrap();
    let dry = gc(env.dir.path(), &env, true);
    assert!(trash.join(".tmp-ticket-a1B2c3").exists(), "{dry:?}");
    let report = gc(env.dir.path(), &env, false);
    assert!(!report.failed(), "{report:?}");
    assert_eq!(
        status_of(&report, "gc-trash/.tmp-ticket-a1B2c3"),
        ["deleted"],
        "{report:?}"
    );
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
}

#[test]
fn small_batches_collect_every_task_whole() {
    let (env, job, retention) = settled();
    let others: Vec<(TaskId, Vec<JobDir>)> = (0..3)
        .map(|t| {
            let task = second_task(&env);
            let jobs = (0..4)
                .map(|n| settle_job(&env, &task, format!("task {t} job {n}").as_bytes()))
                .collect();
            finish(&env, &task);
            (task, jobs)
        })
        .collect();
    let report = gc_with(
        env.dir.path(),
        &env,
        Options {
            dry_run: false,
            batch_size: 1,
        },
        &|_, _| Ok(()),
    );
    assert!(!report.failed(), "{report:?}");
    assert_eq!(report.batches, 4, "{report:?}");
    for (_, jobs) in &others {
        for j in jobs {
            assert!(!j.path.join("output.bin").exists());
        }
    }
    assert!(!job.path.join("output.bin").exists() && !retention.exists());
}

#[test]
fn reasons_name_relative_paths_and_never_quote_file_content() {
    let (env, job, retention) = settled();
    std::os::unix::fs::symlink("/etc/passwd", job.path.join("redirect")).unwrap();
    fs::write(
        retention.join("response.json"),
        b"{\"receipt\": \"SECRET-CONTENT\"",
    )
    .unwrap();
    let other = second_task(&env);
    let other_job = settle_job(&env, &other, b"other");
    finish(&env, &other);
    fs::write(other_job.path.join("receipt.json"), b"[\"SECRET-CONTENT\"]").unwrap();
    let report = gc(env.dir.path(), &env, false);
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("/proc/self/fd"), "{text}");
    assert!(!text.contains("SECRET-CONTENT"), "{text}");
    assert!(text.contains("symlink in the tree: redirect"), "{text}");
}

// ---- Mount gate: real mount roots inside candidates (`sh scripts/check.sh mount`) ----

const MOUNT_GATE: &str = "AGENTOS_GC_MOUNT_TESTS";

/// Whether this run is the mount gate (the `test-mount` Compose service, CAP_SYS_ADMIN).
/// Elsewhere the mount regressions are skipped, loudly.
fn mount_gate() -> bool {
    match std::env::var(MOUNT_GATE).as_deref() {
        Ok("1") => true,
        Err(_) | Ok("") => {
            eprintln!("skipped: needs {MOUNT_GATE}=1 (sh scripts/check.sh mount)");
            false
        }
        Ok(other) => panic!("{MOUNT_GATE}={other:?}: only 1 is known"),
    }
}

/// A writable tmpfs mounted at a path; unmounted when dropped. In the gate a failed mount
/// fails the test: it never silently passes.
struct Mounted(PathBuf);
impl Mounted {
    fn tmpfs(at: &Path) -> Mounted {
        fs::create_dir_all(at).unwrap();
        let status = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=1m", "agentos-gc-test"])
            .arg(at)
            .status()
            .expect("the mount gate needs mount(8)");
        assert!(
            status.success(),
            "the mount gate could not mount tmpfs at {}",
            at.display()
        );
        fs::write(at.join("foreign"), b"foreign data must remain").unwrap();
        Mounted(at.to_path_buf())
    }
}
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = std::process::Command::new("umount")
            .arg("-l")
            .arg(&self.0)
            .status();
    }
}

#[test]
fn mount_gate_a_mount_root_inside_a_workspace_stops_the_pass() {
    if !mount_gate() {
        return;
    }
    let (env, job, retention) = settled();
    let ws = env
        .dir
        .path()
        .join("work")
        .join(env.task.as_str())
        .join("ws");
    let mounted = Mounted::tmpfs(&ws.join("mounted"));
    let report = gc(env.dir.path(), &env, false);
    assert!(report.failed(), "{report:?}");
    assert!(
        !report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(
        report.entries[0].reason.contains("mount boundary"),
        "{report:?}"
    );
    assert_eq!(
        fs::read(mounted.0.join("foreign")).unwrap(),
        b"foreign data must remain"
    );
    assert!(job.path.join("output.bin").exists() && retention.exists());
}

/// A mount that appears after validation and staging is caught by the recursive remover
/// itself, not only by validation.
#[test]
fn mount_gate_the_remover_refuses_a_mount_that_appears_after_staging() {
    if !mount_gate() {
        return;
    }
    let (env, _, _) = settled();
    let root = env.dir.path();
    let mounted: std::cell::RefCell<Option<Mounted>> = std::cell::RefCell::new(None);
    let report = gc_hook(root, &env, false, &|stage, path| {
        if stage == CollectionStage::Staged && path.ends_with("ws") {
            let key = Digest::of(path.as_os_str().as_encoded_bytes()).to_string();
            let late = root.join("gc-trash").join(key).join("data").join("late");
            *mounted.borrow_mut() = Some(Mounted::tmpfs(&late));
        }
        Ok(())
    });
    let at = mounted
        .borrow()
        .as_ref()
        .map(|m| m.0.clone())
        .expect("staged");
    assert!(report.failed(), "{report:?}");
    assert!(
        report.entries[0].reason.contains("mount boundary"),
        "{report:?}"
    );
    assert_eq!(
        fs::read(at.join("foreign")).unwrap(),
        b"foreign data must remain"
    );
    drop(mounted.borrow_mut().take());
    // Without the mount the staged deletion is proven and finished.
    let retry = gc(root, &env, false);
    assert!(!retry.failed(), "{retry:?}");
    assert_eq!(fs::read_dir(root.join("gc-trash")).unwrap().count(), 0);
}

/// A staging rename that fails after the ticket is durable (here EXDEV: `gc-trash` is
/// another filesystem) drops the unexecuted ticket, so later passes are not wedged.
#[test]
fn mount_gate_a_cross_device_staging_failure_never_wedges_later_passes() {
    if !mount_gate() {
        return;
    }
    let (env, job, retention) = settled();
    let root = env.dir.path();
    let trash = Mounted::tmpfs(&root.join("gc-trash"));
    fs::remove_file(trash.0.join("foreign")).unwrap();
    for pass in 0..2 {
        let report = gc(root, &env, false);
        assert!(report.failed(), "pass {pass}: {report:?}");
        assert!(
            report.entries[0].reason.contains("cannot stage"),
            "{report:?}"
        );
        assert!(job.path.join("output.bin").exists() && retention.exists());
        assert_eq!(fs::read_dir(&trash.0).unwrap().count(), 0, "{report:?}");
    }
    drop(trash);
    let report = gc(root, &env, false);
    assert!(!report.failed(), "{report:?}");
    assert!(!job.path.join("output.bin").exists() && !retention.exists());
}

// ---- Round 2: liveness, cross-pass identity, transient errors, mutation killers ----

#[test]
fn a_failed_task_with_a_pending_cancel_is_collected_but_a_running_one_is_not() {
    let (env, job, retention) = settled_with(|env| {
        env.db
            .append(&env.task, &TaskEvent::CancelRequested)
            .unwrap();
    });
    assert!(env.db.task(&env.task).unwrap().cancel_requested);
    let report = gc(env.dir.path(), &env, false);
    assert!(!report.failed(), "{report:?}");
    assert!(
        !job.path.join("output.bin").exists() && !retention.exists(),
        "{report:?}"
    );

    let other = second_task(&env);
    env.db.append(&other, &TaskEvent::CancelRequested).unwrap();
    let ws = env.dir.path().join("work").join(other.as_str()).join("ws");
    fs::create_dir_all(&ws).unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(ws.exists(), "{report:?}");
    assert_eq!(status_of(&report, &rel(&env, &ws)), ["retained"]);
}

/// Stages the workspace of `settled()` and stops (as a crash would) before removal.
fn staged_workspace(env: &Env) -> (PathBuf, PathBuf) {
    let relative = format!("work/{}/ws", env.task);
    let report = gc_hook(env.dir.path(), env, false, &|stage, path| {
        if stage == CollectionStage::Staged && path.ends_with("ws") {
            return Err(std::io::Error::other("injected crash"));
        }
        Ok(())
    });
    assert!(report.failed(), "{report:?}");
    let key = ticket_key(&relative);
    let trash = env.dir.path().join("gc-trash");
    assert!(trash.join(&key).join("data/file").exists());
    (
        trash.join(format!("{key}.json")),
        trash.join(key).join("data"),
    )
}

fn edit_ticket(ticket: &Path, field: &str, delta: u64) {
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(ticket).unwrap()).unwrap();
    let n = value[field].as_u64().unwrap();
    value[field] = serde_json::json!(n.wrapping_add(delta));
    fs::write(ticket, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn a_staged_entry_is_finished_even_if_its_device_number_changed_across_passes() {
    let (env, _, _) = settled();
    let (ticket, data) = staged_workspace(&env);
    edit_ticket(&ticket, "device", 1);
    let report = gc(env.dir.path(), &env, false);
    assert!(!report.failed(), "{report:?}");
    assert!(!data.exists());
    assert_eq!(
        fs::read_dir(env.dir.path().join("gc-trash"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn staged_data_with_another_inode_is_refused_while_classifying_and_left_untouched() {
    let (env, job, _) = settled();
    let (ticket, data) = staged_workspace(&env);
    edit_ticket(&ticket, "inode", 1);
    let dry = gc(env.dir.path(), &env, true);
    assert!(dry.failed(), "{dry:?}");
    assert_eq!(dry.entries[0].status, "refused", "{dry:?}");
    assert!(dry.entries[0].reason.contains("not the entry"), "{dry:?}");
    let report = gc(env.dir.path(), &env, false);
    assert!(report.failed(), "{report:?}");
    assert!(
        !report.entries.iter().any(|e| e.status == "deleted"),
        "an integrity problem found while classifying deletes nothing: {report:?}"
    );
    assert!(data.join("file").exists() && ticket.exists());
    let _ = job;
}

#[test]
fn a_dry_run_creates_no_lock_files() {
    let (env, _, _) = settled();
    let lock = env
        .dir
        .path()
        .join("work")
        .join(env.task.as_str())
        .join("ws.lock");
    assert!(!lock.exists());
    let report = gc(env.dir.path(), &env, true);
    assert!(!report.failed(), "{report:?}");
    assert!(!lock.exists(), "{report:?}");
}

#[test]
fn a_model_response_of_a_superseded_attempt_is_kept() {
    let (env, _, retention) = settled();
    let mut out: ExecOutcome =
        serde_json::from_slice(&fs::read(retention.join("response.json")).unwrap()).unwrap();
    out.receipt.attempt_id = agentos_core::effect::AttemptId::new();
    out.receipt.lease_generation = out.receipt.lease_generation.wrapping_sub(1);
    let old = env.dir.path().join("model").join(format!(
        "{}-{}",
        out.receipt.effect_id, out.receipt.attempt_id
    ));
    fs::create_dir_all(&old).unwrap();
    fs::write(old.join("response.json"), serde_json::to_vec(&out).unwrap()).unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(old.join("response.json").exists(), "{report:?}");
    assert_eq!(
        status_of(&report, &rel(&env, &old)),
        ["retained"],
        "{report:?}"
    );
    assert!(!retention.exists());
}

#[test]
fn a_job_lock_taken_after_validation_keeps_the_job() {
    let (env, job, _) = settled();
    let held: std::cell::RefCell<Option<File>> = std::cell::RefCell::new(None);
    let report = gc_hook(env.dir.path(), &env, false, &|stage, _| {
        if stage == CollectionStage::Validated {
            let f = File::options().write(true).open(job.path.join("lock"))?;
            f.lock()?;
            *held.borrow_mut() = Some(f);
        }
        Ok(())
    });
    assert!(job.path.join("output.bin").exists(), "{report:?}");
    assert!(report.failed(), "{report:?}");
    assert!(
        report.entries[0].reason.contains("the job is live"),
        "{report:?}"
    );
}

/// A deletion interrupted by descriptor exhaustion is a retryable refusal, never an
/// integrity stop, and the next pass (with descriptors) finishes it.
#[test]
fn descriptor_exhaustion_mid_removal_is_retryable_and_never_strands_data() {
    if std::env::var_os(FD_CHILD).is_some() {
        return exhaustion_child();
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -S -n 40 && exec \"$0\" \"$@\"")
        .arg(exe)
        .args([
            "--exact",
            "descriptor_exhaustion_mid_removal_is_retryable_and_never_strands_data",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FD_CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("exhaustion child finished"));
}

fn exhaustion_child() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let (env, _, _) = settled();
    let ws = env
        .dir
        .path()
        .join("work")
        .join(env.task.as_str())
        .join("ws");
    let mut deep = ws.clone();
    for level in 0..60 {
        deep = deep.join(format!("d{level}"));
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("file"), b"deep").unwrap();
    let first = gc(env.dir.path(), &env, false);
    assert!(first.failed(), "{first:?}");
    let text = serde_json::to_string(&first).unwrap();
    no_pass_stop(&first);
    assert!(text.contains("retry"), "{text}");
    let mut limit = getrlimit(Resource::Nofile);
    limit.current = limit.maximum;
    setrlimit(Resource::Nofile, limit).unwrap();
    let second = gc(env.dir.path(), &env, false);
    assert!(!second.failed(), "{second:?}");
    assert!(!ws.exists());
    assert_eq!(
        fs::read_dir(env.dir.path().join("gc-trash"))
            .unwrap()
            .count(),
        0
    );
    println!("exhaustion child finished");
}

// ---- Round 3: shapes staged and recognised agree; put_back under exhaustion; foreign stores ----

/// Every workspace shape the collector would stage is one a later pass recognises: an
/// unusual shape is retained up front instead of wedging every later pass.
#[test]
fn unusual_workspace_shapes_are_retained_and_never_wedge_later_passes() {
    for (name, as_dir) in [("ws", false), ("workspace", false), ("ws.img", true)] {
        let (env, _, _) = settled();
        let work = env.dir.path().join("work").join(env.task.as_str());
        fs::remove_dir_all(work.join("ws")).unwrap();
        fs::remove_file(work.join("ws.img")).unwrap();
        if as_dir {
            fs::create_dir(work.join(name)).unwrap();
            fs::write(work.join(name).join("inner"), b"odd").unwrap();
        } else {
            fs::write(work.join(name), b"odd").unwrap();
        }
        let interrupted = gc_hook(env.dir.path(), &env, false, &|stage, path| {
            if stage == CollectionStage::Staged && path.starts_with("work") {
                return Err(std::io::Error::other("injected crash"));
            }
            Ok(())
        });
        let rerun = gc(env.dir.path(), &env, false);
        assert!(!rerun.failed(), "{name}: {interrupted:?}\n{rerun:?}");
        assert!(work.join(name).exists(), "{name}: {rerun:?}");
        assert_eq!(
            status_of(&rerun, &format!("work/{}/{name}", env.task)),
            ["retained"],
            "{name}: {rerun:?}"
        );
    }
}

/// The pass went on: anything skipped was skipped only because its own task stopped.
fn no_pass_stop(report: &Report) {
    let text = serde_json::to_string(report).unwrap();
    assert!(!text.contains("pass stopped"), "{text}");
    assert!(
        report
            .entries
            .iter()
            .filter(|e| e.status == "skipped")
            .all(|e| e.reason.starts_with("task stopped after a failure")),
        "{text}"
    );
}

fn read_ticket_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

/// A move back that runs out of descriptors (injected at `Restoring`) is a retryable
/// refusal: ticket and data stay, and the next pass moves the data back, never removes it.
#[test]
fn a_move_back_without_descriptors_is_retried_by_the_next_pass() {
    let (env, _, _) = settled();
    let (ticket, data) = staged_workspace(&env);
    // The staged data no longer passes its checks: a later pass must move it back.
    fs::hard_link(data.join("file"), data.join("linked")).unwrap();
    let before = fs::read(&ticket).unwrap();
    let report = gc_hook(env.dir.path(), &env, false, &|stage, _| {
        if stage == CollectionStage::Restoring {
            return Err(std::io::Error::from_raw_os_error(24));
        }
        Ok(())
    });
    let text = serde_json::to_string(&report).unwrap();
    assert!(report.failed(), "{text}");
    no_pass_stop(&report);
    assert!(
        !text.contains("pass stopped") && text.contains("retry"),
        "{text}"
    );
    assert_eq!(
        fs::read(&ticket).unwrap(),
        before,
        "the ticket is unchanged"
    );
    assert!(data.join("file").exists());
    let again = gc(env.dir.path(), &env, false);
    let ws = env
        .dir
        .path()
        .join("work")
        .join(env.task.as_str())
        .join("ws");
    assert!(
        ws.join("file").exists() && ws.join("linked").exists(),
        "{again:?}"
    );
    assert!(!ticket.exists() && !data.exists(), "{again:?}");
}

/// An entry swapped before staging whose move back runs out of descriptors keeps a ticket
/// rewritten to name it with `restore`: the next pass moves it back, never removes it.
#[test]
fn a_swapped_entry_that_cannot_be_moved_back_is_marked_for_restore() {
    let (env, _, _) = settled();
    let work = env.dir.path().join("work").join(env.task.as_str());
    let report = gc_hook(env.dir.path(), &env, false, &|stage, path| {
        if stage == CollectionStage::TicketPublished && path.ends_with("ws") {
            fs::rename(work.join("ws"), work.join("ws-validated"))?;
            fs::create_dir(work.join("ws"))?;
            fs::write(work.join("ws/foreign"), b"not validated")?;
        }
        if stage == CollectionStage::Restoring {
            return Err(std::io::Error::from_raw_os_error(24));
        }
        Ok(())
    });
    let text = serde_json::to_string(&report).unwrap();
    assert!(report.failed() && text.contains("retry"), "{text}");
    let key = ticket_key(&format!("work/{}/ws", env.task));
    let trash = env.dir.path().join("gc-trash");
    let staged = trash.join(&key).join("data");
    let ticket = read_ticket_json(&trash.join(format!("{key}.json")));
    assert_eq!(ticket["restore"], true, "{ticket}");
    assert_eq!(
        ticket["inode"].as_u64().unwrap(),
        std::os::unix::fs::MetadataExt::ino(&fs::symlink_metadata(&staged).unwrap())
    );
    assert!(staged.join("foreign").exists());
    let again = gc(env.dir.path(), &env, false);
    assert_eq!(
        fs::read(work.join("ws/foreign")).unwrap(),
        b"not validated",
        "{again:?}"
    );
    assert!(!trash.join(format!("{key}.json")).exists(), "{again:?}");
    assert!(work.join("ws-validated/file").exists());
}

/// The lock proves one home; a database or blob store of another home is refused.
#[test]
fn a_store_of_another_home_is_refused() {
    for foreign in ["db", "blobs"] {
        let (env, job, retention) = settled();
        let other = Env::new(10);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(env.dir.path().join("driver.lock"))
            .unwrap();
        lock.lock().unwrap();
        let held = HeldDriverLock::verify(env.dir.path(), &lock).unwrap();
        let (db, blobs) = if foreign == "db" {
            (&other.db, &env.blobs)
        } else {
            (&env.db, &other.blobs)
        };
        let report = agentos_engine::gc::collect(&held, db, blobs, Options::new(false)).unwrap();
        assert!(report.failed(), "{foreign}: {report:?}");
        assert!(
            report.entries[0].reason.contains("not this home's"),
            "{report:?}"
        );
        assert!(job.path.join("output.bin").exists() && retention.exists());
    }
}

/// Staged data on another filesystem than the home is not what a ticket staged, even with
/// the ticket's inode number.
#[test]
fn mount_gate_staged_data_on_another_device_is_refused() {
    if !mount_gate() {
        return;
    }
    let (env, job, _) = settled();
    let relative = format!("work/{}/ws", env.task);
    let key = ticket_key(&relative);
    let trash = Mounted::tmpfs(&env.dir.path().join("gc-trash"));
    fs::remove_file(trash.0.join("foreign")).unwrap();
    let data = trash.0.join(&key).join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("elsewhere"), b"must remain").unwrap();
    let mut ticket = ws_ticket(&relative, &env.task, "Workspace", 2);
    ticket["inode"] = serde_json::json!(std::os::unix::fs::MetadataExt::ino(
        &fs::metadata(&data).unwrap()
    ));
    fs::write(
        trash.0.join(format!("{key}.json")),
        serde_json::to_vec(&ticket).unwrap(),
    )
    .unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(report.failed(), "{report:?}");
    assert!(
        report.entries[0].reason.contains("not the entry"),
        "{report:?}"
    );
    assert_eq!(fs::read(data.join("elsewhere")).unwrap(), b"must remain");
    assert!(job.path.join("output.bin").exists());
}

/// Staged data with the ticket's inode but another kind of file than the ticket's name
/// implies (a directory staged as `ws.img`) is not what that ticket staged.
#[test]
fn staged_data_of_another_kind_than_its_ticket_names_is_refused() {
    let (env, _, _) = settled();
    let (ticket, data) = staged_workspace(&env);
    let trash = env.dir.path().join("gc-trash");
    let img = format!("work/{}/ws.img", env.task);
    let key = ticket_key(&img);
    // The real ws.img is collected aside first, so only the forged ticket names it.
    fs::remove_file(env.dir.path().join(&img)).unwrap();
    let mut forged = read_ticket_json(&ticket);
    forged["relative"] = serde_json::json!(img);
    fs::rename(data.parent().unwrap(), trash.join(&key)).unwrap();
    fs::remove_file(&ticket).unwrap();
    fs::write(
        trash.join(format!("{key}.json")),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    let report = gc(env.dir.path(), &env, false);
    assert!(report.failed(), "{report:?}");
    assert!(
        report.entries[0].reason.contains("not the entry"),
        "{report:?}"
    );
    assert!(trash.join(&key).join("data/file").exists());
}
