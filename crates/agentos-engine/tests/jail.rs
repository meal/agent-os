//! The jailed launch path of `FirecrackerWorker` over the fake jailer (`common::fake_jailer`),
//! and `jail::stage`/`collect`/`probe` over temp directories: no root, no KVM, no write to
//! `/sys/fs/cgroup`.

mod common;

use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::guest::{Message, Mode, SCRATCH_IMAGE_BYTES};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::firecracker::{render_vm_json, Answer, FirecrackerConfig, FirecrackerWorker, Inspector, Query, WorkerResult};
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::guestlink::GuestLink;
use agentos_engine::jail::{self, JailConfig, JailMode, StageSources};
use agentos_engine::job::{JobDir, JobRequest, WorkerConfig};
use agentos_engine::worker::Worker;
use common::{contract, copy_dir, fix_patch, fixtures, jailed_fake_firecracker_config};
use rustix::process::{kill_process, Pid, Signal};
use tempfile::TempDir;

struct Fx {
    dir: TempDir,
    cfg: FirecrackerConfig,
    task: TaskId,
    contract: Contract,
}

/// The test switch: the whole environment the fake jailer (and the fake guest it `exec`s)
/// gets, as the real jailer clears its own. No `PATH`: `sh` and the guest's `git`/`python3`
/// resolve through the default search path.
fn test_env() -> Vec<(String, String)> {
    vec![("AGENTOS_TEST_WORKERS".to_string(), "1".to_string())]
}

fn env_with(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut env = test_env();
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    env
}

fn ctx() -> AttemptCtx {
    AttemptCtx { attempt_id: AttemptId::new(), lease_generation: 1, worker: "test".into() }
}

impl Fx {
    fn new() -> Fx {
        Fx::for_task(TaskId::new())
    }

    fn for_task(task: TaskId) -> Fx {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let cfg = jailed_fake_firecracker_config(dir.path());
        Fx { dir, cfg, task, contract: contract(10).0 }
    }

