//! Worker conformance: one table of cases run against `HostProcessWorker` and against
//! `FirecrackerWorker` over the fake guest (jailed through the fake jailer with
//! `AGENTOS_TEST_JAIL=fake`), each in a fresh root with the same task, requests and attempt
//! contexts, comparing every observation field by field with nothing normalized; and, in
//! the KVM tier (`kvm::require()`), the same table against the real, jailed guest (the
//! `Real` column). Then the kill paths and the failure mapping of the Firecracker worker
//! through the real `agentos-supervisor` binary, and the controller's rejection of a
//! guest's forged verification digest. Apart from the `Real` column: no KVM, no root.

mod common;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, EffectState, Outcome};
use agentos_core::guest::{
    Frame, Message, Mode, RAW_FRAME_LIMIT, b64, mint_attempt_token, read_frame, write_frame,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::lease::EffectTimeouts;
use agentos_core::state::TaskState;
use agentos_engine::agent::FakeAgent;
use agentos_engine::crash::{CrashHook, CrashPoint};
use agentos_engine::executor::{
    AttemptCtx, EffectRequest, ExecOutcome, Executor, JobWait, Reconciliation,
};
use agentos_engine::firecracker::{FirecrackerConfig, FirecrackerWorker, Inspector};
use agentos_engine::guestlink::{GuestLauncher, socket_path};
use agentos_engine::jail::JailMode;
use agentos_engine::job::{HostConfig, JobDir, JobRequest, JobState, KillReason, WorkerConfig};
use agentos_engine::runner::run_task;
use agentos_engine::supervised::{ExecCounts, Reconciler, SupervisedExecutor};
use agentos_engine::worker::{HostProcessWorker, Worker};
use agentos_engine::workspace::workspace_digest;
use common::{
    Env, SUPERVISOR_BIN, TEST_WORKERS_ENV, comment_patch, contract, copy_dir, create_patch,
    debugfs_write, fake_firecracker_config, fix_patch, fixtures, host_config,
    jailed_fake_firecracker_config, kvm, proc_state, processes_naming, supervised, test_jail_fake,
};
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

const NEVER_LISTEN: &str = "AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN";
const KILL_VM_AFTER_REQUEST: &str = "AGENTOS_TEST_KILL_VM_AFTER_REQUEST";
/// The README's digest of the fixture snapshot (`fixtures/parser-repo`).
const GOLDEN_SNAPSHOT_DIGEST: &str =
    "be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8";
/// Upper bound for anything a test waits on outside the engine.
const PATIENCE: Duration = Duration::from_secs(20);

fn test_env() -> Vec<(String, String)> {
    vec![(TEST_WORKERS_ENV.into(), "1".into())]
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn ctx(lease: u64) -> AttemptCtx {
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: lease,
        worker: "conformance".into(),
    }
}

fn text(out: &ExecOutcome) -> String {
    String::from_utf8_lossy(&out.output).into_owned()
}

fn reason(out: &ExecOutcome) -> String {
    match &out.receipt.outcome {
        Outcome::Failure(r) => r.clone(),
        Outcome::Success => panic!("expected a failure, got success: {}", text(out)),
    }
}

fn succeeded(out: &ExecOutcome) {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", text(out));
}

fn evidence(out: &ExecOutcome) -> serde_json::Value {
    succeeded(out);
    serde_json::from_slice(&out.output).unwrap()
}

/// A fresh root with the fixture snapshot and verification profile.
fn fresh_root() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(
        &fixtures().join("parser-repo"),
        &dir.path().join("snapshot"),
    );
    copy_dir(
        &fixtures().join("profiles/parser-checks-v1"),
        &dir.path().join("profile"),
    );
    dir
}

/// Points `root`'s profile at `command` (the worker appends the workspace path).
fn set_profile(root: &Path, command: serde_json::Value) {
    let profile = serde_json::json!({ "id": "conformance", "command": command, "protected": true });
    fs::write(root.join("profile/profile.json"), profile.to_string()).unwrap();
}

fn snapshot_digest() -> Digest {
    workspace_digest(&fixtures().join("parser-repo")).unwrap()
}

// ---------------------------------------------------------------------------------------
// The conformance table.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Host,
    Fake,
    /// The real guest image under the real Firecracker and jailer (KVM tier).
    Real,
}

/// Per-case worker settings, the same on both sides.
#[derive(Debug, Clone, Copy)]
struct Settings {
    pinned: Option<Digest>,
    timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            pinned: None,
            timeout_secs: 60,
        }
    }
}

/// One side of a case: a fresh root and the worker over it.
struct Side {
    kind: Kind,
    dir: TempDir,
    task: TaskId,
    host: HostConfig,
    fc: FirecrackerConfig,
}

impl Side {
    fn new(kind: Kind, task: &TaskId, settings: Settings) -> Side {
        Side::build(kind, task, settings, None)
    }

    /// The `Real` side: a root on the guest image's filesystem, the jailed real worker.
    fn real(kvm: &kvm::Kvm, task: &TaskId, settings: Settings) -> Side {
        Side::build(Kind::Real, task, settings, Some(kvm))
    }

