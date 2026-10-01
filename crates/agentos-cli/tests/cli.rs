//! End-to-end tests of the `agentos` binary. Every command is a separate process; a crash
//! injected with `--crash-at` really kills the process (exit 75), and `resume` is a new
//! process that recovers from what is on disk.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use agentos_cli::crash::point_name;
use agentos_core::ids::Digest;
use agentos_engine::crash::CrashPoint;
use agentos_engine::workspace::{copy_tree, workspace_digest};
use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};
use tempfile::TempDir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").canonicalize().unwrap()
}

fn fix_patch() -> PathBuf {
    fixtures().join("parser-repo.fix.patch")
}

/// Applies, fixes nothing.
const COMMENT_PATCH: &str = "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1,2 +1,3 @@\n+# TODO: handle whitespace\n def parse_kv(text: str) -> dict:\n     \"\"\"Parse 'key = value' lines into a dict, skipping blanks and '#' comments.\"\"\"\n";

const TERMINAL: [&str; 3] = ["SUCCEEDED", "FAILED", "CANCELLED"];

/// A scratch directory holding an agentos home (created on demand by the CLI) and inputs.
struct Cli {
    dir: TempDir,
}

impl Cli {
    fn new() -> Cli {
        Cli { dir: tempfile::tempdir().unwrap() }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentos"));
        cmd.arg("--home").arg(self.home()).arg("--profiles").arg(fixtures().join("profiles")).args(args);
        cmd
    }

    /// Runs a command that must succeed; returns its stdout parsed as one JSON value.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).assert().success().get_output().stdout.clone();
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("stdout of {args:?} is not JSON ({e}): {}", String::from_utf8_lossy(&out)))
    }

    fn write(&self, rel: &str, content: &str) -> String {
        let p = self.path(rel);
        fs::write(&p, content).unwrap();
        p.to_str().unwrap().to_string()
    }

    /// A copy of the fixture repository in the scratch dir.
    fn repo_copy(&self) -> PathBuf {
        let repo = self.path("repo");
        copy_tree(&fixtures().join("parser-repo"), &repo).unwrap();
        repo
    }

    fn contract_with(&self, source: &Path, tool_actions: u32, revision: &str) -> String {
        let contract = json!({
            "goal": "fix the parser",
            "repository": { "source": source, "revision": revision },
            "profile": "python-stdlib-v1",
            "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": {
                "model_requests": 1, "max_output_tokens_per_request": 1000, "tool_actions": tool_actions,
                "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256
            }
        });
        self.write(&format!("task-{}.json", Digest::of(contract.to_string().as_bytes())), &contract.to_string())
    }

    fn contract(&self, source: &Path) -> String {
        self.contract_with(source, 10, "recorded-at-submission")
    }

    fn submit_yes(&self, contract: &str, patch: &Path) -> Value {
        self.json(&["submit", contract, "--yes", "--fake-agent-patch", patch.to_str().unwrap()])
    }

    /// `submit --yes --crash-at spec`: must die with exit 75; returns the task id it reported.
    fn crash(&self, contract: &str, spec: &str) -> String {
        let assert = self
            .cmd(&["submit", contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap(), "--crash-at", spec])
            .assert()
            .code(75);
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        let crashed: Value = stderr
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("crashed").is_some())
            .unwrap_or_else(|| panic!("no crash report on stderr: {stderr}"));
        let point = spec.split(':').next().unwrap();
        assert_eq!(crashed["crashed"], point, "{stderr}");
        crashed["task_id"].as_str().unwrap().to_string()
    }

    fn status(&self, id: &str) -> Value {
        self.json(&["status", id])
    }

    fn events(&self, id: &str) -> Vec<Value> {
        let out = self.cmd(&["events", id]).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn event_types(&self, id: &str) -> Vec<String> {
        self.events(id).iter().map(|e| e["type"].as_str().unwrap().to_string()).collect()
    }

    fn export(&self, id: &str, name: &str) -> (PathBuf, Value) {
        let dir = self.path(name);
        let printed = self.json(&["export", id, dir.to_str().unwrap()]);
        let manifest: Value = serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(printed, manifest, "export prints the manifest it wrote");
        (dir, manifest)
    }
}

fn assert_subsequence(haystack: &[String], needles: &[&str]) {
    let mut it = haystack.iter();
    for n in needles {
        assert!(it.any(|h| h == n), "missing {n:?} in order within {haystack:?}");
    }
}