    fn jail(&self) -> JailConfig {
        match &self.cfg.jail {
            JailMode::Jailed(jc) => jc.clone(),
            JailMode::Unjailed => unreachable!("the fixture is jailed"),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn task_dir(&self) -> PathBuf {
        self.path("work").join(self.task.as_str())
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

    async fn run_with(&self, req: &EffectRequest, ctx: &AttemptCtx) -> (ExecOutcome, JobDir) {
        let job = self.job(req, ctx);
        let out = self.worker(&job, test_env()).run(req, ctx).await;
        (out, job)
    }

    async fn snapshot(&self) -> Digest {
        let (out, _) = self.run_with(&self.request(EffectKind::ReadSnapshot, b""), &ctx()).await;
        succeeded(&out);
        out.new_workspace.unwrap()
    }

    /// The plan the worker makes for `job` (the attempt id is the jail id).
    fn plan(&self, job: &JobDir, ctx: &AttemptCtx) -> jail::JailPlan {
        jail::plan(&self.jail(), &self.cfg.firecracker_bin, &job.path, &ctx.attempt_id.to_string()).unwrap()
    }
}

fn succeeded(out: &ExecOutcome) {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", String::from_utf8_lossy(&out.output));
}

/// Pids (other than ours) whose command line contains `needle`.
fn pids_with(needle: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else { continue };
        if String::from_utf8_lossy(&cmdline).replace('\0', " ").contains(needle) {
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
        assert!(Instant::now() < until, "no process with {needle:?} appeared");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Runs a `RunVerification` job (after a snapshot) whose check waits for a release file,
/// calls `observe(job, attempt)` while the VM is up and the check is running, then releases
/// the check and returns the job's result and directory. Deterministic: the observation
/// happens while the job is provably in flight, never by racing a fast job.
async fn held(fx: &Fx, observe: impl FnOnce(&JobDir, &AttemptCtx)) -> (WorkerResult, JobDir) {
    fx.snapshot().await;
    let release = fx.path("release");
    let marker = format!("agentos-jail-hold-{}", fx.task);
    let profile = serde_json::json!({
        "id": "jail-hold",
        "command": ["python3", "-c", "import os, sys, time\nwhile not os.path.exists(sys.argv[1]): time.sleep(0.02)", release, marker],
        "protected": true,
    });
    fs::write(fx.path("profile/profile.json"), profile.to_string()).unwrap();
    let (req, c) = (fx.request(EffectKind::RunVerification, b""), ctx());
    let job = fx.job(&req, &c);
    let worker = fx.worker(&job, test_env());
    let (req2, c2) = (req.clone(), c.clone());
    let running = tokio::spawn(async move { worker.run_job(&req2, &c2).await });
    wait_for_pid(&marker, Duration::from_secs(20));
    observe(&job, &c);
    assert!(!running.is_finished(), "the job ended while it was observed");
    fs::write(&release, b"").unwrap();
    let res = running.await.unwrap();
    (res, job)
}

fn outcome(res: WorkerResult) -> ExecOutcome {
    match res {
        WorkerResult::Outcome(out) => out,
        WorkerResult::NoOutcome(why) => panic!("expected an outcome, got none: {why}"),
    }
}

/// Staging sources over `root`: a registry image dir, a task's `ws.img` and a job's
/// `scratch.img`, all on the temp dir's filesystem.
struct Staging {
    root: TempDir,
    cfg: JailConfig,
    plan: jail::JailPlan,
    image: PathBuf,
    ws_img: PathBuf,
    scratch: PathBuf,
    vm_json: serde_json::Value,
}

impl Staging {
    fn new() -> Staging {
        let root = tempfile::tempdir().unwrap();
        let fc = jailed_fake_firecracker_config(root.path());
        let JailMode::Jailed(mut cfg) = fc.jail.clone() else { unreachable!() };
        // As root (the `test` service), stage to the real jail ids, so a chown that reached
        // a registry inode would show; otherwise only our own ids are possible.
        if as_root() {
            (cfg.uid, cfg.gid) = (jail::JAIL_UID, jail::JAIL_GID);
        }
        let job = root.path().join("jobs/e-a");
        fs::create_dir_all(&job).unwrap();
        let ws_img = root.path().join("work/task-1/ws.img");
        fs::create_dir_all(ws_img.parent().unwrap()).unwrap();
        fs::File::create(&ws_img).unwrap().set_len(1 << 20).unwrap();
        let scratch = job.join("scratch.img");
        fs::File::create(&scratch).unwrap().set_len(SCRATCH_IMAGE_BYTES).unwrap();
        let plan = jail::plan(&cfg, &fc.firecracker_bin, &job, "0b8a5f3e-7c1d-4e2a-9f6b-3d5c7e9a1b2c").unwrap();
        let vm_json = render_vm_json(&fc, &jail::chroot_view(&plan));
        Staging { image: fc.image_dir.clone(), root, cfg, plan, ws_img, scratch, vm_json }
    }

    fn job(&self) -> PathBuf {
        self.root.path().join("jobs/e-a")
    }

    fn stage(&self) -> Result<(), String> {
        self.stage_from(&self.image)
    }

    fn stage_from(&self, image: &Path) -> Result<(), String> {
        jail::stage(
            &self.cfg,
            &self.plan,
            &self.job(),
            StageSources {
                kernel: &image.join("vmlinux"),
                rootfs: &image.join("rootfs.squashfs"),
                ws_img: &self.ws_img,
                scratch_img: &self.scratch,
                vm_json: &self.vm_json,
            },
        )
    }
}

fn as_root() -> bool {
    rustix::process::geteuid().is_root()
}

/// `(uid, gid)` of `path`, not following a final symlink.
fn owner(path: &Path) -> (u32, u32) {
    let m = fs::symlink_metadata(path).unwrap();
    (m.uid(), m.gid())
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------------------
// The jailer argv and staging, observed through the worker.

#[tokio::test(flavor = "multi_thread")]
async fn fake_jailer_records_the_argv_the_worker_passes() {
    let fx = Fx::new();
    let jc = fx.jail();
    let (res, _job) = held(&fx, |job, c| {
        let argv = fs::read_to_string(job.path.join("jail/argv.txt")).unwrap();
        let expected = format!(
            "--id {id} --exec-file {fc} --uid {uid} --gid {gid} --chroot-base-dir {base} --cgroup-version 2 \
             --parent-cgroup agentos --cgroup cpu.max=100000 100000 --cgroup memory.max=402653184 \
             --cgroup memory.swap.max=0 --cgroup pids.max=64 --resource-limit fsize=1073741824 \
             -- --no-api --config-file /vm.json\n",
            id = c.attempt_id,
            fc = fx.cfg.firecracker_bin.display(),
            uid = jc.uid,
            gid = jc.gid,
            base = job.path.join("jail").display(),
        );
        assert_eq!(argv, expected);
        assert!(!argv.contains(&fx.cfg.attempt_token), "the attempt token reached the jailer argv");
        for token in argv.split(|c: char| c.is_whitespace() || c == '/' || c == '=') {
            let hex32 = token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit());
            assert!(!hex32, "a 32-hex token {token:?} in the jailer argv");
        }
    })
    .await;
    succeeded(&outcome(res));
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_hard_links_and_marker_over_a_temp_home() {
    let fx = Fx::new();
    let jc = fx.jail();
    let (res, _job) = held(&fx, |job, c| {
        let plan = fx.plan(job, c);
        for name in ["vmlinux", "rootfs.squashfs", "ws.img", "scratch.img", "firecracker.log"] {
            let meta = fs::metadata(plan.chroot.join(name)).unwrap();
            assert_eq!(meta.nlink(), 2, "{name}");
        }
        let ws = fs::metadata(plan.chroot.join("ws.img")).unwrap();
        assert_eq!(ws.mode() & 0o7777, 0o600);
        assert_eq!(ws.uid(), jc.uid);
        assert_eq!(ws.ino(), fs::metadata(fx.task_dir().join("ws.img")).unwrap().ino());
        assert_eq!(
            fs::metadata(plan.chroot.join("firecracker.log")).unwrap().ino(),
            fs::metadata(job.path.join("firecracker.log")).unwrap().ino()
        );
        let marker = fs::read_to_string(job.path.join("jail/cgroup")).unwrap();
        assert_eq!(marker, format!("{}\n", fx.path("cgroup/agentos").join(c.attempt_id.to_string()).display()));
        assert!(fx.path("cgroup/agentos").join(c.attempt_id.to_string()).is_dir(), "the fake jailer made the cgroup");
        // The chroot's vm.json is the chroot view, and no host vm.json is written.
        let vm: serde_json::Value = serde_json::from_slice(&fs::read(plan.chroot.join("vm.json")).unwrap()).unwrap();
        assert_eq!(vm, render_vm_json(&fx.cfg, &jail::chroot_view(&plan)));
        assert!(!job.path.join("vm.json").exists());
    })
    .await;
    succeeded(&outcome(res));
}

#[test]
fn stage_links_only_the_documented_files_and_nothing_else() {
    let s = Staging::new();
    s.stage().unwrap();
    assert_eq!(
        sorted_names(&s.plan.chroot),
        ["firecracker.log", "rootfs.squashfs", "scratch.img", "vm.json", "vmlinux", "ws.img"]
    );
    assert_eq!(sorted_names(&s.plan.base), ["cgroup", "firecracker"]);
    let vm: serde_json::Value = serde_json::from_slice(&fs::read(s.plan.chroot.join("vm.json")).unwrap()).unwrap();
    assert_eq!(vm, s.vm_json);
    let meta = fs::metadata(s.plan.chroot.join("vm.json")).unwrap();
    assert_eq!((meta.mode() & 0o7777, meta.uid()), (0o644, s.cfg.uid));
    let log = fs::metadata(s.plan.chroot.join("firecracker.log")).unwrap();
    assert_eq!((log.len(), log.mode() & 0o7777, log.uid(), log.nlink()), (0, 0o600, s.cfg.uid, 2));
    let scratch = fs::metadata(s.plan.chroot.join("scratch.img")).unwrap();
    assert_eq!((scratch.mode() & 0o7777, scratch.uid()), (0o600, s.cfg.uid));
    assert_eq!(fs::read_to_string(s.job().join("jail/cgroup")).unwrap(), format!("{}\n", s.plan.cgroup.display()));

    // What the jail uid owns, at the chroot link and at the original path (one inode).
    let jail_ids = (s.cfg.uid, s.cfg.gid);
    let handed = [
        (s.plan.chroot.join("ws.img"), Some(s.ws_img.clone())),
        (s.plan.chroot.join("scratch.img"), Some(s.scratch.clone())),
        (s.plan.chroot.join("firecracker.log"), Some(s.job().join("firecracker.log"))),
        (s.plan.chroot.join("vm.json"), None),
    ];
    if as_root() {
        println!("ownership branch: root, staged to {}:{}", jail::JAIL_UID, jail::JAIL_GID);
        assert_eq!(jail_ids, (jail::JAIL_UID, jail::JAIL_GID));
    } else {
        println!("ownership branch: not root, staged to the test's own {}:{} (chown cannot be observed)", jail_ids.0, jail_ids.1);
    }
    for (link, original) in handed {
        assert_eq!(owner(&link), jail_ids, "{}", link.display());
        if let Some(original) = original {
            assert_eq!(owner(&original), jail_ids, "{}", original.display());
        }
    }
}

#[test]
fn registry_files_stay_root_owned_and_read_only_after_staging() {
    let s = Staging::new();
    let files = [s.image.join("vmlinux"), s.image.join("rootfs.squashfs")];
    for f in &files {
        fs::set_permissions(f, fs::Permissions::from_mode(0o444)).unwrap();
    }
    let before: Vec<(u32, u32)> = files.iter().map(|f| owner(f)).collect();
    if as_root() {
        println!("ownership branch: root, registry 0:0, staged to {}:{}", s.cfg.uid, s.cfg.gid);
        assert!(before.iter().all(|o| *o == (0, 0)), "{before:?}");
        assert_eq!((s.cfg.uid, s.cfg.gid), (jail::JAIL_UID, jail::JAIL_GID));
    } else {
        println!("ownership branch: not root, registry owned by {:?}, staged to the same ids (chown cannot be observed)", before[0]);
    }
    s.stage().unwrap();
    for (f, was) in files.iter().zip(before) {
        let link = s.plan.chroot.join(f.file_name().unwrap());
        for path in [f, &link] {
            let meta = fs::metadata(path).unwrap();
            assert_eq!(meta.mode() & 0o7777, 0o444, "{}", path.display());
            assert_eq!((meta.uid(), meta.gid()), was, "{}", path.display());
            assert_eq!(meta.nlink(), 2, "{}", path.display());
        }
    }
    // ws.img and scratch.img changed hands; the registry did not.
    for img in [&s.ws_img, &s.scratch] {
        let meta = fs::metadata(img).unwrap();
        assert_eq!((meta.mode() & 0o7777, meta.uid(), meta.gid()), (0o600, s.cfg.uid, s.cfg.gid), "{}", img.display());
    }
}

#[test]
fn stage_refuses_a_symlinked_or_non_regular_source() {
    let s = Staging::new();
    // ws.img replaced by a symlink to a file the jail must never own.
    let target = s.root.path().join("precious");
    fs::write(&target, b"x").unwrap();
    let before = (owner(&target), fs::metadata(&target).unwrap().mode());
    fs::remove_file(&s.ws_img).unwrap();
    std::os::unix::fs::symlink(&target, &s.ws_img).unwrap();
    let err = s.stage().unwrap_err();
    assert_eq!(err, format!("cannot prepare the jail: {} is not a regular file", s.ws_img.display()));
    assert_eq!((owner(&target), fs::metadata(&target).unwrap().mode()), before, "the symlink target was touched");
    assert!(!s.plan.chroot.join("ws.img").exists());

    // A directory as scratch.img, and a symlinked registry kernel.
    let s = Staging::new();
    fs::remove_file(&s.scratch).unwrap();
    fs::create_dir(&s.scratch).unwrap();
    assert_eq!(s.stage().unwrap_err(), format!("cannot prepare the jail: {} is not a regular file", s.scratch.display()));
    let s = Staging::new();
    let kernel = s.image.join("vmlinux");
    fs::rename(&kernel, s.root.path().join("vmlinux.real")).unwrap();
    std::os::unix::fs::symlink(s.root.path().join("vmlinux.real"), &kernel).unwrap();
    assert_eq!(s.stage().unwrap_err(), format!("cannot prepare the jail: {} is not a regular file", kernel.display()));
}

#[test]
fn stage_reports_exdev_with_both_paths() {
    let s = Staging::new();
    let shm = match tempfile::tempdir_in("/dev/shm") {
        Ok(d) => d,
        Err(e) => {
            println!("SKIPPED: /dev/shm is not usable here: {e}");
            return;
        }
    };
    if fs::metadata(shm.path()).unwrap().dev() == fs::metadata(s.root.path()).unwrap().dev() {
        println!("SKIPPED: /dev/shm is not a separate filesystem here");
        return;
    }
    copy_dir(&s.image, shm.path());
    let err = s.stage_from(shm.path()).unwrap_err();
    assert_eq!(
        err,
        format!(
            "cannot prepare the jail: {} and {} are on different filesystems",
            shm.path().join("vmlinux").display(),
            s.plan.chroot.join("vmlinux").display()
        )
    );
}

#[test]
fn stale_exec_copy_in_the_chroot_is_refused() {
    let s = Staging::new();
    fs::create_dir_all(&s.plan.chroot).unwrap();
    let stale = s.plan.chroot.join("firecracker");
    fs::write(&stale, b"old copy").unwrap();
    let err = s.stage().unwrap_err();
    assert!(err.contains(&format!("stale jail: {} exists", stale.display())), "{err}");
    assert_eq!(sorted_names(&s.plan.chroot), ["firecracker"], "nothing was staged");
}

// ---------------------------------------------------------------------------------------
// The jailed worker end to end.

/// Snapshot, fix patch and verification, with the same requests and attempt contexts.
async fn three_kinds(run: impl AsyncFn(EffectRequest, AttemptCtx) -> ExecOutcome, fx_req: &dyn Fn(EffectKind, &[u8]) -> EffectRequest, ctxs: &[AttemptCtx; 3]) -> Vec<ExecOutcome> {
    let snap = run(fx_req(EffectKind::ReadSnapshot, b""), ctxs[0].clone()).await;
    succeeded(&snap);
    let base = snap.new_workspace.unwrap();
    let applied = run(fx_req(EffectKind::ApplyPatch { expected_base: base }, fix_patch().as_bytes()), ctxs[1].clone()).await;
    succeeded(&applied);
    let verified = run(fx_req(EffectKind::RunVerification, b""), ctxs[2].clone()).await;
    succeeded(&verified);
    vec![snap, applied, verified]
}

#[tokio::test]
async fn jailed_worker_runs_all_three_effects_through_the_fake_jailer_with_identical_outcomes() {
    let task = TaskId::new();
    let (fc, host) = (Fx::for_task(task.clone()), Fx::for_task(task));
    let ctxs = [ctx(), ctx(), ctx()];
    let exec = FixtureExecutor::new(host.path("snapshot"), host.path("profile"), host.path("work"));
    let direct = three_kinds(async |req, c| exec.run(&req, &c).await, &|k, p| host.request(k, p), &ctxs).await;
    let jailed = three_kinds(async |req, c| fc.run_with(&req, &c).await.0, &|k, p| fc.request(k, p), &ctxs).await;
    for (j, d) in jailed.iter().zip(&direct) {
        assert_eq!(String::from_utf8_lossy(&j.output), String::from_utf8_lossy(&d.output));
        assert_eq!(j.new_workspace, d.new_workspace);
        assert_eq!(j.verification, d.verification);
        assert_eq!(j, d);
    }
    assert!(jailed[2].verification.as_ref().unwrap().passed);
}

#[tokio::test(flavor = "multi_thread")]
async fn jailed_worker_connects_to_the_chroot_socket_not_the_job_socket() {
    let fx = Fx::new();
    let (res, job) = held(&fx, |job, c| {
        let chroot_sock = jail::host_uds(&fx.plan(job, c));
        assert!(fs::metadata(&chroot_sock).unwrap().file_type().is_socket(), "{}", chroot_sock.display());
        assert!(!job.path.join("v.sock").exists());
    })
    .await;
    succeeded(&outcome(res));
    assert!(!job.path.join("v.sock").exists(), "<job>/v.sock never exists jailed");
}

#[tokio::test]
async fn staged_ws_img_is_owned_by_the_jail_uid_and_still_inspectable() {
    let fx = Fx::new();
    let jc = fx.jail();
    let digest = fx.snapshot().await;
    let ws = fs::metadata(fx.task_dir().join("ws.img")).unwrap();
    assert_eq!((ws.uid(), ws.gid(), ws.mode() & 0o7777), (jc.uid, jc.gid, 0o600));

    // A second fake-jailed launch, as the inspector makes it (Task 7): its own directory
    // outside the work root, id `inspect-<uuid>`, the task's ws.img staged again.
    let uuid = AttemptId::new().to_string();
    let dir = fx.path("inspect").join(fx.task.as_str()).join(&uuid);
    fs::create_dir_all(&dir).unwrap();
    let scratch = dir.join("scratch.img");
    fs::File::create(&scratch).unwrap().set_len(SCRATCH_IMAGE_BYTES).unwrap();
    let plan = jail::plan(&jc, &fx.cfg.firecracker_bin, &dir, &format!("inspect-{uuid}")).unwrap();
    let vm_json = render_vm_json(&fx.cfg, &jail::chroot_view(&plan));
    jail::stage(
        &jc,
        &plan,
        &dir,
        StageSources {
            kernel: &fx.cfg.image_dir.join("vmlinux"),
            rootfs: &fx.cfg.image_dir.join("rootfs.squashfs"),
            ws_img: &fx.task_dir().join("ws.img"),
            scratch_img: &scratch,
            vm_json: &vm_json,
        },
    )
    .unwrap();
    let mut child = Command::new(&jc.jailer_bin)
        .args(jail::jailer_args(&jc, &plan, &fx.cfg.firecracker_bin, fx.cfg.vcpus, fx.cfg.memory_mib))
        .env_clear()
        .envs(test_env())
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    let mut link = GuestLink::connect(&jail::host_uds(&plan), until).unwrap();
    let hello = Message::Hello {
        protocol: 1,
        attempt_token: agentos_core::guest::mint_attempt_token(),
        task_id: fx.task.as_str().to_string(),
        effect_id: "inspect".into(),
        attempt_id: uuid.clone(),
        lease_generation: 0,
        mode: Mode::Inspect,
    };
    assert!(matches!(link.hello(hello, until).unwrap(), Message::Ready { mode: Mode::Inspect, .. }));
    link.send(&Message::Digest).unwrap();
    assert_eq!(link.recv(until).unwrap(), Message::DigestIs { workspace_digest: digest });
    link.send(&Message::Shutdown).unwrap();
    let _ = link.recv(until);
    drop(link);
    let status = wait_or_kill(&mut child, Duration::from_secs(5));
    assert!(status.success(), "{status}");
    let collected = jail::collect(&dir, &jc.cgroup_root).unwrap();
    assert_eq!((collected.cgroup_removed, collected.jail_removed), (true, true));

    // The inspector itself (Task 7) reads the jail-owned image through the same jailer, and
    // collects the hand-made inspection above as a dead one first.
    let answer = Inspector::new(fx.cfg.clone(), fx.path("inspect")).with_env(test_env()).query(&fx.task, Query::Digest).unwrap();
    assert_eq!(answer, Answer::Digest(digest));
    assert!(!dir.exists(), "the earlier inspect directory is collected");
    let ws = fs::metadata(fx.task_dir().join("ws.img")).unwrap();
    assert_eq!((ws.uid(), ws.gid()), (jc.uid, jc.gid), "still the jail's");
}

fn wait_or_kill(child: &mut std::process::Child, within: Duration) -> std::process::ExitStatus {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = kill_process(Pid::from_raw(child.id() as i32).unwrap(), Signal::KILL);
    child.wait().unwrap()
}

// ---------------------------------------------------------------------------------------
// Collection.

#[tokio::test]
async fn collect_removes_the_cgroup_dir_and_the_jail_tree() {
    let fx = Fx::new();
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let (out, job) = fx.run_with(&req, &c).await;
    succeeded(&out);
    assert!(!job.path.join("jail").exists(), "the jail tree was left behind");
    assert!(!fx.path("cgroup/agentos").join(c.attempt_id.to_string()).exists(), "the cgroup was left behind");
    assert!(fx.path("cgroup/agentos").is_dir(), "only the VM's own cgroup is removed");
    let log = fs::metadata(job.path.join("firecracker.log")).unwrap();
    assert_eq!(log.nlink(), 1);
    assert!(!job.path.join("scratch.img").exists());
    for name in ["console.log", "stderr.log"] {
        assert!(job.path.join(name).is_file(), "{name} is missing");
    }
}

#[test]
fn collect_tolerates_a_missing_cgroup_and_reports_a_busy_one() {
    let s = Staging::new();
    s.stage().unwrap();
    // The marker names a cgroup that was never created (the jailer never ran).
    let collected = jail::collect(&s.job(), &s.cfg.cgroup_root).unwrap();
    assert_eq!((collected.cgroup_removed, collected.jail_removed), (false, true));
    assert!(!s.job().join("jail").exists());
    assert!(s.job().join("firecracker.log").is_file(), "the job's log link stays");

    // A non-empty cgroup directory stands in for one with a process in it.
    fs::remove_file(s.job().join("firecracker.log")).unwrap();
    s.stage().unwrap();
    fs::create_dir_all(s.plan.cgroup.join("child")).unwrap();
    let err = jail::collect(&s.job(), &s.cfg.cgroup_root).unwrap_err();
    assert_eq!(err, format!("cgroup {} still has processes", s.plan.cgroup.display()));
    assert!(s.plan.chroot.join("ws.img").exists(), "jail/ was touched");
    assert!(s.plan.cgroup.is_dir());

    // Emptied, it goes; no marker and no jail is nothing to do.
    fs::remove_dir(s.plan.cgroup.join("child")).unwrap();
    let collected = jail::collect(&s.job(), &s.cfg.cgroup_root).unwrap();
    assert_eq!((collected.cgroup_removed, collected.jail_removed), (true, true));
    let collected = jail::collect(&s.job(), &s.cfg.cgroup_root).unwrap();
    assert_eq!((collected.cgroup_removed, collected.jail_removed), (false, false));
}

#[tokio::test(flavor = "multi_thread")]
async fn collect_never_runs_before_settlement() {
    let fx = Fx::new();
    let cgroup = std::sync::Mutex::new(PathBuf::new());
    let (res, job) = held(&fx, |job, c| {
        // The check hangs inside the VM: the jail and its cgroup are in use and untouched.
        assert!(job.path.join("jail/argv.txt").is_file());
        assert!(fx.plan(job, c).chroot.join("ws.img").is_file());
        let cg = fx.path("cgroup/agentos").join(c.attempt_id.to_string());
        assert!(cg.is_dir());
        thread::sleep(Duration::from_millis(300));
        assert!(job.path.join("jail/argv.txt").is_file(), "the jail was collected under a live VM");
        *cgroup.lock().unwrap() = cg;
    })
    .await;
    succeeded(&outcome(res));
    assert!(!job.path.join("jail").exists(), "collected once the VM was reaped");
    assert!(!cgroup.lock().unwrap().exists());
}

#[tokio::test]
async fn eof_after_apply_patch_in_jailed_mode_yields_no_outcome_and_leaves_the_jail() {
    let fx = Fx::new();
    let base = fx.snapshot().await;
    let (req, c) = (fx.request(EffectKind::ApplyPatch { expected_base: base }, fix_patch().as_bytes()), ctx());
    let job = fx.job(&req, &c);
    let env = env_with(&[("AGENTOS_TEST_KILL_VM_AFTER_REQUEST", "1")]);
    let res = fx.worker(&job, env).run_job(&req, &c).await;
    let WorkerResult::NoOutcome(why) = res else { panic!("expected no outcome, got {res:?}") };
    assert_eq!(why, "guest exited before reporting: firecracker killed by signal 9");
    // The controller collects after settlement (Task 7/8), not the worker.
    assert!(job.path.join("jail/cgroup").is_file());
    assert!(fx.plan(&job, &c).chroot.join("ws.img").is_file());
    let collected = jail::collect(&job.path, &fx.jail().cgroup_root).unwrap();
    assert_eq!((collected.cgroup_removed, collected.jail_removed), (true, true));
    assert!(!job.path.join("jail").exists());
}

// ---------------------------------------------------------------------------------------
// The real probe, per environment, and path validation.

#[test]
fn probe_result_is_consistent_with_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    // A jailer that answers --version like v1.17, so the probe reaches the cgroup steps.
    let jailer = dir.path().join("jailer");
    fs::write(&jailer, "#!/bin/sh\necho 'Jailer v1.17.0'\n").unwrap();
    fs::set_permissions(&jailer, fs::Permissions::from_mode(0o755)).unwrap();
    // The root the controller configures is the one /proc/mounts names (as the CLI builds it).
    let cgroup_root = jail::find_cgroup2_root(&fs::read_to_string("/proc/mounts").unwrap()).unwrap_or_else(|| "/sys/fs/cgroup".into());
    let cfg = JailConfig { jailer_bin: jailer, uid: 61000, gid: 61000, cgroup_root };
    let [jobs, inspect, work, image] = ["jobs", "inspect", "work", "image"].map(|n| dir.path().join(n));
    for d in [&jobs, &work, &image] {
        fs::create_dir_all(d).unwrap();
    }
    let result = jail::probe(&cfg, &jobs, &inspect, &work, &image);
    let euid = rustix::process::geteuid().as_raw();
    let mounts = fs::read_to_string("/proc/mounts").unwrap();
    let cgroup_ro = mounts
        .lines()
        .map(|l| l.split(' ').collect::<Vec<_>>())
        .find(|f| f.len() > 3 && f[2] == "cgroup2")
        .is_some_and(|f| f[3].split(',').any(|o| o == "ro"));
    if euid != 0 {
        println!("probe branch: not root (uid {euid}) => {result:?}");
        assert!(result.as_ref().unwrap_err().contains("needs root"), "{result:?}");
    } else if cgroup_ro {
        println!("probe branch: root with a read-only cgroup tree => {result:?}");
        assert!(result.as_ref().unwrap_err().contains("read-only"), "{result:?}");
    } else {
        println!("probe branch: root with a writable cgroup tree => {result:?}");
        assert_eq!(result, Ok(()));
    }
}

/// The probe, `collect` and the real jailer must agree on the cgroup root: a configured root
/// other than the one /proc/mounts names fails the probe, before any delegation write.
#[test]
fn probe_refuses_a_cgroup_root_that_disagrees_with_proc_mounts() {
    let dir = tempfile::tempdir().unwrap();
    let jailer = dir.path().join("jailer");
    fs::write(&jailer, "#!/bin/sh\necho 'Jailer v1.17.0'\n").unwrap();
    fs::set_permissions(&jailer, fs::Permissions::from_mode(0o755)).unwrap();
    let elsewhere = dir.path().join("not-the-cgroup-root");
    let cfg = JailConfig { jailer_bin: jailer, uid: 61000, gid: 61000, cgroup_root: elsewhere.clone() };
    let [jobs, inspect, work, image] = ["jobs", "inspect", "work", "image"].map(|n| dir.path().join(n));
    for d in [&jobs, &work, &image] {
        fs::create_dir_all(d).unwrap();
    }
    let result = jail::probe(&cfg, &jobs, &inspect, &work, &image);
    let found = jail::find_cgroup2_root(&fs::read_to_string("/proc/mounts").unwrap());
    let euid = rustix::process::geteuid().as_raw();
    match (euid, found) {
        (0, Some(found)) => {
            let expected = format!("the jail is configured for cgroup root {}, but the cgroup v2 hierarchy in /proc/mounts is {}", elsewhere.display(), found.display());
            assert_eq!(result, Err(expected));
            assert!(!elsewhere.exists(), "nothing was written under the configured root");
        }
        (0, None) => assert_eq!(result, Err("no cgroup v2 hierarchy in /proc/mounts".into())),
        _ => {
            println!("probe branch: not root (uid {euid}) => {result:?}");
            assert!(result.unwrap_err().contains("needs root"));
        }
    }
}

#[test]
fn relative_jailer_bin_or_cgroup_root_is_rejected_by_job_dir_create() {
    let fx = Fx::new();
    let (req, c) = (fx.request(EffectKind::ReadSnapshot, b""), ctx());
    let jobs = fx.path("jobs-relative");
    for (name, edit) in [
        ("jailer_bin", (|j: &mut JailConfig| j.jailer_bin = "jailer".into()) as fn(&mut JailConfig)),
        ("cgroup_root", |j: &mut JailConfig| j.cgroup_root = "sys/fs/cgroup".into()),
    ] {
        let mut jc = fx.jail();
        edit(&mut jc);
        let mut cfg = fx.cfg.clone();
        cfg.jail = JailMode::Jailed(jc);
        let request = JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: vec![],
            contract: req.contract.clone(),
            attempt_id: c.attempt_id.clone(),
            lease_generation: 1,
            lease_expiry_ms: i64::MAX,
            task_deadline_ms: i64::MAX,
            worker: WorkerConfig::Firecracker(cfg.clone()),
        };
        let err = JobDir::create(&jobs, &request).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{name}");
        assert!(err.to_string().starts_with(name), "{err}");
        assert!(WorkerConfig::Firecracker(cfg).check_paths().is_err(), "{name}");
    }
    assert!(!jobs.exists() || fs::read_dir(&jobs).unwrap().count() == 0, "nothing was created");
}
