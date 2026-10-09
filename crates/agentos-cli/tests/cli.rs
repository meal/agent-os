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
use serde_json::{Value, json};
use tempfile::TempDir;

/// The local fake of the Messages API, shared with the engine's tests.
#[path = "../../agentos-engine/tests/common/http.rs"]
#[allow(dead_code)]
mod http;
/// The KVM gate, shared with the engine's tests.
#[path = "../../agentos-engine/tests/common/kvm.rs"]
mod kvm;
/// The Firecracker process scan, shared with the engine's tests.
#[path = "../../agentos-engine/tests/common/procs.rs"]
mod procs;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .unwrap()
}

fn fix_patch() -> PathBuf {
    fixtures().join("parser-repo.fix.patch")
}

/// Applies, fixes nothing.
const COMMENT_PATCH: &str = "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1,2 +1,3 @@\n+# TODO: handle whitespace\n def parse_kv(text: str) -> dict:\n     \"\"\"Parse 'key = value' lines into a dict, skipping blanks and '#' comments.\"\"\"\n";

const TERMINAL: [&str; 3] = ["SUCCEEDED", "FAILED", "CANCELLED"];

/// Which worker `Cli::cmd` runs the tasks on: `AGENTOS_TEST_WORKER` = `host` (default) |
/// `firecracker-fake` (the Firecracker worker over the fake guest, the CLI's own
/// `supervise fake-guest`) | `firecracker` (the real, jailed worker of the KVM tier). Any
/// other value panics, so a typo never runs the host tier under the name of another.
fn test_worker() -> &'static str {
    let worker = match std::env::var("AGENTOS_TEST_WORKER").as_deref() {
        Err(_) | Ok("") | Ok("host") => "host",
        Ok("firecracker-fake") => "firecracker-fake",
        Ok("firecracker") => "firecracker",
        Ok(other) => panic!(
            "AGENTOS_TEST_WORKER={other:?}: the CLI tier knows host, firecracker-fake and firecracker"
        ),
    };
    if test_jail_fake() && worker != "firecracker-fake" {
        panic!("AGENTOS_TEST_JAIL=fake needs AGENTOS_TEST_WORKER=firecracker-fake");
    }
    worker
}

/// `AGENTOS_TEST_JAIL=fake` (any other non-empty value panics). In the CLI tier it makes
/// every command's jail probe answer `ok` (`AGENTOS_TEST_JAIL_PROBE=ok`): the decision is
/// `Jailed` and `Submitted` records `jailed: true`, while the launcher stays the fake guest.
fn test_jail_fake() -> bool {
    match std::env::var("AGENTOS_TEST_JAIL").as_deref() {
        Err(_) | Ok("") => false,
        Ok("fake") => true,
        Ok(other) => panic!("AGENTOS_TEST_JAIL={other:?}: this tier knows only fake"),
    }
}

/// The guest profile the test contracts name: the id of the image under test. Under the KVM
/// tier (`AGENTOS_KVM_TESTS`) that is the configured image's, which need not be the default
/// (`AGENTOS_ACCEPTANCE_IMAGE=python-stdlib-py314-v1`); the dummy image is registered under
/// the same id, so every tier agrees.
fn guest_profile() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        std::env::var_os("AGENTOS_KVM_TESTS")
            .and_then(|_| kvm::configured_image_id())
            .unwrap_or_else(|| "python-stdlib-v1".into())
    })
}

fn fake_mode() -> bool {
    test_worker() == "firecracker-fake"
}

/// The real, jailed Firecracker worker (KVM tier).
fn real_mode() -> bool {
    test_worker() == "firecracker"
}

/// The KVM tier's binaries and image; the gate panics with its reasons when the real worker
/// was asked for and cannot run.
fn real_kvm() -> kvm::Kvm {
    kvm::require().expect("AGENTOS_TEST_WORKER=firecracker needs the KVM tier: set AGENTOS_KVM_TESTS=1 (docker compose run --rm test-kvm …)")
}

/// The CLI's own configuration and test switches: never inherited from the test's
/// environment, so every command runs exactly the worker its helper chose.
const SCRUBBED_ENV: [&str; 13] = [
    "AGENTOS_API_KEY_FILE",
    "AGENTOS_ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "AGENTOS_WORKER",
    "AGENTOS_FIRECRACKER",
    "AGENTOS_JAILER",
    "AGENTOS_JAIL_UID",
    "AGENTOS_JAIL_GID",
    "AGENTOS_ALLOW_UNJAILED",
    "AGENTOS_TEST_WORKERS",
    "AGENTOS_TEST_FAKE_GUEST",
    "AGENTOS_TEST_JAIL_PROBE",
    "AGENTOS_GUEST_IMAGE",
];

/// How a command reaches its worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// No `--worker` flag, no test switches: what a user types.
    Plain,
    /// No `--worker` flag, but the fake-guest switches (`AGENTOS_TEST_WORKERS=1`,
    /// `AGENTOS_TEST_FAKE_GUEST=1`): later commands on a Firecracker task.
    PlainFake,
    /// `--worker firecracker` over the fake guest.
    Fake,
    /// `--worker firecracker --firecracker $AGENTOS_FIRECRACKER --jailer $AGENTOS_JAILER`:
    /// the real worker, jailed (never `--allow-unjailed`: the tier must jail).
    Real,
}

/// A scratch directory holding an agentos home (created on demand by the CLI) and inputs.
struct Cli {
    dir: TempDir,
}

impl Cli {
    /// A scratch home; under `AGENTOS_TEST_WORKER=firecracker-fake` the dummy guest image is
    /// registered once, under `firecracker` the real one (`$AGENTOS_GUEST_IMAGE`), so `cmd`
    /// can submit to the Firecracker worker.
    fn new() -> Cli {
        let cli = Cli::bare();
        if fake_mode() {
            cli.register_guest_image();
        }
        if real_mode() {
            let image = real_kvm().image_dir;
            cli.json_as(Mode::Plain, &["image", "register", image.to_str().unwrap()]);
        }
        cli
    }