    fn build(kind: Kind, task: &TaskId, settings: Settings, kvm: Option<&kvm::Kvm>) -> Side {
        let dir = match kvm {
            Some(kvm) => {
                let dir = kvm.root();
                copy_dir(
                    &fixtures().join("parser-repo"),
                    &dir.path().join("snapshot"),
                );
                copy_dir(
                    &fixtures().join("profiles/parser-checks-v1"),
                    &dir.path().join("profile"),
                );
                dir
            }
            None => fresh_root(),
        };
        let mut host = host_config(dir.path());
        host.profile_digest = settings.pinned;
        host.verify_timeout_secs = settings.timeout_secs;
        let mut fc = match kvm {
            Some(kvm) => kvm.jailed_config(dir.path()),
            None if test_jail_fake() => jailed_fake_firecracker_config(dir.path()),
            None => fake_firecracker_config(dir.path()),
        };
        fc.profile_digest = settings.pinned;
        fc.verify_timeout_secs = settings.timeout_secs;
        Side {
            kind,
            dir,
            task: task.clone(),
            host,
            fc,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn task_dir(&self) -> PathBuf {
        self.root().join("work").join(self.task.as_str())
    }

    /// The workspace tree on the host: the host worker's directory, or the fake guest's
    /// view of `ws.img`. The real guest's tree is inside the ext4 `ws.img` (`plant_*`).
    fn ws(&self) -> PathBuf {
        match self.kind {
            Kind::Host => self.task_dir().join("ws"),
            Kind::Fake => self.task_dir().join("workspace"),
            Kind::Real => panic!("the real guest's workspace is inside ws.img"),
        }
    }

    /// The workspace is gone: the host directory, or `ws.img` (with the fake guest's tree).
    fn lose_workspace(&self) {
        match self.kind {
            Kind::Host => fs::remove_dir_all(self.ws()).unwrap(),
            Kind::Fake => {
                fs::remove_file(self.task_dir().join("ws.img")).unwrap();
                fs::remove_dir_all(self.ws()).unwrap();
            }
            Kind::Real => fs::remove_file(self.task_dir().join("ws.img")).unwrap(),
        }
    }

    /// A symlink `src/<name>` to `target` planted in the workspace behind the worker's back
    /// (in the real guest's image with `debugfs`).
    fn plant_symlink(&self, name: &str, target: &str) {
        match self.kind {
            Kind::Real => debugfs_write(
                &self.task_dir().join("ws.img"),
                &format!("cd /src\nsymlink {name} {target}\n"),
            ),
            _ => std::os::unix::fs::symlink(target, self.ws().join("src").join(name)).unwrap(),
        }
    }

    /// A file `src/<name>` planted in the workspace behind the worker's back.
    fn plant_file(&self, name: &str, content: &str) {
        match self.kind {
            Kind::Real => {
                let local = self.root().join(format!("planted-{name}"));
                fs::write(&local, content).unwrap();
                debugfs_write(
                    &self.task_dir().join("ws.img"),
                    &format!("cd /src\nwrite {} {name}\n", local.display()),
                );
            }
            _ => fs::write(self.ws().join("src").join(name), content).unwrap(),
        }
    }

    fn inspector(&self) -> Reconciler {
        Reconciler::Firecracker(
            Inspector::new(self.fc.clone(), self.root().join("inspect")).with_env(test_env()),
        )
    }

    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match self.kind {
            Kind::Host => HostProcessWorker::new(&self.host, None).run(req, ctx).await,
            Kind::Fake | Kind::Real => {
                let job = JobDir::create(
                    &self.root().join("jobs"),
                    &job_request(
                        req,
                        ctx,
                        WorkerConfig::Firecracker(self.fc.clone()),
                        i64::MAX,
                        0,
                    ),
                )
                .unwrap()
                .0;
                FirecrackerWorker::new(&self.fc, &job)
                    .with_env(test_env())
                    .run(req, ctx)
                    .await
            }
        }
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        match self.kind {
            Kind::Host => {
                HostProcessWorker::new(&self.host, None)
                    .reconcile(req, ctx)
                    .await
            }
            Kind::Fake | Kind::Real => self.inspector().reconcile(req, ctx).await,
        }
    }

    fn current(&self) -> Option<Result<Digest, String>> {
        match self.kind {
            Kind::Host => HostProcessWorker::new(&self.host, None).current_workspace(&self.task),
            Kind::Fake | Kind::Real => self.inspector().current_workspace(&self.task),
        }
    }
}

fn job_request(
    req: &EffectRequest,
    ctx: &AttemptCtx,
    worker: WorkerConfig,
    lease_expiry_ms: i64,
    task_deadline_ms: i64,
) -> JobRequest {
    JobRequest {
        effect_id: req.effect_id.clone(),
        task_id: req.task_id.clone(),
        kind: req.kind.clone(),
        payload: req.payload.clone(),
        contract: req.contract.clone(),
        attempt_id: ctx.attempt_id.clone(),
        lease_generation: ctx.lease_generation,
        lease_expiry_ms,
        task_deadline_ms,
        worker,
    }
}

/// What a step asks of the worker; `Do` changes the side's world instead.
enum Step {
    Snapshot,
    /// A patch against the snapshot's digest.
    Patch(String),
    /// A patch against another base.
    PatchOn(String, Digest),
    Verify,
    Reconcile(String),
    Current,
    Do(fn(&Side)),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Obs {
    Out(ExecOutcome),
    Recon(Reconciliation),
    Current(Option<Result<Digest, String>>),
    Done,
}

impl Obs {
    fn out(&self) -> &ExecOutcome {
        match self {
            Obs::Out(o) => o,
            other => panic!("expected an outcome, got {other:?}"),
        }
    }
}

struct Case {
    name: &'static str,
    settings: Settings,
    steps: Vec<Step>,
    /// What the case is about, asserted on the host's observations (the fake's are equal).
    check: fn(&[Obs]),
    /// Only where the real guest's isolation makes the outcome differ by design: what the
    /// `Real` column observes instead (asserted on the real observations; every other
    /// case's real observations must equal the host's).
    real: Option<fn(&[Obs])>,
}

fn case(name: &'static str, steps: Vec<Step>, check: fn(&[Obs])) -> Case {
    Case {
        name,
        settings: Settings::default(),
        steps,
        check,
        real: None,
    }
}

/// A git binary patch creating `src/blob.bin` (made with `git diff --binary`).
const BINARY_PATCH: &str = "diff --git a/src/blob.bin b/src/blob.bin\nnew file mode 100644\nindex 0000000000000000000000000000000000000000..b43761b27df02a0c6c305120d37445368d1ac5e1\nGIT binary patch\nliteral 10\nRcmZQzWJ=1+ODwAV4*(1}1Bd_s\n\nliteral 0\nHcmV?d00001\n\n";

fn failed_with(obs: &Obs, prefix: &str) {
    let why = reason(obs.out());
    assert!(
        why.starts_with(prefix),
        "expected a failure starting with {prefix:?}, got {why:?}"
    );
}

fn cases() -> Vec<Case> {
    use Step::*;
    vec![
        case("snapshot_digest_equals_host_digest", vec![Snapshot], |o| {
            succeeded(o[0].out());
            assert_eq!(o[0].out().new_workspace, Some(snapshot_digest()));
        }),
        case(
            "patch_applies_with_identical_result_bytes",
            vec![Snapshot, Patch(fix_patch())],
            |o| {
                let v = evidence(o[1].out());
                assert_eq!(v["applied"], true);
                assert_eq!(v["paths"], serde_json::json!(["src/parser.py"]));
                assert_ne!(o[1].out().new_workspace, Some(snapshot_digest()));
            },
        ),
        case(
            "version_conflict",
            vec![Snapshot, PatchOn(fix_patch(), Digest::of(b"another base"))],
            |o| failed_with(&o[1], "version conflict: expected "),
        ),
        case(
            "non_editable_path",
            vec![Snapshot, Patch(create_patch("README.extra", "x"))],
            |o| failed_with(&o[1], "path not editable: README.extra"),
        ),
        // `src/**` never matches a path with a `..` component (the contract's rule).
        case(
            "traversal_path",
            vec![Snapshot, Patch(create_patch("src/../../escape.py", "x"))],
            |o| failed_with(&o[1], "path not editable: src/../../escape.py"),
        ),
        case(
            "symlink_on_path",
            vec![
                Snapshot,
                Do(|s| s.plant_symlink("link", "/tmp")),
                Patch(create_patch("src/link/planted.py", "x")),
            ],
            |o| failed_with(&o[2], "path src/link/planted.py crosses symlink src/link"),
        ),
        case(
            "excluded_component",
            vec![
                Snapshot,
                Patch(create_patch("src/__pycache__/planted.py", "x")),
            ],
            |o| {
                failed_with(
                    &o[1],
                    "path excluded from the workspace digest: src/__pycache__/planted.py",
                )
            },
        ),
        case(
            "binary_patch",
            vec![Snapshot, Patch(BINARY_PATCH.into())],
            |o| failed_with(&o[1], "binary patches are not supported: src/blob.bin"),
        ),
        case("empty_patch", vec![Snapshot, Patch(String::new())], |o| {
            failed_with(&o[1], "invalid patch: ")
        }),
        case(
            "verification_passes_with_identical_evidence",
            vec![Snapshot, Patch(fix_patch()), Verify],
            |o| {
                let v = evidence(o[2].out());
                assert_eq!(v["passed"], true, "{v}");
                assert!(o[2].out().verification.as_ref().unwrap().passed);
            },
        ),
        case(
            "verification_fails_with_identical_evidence",
            vec![Snapshot, Patch(comment_patch()), Verify],
            |o| {
                let v = evidence(o[2].out());
                assert_eq!(v["passed"], false, "{v}");
                assert_ne!(v["exit_code"], 0);
            },
        ),
        Case {
            name: "pinned_profile_mismatch",
            settings: Settings {
                pinned: Some(Digest::of(b"some other profile")),
                ..Settings::default()
            },
            steps: vec![Snapshot, Verify],
            check: |o| failed_with(&o[1], "profile digest mismatch: pinned "),
            real: None,
        },
        Case {
            name: "verification_timeout",
            settings: Settings {
                timeout_secs: 1,
                ..Settings::default()
            },
            steps: vec![
                Snapshot,
                Do(|s| set_profile(s.root(), serde_json::json!(["sh", "-c", "sleep 5", "sh"]))),
                Verify,
            ],
            check: |o| failed_with(&o[2], "timeout"),
            real: None,
        },
        case(
            "oversized_output_truncation_flags",
            vec![
                Snapshot,
                Do(|s| {
                    set_profile(
                        s.root(),
                        serde_json::json!([
                            "python3",
                            "-c",
                            "import sys; sys.stdout.write('o' * 100000); sys.stderr.write('e' * 70000); print('\\nPASSED')"
                        ]),
                    )
                }),
                Verify,
            ],
            |o| {
                let v = evidence(o[2].out());
                assert_eq!(
                    (v["stdout_truncated"].clone(), v["stderr_truncated"].clone()),
                    (serde_json::json!(true), serde_json::json!(true)),
                    "{v}"
                );
                assert_eq!(v["exit_code"], 0);
            },
        ),
        Case {
            real: Some(|o| {
                // The real check (uid 1001) cannot write to /workspace (builder's, 0755):
                // it fails on its own and the workspace stays clean.
                let v = evidence(o[2].out());
                assert_eq!(
                    (v["passed"].clone(), v["exit_code"].clone()),
                    (serde_json::json!(false), serde_json::json!(1)),
                    "{v}"
                );
                assert!(v["stderr"].as_str().unwrap().contains("PermissionError: [Errno 13] Permission denied: '/workspace/src/__pycache__'"), "{v}");
            }),
            ..case(
                "check_pollution_voids_evidence",
                vec![
                    Snapshot,
                    Do(|s| {
                        set_profile(
                            s.root(),
                            serde_json::json!([
                                "python3",
                                "-c",
                                "import os, sys; os.makedirs(os.path.join(sys.argv[1], 'src', '__pycache__')); print('PASSED')"
                            ]),
                        )
                    }),
                    Verify,
                ],
                |o| {
                    failed_with(
                        &o[2],
                        "workspace polluted by excluded entries: src/__pycache__",
                    )
                },
            )
        },
        case(
            "reconcile_not_applied",
            vec![Snapshot, Reconcile(fix_patch())],
            |o| {
                assert_eq!(o[1], Obs::Recon(Reconciliation::NotApplied));
            },
        ),
        case(
            "reconcile_applied",
            vec![Snapshot, Patch(fix_patch()), Reconcile(fix_patch())],
            |o| {
                let Obs::Recon(Reconciliation::Applied(out)) = &o[2] else {
                    panic!("{:?}", o[2])
                };
                assert_eq!(out.new_workspace, o[1].out().new_workspace);
            },
        ),
        case(
            "reconcile_unknown_on_tampered_workspace",
            vec![
                Snapshot,
                Do(|s| s.plant_file("stray.py", "x = 1\n")),
                Reconcile(fix_patch()),
            ],
            |o| assert_eq!(o[2], Obs::Recon(Reconciliation::Unknown)),
        ),
        case(
            "current_workspace_missing",
            vec![Snapshot, Current, Do(Side::lose_workspace), Current],
            |o| {
                assert_eq!(o[1], Obs::Current(Some(Ok(snapshot_digest()))));
                assert!(matches!(&o[3], Obs::Current(Some(Err(_)))), "{:?}", o[3]);
            },
        ),
    ]
}

fn request(task: &TaskId, contract: &Contract, kind: EffectKind, payload: &[u8]) -> EffectRequest {
    EffectRequest {
        effect_id: EffectId::derive(task, 0, &kind, &Digest::of(payload)),
        task_id: task.clone(),
        kind,
        payload: payload.to_vec(),
        contract: contract.clone(),
        deadline_ts: 0,
    }
}

async fn observe(side: &Side, step: &Step, req: Option<&EffectRequest>, ctx: &AttemptCtx) -> Obs {
    match (step, req) {
        (Step::Do(change), _) => {
            change(side);
            Obs::Done
        }
        (Step::Current, _) => Obs::Current(side.current()),
        (Step::Reconcile(_), Some(req)) => Obs::Recon(side.reconcile(req, ctx).await),
        (_, Some(req)) => Obs::Out(side.run(req, ctx).await),
        (_, None) => unreachable!(),
    }
}

/// Runs `case` on both workers and asserts every observation is equal. A missing workspace
/// is the one place the texts differ by design (the spec names `workspace directory <path>
/// is missing` for the host and `workspace image <path> is missing` for Firecracker, each
/// path in its own root): there, both must be errors with their documented text. With
/// `real`, the other side is the real guest, and a case's `real` expectation replaces the
/// equality for the observations it is about.
async fn run_case(case: &Case) {
    run_case_against(case, None).await
}

async fn run_case_against(case: &Case, real: Option<&kvm::Kvm>) {
    let task = TaskId::new();
    let contract = contract(10).0;
    let host = Side::new(Kind::Host, &task, case.settings);
    let fake = match real {
        Some(kvm) => Side::real(kvm, &task, case.settings),
        None => Side::new(Kind::Fake, &task, case.settings),
    };
    let differs = real.is_some() && case.real.is_some();
    let mut theirs = Vec::new();
    let mut seen = Vec::new();
    for (i, step) in case.steps.iter().enumerate() {
        let req = match step {
            Step::Snapshot => Some(request(&task, &contract, EffectKind::ReadSnapshot, b"")),
            Step::Patch(p) | Step::Reconcile(p) => Some(request(
                &task,
                &contract,
                EffectKind::ApplyPatch {
                    expected_base: snapshot_digest(),
                },
                p.as_bytes(),
            )),
            Step::PatchOn(p, base) => Some(request(
                &task,
                &contract,
                EffectKind::ApplyPatch {
                    expected_base: *base,
                },
                p.as_bytes(),
            )),
            Step::Verify => Some(request(&task, &contract, EffectKind::RunVerification, b"")),
            Step::Current | Step::Do(_) => None,
        };
        let ctx = ctx(i as u64 + 1);
        let h = observe(&host, step, req.as_ref(), &ctx).await;
        let f = observe(&fake, step, req.as_ref(), &ctx).await;
        theirs.push(f.clone());
        match (&h, &f) {
            _ if differs && matches!(step, Step::Verify) => {}
            (Obs::Current(Some(Err(he))), Obs::Current(Some(Err(fe)))) => {
                assert_eq!(
                    *he,
                    format!("workspace directory {} is missing", host.ws().display()),
                    "{}",
                    case.name
                );
                assert_eq!(
                    *fe,
                    format!(
                        "workspace image {} is missing",
                        fake.task_dir().join("ws.img").display()
                    ),
                    "{}",
                    case.name
                );
            }
            (Obs::Out(ho), Obs::Out(fo)) => {
                assert_eq!(text(fo), text(ho), "{} step {i}: output bytes", case.name);
                assert_eq!(fo.receipt, ho.receipt, "{} step {i}: receipt", case.name);
                assert_eq!(
                    fo.new_workspace, ho.new_workspace,
                    "{} step {i}: new_workspace",
                    case.name
                );
                assert_eq!(
                    fo.verification, ho.verification,
                    "{} step {i}: verification",
                    case.name
                );
                assert_eq!(fo, ho, "{} step {i}", case.name);
            }
            _ => assert_eq!(f, h, "{} step {i}", case.name),
        }
        seen.push(h);
    }
    (case.check)(&seen);
    if let (Some(_), Some(real_check)) = (real, case.real) {
        real_check(&theirs);
    }
}

#[tokio::test]
async fn every_case_is_observationally_equal_on_host_and_fake() {
    let cases = cases();
    let mut names: Vec<&str> = cases.iter().map(|c| c.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), 19, "every case has its own name");
    for case in &cases {
        run_case(case).await;
    }
}

/// The `Real` column: the table against the real, jailed guest (KVM tier only).
#[tokio::test(flavor = "multi_thread")]
async fn conformance_table_passes_on_the_real_guest() {
    let Some(kvm) = kvm::require() else { return };
    // The cases run side by side: each boots its own VMs in its own root.
    let mut runs = Vec::new();
    for i in 0..cases().len() {
        let kvm = kvm.clone();
        runs.push(tokio::spawn(async move {
            let cases = cases();
            let case = &cases[i];
            run_case_against(case, Some(&kvm)).await;
            case.name
        }));
    }
    for run in runs {
        let name = run.await.unwrap();
        println!("Real column: {name} ok");
    }
}

#[tokio::test]
async fn snapshot_digest_from_the_guest_equals_the_host_digest() {
    let task = TaskId::new();
    let fake = Side::new(Kind::Fake, &task, Settings::default());
    let req = request(&task, &contract(10).0, EffectKind::ReadSnapshot, b"");
    let out = fake.run(&req, &ctx(1)).await;
    succeeded(&out);
    let host_digest = workspace_digest(&fake.root().join("snapshot")).unwrap();
    assert_eq!(host_digest.to_string(), GOLDEN_SNAPSHOT_DIGEST);
    assert_eq!(
        out.new_workspace,
        Some(host_digest),
        "the guest's SnapshotDone digest"
    );
    assert_eq!(
        workspace_digest(&fake.ws()).unwrap(),
        host_digest,
        "the tree the guest wrote"
    );
    assert_eq!(
        fake.current(),
        Some(Ok(host_digest)),
        "an inspection boot reports it too"
    );

    // The real guest (KVM tier): its digest of the tree it wrote into the ext4 image.
    let Some(kvm) = kvm::require() else { return };
    let real = Side::real(&kvm, &task, Settings::default());
    let out = real.run(&req, &ctx(1)).await;
    succeeded(&out);
    assert_eq!(
        out.new_workspace,
        Some(host_digest),
        "the real guest's SnapshotDone digest"
    );
    assert_eq!(
        real.current(),
        Some(Ok(host_digest)),
        "a real inspection boot reports it too"
    );
}

// ---------------------------------------------------------------------------------------
// Kill paths and the failure mapping through the real supervisor binary.

/// A Firecracker task over the fake guest, jailed when asked (or with
/// `AGENTOS_TEST_JAIL=fake`), whose jobs run under `<root>/jobs`.
struct KFx {
    dir: TempDir,
    task: TaskId,
    contract: Contract,
    cfg: FirecrackerConfig,
    counts: ExecCounts,
}

impl KFx {
    /// Jailed with `AGENTOS_TEST_JAIL=fake`, else unjailed.
    fn new() -> KFx {
        KFx::build(test_jail_fake())
    }