/// The manifest without what legitimately differs between two runs of the same contract
/// (task and effect ids, journal length).
fn normalized(manifest: &Value) -> Value {
    let mut m = manifest.clone();
    let obj = m.as_object_mut().unwrap();
    obj.remove("task_id");
    obj.remove("generated_events");
    for list in ["patches", "verification_results"] {
        for item in obj[list].as_array_mut().unwrap() {
            item.as_object_mut().unwrap().remove("effect_id");
        }
    }
    m
}

fn digest_of_file(path: &Path) -> String {
    Digest::of(&fs::read(path).unwrap()).to_string()
}

fn run_ok(cmd: &mut StdCommand) {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn invalid_contract_exits_2_with_the_validation_message_and_writes_nothing() {
    let cli = Cli::new();
    let repo = cli.repo_copy();
    let bad = cli.contract_with(&repo, 0, "recorded-at-submission");

    cli.cmd(&["submit", &bad]).assert().code(2).stdout("").stderr(predicate::str::contains("limit tool_actions must be > 0"));
    assert!(!cli.home().exists(), "no home, no database");

    let garbage = cli.write("garbage.json", "{ not json");
    cli.cmd(&["submit", &garbage]).assert().code(2).stderr(predicate::str::contains("invalid contract json"));
    let missing_profile = cli.write("p.json", &fs::read_to_string(cli.contract(&repo)).unwrap().replace("parser-checks-v1", "nope-v1"));
    cli.cmd(&["submit", &missing_profile]).assert().code(2).stderr(predicate::str::contains("verification profile nope-v1"));
    let wrong_rev = cli.contract_with(&repo, 10, &Digest::of(b"another tree").to_string());
    cli.cmd(&["submit", &wrong_rev]).assert().code(2).stderr(predicate::str::contains("revision"));
    let contract = cli.contract(&repo);
    cli.cmd(&["submit", &contract, "--yes"]).assert().code(2).stderr(predicate::str::contains("--fake-agent-patch"));
    assert!(!cli.home().exists(), "nothing written by any rejected submission");
}

#[test]
fn full_flow_submit_status_events_export_and_the_patch_reproduces_the_fix() {
    let cli = Cli::new();
    let repo = cli.repo_copy();
    let contract = cli.contract(&repo);

    let submitted = cli.submit_yes(&contract, &fix_patch());
    assert_eq!(submitted["state"], "SUCCEEDED");
    let id = submitted["task_id"].as_str().unwrap();

    let status = cli.status(id);
    assert_eq!(status["task_id"], id);
    assert_eq!(status["state"], "SUCCEEDED");
    assert_eq!(status["cancel_requested"], false);
    assert_eq!(status["verified_digest"], status["workspace_digest"]);
    assert_eq!(status["actions_used"], 2, "snapshot and patch");
    assert_eq!(status["outstanding_effects"], json!([]));
    assert_eq!(status["usage"]["settled_tool_actions"], 2);

    let events = cli.events(id);
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>(), "gapless");
    let types = cli.event_types(id);
    assert_subsequence(&types, &["TaskCreated", "Submitted", "Started", "WorkspaceUpdated", "WorkspaceUpdated", "VerifyStarted", "VerifyPassed"]);
    let submitted_event = &events[1];
    assert_eq!(submitted_event["type"], "Submitted");
    let repo_digest = workspace_digest(&repo).unwrap().to_string();
    let profile_digest = workspace_digest(&fixtures().join("profiles/parser-checks-v1")).unwrap().to_string();
    assert_eq!(submitted_event["payload"]["repository_digest"], repo_digest);
    assert_eq!(submitted_event["payload"]["profile_id"], "parser-checks-v1");
    assert_eq!(submitted_event["payload"]["profile_digest"], profile_digest);
    assert_eq!(submitted_event["payload"]["guest_image"], "fixture-executor-v0");
    assert_eq!(submitted_event["payload"]["contract_digest"], events[0]["payload"]["contract_digest"]);

    let (bundle, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["task_id"], id);
    assert_eq!(manifest["state"], "SUCCEEDED");
    assert_eq!(manifest["base_revision"], repo_digest);
    assert_eq!(manifest["base_workspace_digest"], repo_digest);
    assert_eq!(manifest["final_workspace_digest"], status["workspace_digest"]);
    assert_eq!(manifest["verified_digest"], status["verified_digest"]);
    assert_eq!(manifest["verification_profile_digest"], profile_digest);
    assert_eq!(manifest["contract_digest"], events[0]["payload"]["contract_digest"]);
    assert_eq!(manifest["usage_summary"], status["usage"]);
    assert_eq!(manifest["model"], "fake-agent");
    assert_eq!(manifest["patch_digest"], digest_of_file(&bundle.join("patch.diff")));
    assert_eq!(fs::read(bundle.join("patch.diff")).unwrap(), fs::read(fix_patch()).unwrap());
    let results = manifest["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["passed"], true);
    assert_eq!(results[0]["workspace_digest"], status["verified_digest"]);
    let evidence = results[0]["evidence_digest"].as_str().unwrap();
    assert_eq!(digest_of_file(&bundle.join(format!("evidence/{evidence}.json"))), evidence);
    // The evidence digest is the one the journal recorded for the verification effect.
    let completed: Vec<&Value> = events.iter().filter(|e| e["type"] == "EffectCompleted").collect();
    assert!(completed.iter().any(|e| e["payload"].to_string().contains(evidence)), "evidence digest is journaled");

    // patch.diff turns a pristine copy of the fixture into the verified workspace.
    let pristine = tempfile::tempdir().unwrap();
    let ws = pristine.path().join("ws");
    copy_tree(&fixtures().join("parser-repo"), &ws).unwrap();
    run_ok(
        StdCommand::new("git")
            .current_dir(&ws)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("GIT_CEILING_DIRECTORIES", pristine.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .arg("apply")
            .arg(bundle.join("patch.diff")),
    );
    assert_eq!(json!(workspace_digest(&ws).unwrap()), manifest["final_workspace_digest"]);
    run_ok(StdCommand::new("python3").current_dir(&ws).env("PYTHONDONTWRITEBYTECODE", "1").args(["-m", "unittest"]));
    run_ok(
        StdCommand::new("python3")
            .current_dir(fixtures().join("profiles/parser-checks-v1"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .arg("check_parser.py")
            .arg(&ws),
    );
}

#[test]
fn export_of_an_unfinished_task_fails_and_leaves_no_directory() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json(&["submit", &contract])["task_id"].as_str().unwrap().to_string();
    let running = cli.crash(&contract, "after-dispatch:apply_patch");
    let paused = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(cli.json(&["pause", &paused])["state"], "PAUSED");

    for (id, state) in [(&ready, "READY"), (&running, "RUNNING"), (&paused, "PAUSED")] {
        assert_eq!(cli.status(id)["state"], state);
        let out = cli.path(&format!("bundle-{state}"));
        cli.cmd(&["export", id, out.to_str().unwrap()])
            .assert()
            .code(1)
            .stdout("")
            .stderr(predicate::str::contains(format!("task is {state}, only finished tasks can be exported")));
        assert!(!out.exists());
    }
    let leftovers: Vec<_> = fs::read_dir(cli.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("export") || n.starts_with("bundle"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn submit_without_yes_waits_for_approval_and_resume_runs_it() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());

    let assert = cli.cmd(&["submit", &contract, "--fake-agent-patch", fix_patch().to_str().unwrap()]).assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    for shown in ["snapshot.read", "workspace.apply_patch", "src/**", "parser-checks-v1", "tool_actions"] {
        assert!(stderr.contains(shown), "permission summary shows {shown}: {stderr}");
    }
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["state"], "READY");
    assert!(out["note"].as_str().unwrap().contains("agentos resume"));
    let id = out["task_id"].as_str().unwrap();
    assert_eq!(cli.status(id)["state"], "READY");
    assert_eq!(cli.event_types(id), vec!["TaskCreated", "Submitted"], "nothing ran before approval");

    // The patch given at submission is used; no flag needed.
    let resumed = cli.json(&["resume", id]);
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }));
    assert_eq!(cli.status(id)["state"], "SUCCEEDED");
    // Resuming a finished task only reports it.
    assert_eq!(cli.json(&["resume", id]), json!({ "task_id": id, "state": "SUCCEEDED" }));
}