    /// A scratch home with nothing registered, whatever the tier.
    fn bare() -> Cli {
        Cli {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// Registers the dummy image under [`guest_profile`] (the contracts' `profile`); returns its digest.
    fn register_guest_image(&self) -> String {
        let dir = fake_image_dir(self, "guest-image", guest_profile(), 0x68);
        self.json_as(Mode::Plain, &["image", "register", dir.to_str().unwrap()])["digest"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The tier's mode: `Fake` under `AGENTOS_TEST_WORKER=firecracker-fake`, `Real` under
    /// `firecracker`, else `Plain`.
    fn mode() -> Mode {
        if fake_mode() {
            Mode::Fake
        } else if real_mode() {
            Mode::Real
        } else {
            Mode::Plain
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_with_profiles(&fixtures().join("profiles"), args)
    }

    fn cmd_with_profiles(&self, profiles: &Path, args: &[&str]) -> Command {
        Command::from_std(self.std_cmd(Cli::mode(), true, profiles, args))
    }

    /// A command in `mode`, whatever the tier.
    fn cmd_as(&self, mode: Mode, args: &[&str]) -> Command {
        Command::from_std(self.std_cmd(mode, false, &fixtures().join("profiles"), args))
    }

    /// `tier`: the command is the tier's own (`cmd`), not one a test chose explicitly.
    fn std_cmd(&self, mode: Mode, tier: bool, profiles: &Path, args: &[&str]) -> StdCommand {
        let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_agentos"));
        for var in SCRUBBED_ENV {
            cmd.env_remove(var);
        }
        cmd.arg("--home")
            .arg(self.home())
            .arg("--profiles")
            .arg(profiles);
        if mode == Mode::Fake {
            cmd.args(["--worker", "firecracker"]);
        }
        if mode == Mode::Real {
            let kvm = real_kvm();
            cmd.args(["--worker", "firecracker", "--firecracker"])
                .arg(&kvm.firecracker_bin)
                .arg("--jailer")
                .arg(&kvm.jailer_bin);
        }
        if matches!(mode, Mode::Fake | Mode::PlainFake) {
            cmd.env("AGENTOS_TEST_WORKERS", "1")
                .env("AGENTOS_TEST_FAKE_GUEST", "1");
        }
        // The tier's jail setting applies to the tier's own commands only.
        if tier && mode == Mode::Fake && test_jail_fake() {
            cmd.env("AGENTOS_TEST_JAIL_PROBE", "ok");
        }
        cmd.args(args);
        cmd
    }

    fn json_as(&self, mode: Mode, args: &[&str]) -> Value {
        let out = self
            .cmd_as(mode, args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap_or_else(|e| {
            panic!(
                "stdout of {args:?} is not JSON ({e}): {}",
                String::from_utf8_lossy(&out)
            )
        })
    }

    /// Runs a command that must succeed; returns its stdout parsed as one JSON value.
    fn json(&self, args: &[&str]) -> Value {
        let out = self
            .cmd(args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap_or_else(|e| {
            panic!(
                "stdout of {args:?} is not JSON ({e}): {}",
                String::from_utf8_lossy(&out)
            )
        })
    }

    fn json_with_profiles(&self, profiles: &Path, args: &[&str]) -> Value {
        let out = self
            .cmd_with_profiles(profiles, args)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice(&out).unwrap_or_else(|e| {
            panic!(
                "stdout of {args:?} is not JSON ({e}): {}",
                String::from_utf8_lossy(&out)
            )
        })
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
            "profile": guest_profile(),
            "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": {
                "model_requests": 1, "max_output_tokens_per_request": 1000, "tool_actions": tool_actions,
                "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256
            }
        });
        self.write(
            &format!("task-{}.json", Digest::of(contract.to_string().as_bytes())),
            &contract.to_string(),
        )
    }

    fn contract(&self, source: &Path) -> String {
        self.contract_with(source, 10, "recorded-at-submission")
    }

    fn submit_yes(&self, contract: &str, patch: &Path) -> Value {
        self.json(&[
            "submit",
            contract,
            "--yes",
            "--fake-agent-patch",
            patch.to_str().unwrap(),
        ])
    }

    /// `submit --yes --crash-at spec`: must die with exit 75; returns the task id it reported.
    fn crash(&self, contract: &str, spec: &str) -> String {
        let assert = self
            .cmd(&[
                "submit",
                contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
                "--crash-at",
                spec,
            ])
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
        let out = self
            .cmd(&["events", id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn event_types(&self, id: &str) -> Vec<String> {
        self.events(id)
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_string())
            .collect()
    }

    fn export(&self, id: &str, name: &str) -> (PathBuf, Value) {
        let dir = self.path(name);
        let printed = self.json(&["export", id, dir.to_str().unwrap()]);
        let manifest: Value =
            serde_json::from_slice(&fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(printed, manifest, "export prints the manifest it wrote");
        (dir, manifest)
    }
}

fn assert_subsequence(haystack: &[String], needles: &[&str]) {
    let mut it = haystack.iter();
    for n in needles {
        assert!(
            it.any(|h| h == n),
            "missing {n:?} in order within {haystack:?}"
        );
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

/// Task `id` ran on the tier's worker: never a silent fallback to the host worker.
fn assert_tier_worker(cli: &Cli, id: &str) {
    let submitted = cli.submitted(id);
    if fake_mode() {
        assert_eq!(submitted["worker"], "firecracker", "{submitted}");
        assert_eq!(submitted["firecracker_version"], "fake", "{submitted}");
        assert_eq!(submitted["jailed"], test_jail_fake(), "{submitted}");
    } else if real_mode() {
        assert_eq!(submitted["worker"], "firecracker", "{submitted}");
        assert_eq!(
            submitted["firecracker_version"], "Firecracker v1.17.0",
            "{submitted}"
        );
        assert_eq!(
            submitted["jailed"], true,
            "the real tier always jails: {submitted}"
        );
    } else {
        assert_eq!(submitted["worker"], "host", "{submitted}");
    }
}

/// Nothing of a task was written: no home at all or, when the tier registered its guest
/// image up front, a home holding only that registry.
fn assert_nothing_recorded(cli: &Cli) {
    let Ok(entries) = fs::read_dir(cli.home()) else {
        return;
    };
    let names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        (fake_mode() || real_mode()) && names == ["registry"],
        "the home holds {names:?}"
    );
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

    cli.cmd(&["submit", &bad])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("limit tool_actions must be > 0"));
    assert_nothing_recorded(&cli);

    let garbage = cli.write("garbage.json", "{ not json");
    cli.cmd(&["submit", &garbage])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid contract json"));
    let missing_profile = cli.write(
        "p.json",
        &fs::read_to_string(cli.contract(&repo))
            .unwrap()
            .replace("parser-checks-v1", "nope-v1"),
    );
    cli.cmd(&["submit", &missing_profile])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("verification profile nope-v1"));
    // A profile id is one plain registry name: it can never reach outside the registry.
    let valid = fs::read_to_string(cli.contract(&repo)).unwrap();
    for (i, bad) in ["../../tmp/x", "/abs/path", "a/b", "..", "", ".", "-x"]
        .into_iter()
        .enumerate()
    {
        let id = serde_json::to_string(bad).unwrap();
        let file = cli.write(
            &format!("bad-profile-{i}.json"),
            &valid.replace("\"parser-checks-v1\"", &id),
        );
        cli.cmd(&[
            "submit",
            &file,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains("verification_profile"));
    }
    let wrong_rev = cli.contract_with(&repo, 10, &Digest::of(b"another tree").to_string());
    cli.cmd(&["submit", &wrong_rev])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("revision"));
    let contract = cli.contract(&repo);
    cli.cmd(&["submit", &contract, "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--fake-agent-patch"));
    assert_nothing_recorded(&cli);
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
    assert_eq!(
        seqs,
        (1..=events.len() as u64).collect::<Vec<_>>(),
        "gapless"
    );
    let types = cli.event_types(id);
    assert_subsequence(
        &types,
        &[
            "TaskCreated",
            "Submitted",
            "Started",
            "WorkspaceUpdated",
            "WorkspaceUpdated",
            "VerifyStarted",
            "VerifyPassed",
        ],
    );
    let submitted_event = &events[1];
    assert_eq!(submitted_event["type"], "Submitted");
    let repo_digest = workspace_digest(&repo).unwrap().to_string();
    let profile_digest = workspace_digest(&fixtures().join("profiles/parser-checks-v1"))
        .unwrap()
        .to_string();
    assert_eq!(submitted_event["payload"]["repository_digest"], repo_digest);
    assert_eq!(submitted_event["payload"]["profile_id"], "parser-checks-v1");
    assert_eq!(submitted_event["payload"]["profile_digest"], profile_digest);
    if fake_mode() || real_mode() {
        // The Firecracker worker records the registered image instead of the host's label.
        assert_eq!(
            submitted_event["payload"]["guest_image_id"],
            guest_profile()
        );
        assert!(submitted_event["payload"].get("guest_image").is_none());
    } else {
        assert_eq!(
            submitted_event["payload"]["guest_image"],
            "fixture-executor-v0"
        );
    }
    assert_eq!(
        submitted_event["payload"]["contract_digest"],
        events[0]["payload"]["contract_digest"]
    );

    let (bundle, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["task_id"], id);
    assert_eq!(manifest["state"], "SUCCEEDED");
    assert_eq!(manifest["base_revision"], repo_digest);
    assert_eq!(manifest["base_workspace_digest"], repo_digest);
    assert_eq!(
        manifest["final_workspace_digest"],
        status["workspace_digest"]
    );
    assert_eq!(manifest["verified_digest"], status["verified_digest"]);
    assert_eq!(manifest["verification_profile_digest"], profile_digest);
    assert_eq!(
        manifest["contract_digest"],
        events[0]["payload"]["contract_digest"]
    );
    assert_eq!(manifest["usage_summary"], status["usage"]);
    assert_eq!(manifest["model"], "fake-agent");
    assert_eq!(
        manifest["patch_digest"],
        digest_of_file(&bundle.join("patch.diff"))
    );
    assert_eq!(
        fs::read(bundle.join("patch.diff")).unwrap(),
        fs::read(fix_patch()).unwrap()
    );
    let results = manifest["verification_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["passed"], true);
    assert_eq!(results[0]["workspace_digest"], status["verified_digest"]);
    let evidence = results[0]["evidence_digest"].as_str().unwrap();
    assert_eq!(
        digest_of_file(&bundle.join(format!("evidence/{evidence}.json"))),
        evidence
    );
    // The evidence digest is the one the journal recorded for the verification effect.
    let completed: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "EffectCompleted")
        .collect();
    assert!(
        completed
            .iter()
            .any(|e| e["payload"].to_string().contains(evidence)),
        "evidence digest is journaled"
    );

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
    assert_eq!(
        json!(workspace_digest(&ws).unwrap()),
        manifest["final_workspace_digest"]
    );
    run_ok(
        StdCommand::new("python3")
            .current_dir(&ws)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args(["-m", "unittest"]),
    );
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
    let ready = cli.json(&["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let running = cli.crash(&contract, "after-dispatch:apply_patch");
    let paused = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(cli.json(&["pause", &paused])["state"], "PAUSED");

    for (id, state) in [
        (&ready, "READY"),
        (&running, "RUNNING"),
        (&paused, "PAUSED"),
    ] {
        assert_eq!(cli.status(id)["state"], state);
        let out = cli.path(&format!("bundle-{state}"));
        cli.cmd(&["export", id, out.to_str().unwrap()])
            .assert()
            .code(1)
            .stdout("")
            .stderr(predicate::str::contains(format!(
                "task is {state}, only finished tasks can be exported"
            )));
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

    let assert = cli
        .cmd(&[
            "submit",
            &contract,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ])
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    for shown in [
        "snapshot.read",
        "workspace.apply_patch",
        "src/**",
        "parser-checks-v1",
        "tool_actions",
    ] {
        assert!(
            stderr.contains(shown),
            "permission summary shows {shown}: {stderr}"
        );
    }
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["state"], "READY");
    assert!(out["note"].as_str().unwrap().contains("agentos resume"));
    let id = out["task_id"].as_str().unwrap();
    assert_eq!(cli.status(id)["state"], "READY");
    assert_eq!(
        cli.event_types(id),
        vec!["TaskCreated", "Submitted"],
        "nothing ran before approval"
    );

    // The patch given at submission is used; no flag needed.
    let resumed = cli.json(&["resume", id]);
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }));
    assert_eq!(cli.status(id)["state"], "SUCCEEDED");
    // Resuming a finished task only reports it.
    assert_eq!(
        cli.json(&["resume", id]),
        json!({ "task_id": id, "state": "SUCCEEDED" })
    );
}

#[test]
fn resume_without_any_agent_patch_is_a_usage_error() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.json(&["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.cmd(&["resume", &id])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--fake-agent-patch"));
    assert_eq!(cli.status(&id)["state"], "READY");
    let done = cli.json(&[
        "resume",
        &id,
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ]);
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
    assert_tier_worker(&cli, &id);
    let stuck = cli.status(&id);
    assert!(
        !TERMINAL.contains(&stuck["state"].as_str().unwrap()),
        "{spec}: crashed task is unfinished: {stuck}"
    );
    assert_crashed_on_kind(&cli, &id, spec, &stuck);

    let resumed = cli.json(&["resume", &id]);
    assert_eq!(
        resumed,
        json!({ "task_id": id, "state": "SUCCEEDED" }),
        "{spec}"
    );
    let status = cli.status(&id);
    assert_eq!(status["verified_digest"], status["workspace_digest"]);
    assert_eq!(status["outstanding_effects"], json!([]));
    let (_, manifest) = cli.export(&id, "recovered");
    assert_eq!(manifest["task_id"], id.as_str());
    assert_eq!(
        normalized(&manifest),
        normalized(&expected),
        "{spec}: recovered bundle equals the uncrashed one"
    );
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
    let outstanding: Vec<&str> = status["outstanding_effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    if point == "after-agent-turn-journaled" {
        let last = events.last().unwrap();
        assert_eq!(last["type"], "AgentTurn", "{spec}");
        let action = &last["payload"]["action"];
        assert!(
            action == variant
                || action.get(variant).is_some()
                || (kind == "run_verification" && action == "Verify"),
            "{spec}: {action}"
        );
        assert!(outstanding.is_empty(), "{spec}: {outstanding:?}");
        return;
    }
    let intended = events
        .iter()
        .rev()
        .find(|e| e["type"] == "EffectIntended")
        .expect("an effect was intended");
    let k = &intended["payload"]["kind"];
    assert!(
        k == variant || k.get(variant).is_some(),
        "{spec}: last intended effect is {k}"
    );
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
    let covered: BTreeSet<String> = DEMO
        .iter()
        .map(|s| s.split(':').next().unwrap().to_string())
        .collect();
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
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
        "--crash-at",
        "after-dispatch:apply_patch:2",
    ]);
    assert_eq!(out["state"], "SUCCEEDED");
    for bad in [
        "nowhere",
        "after-dispatch:teleport",
        "after-dispatch:apply_patch:0",
        "after-dispatch:1:2",
    ] {
        cli.cmd(&[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
            "--crash-at",
            bad,
        ])
        .assert()
        .code(2);
    }
}

#[test]
fn pause_then_resume() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json(&["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.cmd(&["pause", &ready])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("READY"));

    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(cli.status(&id)["state"], "RUNNING");
    assert_eq!(
        cli.json(&["pause", &id]),
        json!({ "task_id": id, "state": "PAUSED" })
    );
    assert_eq!(
        cli.json(&["pause", &id])["state"],
        "PAUSED",
        "pausing twice is harmless"
    );
    let status = cli.status(&id);
    assert_eq!(status["state"], "PAUSED");
    assert_eq!(
        status["outstanding_effects"].as_array().unwrap().len(),
        1,
        "the in-flight patch waits for the resume"
    );

    assert_eq!(cli.json(&["resume", &id])["state"], "SUCCEEDED");
    assert_subsequence(
        &cli.event_types(&id),
        &[
            "Started",
            "Paused",
            "Resumed",
            "RecoveryDecision",
            "VerifyPassed",
        ],
    );
    assert_eq!(cli.status(&id)["outstanding_effects"], json!([]));
    // The new session's agent re-sent its patch, which no longer applied (a FAILED effect):
    // only the patch that really applied is in the bundle.
    assert!(cli.event_types(&id).contains(&"EffectFailed".to_string()));
    let (bundle, manifest) = cli.export(&id, "bundle");
    assert_eq!(
        fs::read(bundle.join("patch.diff")).unwrap(),
        fs::read(fix_patch()).unwrap()
    );
    assert_eq!(manifest["patches"].as_array().unwrap().len(), 1);
}

#[test]
fn cancel_after_a_crash_reconciles_the_in_flight_effect_and_cancels() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    assert_eq!(
        cli.status(&id)["outstanding_effects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let out = cli.json(&["cancel", &id]);
    assert_eq!(out["task_id"], id.as_str());
    assert_eq!(out["state"], "CANCELLED");

    let status = cli.status(&id);
    assert_eq!(status["state"], "CANCELLED");
    assert_eq!(status["cancel_requested"], true);
    assert_eq!(status["verified_digest"], Value::Null);
    assert_eq!(
        status["outstanding_effects"],
        json!([]),
        "no leaked effects"
    );
    assert_eq!(status["usage"]["uncertain_tool_actions"], 0);
    let events = cli.events(&id);
    let decision = events
        .iter()
        .find(|e| e["type"] == "RecoveryDecision")
        .expect("the in-flight patch was decided");
    assert_eq!(decision["payload"]["kind"], "apply_patch");
    assert_eq!(
        decision["payload"]["decision"], "Abandon",
        "the patch never ran: reconciliation proves it"
    );
    assert_subsequence(
        &cli.event_types(&id),
        &["CancelRequested", "RecoveryDecision", "CancelCompleted"],
    );

    // Cancelling again only reports; a READY task cancels at once.
    assert_eq!(cli.json(&["cancel", &id])["state"], "CANCELLED");
    let ready = cli.json(&["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.json(&["cancel", &ready])["state"], "CANCELLED");
    assert_eq!(
        cli.event_types(&ready),
        vec![
            "TaskCreated",
            "Submitted",
            "CancelRequested",
            "CancelCompleted"
        ]
    );

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
        cli.cmd(&args)
            .assert()
            .code(1)
            .stdout("")
            .stderr(predicate::str::contains(format!("unknown task {unknown}")));
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
    assert_eq!(
        fs::read_to_string(bundle.join("patch.diff")).unwrap(),
        COMMENT_PATCH,
        "the applied patch, honestly"
    );
    assert!(!manifest.to_string().contains("SUCCEEDED"));
}

#[test]
fn while_another_process_drives_cancel_only_requests_and_resume_waits() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.crash(&contract, "after-dispatch:apply_patch");
    // Stand in for a live driver: hold the home's driver lock in this process.
    let lock = fs::File::options()
        .write(true)
        .open(cli.home().join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    // As every real holder does on acquiring it: forget what a previous (dead) holder drove.
    lock.set_len(0).unwrap();

    let out = cli.json(&["cancel", &id]);
    assert_eq!(out["state"], "RUNNING");
    assert_eq!(out["cancel_requested"], true);
    let note = out["note"].as_str().unwrap();
    assert!(
        note.contains(&format!(
            "next `agentos resume {id}` or `agentos cancel {id}`"
        )),
        "{note}"
    );
    // When the lock holder says it drives this very task, its runner completes the cancel.
    fs::write(cli.home().join("driver.lock"), &id).unwrap();
    let note = cli.json(&["cancel", &id])["note"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        note.contains("is driving this task; it completes the cancel at its next step"),
        "{note}"
    );
    fs::write(cli.home().join("driver.lock"), "").unwrap();
    cli.cmd(&["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "another agentos process is driving",
        ));
    let status = cli.status(&id);
    assert_eq!(status["state"], "RUNNING");
    assert_eq!(
        status["outstanding_effects"].as_array().unwrap().len(),
        1,
        "nothing was recovered under someone else's lock"
    );

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
    let assert = cli
        .cmd(&["resume", &id, "--crash-at", "after-dispatch:apply_patch"])
        .assert()
        .code(75);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    assert!(
        stderr.contains(&format!(
            r#"{{"crashed":"after-dispatch","task_id":"{id}"}}"#
        )),
        "{stderr}"
    );
    assert!(
        !stderr.contains('\u{1b}'),
        "no terminal colours when stderr is not a terminal"
    );
    assert_eq!(cli.status(&id)["state"], "RUNNING");

    assert_eq!(cli.json(&["resume", &id])["state"], "SUCCEEDED");
    let decisions: Vec<Value> = cli
        .events(&id)
        .into_iter()
        .filter(|e| e["type"] == "RecoveryDecision")
        .map(|e| e["payload"].clone())
        .collect();
    assert_eq!(
        decisions.len(),
        2,
        "one decision per restart: {decisions:?}"
    );
    assert!(
        decisions
            .iter()
            .all(|d| d["kind"] == "apply_patch" && d["decision"] == "Redispatch")
    );
    assert_eq!(
        decisions[1]["lease_generation"], 2,
        "the second restart found the second lease in flight"
    );
}

#[test]
fn a_registry_entry_that_links_outside_the_registry_is_refused() {
    let cli = Cli::new();
    let registry = cli.path("profiles");
    copy_tree(
        &fixtures().join("profiles/parser-checks-v1"),
        &registry.join("parser-checks-v1"),
    )
    .unwrap();
    let outside = cli.path("outside");
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, registry.join("evil")).unwrap();
    let contract = cli.contract(&cli.repo_copy());
    let evil = cli.write(
        "evil.json",
        &fs::read_to_string(&contract)
            .unwrap()
            .replace("\"parser-checks-v1\"", "\"evil\""),
    );

    cli.cmd_with_profiles(&registry, &["submit", &evil])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("outside the profile registry"));
    assert_nothing_recorded(&cli);
    // A real registry entry still works.
    let out = cli
        .cmd_with_profiles(
            &registry,
            &[
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .assert()
        .success();
    let out: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(out["state"], "SUCCEEDED");
}

#[test]
fn every_export_is_journaled_and_a_refused_one_is_not() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let exported = |cli: &Cli| -> Vec<Value> {
        cli.events(&id)
            .into_iter()
            .filter(|e| e["type"] == "Exported")
            .map(|e| e["payload"].clone())
            .collect()
    };

    let (bundle, _) = cli.export(&id, "one");
    let events = exported(&cli);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]["manifest_digest"],
        digest_of_file(&bundle.join("manifest.json"))
    );
    assert_eq!(events[0]["dir"], bundle.to_str().unwrap());
    assert_eq!(
        events[0]["files"], 5,
        "manifest, patch.diff, one patch, two evidence files"
    );

    cli.cmd(&["export", &id, bundle.to_str().unwrap()])
        .assert()
        .code(1);
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
    fs::write(
        cli.home()
            .join("tasks")
            .join(&id)
            .join("snapshot/src/parser.py"),
        "tampered = True\n",
    )
    .unwrap();

    let resumed = cli.json(&["resume", &id]);

    assert_eq!(resumed["state"], "FAILED");
    let status = cli.status(&id);
    let failed = cli
        .events(&id)
        .into_iter()
        .find(|e| e["type"] == "Failed")
        .unwrap();
    assert!(
        failed["payload"]["Failed"]["reason"]
            .as_str()
            .unwrap()
            .contains("recorded snapshot changed"),
        "{failed}"
    );
    // The in-flight snapshot was reconciled, not left DISPATCHED with a live reservation.
    let outstanding = status["outstanding_effects"].as_array().unwrap();
    assert!(
        outstanding
            .iter()
            .all(|e| e["state"] != "Dispatched" && e["state"] != "Intended"),
        "{status}"
    );
    assert_eq!(status["usage"]["reserved_tool_actions"], 0, "{status}");
    assert_eq!(
        status["usage"]["uncertain_tool_actions"], 1,
        "it may have run: the reservation stays visible"
    );
    assert!(
        cli.events(&id)
            .iter()
            .any(|e| e["type"] == "RecoveryDecision")
    );
    // Nothing changes on a further resume.
    let n = cli.events(&id).len();
    assert_eq!(cli.json(&["resume", &id])["state"], "FAILED");
    assert_eq!(cli.events(&id).len(), n);
}

impl Cli {
    /// The task's capability grants, read straight from the home's store.
    fn grants(&self, id: &str) -> Vec<agentos_core::broker::CapabilityGrant> {
        let db = agentos_store::db::Db::open(&self.home().join("agentos.db")).unwrap();
        db.grants(&serde_json::from_value(json!(id)).unwrap())
            .unwrap()
    }

    fn deadline_ts(&self, id: &str) -> i64 {
        let db = agentos_store::db::Db::open(&self.home().join("agentos.db")).unwrap();
        db.deadline_ts(&serde_json::from_value(json!(id)).unwrap())
            .unwrap()
    }
}

#[test]
fn submit_without_yes_issues_no_handles_and_resume_approves() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let out = cli.json(&[
        "submit",
        &contract,
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ]);
    let id = out["task_id"].as_str().unwrap();
    assert!(cli.grants(id).is_empty(), "no handles before approval");
    assert_eq!(cli.deadline_ts(id), 0, "the deadline has not started");
    assert!(
        !cli.event_types(id)
            .contains(&"CapabilitiesIssued".to_string())
    );

    assert_eq!(cli.json(&["resume", id])["state"], "SUCCEEDED");
    let grants = cli.grants(id);
    assert_eq!(grants.len(), 4, "one handle per contract capability");
    assert!(cli.deadline_ts(id) > 0);
    let types = cli.event_types(id);
    assert_eq!(
        types.iter().filter(|t| *t == "CapabilitiesIssued").count(),
        1
    );
    assert_subsequence(
        &types,
        &["TaskCreated", "Submitted", "CapabilitiesIssued", "Started"],
    );
    // The journal shows prefixes only.
    let printed = String::from_utf8(
        cli.cmd(&["events", id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    for g in &grants {
        assert!(printed.contains(g.handle.prefix()));
        assert!(
            !printed.contains(&g.handle.to_string()),
            "full handle in `agentos events`"
        );
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
    assert_eq!(
        types.iter().filter(|t| *t == "CapabilitiesIssued").count(),
        1
    );
    assert_subsequence(
        &types,
        &[
            "TaskCreated",
            "Submitted",
            "CapabilitiesIssued",
            "Started",
            "CapabilityGranted",
            "EffectIntended",
        ],
    );
}

/// Job directories under the home, as (effect id, directory name): names are
/// `<effect_id>-<attempt_id>` and effect ids are 64 hex digits.
fn job_dirs(cli: &Cli) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(cli.home().join("jobs")) else {
        return Vec::new();
    };
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
    assert_eq!(
        effects.len(),
        dirs.len(),
        "{what}: more than one job for an effect: {dirs:?}"
    );
}

/// Pids of processes whose command line mentions `needle`.
fn processes_mentioning(needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&cmdline).contains(needle) {
            found.push(name);
        }
    }
    found
}

#[test]
fn supervise_subcommands_are_hidden_from_help() {
    let cli = Cli::new();
    let out = cli
        .cmd(&["--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!String::from_utf8_lossy(&out).contains("supervise"));
}

#[test]
fn home_has_no_receipts_dir() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    assert_eq!(
        cli.submit_yes(&contract, &fix_patch())["state"],
        "SUCCEEDED"
    );
    assert!(!cli.home().join("receipts").exists());
    assert!(cli.home().join("jobs").is_dir());
    assert_one_job_per_effect(&cli, "clean run");
}

#[test]
fn during_execute_rows_resume_by_publishing_the_receipt_with_one_job_per_effect() {
    for spec in [
        "during-execute:apply_patch",
        "during-execute:run_verification",
    ] {
        let cli = Cli::new();
        let contract = cli.contract(&fixtures().join("parser-repo"));
        let clean = cli.submit_yes(&contract, &fix_patch());
        let (_, expected) = cli.export(clean["task_id"].as_str().unwrap(), "clean");
        let before = job_dirs(&cli).len();

        let id = cli.crash(&contract, spec);
        assert_tier_worker(&cli, &id);
        assert!(
            job_dirs(&cli).len() > before,
            "{spec}: the crashed run launched its job"
        );
        assert_eq!(
            cli.json(&["resume", &id]),
            json!({ "task_id": id, "state": "SUCCEEDED" }),
            "{spec}"
        );
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
    assert!(
        jobs[0]["alive"] == true || jobs[0]["receipt"] == true,
        "{status}"
    );
    // A job that has not written its first status yet has no state; a finished one has.
    assert!(
        jobs[0]["state"].is_string() || jobs[0]["receipt"] == false,
        "{status}"
    );
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
    fs::write(
        slow.join("check_parser.py"),
        format!("import time\ntime.sleep(30)\n{script}"),
    )
    .unwrap();
    fs::write(slow.join("profile.json"), r#"{ "id": "slow-checks-v1", "command": ["python3", "check_parser.py"], "protected": true }"#).unwrap();
    let contract = cli.write(
        "slow.json",
        &fs::read_to_string(cli.contract(&cli.repo_copy()))
            .unwrap()
            .replace("parser-checks-v1", "slow-checks-v1"),
    );
    (profiles, contract)
}

/// `submit --yes` as a background process.
fn spawn_submit(cli: &Cli, profiles: &Path, contract: &str) -> std::process::Child {
    cli.std_cmd(
        Cli::mode(),
        true,
        profiles,
        &[
            "submit",
            contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )
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
        let up = fs::read_dir(cli.home().join("jobs"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| {
                e.path().join("status.json").is_file()
                    && fs::read_to_string(e.path().join("request.json"))
                        .is_ok_and(|r| r.contains("RunVerification"))
            });
        if up {
            return;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "the verification job never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// No supervisor, worker, check or (fake) guest is left: each names the home on its command
/// line (`<home>/jobs/…`, or the fake guest's `<home>/work/<task>`).
fn assert_no_job_processes(cli: &Cli) {
    assert_eq!(
        processes_mentioning(cli.home().to_str().unwrap()),
        Vec::<String>::new(),
        "no supervisor, worker or guest left"
    );
    assert_eq!(
        home_vms(cli),
        Vec::<String>::new(),
        "no Firecracker of this home left"
    );
}

/// Live `firecracker` processes of this home (matched by `--id`: a jailed one's command
/// line names no host path).
fn home_vms(cli: &Cli) -> Vec<String> {
    procs::home_firecrackers(&cli.home())
        .into_iter()
        .map(|p| p.cmdline.join(" "))
        .collect()
}

#[test]
fn controller_sigkill_while_a_slow_verification_runs_then_resume_publishes_the_receipt() {
    let cli = Cli::new();
    let (profiles, contract) = slow_world(&cli);
    // Shorten the check: the 30 s sleep becomes 3 s.
    let script = profiles.join("slow-checks-v1/check_parser.py");
    fs::write(
        &script,
        fs::read_to_string(&script)
            .unwrap()
            .replace("sleep(30)", "sleep(3)"),
    )
    .unwrap();
    let mut child = spawn_submit(&cli, &profiles, &contract);
    wait_for_verification_job(&cli);
    child.kill().unwrap();
    child.wait().unwrap();

    let id = first_task(&cli);
    assert_tier_worker(&cli, &id);
    let resumed = cli
        .cmd_with_profiles(&profiles, &["resume", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&resumed).unwrap(),
        json!({ "task_id": id, "state": "SUCCEEDED" })
    );
    let verifications = job_dirs(&cli)
        .into_iter()
        .filter(|(_, name)| {
            fs::read_to_string(cli.home().join("jobs").join(name).join("request.json"))
                .is_ok_and(|r| r.contains("RunVerification"))
        })
        .count();
    assert_eq!(
        verifications, 1,
        "exactly one job for the verification effect"
    );
    assert_one_job_per_effect(&cli, "after sigkill");
    assert_no_job_processes(&cli);
}

#[test]
fn revoke_unknown_capability_name_exits_2() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let done = cli.submit_yes(&contract, &fix_patch());
    let id = done["task_id"].as_str().unwrap();
    cli.cmd(&["revoke", id, "--capability", "teleport.now"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown capability"));
}

#[test]
fn revoke_on_a_terminal_task_is_allowed_and_cancels_nothing() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let done = cli.submit_yes(&contract, &fix_patch());
    let id = done["task_id"].as_str().unwrap();
    let out = cli.json(&["revoke", id, "--capability", "verification.run"]);
    assert_eq!(
        out,
        json!({ "task_id": id, "revoked": ["verification.run"], "cancelled_jobs": 0 })
    );
    // Revoking again changes nothing.
    assert_eq!(
        cli.json(&["revoke", id, "--capability", "verification.run"])["revoked"],
        json!([])
    );
    assert_eq!(cli.status(id)["state"], "SUCCEEDED");
    assert!(
        cli.event_types(id)
            .contains(&"CapabilityRevoked".to_string())
    );
    cli.cmd(&["revoke", "00000000-0000-4000-8000-000000000000"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown task"));
}

#[test]
fn revoke_verification_run_stops_the_running_check_from_another_process() {
    let cli = Cli::new();
    let (profiles, contract) = slow_world(&cli);
    let started = std::time::Instant::now();
    let mut child = spawn_submit(&cli, &profiles, &contract);
    wait_for_verification_job(&cli);
    let id = first_task(&cli);
    assert_tier_worker(&cli, &id);
    let out = cli
        .cmd_with_profiles(
            &profiles,
            &["revoke", &id, "--capability", "verification.run"],
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap(),
        json!({ "task_id": id, "revoked": ["verification.run"], "cancelled_jobs": 1 })
    );
    assert!(
        child.wait().unwrap().success(),
        "the driver finished the task"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "the 30 s check was stopped, took {:?}",
        started.elapsed()
    );
    assert_eq!(cli.status(&id)["state"], "FAILED");
    let events = cli.events(&id);
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "EffectFailed" && e["payload"].to_string().contains("cancelled")),
        "the killed check is a recorded failure"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "EffectCompleted")
            .count(),
        2,
        "earlier results stay"
    );
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
    assert_tier_worker(&cli, &id);
    // Another process drives the task: the cancel is only requested, but the running job is
    // told to stop at once, so the driver does not wait out the check.
    let out = cli
        .cmd_with_profiles(&profiles, &["cancel", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap()["cancel_requested"],
        true
    );
    child.wait().unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(cli.status(&id)["state"], "CANCELLED");
    assert_no_job_processes(&cli);
}

/// The id of the only task in the home.
fn first_task(cli: &Cli) -> String {
    let mut ids: Vec<String> = fs::read_dir(cli.home().join("tasks"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    ids.remove(0)
}

/// A profile directory in the scratch dir: the fixture's parser check with `check_prefix`
/// prepended (so its bytes, and digest, differ), registered under `id`.
fn profile_variant(cli: &Cli, name: &str, id: &str, check_prefix: &str) -> PathBuf {
    let dir = cli.path(name);
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &dir).unwrap();
    let script = fs::read_to_string(dir.join("check_parser.py")).unwrap();
    fs::write(
        dir.join("check_parser.py"),
        format!("{check_prefix}{script}"),
    )
    .unwrap();
    fs::write(
        dir.join("profile.json"),
        json!({ "id": id, "command": ["python3", "check_parser.py"], "protected": true })
            .to_string(),
    )
    .unwrap();
    dir
}

/// The profile registry's entries (`<home>/registry/*`; the image registry `images/` beside
/// them is not one).
fn profile_registry_entries(cli: &Cli) -> Vec<String> {
    let Ok(entries) = fs::read_dir(cli.home().join("registry")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "images")
        .collect()
}

fn register(cli: &Cli, dir: &Path) -> Value {
    cli.json(&["profile", "register", dir.to_str().unwrap()])
}

/// A contract over a repository copy, naming `verification_profile` and optionally pinning it.
fn contract_for_profile(cli: &Cli, id: &str, pin: Option<&str>) -> String {
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    contract["verification_profile"] = json!(id);
    if let Some(pin) = pin {
        contract["profile_digest"] = json!(pin);
    }
    cli.write(
        &format!(
            "pinned-{}.json",
            Digest::of(contract.to_string().as_bytes())
        ),
        &contract.to_string(),
    )
}

/// `--profiles` pointing at an empty directory: only the registry can supply a profile.
fn no_legacy(cli: &Cli) -> PathBuf {
    let dir = cli.path("no-legacy");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn submitted_profile_digest(cli: &Cli, id: &str) -> String {
    cli.events(id)
        .iter()
        .find(|e| e["type"] == "Submitted")
        .unwrap()["payload"]["profile_digest"]
        .as_str()
        .unwrap()
        .to_string()
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
    let digest = register(&cli, &profile_variant(&cli, "p1", "ro-v1", ""))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let entry = cli.home().join("registry").join(format!("ro-v1@{digest}"));
    for path in [
        entry.clone(),
        entry.join("profile.json"),
        entry.join("check_parser.py"),
    ] {
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o222, 0, "{} is writable: {mode:o}", path.display());
    }
    assert!(
        cli.home()
            .join("registry")
            .join(format!("ro-v1@{digest}.meta.json"))
            .is_file()
    );
}

#[test]
fn ids_with_at_sign_or_traversal_are_rejected_at_register() {
    let cli = Cli::new();
    for (i, id) in ["a@b", "../x", "a/b", "..", "", "-x"]
        .into_iter()
        .enumerate()
    {
        let dir = profile_variant(&cli, &format!("bad-{i}"), id, "");
        cli.cmd(&["profile", "register", dir.to_str().unwrap()])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("plain name"));
    }
    assert_eq!(profile_registry_entries(&cli), Vec::<String>::new());
    let empty = cli.path("empty-command");
    fs::create_dir_all(&empty).unwrap();
    fs::write(empty.join("profile.json"), r#"{"id":"e-v1","command":[]}"#).unwrap();
    cli.cmd(&["profile", "register", empty.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("command"));
}

/// The guest protocol carries file contents without modes, so a command that runs a file from
/// the profile directory directly could never execute in a VM: registration refuses it and
/// names the interpreter form. Interpreter commands and absolute guest paths are accepted.
#[test]
fn commands_that_need_an_executable_bit_are_rejected_at_register() {
    let cli = Cli::new();
    let profile = |name: &str, command: Value| {
        let dir = cli.path(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("check.sh"), "exit 0\n").unwrap();
        fs::write(
            dir.join("profile.json"),
            json!({ "id": name, "command": command }).to_string(),
        )
        .unwrap();
        dir
    };
    for (name, command) in [
        ("dot-slash-v1", json!(["./check.sh"])),
        ("nested-v1", json!(["bin/check.sh", "--fast"])),
        ("bare-file-v1", json!(["check.sh"])),
    ] {
        cli.cmd(&[
            "profile",
            "register",
            profile(name, command).to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("file modes"))
        .stderr(predicate::str::contains("[\"sh\", \"check.sh\"]"));
    }
    assert_eq!(profile_registry_entries(&cli), Vec::<String>::new());
    for (name, command) in [
        ("sh-v1", json!(["sh", "check.sh"])),
        ("python-v1", json!(["python3", "check.py"])),
        ("absolute-v1", json!(["/usr/bin/true"])),
    ] {
        assert_eq!(register(&cli, &profile(name, command))["id"], name);
    }
    let mut ids: Vec<String> = cli
        .json(&["profile", "list"])
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["absolute-v1", "python-v1", "sh-v1"]);
}

#[test]
fn registered_profile_runs_end_to_end() {
    let cli = Cli::new();
    let digest = register(&cli, &fixtures().join("profiles/parser-checks-v1"))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let contract = contract_for_profile(&cli, "parser-checks-v1", None);
    let out = cli.json_with_profiles(
        &no_legacy(&cli),
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(
        submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()),
        digest
    );
}

#[test]
fn submit_with_a_pin_for_a_missing_digest_exits_2() {
    let cli = Cli::new();
    register(&cli, &fixtures().join("profiles/parser-checks-v1"));
    let contract = contract_for_profile(&cli, "parser-checks-v1", Some(&"0".repeat(64)));
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("is not in the registry"));
}

#[test]
fn submit_with_the_pin_uses_exactly_that_digest_even_if_a_newer_entry_exists() {
    let cli = Cli::new();
    let older = register(&cli, &profile_variant(&cli, "p1", "pin-v1", ""))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    // Registration times are milliseconds: make sure the second entry is strictly newer.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer = register(&cli, &profile_variant(&cli, "p2", "pin-v1", "# newer\n"))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(older, newer);

    let pinned = contract_for_profile(&cli, "pin-v1", Some(&older));
    let out = cli.json_with_profiles(
        &no_legacy(&cli),
        &[
            "submit",
            &pinned,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    assert_eq!(
        submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()),
        older
    );
    let unpinned = contract_for_profile(&cli, "pin-v1", None);
    let out = cli.json_with_profiles(
        &no_legacy(&cli),
        &[
            "submit",
            &unpinned,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    assert_eq!(
        submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()),
        newer,
        "no pin: the newest entry"
    );
}

#[test]
fn legacy_profiles_dir_still_works_with_the_profiles_flag() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    assert_eq!(
        profile_registry_entries(&cli),
        Vec::<String>::new(),
        "no registered profile"
    );
    assert_eq!(
        cli.submit_yes(&contract, &fix_patch())["state"],
        "SUCCEEDED"
    );
}

#[test]
fn registry_wins_over_legacy_when_both_exist() {
    let cli = Cli::new();
    // The registry's parser-checks-v1 rejects everything; the legacy one (the fixture) is right.
    let strict = profile_variant(&cli, "p1", "parser-checks-v1", "import sys\nsys.exit(1)\n");
    let digest = register(&cli, &strict)["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let contract = contract_for_profile(&cli, "parser-checks-v1", None);
    let out = cli.json(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ]);
    assert_eq!(
        out["state"], "FAILED",
        "the registry entry was used, not the legacy directory"
    );
    assert_eq!(
        submitted_profile_digest(&cli, out["task_id"].as_str().unwrap()),
        digest
    );
}

#[test]
fn cli_tampered_staged_profile_fails_the_task_before_any_verification() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json(&[
        "submit",
        &contract,
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ]);
    let id = ready["task_id"].as_str().unwrap();
    let staged = cli
        .home()
        .join("tasks")
        .join(id)
        .join("profile/check_parser.py");
    fs::write(&staged, "import sys\nsys.exit(0)\n").unwrap();

    assert_eq!(cli.json(&["resume", id])["state"], "FAILED");
    let events = cli.events(id);
    let reason = events.iter().find(|e| e["type"] == "Failed").unwrap()["payload"].to_string();
    assert!(reason.contains("recorded profile changed"), "{reason}");
    assert!(
        !events.iter().any(|e| e["type"] == "EffectIntended"),
        "nothing ran on the tampered profile"
    );
    assert!(job_dirs(&cli).is_empty());
}

/// Every full handle of the task, read from the database (they appear nowhere else).
fn full_handles(cli: &Cli, id: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(cli.home().join("agentos.db")).unwrap();
    let mut stmt = conn
        .prepare("SELECT id FROM capabilities WHERE task_id = ?1")
        .unwrap();
    stmt.query_map([id], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn denials(cli: &Cli, id: &str) -> Vec<Value> {
    cli.events(id)
        .into_iter()
        .filter(|e| e["type"] == "CapabilityDenied")
        .map(|e| e["payload"].clone())
        .collect()
}

#[test]
fn export_journals_the_granted_decision() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.export(&id, "bundle");
    let granted: Vec<Value> = cli
        .events(&id)
        .into_iter()
        .filter(|e| {
            e["type"] == "CapabilityGranted" && e["payload"]["operation"] == "artifact.export"
        })
        .collect();
    assert_eq!(granted.len(), 1, "{granted:?}");
    assert!(
        granted[0]["payload"]["handle_prefix"]
            .as_str()
            .unwrap()
            .len()
            == 8
    );
}

#[test]
fn export_without_artifact_export_capability_exits_1_writes_nothing_and_journals_the_denial() {
    let cli = Cli::new();
    let repo = cli.repo_copy();
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&repo)).unwrap()).unwrap();
    contract["capabilities"] =
        json!(["snapshot.read", "workspace.apply_patch", "verification.run"]);
    let file = cli.write("no-export.json", &contract.to_string());
    let id = cli.submit_yes(&file, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = cli.path("bundle");
    cli.cmd(&["export", &id, dir.to_str().unwrap()])
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("export denied"));
    assert!(!dir.exists(), "nothing written");
    let denied = denials(&cli, &id);
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(
        (
            denied[0]["operation"].as_str(),
            denied[0]["reason"].as_str()
        ),
        (Some("artifact.export"), Some("unknown_handle"))
    );
    assert!(!cli.event_types(&id).contains(&"Exported".to_string()));
}

#[test]
fn export_after_revoking_artifact_export_is_denied_revoked() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.json(&["revoke", &id, "--capability", "artifact.export"]);
    let dir = cli.path("bundle");
    cli.cmd(&["export", &id, dir.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("revoked"));
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
    let out = cli.json_with_profiles(
        &profiles,
        &[
            "submit",
            &file,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    assert_eq!(out["state"], "FAILED");
    let id = out["task_id"].as_str().unwrap();
    let failed = cli
        .events(id)
        .into_iter()
        .find(|e| e["type"] == "Failed")
        .unwrap();
    assert!(
        failed["payload"].to_string().contains("deadline exceeded"),
        "{failed}"
    );
    assert_no_job_processes(&cli);
    let (_, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["state"], "FAILED");
    assert!(manifest["verified_digest"].is_null(), "no success claim");
}

#[test]
fn manifest_lists_capabilities_with_prefixes_only() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let (dir, manifest) = cli.export(&id, "bundle");
    let caps = manifest["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), 4);
    assert!(
        caps.iter()
            .all(|c| c["handle_prefix"].as_str().unwrap().len() == 8 && c["revoked"] == false)
    );
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
        assert!(
            !everything.contains(handle.as_str()),
            "a full handle leaked"
        );
        assert!(everything.contains(&handle[..8]), "its prefix is shown");
    }
}

#[test]
fn status_lists_capabilities_without_full_handles() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.submit_yes(&contract, &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.json(&["revoke", &id, "--capability", "verification.run"]);
    let status = cli.status(&id);
    let caps = status["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), 4);
    let revoked: Vec<&str> = caps
        .iter()
        .filter(|c| c["revoked"] == true)
        .map(|c| c["operation"].as_str().unwrap())
        .collect();
    assert_eq!(revoked, vec!["verification.run"]);
    let export = caps
        .iter()
        .find(|c| c["operation"] == "artifact.export")
        .unwrap();
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

// ---- guest image registry ----

/// A dummy guest image (the Task 5 shape: `image.json` plus 16-byte `vmlinux` and
/// `rootfs.squashfs`) in the scratch dir, named `name`, with manifest id `id`.
fn fake_image_dir(cli: &Cli, name: &str, id: &str, rootfs_byte: u8) -> PathBuf {
    let dir = cli.path(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("image.json"),
        json!({
            "id": id, "protocol": 1, "kernel": "vmlinux", "rootfs": "rootfs.squashfs", "agent_version": "0.1.0",
            "kernel_sha256": "0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447", "built_from": "test",
        })
        .to_string(),
    )
    .unwrap();
    fs::write(dir.join("vmlinux"), [0x7fu8; 16]).unwrap();
    fs::write(dir.join("rootfs.squashfs"), [rootfs_byte; 16]).unwrap();
    dir
}

fn register_image(cli: &Cli, dir: &Path) -> Value {
    cli.json(&["image", "register", dir.to_str().unwrap()])
}

#[test]
fn image_register_twice_is_a_noop_and_changed_bytes_are_a_new_entry() {
    let cli = Cli::bare();
    let dir = fake_image_dir(&cli, "i1", "img-v1", 0x68);
    let first = register_image(&cli, &dir);
    assert_eq!(first["id"], "img-v1");
    assert_eq!(first["digest"].as_str().unwrap().len(), 64);
    assert_eq!(register_image(&cli, &dir), first, "same bytes, same entry");
    assert_eq!(cli.json(&["image", "list"]).as_array().unwrap().len(), 1);
    let second = register_image(&cli, &fake_image_dir(&cli, "i2", "img-v1", 0x69));
    assert_ne!(second["digest"], first["digest"]);
    assert_eq!(cli.json(&["image", "list"]).as_array().unwrap().len(), 2);
    let entry = cli
        .home()
        .join("registry/images")
        .join(format!("img-v1@{}", first["digest"].as_str().unwrap()));
    assert!(entry.join("vmlinux").is_file() && entry.join("image.json").is_file());
}

#[test]
fn image_register_refuses_a_bad_manifest() {
    let cli = Cli::bare();
    let dir = fake_image_dir(&cli, "i1", "img-v1", 0x68);
    let manifest = fs::read_to_string(dir.join("image.json")).unwrap();
    fs::write(
        dir.join("image.json"),
        manifest.replace("\"protocol\":1", "\"protocol\":2"),
    )
    .unwrap();
    cli.cmd(&["image", "register", dir.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("protocol"));

    let dir = fake_image_dir(&cli, "i2", "img-v1", 0x68);
    fs::remove_file(dir.join("vmlinux")).unwrap();
    cli.cmd(&["image", "register", dir.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("vmlinux"));

    cli.cmd(&["image", "register", cli.path("nope").to_str().unwrap()])
        .assert()
        .code(2);
    assert!(
        !cli.home().join("registry/images").exists()
            || cli.json(&["image", "list"]).as_array().unwrap().is_empty()
    );
}

#[test]
fn image_ids_with_at_sign_or_traversal_are_rejected_at_register() {
    let cli = Cli::bare();
    for (i, id) in ["a@b", "../x", "a/b", "..", "", "-x"]
        .into_iter()
        .enumerate()
    {
        let dir = fake_image_dir(&cli, &format!("bad-{i}"), id, 0x68);
        cli.cmd(&["image", "register", dir.to_str().unwrap()])
            .assert()
            .code(2)
            .stderr(predicate::str::contains("plain name"));
    }
    assert!(cli.json(&["image", "list"]).as_array().unwrap().is_empty());
}

#[test]
fn registered_images_have_no_write_bits() {
    use std::os::unix::fs::PermissionsExt;
    let cli = Cli::bare();
    let digest = register_image(&cli, &fake_image_dir(&cli, "i1", "ro-v1", 0x68))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let entry = cli
        .home()
        .join("registry/images")
        .join(format!("ro-v1@{digest}"));
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&entry), 0o555);
    for file in ["image.json", "vmlinux", "rootfs.squashfs"] {
        assert_eq!(mode(&entry.join(file)), 0o444, "{file}");
    }
    assert!(
        cli.home()
            .join("registry/images")
            .join(format!("ro-v1@{digest}.meta.json"))
            .is_file()
    );
}

#[test]
fn image_list_shows_entries_sorted() {
    let cli = Cli::bare();
    register_image(&cli, &fake_image_dir(&cli, "i1", "zeta-v1", 0x68));
    register_image(&cli, &fake_image_dir(&cli, "i2", "alpha-v1", 0x68));
    let listed = cli.json(&["image", "list"]);
    let rows = listed.as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha-v1", "zeta-v1"]
    );
    for row in rows {
        assert_eq!(row["digest"].as_str().unwrap().len(), 64);
        assert!(row["registered_ms"].as_i64().unwrap() > 0);
        assert_eq!(row.as_object().unwrap().len(), 3);
    }
}

#[test]
fn images_do_not_leak_into_the_profile_list_and_back() {
    let cli = Cli::bare();
    register_image(&cli, &fake_image_dir(&cli, "i1", "img-v1", 0x68));
    register(&cli, &profile_variant(&cli, "p1", "reg-v1", ""));
    let profiles = cli.json(&["profile", "list"]);
    assert_eq!(profiles.as_array().unwrap().len(), 1, "{profiles}");
    assert_eq!(profiles[0]["id"], "reg-v1");
    let images = cli.json(&["image", "list"]);
    assert_eq!(images.as_array().unwrap().len(), 1, "{images}");
}

#[test]
fn profile_register_and_list_are_unchanged_by_the_shared_code() {
    let cli = Cli::new();
    let digest = register(&cli, &profile_variant(&cli, "p1", "shape-v1", ""))["digest"]
        .as_str()
        .unwrap()
        .to_string();
    let out = cli
        .cmd(&["profile", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let row = &serde_json::from_str::<Value>(&text).unwrap()[0];
    let ms = row["registered_ms"].as_i64().unwrap();
    assert_eq!(
        text,
        format!("[{{\"digest\":\"{digest}\",\"id\":\"shape-v1\",\"registered_ms\":{ms}}}]\n")
    );
}

#[test]
fn image_help_lists_register_and_list() {
    let cli = Cli::new();
    cli.cmd(&["image", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("register"))
        .stdout(predicate::str::contains("list"));
    cli.cmd(&["--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("image"));
}

// ---- the Firecracker worker over the fake guest: selection, preflight, jail, records ----

const NEEDS_ROOT: &str = "needs root (euid 0), running as uid 1000";

impl Cli {
    fn submit_fc(&self, contract: &str, extra: &[&str]) -> Value {
        let patch = fix_patch();
        let mut args = vec![
            "submit",
            contract,
            "--yes",
            "--fake-agent-patch",
            patch.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        self.json_as(Mode::Fake, &args)
    }

    /// The `Submitted` payload of task `id`, whatever its worker (no `--worker` flag).
    fn submitted(&self, id: &str) -> Value {
        let out = self
            .cmd_as(Mode::Plain, &["events", id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let events: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        events
            .into_iter()
            .find(|e| e["type"] == "Submitted")
            .unwrap()["payload"]
            .clone()
    }

    /// `<home>/registry/images/<guest_profile>@<digest>/`.
    fn image_entry(&self, digest: &str) -> PathBuf {
        self.home()
            .join("registry/images")
            .join(format!("{}@{digest}", guest_profile()))
    }

    /// Changes one byte of the registered `rootfs.squashfs` (the registry is read-only: the
    /// test lifts that first, as an attacker with the owner's rights would).
    fn tamper_image(&self, digest: &str) {
        use std::os::unix::fs::PermissionsExt;
        let entry = self.image_entry(digest);
        let rootfs = entry.join("rootfs.squashfs");
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&rootfs, fs::Permissions::from_mode(0o644)).unwrap();
        let mut bytes = fs::read(&rootfs).unwrap();
        bytes[0] ^= 0xff;
        fs::write(&rootfs, bytes).unwrap();
    }

    /// Task directories under `<home>/tasks` (staging leftovers included) and `TaskCreated`
    /// rows in the journal, read straight from the database.
    fn task_footprint(&self) -> (Vec<String>, i64) {
        let dirs = fs::read_dir(self.home().join("tasks"))
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let db = self.home().join("agentos.db");
        let rows = if db.exists() {
            let conn = rusqlite::Connection::open(db).unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'TaskCreated'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        } else {
            0
        };
        (dirs, rows)
    }

    fn assert_no_task(&self) {
        assert_eq!(
            self.task_footprint(),
            (Vec::new(), 0),
            "a refused submission left a task behind"
        );
    }
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
}

#[test]
fn submit_with_worker_firecracker_records_the_worker_image_version_and_host_kernel() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let out = cli.submit_fc(&cli.contract(&cli.repo_copy()), &[]);
    assert_eq!(out["state"], "SUCCEEDED");
    let s = cli.submitted(out["task_id"].as_str().unwrap());
    assert_eq!(s["worker"], "firecracker");
    assert_eq!(s["guest_image_id"], guest_profile());
    assert_eq!(s["guest_image_digest"], digest.as_str());
    assert_eq!(s["firecracker_version"], "fake");
    assert!(!s["host_kernel"].as_str().unwrap().is_empty(), "{s}");
    assert_eq!(s["jailed"], false);
    assert!(
        s.get("guest_image").is_none(),
        "the host worker's label is not recorded for a VM: {s}"
    );
}

#[test]
fn fake_launcher_records_jailed_false() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let assert = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .assert()
        .success();
    let stderr = stderr_of(&assert);
    assert!(
        !stderr.contains("warning"),
        "no probe, no warning: {stderr}"
    );
    let id = serde_json::from_slice::<Value>(&assert.get_output().stdout).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.submitted(&id)["jailed"], false);
    let assert = cli
        .cmd_as(Mode::PlainFake, &["resume", &id])
        .assert()
        .success();
    assert!(!stderr_of(&assert).contains("warning"));
    assert_eq!(cli.status(&id)["state"], "SUCCEEDED");
}

#[test]
fn submit_refuses_when_the_jailer_is_unavailable_and_the_task_is_untouched() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    // A first task makes the journal exist, so "no TaskCreated row" is read from a real database.
    cli.submit_fc(&contract, &[]);
    let before = cli.task_footprint();
    cli.cmd_as(Mode::Fake, &["submit", &contract, "--yes", "--fake-agent-patch", fix_patch().to_str().unwrap()])
        .env("AGENTOS_TEST_JAIL_PROBE", format!("fail:{NEEDS_ROOT}"))
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains(format!(
            "firecracker worker unavailable: jailer unavailable: {NEEDS_ROOT}; pass --allow-unjailed to run Firecracker without a jail as the current user"
        )));
    assert_eq!(
        cli.task_footprint(),
        before,
        "no task directory, no TaskCreated row"
    );
    // On a fresh home: no task at all.
    let fresh = Cli::bare();
    fresh.register_guest_image();
    let contract = fresh.contract(&fresh.repo_copy());
    fresh
        .cmd_as(Mode::Fake, &["submit", &contract])
        .env("AGENTOS_TEST_JAIL_PROBE", format!("fail:{NEEDS_ROOT}"))
        .assert()
        .code(1);
    fresh.assert_no_task();
}

#[test]
fn allow_unjailed_records_jailed_false_and_warns() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let assert = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
                "--allow-unjailed",
            ],
        )
        .env("AGENTOS_TEST_JAIL_PROBE", format!("fail:{NEEDS_ROOT}"))
        .assert()
        .success();
    let warnings: Vec<String> = stderr_of(&assert)
        .lines()
        .filter(|l| l.starts_with("warning:"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        warnings,
        [format!(
            "warning: running Firecracker unjailed: {NEEDS_ROOT}"
        )]
    );
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(out["task_id"].as_str().unwrap())["jailed"],
        false
    );
}

#[test]
fn agentos_allow_unjailed_env_is_the_flag() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let probe = format!("fail:{NEEDS_ROOT}");
    let assert = cli
        .cmd_as(Mode::Fake, &["submit", &contract])
        .env("AGENTOS_TEST_JAIL_PROBE", &probe)
        .env("AGENTOS_ALLOW_UNJAILED", "1")
        .assert()
        .success();
    assert!(stderr_of(&assert).contains("warning: running Firecracker unjailed"));
    let id = serde_json::from_slice::<Value>(&assert.get_output().stdout).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.submitted(&id)["jailed"], false);
    // `0` is not the flag.
    cli.cmd_as(Mode::Fake, &["submit", &contract])
        .env("AGENTOS_TEST_JAIL_PROBE", &probe)
        .env("AGENTOS_ALLOW_UNJAILED", "0")
        .assert()
        .code(1);
}

#[test]
fn a_probe_that_passes_records_jailed_true() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    // The launcher is still the fake guest (nothing is really jailed): the record reflects the
    // decision, which is what this pins.
    let assert = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .env("AGENTOS_TEST_JAIL_PROBE", "ok")
        .assert()
        .success();
    assert!(!stderr_of(&assert).contains("warning"));
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(out["task_id"].as_str().unwrap())["jailed"],
        true
    );
}

#[test]
fn a_task_submitted_jailed_refuses_to_run_unjailed_later() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let out = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .env("AGENTOS_TEST_JAIL_PROBE", "ok")
        .assert()
        .success();
    let id = serde_json::from_slice::<Value>(&out.get_output().stdout).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.submitted(&id)["jailed"], true);
    let events = cli.events(&id);

    for probe in [Some("fail:jailer gone"), None] {
        let mut cmd = cli.cmd_as(Mode::PlainFake, &["resume", &id, "--allow-unjailed"]);
        if let Some(p) = probe {
            cmd.env("AGENTOS_TEST_JAIL_PROBE", p);
        }
        let assert = cmd.assert().code(1).stdout("");
        let stderr = stderr_of(&assert);
        assert!(
            stderr.contains("task was submitted jailed: jailer unavailable: "),
            "{stderr}"
        );
        if probe.is_some() {
            assert!(
                stderr.contains("task was submitted jailed: jailer unavailable: jailer gone"),
                "{stderr}"
            );
        }
        assert_eq!(cli.events(&id), events, "the task is untouched");
        assert_eq!(cli.status(&id)["state"], "READY");
        assert!(job_dirs(&cli).is_empty());
    }
    cli.cmd_as(Mode::PlainFake, &["cancel", &id])
        .env("AGENTOS_TEST_JAIL_PROBE", "fail:jailer gone")
        .assert()
        .success();
    // Cancelling a READY task needs no worker; the refusal is about running.
    let ok = cli
        .cmd_as(Mode::PlainFake, &["resume", &id])
        .env("AGENTOS_TEST_JAIL_PROBE", "ok")
        .assert()
        .success();
    assert_eq!(
        serde_json::from_slice::<Value>(&ok.get_output().stdout).unwrap()["state"],
        "CANCELLED"
    );
}

#[test]
fn a_task_submitted_unjailed_resumes_without_the_flag() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let probe = format!("fail:{NEEDS_ROOT}");
    let out = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
                "--allow-unjailed",
            ],
        )
        .env("AGENTOS_TEST_JAIL_PROBE", &probe)
        .assert()
        .success();
    let id = serde_json::from_slice::<Value>(&out.get_output().stdout).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.submitted(&id)["jailed"], false);
    let assert = cli
        .cmd_as(Mode::PlainFake, &["resume", &id])
        .env("AGENTOS_TEST_JAIL_PROBE", &probe)
        .assert()
        .success();
    assert!(
        !stderr_of(&assert).contains("warning"),
        "the acknowledgement was given at submit"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&assert.get_output().stdout).unwrap()["state"],
        "SUCCEEDED"
    );
}

