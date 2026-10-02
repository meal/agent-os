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
        self.cmd_with_profiles(&fixtures().join("profiles"), args)
    }

    fn cmd_with_profiles(&self, profiles: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentos"));
        cmd.arg("--home").arg(self.home()).arg("--profiles").arg(profiles).args(args);
        cmd
    }

    /// Runs a command that must succeed; returns its stdout parsed as one JSON value.
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).assert().success().get_output().stdout.clone();
        serde_json::from_slice(&out).unwrap_or_else(|e| panic!("stdout of {args:?} is not JSON ({e}): {}", String::from_utf8_lossy(&out)))
    }

    fn json_with_profiles(&self, profiles: &Path, args: &[&str]) -> Value {
        let out = self.cmd_with_profiles(profiles, args).assert().success().get_output().stdout.clone();
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
    if let Some(caps) = obj.get_mut("capabilities").and_then(Value::as_array_mut) {
        for cap in caps {
            if let Some(cap) = cap.as_object_mut() {
                cap.remove("handle_prefix");
            }
        }
    }
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
    // A profile id is one plain registry name: it can never reach outside the registry.
    let valid = fs::read_to_string(cli.contract(&repo)).unwrap();
    for (i, bad) in ["../../tmp/x", "/abs/path", "a/b", "..", "", ".", "-x"].into_iter().enumerate() {
        let id = serde_json::to_string(bad).unwrap();
        let file = cli.write(&format!("bad-profile-{i}.json"), &valid.replace("\"parser-checks-v1\"", &id));
        cli.cmd(&["submit", &file, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()])
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::contains("verification_profile"));
    }
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
    assert_crashed_on_kind(&cli, &id, spec, &stuck);

    let resumed = cli.json(&["resume", &id]);
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }), "{spec}");
    let status = cli.status(&id);
    assert_eq!(status["verified_digest"], status["workspace_digest"]);
    assert_eq!(status["outstanding_effects"], json!([]));
    let (_, manifest) = cli.export(&id, "recovered");
    assert_eq!(manifest["task_id"], id.as_str());
    assert_eq!(normalized(&manifest), normalized(&expected), "{spec}: recovered bundle equals the uncrashed one");
}

