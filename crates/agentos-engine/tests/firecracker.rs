//! `FirecrackerWorker` over the fake guest (`agentos-supervisor fake-guest`) and over scripted
//! test peers that speak the guest protocol, without KVM.

mod common;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agentos_core::contract::Contract;
use agentos_core::effect::{AgentSessionSpec, AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::guest::{
    Frame, Message, Mode, RAW_FRAME_LIMIT, WS_IMAGE_BYTES, b64, read_frame, unb64, write_frame,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::resources::VmResources;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::firecracker::{FirecrackerConfig, FirecrackerWorker, WorkerResult, preflight};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::guestlink::{GuestLauncher, socket_path};
use agentos_engine::job::{JobDir, JobRequest, Mailbox, WorkerConfig};
use agentos_engine::worker::Worker;
use agentos_engine::workspace::workspace_digest;
use agentos_store::db::Db;
use common::{contract, copy_dir, fake_firecracker_config, fix_patch, fixtures};
use rustix::process::{Pid, Signal, kill_process};
use tempfile::TempDir;

struct Fx {
    dir: TempDir,
    cfg: FirecrackerConfig,
    task: TaskId,
    contract: Contract,
}

fn test_env() -> Vec<(String, String)> {
    vec![("AGENTOS_TEST_WORKERS".into(), "1".into())]
}

fn env_with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut env = test_env();
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    env
}

fn ctx() -> AttemptCtx {
    AttemptCtx {
        attempt_id: AttemptId::new(),
        lease_generation: 1,
        worker: "test".into(),
    }
}

impl Fx {
    fn new() -> Fx {
        Fx::for_task(TaskId::new())
    }

    fn for_task(task: TaskId) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let cfg = fake_firecracker_config(dir.path());
        Fx {
            dir,
            cfg,
            task,
            contract: contract(10).0,
        }
    }

    /// A fixture whose launcher spawns nothing that matters (`/bin/true`): a test peer
    /// bound to the job's `v.sock` plays the guest.
    fn with_peer() -> Fx {
        let mut fx = Fx::new();
        fx.cfg.launcher = GuestLauncher::Fake {
            program: "/bin/true".into(),
            prefix_args: vec![],
        };
        fx
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn task_dir(&self) -> PathBuf {
        self.path("work").join(self.task.as_str())
    }

    fn guest_workspace(&self) -> PathBuf {
        self.task_dir().join("workspace")
    }

    /// Makes `ws.img` exist without a snapshot (for requests that need one): sparse, at the
    /// task's recorded size, as a snapshot leaves it.
    fn fake_ws_img(&self) {
        fs::create_dir_all(self.task_dir()).unwrap();
        fs::File::create(self.task_dir().join("ws.img"))
            .unwrap()
            .set_len(self.cfg.resources.disk_bytes())
            .unwrap();
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

    fn job(&self, req: &EffectRequest, ctx: &AttemptCtx) -> JobDir {
        let request = JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms: i64::MAX,
            task_deadline_ms: i64::MAX,
            worker: WorkerConfig::Firecracker(self.cfg.clone()),
        };
        JobDir::create(&self.path("jobs"), &request).unwrap().0
    }

    fn worker(&self, job: &JobDir, env: Vec<(String, String)>) -> FirecrackerWorker {
        FirecrackerWorker::new(&self.cfg, job).with_env(env)
    }

    async fn run_with(
        &self,
        req: &EffectRequest,
        ctx: &AttemptCtx,
        env: Vec<(String, String)>,
    ) -> (ExecOutcome, JobDir) {
        let job = self.job(req, ctx);
        let out = self.worker(&job, env).run(req, ctx).await;
        (out, job)
    }

    async fn run(&self, kind: EffectKind, payload: &[u8]) -> (ExecOutcome, JobDir) {
        self.run_with(&self.request(kind, payload), &ctx(), test_env())
            .await
    }

    async fn run_job(
        &self,
        kind: EffectKind,
        payload: &[u8],
        env: Vec<(String, String)>,
    ) -> (WorkerResult, JobDir) {
        let (req, ctx) = (self.request(kind, payload), ctx());
        let job = self.job(&req, &ctx);
        let res = self.worker(&job, env).run_job(&req, &ctx).await;
        (res, job)
    }

    async fn snapshot(&self) -> Digest {
        let (out, _) = self.run(EffectKind::ReadSnapshot, b"").await;
        succeeded(&out);
        out.new_workspace.unwrap()
    }

    fn script_profile(&self, command: serde_json::Value) {
        let profile = serde_json::json!({ "id": "fc-test", "command": command, "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
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

fn json(out: &ExecOutcome) -> serde_json::Value {
    serde_json::from_slice(&out.output).unwrap()
}

/// Pids (other than ours) whose command line contains `needle`.
fn pids_with(needle: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&cmdline)
            .replace('\0', " ")
            .contains(needle)
        {
            found.push(pid);
        }
    }
    found
}

fn wait_for_pid(needle: &str, within: Duration) -> i32 {
    let until = Instant::now() + within;
    loop {
        if let Some(pid) = pids_with(needle).first() {
            return *pid;
        }
        assert!(
            Instant::now() < until,
            "no process with {needle:?} appeared"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn sigkill(pid: i32) {
    let _ = kill_process(Pid::from_raw(pid).unwrap(), Signal::KILL);
}

/// Pids (other than ours) whose working directory is `dir`: the guest of a job runs in its
/// job directory (its `git` and check children do not).
fn pids_in(dir: &Path) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        if pid != me && fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd == dir) {
            found.push(pid);
        }
    }
    found
}

fn guests_of(job: &JobDir) -> Vec<i32> {
    pids_in(&job.path)
}

/// Every regular file under `dir`, recursively.
fn regular_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let ty = entry.file_type().unwrap();
        if ty.is_dir() {
            out.extend(regular_files(&entry.path()));
        } else if ty.is_file() {
            out.push(entry.path());
        }
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------------------
// Test peers: a Unix listener on the job's `v.sock` that speaks the handshake and scripts
// the guest's side.

fn ready() -> Message {
    Message::Ready {
        protocol: 2,
        agent_version: "0.1.0".into(),
        mode: Mode::Job,
        vcpus: 1,
        memory_mib: 256,
    }
}

fn send(s: &mut UnixStream, m: Message) {
    let _ = write_frame(s, &Frame::Json(m));
}

/// Reads frames until `EndFiles` (a file stream), discarding them.
fn drain_files(s: &mut UnixStream) {
    loop {
        match read_frame(s, RAW_FRAME_LIMIT).unwrap() {
            Frame::Json(Message::EndFiles) => return,
            _ => continue,
        }
    }
}

/// Binds `<job>/v.sock`, then on its own thread accepts one connection, performs the
/// `CONNECT`/`OK` handshake, answers `Hello` with `Ready`, reads the request and runs
/// `script` with it.
fn peer(
    job: &JobDir,
    script: impl FnOnce(&mut UnixStream, Message) + Send + 'static,
) -> thread::JoinHandle<()> {
    let (path, _dir) = socket_path(&job.path.join("v.sock")).unwrap();
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

/// Answers `Shutdown` with `Bye` if the worker sends one.
fn bye(s: &mut UnixStream) {
    if let Ok(Frame::Json(Message::Shutdown)) = read_frame(s, 0) {
        send(s, Message::Bye);
    }
}

// ---------------------------------------------------------------------------------------
// Preflight and the image pin.

#[test]
fn preflight_in_fake_mode_checks_the_program_and_the_image() {
    let fx = Fx::new();
    preflight(&fx.cfg).unwrap();

    let mut missing = fx.cfg.clone();
    let nope = fx.path("no-such-guest");
    missing.launcher = GuestLauncher::Fake {
        program: nope.clone(),
        prefix_args: vec![],
    };
    let err = preflight(&missing).unwrap_err();
    assert!(err.contains(&nope.display().to_string()), "{err}");

    let image_json = fx.cfg.image_dir.join("image.json");
    let original = fs::read_to_string(&image_json).unwrap();
    fs::write(
        &image_json,
        original.replace("\"protocol\":2", "\"protocol\":1"),
    )
    .unwrap();
    let err = preflight(&fx.cfg).unwrap_err();
    assert!(err.contains("protocol 1"), "{err}");
    fs::write(&image_json, &original).unwrap();
    preflight(&fx.cfg).unwrap();

    let rootfs = fx.cfg.image_dir.join("rootfs.squashfs");
    let mut bytes = fs::read(&rootfs).unwrap();
    bytes[3] ^= 1;
    fs::write(&rootfs, bytes).unwrap();
    let found = workspace_digest(&fx.cfg.image_dir).unwrap();
    let err = preflight(&fx.cfg).unwrap_err();
    assert_eq!(
        err,
        format!(
            "guest image digest mismatch: pinned {}, found {found}",
            fx.cfg.image_digest
        )
    );
}

#[tokio::test]
async fn the_worker_refuses_to_launch_when_the_image_digest_moved() {
    let fx = Fx::new();
    // The registry entry changes after the config (and its pin) was built.
    fs::write(fx.cfg.image_dir.join("vmlinux"), b"another kernel!!").unwrap();
    let (out, job) = fx.run(EffectKind::ReadSnapshot, b"").await;
    let why = reason(&out);
    assert!(
        why.starts_with("firecracker worker unavailable: guest image digest mismatch: pinned "),
        "{why}"
    );
    for name in ["vm.json", "scratch.img", "console.log", "v.sock"] {
        assert!(!job.path.join(name).exists(), "{name} was created");
    }
    assert!(!fx.task_dir().join("ws.img").exists());
    assert!(guests_of(&job).is_empty(), "a guest was started");
}

#[tokio::test]
async fn the_fake_launcher_needs_the_test_workers_switch() {
    let fx = Fx::new();
    let (out, job) = fx
        .run_with(&fx.request(EffectKind::ReadSnapshot, b""), &ctx(), vec![])
        .await;
    let why = reason(&out);
    assert!(
        why.starts_with("firecracker worker unavailable: ")
            && why.contains("AGENTOS_TEST_WORKERS=1"),
        "{why}"
    );
    assert!(!job.path.join("vm.json").exists());
}

// ---------------------------------------------------------------------------------------
// Effects through the fake guest.

#[tokio::test]
async fn read_snapshot_creates_the_sparse_image_and_reports_the_host_bytes() {
    let fx = Fx::new();
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let (out, _job) = fx.run_with(&req, &c, test_env()).await;
    succeeded(&out);

    let img = fs::metadata(fx.task_dir().join("ws.img")).unwrap();
    assert_eq!(img.len(), WS_IMAGE_BYTES);
    assert!(
        img.blocks() * 512 < 1 << 20,
        "ws.img is not sparse: {} blocks",
        img.blocks()
    );

    let host = tempfile::tempdir().unwrap();
    let exec = FixtureExecutor::new(
        fx.path("snapshot"),
        fx.path("profile"),
        host.path().join("work"),
    );
    let expected = exec.run(&req, &c).await;
    assert_eq!(out.output, expected.output);
    assert_eq!(out.new_workspace, expected.new_workspace);
    assert!(out.new_workspace.is_some());
    assert_eq!(out, expected);
}

fn sized(disk_mib: u32, scratch_mib: u32) -> VmResources {
    VmResources {
        version: 1,
        disk_mib,
        scratch_mib,
        bandwidth_mib_s: None,
        iops: None,
    }
}

#[tokio::test]
async fn read_snapshot_creates_the_workspace_image_at_the_contracted_size() {
    let mut fx = Fx::new();
    fx.cfg.resources = sized(2048, 768);
    let (out, _job) = fx.run(EffectKind::ReadSnapshot, b"").await;
    succeeded(&out);
    let img = fs::metadata(fx.task_dir().join("ws.img")).unwrap();
    assert_eq!(img.len(), 2048 << 20);
    assert!(img.blocks() * 512 < 1 << 20, "ws.img stays sparse");
}

/// The image's size is fixed by the first snapshot: a later effect that expects another
/// size fails before launching anything and leaves the image as it is.
#[tokio::test]
async fn an_existing_workspace_image_of_another_size_fails_and_is_left_as_is() {
    let mut fx = Fx::new();
    let (out, _job) = fx.run(EffectKind::ReadSnapshot, b"").await;
    succeeded(&out);
    let base = out.new_workspace.unwrap();
    fx.cfg.resources = sized(2048, 512);
    for (kind, payload) in [
        (
            EffectKind::ApplyPatch {
                expected_base: base,
            },
            fix_patch().into_bytes(),
        ),
        (EffectKind::RunVerification, Vec::new()),
    ] {
        let (out, job) = fx.run(kind, &payload).await;
        let why = reason(&out);
        assert!(
            why.contains("workspace image") && why.contains("recorded size"),
            "{why}"
        );
        assert!(!out.unresolved, "nothing was sent: a definite failure");
        assert!(!job.path.join("vm.json").exists(), "nothing was launched");
    }
    assert_eq!(
        fs::metadata(fx.task_dir().join("ws.img")).unwrap().len(),
        WS_IMAGE_BYTES
    );
}

#[tokio::test]
async fn too_little_host_disk_fails_before_launch() {
    let fx = Fx::new();
    let (out, job) = fx
        .run_with(
            &fx.request(EffectKind::ReadSnapshot, b""),
            &ctx(),
            env_with(&[("AGENTOS_TEST_HOST_FREE_MIB", "100")]),
        )
        .await;
    let why = reason(&out);
    assert!(why.starts_with("host disk: 100 MiB free under "), "{why}");
    assert!(why.contains("the VM may write 1536 MiB"), "{why}");
    assert!(!job.path.join("vm.json").exists());
    assert!(!fx.task_dir().join("ws.img").exists());

    // Enough room for what is not yet allocated: the job runs.
    let (out, _job) = fx
        .run_with(
            &fx.request(EffectKind::ReadSnapshot, b""),
            &ctx(),
            env_with(&[("AGENTOS_TEST_HOST_FREE_MIB", "1536")]),
        )
        .await;
    succeeded(&out);
}

/// Snapshot, fix patch and verification, with the same requests and attempt contexts.
async fn three_kinds(
    run: impl AsyncFn(EffectRequest, AttemptCtx) -> ExecOutcome,
    fx_req: &dyn Fn(EffectKind, &[u8]) -> EffectRequest,
    ctxs: &[AttemptCtx; 3],
) -> Vec<ExecOutcome> {
    let snap = run(fx_req(EffectKind::ReadSnapshot, b""), ctxs[0].clone()).await;
    succeeded(&snap);
    let base = snap.new_workspace.unwrap();
    let applied = run(
        fx_req(
            EffectKind::ApplyPatch {
                expected_base: base,
            },
            fix_patch().as_bytes(),
        ),
        ctxs[1].clone(),
    )
    .await;
    succeeded(&applied);
    let verified = run(fx_req(EffectKind::RunVerification, b""), ctxs[2].clone()).await;
    succeeded(&verified);
    vec![snap, applied, verified]
}

#[tokio::test]
async fn apply_patch_and_verify_produce_byte_identical_outcomes_to_the_host_worker() {
    let task = TaskId::new();
    let (fc, host) = (Fx::for_task(task.clone()), Fx::for_task(task));
    let ctxs = [ctx(), ctx(), ctx()];
    let exec = FixtureExecutor::new(
        host.path("snapshot"),
        host.path("profile"),
        host.path("work"),
    );
    let direct = three_kinds(
        async |req, c| exec.run(&req, &c).await,
        &|k, p| host.request(k, p),
        &ctxs,
    )
    .await;
    let guest = three_kinds(
        async |req, c| fc.run_with(&req, &c, test_env()).await.0,
        &|k, p| fc.request(k, p),
        &ctxs,
    )
    .await;
    for (g, d) in guest.iter().zip(&direct) {
        assert_eq!(
            String::from_utf8_lossy(&g.output),
            String::from_utf8_lossy(&d.output)
        );
        assert_eq!(g.new_workspace, d.new_workspace);
        assert_eq!(g.verification, d.verification);
        assert_eq!(g, d);
    }
    assert!(guest[2].verification.as_ref().unwrap().passed);
}

#[tokio::test]
async fn apply_and_verify_without_a_snapshot_are_refused_with_the_3a_wording() {
    let fx = Fx::new();
    let base = Digest::of(b"base");
    for (kind, payload) in [
        (
            EffectKind::ApplyPatch {
                expected_base: base,
            },
            fix_patch().into_bytes(),
        ),
        (EffectKind::RunVerification, Vec::new()),
    ] {
        let (out, job) = fx.run(kind, &payload).await;
        assert_eq!(reason(&out), "workspace missing: no snapshot was read");
        assert!(!out.unresolved);
        assert!(
            !job.path.join("vm.json").exists(),
            "nothing is launched for a missing workspace"
        );
    }
    assert!(
        !fx.task_dir().join("ws.img").exists(),
        "only ReadSnapshot creates ws.img"
    );
}

#[tokio::test]
async fn scratch_img_is_removed_after_a_successful_job_and_vm_json_console_log_remain() {
    let fx = Fx::new();
    let (out, job) = fx.run(EffectKind::ReadSnapshot, b"").await;
    succeeded(&out);
    assert!(
        !job.path.join("scratch.img").exists(),
        "scratch.img was left behind"
    );
    for name in ["vm.json", "console.log", "firecracker.log", "stderr.log"] {
        assert!(job.path.join(name).is_file(), "{name} is missing");
    }
    let vm: serde_json::Value =
        serde_json::from_slice(&fs::read(job.path.join("vm.json")).unwrap()).unwrap();
    assert_eq!(
        vm["drives"][2]["path_on_host"],
        serde_json::json!(job.path.join("scratch.img"))
    );
    assert_eq!(
        vm["drives"][1]["path_on_host"],
        serde_json::json!(fx.task_dir().join("ws.img"))
    );
    // Relative to Firecracker's working directory, the job directory (sun_path limit).
    assert_eq!(vm["vsock"]["uds_path"], "v.sock");
    assert!(
        job.path.join("v.sock").as_os_str().len() > 107,
        "a job's socket path exceeds sun_path"
    );
    assert!(guests_of(&job).is_empty(), "the guest outlived its job");
}

#[tokio::test]
async fn boot_timeout_is_a_failure_for_every_kind_and_sends_nothing() {
    let fx = Fx::new();
    fx.fake_ws_img();
    let env = env_with(&[("AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN", "1")]);
    for (kind, payload) in [
        (EffectKind::ReadSnapshot, Vec::new()),
        (
            EffectKind::ApplyPatch {
                expected_base: Digest::of(b"base"),
            },
            fix_patch().into_bytes(),
        ),
        (EffectKind::RunVerification, Vec::new()),
    ] {
        let (req, c) = (fx.request(kind, &payload), ctx());
        let job = fx.job(&req, &c);
        let started = Instant::now();
        let res = fx
            .worker(&job, env.clone())
            .with_boot_timeout(Duration::from_secs(1))
            .run_job(&req, &c)
            .await;
        let WorkerResult::Outcome(out) = res else {
            panic!("no outcome for {:?}", req.kind)
        };
        let why = reason(&out);
        assert!(why.starts_with("guest did not come up: "), "{why}");
        assert!(
            !out.unresolved,
            "nothing was sent, so nothing is unresolved"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert!(
            guests_of(&job).is_empty(),
            "the never-listening guest was not killed"
        );
        assert!(!job.path.join("scratch.img").exists());
    }
    assert!(!fx.guest_workspace().exists(), "nothing reached a guest");
}

#[tokio::test]
async fn eof_after_run_verification_is_a_failure_guest_exited_before_reporting() {
    let fx = Fx::new();
    fx.snapshot().await;
    let marker = format!("agentos-fc-eof-{}", fx.task);
    fx.script_profile(serde_json::json!([
        "python3",
        "-c",
        "import time; time.sleep(5)",
        marker
    ]));
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx());
    let job = fx.job(&req, &c);
    let job_path = job.path.clone();
    let killer = thread::spawn(move || {
        let check = wait_for_pid(&marker, Duration::from_secs(10));
        // The check runs, so the request was sent: now the guest dies mid-request.
        let guests = pids_in(&job_path);
        assert_eq!(guests.len(), 1, "{guests:?}");
        for guest in guests {
            sigkill(guest);
        }
        check
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    let check = killer.join().unwrap();
    sigkill(check);
    let why = reason(&out);
    assert_eq!(
        why,
        "guest exited before reporting: firecracker killed by signal 9"
    );
    assert!(out.verification.is_none());
}

#[tokio::test]
async fn eof_after_apply_patch_yields_no_outcome() {
    let fx = Fx::with_peer();
    fx.fake_ws_img();
    let (req, c) = (
        fx.request(
            EffectKind::ApplyPatch {
                expected_base: Digest::of(b"base"),
            },
            fix_patch().as_bytes(),
        ),
        ctx(),
    );
    let job = fx.job(&req, &c);
    let guest = peer(&job, |s, request| {
        assert!(matches!(request, Message::ApplyPatch { .. }), "{request:?}");
        let Frame::Raw(patch) = read_frame(s, RAW_FRAME_LIMIT).unwrap() else {
            panic!("expected the patch")
        };
        assert_eq!(patch, fix_patch().into_bytes());
        // The connection closes without a reply.
    });
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    guest.join().unwrap();
    let WorkerResult::NoOutcome(why) = res else {
        panic!("expected no outcome, got {res:?}")
    };
    assert!(why.starts_with("guest exited before reporting: "), "{why}");
}

#[tokio::test]
async fn kill_vm_after_request_hook_leaves_the_workspace_base_or_patched_and_no_outcome() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let patched = {
        let host = Fx::for_task(fx.task.clone());
        let exec = FixtureExecutor::new(
            host.path("snapshot"),
            host.path("profile"),
            host.path("work"),
        );
        succeeded(
            &exec
                .run(&host.request(EffectKind::ReadSnapshot, b""), &ctx())
                .await,
        );
        let out = exec
            .run(
                &host.request(
                    EffectKind::ApplyPatch {
                        expected_base: base,
                    },
                    fix_patch().as_bytes(),
                ),
                &ctx(),
            )
            .await;
        succeeded(&out);
        out.new_workspace.unwrap()
    };
    let env = env_with(&[("AGENTOS_TEST_KILL_VM_AFTER_REQUEST", "1")]);
    let (res, job) = fx
        .run_job(
            EffectKind::ApplyPatch {
                expected_base: base,
            },
            fix_patch().as_bytes(),
            env,
        )
        .await;
    let WorkerResult::NoOutcome(why) = res else {
        panic!("expected no outcome, got {res:?}")
    };
    assert_eq!(
        why,
        "guest exited before reporting: firecracker killed by signal 9"
    );
    assert!(
        guests_of(&job).is_empty(),
        "the killed guest is still running"
    );
    // A `git apply` the dead guest started may still finish; it never leaves a torn tree.
    thread::sleep(Duration::from_millis(500));
    let now = workspace_digest(&fx.guest_workspace()).unwrap();
    assert!(
        now == base || now == patched,
        "workspace {now} is neither the base {base} nor patched {patched}"
    );
    assert!(job.read_outcome().is_none());
}

#[tokio::test]
async fn ws_lock_busy_is_a_failure_for_retry_kinds_and_unresolved_for_apply_patch() {
    let fx = Fx::new();
    fx.fake_ws_img();
    let lock = fs::File::create(fx.task_dir().join("ws.lock")).unwrap();
    lock.lock().unwrap();
    for kind in [EffectKind::ReadSnapshot, EffectKind::RunVerification] {
        let (out, job) = fx.run(kind, b"").await;
        assert_eq!(reason(&out), "workspace image is attached to another VM");
        assert!(!out.unresolved);
        assert!(!job.path.join("vm.json").exists());
    }
    let (res, job) = fx
        .run_job(
            EffectKind::ApplyPatch {
                expected_base: Digest::of(b"base"),
            },
            fix_patch().as_bytes(),
            test_env(),
        )
        .await;
    let WorkerResult::Outcome(out) = res else {
        panic!("expected the unresolved outcome, got {res:?}")
    };
    assert_eq!(reason(&out), "workspace image is attached to another VM");
    assert!(out.unresolved, "another VM may be applying the patch");
    assert!(!job.path.join("vm.json").exists());
    drop(lock);
    // Once released, the lock is free again for the next job.
    fx.snapshot().await;
}

#[tokio::test]
async fn a_refused_reason_is_copied_verbatim_into_the_failure_outcome_and_bounded() {
    let mut fx = Fx::new();
    fx.snapshot().await;
    let pinned = Digest::of(b"some other profile");
    fx.cfg.profile_digest = Some(pinned);
    let found = workspace_digest(&fx.path("profile")).unwrap();
    let (out, _) = fx.run(EffectKind::RunVerification, b"").await;
    assert_eq!(
        reason(&out),
        format!("profile digest mismatch: pinned {pinned}, found {found}")
    );
    assert!(out.verification.is_none());

    // A reason over the JSON frame limit is a protocol violation, never copied.
    let fx = Fx::with_peer();
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let job = fx.job(&req, &c);
    let guest = peer(&job, |s, _| {
        drain_files(s);
        send(
            s,
            Message::Refused {
                reason: "x".repeat(2 << 20),
            },
        );
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    guest.join().unwrap();
    let why = reason(&out);
    assert!(
        why.starts_with("guest protocol violation: frame too large: json "),
        "{why}"
    );
    assert!(why.len() < 200, "the reason is bounded");
}

#[tokio::test]
async fn a_frame_over_the_limit_is_a_protocol_failure() {
    // 4 GiB announced: refused from the header, nothing allocated, no hang.
    let header = |s: &mut UnixStream, kind: u8| {
        let mut h = u32::MAX.to_be_bytes().to_vec();
        h.push(kind);
        let _ = s.write_all(&h);
        thread::sleep(Duration::from_millis(200));
    };
    for kind in [0u8, 1] {
        let fx = Fx::with_peer();
        let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
        let job = fx.job(&req, &c);
        let guest = peer(&job, move |s, _| {
            drain_files(s);
            header(s, kind);
        });
        let started = Instant::now();
        let out = fx.worker(&job, test_env()).run(&req, &c).await;
        guest.join().unwrap();
        let why = reason(&out);
        assert!(
            why.starts_with("guest protocol violation: frame too large: "),
            "{why}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // After an ApplyPatch was sent, a violation is no outcome (reconciled by inspection).
    let fx = Fx::with_peer();
    fx.fake_ws_img();
    let (req, c) = (
        fx.request(
            EffectKind::ApplyPatch {
                expected_base: Digest::of(b"base"),
            },
            fix_patch().as_bytes(),
        ),
        ctx(),
    );
    let job = fx.job(&req, &c);
    let guest = peer(&job, move |s, _| {
        let _ = read_frame(s, RAW_FRAME_LIMIT);
        header(s, 0);
    });
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    guest.join().unwrap();
    let WorkerResult::NoOutcome(why) = res else {
        panic!("expected no outcome, got {res:?}")
    };
    assert!(
        why.starts_with("guest protocol violation: frame too large: json"),
        "{why}"
    );
}

#[tokio::test]
async fn a_reply_of_the_wrong_type_is_a_protocol_failure() {
    let fx = Fx::with_peer();
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let job = fx.job(&req, &c);
    let guest = peer(&job, |s, _| {
        drain_files(s);
        send(
            s,
            Message::DigestIs {
                workspace_digest: Digest::of(b"x"),
            },
        );
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    guest.join().unwrap();
    assert_eq!(
        reason(&out),
        "guest protocol violation: expected SnapshotDone, got DigestIs"
    );
}

#[tokio::test]
async fn guest_paths_in_replies_never_touch_host_paths() {
    let fx = Fx::with_peer();
    let reported = Digest::of(b"whatever the guest says");
    let evil = vec![
        "../../etc/passwd".to_string(),
        "../../../agentos-escape-marker".to_string(),
        "/agentos-escape-marker".to_string(),
    ];
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let job = fx.job(&req, &c);
    let files = evil.clone();
    let guest = peer(&job, move |s, _| {
        drain_files(s);
        send(
            s,
            Message::SnapshotDone {
                files,
                workspace_digest: reported,
            },
        );
        bye(s);
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    guest.join().unwrap();
    succeeded(&out);
    assert_eq!(
        json(&out),
        serde_json::json!({ "files": evil, "workspace_digest": reported })
    );
    assert_eq!(out.new_workspace, Some(reported));
    assert!(!Path::new("/agentos-escape-marker").exists());
    // Everything the job touched (jobs/, work/) lives under the fixture's root.
    let names: Vec<String> = regular_files(fx.dir.path())
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.contains("agentos-escape-marker") || n == "passwd"),
        "{names:?}"
    );
}

#[tokio::test]
async fn a_verified_with_exit_1_is_never_passed() {
    let fx = Fx::with_peer();
    fx.fake_ws_img();
    let profile_digest = workspace_digest(&fx.path("profile")).unwrap();
    let ws = Digest::of(b"ws");
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx());
    let job = fx.job(&req, &c);
    let guest = peer(&job, move |s, request| {
        assert!(
            matches!(
                request,
                Message::RunVerification {
                    profile_digest: None,
                    timeout_secs: 60,
                    ..
                }
            ),
            "{request:?}"
        );
        drain_files(s);
        send(
            s,
            Message::Verified {
                profile_id: "parser-checks-v1".into(),
                command: vec!["python3".into(), "check_parser.py".into()],
                profile_digest,
                workspace_digest: ws,
                exit_code: Some(1),
                stdout_b64: b64(b"10/10 checks passed\nPASSED\n"),
                stdout_truncated: false,
                stderr_b64: b64(b""),
                stderr_truncated: false,
            },
        );
        bye(s);
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    guest.join().unwrap();
    succeeded(&out);
    let evidence = json(&out);
    assert_eq!(evidence["passed"], false);
    assert_eq!(evidence["exit_code"], 1);
    assert_eq!(evidence["summary"], "PASSED");
    let report = out.verification.unwrap();
    assert!(!report.passed);
    assert_eq!(report.workspace, ws);
}

#[tokio::test]
async fn receipt_fields_come_from_request_json_only() {
    let fx = Fx::with_peer();
    let (req, c) = (
        fx.request(EffectKind::ReadSnapshot, b""),
        AttemptCtx {
            lease_generation: 7,
            ..ctx()
        },
    );
    let job = fx.job(&req, &c);
    let guest = peer(&job, |s, _| {
        drain_files(s);
        send(
            s,
            Message::SnapshotDone {
                files: vec!["a".into()],
                workspace_digest: Digest::of(b"a"),
            },
        );
        bye(s);
    });
    let out = fx.worker(&job, test_env()).run(&req, &c).await;
    guest.join().unwrap();
    let request = job.request().unwrap();
    assert_eq!(out.receipt.effect_id, request.effect_id);
    assert_eq!(out.receipt.attempt_id, request.attempt_id);
    assert_eq!(out.receipt.lease_generation, request.lease_generation);
    assert_eq!(out.receipt.result_digest, Some(Digest::of(&out.output)));
}

#[tokio::test]
async fn an_unpinned_profile_changed_during_the_run_voids_the_evidence() {
    let fx = Fx::new();
    fx.snapshot().await;
    // The check rewrites the protected source while it runs (the guest cannot see it).
    let source = fx.path("profile/extra.txt");
    fx.script_profile(serde_json::json!([
        "python3",
        "-c",
        format!("open({:?}, 'w').write('changed')", source.to_str().unwrap())
    ]));
    let (out, _) = fx.run(EffectKind::RunVerification, b"").await;
    assert_eq!(
        reason(&out),
        "protected profile changed during verification"
    );
    assert!(out.verification.is_none());
}

#[tokio::test]
async fn no_handle_and_no_token_appears_in_vm_json_console_or_firecracker_log() {
    let root = tempfile::tempdir().unwrap();
    let db = Db::open(&root.path().join("agentos.db")).unwrap();
    let (contract, digest) = contract(10);
    let task = db.create_task(&contract, &digest).unwrap();
    db.approve_task(&task).unwrap();
    let handles: Vec<String> = db
        .grants(&task)
        .unwrap()
        .iter()
        .map(|g| g.handle.to_string())
        .collect();
    assert!(!handles.is_empty());

    let fx = Fx::for_task(task);
    let token = fx.cfg.attempt_token.clone();
    let mut jobs = Vec::new();
    let (snap, job) = fx.run(EffectKind::ReadSnapshot, b"").await;
    succeeded(&snap);
    job.write_outcome(&snap).unwrap();
    jobs.push(job);
    let (applied, job) = fx
        .run(
            EffectKind::ApplyPatch {
                expected_base: snap.new_workspace.unwrap(),
            },
            fix_patch().as_bytes(),
        )
        .await;
    succeeded(&applied);
    job.write_outcome(&applied).unwrap();
    jobs.push(job);
    let (verified, job) = fx.run(EffectKind::RunVerification, b"").await;
    succeeded(&verified);
    job.write_outcome(&verified).unwrap();
    jobs.push(job);

    for job in &jobs {
        let request = fs::read(job.path.join("request.json")).unwrap();
        assert!(
            contains(&request, token.as_bytes()),
            "the token travels in request.json"
        );
        let mut seen = Vec::new();
        for file in regular_files(&job.path) {
            if file.file_name().is_some_and(|n| n == "request.json") {
                continue;
            }
            let bytes = fs::read(&file).unwrap();
            assert!(
                !contains(&bytes, token.as_bytes()),
                "the token leaked into {}",
                file.display()
            );
            for h in &handles {
                assert!(
                    !contains(&bytes, h.as_bytes()),
                    "a handle leaked into {}",
                    file.display()
                );
            }
            seen.push(file.file_name().unwrap().to_string_lossy().into_owned());
        }
        for name in [
            "vm.json",
            "console.log",
            "firecracker.log",
            "stderr.log",
            "outcome.json",
        ] {
            assert!(
                seen.iter().any(|n| n == name),
                "{name} was not checked: {seen:?}"
            );
        }
        assert!(
            fs::metadata(job.path.join("v.sock"))
                .map(|m| m.file_type().is_socket())
                .unwrap_or(true)
        );
    }
}

/// The `agentos-supervisor worker <job>` process, with `AGENTOS_TEST_WORKERS` set to `workers`
/// or removed, independent of this process's environment.
fn worker_process(job: &JobDir, workers: Option<&str>) -> std::process::ExitStatus {
    let mut cmd = std::process::Command::new(common::SUPERVISOR_BIN);
    cmd.arg("worker")
        .arg(&job.path)
        .env_remove("AGENTOS_TEST_WORKERS");
    if let Some(v) = workers {
        cmd.env("AGENTOS_TEST_WORKERS", v);
    }
    cmd.status().unwrap()
}

#[test]
fn run_worker_runs_a_firecracker_request_and_writes_its_outcome() {
    // Without AGENTOS_TEST_WORKERS=1 the fake launcher is refused, and the refusal is the
    // outcome the worker process writes.
    let fx = Fx::new();
    let job = fx.job(&fx.request(EffectKind::ReadSnapshot, b""), &ctx());
    assert!(worker_process(&job, None).success());
    let out = job.read_outcome().expect("an outcome was written");
    let why = reason(&out);
    assert!(
        why.starts_with("firecracker worker unavailable: ")
            && why.contains("AGENTOS_TEST_WORKERS=1"),
        "{why}"
    );
    assert_eq!(out.receipt.attempt_id, job.request().unwrap().attempt_id);
}

/// A JSON frame with `body` (not necessarily valid JSON).
fn raw_json_frame(s: &mut UnixStream, body: &[u8]) {
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.push(0);
    frame.extend_from_slice(body);
    let _ = s.write_all(&frame);
}

/// ~100 KiB of a reply type full of newlines and forged supervisor.log lines.
fn forged_type_body() -> Vec<u8> {
    let line = "a\\n[1700000000000] supervisor 1: forged line\\n\\u001b[2J";
    format!(r#"{{"type":"{}"}}"#, line.repeat(100 * 1024 / line.len())).into_bytes()
}

fn assert_clean(why: &str) {
    assert!(why.len() < 1024, "unbounded reason: {} bytes", why.len());
    assert!(
        !why.chars().any(|c| c.is_control()),
        "control characters in {why:?}"
    );
}

#[tokio::test]
async fn guest_text_in_a_no_outcome_reason_is_escaped_and_bounded() {
    for body in [
        forged_type_body(),
        b"not json\n[1] supervisor 1: forged\n".repeat(4096),
    ] {
        let fx = Fx::with_peer();
        fx.fake_ws_img();
        let (req, c) = (
            fx.request(
                EffectKind::ApplyPatch {
                    expected_base: Digest::of(b"base"),
                },
                fix_patch().as_bytes(),
            ),
            ctx(),
        );
        let job = fx.job(&req, &c);
        let guest = peer(&job, move |s, _| {
            let _ = read_frame(s, RAW_FRAME_LIMIT);
            raw_json_frame(s, &body);
            thread::sleep(Duration::from_millis(200));
        });
        let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
        guest.join().unwrap();
        let WorkerResult::NoOutcome(why) = res else {
            panic!("expected no outcome, got {res:?}")
        };
        assert!(
            why.starts_with("guest protocol violation: invalid json frame: "),
            "{why}"
        );
        assert_clean(&why);
    }
}

#[test]
fn a_no_outcome_worker_cannot_forge_supervisor_log_lines() {
    let fx = Fx::with_peer();
    fx.fake_ws_img();
    let (req, c) = (
        fx.request(
            EffectKind::ApplyPatch {
                expected_base: Digest::of(b"base"),
            },
            fix_patch().as_bytes(),
        ),
        ctx(),
    );
    let job = fx.job(&req, &c);
    fs::write(job.path.join("supervisor.log"), b"").unwrap();
    let guest = peer(&job, |s, _| {
        let _ = read_frame(s, RAW_FRAME_LIMIT);
        raw_json_frame(s, &forged_type_body());
        thread::sleep(Duration::from_millis(200));
    });
    let status = worker_process(&job, Some("1"));
    guest.join().unwrap();
    assert_eq!(status.code(), Some(1), "no outcome is exit 1");
    assert!(job.read_outcome().is_none());
    let log = fs::read_to_string(job.path.join("supervisor.log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines.len(), 1, "{log:?}");
    assert!(
        lines[0].contains("] worker ") && lines[0].contains("failed: guest protocol violation: "),
        "{log:?}"
    );
    assert_clean(lines[0]);
}

// ---------------------------------------------------------------------------------------
// Agent sessions: the model mailbox, the cancel marker and the lease.

/// The guest's `curl` of the scripted CLI: one model call, body from `$IN`, reply to `$OUT`.
const CURL: &str = "/usr/bin/curl -fsS -o \"$HOME/$OUT\" -X POST \"$ANTHROPIC_BASE_URL/v1/messages\" -H 'content-type: application/json' --data-binary @\"$HOME/$IN\"";

/// Writes a scripted CLI outside the workspace; its path is what `argv` names.
fn write_cli(fx: &Fx, script: &str) -> String {
    let path = fx.path("cli.sh");
    fs::write(&path, script).unwrap();
    path.display().to_string()
}

/// A `RunAgentSession` request over `base`, running `/bin/sh <cli>`.
fn session_request(fx: &Fx, base: Digest, cli: &str) -> EffectRequest {
    let payload = AgentSessionSpec {
        argv: vec!["/bin/sh".into(), cli.into()],
        env: vec![],
    }
    .to_payload();
    fx.request(
        EffectKind::RunAgentSession {
            expected_base: base,
        },
        &payload,
    )
}

/// Plays the controller side of the job's session mailbox on its own thread until `done`:
/// each request is answered by `answer` (`None` leaves it unanswered). Returns the requests
/// it saw, in the order it saw them.
fn controller(
    job: &JobDir,
    done: Arc<AtomicBool>,
    answer: impl Fn(u64, &[u8]) -> Option<(u16, Vec<u8>)> + Send + 'static,
) -> thread::JoinHandle<Vec<(u64, Vec<u8>)>> {
    let mailbox = Mailbox::new(job.session_dir());
    thread::spawn(move || {
        let mut seen = Vec::new();
        let mut after = 0;
        while !done.load(Ordering::SeqCst) {
            if let Some((id, body)) = mailbox.controller_next_request(after).unwrap() {
                if seen.last().is_none_or(|(last, _)| *last != id) {
                    seen.push((id, body.clone()));
                }
                if let Some((status, reply)) = answer(id, &body) {
                    mailbox.controller_post_reply(id, status, &reply).unwrap();
                    after = id;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        seen
    })
}

/// The scripted CLI of the happy path: two model calls, an edit in between.
fn two_call_cli(fx: &Fx) -> String {
    write_cli(
        fx,
        &format!(
            "set -eu\n\
             printf '%s' '{{\"model\":\"m\",\"max_tokens\":8,\"stream\":false,\"messages\":[]}}' > \"$HOME/req1.json\"\n\
             IN=req1.json\nOUT=reply1.json\n{CURL}\n\
             printf '\\n# edited by the agent\\n' >> src/parser.py\n\
             printf '%s' '{{\"model\":\"m\",\"max_tokens\":8,\"stream\":false,\"messages\":[{{\"role\":\"user\",\"content\":\"again\"}}]}}' > \"$HOME/req2.json\"\n\
             IN=req2.json\nOUT=reply2.json\n{CURL}\n"
        ),
    )
}

#[tokio::test]
async fn a_session_relays_two_model_calls_in_order_and_returns_the_patch() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let cli = two_call_cli(&fx);
    let (req, c) = (session_request(&fx, base, &cli), ctx());
    let job = fx.job(&req, &c);
    let done = Arc::new(AtomicBool::new(false));
    let mailbox = controller(&job, done.clone(), |id, _| {
        Some((200, format!("{{\"reply\":{id}}}").into_bytes()))
    });
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    done.store(true, Ordering::SeqCst);
    let seen = mailbox.join().unwrap();
    let WorkerResult::Outcome(out) = res else {
        panic!("expected an outcome, got {res:?}")
    };
    succeeded(&out);
    let ids: Vec<u64> = seen.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 2], "requests in order, each once");
    assert!(String::from_utf8_lossy(&seen[1].1).contains("again"));
    let v = json(&out);
    assert_eq!(v["exit_code"], 0, "{v}");
    assert_eq!(v["timed_out"], false, "{v}");
    let patch = String::from_utf8(unb64(v["patch_b64"].as_str().unwrap()).unwrap()).unwrap();
    assert!(
        patch.contains("src/parser.py") && patch.contains("+# edited by the agent"),
        "{patch}"
    );
    // The session never touched the workspace image's files: the patch is for ApplyPatch.
    assert!(guests_of(&job).is_empty());
}

#[tokio::test]
async fn a_controller_that_never_answers_is_ended_by_the_cancel_marker_and_the_vm_reaped() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    // One call, never answered, then the CLI waits for ever.
    let cli = write_cli(
        &fx,
        &format!(
            "set -eu\nprintf '{{}}' > \"$HOME/req1.json\"\nIN=req1.json\nOUT=reply1.json\n{CURL}\nsleep 600\n"
        ),
    );
    let (req, c) = (session_request(&fx, base, &cli), ctx());
    let job = fx.job(&req, &c);
    let done = Arc::new(AtomicBool::new(false));
    let mailbox = controller(&job, done.clone(), |_, _| None);
    let cancel = job.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        cancel.drop_cancel().unwrap();
    });
    let started = Instant::now();
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    done.store(true, Ordering::SeqCst);
    let seen = mailbox.join().unwrap();
    let WorkerResult::Outcome(out) = res else {
        panic!("expected an outcome, got {res:?}")
    };
    assert!(started.elapsed() < Duration::from_secs(60));
    assert_eq!(seen.len(), 1, "the one request is seen, never answered");
    let why = reason(&out);
    assert!(why.contains("cancelled"), "{why}");
    assert!(
        matches!(out.receipt.outcome, Outcome::Failure(_)),
        "a lost session is a failure, never unresolved"
    );
    assert!(guests_of(&job).is_empty(), "the VM is reaped");
    // The guest kills its CLI as it goes: a process that is still exiting gets a moment.
    let until = Instant::now() + Duration::from_secs(10);
    while !pids_with(&cli).is_empty() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(pids_with(&cli).is_empty(), "no CLI survives the job");
    assert!(
        pids_with("sleep 600").is_empty(),
        "no sleep survives the job"
    );
}

#[tokio::test]
async fn a_session_past_its_lease_is_ended_with_the_vm_reaped() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let cli = write_cli(
        &fx,
        &format!(
            "set -eu\nprintf '{{}}' > \"$HOME/req1.json\"\nIN=req1.json\nOUT=reply1.json\n{CURL}\nsleep 600\n"
        ),
    );
    let (req, c) = (session_request(&fx, base, &cli), ctx());
    // A lease two seconds from now, with the job written for it.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let job = JobDir::create(
        &fx.path("jobs"),
        &JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: c.attempt_id.clone(),
            lease_generation: c.lease_generation,
            lease_expiry_ms: now + 2_000,
            task_deadline_ms: i64::MAX,
            worker: WorkerConfig::Firecracker(fx.cfg.clone()),
        },
    )
    .unwrap()
    .0;
    let done = Arc::new(AtomicBool::new(false));
    let mailbox = controller(&job, done.clone(), |_, _| None);
    let started = Instant::now();
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    done.store(true, Ordering::SeqCst);
    mailbox.join().unwrap();
    let WorkerResult::Outcome(out) = res else {
        panic!("expected an outcome, got {res:?}")
    };
    assert!(started.elapsed() < Duration::from_secs(60));
    let why = reason(&out);
    assert!(why.contains("lease"), "{why}");
    assert!(guests_of(&job).is_empty(), "the VM is reaped");
    assert!(
        pids_with("sleep 600").is_empty(),
        "no sleep survives the job"
    );
}

#[tokio::test]
async fn a_session_over_another_base_is_refused_and_nothing_runs() {
    let fx = Fx::new();
    let _ = fx.snapshot().await;
    let cli = write_cli(&fx, "printf ran > \"$HOME/ran\"\n");
    let (req, c) = (
        session_request(&fx, Digest::of(b"not the base"), &cli),
        ctx(),
    );
    let job = fx.job(&req, &c);
    let done = Arc::new(AtomicBool::new(false));
    let mailbox = controller(&job, done.clone(), |_, _| None);
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    done.store(true, Ordering::SeqCst);
    let seen = mailbox.join().unwrap();
    let WorkerResult::Outcome(out) = res else {
        panic!("expected an outcome, got {res:?}")
    };
    assert!(seen.is_empty(), "a refused session sends nothing");
    assert!(matches!(out.receipt.outcome, Outcome::Failure(_)));
    assert!(
        reason(&out).starts_with("version conflict: expected "),
        "{}",
        reason(&out)
    );
}

#[tokio::test]
async fn a_model_request_out_of_order_from_the_guest_is_a_failure() {
    let fx = Fx::with_peer();
    // The workspace image must exist for a job VM (a snapshot would need the peer's protocol).
    fs::create_dir_all(fx.task_dir()).unwrap();
    fs::File::create(fx.task_dir().join("ws.img"))
        .unwrap()
        .set_len(fx.cfg.resources.disk_bytes())
        .unwrap();
    let (req, c) = (
        session_request(&fx, Digest::of(b"base"), "/bin/true"),
        ctx(),
    );
    let job = fx.job(&req, &c);
    let guest = peer(&job, |s, request| {
        assert!(matches!(request, Message::RunAgent { .. }), "{request:?}");
        // The first request of a session is 1; a peer that skips to 2 is a violation.
        send(s, Message::ModelRequest { id: 2 });
        write_frame(s, &Frame::Raw(b"{}".to_vec())).unwrap();
        bye(s);
    });
    let res = fx.worker(&job, test_env()).run_job(&req, &c).await;
    guest.join().unwrap();
    let WorkerResult::Outcome(out) = res else {
        panic!("expected an outcome, got {res:?}")
    };
    let why = reason(&out);
    assert!(why.contains("model request 2"), "{why}");
    assert!(matches!(out.receipt.outcome, Outcome::Failure(_)));
}