#[test]
fn status_prints_jailed() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    for (probe, flag, jailed) in [
        ("ok", None, true),
        ("fail:no jailer", Some("--allow-unjailed"), false),
    ] {
        let mut args = vec!["submit", contract.as_str()];
        args.extend(flag);
        let out = cli
            .cmd_as(Mode::Fake, &args)
            .env("AGENTOS_TEST_JAIL_PROBE", probe)
            .assert()
            .success();
        let id = serde_json::from_slice::<Value>(&out.get_output().stdout).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(cli.json_as(Mode::Plain, &["status", &id])["jailed"], jailed);
    }
}

#[test]
fn submit_without_the_flag_records_worker_host_and_no_image_fields() {
    let cli = Cli::bare();
    let out = cli.json_as(Mode::Plain, &["submit", &cli.contract(&cli.repo_copy())]);
    let s = cli.submitted(out["task_id"].as_str().unwrap());
    let keys: BTreeSet<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
    let three_a = [
        "contract_digest",
        "repository_source",
        "repository_digest",
        "profile_id",
        "profile_digest",
        "guest_image",
        "model",
        "fake_agent_patch_digest",
    ];
    let expected: BTreeSet<&str> = three_a
        .into_iter()
        .chain([
            "worker",
            "model_endpoint",
            "model_limits_version",
            "model_policy_version",
        ])
        .collect();
    assert_eq!(keys, expected, "submission provenance, no image fields");
    assert_eq!(s["worker"], "host");
    assert_eq!(s["guest_image"], "fixture-executor-v0");
}