/// The crash happened on an effect of the spec's KIND: the last intended effect (or, for a
/// crash right after the agent turn, the journaled action) is of that kind, and an effect
/// crashed mid-flight is the one outstanding.
fn assert_crashed_on_kind(cli: &Cli, id: &str, spec: &str, status: &Value) {
    let mut parts = spec.split(':');
    let (point, kind) = (parts.next().unwrap(), parts.next().unwrap());
    let variant = match kind {
        "read_snapshot" => "ReadSnapshot",
        "apply_patch" => "ApplyPatch",
        "run_verification" => "RunVerification",
        other => panic!("table row without a kind: {other}"),
    };
    let events = cli.events(id);
    let outstanding: Vec<&str> =
        status["outstanding_effects"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    if point == "after-agent-turn-journaled" {
        let last = events.last().unwrap();
        assert_eq!(last["type"], "AgentTurn", "{spec}");
        let action = &last["payload"]["action"];
        assert!(action == variant || action.get(variant).is_some() || (kind == "run_verification" && action == "Verify"), "{spec}: {action}");
        assert!(outstanding.is_empty(), "{spec}: {outstanding:?}");
        return;
    }
    let intended = events.iter().rev().find(|e| e["type"] == "EffectIntended").expect("an effect was intended");
    let k = &intended["payload"]["kind"];
    assert!(k == variant || k.get(variant).is_some(), "{spec}: last intended effect is {k}");
    if point == "after-complete" {
        assert!(outstanding.is_empty(), "{spec}: {outstanding:?}");
    } else {
        assert_eq!(outstanding, vec![kind], "{spec}");
    }
    if kind == "run_verification" {
        assert_eq!(status["state"], "VERIFYING", "{spec}");
    }
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
    // The new session's agent re-sent its patch, which no longer applied (a FAILED effect):
    // only the patch that really applied is in the bundle.
    assert!(cli.event_types(&id).contains(&"EffectFailed".to_string()));
    let (bundle, manifest) = cli.export(&id, "bundle");
    assert_eq!(fs::read(bundle.join("patch.diff")).unwrap(), fs::read(fix_patch()).unwrap());
    assert_eq!(manifest["patches"].as_array().unwrap().len(), 1);
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
    // As every real holder does on acquiring it: forget what a previous (dead) holder drove.
    lock.set_len(0).unwrap();

    let out = cli.json(&["cancel", &id]);
    assert_eq!(out["state"], "RUNNING");
    assert_eq!(out["cancel_requested"], true);
    let note = out["note"].as_str().unwrap();
    assert!(note.contains(&format!("next `agentos resume {id}` or `agentos cancel {id}`")), "{note}");
    // When the lock holder says it drives this very task, its runner completes the cancel.
    fs::write(cli.home().join("driver.lock"), &id).unwrap();
    let note = cli.json(&["cancel", &id])["note"].as_str().unwrap().to_string();
    assert!(note.contains("is driving this task; it completes the cancel at its next step"), "{note}");
    fs::write(cli.home().join("driver.lock"), "").unwrap();
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

#[test]
fn a_registry_entry_that_links_outside_the_registry_is_refused() {
    let cli = Cli::new();
    let registry = cli.path("profiles");
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &registry.join("parser-checks-v1")).unwrap();
    let outside = cli.path("outside");
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, registry.join("evil")).unwrap();
    let contract = cli.contract(&cli.repo_copy());
    let evil = cli.write("evil.json", &fs::read_to_string(&contract).unwrap().replace("\"parser-checks-v1\"", "\"evil\""));

    cli.cmd_with_profiles(&registry, &["submit", &evil]).assert().code(2).stderr(predicate::str::contains("outside the profile registry"));
    assert!(!cli.home().exists());
    // A real registry entry still works.
    let out = cli.cmd_with_profiles(&registry, &["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()]).assert().success();
    let out: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(out["state"], "SUCCEEDED");
}

#[test]
fn every_export_is_journaled_and_a_refused_one_is_not() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"].as_str().unwrap().to_string();
    let exported = |cli: &Cli| -> Vec<Value> {
        cli.events(&id).into_iter().filter(|e| e["type"] == "Exported").map(|e| e["payload"].clone()).collect()
    };

    let (bundle, _) = cli.export(&id, "one");
    let events = exported(&cli);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["manifest_digest"], digest_of_file(&bundle.join("manifest.json")));
    assert_eq!(events[0]["dir"], bundle.to_str().unwrap());
    assert_eq!(events[0]["files"], 5, "manifest, patch.diff, one patch, two evidence files");

    cli.cmd(&["export", &id, bundle.to_str().unwrap()]).assert().code(1);
    assert_eq!(exported(&cli).len(), 1, "a refused export journals nothing");
    cli.export(&id, "two");
    assert_eq!(exported(&cli).len(), 2);
}

#[test]
fn tampered_inputs_after_a_crash_fail_the_task_without_stranding_its_in_flight_effect() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:read_snapshot");
    let stuck = cli.status(&id);
    assert_eq!(stuck["outstanding_effects"][0]["state"], "Dispatched");
    assert_eq!(stuck["usage"]["reserved_tool_actions"], 1);
    fs::write(cli.home().join("tasks").join(&id).join("snapshot/src/parser.py"), "tampered = True\n").unwrap();

    let resumed = cli.json(&["resume", &id]);

    assert_eq!(resumed["state"], "FAILED");
    let status = cli.status(&id);
    let failed = cli.events(&id).into_iter().find(|e| e["type"] == "Failed").unwrap();
    assert!(failed["payload"]["Failed"]["reason"].as_str().unwrap().contains("recorded snapshot changed"), "{failed}");
    // The in-flight snapshot was reconciled, not left DISPATCHED with a live reservation.
    let outstanding = status["outstanding_effects"].as_array().unwrap();
    assert!(outstanding.iter().all(|e| e["state"] != "Dispatched" && e["state"] != "Intended"), "{status}");
    assert_eq!(status["usage"]["reserved_tool_actions"], 0, "{status}");
    assert_eq!(status["usage"]["uncertain_tool_actions"], 1, "it may have run: the reservation stays visible");
    assert!(cli.events(&id).iter().any(|e| e["type"] == "RecoveryDecision"));
    // Nothing changes on a further resume.
    let n = cli.events(&id).len();
    assert_eq!(cli.json(&["resume", &id])["state"], "FAILED");
    assert_eq!(cli.events(&id).len(), n);
}

