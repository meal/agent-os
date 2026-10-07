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
#[test]
fn many_settled_jobs_are_collected_under_a_low_descriptor_limit() {
    if std::env::var_os(FD_CHILD).is_some() {
        return low_descriptor_child();
    }
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("ulimit -n 256 && exec \"$0\" \"$@\"")
        .arg(exe)
        .args([
            "--exact",
            "many_settled_jobs_are_collected_under_a_low_descriptor_limit",
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("low-descriptor child collected"));
}

fn low_descriptor_child() {
    let limit = std::fs::read_to_string("/proc/self/limits").unwrap();
    let line = limit
        .lines()
        .find(|l| l.starts_with("Max open files"))
        .unwrap()
        .to_string();
    assert!(line.split_whitespace().nth(3) == Some("256"), "{line}");
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