#[test]
fn agentos_worker_env_selects_the_worker() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    for (worker, expected) in [("firecracker", "firecracker"), ("host", "host")] {
        let out = cli
            .cmd_as(Mode::PlainFake, &["submit", &contract])
            .env("AGENTOS_WORKER", worker)
            .assert()
            .success();
        let id = serde_json::from_slice::<Value>(&out.get_output().stdout).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(cli.submitted(&id)["worker"], expected);
    }
    cli.cmd_as(Mode::Plain, &["submit", &contract])
        .env("AGENTOS_WORKER", "qemu")
        .assert()
        .code(2);
}

#[test]
fn later_commands_use_the_recorded_worker_and_a_disagreeing_flag_exits_2() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let ready = cli.json_as(
        Mode::Fake,
        &[
            "submit",
            &contract,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    let id = ready["task_id"].as_str().unwrap().to_string();
    let events = cli.events(&id);
    let bundle = cli.path("refused-bundle");
    for args in [
        vec!["resume", id.as_str()],
        vec!["status", id.as_str()],
        vec!["cancel", id.as_str()],
        vec!["revoke", id.as_str(), "--capability", "verification.run"],
        vec!["export", id.as_str(), bundle.to_str().unwrap()],
        vec!["events", id.as_str()],
        vec!["pause", id.as_str()],
    ] {
        let mut args: Vec<&str> = args;
        args.extend(["--worker", "host"]);
        cli.cmd_as(Mode::PlainFake, &args)
            .assert()
            .code(2)
            .stdout("")
            .stderr(predicate::str::contains(
                "task was submitted with worker firecracker",
            ));
    }
    assert_eq!(cli.events(&id), events, "the task is untouched");
    assert!(!bundle.exists());
    assert!(
        cli.grants(&id).is_empty(),
        "nothing approved, nothing revoked"
    );
    // Without the flag every command uses the record.
    assert_eq!(
        cli.json_as(Mode::PlainFake, &["resume", &id])["state"],
        "SUCCEEDED"
    );
    assert_eq!(
        cli.json_as(Mode::Plain, &["status", &id])["worker"],
        "firecracker"
    );
    assert!(
        !cli.cmd_as(Mode::Plain, &["events", &id])
            .assert()
            .success()
            .get_output()
            .stdout
            .is_empty()
    );
    assert_eq!(
        cli.json_as(Mode::Fake, &["status", &id])["state"],
        "SUCCEEDED",
        "an agreeing flag is fine"
    );
    let dir = cli.path("bundle");
    assert_eq!(
        cli.json_as(Mode::Plain, &["export", &id, dir.to_str().unwrap()])["state"],
        "SUCCEEDED"
    );
    // And the other way round.
    let host = cli.json_as(Mode::Plain, &["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.cmd_as(Mode::Fake, &["resume", &host])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "task was submitted with worker host",
        ));
}

fn contract_with_limits(cli: &Cli, vcpus: u32, memory: u32) -> String {
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    contract["limits"]["worker_vcpus"] = json!(vcpus);
    contract["limits"]["worker_memory_mib"] = json!(memory);
    cli.write(
        &format!("limits-{vcpus}-{memory}.json"),
        &contract.to_string(),
    )
}

#[test]
fn firecracker_limits_are_validated_at_submit() {
    let cli = Cli::bare();
    cli.register_guest_image();
    for (vcpus, memory, message) in [
        (
            33,
            256,
            "limit worker_vcpus must be at most 32 for the firecracker worker",
        ),
        (
            1,
            127,
            "limit worker_memory_mib must be at least 128 for the firecracker worker",
        ),
    ] {
        let contract = contract_with_limits(&cli, vcpus, memory);
        let before = cli.task_footprint();
        cli.cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .assert()
        .code(2)
        .stdout("")
        .stderr(predicate::str::contains(message));
        // Even where the preflight itself cannot pass (no /dev/kvm), a limit is a usage error.
        cli.cmd_as(
            Mode::Plain,
            &["--worker", "firecracker", "submit", &contract],
        )
        .assert()
        .code(2)
        .stderr(predicate::str::contains(message));
        assert_eq!(cli.task_footprint(), before, "no task left behind");
        // The host worker accepts both.
        assert_eq!(
            cli.json_as(Mode::Plain, &["submit", &contract])["state"],
            "READY"
        );
    }
    assert_eq!(
        cli.json_as(
            Mode::Fake,
            &["submit", &contract_with_limits(&cli, 32, 128)]
        )["state"],
        "READY"
    );
}

#[test]
fn submit_without_a_registered_image_exits_2() {
    let cli = Cli::bare();
    let contract = cli.contract(&cli.repo_copy());
    cli.cmd_as(Mode::Fake, &["submit", &contract])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(format!(
            "guest image {} not found in the registry",
            guest_profile()
        )))
        .stderr(predicate::str::contains("build and register it first"));
    cli.assert_no_task();
}