impl Cli {
    /// The task's capability grants, read straight from the home's store.
    fn grants(&self, id: &str) -> Vec<agentos_core::broker::CapabilityGrant> {
        let db = agentos_store::db::Db::open(&self.home().join("agentos.db")).unwrap();
        db.grants(&serde_json::from_value(json!(id)).unwrap()).unwrap()
    }

    fn deadline_ts(&self, id: &str) -> i64 {
        let db = agentos_store::db::Db::open(&self.home().join("agentos.db")).unwrap();
        db.deadline_ts(&serde_json::from_value(json!(id)).unwrap()).unwrap()
    }
}

#[test]
fn submit_without_yes_issues_no_handles_and_resume_approves() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let out = cli.json(&["submit", &contract, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    let id = out["task_id"].as_str().unwrap();
    assert!(cli.grants(id).is_empty(), "no handles before approval");
    assert_eq!(cli.deadline_ts(id), 0, "the deadline has not started");
    assert!(!cli.event_types(id).contains(&"CapabilitiesIssued".to_string()));

    assert_eq!(cli.json(&["resume", id])["state"], "SUCCEEDED");
    let grants = cli.grants(id);
    assert_eq!(grants.len(), 4, "one handle per contract capability");
    assert!(cli.deadline_ts(id) > 0);
    let types = cli.event_types(id);
    assert_eq!(types.iter().filter(|t| *t == "CapabilitiesIssued").count(), 1);
    assert_subsequence(&types, &["TaskCreated", "Submitted", "CapabilitiesIssued", "Started"]);
    // The journal shows prefixes only.
    let printed = String::from_utf8(cli.cmd(&["events", id]).assert().success().get_output().stdout.clone()).unwrap();
    for g in &grants {
        assert!(printed.contains(g.handle.prefix()));
        assert!(!printed.contains(&g.handle.to_string()), "full handle in `agentos events`");
    }
    // A finished task is not approved again.
    cli.json(&["resume", id]);
    assert_eq!(cli.grants(id), grants);
}

#[test]
fn submit_yes_approves_before_driving() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let out = cli.submit_yes(&contract, &fix_patch());
    let id = out["task_id"].as_str().unwrap();
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(cli.grants(id).len(), 4);
    let types = cli.event_types(id);
    assert_eq!(types.iter().filter(|t| *t == "CapabilitiesIssued").count(), 1);
    assert_subsequence(&types, &["TaskCreated", "Submitted", "CapabilitiesIssued", "Started", "CapabilityGranted", "EffectIntended"]);
}

/// Job directories under the home, as (effect id, directory name): names are
/// `<effect_id>-<attempt_id>` and effect ids are 64 hex digits.
fn job_dirs(cli: &Cli) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(cli.home().join("jobs")) else { return Vec::new() };
    let mut dirs: Vec<(String, String)> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .map(|name| (name[..64].to_string(), name))
        .collect();
    dirs.sort();
    dirs
}

fn assert_one_job_per_effect(cli: &Cli, what: &str) {
    let dirs = job_dirs(cli);
    assert!(!dirs.is_empty(), "{what}: no job directories");
    let effects: BTreeSet<&String> = dirs.iter().map(|(e, _)| e).collect();
    assert_eq!(effects.len(), dirs.len(), "{what}: more than one job for an effect: {dirs:?}");
}

/// Pids of processes whose command line mentions `needle`.
fn processes_mentioning(needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
        if String::from_utf8_lossy(&cmdline).contains(needle) {
            found.push(name);
        }
    }
    found
}

#[test]
fn supervise_subcommands_are_hidden_from_help() {
    let cli = Cli::new();
    let out = cli.cmd(&["--help"]).assert().success().get_output().stdout.clone();
    assert!(!String::from_utf8_lossy(&out).contains("supervise"));
}

#[test]
fn home_has_no_receipts_dir() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    assert_eq!(cli.submit_yes(&contract, &fix_patch())["state"], "SUCCEEDED");
    assert!(!cli.home().join("receipts").exists());
    assert!(cli.home().join("jobs").is_dir());
    assert_one_job_per_effect(&cli, "clean run");
}