#[test]
fn resume_without_any_agent_patch_is_a_usage_error() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.json(&["submit", &contract])["task_id"].as_str().unwrap().to_string();
    cli.cmd(&["resume", &id]).assert().code(2).stderr(predicate::str::contains("--fake-agent-patch"));
    assert_eq!(cli.status(&id)["state"], "READY");
    let done = cli.json(&["resume", &id, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(done["state"], "SUCCEEDED");
}

/// THE DEMO: kill the controller at `spec`, restart it, recover the same task, export, and
/// get exactly the bundle an uncrashed run of the same contract gives.
fn crash_restart_recover_export(spec: &str) {
    let cli = Cli::new();
    // Both runs read the same source, so their contracts (and digests) are identical.
    let contract = cli.contract(&fixtures().join("parser-repo"));
    let clean = cli.submit_yes(&contract, &fix_patch());
    assert_eq!(clean["state"], "SUCCEEDED");
    let (_, expected) = cli.export(clean["task_id"].as_str().unwrap(), "clean");

    let id = cli.crash(&contract, spec);
    let stuck = cli.status(&id);
    assert!(!TERMINAL.contains(&stuck["state"].as_str().unwrap()), "{spec}: crashed task is unfinished: {stuck}");

    let resumed = cli.json(&["resume", &id]);
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }), "{spec}");
    let status = cli.status(&id);
    assert_eq!(status["verified_digest"], status["workspace_digest"]);
    assert_eq!(status["outstanding_effects"], json!([]));
    let (_, manifest) = cli.export(&id, "recovered");
    assert_eq!(manifest["task_id"], id.as_str());
    assert_eq!(normalized(&manifest), normalized(&expected), "{spec}: recovered bundle equals the uncrashed one");
}