/// A contract over a repository copy pinning the guest image to `pin`.
fn contract_pinning_image(cli: &Cli, pin: &str) -> String {
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    contract["guest_image_digest"] = json!(pin);
    cli.write(&format!("image-pin-{pin}.json"), &contract.to_string())
}

#[test]
fn a_pinned_guest_image_digest_must_be_registered() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let missing = contract_pinning_image(&cli, &"0".repeat(64));
    cli.cmd_as(Mode::Fake, &["submit", &missing])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("is not in the registry"));
    cli.assert_no_task();
    let out = cli.submit_fc(&contract_pinning_image(&cli, &digest), &[]);
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(out["task_id"].as_str().unwrap())["guest_image_digest"],
        digest.as_str()
    );
}

#[test]
fn submit_with_a_pin_uses_exactly_that_image_even_if_a_newer_entry_exists() {
    let cli = Cli::bare();
    let older = cli.register_guest_image();
    // Registration times are milliseconds: make sure the second entry is strictly newer.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let newer_dir = fake_image_dir(&cli, "guest-image-2", guest_profile(), 0x69);
    let newer = register_image(&cli, &newer_dir)["digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(older, newer);

    let pinned = cli.submit_fc(&contract_pinning_image(&cli, &older), &[]);
    assert_eq!(pinned["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(pinned["task_id"].as_str().unwrap())["guest_image_digest"],
        older.as_str()
    );
    let unpinned = cli.json_as(Mode::Fake, &["submit", &cli.contract(&cli.repo_copy())]);
    assert_eq!(
        cli.submitted(unpinned["task_id"].as_str().unwrap())["guest_image_digest"],
        newer.as_str(),
        "no pin: the newest entry"
    );
}

#[test]
fn preflight_failure_exits_1_before_the_task_is_touched() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    cli.tamper_image(&digest);
    let contract = cli.contract(&cli.repo_copy());
    cli.cmd_as(
        Mode::Fake,
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )
    .assert()
    .code(1)
    .stdout("")
    .stderr(predicate::str::contains(
        "firecracker worker unavailable: guest image digest mismatch",
    ));
    cli.assert_no_task();

    // The real launcher, without /dev/kvm.
    if fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
    {
        println!("SKIPPED the /dev/kvm variant: /dev/kvm is usable here");
        return;
    }
    let fresh = Cli::bare();
    fresh.register_guest_image();
    let contract = fresh.contract(&fresh.repo_copy());
    fresh
        .cmd_as(
            Mode::Plain,
            &[
                "--worker",
                "firecracker",
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .env("AGENTOS_TEST_WORKERS", "1")
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains(
            "firecracker worker unavailable: /dev/kvm: ",
        ));
    fresh.assert_no_task();
}

#[test]
fn a_tampered_registered_image_fails_the_preflight_before_the_task_is_touched() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.json_as(
        Mode::Fake,
        &[
            "submit",
            &contract,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let events = cli.events(&id);
    cli.tamper_image(&digest);

    cli.cmd_as(Mode::PlainFake, &["resume", &id])
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains(format!(
            "firecracker worker unavailable: guest image digest mismatch: pinned {digest}"
        )));
    assert_eq!(cli.status(&id)["state"], "READY");
    assert_eq!(cli.events(&id), events, "no new events");
    assert!(cli.grants(&id).is_empty(), "not even approved");
    assert!(job_dirs(&cli).is_empty());
}

#[test]
fn a_recorded_image_that_is_no_longer_registered_fails_before_the_task_is_touched() {
    use std::os::unix::fs::PermissionsExt;
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let id = cli.json_as(
        Mode::Fake,
        &[
            "submit",
            &contract,
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let events = cli.events(&id);
    let entry = cli.image_entry(&digest);
    fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    fs::remove_dir_all(&entry).unwrap();
    cli.cmd_as(Mode::PlainFake, &["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "recorded guest image {}@{digest} is no longer registered",
            guest_profile()
        )));
    assert_eq!(cli.events(&id), events);
}

#[test]
fn status_prints_worker_and_guest_image() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let fc = cli.submit_fc(&contract, &[])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let status = cli.json_as(Mode::Plain, &["status", &fc]);
    assert_eq!(status["worker"], "firecracker");
    assert_eq!(
        status["guest_image"],
        json!({ "id": guest_profile(), "digest": digest })
    );
    assert_eq!(status["jailed"], false);
    let host = cli.json_as(Mode::Plain, &["submit", &contract])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let status = cli.json_as(Mode::Plain, &["status", &host]);
    assert_eq!(status["worker"], "host");
    assert!(
        status.get("guest_image").is_none() && status.get("jailed").is_none(),
        "{status}"
    );
}

#[test]
fn manifest_names_the_guest_image_for_firecracker_tasks_and_omits_it_for_host_tasks() {
    let cli = Cli::bare();
    let digest = cli.register_guest_image();
    let contract = cli.contract(&cli.repo_copy());
    let fc = cli.submit_fc(&contract, &[])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = cli.path("fc-bundle");
    let manifest = cli.json_as(Mode::Plain, &["export", &fc, dir.to_str().unwrap()]);
    assert_eq!(manifest["guest_image_digest"], digest.as_str());
    assert_eq!(
        manifest["vm_resources"],
        json!({ "version": 1, "disk_mib": 1024, "scratch_mib": 512, "bandwidth_mib_s": null, "iops": null })
    );
    let host = cli.json_as(
        Mode::Plain,
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let dir = cli.path("host-bundle");
    let manifest = cli.json_as(Mode::Plain, &["export", &host, dir.to_str().unwrap()]);
    assert!(manifest.get("guest_image_digest").is_none(), "{manifest}");
    assert!(manifest.get("vm_resources").is_none(), "{manifest}");
    let written = fs::read_to_string(dir.join("manifest.json")).unwrap();
    assert!(!written.contains("guest_image_digest") && !written.contains("vm_resources"));
}

#[test]
fn firecracker_task_runs_to_succeeded_with_the_fake_guest_and_host_paths_do_not_appear_in_evidence()
{
    let cli = Cli::bare();
    cli.register_guest_image();
    let out = cli.submit_fc(&cli.contract(&cli.repo_copy()), &[]);
    assert_eq!(out["state"], "SUCCEEDED");
    let id = out["task_id"].as_str().unwrap();
    let dir = cli.path("bundle");
    let manifest = cli.json_as(Mode::Plain, &["export", id, dir.to_str().unwrap()]);
    assert_eq!(manifest["verification_results"][0]["passed"], true);
    let home = cli.home().to_str().unwrap().to_string();
    let mut checked = 0;
    for file in walk(&dir.join("evidence")) {
        let evidence: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        for stream in ["stdout", "stderr"] {
            if let Some(text) = evidence.get(stream).and_then(Value::as_str) {
                assert!(
                    !text.contains(&home),
                    "{stream} of {} names the home: {text}",
                    file.display()
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 2, "the verification evidence was checked");
    assert_no_job_processes(&cli);
}

/// Waits (bounded) until no process names the home: the job's supervisor, worker and fake
/// guest are gone.
fn wait_for_no_home_processes(cli: &Cli, bound: std::time::Duration) {
    let started = std::time::Instant::now();
    while !processes_mentioning(cli.home().to_str().unwrap()).is_empty() {
        assert!(
            started.elapsed() < bound,
            "processes still name the home after {bound:?}: {:?}",
            processes_mentioning(cli.home().to_str().unwrap())
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// A Firecracker task (fake guest) whose controller died right after launching a 30 s
/// verification: the supervisor and the guest outlive it. `probe` is the jail probe answer at
/// submission. Returns the profiles registry, the task id and the image digest.
fn fc_task_with_a_running_slow_verification(
    cli: &Cli,
    probe: Option<&str>,
) -> (PathBuf, String, String) {
    let digest = cli.register_guest_image();
    let (profiles, contract) = slow_world(cli);
    let mut cmd = Command::from_std(cli.std_cmd(
        Mode::Fake,
        false,
        &profiles,
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
            "--crash-at",
            "during-execute:run_verification",
        ],
    ));
    if let Some(p) = probe {
        cmd.env("AGENTOS_TEST_JAIL_PROBE", p);
    }
    cmd.assert().code(75);
    wait_for_verification_job(cli);
    let id = first_task(cli);
    assert_eq!(cli.submitted(&id)["worker"], "firecracker");
    assert!(
        !processes_mentioning(cli.home().to_str().unwrap()).is_empty(),
        "the verification job outlived its controller"
    );
    (profiles, id, digest)
}

#[test]
fn cancel_records_its_intent_and_stops_running_jobs_even_when_the_worker_cannot_run() {
    let started = std::time::Instant::now();
    // A tampered image: the preflight fails.
    let cli = Cli::bare();
    let (profiles, id, digest) = fc_task_with_a_running_slow_verification(&cli, None);
    cli.tamper_image(&digest);
    let assert =
        Command::from_std(cli.std_cmd(Mode::PlainFake, false, &profiles, &["cancel", &id]))
            .assert()
            .code(1)
            .stdout("");
    let stderr = stderr_of(&assert);
    assert!(
        stderr.contains("firecracker worker unavailable: guest image digest mismatch"),
        "{stderr}"
    );
    assert!(
        stderr.contains("the cancel is requested and completes on a later"),
        "{stderr}"
    );
    let status = cli.json_as(Mode::Plain, &["status", &id]);
    assert_eq!(status["cancel_requested"], true, "{status}");
    assert!(
        !TERMINAL.contains(&status["state"].as_str().unwrap()),
        "{status}"
    );
    assert!(
        cli.event_types(&id)
            .contains(&"CancelRequested".to_string())
    );
    wait_for_no_home_processes(&cli, std::time::Duration::from_secs(15));

    // Recorded jailed, jailer gone: the jail recheck fails.
    let cli = Cli::bare();
    let (profiles, id, _) = fc_task_with_a_running_slow_verification(&cli, Some("ok"));
    let assert =
        Command::from_std(cli.std_cmd(Mode::PlainFake, false, &profiles, &["cancel", &id]))
            .env("AGENTOS_TEST_JAIL_PROBE", "fail:jailer gone")
            .assert()
            .code(1);
    assert!(
        stderr_of(&assert).contains("task was submitted jailed: jailer unavailable: jailer gone")
    );
    assert_eq!(
        cli.json_as(Mode::Plain, &["status", &id])["cancel_requested"],
        true
    );
    wait_for_no_home_processes(&cli, std::time::Duration::from_secs(15));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(50),
        "the 30 s checks were stopped, took {:?}",
        started.elapsed()
    );
}

#[test]
fn revoke_stops_a_running_job_even_when_the_worker_cannot_run() {
    let started = std::time::Instant::now();
    let cli = Cli::bare();
    let (profiles, id, digest) = fc_task_with_a_running_slow_verification(&cli, Some("ok"));
    cli.tamper_image(&digest);
    let out = Command::from_std(cli.std_cmd(
        Mode::PlainFake,
        false,
        &profiles,
        &["revoke", &id, "--capability", "verification.run"],
    ))
    .env("AGENTOS_TEST_JAIL_PROBE", "fail:jailer gone")
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&out).unwrap(),
        json!({ "task_id": id, "revoked": ["verification.run"], "cancelled_jobs": 1 })
    );
    wait_for_no_home_processes(&cli, std::time::Duration::from_secs(15));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "the 30 s check was stopped, took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------------------
// The KVM tier: the CLI on the real, jailed worker, whatever AGENTOS_TEST_WORKER says.

/// `submit` with a jailer that is not there: refused before the task is touched (the probe
/// runs the real `jailer --version`); with `--allow-unjailed` the task runs unjailed on the
/// real worker and records it.
#[test]
fn jailer_unavailable_refuses_before_the_task_is_touched() {
    let Some(kvm) = kvm::require() else { return };
    let cli = Cli::bare();
    cli.json_as(
        Mode::Plain,
        &["image", "register", kvm.image_dir.to_str().unwrap()],
    );
    let contract = cli.contract(&cli.repo_copy());
    let patch = fix_patch();
    let submit = |extra: &[&str]| {
        let mut cmd = cli.cmd_as(
            Mode::Plain,
            &[
                "--worker",
                "firecracker",
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                patch.to_str().unwrap(),
            ],
        );
        cmd.arg("--firecracker")
            .arg(&kvm.firecracker_bin)
            .args(["--jailer", "/nonexistent/jailer"])
            .args(extra);
        cmd
    };
    submit(&[]).assert().code(1).stdout("").stderr(predicate::str::contains(
        "firecracker worker unavailable: jailer unavailable: jailer --version: /nonexistent/jailer: No such file or directory",
    ));
    assert_nothing_recorded_but_the_registry(&cli);

    let out = submit(&["--allow-unjailed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let done: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let submitted = cli.submitted(done["task_id"].as_str().unwrap());
    assert_eq!(
        (
            submitted["jailed"].clone(),
            submitted["firecracker_version"].clone()
        ),
        (json!(false), json!("Firecracker v1.17.0")),
        "{submitted}"
    );
    assert_no_job_processes(&cli);
}

fn assert_nothing_recorded_but_the_registry(cli: &Cli) {
    let names: Vec<String> = fs::read_dir(cli.home())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["registry"], "the home holds {names:?}");
}

/// The CLI's jailed launch end to end on the real worker: `submit --yes` (jailed) to
/// SUCCEEDED, `status`, `export`; then the controller killed right after the patch job's
/// launch (exit 75), `resume` to SUCCEEDED, and the bundle equals the uncrashed one.
#[test]
fn cli_jailed_submit_status_export_then_kill_and_resume_on_the_real_worker() {
    let Some(kvm) = kvm::require() else { return };
    let cli = Cli::bare();
    cli.json_as(
        Mode::Plain,
        &["image", "register", kvm.image_dir.to_str().unwrap()],
    );
    let contract = cli.contract(&fixtures().join("parser-repo"));
    let patch = fix_patch();
    let real = |args: &[&str]| cli.cmd_as(Mode::Real, args);

    let out = real(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        patch.to_str().unwrap(),
    ])
    .assert()
    .success()
    .get_output()
    .stdout
    .clone();
    let done: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let id = done["task_id"].as_str().unwrap().to_string();
    let submitted = cli.submitted(&id);
    assert_eq!(
        (submitted["worker"].clone(), submitted["jailed"].clone()),
        (json!("firecracker"), json!(true)),
        "{submitted}"
    );
    let status: Value = serde_json::from_slice(
        &real(&["status", &id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(
        (status["state"].clone(), status["jailed"].clone()),
        (json!("SUCCEEDED"), json!(true)),
        "{status}"
    );
    assert_eq!(status["verified_digest"], status["workspace_digest"]);
    let clean = cli.path("clean");
    let expected: Value = serde_json::from_slice(
        &real(&["export", &id, clean.to_str().unwrap()])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(
        expected["guest_image_digest"], submitted["guest_image_digest"],
        "{expected}"
    );

    let crashed = real(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        patch.to_str().unwrap(),
        "--crash-at",
        "during-execute:apply_patch",
    ])
    .assert()
    .code(75)
    .get_output()
    .stderr
    .clone();
    let crashed: Value = String::from_utf8_lossy(&crashed)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v.get("crashed").is_some())
        .unwrap();
    let id = crashed["task_id"].as_str().unwrap().to_string();
    let resumed: Value = serde_json::from_slice(
        &real(&["resume", &id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }));
    let recovered = cli.path("recovered");
    let manifest: Value = serde_json::from_slice(
        &real(&["export", &id, recovered.to_str().unwrap()])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(
        normalized(&manifest),
        normalized(&expected),
        "the recovered bundle equals the uncrashed one"
    );
    assert_one_job_per_effect(&cli, "after the controller's kill");
    assert_no_job_processes(&cli);
}

/// A verification profile (a copy of `parser-checks-v1`) whose command first writes every
/// ancestor's environment, up to and including the first one holding `CARGO_CANARY`, to
/// `dump`, then runs the real check.
fn env_dump_profiles(cli: &Cli, dump: &Path) -> PathBuf {
    let profiles = cli.path("env-dump-profiles");
    copy_tree(
        &fixtures().join("profiles/parser-checks-v1"),
        &profiles.join("parser-checks-v1"),
    )
    .unwrap();
    let script = format!(
        r#"p=$PPID; : > {d}
while [ "$p" -gt 1 ]; do
  {{ echo "@@@ $(tr '\0' ' ' < /proc/$p/cmdline)"; tr '\0' '\n' < /proc/$p/environ; }} >> {d}
  if grep -qz '^CARGO_CANARY=' /proc/$p/environ; then break; fi
  p=$(sed 's/.*) //' /proc/$p/stat | cut -d' ' -f2)
done
exec python3 check_parser.py "$1""#,
        d = dump.display()
    );
    let profile = json!({ "id": "parser-checks-v1", "command": ["sh", "-c", script, "sh"], "protected": true });
    fs::write(
        profiles.join("parser-checks-v1/profile.json"),
        profile.to_string(),
    )
    .unwrap();
    profiles
}

/// The controller's own environment reaches neither the supervisor nor the worker: only the
/// `AGENTOS_TEST_*` switches travel, and only because `AGENTOS_TEST_WORKERS=1`.
#[test]
fn test_switches_reach_the_supervisor_only_through_the_forwarding() {
    let cli = Cli::bare();
    let dump = cli.path("environ-dump.txt");
    let profiles = env_dump_profiles(&cli, &dump);
    let contract = cli.contract(&cli.repo_copy());
    let mut cmd = Command::from_std(cli.std_cmd(
        Mode::PlainFake,
        false,
        &profiles,
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    ));
    // The secret reaches the controller (a `Command::env`, never `set_var`).
    cmd.env("CARGO_CANARY", "sk-ant-canary-0123456789");
    cmd.assert().success();
    let text = fs::read_to_string(&dump).expect("the verification wrote its environment dump");
    let sections: Vec<&str> = text.split("@@@ ").filter(|s| !s.is_empty()).collect();
    assert!(
        sections.len() >= 3,
        "worker, supervisor and controller expected:\n{text}"
    );
    let (controller, children) = sections.split_last().unwrap();
    assert!(
        controller.contains("CARGO_CANARY=sk-ant-canary-0123456789"),
        "control: the controller holds the canary:\n{text}"
    );
    for child in children {
        assert!(
            !child.contains("CARGO_CANARY") && !child.contains("sk-ant-canary"),
            "a child inherited the controller's environment:\n{child}"
        );
        assert!(
            !child.contains("HOME=") && !child.contains("CARGO_MANIFEST_DIR="),
            "a child inherited the controller's environment:\n{child}"
        );
        assert!(child.lines().any(|l| l.starts_with("PATH=")), "{child}");
    }
    let supervisor = children
        .iter()
        .find(|c| c.contains("supervise"))
        .expect("a supervisor among the ancestors");
    for want in ["AGENTOS_TEST_WORKERS=1", "AGENTOS_TEST_FAKE_GUEST=1"] {
        assert!(
            supervisor.lines().any(|l| l == want),
            "the supervisor lacks {want}:\n{supervisor}"
        );
    }
}

// ---- Phase 4: --model, --api-key-file, --anthropic-base-url ----

/// A recognisable key: every scan below looks for `SECRET`.
const CANARY: &str = "sk-ant-test-SECRET";

fn transcript(name: &str) -> PathBuf {
    fixtures().join("transcripts").join(name)
}

fn fake_spec(name: &str) -> String {
    format!("fake:{}", transcript(name).display())
}

fn fake_api(name: &str) -> http::FakeApi {
    http::serve(http::Reply::Transcript(transcript(name)))
}

impl Cli {
    /// A contract that may call the model: the fifth capability and a token cap.
    fn model_contract(&self, repo: &Path, model_requests: u32, tool_actions: u32) -> String {
        let contract = json!({
            "goal": "fix the parser",
            "repository": { "source": repo, "revision": "recorded-at-submission" },
            "profile": guest_profile(),
            "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export", "model.request"],
            "limits": {
                "model_requests": model_requests, "max_output_tokens_per_request": 1000, "tool_actions": tool_actions,
                "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256
            }
        });
        self.write(
            &format!(
                "model-task-{}.json",
                Digest::of(contract.to_string().as_bytes())
            ),
            &contract.to_string(),
        )
    }

    /// `model_contract` naming the verification profile `id` instead.
    fn model_contract_for_profile(&self, id: &str) -> String {
        let mut contract: Value = serde_json::from_str(
            &fs::read_to_string(self.model_contract(&self.repo_copy(), 12, 10)).unwrap(),
        )
        .unwrap();
        contract["verification_profile"] = json!(id);
        self.write(
            &format!(
                "model-env-{}.json",
                Digest::of(contract.to_string().as_bytes())
            ),
            &contract.to_string(),
        )
    }

    /// `submit --yes --model <spec>` with the key in the environment; the task id.
    fn submit_model(&self, contract: &str, spec: &str) -> Value {
        self.json(&["submit", contract, "--yes", "--model", spec])
    }

    /// `Cli::crash` for a model task.
    fn crash_model(&self, contract: &str, spec: &str, point: &str) -> String {
        let assert = self
            .cmd(&[
                "submit",
                contract,
                "--yes",
                "--model",
                spec,
                "--crash-at",
                point,
            ])
            .assert()
            .code(75);
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        let crashed: Value = stderr
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("crashed").is_some())
            .unwrap_or_else(|| panic!("no crash report on stderr: {stderr}"));
        assert_eq!(
            crashed["crashed"],
            point.split(':').next().unwrap(),
            "{stderr}"
        );
        crashed["task_id"].as_str().unwrap().to_string()
    }
}

/// The profile whose command prints the worker's and the supervisor's environments and
/// exits 0, registered; returns its id.
fn env_dump_profile(cli: &Cli) -> String {
    let dir = profile_variant(cli, "env-dump", "env-dump-v1", "");
    let command = "p=$PPID; tr '\\0' '\\n' < /proc/$p/environ; echo ---; tr '\\0' '\\n' < /proc/$(awk '/^PPid:/{print $2}' /proc/$p/status)/environ; exit 0";
    fs::write(
        dir.join("profile.json"),
        json!({ "id": "env-dump-v1", "command": ["sh", "-c", command], "protected": true })
            .to_string(),
    )
    .unwrap();
    register(cli, &dir);
    "env-dump-v1".to_string()
}

fn status_usage(status: &Value, field: &str) -> u64 {
    status["usage"][field]
        .as_u64()
        .unwrap_or_else(|| panic!("no usage.{field} in {status}"))
}

fn events_of<'a>(events: &'a [Value], ty: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == ty).collect()
}

fn model_dirs(cli: &Cli) -> Vec<String> {
    let Ok(entries) = fs::read_dir(cli.home().join("model")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// Every file of the home, wherever it is (database and its WAL included), must be free of
/// `needle`.
fn assert_home_free_of(cli: &Cli, needle: &str, what: &str) {
    let files = walk(&cli.home());
    assert!(
        files.len() > 5,
        "the scan saw only {} files: {files:?}",
        files.len()
    );
    for file in files {
        let bytes = fs::read(&file).unwrap_or_default();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "{what}: {needle} found in {}",
            file.display()
        );
    }
}

fn assert_text_free_of(text: &str, needle: &str, what: &str) {
    assert!(!text.contains(needle), "{what} leaks {needle}:\n{text}");
}

#[test]
fn submit_with_a_fake_transcript_runs_the_model_agent_to_success() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let done = cli.submit_model(&contract, &fake_spec("parser-fix.json"));
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let id = done["task_id"].as_str().unwrap();
    let status = cli.status(id);
    assert_eq!(status["model"], "fake:parser-fix.json", "{status}");
    assert_eq!(
        status_usage(&status, "settled_model_requests"),
        6,
        "{status}"
    );
    let events = cli.events(id);
    let intended = events_of(&events, "EffectIntended")
        .into_iter()
        .filter(|e| e["payload"]["kind"].get("ModelCall").is_some())
        .count();
    assert_eq!(intended, 6, "{events:?}");
    let submitted = cli.submitted(id);
    assert_eq!(submitted["model"], "fake:parser-fix.json");
    assert_eq!(
        submitted["transcript_digest"],
        digest_of_file(&transcript("parser-fix.json"))
    );
    assert_eq!(
        digest_of_file(&cli.home().join("tasks").join(id).join("transcript.json")),
        digest_of_file(&transcript("parser-fix.json"))
    );
    let (dir, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["model"], "fake:parser-fix.json");
    let calls = manifest["model_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 6, "{manifest}");
    for call in calls {
        assert!(
            dir.join(call["request_file"].as_str().unwrap()).is_file(),
            "{call}"
        );
        assert!(
            dir.join(call["response_file"].as_str().unwrap()).is_file(),
            "{call}"
        );
    }
    assert_eq!(model_dirs(&cli).len(), 6, "{:?}", model_dirs(&cli));
}

#[test]
fn submit_with_anthropic_against_the_local_api_fixes_the_fixture() {
    let cli = Cli::new();
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let done = {
        let out = cli
            .cmd(&[
                "submit",
                &contract,
                "--yes",
                "--model",
                "anthropic:claude-opus-5-5",
                "--anthropic-base-url",
                &api.url(),
            ])
            .env("ANTHROPIC_API_KEY", CANARY)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        serde_json::from_slice::<Value>(&out).unwrap()
    };
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    assert_eq!(api.hits(), 4);
    for request in api.requests() {
        let header = |name: &str| {
            request
                .headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(header("x-api-key").as_deref(), Some(CANARY));
        assert_eq!(header("anthropic-version").as_deref(), Some("2023-06-01"));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 1000);
    }
    assert_eq!(
        cli.submitted(done["task_id"].as_str().unwrap())["model"],
        "anthropic:claude-opus-5-5"
    );
}

#[test]
fn api_key_reaches_only_the_provider_and_never_the_journal_blobs_home_or_job_environments() {
    let cli = Cli::new();
    let profile = env_dump_profile(&cli);
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract_for_profile(&profile);
    let url = api.url();

    let mut seen_requests = 0;
    for (round, via_file) in [false, true].into_iter().enumerate() {
        let key_file = cli.write("key.txt", &format!("{CANARY}\n"));
        let mut args = vec![
            "submit",
            contract.as_str(),
            "--yes",
            "--model",
            "anthropic:claude-opus-5-5",
            "--anthropic-base-url",
            url.as_str(),
        ];
        if via_file {
            args.extend(["--api-key-file", key_file.as_str()]);
        }
        let mut cmd = cli.cmd(&args);
        if !via_file {
            cmd.env("ANTHROPIC_API_KEY", CANARY);
        }
        let out = cmd.assert().success().get_output().clone();
        let (stdout, stderr) = (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
        let done: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(done["state"], "SUCCEEDED", "round {round}: {done}");
        let id = done["task_id"].as_str().unwrap().to_string();

        // Positive control: the key really travelled, in the header of the provider's own request.
        let requests = api.requests();
        assert!(
            requests.len() > seen_requests,
            "round {round}: the fake API saw no request"
        );
        let first = &requests[seen_requests];
        assert_eq!(
            first
                .headers
                .iter()
                .find(|(k, _)| k == "x-api-key")
                .map(|(_, v)| v.as_str()),
            Some(CANARY),
            "round {round}"
        );
        seen_requests = requests.len();

        // Negative scan: everything the controller wrote or printed.
        assert_text_free_of(&stdout, "SECRET", "submit stdout");
        assert_text_free_of(&stderr, "SECRET", "submit stderr");
        let status = serde_json::to_string(&cli.status(&id)).unwrap();
        assert_text_free_of(&status, "SECRET", "status");
        let events = serde_json::to_string(&cli.events(&id)).unwrap();
        assert_text_free_of(&events, "SECRET", "events");
        let (bundle, manifest) = cli.export(&id, &format!("bundle-{round}"));
        assert_text_free_of(&manifest.to_string(), "SECRET", "the manifest");
        for file in walk(&bundle) {
            let bytes = fs::read(&file).unwrap();
            assert!(
                !bytes.windows(6).any(|w| w == b"SECRET"),
                "the export bundle file {} leaks the key",
                file.display()
            );
        }
        assert_home_free_of(&cli, "SECRET", "the home");
        for sub in ["blobs", "jobs", "model", "tasks"] {
            assert!(
                cli.home().join(sub).is_dir(),
                "round {round}: {sub} should exist so its scan is not vacuous"
            );
        }

        // The verification evidence holds the worker's and the supervisor's environments.
        let mut dumps = 0;
        for file in walk(&cli.home().join("blobs")) {
            let Ok(blob) = serde_json::from_slice::<Value>(&fs::read(&file).unwrap()) else {
                continue;
            };
            let Some(stdout) = blob.get("stdout").and_then(Value::as_str) else {
                continue;
            };
            if !stdout.contains("---") {
                continue;
            }
            dumps += 1;
            // In the real guest the parent is the guest's init: its environment is not
            // readable, so the dump is empty. That is a stronger statement about the key (the
            // guest never had it, see `api_key_never_reaches_the_guest`), but the dump is
            // only a non-vacuous scan on the host-side workers.
            if !real_mode() {
                assert!(stdout.contains("PATH="), "round {round}: {stdout}");
            }
            assert!(
                !stdout.contains("ANTHROPIC_API_KEY") && !stdout.contains("SECRET"),
                "round {round}: a job environment holds the key:\n{stdout}"
            );
        }
        assert!(
            dumps >= 1,
            "round {round}: no environment dump among the evidence blobs"
        );
    }
}

#[test]
fn missing_key_is_a_usage_error_before_anything_is_written() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    cli.cmd(&["submit", &contract, "--yes", "--model", "anthropic:x"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no API key: pass --api-key-file FILE or set ANTHROPIC_API_KEY",
        ));
    assert_nothing_recorded(&cli);
}

#[test]
fn an_unreadable_key_file_exits_2() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let missing = cli.path("no-such-key");
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--model",
        "anthropic:x",
        "--api-key-file",
        missing.to_str().unwrap(),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("cannot read"));
    let empty = cli.write("empty-key", "  \n");
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--model",
        "anthropic:x",
        "--api-key-file",
        &empty,
    ])
    .assert()
    .code(2);
    assert_nothing_recorded(&cli);
}

#[test]
fn unknown_model_spec_exits_2() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    cli.cmd(&["submit", &contract, "--yes", "--model", "gpt:4"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unknown model spec"));
    for bad in [
        "anthropic:",
        "anthropic:a/b",
        &format!("anthropic:{}", "m".repeat(200)),
        "fake:",
    ] {
        cli.cmd(&["submit", &contract, "--yes", "--model", bad])
            .env("ANTHROPIC_API_KEY", CANARY)
            .assert()
            .code(2);
    }
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--model",
        "fake:/no/such/transcript.json",
    ])
    .assert()
    .code(2);
    let bad = cli.write("bad-transcript.json", "{\"responses\": 3}");
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--model",
        &format!("fake:{bad}"),
    ])
    .assert()
    .code(2);
    assert_nothing_recorded(&cli);
}