#[test]
fn during_execute_rows_resume_by_publishing_the_receipt_with_one_job_per_effect() {
    for spec in ["during-execute:apply_patch", "during-execute:run_verification"] {
        let cli = Cli::new();
        let contract = cli.contract(&fixtures().join("parser-repo"));
        let clean = cli.submit_yes(&contract, &fix_patch());
        let (_, expected) = cli.export(clean["task_id"].as_str().unwrap(), "clean");
        let before = job_dirs(&cli).len();

        let id = cli.crash(&contract, spec);
        assert!(job_dirs(&cli).len() > before, "{spec}: the crashed run launched its job");
        assert_eq!(cli.json(&["resume", &id]), json!({ "task_id": id, "state": "SUCCEEDED" }), "{spec}");
        // Resume published the crashed effect's receipt instead of launching it again: the
        // two runs together have exactly two jobs per effect of one clean run.
        assert_eq!(job_dirs(&cli).len(), 2 * before, "{spec}");
        assert_one_job_per_effect(&cli, spec);
        let (_, manifest) = cli.export(&id, "recovered");
        assert_eq!(normalized(&manifest), normalized(&expected), "{spec}");
    }
}

#[test]
fn status_shows_job_state_for_outstanding_effects() {
    let cli = Cli::new();
    let contract = cli.contract(&fixtures().join("parser-repo"));
    let id = cli.crash(&contract, "during-execute:run_verification");
    let status = cli.status(&id);
    let outstanding = status["outstanding_effects"].as_array().unwrap();
    assert_eq!(outstanding.len(), 1);
    let jobs = status["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1, "{status}");
    assert_eq!(jobs[0]["effect_id"], outstanding[0]["effect_id"]);
    // The controller died right after the launch: the job is either still running or done.
    assert!(jobs[0]["alive"] == true || jobs[0]["receipt"] == true, "{status}");
    // A job that has not written its first status yet has no state; a finished one has.
    assert!(jobs[0]["state"].is_string() || jobs[0]["receipt"] == false, "{status}");
    cli.json(&["resume", &id]);
    assert_eq!(cli.status(&id)["jobs"], json!([]));
}

/// A registry whose only extra profile is the parser check preceded by a 30 s sleep, and a
/// contract for it over a copy of the fixture repository.
fn slow_world(cli: &Cli) -> (PathBuf, String) {
    let profiles = cli.path("profiles");
    copy_tree(&fixtures().join("profiles"), &profiles).unwrap();
    let slow = profiles.join("slow-checks-v1");
    copy_tree(&profiles.join("parser-checks-v1"), &slow).unwrap();
    let script = fs::read_to_string(slow.join("check_parser.py")).unwrap();
    fs::write(slow.join("check_parser.py"), format!("import time\ntime.sleep(30)\n{script}")).unwrap();
    fs::write(slow.join("profile.json"), r#"{ "id": "slow-checks-v1", "command": ["python3", "check_parser.py"], "protected": true }"#).unwrap();
    let contract = cli.write("slow.json", &fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap().replace("parser-checks-v1", "slow-checks-v1"));
    (profiles, contract)
}

/// `submit --yes` as a background process.
fn spawn_submit(cli: &Cli, profiles: &Path, contract: &str) -> std::process::Child {
    StdCommand::new(env!("CARGO_BIN_EXE_agentos"))
        .arg("--home")
        .arg(cli.home())
        .arg("--profiles")
        .arg(profiles)
        .args(["submit", contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

/// Waits until the verification job's supervisor is up (it has written a status). Before
/// that the job is only a directory the controller still locks, which recovery rightly
/// retries and a cancel marker would not reach.
fn wait_for_verification_job(cli: &Cli) {
    let started = std::time::Instant::now();
    loop {
        let up = fs::read_dir(cli.home().join("jobs")).into_iter().flatten().flatten().any(|e| {
            e.path().join("status.json").is_file() && fs::read_to_string(e.path().join("request.json")).is_ok_and(|r| r.contains("RunVerification"))
        });
        if up {
            return;
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "the verification job never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn assert_no_job_processes(cli: &Cli) {
    let jobs = cli.home().join("jobs");
    assert_eq!(processes_mentioning(jobs.to_str().unwrap()), Vec::<String>::new(), "no supervisor or worker left");
}

#[test]
fn controller_sigkill_while_a_slow_verification_runs_then_resume_publishes_the_receipt() {
    let cli = Cli::new();
    let (profiles, contract) = slow_world(&cli);
    // Shorten the check: the 30 s sleep becomes 3 s.
    let script = profiles.join("slow-checks-v1/check_parser.py");
    fs::write(&script, fs::read_to_string(&script).unwrap().replace("sleep(30)", "sleep(3)")).unwrap();
    let mut child = spawn_submit(&cli, &profiles, &contract);
    wait_for_verification_job(&cli);
    child.kill().unwrap();
    child.wait().unwrap();

    let id = first_task(&cli);
    let resumed = cli.cmd_with_profiles(&profiles, &["resume", &id]).assert().success().get_output().stdout.clone();
    assert_eq!(serde_json::from_slice::<Value>(&resumed).unwrap(), json!({ "task_id": id, "state": "SUCCEEDED" }));
    let verifications = job_dirs(&cli)
        .into_iter()
        .filter(|(_, name)| fs::read_to_string(cli.home().join("jobs").join(name).join("request.json")).is_ok_and(|r| r.contains("RunVerification")))
        .count();
    assert_eq!(verifications, 1, "exactly one job for the verification effect");
    assert_one_job_per_effect(&cli, "after sigkill");
    assert_no_job_processes(&cli);
}

#[test]
fn revoke_unknown_capability_name_exits_2() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let done = cli.submit_yes(&contract, &fix_patch());
    let id = done["task_id"].as_str().unwrap();
    cli.cmd(&["revoke", id, "--capability", "teleport.now"]).assert().code(2).stderr(predicate::str::contains("unknown capability"));
}

#[test]
fn revoke_on_a_terminal_task_is_allowed_and_cancels_nothing() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let done = cli.submit_yes(&contract, &fix_patch());
    let id = done["task_id"].as_str().unwrap();
    let out = cli.json(&["revoke", id, "--capability", "verification.run"]);
    assert_eq!(out, json!({ "task_id": id, "revoked": ["verification.run"], "cancelled_jobs": 0 }));
    // Revoking again changes nothing.
    assert_eq!(cli.json(&["revoke", id, "--capability", "verification.run"])["revoked"], json!([]));
    assert_eq!(cli.status(id)["state"], "SUCCEEDED");
    assert!(cli.event_types(id).contains(&"CapabilityRevoked".to_string()));
    cli.cmd(&["revoke", "00000000-0000-4000-8000-000000000000"]).assert().code(1).stderr(predicate::str::contains("unknown task"));
}

#[test]
fn revoke_verification_run_stops_the_running_check_from_another_process() {
    let cli = Cli::new();
    let (profiles, contract) = slow_world(&cli);
    let started = std::time::Instant::now();
    let mut child = spawn_submit(&cli, &profiles, &contract);
    wait_for_verification_job(&cli);
    let id = first_task(&cli);
    let out = cli.cmd_with_profiles(&profiles, &["revoke", &id, "--capability", "verification.run"]).assert().success().get_output().stdout.clone();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), json!({ "task_id": id, "revoked": ["verification.run"], "cancelled_jobs": 1 }));
    assert!(child.wait().unwrap().success(), "the driver finished the task");
    assert!(started.elapsed() < std::time::Duration::from_secs(25), "the 30 s check was stopped, took {:?}", started.elapsed());
    assert_eq!(cli.status(&id)["state"], "FAILED");
    let events = cli.events(&id);
    assert!(events.iter().any(|e| e["type"] == "EffectFailed" && e["payload"].to_string().contains("cancelled")), "the killed check is a recorded failure");
    assert_eq!(events.iter().filter(|e| e["type"] == "EffectCompleted").count(), 2, "earlier results stay");
    assert_no_job_processes(&cli);
}

#[test]
fn cancel_drops_markers_for_running_jobs_and_ends_cancelled_with_no_live_process() {
    let cli = Cli::new();
    let (profiles, contract) = slow_world(&cli);
    let started = std::time::Instant::now();
    let mut child = spawn_submit(&cli, &profiles, &contract);
    wait_for_verification_job(&cli);
    let id = first_task(&cli);
    // Another process drives the task: the cancel is only requested, but the running job is
    // told to stop at once, so the driver does not wait out the check.
    let out = cli.cmd_with_profiles(&profiles, &["cancel", &id]).assert().success().get_output().stdout.clone();
    assert_eq!(serde_json::from_slice::<Value>(&out).unwrap()["cancel_requested"], true);
    child.wait().unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(25), "took {:?}", started.elapsed());
    assert_eq!(cli.status(&id)["state"], "CANCELLED");
    assert_no_job_processes(&cli);
}

/// The id of the only task in the home.
fn first_task(cli: &Cli) -> String {
    let mut ids: Vec<String> = fs::read_dir(cli.home().join("tasks")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids.remove(0)
}

/// A profile directory in the scratch dir: the fixture's parser check with `check_prefix`
/// prepended (so its bytes, and digest, differ), registered under `id`.
fn profile_variant(cli: &Cli, name: &str, id: &str, check_prefix: &str) -> PathBuf {
    let dir = cli.path(name);
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &dir).unwrap();
    let script = fs::read_to_string(dir.join("check_parser.py")).unwrap();
    fs::write(dir.join("check_parser.py"), format!("{check_prefix}{script}")).unwrap();
    fs::write(dir.join("profile.json"), json!({ "id": id, "command": ["python3", "check_parser.py"], "protected": true }).to_string()).unwrap();
    dir
}

fn register(cli: &Cli, dir: &Path) -> Value {
    cli.json(&["profile", "register", dir.to_str().unwrap()])
}

/// A contract over a repository copy, naming `verification_profile` and optionally pinning it.
fn contract_for_profile(cli: &Cli, id: &str, pin: Option<&str>) -> String {
    let mut contract: Value = serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    contract["verification_profile"] = json!(id);
    if let Some(pin) = pin {
        contract["profile_digest"] = json!(pin);
    }
    cli.write(&format!("pinned-{}.json", Digest::of(contract.to_string().as_bytes())), &contract.to_string())
}

/// `--profiles` pointing at an empty directory: only the registry can supply a profile.
fn no_legacy(cli: &Cli) -> PathBuf {
    let dir = cli.path("no-legacy");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn submitted_profile_digest(cli: &Cli, id: &str) -> String {
    cli.events(id).iter().find(|e| e["type"] == "Submitted").unwrap()["payload"]["profile_digest"].as_str().unwrap().to_string()
}

#[test]
fn register_twice_is_a_noop_and_changed_bytes_are_a_new_entry() {
    let cli = Cli::new();
    let dir = profile_variant(&cli, "p1", "reg-v1", "");
    let first = register(&cli, &dir);
    assert_eq!(first["id"], "reg-v1");
    assert_eq!(first["digest"].as_str().unwrap().len(), 64);
    assert_eq!(register(&cli, &dir), first, "same bytes, same entry");
    assert_eq!(cli.json(&["profile", "list"]).as_array().unwrap().len(), 1);

    let changed = profile_variant(&cli, "p2", "reg-v1", "# changed\n");
    let second = register(&cli, &changed);
    assert_ne!(second["digest"], first["digest"]);
    let listed = cli.json(&["profile", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
}

#[test]
fn registered_entries_have_no_write_bits() {
    use std::os::unix::fs::PermissionsExt;
    let cli = Cli::new();
    let digest = register(&cli, &profile_variant(&cli, "p1", "ro-v1", ""))["digest"].as_str().unwrap().to_string();
    let entry = cli.home().join("registry").join(format!("ro-v1@{digest}"));
    for path in [entry.clone(), entry.join("profile.json"), entry.join("check_parser.py")] {
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "{} is writable: {mode:o}", path.display());
    }
    assert!(cli.home().join("registry").join(format!("ro-v1@{digest}.meta.json")).is_file());
}

#[test]
fn ids_with_at_sign_or_traversal_are_rejected_at_register() {
    let cli = Cli::new();
    for (i, id) in ["a@b", "../x", "a/b", "..", "", "-x"].into_iter().enumerate() {
        let dir = profile_variant(&cli, &format!("bad-{i}"), id, "");
        cli.cmd(&["profile", "register", dir.to_str().unwrap()]).assert().code(2).stderr(predicate::str::contains("plain name"));
    }
    assert!(!cli.home().join("registry").exists() || fs::read_dir(cli.home().join("registry")).unwrap().next().is_none());
    let empty = cli.path("empty-command");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("profile.json"), r#"{"id":"e-v1","command":[]}"#).unwrap();
    cli.cmd(&["profile", "register", empty.to_str().unwrap()]).assert().code(2).stderr(predicate::str::contains("command"));
}

#[test]
fn registered_profile_runs_end_to_end() {
    let cli = Cli::new();
    let digest = register(&cli, &fixtures().join("profiles/parser-checks-v1"))["digest"].as_str().unwrap().to_string();
    let contract = contract_for_profile(&cli, "parser-checks-v1", None);
    let out = cli.json_with_profiles(&no_legacy(&cli), &["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()), digest);
}

#[test]
fn submit_with_a_pin_for_a_missing_digest_exits_2() {
    let cli = Cli::new();
    register(&cli, &fixtures().join("profiles/parser-checks-v1"));
    let contract = contract_for_profile(&cli, "parser-checks-v1", Some(&"0".repeat(64)));
    cli.cmd(&["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is not in the registry"));
}

#[test]
fn submit_with_the_pin_uses_exactly_that_digest_even_if_a_newer_entry_exists() {
    let cli = Cli::new();
    let older = register(&cli, &profile_variant(&cli, "p1", "pin-v1", ""))["digest"].as_str().unwrap().to_string();
    // Registration times are milliseconds: make sure the second entry is strictly newer.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer = register(&cli, &profile_variant(&cli, "p2", "pin-v1", "# newer\n"))["digest"].as_str().unwrap().to_string();
    assert_ne!(older, newer);

    let pinned = contract_for_profile(&cli, "pin-v1", Some(&older));
    let out = cli.json_with_profiles(&no_legacy(&cli), &["submit", &pinned, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()), older);
    let unpinned = contract_for_profile(&cli, "pin-v1", None);
    let out = cli.json_with_profiles(&no_legacy(&cli), &["submit", &unpinned, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()), newer, "no pin: the newest entry");
}

#[test]
fn legacy_profiles_dir_still_works_with_the_profiles_flag() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    assert!(!cli.home().join("registry").exists());
    assert_eq!(cli.submit_yes(&contract, &fix_patch())["state"], "SUCCEEDED");
}

#[test]
fn registry_wins_over_legacy_when_both_exist() {
    let cli = Cli::new();
    // The registry's parser-checks-v1 rejects everything; the legacy one (the fixture) is right.
    let strict = profile_variant(&cli, "p1", "parser-checks-v1", "import sys\nsys.exit(1)\n");
    let digest = register(&cli, &strict)["digest"].as_str().unwrap().to_string();
    let contract = contract_for_profile(&cli, "parser-checks-v1", None);
    let out = cli.json(&["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(out["state"], "FAILED", "the registry entry was used, not the legacy directory");
    assert_eq!(submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()), digest);
}

#[test]
fn cli_tampered_staged_profile_fails_the_task_before_any_verification() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json(&["submit", &contract, "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    let id = ready["task_id"].as_str().unwrap();
    let staged = cli.home().join("tasks").join(id).join("profile/check_parser.py");
    fs::write(&staged, "import sys\nsys.exit(0)\n").unwrap();

    assert_eq!(cli.json(&["resume", id])["state"], "FAILED");
    let events = cli.events(id);
    let reason = events.iter().find(|e| e["type"] == "Failed").unwrap()["payload"].to_string();
    assert!(reason.contains("recorded profile changed"), "{reason}");
    assert!(!events.iter().any(|e| e["type"] == "EffectIntended"), "nothing ran on the tampered profile");
    assert!(job_dirs(&cli).is_empty());
}

/// Every full handle of the task, read from the database (they appear nowhere else).
fn full_handles(cli: &Cli, id: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(cli.home().join("agentos.db")).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM capabilities WHERE task_id = ?1").unwrap();
    stmt.query_map([id], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
}

fn denials(cli: &Cli, id: &str) -> Vec<Value> {
    cli.events(id).into_iter().filter(|e| e["type"] == "CapabilityDenied").map(|e| e["payload"].clone()).collect()
}

#[test]
fn export_journals_the_granted_decision() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"].as_str().unwrap().to_string();
    cli.export(&id, "bundle");
    let granted: Vec<Value> = cli.events(&id).into_iter().filter(|e| e["type"] == "CapabilityGranted" && e["payload"]["operation"] == "artifact.export").collect();
    assert_eq!(granted.len(), 1, "{granted:?}");
    assert!(granted[0]["payload"]["handle_prefix"].as_str().unwrap().len() == 8);
}

#[test]
fn export_without_artifact_export_capability_exits_1_writes_nothing_and_journals_the_denial() {
    let cli = Cli::new();
    let repo = cli.repo_copy();
    let mut contract: Value = serde_json::from_str(&fs::read_to_string(cli.contract(&repo)).unwrap()).unwrap();
    contract["capabilities"] = json!(["snapshot.read", "workspace.apply_patch", "verification.run"]);
    let file = cli.write("no-export.json", &contract.to_string());
    let id = cli.submit_yes(&file, &fix_patch())["task_id"].as_str().unwrap().to_string();
    let dir = cli.path("bundle");
    cli.cmd(&["export", &id, dir.to_str().unwrap()]).assert().code(1).stdout("").stderr(predicate::str::contains("export denied"));
    assert!(!dir.exists(), "nothing written");
    let denied = denials(&cli, &id);
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!((denied[0]["operation"].as_str(), denied[0]["reason"].as_str()), (Some("artifact.export"), Some("unknown_handle")));
    assert!(!cli.event_types(&id).contains(&"Exported".to_string()));
}

#[test]
fn export_after_revoking_artifact_export_is_denied_revoked() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"].as_str().unwrap().to_string();
    cli.json(&["revoke", &id, "--capability", "artifact.export"]);
    let dir = cli.path("bundle");
    cli.cmd(&["export", &id, dir.to_str().unwrap()]).assert().code(1).stderr(predicate::str::contains("revoked"));
    assert!(!dir.exists());
    assert_eq!(denials(&cli, &id)[0]["reason"], "revoked");
}

#[test]
fn a_task_failed_on_its_deadline_can_still_be_exported() {
    let cli = Cli::new();
    let (profiles, slow) = slow_world(&cli);
    // The 30 s check cannot finish in 3 s: the supervisor kills it at the deadline.
    let mut contract: Value = serde_json::from_str(&fs::read_to_string(&slow).unwrap()).unwrap();
    contract["limits"]["deadline_seconds"] = json!(3);
    let file = cli.write("slow-deadline.json", &contract.to_string());
    let out = cli.json_with_profiles(&profiles, &["submit", &file, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()]);
    assert_eq!(out["state"], "FAILED");
    let id = out["task_id"].as_str().unwrap();
    let failed = cli.events(id).into_iter().find(|e| e["type"] == "Failed").unwrap();
    assert!(failed["payload"].to_string().contains("deadline exceeded"), "{failed}");
    assert_no_job_processes(&cli);
    let (_, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["state"], "FAILED");
    assert!(manifest["verified_digest"].is_null(), "no success claim");
}

#[test]
fn manifest_lists_capabilities_with_prefixes_only() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"].as_str().unwrap().to_string();
    let (dir, manifest) = cli.export(&id, "bundle");
    let caps = manifest["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), 4);
    assert!(caps.iter().all(|c| c["handle_prefix"].as_str().unwrap().len() == 8 && c["revoked"] == false));
    let handles = full_handles(&cli, &id);
    assert_eq!(handles.len(), 4);
    let mut everything = String::new();
    for entry in walk(&dir) {
        everything.push_str(&String::from_utf8_lossy(&fs::read(entry).unwrap()));
    }
    everything.push_str(&serde_json::to_string(&cli.status(&id)).unwrap());
    for e in cli.events(&id) {
        everything.push_str(&e.to_string());
    }
    for handle in &handles {
        assert!(!everything.contains(handle.as_str()), "a full handle leaked");
        assert!(everything.contains(&handle[..8]), "its prefix is shown");
    }
}

#[test]
fn status_lists_capabilities_without_full_handles() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"].as_str().unwrap().to_string();
    cli.json(&["revoke", &id, "--capability", "verification.run"]);
    let status = cli.status(&id);
    let caps = status["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), 4);
    let revoked: Vec<&str> = caps.iter().filter(|c| c["revoked"] == true).map(|c| c["operation"].as_str().unwrap()).collect();
    assert_eq!(revoked, vec!["verification.run"]);
    let export = caps.iter().find(|c| c["operation"] == "artifact.export").unwrap();
    assert!(export["expires_ts"].is_null(), "export never expires");
    let text = status.to_string();
    for handle in full_handles(&cli, &id) {
        assert!(!text.contains(&handle));
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        if entry.file_type().unwrap().is_dir() {
            out.extend(walk(&entry.path()));
        } else {
            out.push(entry.path());
        }
    }
    out
}