/// Crash specs of the demo table; together they cover every crash point.
const DEMO: [&str; 10] = [
    "after-agent-turn-journaled:apply_patch",
    "after-intent:read_snapshot",
    "after-dispatch:apply_patch",
    "during-execute:apply_patch",
    "during-execute:run_verification",
    "after-execute-before-publish:run_verification",
    "after-blob-put:apply_patch",
    "after-register:read_snapshot",
    "after-complete:apply_patch",
    "after-dispatch:run_verification:1",
];

#[test]
fn demo_table_covers_every_crash_point() {
    let covered: BTreeSet<String> = DEMO.iter().map(|s| s.split(':').next().unwrap().to_string()).collect();
    let all: BTreeSet<String> = CrashPoint::ALL.iter().map(|p| point_name(*p)).collect();
    assert_eq!(covered, all);
}

macro_rules! demo {
    ($($name:ident => $i:expr),* $(,)?) => {
        $(#[test] fn $name() { crash_restart_recover_export(DEMO[$i]); })*
    };
}

demo! {
    demo_after_agent_turn_journaled => 0,
    demo_after_intent => 1,
    demo_after_dispatch => 2,
    demo_during_execute_patch => 3,
    demo_during_execute_verification => 4,
    demo_after_execute_before_publish => 5,
    demo_after_blob_put => 6,
    demo_after_register => 7,
    demo_after_complete => 8,
    demo_after_dispatch_nth => 9,
}

#[test]
fn a_crash_spec_that_never_fires_runs_to_completion() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let out = cli.json(&[
        "submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap(), "--crash-at", "after-dispatch:apply_patch:2",
    ]);
    assert_eq!(out["state"], "SUCCEEDED");
    for bad in ["nowhere", "after-dispatch:teleport", "after-dispatch:apply_patch:0", "after-dispatch:1:2"] {
        cli.cmd(&["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap(), "--crash-at", bad])
            .assert()
            .code(2);
    }
}

#[test]
fn pause_then_resume() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json(&["submit", &contract])["task_id"].as_str().unwrap().to_string();
    cli.cmd(&["pause", &ready]).assert().code(1).stderr(predicate::str::contains("READY"));

    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(cli.status(&id)["state"], "RUNNING");
    assert_eq!(cli.json(&["pause", &id]), json!({ "task_id": id, "state": "PAUSED" }));
    assert_eq!(cli.json(&["pause", &id])["state"], "PAUSED", "pausing twice is harmless");
    let status = cli.status(&id);
    assert_eq!(status["state"], "PAUSED");
    assert_eq!(status["outstanding_effects"].as_array().unwrap().len(), 1, "the in-flight patch waits for the resume");

    assert_eq!(cli.json(&["resume", &id])["state"], "SUCCEEDED");
    assert_subsequence(&cli.event_types(&id), &["Started", "Paused", "Resumed", "RecoveryDecision", "VerifyPassed"]);
    assert_eq!(cli.status(&id)["outstanding_effects"], json!([]));
}

#[test]
fn cancel_after_a_crash_reconciles_the_in_flight_effect_and_cancels() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(cli.status(&id)["outstanding_effects"].as_array().unwrap().len(), 1);

    let out = cli.json(&["cancel", &id]);
    assert_eq!(out["task_id"], id.as_str());
    assert_eq!(out["state"], "CANCELLED");

    let status = cli.status(&id);
    assert_eq!(status["state"], "CANCELLED");
    assert_eq!(status["cancel_requested"], true);
    assert_eq!(status["verified_digest"], Value::Null);
    assert_eq!(status["outstanding_effects"], json!([]), "no leaked effects");
    assert_eq!(status["usage"]["uncertain_tool_actions"], 0);
    let events = cli.events(&id);
    let decision = events.iter().find(|e| e["type"] == "RecoveryDecision").expect("the in-flight patch was decided");
    assert_eq!(decision["payload"]["kind"], "apply_patch");
    assert_eq!(decision["payload"]["decision"], "Abandon", "the patch never ran: reconciliation proves it");
    assert_subsequence(&cli.event_types(&id), &["CancelRequested", "RecoveryDecision", "CancelCompleted"]);

    // Cancelling again only reports; a READY task cancels at once.
    assert_eq!(cli.json(&["cancel", &id])["state"], "CANCELLED");
    let ready = cli.json(&["submit", &contract])["task_id"].as_str().unwrap().to_string();
    assert_eq!(cli.json(&["cancel", &ready])["state"], "CANCELLED");
    assert_eq!(cli.event_types(&ready), vec!["TaskCreated", "Submitted", "CancelRequested", "CancelCompleted"]);

    let (_, manifest) = cli.export(&id, "cancelled");
    assert_eq!(manifest["state"], "CANCELLED");
    assert_eq!(manifest["verified_digest"], Value::Null);
    assert_eq!(manifest["patches"], json!([]));
}