#[test]
fn an_oversized_key_file_fails_before_submission_without_quoting_the_key() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let key = cli.write("oversized-key", &CANARY.repeat(300));
    let api = fake_api("parser-fix-direct.json");
    let result = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--model",
            "anthropic:x",
            "--api-key-file",
            &key,
            "--anthropic-base-url",
            &api.url(),
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(stderr.contains("exceeds 4096 bytes"), "{stderr}");
    assert!(!stderr.contains("SECRET"), "{stderr}");
    assert_eq!(api.hits(), 0);
    assert_nothing_recorded(&cli);
}

#[test]
fn yes_without_patch_or_model_exits_2_with_the_new_text() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    cli.cmd(&["submit", &contract, "--yes"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "--yes needs --fake-agent-patch FILE or --model anthropic:<model>|fake:<transcript>",
        ));
    assert_nothing_recorded(&cli);
}

#[test]
fn patch_and_model_together_exit_2() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
        "--model",
        &fake_spec("parser-fix.json"),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains(
        "pass either --fake-agent-patch or --model",
    ));
    assert_nothing_recorded(&cli);
}

#[test]
fn resume_of_a_model_task_needs_no_patch_and_refuses_one() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let id = cli.json(&[
        "submit",
        &contract,
        "--model",
        &fake_spec("parser-fix.json"),
    ])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(cli.status(&id)["state"], "READY");
    assert_eq!(
        cli.json(&["resume", &id]),
        json!({ "task_id": id, "state": "SUCCEEDED" })
    );

    let other = cli.model_contract(&cli.repo_copy(), 12, 11);
    let id = cli.json(&["submit", &other, "--model", &fake_spec("parser-fix.json")])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.cmd(&[
        "resume",
        &id,
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains(format!(
        "task {id} runs a model, not the fake agent"
    )));
    assert_eq!(
        cli.status(&id)["state"],
        "READY",
        "the refusal changed nothing"
    );
}

