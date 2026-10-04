//! Collection uses actual journal/effect/blob state and real file locks.
mod common;
use agentos_core::{broker::Resource, effect::EffectKind, ids::Digest, state::TaskEvent};
use agentos_engine::{
    executor::ExecOutcome,
    gc::collect,
    job::{JobDir, JobRequest, WorkerConfig},
    steps,
};
use common::Env;
use std::fs::{self, File};
use std::path::PathBuf;

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
    let report = collect(env.dir.path(), &env.db, &env.blobs, true).unwrap();
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
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert_eq!(
        report
            .entries
            .iter()
            .filter(|e| e.status == "deleted")
            .count(),
        4,
        "{report:?}"
    );
    assert!(!job.path.exists() && !retention.exists());
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
    assert!(
        collect(env.dir.path(), &env.db, &env.blobs, false)
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(env.events(), before);
    for digest in env.db.referenced_blobs().unwrap() {
        env.blobs.get(&digest).unwrap();
    }
}

#[test]
fn missing_receipt_or_live_job_retains_the_whole_task() {
    for live in [false, true] {
        let (env, job, retention) = settled();
        let held = if live {
            let f = File::options()
                .write(true)
                .open(job.path.join("lock"))
                .unwrap();
            f.lock().unwrap();
            Some(f)
        } else {
            fs::remove_file(job.path.join("receipt.json")).unwrap();
            None
        };
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{report:?}"
        );
        assert!(job.path.exists() && retention.exists());
        drop(held);
    }
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
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
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
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
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
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
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
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
        assert!(
            !report.entries.iter().any(|e| e.status == "deleted"),
            "{report:?}"
        );
        assert!(job.path.exists() && retention.exists());
    }
}

#[test]
fn unknown_paths_and_overfull_reports_are_refused() {
    let (env, job, retention) = settled();
    for n in 0..1001 {
        fs::create_dir(env.dir.path().join("model").join(format!("unknown-{n}"))).unwrap();
    }
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert!(report.entries.len() <= 1000);
    assert!(!report.entries.iter().any(|e| e.status == "deleted"));
    assert!(job.path.exists() && retention.exists());
}

#[test]
fn socket_files_are_retained_outside_the_firecracker_job_socket() {
    for name in ["v.sock", "unrecognized.sock"] {
        let (env, job, retention) = settled();
        let (path, _held_dir) =
            agentos_engine::guestlink::socket_path(&job.path.join(name)).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(path).unwrap();
        drop(socket);
        let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
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
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert_eq!(report.entries[0].status, "refused");
    assert!(work.exists());
}

#[test]
fn an_overlarge_candidate_tree_is_never_partially_removed() {
    let (env, job, retention) = settled();
    for n in 0..10001 {
        fs::write(job.path.join(format!("entry-{n}")), b"").unwrap();
    }
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
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
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert!(
        !report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(job.path.exists() && retention.exists());
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
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert!(
        report.entries.iter().any(|e| e.status == "deleted"),
        "{report:?}"
    );
    assert!(!job.path.exists() && !retention.exists());
}

#[test]
fn ancestor_substitution_cannot_redirect_workspace_deletion() {
    use agentos_engine::gc::{CollectionStage, collect_with_hook};
    let (env, _, _) = settled();
    let outside = tempfile::tempdir().unwrap();
    let external_ws = outside.path().join(env.task.as_str()).join("ws");
    fs::create_dir_all(&external_ws).unwrap();
    fs::write(external_ws.join("secret"), b"must survive").unwrap();
    let root = env.dir.path();
    let report = collect_with_hook(root, &env.db, &env.blobs, false, &|stage, _| {
        if stage == CollectionStage::Validated {
            fs::rename(root.join("work"), root.join("original-work"))?;
            std::os::unix::fs::symlink(outside.path(), root.join("work"))?;
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(
        fs::read(external_ws.join("secret")).unwrap(),
        b"must survive",
        "{report:?}"
    );
}

#[test]
fn interrupted_deletion_retries_after_job_receipt_has_been_removed() {
    use agentos_engine::gc::{CollectionStage, collect_with_hook};
    let (env, _, retention) = settled();
    let root = env.dir.path();
    let report = collect_with_hook(root, &env.db, &env.blobs, false, &|stage, original| {
        if stage == CollectionStage::Staged && original.starts_with("jobs") {
            for entry in fs::read_dir(root.join("gc-trash"))? {
                let data = entry?.path().join("data");
                if data.join("receipt.json").is_file() {
                    fs::remove_file(data.join("receipt.json"))?;
                }
            }
            return Err(std::io::Error::from_raw_os_error(28));
        }
        Ok(())
    })
    .unwrap();
    assert!(
        report.entries.iter().any(|e| e.status == "refused"),
        "{report:?}"
    );
    assert!(retention.exists());
    let retry = collect(root, &env.db, &env.blobs, false).unwrap();
    assert!(
        retry.entries.iter().any(|e| e.status == "deleted"),
        "{retry:?}"
    );
    assert!(!retention.exists());
    assert_eq!(fs::read_dir(root.join("gc-trash")).unwrap().count(), 0);
    assert!(
        collect(root, &env.db, &env.blobs, false)
            .unwrap()
            .entries
            .is_empty()
    );
}

#[test]
fn candidate_replacement_after_validation_is_retained() {
    use agentos_engine::gc::{CollectionStage, collect_with_hook};
    let (env, job, retention) = settled();
    let original = job.path.clone();
    let moved = env.dir.path().join("original-job");
    let report = collect_with_hook(env.dir.path(), &env.db, &env.blobs, false, &|stage, _| {
        if stage == CollectionStage::Validated {
            fs::rename(&original, &moved)?;
            fs::create_dir(&original)?;
            fs::write(original.join("foreign"), b"retain replacement")?;
        }
        Ok(())
    })
    .unwrap();
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
    let report = collect(env.dir.path(), &env.db, &env.blobs, false).unwrap();
    assert!(serde_json::to_vec(&report).unwrap().len() < 4096);
    assert!(retention.exists());
}