#[test]
fn unknown_task_ids_are_reported() {
    let cli = Cli::new();
    let unknown = "01920000-0000-7000-8000-000000000000";
    let out = cli.path("out");
    for args in [
        vec!["status", unknown],
        vec!["events", unknown],
        vec!["pause", unknown],
        vec!["resume", unknown],
        vec!["cancel", unknown],
        vec!["export", unknown, out.to_str().unwrap()],
    ] {
        cli.cmd(&args).assert().code(1).stdout("").stderr(predicate::str::contains(format!("unknown task {unknown}")));
    }
    assert!(!out.exists());
}

#[test]
fn a_failed_task_exports_without_any_success_claim() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let patch = cli.write("comment.patch", COMMENT_PATCH);

    let out = cli.submit_yes(&contract, Path::new(&patch));
    assert_eq!(out["state"], "FAILED");
    let id = out["task_id"].as_str().unwrap();

    let (bundle, manifest) = cli.export(id, "failed");
    assert_eq!(manifest["state"], "FAILED");
    assert_eq!(manifest["verified_digest"], Value::Null);
    let results = manifest["verification_results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| r["passed"] == false), "{results:?}");
    assert_eq!(fs::read_to_string(bundle.join("patch.diff")).unwrap(), COMMENT_PATCH, "the applied patch, honestly");
    assert!(!manifest.to_string().contains("SUCCEEDED"));
}

#[test]
fn while_another_process_drives_cancel_only_requests_and_resume_waits() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    // Stand in for a live driver: hold the home's driver lock in this process.
    let lock = fs::File::options().write(true).open(cli.home().join("driver.lock")).unwrap();
    lock.lock().unwrap();

    let out = cli.json(&["cancel", &id]);
    assert_eq!(out["state"], "RUNNING");
    assert_eq!(out["cancel_requested"], true);
    assert!(out["note"].as_str().unwrap().contains("another agentos process"));
    cli.cmd(&["resume", &id]).assert().code(1).stderr(predicate::str::contains("another agentos process is driving"));
    let status = cli.status(&id);
    assert_eq!(status["state"], "RUNNING");
    assert_eq!(status["outstanding_effects"].as_array().unwrap().len(), 1, "nothing was recovered under someone else's lock");

    // The driver goes away; whoever drives next completes the cancel.
    drop(lock);
    assert_eq!(cli.json(&["resume", &id])["state"], "CANCELLED");
    assert_eq!(cli.status(&id)["outstanding_effects"], json!([]));
}

#[test]
fn recovery_itself_can_be_killed_and_resumed_again() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:apply_patch");

    // The restarted controller re-dispatches the patch and dies right there, again.
    let assert = cli.cmd(&["resume", &id, "--crash-at", "after-dispatch:apply_patch"]).assert().code(75);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(stderr.contains(&format!(r#"{{"crashed":"after-dispatch","task_id":"{id}"}}"#)), "{stderr}");
    assert!(!stderr.contains('\u{1b}'), "no terminal colours when stderr is not a terminal");
    assert_eq!(cli.status(&id)["state"], "RUNNING");

    assert_eq!(cli.json(&["resume", &id])["state"], "SUCCEEDED");
    let decisions: Vec<Value> =
        cli.events(&id).into_iter().filter(|e| e["type"] == "RecoveryDecision").map(|e| e["payload"].clone()).collect();
    assert_eq!(decisions.len(), 2, "one decision per restart: {decisions:?}");
    assert!(decisions.iter().all(|d| d["kind"] == "apply_patch" && d["decision"] == "Redispatch"));
    assert_eq!(decisions[1]["lease_generation"], 2, "the second restart found the second lease in flight");
}