#[test]
fn a_replaced_transcript_fails_the_resume_instead_of_replaying_another_file() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let id = cli.json(&[
        "submit",
        &contract,
        "--model",
        &fake_spec("parser-fix.json"),
    ])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    fs::write(
        cli.home().join("tasks").join(&id).join("transcript.json"),
        fs::read(transcript("parser-fix-direct.json")).unwrap(),
    )
    .unwrap();
    cli.cmd(&["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("transcript"));
    assert_eq!(cli.status(&id)["state"], "READY");
}

#[test]
fn resume_of_an_anthropic_task_without_a_key_is_a_usage_error_and_changes_nothing() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let id = cli.json(&["submit", &contract, "--model", "anthropic:claude-opus-5-5"])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    cli.cmd(&["resume", &id])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "no API key: pass --api-key-file FILE or set ANTHROPIC_API_KEY",
        ));
    assert_eq!(cli.status(&id)["state"], "READY");
    assert_eq!(cli.status(&id)["model"], "anthropic:claude-opus-5-5");
}

#[test]
fn resume_uses_the_recorded_endpoint_and_rejects_overrides_before_key_reads() {
    let cli = Cli::new();
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let id = cli.json(&[
        "submit",
        &contract,
        "--model",
        "anthropic:x",
        "--anthropic-base-url",
        &api.url(),
    ])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let status = cli.status(&id);
    assert_eq!(status["model_endpoint"], api.url());
    assert_eq!(status["model_policy_version"], 1);
    assert_eq!(status["model_limits_version"], 1);
    let before = cli.events(&id);
    let missing = cli.path("no-key");
    for environment in [false, true] {
        let mut command = cli.cmd(&["resume", &id, "--api-key-file", missing.to_str().unwrap()]);
        if environment {
            command.env("AGENTOS_ANTHROPIC_BASE_URL", "http://127.0.0.1:1");
        } else {
            command.args(["--anthropic-base-url", "http://127.0.0.1:1"]);
        }
        command.assert().code(2).stderr(predicate::str::contains(
            "differs from the recorded endpoint",
        ));
        assert_eq!(cli.events(&id), before);
    }
    cli.cmd(&["resume", &id])
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .success();
    assert_eq!(cli.status(&id)["state"], "SUCCEEDED");
    assert_eq!(api.hits(), 4);
    let export = cli.path("endpoint-export");
    cli.cmd(&["export", &id, export.to_str().unwrap()])
        .assert()
        .success();
    let manifest: Value =
        serde_json::from_slice(&fs::read(export.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["model_endpoint"], api.url());
    assert_eq!(manifest["model_policy_version"], 1);
}

#[test]
fn legacy_missing_endpoint_allows_only_the_official_provider() {
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let id = cli.json(&["submit", &contract, "--model", "anthropic:x"])["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    rusqlite::Connection::open(cli.home().join("agentos.db")).unwrap().execute(
        "UPDATE events SET payload=json_remove(payload, '$.model_endpoint', '$.model_policy_version', '$.model_limits_version')
         WHERE task_id=?1 AND type='Submitted'", [&id]).unwrap();
    let before = cli.events(&id);
    cli.cmd(&["resume", &id, "--anthropic-base-url", "http://127.0.0.1:1"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "differs from the recorded endpoint",
        ));
    assert_eq!(cli.events(&id), before);
    let status = cli.status(&id);
    assert_eq!(status["model_endpoint"], "https://api.anthropic.com");
    assert_eq!(status["model_policy_version"], 0);
}

#[test]
fn a_crash_on_the_model_call_resumes_with_a_forfeit_and_no_second_charge() {
    for (point, decision, uncertain) in [
        ("after-dispatch:model_call", "Forfeit", 1),
        ("during-execute:model_call", "Forfeit", 1),
        (
            "after-execute-before-publish:model_call",
            "PublishRetained",
            0,
        ),
    ] {
        let cli = Cli::new();
        let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
        let id = cli.crash_model(&contract, &fake_spec("parser-fix-direct.json"), point);
        if point.starts_with("after-dispatch") {
            let status = cli.status(&id);
            let outstanding = status["outstanding_effects"].as_array().unwrap();
            assert_eq!(outstanding.len(), 1, "{status}");
            assert_eq!(
                (
                    outstanding[0]["kind"].clone(),
                    outstanding[0]["state"].clone()
                ),
                (json!("model_call"), json!("Dispatched")),
                "{status}"
            );
            assert_eq!(
                status_usage(&status, "reserved_model_requests"),
                1,
                "{status}"
            );
        }
        assert_eq!(
            cli.json(&["resume", &id]),
            json!({ "task_id": id, "state": "SUCCEEDED" }),
            "{point}"
        );
        let events = cli.events(&id);
        let decisions: Vec<&str> = events_of(&events, "RecoveryDecision")
            .iter()
            .map(|e| e["payload"]["decision"].as_str().unwrap())
            .collect();
        assert_eq!(decisions, [decision], "{point}: {events:?}");
        assert_eq!(
            events_of(&events, "EffectForfeited").len(),
            usize::from(decision == "Forfeit"),
            "{point}"
        );
        let status = cli.status(&id);
        assert_eq!(
            status_usage(&status, "settled_model_requests"),
            4,
            "{point}: {status}"
        );
        assert_eq!(
            status_usage(&status, "uncertain_model_requests"),
            uncertain,
            "{point}: {status}"
        );
        assert_eq!(
            status_usage(&status, "reserved_model_requests"),
            0,
            "{point}: {status}"
        );
        for e in events_of(&events, "EffectDispatched") {
            let is_model = events_of(&events, "EffectIntended").iter().any(|i| {
                i["payload"]["effect_id"] == e["payload"]["effect_id"]
                    && i["payload"]["kind"].get("ModelCall").is_some()
            });
            if is_model {
                assert_eq!(
                    e["payload"]["lease_generation"], 1,
                    "{point}: a model call was dispatched again: {e}"
                );
            }
        }
    }
}

#[test]
fn a_permanent_4xx_is_journaled_bounded_and_stops_after_one_send() {
    let cli = Cli::new();
    let api = http::serve(http::Reply::Status(400, "x".repeat(100_000)));
    let contract = cli.model_contract(&cli.repo_copy(), 2, 10);
    let out = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--model",
            "anthropic:claude-opus-5-5",
            "--anthropic-base-url",
            &api.url(),
        ])
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let done: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(done["state"], "FAILED", "{done}");
    let id = done["task_id"].as_str().unwrap();
    assert_eq!(api.hits(), 1);
    let events = cli.events(id);
    assert!(
        events_of(&events, "Failed")[0]["payload"]["Failed"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("http 400: ")
    );
    let reasons: Vec<String> = walk(&cli.home().join("blobs"))
        .iter()
        .filter_map(|f| serde_json::from_slice::<Value>(&fs::read(f).unwrap()).ok())
        .filter_map(|b| b["reason"].as_str().map(str::to_string))
        .filter(|r| r.starts_with("http 400: "))
        .collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons.iter().all(|r| r.len() <= 600),
        "{:?}",
        reasons.iter().map(String::len).collect::<Vec<_>>()
    );
}

#[test]
fn status_and_export_show_the_fake_agent_model_for_patch_tasks() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let done = cli.submit_yes(&contract, &fix_patch());
    let id = done["task_id"].as_str().unwrap();
    assert_eq!(cli.status(id)["model"], "fake-agent");
    let (_, manifest) = cli.export(id, "bundle");
    assert_eq!(manifest["model"], "fake-agent");
    assert_eq!(manifest["model_calls"], json!([]));
}

#[test]
fn model_tasks_run_on_the_firecracker_worker_too() {
    if !fake_mode() {
        println!("SKIPPED: needs AGENTOS_TEST_WORKER=firecracker-fake");
        return;
    }
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let done = cli.submit_model(&contract, &fake_spec("parser-fix.json"));
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let id = done["task_id"].as_str().unwrap();
    assert_tier_worker(&cli, id);
    assert!(
        cli.home().join("tasks").join(id).join("shadow").is_dir(),
        "the reads were served from the host-side shadow workspace"
    );
    assert_eq!(cli.status(id)["model"], "fake:parser-fix.json");
}

#[test]
fn api_key_never_reaches_the_guest() {
    if !real_mode() {
        println!("SKIPPED: needs the KVM tier (AGENTOS_TEST_WORKER=firecracker)");
        return;
    }
    let cli = Cli::new();
    let profile = env_dump_profile(&cli);
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract_for_profile(&profile);
    let out = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--model",
            "anthropic:claude-opus-5-5",
            "--anthropic-base-url",
            &api.url(),
        ])
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let done: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    assert!(
        api.requests().iter().all(|r| r
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == CANARY)),
        "positive control"
    );
    assert_home_free_of(
        &cli,
        "SECRET",
        "the home, job directories (console.log, firecracker.log, vm.json) included",
    );
}

/// The key is never an argument (it would show in `/proc/*/cmdline`): there is no flag for it,
/// and the help names only the file and the environment variable.
#[test]
fn there_is_no_flag_that_takes_the_key_itself() {
    let cli = Cli::bare();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--model",
        "anthropic:x",
        "--api-key",
        CANARY,
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("unexpected argument"));
    let help = String::from_utf8(
        cli.cmd(&["submit", "--help"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        help.contains("--api-key-file") && !help.contains("--api-key <"),
        "{help}"
    );
    assert_nothing_recorded_or_bare(&cli);
}

fn assert_nothing_recorded_or_bare(cli: &Cli) {
    assert!(
        !cli.home().join("agentos.db").exists() || cli.task_footprint() == (Vec::new(), 0),
        "a refused invocation left a task behind"
    );
}

#[test]
fn a_cleartext_non_loopback_base_url_is_refused_before_anything_is_written_and_never_leaks_the_key()
{
    let cli = Cli::new();
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    for url in ["http://example.com", "https://user:pw@api.example/?k=v"] {
        let out = cli
            .cmd(&["submit", &contract, "--yes", "--model", "anthropic:x"])
            .env("AGENTOS_ANTHROPIC_BASE_URL", url)
            .env("ANTHROPIC_API_KEY", CANARY)
            .assert()
            .code(2)
            .get_output()
            .clone();
        let (o, e) = (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
        assert!(e.contains("invalid --anthropic-base-url"), "{e}");
        for text in [&o, &e] {
            assert!(
                !text.contains("SECRET") && !text.contains("pw") && !text.contains("k=v"),
                "{text}"
            );
        }
        assert_nothing_recorded(&cli);
    }
}

#[test]
fn resume_refuses_a_bad_base_url_but_cancel_still_works() {
    let cli = Cli::new();
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let assert = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--model",
            "anthropic:claude-opus-5-5",
            "--anthropic-base-url",
            &api.url(),
            "--crash-at",
            "after-dispatch:model_call",
        ])
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .code(75);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let id = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v["task_id"].as_str().map(str::to_string))
        .unwrap();
    cli.cmd(&["resume", &id])
        .env("AGENTOS_ANTHROPIC_BASE_URL", "http://example.com")
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("invalid --anthropic-base-url"));
    assert_ne!(cli.status(&id)["state"], "SUCCEEDED");
    let cancelled = cli
        .cmd(&["cancel", &id])
        .env("AGENTOS_ANTHROPIC_BASE_URL", "http://example.com")
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&cancelled).unwrap()["state"],
        "CANCELLED"
    );
    assert_eq!(api.hits(), 0, "the crash came before the send");
}

#[test]
fn gc_dry_run_collection_and_exports_preserve_verified_results() {
    let cli = Cli::new();
    let contract = cli.contract(&cli.repo_copy());
    let submitted = cli.submit_yes(&contract, &fix_patch());
    let id = submitted["task_id"].as_str().unwrap();
    let work = cli.home().join("work").join(id);
    let before = cli.path("before-gc");
    cli.json(&["export", id, before.to_str().unwrap()]);
    let dry = cli.json(&["gc", "--dry-run"]);
    assert!(
        dry["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == "candidate"),
        "{dry}"
    );
    assert!(
        work.join(if fake_mode() { "workspace" } else { "ws" })
            .exists()
            || real_mode()
    );
    let report = cli.json(&["gc"]);
    assert!(
        report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == "deleted"),
        "{report}"
    );
    let after = cli.path("after-gc");
    cli.json(&["export", id, after.to_str().unwrap()]);
    assert_eq!(
        fs::read(before.join("patch.diff")).unwrap(),
        fs::read(after.join("patch.diff")).unwrap()
    );
    for entry in fs::read_dir(before.join("evidence")).unwrap() {
        let entry = entry.unwrap();
        assert_eq!(
            fs::read(entry.path()).unwrap(),
            fs::read(after.join("evidence").join(entry.file_name())).unwrap()
        );
    }
    // A second pass finds only collected job remnants (their logs and receipts stay).
    let again = cli.json(&["gc"]);
    assert_eq!(again["summary"]["deleted"], 0, "{again}");
    assert_eq!(again["summary"]["candidate"], 0, "{again}");
    assert_eq!(again["summary"]["refused"], 0, "{again}");
    assert!(
        again["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["status"] == "collected"),
        "{again}"
    );
}

/// A command that opens (and so creates) the home of a bare `Cli`, then fails on the task.
fn create_home(cli: &Cli) {
    cli.cmd(&["status", "00000000-0000-0000-0000-000000000000"])
        .assert()
        .code(1);
    assert!(cli.home().join("agentos.db").is_file());
}

#[test]
fn gc_never_creates_a_home() {
    let cli = Cli::bare();
    for args in [&["gc", "--dry-run"][..], &["gc"][..]] {
        cli.cmd(args)
            .assert()
            .code(2)
            .stderr(predicate::str::contains("not an agentos home"));
        assert!(!cli.home().exists());
    }
}

#[test]
fn gc_prints_its_report_and_exits_1_when_it_refuses() {
    let cli = Cli::bare();
    create_home(&cli);
    let unknown = cli.home().join("model").join("unknown");
    fs::create_dir_all(&unknown).unwrap();
    let out = cli
        .cmd(&["gc"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("gc refused 1"))
        .get_output()
        .stdout
        .clone();
    let report: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(report["summary"]["refused"], 1, "{report}");
    assert_eq!(report["entries"][0]["path"], "model/unknown", "{report}");
    assert_eq!(report["entries"][0]["status"], "refused", "{report}");
    assert!(unknown.is_dir());
}

#[test]
fn gc_takes_the_driver_lock_before_opening_the_home() {
    let cli = Cli::bare();
    create_home(&cli);
    let tasks = cli.home().join("tasks");
    fs::remove_dir_all(&tasks).unwrap();
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cli.home().join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    cli.cmd(&["gc", "--dry-run"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("another agentos process"));
    assert!(
        !tasks.exists(),
        "the home was opened before the lock was taken"
    );
}

#[test]
fn gc_refuses_while_another_driver_holds_the_lock() {
    let cli = Cli::bare();
    create_home(&cli);
    let lock = fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cli.home().join("driver.lock"))
        .unwrap();
    lock.lock().unwrap();
    cli.cmd(&["gc"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("another agentos process"));
}

#[test]
fn cancel_of_an_anthropic_task_needs_no_key_and_never_contacts_the_api() {
    let cli = Cli::new();
    let api = fake_api("parser-fix-direct.json");
    let contract = cli.model_contract(&cli.repo_copy(), 12, 10);
    let assert = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--model",
            "anthropic:claude-opus-5-5",
            "--anthropic-base-url",
            &api.url(),
            "--crash-at",
            "after-dispatch:model_call",
        ])
        .env("ANTHROPIC_API_KEY", CANARY)
        .assert()
        .code(75);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    let id = stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v["task_id"].as_str().map(str::to_string))
        .unwrap();
    // Neither a key in the environment nor a readable key file: recovery does not look.
    let absent = cli.path("no-such-key");
    let cancelled = cli
        .cmd(&["cancel", &id, "--api-key-file", absent.to_str().unwrap()])
        .env_remove("ANTHROPIC_API_KEY")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        serde_json::from_slice::<Value>(&cancelled).unwrap()["state"],
        "CANCELLED"
    );
    assert_eq!(api.hits(), 0, "the crash came before the send");
}