    fn jailed() -> KFx {
        KFx::build(true)
    }

    fn build(jailed: bool) -> KFx {
        let dir = fresh_root();
        let cfg = if jailed {
            jailed_fake_firecracker_config(dir.path())
        } else {
            fake_firecracker_config(dir.path())
        };
        KFx {
            dir,
            task: TaskId::new(),
            contract: contract(10).0,
            cfg,
            counts: ExecCounts::default(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn jobs_root(&self) -> PathBuf {
        self.path("jobs")
    }

    fn task_dir(&self) -> PathBuf {
        self.path("work").join(self.task.as_str())
    }

    fn worker(&self) -> WorkerConfig {
        WorkerConfig::Firecracker(self.cfg.clone())
    }

    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        request(&self.task, &self.contract, kind, payload)
    }

    fn patch(&self) -> EffectRequest {
        self.request(
            EffectKind::ApplyPatch {
                expected_base: snapshot_digest(),
            },
            fix_patch().as_bytes(),
        )
    }

    /// A supervised executor over this task (real supervisor, the fake guest allowed).
    fn executor(&self, crash: Option<CrashHook>, env: &[(&str, &str)]) -> SupervisedExecutor {
        let mut all = vec![(TEST_WORKERS_ENV, "1")];
        all.extend_from_slice(env);
        supervised(&self.jobs_root(), self.worker(), &self.counts, crash, &all)
    }

    /// Reads the snapshot into the task's workspace image (a supervised job).
    async fn snapshot(&self) {
        let out = self
            .executor(None, &[])
            .run(&self.request(EffectKind::ReadSnapshot, b""), &ctx(1))
            .await;
        succeeded(&out);
    }

    /// A check that drops `<root>/check-started` and then hangs. One Python process whose
    /// command line names this root, so the `/proc` scans see it.
    fn hang_profile(&self) -> PathBuf {
        let marker = self.path("check-started");
        let script = format!(
            "import time; open({:?}, 'w').close(); time.sleep(30)",
            marker.to_str().unwrap()
        );
        set_profile(
            self.dir.path(),
            serde_json::json!(["python3", "-c", script]),
        );
        marker
    }

    /// Creates the job for `req` (its lock held by the returned file), as the controller does.
    fn create(
        &self,
        req: &EffectRequest,
        c: &AttemptCtx,
        lease_in_ms: i64,
        deadline_in_ms: Option<i64>,
    ) -> (JobDir, File) {
        let deadline = deadline_in_ms.map_or(0, |d| now_ms() + d);
        JobDir::create(
            &self.jobs_root(),
            &job_request(req, c, self.worker(), now_ms() + lease_in_ms, deadline),
        )
        .unwrap()
    }

    /// Live processes naming anything under this root: supervisors and workers (the job
    /// directory), the fake guest (the task directory) and the check (the marker).
    fn live(&self) -> Vec<i32> {
        processes_naming(self.dir.path())
    }

    fn assert_nothing_left_within(&self, within: Duration) {
        let started = Instant::now();
        loop {
            let live = self.live();
            if live.is_empty() {
                return;
            }
            assert!(
                started.elapsed() < within,
                "processes outlived the job: {}",
                describe(&live)
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The live fake guest of this task: its command line is `… fake-guest <sock> <task dir>`.
    fn guest_pids(&self) -> Vec<i32> {
        let task_dir = self.task_dir();
        self.live()
            .into_iter()
            .filter(|pid| {
                fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
                    let args: Vec<&[u8]> = c.split(|b| *b == 0).collect();
                    args.contains(&&b"fake-guest"[..])
                        && args.contains(&task_dir.as_os_str().as_encoded_bytes())
                })
            })
            .collect()
    }

    fn cgroup_root(&self) -> PathBuf {
        match &self.cfg.jail {
            JailMode::Jailed(jc) => jc.cgroup_root.clone(),
            JailMode::Unjailed => panic!("not jailed"),
        }
    }

    /// The fake jailer's "cgroup" of attempt `c`.
    fn cgroup_of(&self, c: &AttemptCtx) -> PathBuf {
        self.cgroup_root()
            .join("agentos")
            .join(c.attempt_id.to_string())
    }
}

fn describe(pids: &[i32]) -> String {
    pids.iter()
        .map(|p| {
            format!(
                "{p}: {}",
                fs::read(format!("/proc/{p}/cmdline"))
                    .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Starts the real supervisor the way the controller does: the locked file is its stdin.
fn spawn_supervisor(job: &JobDir, lock: File, env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(SUPERVISOR_BIN);
    cmd.arg("run")
        .arg(&job.path)
        .stdin(Stdio::from(lock))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.env(TEST_WORKERS_ENV, "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if started.elapsed() > PATIENCE {
            let _ = child.kill();
            panic!("supervisor still running after {PATIENCE:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

async fn wait_for_async(what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < PATIENCE, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn assert_killed(job: &JobDir, why: KillReason) {
    let status = job.read_status().expect("a status");
    assert_eq!(
        (status.state, status.reason),
        (JobState::Killed, Some(why)),
        "{status:?}"
    );
}

fn receipt_reason(job: &JobDir) -> String {
    reason(&job.read_receipt().expect("a receipt"))
}

fn alive(pid: i32) -> bool {
    proc_state(pid).is_some_and(|(s, ..)| s != "Z")
}

fn nlink(path: &Path) -> u64 {
    fs::metadata(path).unwrap().nlink()
}

#[tokio::test]
async fn lease_expiry_kills_the_worker_and_the_fake_guest() {
    let fx = KFx::new();
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let (job, lock) = fx.create(&req, &c, 500, None);
    let mut supervisor = spawn_supervisor(&job, lock, &[]);
    assert!(wait_exit(&mut supervisor).success());
    assert_killed(&job, KillReason::Lease);
    assert_eq!(receipt_reason(&job), "lease expired");
    assert!(
        marker.exists(),
        "the check was running when the lease ended"
    );
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

#[tokio::test]
async fn cancel_marker_kills_the_worker_and_the_guest_within_500ms() {
    let fx = KFx::new();
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let (job, lock) = fx.create(&req, &c, 60_000, None);
    let mut supervisor = spawn_supervisor(&job, lock, &[]);
    wait_for("the check to start", || marker.exists());
    let guests = fx.guest_pids();
    assert_eq!(guests.len(), 1, "one fake guest: {}", describe(&fx.live()));
    let dropped = Instant::now();
    job.drop_cancel().unwrap();
    wait_for("the kill", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Killed)
    });
    let took = dropped.elapsed();
    assert!(took < Duration::from_millis(500), "took {took:?}");
    assert!(wait_exit(&mut supervisor).success());
    assert_killed(&job, KillReason::Cancel);
    assert_eq!(receipt_reason(&job), "cancelled");
    assert!(!alive(guests[0]), "the guest outlived the cancel");
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

#[tokio::test]
async fn deadline_kills_the_vm() {
    let fx = KFx::new();
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let (job, lock) = fx.create(&req, &c, 60_000, Some(1_000));
    let mut supervisor = spawn_supervisor(&job, lock, &[]);
    wait_for("the check to start", || marker.exists());
    assert_eq!(fx.guest_pids().len(), 1);
    assert!(wait_exit(&mut supervisor).success());
    assert_killed(&job, KillReason::Deadline);
    assert_eq!(receipt_reason(&job), "deadline exceeded");
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

/// Launches a hanging verification through a controller that "dies" right after the
/// launch, waits for its check, and SIGKILLs the job's supervisor. Returns the job.
async fn orphan_a_running_verification(fx: &KFx, c: &AttemptCtx) -> JobDir {
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let req = fx.request(EffectKind::RunVerification, b"");
    let dying = fx.executor(
        Some(CrashHook::at(CrashPoint::DuringExecute, "run_verification")),
        &[],
    );
    let out = dying.run(&req, c).await;
    assert_eq!(reason(&out), "injected crash after the launch");
    let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    wait_for_async("the check to start", || marker.exists()).await;
    wait_for_async("Running status", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Running)
    })
    .await;
    let supervisor = job.read_status().unwrap().supervisor_pid.unwrap() as i32;
    kill_process(Pid::from_raw(supervisor).unwrap(), Signal::KILL).unwrap();
    // The executor's reaper thread reaps it; its lock dies with it.
    wait_for_async("the job's lock to be free", || job.is_dead()).await;
    wait_for_async("the supervisor to be reaped", || !alive(supervisor)).await;
    job
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_sigkill_then_fence_leaves_no_worker_and_no_guest_process() {
    let fx = KFx::new();
    let c = ctx(2);
    let job = orphan_a_running_verification(&fx, &c).await;
    // The worker and its guest outlived the supervisor: only the fence kills them.
    assert!(
        !processes_naming(&job.path).is_empty(),
        "the worker lives on: {}",
        describe(&fx.live())
    );
    let guests = fx.guest_pids();
    assert_eq!(
        guests.len(),
        1,
        "the guest lives on: {}",
        describe(&fx.live())
    );
    assert!(job.read_receipt().is_none());

    let controller = fx.executor(None, &[]);
    assert!(
        controller.fence_jobs(std::slice::from_ref(&job)).await,
        "the fence settles the job"
    );
    assert!(!alive(guests[0]));
    fx.assert_nothing_left_within(Duration::from_secs(2));
    assert!(
        job.read_receipt().is_none(),
        "killed mid-flight: no receipt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_sigkill_leaves_the_jail_and_the_controller_collects_it_after_the_fence() {
    let fx = KFx::jailed();
    let c = ctx(2);
    let job = orphan_a_running_verification(&fx, &c).await;
    let (jail, cgroup, scratch) = (
        job.path.join("jail"),
        fx.cgroup_of(&c),
        job.path.join("scratch.img"),
    );
    // Dead by its lock, not settled: the jail stays, its cgroup too, and the chroot's hard
    // link keeps the scratch image's blocks alive.
    assert!(
        jail.is_dir(),
        "the jail is left until the controller collects it"
    );
    assert!(cgroup.is_dir(), "the fake cgroup {}", cgroup.display());
    assert_eq!(nlink(&scratch), 2, "scratch.img and its link in the chroot");
    assert_eq!(fx.guest_pids().len(), 1);

    let controller = fx.executor(None, &[]);
    assert!(controller.fence_jobs(std::slice::from_ref(&job)).await);
    assert!(!jail.exists(), "the jail is collected after the fence");
    assert!(!cgroup.exists(), "the cgroup is removed after the fence");
    assert_eq!(nlink(&scratch), 1, "the chroot's link is gone");
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_kill_leaves_the_jail_until_the_controller_collects_it() {
    // The supervisor alone: it kills the worker and the VM on the lease, and nothing but the
    // controller collects the jail.
    let fx = KFx::jailed();
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let (job, lock) = fx.create(&req, &c, 1_500, None);
    let mut supervisor = spawn_supervisor(&job, lock, &[]);
    wait_for("the check to start", || marker.exists());
    assert!(wait_exit(&mut supervisor).success());
    assert_killed(&job, KillReason::Lease);
    assert_eq!(receipt_reason(&job), "lease expired");
    let (jail, cgroup) = (job.path.join("jail"), fx.cgroup_of(&c));
    assert!(
        jail.is_dir() && cgroup.is_dir(),
        "the jail is there when the supervisor reports Killed"
    );
    assert_eq!(nlink(&job.path.join("scratch.img")), 2);
    fx.assert_nothing_left_within(Duration::from_secs(2));
    let controller = fx.executor(None, &[]);
    assert!(
        controller.fence_jobs(std::slice::from_ref(&job)).await,
        "a killed job is settled"
    );
    assert!(
        !jail.exists() && !cgroup.exists(),
        "collected by the controller"
    );

    // Through `SupervisedExecutor::run`: the jail is gone once it returns.
    fs::remove_file(&marker).unwrap();
    let short = EffectTimeouts {
        verification: Duration::from_millis(1_500),
        other: Duration::from_secs(30),
    };
    let exec = fx.executor(None, &[]).with_timeouts(short);
    let (req, c) = (fx.request(EffectKind::RunVerification, b"again"), ctx(3));
    let out = exec.run(&req, &c).await;
    assert_eq!(reason(&out), "lease expired");
    assert!(marker.exists(), "the check ran");
    let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    // The receipt settles the job (`run` returns on it); the supervisor writes `Killed`
    // right after it, so the jail is checked first, then the status is awaited.
    assert!(
        !job.path.join("jail").exists(),
        "the jail is collected before run returns"
    );
    assert!(!fx.cgroup_of(&c).exists());
    wait_for_async("the Killed status", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Killed)
    })
    .await;
    assert_killed(&job, KillReason::Lease);
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------------------
// Test peers on a job's `v.sock`: play the guest where the fake guest cannot misbehave.

fn send(s: &mut UnixStream, m: Message) {
    let _ = write_frame(s, &Frame::Json(m));
}

fn ready() -> Message {
    Message::Ready {
        protocol: 2,
        agent_version: "0.1.0".into(),
        mode: Mode::Job,
        vcpus: 1,
        memory_mib: 256,
    }
}

/// Binds `<job>/v.sock`; on its own thread accepts one connection, does the `CONNECT`/`OK`
/// handshake, answers `Hello` with `Ready`, reads the request and runs `script` with it.
fn peer(
    job: &Path,
    script: impl FnOnce(&mut UnixStream, Message) + Send + 'static,
) -> thread::JoinHandle<()> {
    let (path, _dir) = socket_path(&job.join("v.sock")).unwrap();
    let listener = UnixListener::bind(path).unwrap();
    thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while line.last() != Some(&b'\n') {
            s.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        assert_eq!(line, b"CONNECT 5200\n");
        s.write_all(b"OK 5200\n").unwrap();
        let Frame::Json(Message::Hello { .. }) = read_frame(&mut s, 0).unwrap() else {
            panic!("expected Hello")
        };
        send(&mut s, ready());
        let Frame::Json(request) = read_frame(&mut s, 0).unwrap() else {
            panic!("expected a request")
        };
        script(&mut s, request);
    })
}

/// A launcher whose "VM" is a process that stays up and never listens: the test peer is the
/// guest.
fn peer_launcher() -> GuestLauncher {
    GuestLauncher::Fake {
        program: "/bin/sh".into(),
        prefix_args: vec!["-c".into(), "exec sleep 60".into(), "sh".into()],
    }
}

#[tokio::test]
async fn a_reply_that_never_comes_is_bounded_by_the_lease() {
    let mut fx = KFx::build(false);
    // The workspace exists (an honest snapshot), then the "VM" is a peer that takes the
    // request, announces a legal reply frame and stalls in the middle of it.
    fx.snapshot().await;
    let honest = fx.cfg.clone();
    fx.cfg.launcher = peer_launcher();
    let stall = |s: &mut UnixStream, _request: Message| {
        let mut frame = 1_000u32.to_be_bytes().to_vec();
        frame.push(0);
        frame.extend_from_slice(br#"{"type":"Verif"#);
        let _ = s.write_all(&frame);
        thread::sleep(Duration::from_secs(10));
    };
    for (req, c) in [
        (fx.request(EffectKind::RunVerification, b""), ctx(2)),
        (fx.patch(), ctx(3)),
    ] {
        let is_patch = matches!(req.kind, EffectKind::ApplyPatch { .. });
        let (job, lock) = fx.create(&req, &c, 800, None);
        let _guest = peer(&job.path, stall);
        let started = Instant::now();
        let mut supervisor = spawn_supervisor(&job, lock, &[]);
        assert!(wait_exit(&mut supervisor).success());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "bounded by the 800 ms lease: {:?}",
            started.elapsed()
        );
        assert_killed(&job, KillReason::Lease);
        if is_patch {
            // No receipt: the effect is unknown until the controller reconciles it.
            assert!(job.read_receipt().is_none() && job.read_outcome().is_none());
        } else {
            assert_eq!(receipt_reason(&job), "lease expired");
        }
        fx.assert_nothing_left_within(Duration::from_secs(2));
    }
    // The patch's job is dead without a receipt; the controller (with the real fake guest
    // for its inspection boot) waits, finds none, and reconciles: the stalled "guest" never
    // touched the image.
    fx.cfg = honest;
    let exec = fx.executor(None, &[]);
    let (req, c) = (fx.patch(), ctx(4));
    assert_eq!(
        exec.wait_for_job(&req.effect_id, Duration::from_secs(1))
            .await,
        JobWait::Dead
    );
    assert_eq!(exec.reconcile(&req, &c).await, Reconciliation::NotApplied);
}

#[tokio::test]
async fn boot_timeout_failure_mapping() {
    // A guest that never listens: no request is ever sent, so every kind (the patch too) is
    // a plain failure receipt, after BOOT_TIMEOUT. Three tasks, so the boots run side by side.
    let kinds = |fx: &KFx| {
        [
            fx.request(EffectKind::ReadSnapshot, b""),
            fx.patch(),
            fx.request(EffectKind::RunVerification, b""),
        ]
    };
    let mut runs = Vec::new();
    for i in 0..3 {
        let fx = KFx::new();
        fs::create_dir_all(fx.task_dir()).unwrap();
        // Sparse, at the recorded size, as a snapshot leaves it.
        fs::File::create(fx.task_dir().join("ws.img"))
            .unwrap()
            .set_len(agentos_core::resources::VmResources::V0.disk_bytes())
            .unwrap();
        let req = kinds(&fx)[i].clone();
        runs.push(tokio::spawn(async move {
            let exec = fx.executor(None, &[(NEVER_LISTEN, "1")]);
            let c = ctx(1);
            let out = exec.run(&req, &c).await;
            let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
                .unwrap()
                .pop()
                .unwrap();
            (fx, req, out, job)
        }));
    }
    for run in runs {
        let (fx, req, out, job) = run.await.unwrap();
        let why = reason(&out);
        assert_eq!(
            why,
            "guest did not come up: no connection before the boot deadline",
            "{}",
            req.kind.tag()
        );
        assert!(!out.unresolved, "{}: nothing was sent", req.kind.tag());
        assert_eq!(
            job.read_receipt(),
            Some(out.clone()),
            "the worker's outcome is the receipt"
        );
        // The receipt comes first; the terminal status right after it.
        wait_for_async("the Exited status", || {
            job.read_status()
                .is_some_and(|s| s.state == JobState::Exited)
        })
        .await;
        fx.assert_nothing_left_within(Duration::from_secs(2));
    }
}

#[tokio::test]
async fn eof_after_apply_patch_reconciles_through_the_fake_inspector() {
    let fx = KFx::new();
    fx.snapshot().await;
    let (req, c) = (fx.patch(), ctx(2));
    let out = fx
        .executor(None, &[(KILL_VM_AFTER_REQUEST, "1")])
        .run(&req, &c)
        .await;
    let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    assert!(job.read_outcome().is_none(), "the worker wrote no outcome");
    assert!(
        job.read_receipt().is_none(),
        "so the supervisor wrote no receipt"
    );
    assert_eq!(job.read_status().unwrap().state, JobState::Exited);
    assert!(!out.unresolved, "the inspection could tell: {}", text(&out));
    // The VM died right after the request: the image holds the base or the base plus the
    // patch, and the reconciled outcome says which.
    let on_disk = workspace_digest(&fx.task_dir().join("workspace")).unwrap();
    if on_disk == snapshot_digest() {
        assert_eq!(reason(&out), "patch provably not applied");
    } else {
        succeeded(&out);
        assert_eq!(out.new_workspace, Some(on_disk));
        assert_eq!(evidence(&out)["applied"], true);
    }
    assert_eq!(
        (out.receipt.attempt_id.clone(), out.receipt.lease_generation),
        (c.attempt_id.clone(), 2)
    );
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn eof_after_run_verification_is_guest_exited_before_reporting() {
    let fx = Arc::new(KFx::new());
    fx.snapshot().await;
    let marker = fx.hang_profile();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let running = {
        let (fx, req, c) = (fx.clone(), req.clone(), c.clone());
        tokio::spawn(async move { fx.executor(None, &[]).run(&req, &c).await })
    };
    wait_for_async("the check to start", || marker.exists()).await;
    let guests = fx.guest_pids();
    assert_eq!(guests.len(), 1, "{}", describe(&fx.live()));
    // The VM dies under the request: the connection reaches EOF.
    kill_process(Pid::from_raw(guests[0]).unwrap(), Signal::KILL).unwrap();
    let out = running.await.unwrap();
    assert_eq!(
        reason(&out),
        "guest exited before reporting: firecracker killed by signal 9"
    );
    assert!(!out.unresolved);
    let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(job.read_receipt(), Some(out));
    // The receipt comes first; the terminal status right after it.
    wait_for_async("the Exited status", || {
        job.read_status()
            .is_some_and(|s| s.state == JobState::Exited)
    })
    .await;
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

/// The `v.sock` the job's guest listens on (unjailed `<job>/v.sock`, jailed in the chroot).
fn job_socket(dir: &Path) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let ty = entry.file_type().ok()?;
        if ty.is_socket() && entry.file_name() == "v.sock" {
            return Some(entry.path());
        }
        if ty.is_dir()
            && let Some(found) = job_socket(&entry.path())
        {
            return Some(found);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn second_hello_with_another_token_is_refused() {
    let fx = Arc::new(KFx::new());
    fx.snapshot().await;
    // A check that waits for the stray connection to be done, then passes.
    let (started, release) = (fx.path("check-started"), fx.path("release"));
    let script = format!(
        "import os, time\nopen({s:?}, 'w').close()\nwhile not os.path.exists({r:?}): time.sleep(0.01)\nprint('PASSED')",
        s = started.to_str().unwrap(),
        r = release.to_str().unwrap()
    );
    set_profile(fx.dir.path(), serde_json::json!(["python3", "-c", script]));
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx(2));
    let running = {
        let (fx, req, c) = (fx.clone(), req.clone(), c.clone());
        tokio::spawn(async move { fx.executor(None, &[]).run(&req, &c).await })
    };
    wait_for_async("the check to start", || started.exists()).await;
    let job = JobDir::list(&fx.jobs_root(), &req.effect_id)
        .unwrap()
        .pop()
        .unwrap();
    let sock = job_socket(&job.path).expect("the job's v.sock");
    let (path, _dir) = socket_path(&sock).unwrap();
    let mut stray = UnixStream::connect(path).unwrap();
    stray.write_all(b"CONNECT 5200\n").unwrap();
    let mut ok = [0u8; 8];
    stray.read_exact(&mut ok).unwrap();
    assert_eq!(&ok, b"OK 5200\n");
    let hello = Message::Hello {
        protocol: 2,
        attempt_token: mint_attempt_token(),
        task_id: fx.task.as_str().to_string(),
        effect_id: req.effect_id.as_str().to_string(),
        attempt_id: c.attempt_id.to_string(),
        lease_generation: c.lease_generation,
        mode: Mode::Job,
    };
    write_frame(&mut stray, &Frame::Json(hello)).unwrap();
    stray
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // Another token gets no reply at all: the connection is closed.
    let mut rest = Vec::new();
    assert_eq!(
        stray.read_to_end(&mut rest).unwrap(),
        0,
        "a reply to another token: {rest:?}"
    );
    fs::write(&release, b"").unwrap();
    // The job's own session is untouched.
    let out = running.await.unwrap();
    let v = evidence(&out);
    assert_eq!(
        (v["passed"].clone(), v["summary"].clone()),
        (serde_json::json!(true), serde_json::json!("PASSED")),
        "{v}"
    );
    fx.assert_nothing_left_within(Duration::from_secs(2));
}

// ---------------------------------------------------------------------------------------
// A forged verification digest reaches the controller and is rejected there.

/// Verifications go through `verify` (a supervisor whose "VM" is a test peer), everything
/// else through `plain` (the fake guest).
struct ByKind {
    plain: SupervisedExecutor,
    verify: SupervisedExecutor,
}

impl Executor for ByKind {
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match req.kind {
            EffectKind::RunVerification => self.verify.run(req, ctx).await,
            _ => self.plain.run(req, ctx).await,
        }
    }

    fn retained_outcome(&self, effect: &EffectId) -> Option<ExecOutcome> {
        self.plain.retained_outcome(effect)
    }

    async fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Reconciliation {
        self.plain.reconcile(req, ctx).await
    }

    fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>> {
        self.plain.current_workspace(task)
    }
}

/// Waits for the first verification job under `jobs` and plays its guest: a `Verified` that
/// passes, for the real profile, but names `forged` as the workspace it checked.
fn forging_guest(jobs: PathBuf, profile_digest: Digest, forged: Digest) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let started = Instant::now();
        let job = loop {
            assert!(started.elapsed() < PATIENCE, "no verification job appeared");
            let found = fs::read_dir(&jobs)
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .find(|p| {
                    JobDir::open(p)
                        .and_then(|j| j.request())
                        .is_ok_and(|r| matches!(r.kind, EffectKind::RunVerification))
                });
            if let Some(job) = found {
                break job;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let guest = peer(&job, move |s, request| {
            assert!(
                matches!(request, Message::RunVerification { .. }),
                "{request:?}"
            );
            loop {
                if let Frame::Json(Message::EndFiles) = read_frame(s, RAW_FRAME_LIMIT).unwrap() {
                    break;
                }
            }
            send(
                s,
                Message::Verified {
                    profile_id: "parser-checks-v1".into(),
                    command: vec!["python3".into(), "check_parser.py".into()],
                    profile_digest,
                    workspace_digest: forged,
                    exit_code: Some(0),
                    stdout_b64: b64(b"10/10 checks passed\nPASSED\n"),
                    stdout_truncated: false,
                    stderr_b64: b64(b""),
                    stderr_truncated: false,
                },
            );
            if let Ok(Frame::Json(Message::Shutdown)) = read_frame(s, 0) {
                send(s, Message::Bye);
            }
        });
        guest.join().unwrap();
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_verified_workspace_digest_is_rejected_by_the_controller() {
    let env = Env::new(10);
    let root = env.dir.path();
    let jobs = root.join("jobs");
    let real = fake_firecracker_config(root);
    let forger = FirecrackerConfig {
        launcher: peer_launcher(),
        ..real.clone()
    };
    let counts = ExecCounts::default();
    let env_vars = [(TEST_WORKERS_ENV, "1")];
    let exec = ByKind {
        plain: supervised(
            &jobs,
            WorkerConfig::Firecracker(real),
            &counts,
            None,
            &env_vars,
        ),
        verify: supervised(
            &jobs,
            WorkerConfig::Firecracker(forger),
            &counts,
            None,
            &env_vars,
        ),
    };
    let forged = Digest::of(b"a workspace the task never had");
    let guest = forging_guest(
        jobs.clone(),
        workspace_digest(&root.join("profile")).unwrap(),
        forged,
    );
    let mut agent = FakeAgent::from_fixture_patch(fix_patch());

    let state = run_task(&env.db, &env.blobs, &exec, &mut agent, &env.task)
        .await
        .unwrap();
    guest.join().unwrap();

    assert_ne!(state, TaskState::Succeeded);
    let task = env.db.task(&env.task).unwrap();
    assert_eq!(
        task.verified_digest, None,
        "the forged evidence proves nothing"
    );
    let patched =
        workspace_digest(&root.join("work").join(env.task.as_str()).join("workspace")).unwrap();
    assert_eq!(
        task.workspace_digest, patched,
        "the journal holds the digest of the patched image"
    );
    assert_ne!(patched, forged);
    assert_eq!(env.count("VerifyPassed"), 0);
    assert_eq!(env.count("VerifyFailed"), 1, "{:?}", env.event_types());
    // The guest's claim reached the controller intact (the evidence names the forged
    // digest and says passed) and was rejected there, not on the way.
    let verify = env.effects("RunVerification").remove(0);
    assert_eq!(verify.state, EffectState::Completed);
    let evidence = env.blob_json(&verify.result_digest.unwrap());
    assert_eq!(evidence["workspace_digest"], forged.to_string());
    assert_eq!(evidence["passed"], true);
}