/// The plain contract with `resources` merged into its `limits`.
fn contract_with_resources(cli: &Cli, name: &str, resources: Value) -> String {
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    for (k, v) in resources.as_object().unwrap() {
        contract["limits"][k] = v.clone();
    }
    cli.write(&format!("{name}.json"), &contract.to_string())
}

/// `submit --yes --crash-at` on the fake Firecracker worker: dies with 75; returns the task id.
fn crash_fc(cli: &Cli, contract: &str) -> String {
    let assert = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
                "--crash-at",
                "during-execute:apply_patch",
            ],
        )
        .assert()
        .code(75);
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v["task_id"].as_str().map(str::to_string))
        .expect("the crash report names the task")
}

fn edit_submitted(cli: &Cli, id: &str, sql_expr: &str) {
    rusqlite::Connection::open(cli.home().join("agentos.db"))
        .unwrap()
        .execute(
            &format!("UPDATE events SET payload={sql_expr} WHERE task_id=?1 AND type='Submitted'"),
            [id],
        )
        .unwrap();
}

#[test]
fn submit_records_the_resolved_vm_resources_for_firecracker_tasks_only() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let plain = cli.submit_fc(&cli.contract(&cli.repo_copy()), &[]);
    assert_eq!(plain["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(plain["task_id"].as_str().unwrap())["vm_resources"],
        json!({ "version": 1, "disk_mib": 1024, "scratch_mib": 512, "bandwidth_mib_s": null, "iops": null })
    );
    let sized = contract_with_resources(
        &cli,
        "sized",
        json!({ "worker_disk_mib": 2048, "worker_scratch_mib": 768,
                "worker_disk_bandwidth_mib_s": 64, "worker_disk_iops": 5000 }),
    );
    let out = cli.submit_fc(&sized, &[]);
    assert_eq!(out["state"], "SUCCEEDED");
    assert_eq!(
        cli.submitted(out["task_id"].as_str().unwrap())["vm_resources"],
        json!({ "version": 1, "disk_mib": 2048, "scratch_mib": 768, "bandwidth_mib_s": 64, "iops": 5000 })
    );
    let host = cli.json_as(
        Mode::Plain,
        &[
            "submit",
            &sized,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    );
    assert!(
        cli.submitted(host["task_id"].as_str().unwrap())
            .get("vm_resources")
            .is_none(),
        "the host worker has no drives"
    );
}

#[test]
fn resume_refuses_vm_resources_that_disagree_with_the_contract_and_journals_nothing() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let sized = contract_with_resources(&cli, "sized", json!({ "worker_disk_mib": 2048 }));

    // A record that differs from what the stored contract resolves to.
    let id = crash_fc(&cli, &sized);
    edit_submitted(
        &cli,
        &id,
        "json_set(payload, '$.vm_resources.disk_mib', 4096)",
    );
    let before = cli.events(&id);
    cli.cmd_as(Mode::Fake, &["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("vm_resources"));
    assert_eq!(cli.events(&id), before);

    // No record (version 0) but a contract that asks for resources: also inconsistent.
    let id = crash_fc(&cli, &sized);
    edit_submitted(&cli, &id, "json_remove(payload, '$.vm_resources')");
    let before = cli.events(&id);
    cli.cmd_as(Mode::Fake, &["resume", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("vm_resources"));
    assert_eq!(cli.events(&id), before);
}

/// A task submitted before resources were recorded resumes with the version-0 values.
#[test]
fn a_task_without_recorded_vm_resources_resumes_with_version_zero() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let id = crash_fc(&cli, &cli.contract(&cli.repo_copy()));
    edit_submitted(&cli, &id, "json_remove(payload, '$.vm_resources')");
    let done = cli.json_as(Mode::Fake, &["resume", &id]);
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let dir = cli.path("legacy-bundle");
    let manifest = cli.json_as(Mode::Plain, &["export", &id, dir.to_str().unwrap()]);
    assert_eq!(
        manifest["vm_resources"],
        json!({ "version": 0, "disk_mib": 1024, "scratch_mib": 512, "bandwidth_mib_s": null, "iops": null })
    );
}

#[test]
fn the_approval_summary_shows_the_vm_disks_of_a_firecracker_task() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let summary = |contract: &str| {
        let out = cli
            .cmd_as(Mode::Fake, &["submit", contract])
            .assert()
            .success()
            .get_output()
            .stderr
            .clone();
        String::from_utf8(out).unwrap()
    };
    let plain = summary(&cli.contract(&cli.repo_copy()));
    assert!(
        plain.contains(
            "  vm disks:             workspace 1024 MiB, scratch 512 MiB, no rate limit\n"
        ),
        "{plain}"
    );
    let limited = contract_with_resources(
        &cli,
        "limited",
        json!({ "worker_disk_mib": 2048, "worker_disk_bandwidth_mib_s": 64, "worker_disk_iops": 5000 }),
    );
    let text = summary(&limited);
    assert!(
        text.contains("  vm disks:             workspace 2048 MiB, scratch 512 MiB, each writable drive at most 64 MiB/s and 5000 operations/s\n"),
        "{text}"
    );
}

#[test]
fn submit_refuses_a_firecracker_task_the_host_disk_cannot_hold() {
    let cli = Cli::bare();
    cli.register_guest_image();
    let contract = contract_with_resources(&cli, "big", json!({ "worker_disk_mib": 4096 }));
    cli.cmd_as(
        Mode::Fake,
        &[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ],
    )
    .env("AGENTOS_TEST_HOST_FREE_MIB", "4000")
    .assert()
    .code(1)
    .stderr(predicate::str::contains("host disk: 4000 MiB free under "))
    .stderr(predicate::str::contains("the VM may write 4608 MiB"));
    cli.assert_no_task();
    let out = cli
        .cmd_as(
            Mode::Fake,
            &[
                "submit",
                &contract,
                "--yes",
                "--fake-agent-patch",
                fix_patch().to_str().unwrap(),
            ],
        )
        .env("AGENTOS_TEST_HOST_FREE_MIB", "4608")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let done: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
}

/// Kill and resume on the real, jailed worker with non-default resources: the recorded
/// sizes and rates are used before and after the crash, and the export carries them.
#[test]
fn a_task_with_contracted_vm_resources_survives_a_kill_on_the_real_worker() {
    let Some(kvm) = kvm::require() else { return };
    let cli = Cli::bare();
    cli.json_as(
        Mode::Plain,
        &["image", "register", kvm.image_dir.to_str().unwrap()],
    );
    let contract = contract_with_resources(
        &cli,
        "resources",
        json!({ "worker_disk_mib": 1536, "worker_scratch_mib": 768,
                "worker_disk_bandwidth_mib_s": 64, "worker_disk_iops": 10000 }),
    );
    let real = |args: &[&str]| cli.cmd_as(Mode::Real, args);
    let crashed = real(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
        "--crash-at",
        "during-execute:apply_patch",
    ])
    .assert()
    .code(75)
    .get_output()
    .stderr
    .clone();
    let id = String::from_utf8_lossy(&crashed)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| v["task_id"].as_str().map(str::to_string))
        .unwrap();
    let recorded = json!({ "version": 1, "disk_mib": 1536, "scratch_mib": 768,
                           "bandwidth_mib_s": 64, "iops": 10000 });
    assert_eq!(cli.submitted(&id)["vm_resources"], recorded);
    let resumed: Value = serde_json::from_slice(
        &real(&["resume", &id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(resumed, json!({ "task_id": id, "state": "SUCCEEDED" }));
    assert_eq!(
        fs::metadata(cli.home().join("work").join(&id).join("ws.img"))
            .unwrap()
            .len(),
        1536 << 20
    );
    let bundle = cli.path("resources-bundle");
    let manifest: Value = serde_json::from_slice(
        &real(&["export", &id, bundle.to_str().unwrap()])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert_eq!(manifest["vm_resources"], recorded);
    assert_eq!(
        manifest["verified_digest"],
        manifest["final_workspace_digest"]
    );
}

fn component_fixture(name: &str) -> PathBuf {
    fixtures().join("components").join(name)
}

#[test]
fn component_register_is_content_addressed_and_lists_entries() {
    let cli = Cli::bare();
    let first = cli.json(&[
        "component",
        "register",
        component_fixture("repo-analyzer-v1").to_str().unwrap(),
    ]);
    assert_eq!(first["id"], "repo-analyzer-v1");
    let digest = first["digest"].as_str().unwrap().to_string();
    assert_eq!(digest.len(), 64);
    let again = cli.json(&[
        "component",
        "register",
        component_fixture("repo-analyzer-v1").to_str().unwrap(),
    ]);
    assert_eq!(again, first, "the same bytes twice change nothing");
    let entry = cli
        .home()
        .join("registry/components")
        .join(format!("repo-analyzer-v1@{digest}"));
    assert!(entry.join("component.wasm").is_file() && entry.join("component.json").is_file());
    let listed = cli.json(&["component", "list"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(
        (listed[0]["id"].clone(), listed[0]["digest"].clone()),
        (json!("repo-analyzer-v1"), json!(digest))
    );
    // Not a profile: the profile registry does not list it.
    assert_eq!(cli.json(&["profile", "list"]), json!([]));
}

#[test]
fn component_register_refuses_what_is_not_an_analyzer() {
    let cli = Cli::bare();
    for (fixture, message) in [
        ("wasi-import-v1", "imports wasi:cli/environment@0.2.0"),
        ("no-export-v1", "does not export analyze"),
    ] {
        cli.cmd(&[
            "component",
            "register",
            component_fixture(fixture).to_str().unwrap(),
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains(message));
    }
    let bad = cli.path("bad-component");
    fs::create_dir_all(&bad).unwrap();
    fs::write(
        bad.join("component.json"),
        r#"{"id":"bad-v1","world":"agentos:analyzer/analyzer@1.0.0"}"#,
    )
    .unwrap();
    fs::write(bad.join("component.wasm"), b"not wasm").unwrap();
    cli.cmd(&["component", "register", bad.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("not a component"));
    fs::write(
        bad.join("component.json"),
        r#"{"id":"bad-v1","world":"other:world/x@1.0.0"}"#,
    )
    .unwrap();
    fs::copy(
        component_fixture("repo-analyzer-v1").join("component.wasm"),
        bad.join("component.wasm"),
    )
    .unwrap();
    cli.cmd(&["component", "register", bad.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("agentos:analyzer/analyzer@1.0.0"));
    fs::write(
        bad.join("component.json"),
        r#"{"id":"../x","world":"agentos:analyzer/analyzer@1.0.0"}"#,
    )
    .unwrap();
    cli.cmd(&["component", "register", bad.to_str().unwrap()])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("plain name"));
    assert_eq!(cli.json(&["component", "list"]), json!([]));
}

/// The plain contract with the `snapshot.analyze` capability and an analyzer pin.
fn contract_with_analyzer(cli: &Cli, id: &str, digest: &str) -> String {
    let mut contract: Value =
        serde_json::from_str(&fs::read_to_string(cli.contract(&cli.repo_copy())).unwrap()).unwrap();
    contract["capabilities"]
        .as_array_mut()
        .unwrap()
        .push(json!("snapshot.analyze"));
    contract["analyzer"] = json!({ "id": id, "digest": digest });
    cli.write(&format!("analyzer-{id}.json"), &contract.to_string())
}

#[test]
fn a_task_with_an_analyzer_runs_it_once_and_exports_the_report() {
    let cli = Cli::new();
    let registered = cli.json(&[
        "component",
        "register",
        component_fixture("repo-analyzer-v1").to_str().unwrap(),
    ]);
    let digest = registered["digest"].as_str().unwrap().to_string();
    let contract = contract_with_analyzer(&cli, "repo-analyzer-v1", &digest);
    let out = cli
        .cmd(&[
            "submit",
            &contract,
            "--yes",
            "--fake-agent-patch",
            fix_patch().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "  analyzer:             repo-analyzer-v1@{digest}"
        )),
        "{stderr}"
    );
    assert!(stderr.contains("snapshot.analyze"), "{stderr}");
    let done: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(done["state"], "SUCCEEDED", "{done}");
    let id = done["task_id"].as_str().unwrap().to_string();
    let submitted = cli.submitted(&id);
    assert_eq!(
        (
            submitted["analyzer_id"].clone(),
            submitted["analyzer_digest"].clone()
        ),
        (json!("repo-analyzer-v1"), json!(digest)),
        "{submitted}"
    );
    assert!(
        cli.home()
            .join("tasks")
            .join(&id)
            .join("analyzer/component.wasm")
            .is_file()
    );
    let analyses: Vec<Value> = cli
        .events(&id)
        .into_iter()
        .filter(|e| e["type"] == "EffectIntended" && e["payload"]["kind"] == "AnalyzeSnapshot")
        .collect();
    assert_eq!(analyses.len(), 1);

    let bundle = cli.path("analysis-bundle");
    let manifest = cli.json(&["export", &id, bundle.to_str().unwrap()]);
    assert_eq!(
        manifest["analysis"]["component_digest"],
        json!(digest),
        "{manifest}"
    );
    assert_eq!(manifest["analysis"]["state"], "COMPLETED");
    assert_eq!(manifest["analysis"]["file"], "analysis/report.json");
    let report: Value =
        serde_json::from_slice(&fs::read(bundle.join("analysis/report.json")).unwrap()).unwrap();
    assert_eq!(report["analyzer"], "repo-analyzer-v1");
    assert_eq!(
        manifest["analysis"]["report_digest"],
        json!(Digest::of(&fs::read(bundle.join("analysis/report.json")).unwrap()).to_string())
    );
}

#[test]
fn an_unregistered_analyzer_is_refused_at_submit() {
    let cli = Cli::new();
    let contract = contract_with_analyzer(&cli, "repo-analyzer-v1", &"a".repeat(64));
    cli.cmd(&[
        "submit",
        &contract,
        "--yes",
        "--fake-agent-patch",
        fix_patch().to_str().unwrap(),
    ])
    .assert()
    .code(2)
    .stderr(predicate::str::contains("not in the registry"));
    cli.assert_no_task();
}

#[test]
fn a_task_without_an_analyzer_exports_no_analysis() {
    let cli = Cli::new();
    let id = cli.submit_yes(&cli.contract(&cli.repo_copy()), &fix_patch())["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let bundle = cli.path("plain-bundle");
    let manifest = cli.json(&["export", &id, bundle.to_str().unwrap()]);
    assert!(manifest.get("analysis").is_none(), "{manifest}");
    assert!(!bundle.join("analysis").exists());
}

#[test]
fn version_reports_what_the_code_speaks() {
    let cli = Cli::bare();
    let v = cli.json(&["version"]);
    assert_eq!(v["agentos"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["guest_protocol"], 1);
    assert_eq!(v["model_policy_version"], 1);
    assert_eq!(v["model_limits_version"], 1);
    assert_eq!(v["vm_resources_version"], 1);
    assert_eq!(v["analyzer_runtime"], "wasmtime 49.0.2");
    assert_eq!(v["analyzer_world"], "agentos:analyzer/analyzer@1.0.0");
    assert_eq!(v["firecracker"], "Firecracker v1.17.");
}

/// The host check reports every requirement; the jail is required unless --allow-unjailed.
#[test]
fn host_check_reports_each_requirement_and_fails_on_a_required_one() {
    let cli = Cli::bare();
    let check = |extra: &[&str], probe: &str| {
        let mut args = vec!["--firecracker", "/nonexistent/firecracker"];
        args.extend_from_slice(extra);
        args.push("host-check");
        let out = cli
            .cmd_as(Mode::Plain, &args)
            .env("AGENTOS_TEST_WORKERS", "1")
            .env("AGENTOS_TEST_JAIL_PROBE", probe)
            .assert()
            .code(1)
            .get_output()
            .clone();
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let report = check(&[], "fail:cgroup v2 hierarchy /sys/fs/cgroup is read-only");
    for key in ["kvm", "firecracker", "git", "jail"] {
        assert!(report.get(key).is_some(), "{report}");
    }
    assert_eq!(report["git"], "ok");
    assert!(
        report["firecracker"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/firecracker"),
        "{report}"
    );
    assert_eq!(
        report["jail"],
        "cgroup v2 hierarchy /sys/fs/cgroup is read-only"
    );
    assert_eq!(report["jail_required"], true);
    let unjailed = check(&["--allow-unjailed"], "fail:needs root");
    assert_eq!(unjailed["jail_required"], false);
    assert_eq!(unjailed["jail"], "needs root");
    assert_eq!(check(&[], "ok")["jail"], "ok");
}

/// Registration works for an unprivileged owner: a directory moved to another parent needs
/// its own write bit, which root bypasses (the test tier runs as root, so it drops to nobody).
#[test]
fn an_unprivileged_owner_can_register() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    let cli = Cli::bare();
    let source = cli.path("unprivileged-profile");
    copy_tree(&fixtures().join("profiles/parser-checks-v1"), &source).unwrap();
    let home = cli.path("unprivileged-home");
    fs::create_dir_all(&home).unwrap();
    let root = rustix::process::geteuid().is_root();
    if root {
        for p in [cli.path(""), source.clone(), home.clone()] {
            std::os::unix::fs::chown(&p, Some(65534), Some(65534)).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        for f in fs::read_dir(&source).unwrap() {
            std::os::unix::fs::chown(f.unwrap().path(), Some(65534), Some(65534)).unwrap();
        }
    }
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_agentos"));
    cmd.args([
        "--home",
        home.to_str().unwrap(),
        "profile",
        "register",
        source.to_str().unwrap(),
    ]);
    if root {
        cmd.uid(65534).gid(65534);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let entry = fs::read_dir(home.join("registry"))
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("parser-checks-v1@")
                && e.path().is_dir()
        })
        .expect("the entry");
    assert_eq!(
        fs::metadata(entry.path()).unwrap().permissions().mode() & 0o777,
        0o555
    );
}
