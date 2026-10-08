#![allow(dead_code)]

pub mod evidence;
pub mod http;
pub mod kvm;
pub mod live;
pub mod procs;

use std::fs;
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{EffectId, EffectRecord};
use agentos_core::guest::mint_attempt_token;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::{Agent, AgentAction, ModelAgent, Observation};
use agentos_engine::crash::CrashHook;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_engine::firecracker::FirecrackerConfig;
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::jail::{JailConfig, JailMode};
use agentos_engine::job::{HostConfig, WorkerConfig};
use agentos_engine::model::executor::ModelExecutor;
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::ModelProvider;
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::shadow::ShadowReader;
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_engine::supervisor::SupervisorCmd;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::{Db, StoredEvent};
use tempfile::TempDir;

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

/// The real supervisor binary, built by cargo for the engine's integration tests.
pub const SUPERVISOR_BIN: &str = env!("CARGO_BIN_EXE_agentos-supervisor");
pub const TEST_WORKERS_ENV: &str = "AGENTOS_TEST_WORKERS";
pub const EXIT_BEFORE_RECEIPT_ENV: &str = "AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT";

/// The fixture worker over `root`'s `snapshot`, `profile` and `work` (absolute paths, as
/// the supervisor runs in its own working directory).
pub fn host_config(root: &Path) -> HostConfig {
    HostConfig {
        snapshot_dir: root.join("snapshot"),
        profile_dir: root.join("profile"),
        work_root: root.join("work"),
        verify_timeout_secs: 60,
        profile_digest: None,
    }
}

/// A dummy registered guest image under `<root>/image`: `image.json` naming a 16-byte
/// `vmlinux` and `rootfs.squashfs` (the fake tier never boots them).
pub fn fake_image(root: &Path) -> PathBuf {
    let dir = root.join("image");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("image.json"),
        r#"{"id":"python-stdlib-v1","protocol":1,"kernel":"vmlinux","rootfs":"rootfs.squashfs","agent_version":"0.1.0","kernel_sha256":"0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447","built_from":"test"}"#,
    )
    .unwrap();
    fs::write(dir.join("vmlinux"), [0x7fu8; 16]).unwrap();
    fs::write(dir.join("rootfs.squashfs"), [0x68u8; 16]).unwrap();
    dir
}

/// A Firecracker worker config over `root`'s `snapshot`, `profile` and `work`, with the fake
/// guest (`agentos-supervisor fake-guest`) as its launcher, unjailed, and the dummy image
/// pinned by its digest.
pub fn fake_firecracker_config(root: &Path) -> FirecrackerConfig {
    let image_dir = fake_image(root);
    FirecrackerConfig {
        resources: agentos_core::resources::VmResources::V0,
        firecracker_bin: root.join("bin/firecracker"),
        image_digest: workspace_digest(&image_dir).unwrap(),
        image_dir,
        snapshot_dir: root.join("snapshot"),
        profile_dir: root.join("profile"),
        profile_digest: None,
        work_root: root.join("work"),
        verify_timeout_secs: 60,
        vcpus: 1,
        memory_mib: 256,
        attempt_token: mint_attempt_token(),
        launcher: GuestLauncher::Fake {
            program: SUPERVISOR_BIN.into(),
            prefix_args: Vec::new(),
        },
        jail: JailMode::Unjailed,
    }
}

/// The fake jailer, written as `<root>/fake-jailer.sh` (0755): it parses the documented
/// jailer argv, records it in `<base>/argv.txt`, creates the "cgroup" directory under
/// `cgroup_root` and `exec`s the fake guest in place (as the real jailer `exec`s Firecracker,
/// keeping the pid) in the chroot, on the chroot's socket. The socket is passed bare
/// (`v.sock`, relative to the chroot, as `vm.json` names it): `<chroot>/v.sock` is longer
/// than a Unix socket address. The guest's root is the task directory that holds the inode
/// staged as `<chroot>/ws.img`.
pub fn fake_jailer(root: &Path, cgroup_root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
    let script = r#"#!/bin/sh
# fake jailer: records argv, creates the "cgroup", execs the fake guest on the chroot socket (exec in place)
set -eu
WORK_ROOT=@WORK_ROOT@; SUPERVISOR=@SUPERVISOR@; CGROUP_ROOT=@CGROUP_ROOT@
id=; base=; exec_file=; parent=agentos; all="$*"
while [ $# -gt 0 ]; do case "$1" in
  --id) id=$2; shift 2;; --exec-file) exec_file=$2; shift 2;; --chroot-base-dir) base=$2; shift 2;;
  --parent-cgroup) parent=$2; shift 2;; --) shift; break;; *) shift;; esac; done
chroot="$base/$(basename "$exec_file")/$id/root"
mkdir -p "$CGROUP_ROOT/$parent/$id"
# argv.txt is the signal tests wait for: create the "cgroup" first, publish the file atomically
printf '%s\n' "$all" > "$base/argv.txt.tmp"
mv "$base/argv.txt.tmp" "$base/argv.txt"
found=$(find "$WORK_ROOT" -samefile "$chroot/ws.img" -print -quit)   # the hard link finds the task
[ -n "$found" ] || { echo "fake jailer: no task under $WORK_ROOT holds $chroot/ws.img" >&2; exit 1; }
root=$(dirname "$found")
cd "$chroot"
exec "$SUPERVISOR" fake-guest v.sock "$root"
"#
    .replace("@WORK_ROOT@", &quote(&root.join("work")))
    .replace("@SUPERVISOR@", &quote(Path::new(SUPERVISOR_BIN)))
    .replace("@CGROUP_ROOT@", &quote(cgroup_root));
    let path = root.join("fake-jailer.sh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// `fake_firecracker_config(root)`, jailed through the fake jailer as the test's own uid and
/// gid, with `<root>/cgroup` standing in for the cgroup v2 root. `<root>/bin/firecracker` is
/// a 16-byte dummy that names the chroot level (`--exec-file`); nothing executes it.
///
/// The launcher stays `Fake` (the brief said `Real`): a `Real` launcher's preflight opens
/// `/dev/kvm` and runs `firecracker --version`, which the default tier cannot pass; the jailed
/// launch path does not depend on the launcher (the jailer is what runs).
pub fn jailed_fake_firecracker_config(root: &Path) -> FirecrackerConfig {
    let mut cfg = fake_firecracker_config(root);
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/firecracker"), [0u8; 16]).unwrap();
    let cgroup_root = root.join("cgroup");
    cfg.jail = JailMode::Jailed(JailConfig {
        jailer_bin: fake_jailer(root, &cgroup_root),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        cgroup_root,
    });
    cfg
}

/// Which worker the migrated 3a suites (crash matrix, deadline, revoke, supervised) run
/// against: `AGENTOS_TEST_WORKER` = `host` (default) | `firecracker-fake`, the latter jailed
/// through the fake jailer with `AGENTOS_TEST_JAIL=fake` | `firecracker`, the real jailed
/// worker of the KVM tier (`kvm::require()` must pass). Any other value panics, so a typo
/// never runs the host tier under the name of another.
pub fn test_worker() -> &'static str {
    let worker = match std::env::var("AGENTOS_TEST_WORKER").as_deref() {
        Err(_) | Ok("") | Ok("host") => "host",
        Ok("firecracker-fake") => "firecracker-fake",
        Ok("firecracker") => "firecracker",
        Ok(other) => panic!(
            "AGENTOS_TEST_WORKER={other:?}: this tier knows host, firecracker-fake and firecracker"
        ),
    };
    if test_jail_fake() && worker != "firecracker-fake" {
        panic!("AGENTOS_TEST_JAIL=fake needs AGENTOS_TEST_WORKER=firecracker-fake");
    }
    worker
}

/// `AGENTOS_TEST_JAIL=fake` (any other non-empty value panics).
pub fn test_jail_fake() -> bool {
    match std::env::var("AGENTOS_TEST_JAIL").as_deref() {
        Err(_) | Ok("") => false,
        Ok("fake") => true,
        Ok(other) => panic!("AGENTOS_TEST_JAIL={other:?}: this tier knows only fake"),
    }
}

/// Whether the migrated suites run on the Firecracker worker over the fake guest.
pub fn fake_mode() -> bool {
    test_worker() == "firecracker-fake"
}

/// Whether the migrated suites run on the real, jailed Firecracker worker (KVM tier).
pub fn real_mode() -> bool {
    test_worker() == "firecracker"
}

/// A scratch root for one test's home: in real mode on the guest image's filesystem (the
/// jail hard-links the image; elsewhere `Kvm::image_for` would copy it), else a plain
/// temporary directory.
pub fn scratch_root() -> TempDir {
    if real_mode() {
        real_kvm().root()
    } else {
        tempfile::tempdir().unwrap()
    }
}

/// The KVM tier's settings, for `AGENTOS_TEST_WORKER=firecracker`: the gate must pass (it
/// panics with its reasons when it cannot; without `AGENTOS_KVM_TESTS` this panics too, as
/// the real worker was asked for).
pub fn real_kvm() -> kvm::Kvm {
    kvm::require().expect("AGENTOS_TEST_WORKER=firecracker needs the KVM tier: set AGENTOS_KVM_TESTS=1 (docker compose run --rm test-kvm …)")
}

/// The worker the migrated suites use over `root`'s `snapshot`, `profile` and `work`:
/// `Host(host_config(root))`, or in fake mode `Firecracker(fake_firecracker_config(root))`
/// (jailed with `AGENTOS_TEST_JAIL=fake`). The Firecracker config is built once per root
/// and kept in `<root>/worker-config.json`: a restarted controller must never rewrite the
/// image, the dummy binary or the fake jailer while a job that outlived it uses them.
pub fn worker_config(root: &Path) -> WorkerConfig {
    if !fake_mode() && !real_mode() {
        return WorkerConfig::Host(host_config(root));
    }
    let kept = root.join("worker-config.json");
    if let Ok(bytes) = fs::read(&kept) {
        return serde_json::from_slice(&bytes).unwrap();
    }
    let cfg = if real_mode() {
        // The real tier is always jailed.
        real_kvm().jailed_config(root)
    } else if test_jail_fake() {
        jailed_fake_firecracker_config(root)
    } else {
        fake_firecracker_config(root)
    };
    let worker = WorkerConfig::Firecracker(cfg);
    fs::write(&kept, serde_json::to_vec(&worker).unwrap()).unwrap();
    worker
}

/// Where the chosen worker keeps `task`'s workspace tree on the host: the host worker's
/// `<work>/<task>/ws`, or the fake guest's view of `ws.img`, `<work>/<task>/workspace`, or
/// (real mode) a fresh copy of the tree in the ext4 `ws.img`, read with `debugfs` (the host
/// never mounts a guest's filesystem): `<root>/ws-dumps/<n>`.
pub fn workspace_dir(root: &Path, task: &TaskId) -> PathBuf {
    let task_dir = root.join("work").join(task.as_str());
    if real_mode() {
        return dump_ws_img(&task_dir.join("ws.img"), &root.join("ws-dumps"));
    }
    if fake_mode() {
        task_dir.join("workspace")
    } else {
        task_dir.join("ws")
    }
}

/// The tree of an ext4 image, copied out with `debugfs -R "rdump / DIR"` into a new
/// directory under `dumps` (read-only: the image is opened without `-w`).
pub fn dump_ws_img(img: &Path, dumps: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = dumps.join(N.fetch_add(1, Ordering::Relaxed).to_string());
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let out = std::process::Command::new("debugfs")
        .arg("-R")
        .arg(format!("rdump / {}", dir.display()))
        .arg(img)
        .output()
        .expect("run debugfs");
    assert!(
        out.status.success(),
        "debugfs rdump of {}: {}",
        img.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    // `rdump` keeps the image's root and its `lost+found` (removed by the guest; never in
    // the digest anyway).
    dir
}

/// Runs `debugfs -w` on `img` with `commands` (one per line), for tests that tamper with a
/// real guest's workspace image from the host. Panics with debugfs's output on failure.
pub fn debugfs_write(img: &Path, commands: &str) {
    use std::io::Write;
    let mut child = std::process::Command::new("debugfs")
        .args(["-w", "-f", "-"])
        .arg(img)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run debugfs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // debugfs exits 0 even when a command fails; it reports a failure as `<command>: <why>`.
    let words: Vec<&str> = commands
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    let failed = text
        .lines()
        .any(|l| words.iter().any(|w| l.starts_with(&format!("{w}: "))));
    assert!(
        out.status.success() && !failed,
        "debugfs -w {} <<< {commands:?}: {text}",
        img.display()
    );
}

/// Writes `content` to `rel` (`dir/name`) in `task`'s workspace behind the worker's back:
/// in the host's (or fake guest's) tree, or (real mode) in `ws.img` with `debugfs`.
pub fn write_in_workspace(root: &Path, task: &TaskId, rel: &str, content: &str) {
    if !real_mode() {
        fs::write(workspace_dir(root, task).join(rel), content).unwrap();
        return;
    }
    let img = root.join("work").join(task.as_str()).join("ws.img");
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let local = root.join(format!("planted-{}", rel.replace('/', "_")));
    fs::write(&local, content).unwrap();
    // `write` refuses an existing name: remove it first (a missing one is fine).
    let _ = std::process::Command::new("debugfs")
        .args(["-w", "-R"])
        .arg(format!("rm /{rel}"))
        .arg(&img)
        .output();
    debugfs_write(
        &img,
        &format!("cd /{dir}\nwrite {} {name}\n", local.display()),
    );
}

/// Makes `task`'s workspace gone for the chosen worker: the host directory, or (fake mode)
/// `ws.img`, which is what the Firecracker worker and the inspector check, and the fake
/// guest's tree with it, or (real mode) `ws.img`.
pub fn lose_workspace(root: &Path, task: &TaskId) {
    if real_mode() {
        fs::remove_file(root.join("work").join(task.as_str()).join("ws.img")).unwrap();
        return;
    }
    if fake_mode() {
        let task_dir = root.join("work").join(task.as_str());
        fs::remove_file(task_dir.join("ws.img")).unwrap();
        fs::remove_dir_all(task_dir.join("workspace")).unwrap();
    } else {
        fs::remove_dir_all(workspace_dir(root, task)).unwrap();
    }
}

/// `(state, process group, session)` of `pid`, from `/proc/<pid>/stat`.
pub fn proc_state(pid: i32) -> Option<(String, i32, i32)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.to_string();
    let pgrp = fields.nth(1)?.parse().ok()?;
    let session = fields.next()?.parse().ok()?;
    Some((state, pgrp, session))
}

pub fn all_pids() -> Vec<i32> {
    fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect()
}

/// Live (non-zombie) processes whose command line contains `path`.
pub fn processes_naming(path: &Path) -> Vec<i32> {
    let needle = path.as_os_str().as_encoded_bytes();
    all_pids()
        .into_iter()
        .filter(|pid| {
            let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            cmdline.windows(needle.len()).any(|w| w == needle)
                && proc_state(*pid).is_some_and(|(s, ..)| s != "Z")
        })
        .collect()
}

#[allow(
    unused_imports,
    reason = "each test crate uses its own part of the shared scan"
)]
pub use procs::{FcProc, firecracker_processes, home_firecrackers, home_vm_ids};

/// `processes_naming(root)` plus, in real mode, the pids of `home_firecrackers(root)`.
pub fn processes_of_home(root: &Path) -> Vec<i32> {
    let mut pids = processes_naming(root);
    if real_mode() {
        pids.extend(home_firecrackers(root).into_iter().map(|p| p.pid));
    }
    pids
}

/// A supervised executor running `worker` jobs under `jobs_root` with the real supervisor
/// binary, counting launches in `counts`, consulting `crash` right after each launch, and
/// with `env` set in the supervisor's (and worker's) environment. In fake mode
/// `AGENTOS_TEST_WORKERS=1` is added (the fake guest launcher, and the inspector that
/// reconciles through it, run only with it).
pub fn supervised(
    jobs_root: &Path,
    worker: WorkerConfig,
    counts: &ExecCounts,
    crash: Option<CrashHook>,
    env: &[(&str, &str)],
) -> SupervisedExecutor {
    let cmd = SupervisorCmd {
        program: SUPERVISOR_BIN.into(),
        prefix_args: Vec::new(),
    };
    let mut exec = SupervisedExecutor::new(jobs_root.to_path_buf(), cmd, worker, counts.clone())
        .unwrap()
        .with_crash(crash);
    if fake_mode() {
        exec = exec.with_env(TEST_WORKERS_ENV, "1");
    }
    for (k, v) in env {
        exec = exec.with_env(*k, *v);
    }
    exec
}

pub fn fix_patch() -> String {
    fs::read_to_string(fixtures().join("parser-repo.fix.patch")).unwrap()
}

/// A patch that only adds a comment to `src/parser.py`: applies, fixes nothing.
pub fn comment_patch() -> String {
    "--- a/src/parser.py\n+++ b/src/parser.py\n@@ -1,2 +1,3 @@\n+# TODO: handle whitespace\n def parse_kv(text: str) -> dict:\n     \"\"\"Parse 'key = value' lines into a dict, skipping blanks and '#' comments.\"\"\"\n".into()
}

/// A syntactically valid patch creating `path` with one line.
pub fn create_patch(path: &str, line: &str) -> String {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+{line}\n"
    )
}

/// A patch editing the first line of `path` from `old` to `new`.
pub fn edit_patch(path: &str, old: &str, new: &str) -> String {
    format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+{new}\n")
}

pub const ALL_CAPS: &[&str] = &[
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "artifact.export",
];

pub fn contract(tool_actions: u32) -> (Contract, Digest) {
    contract_with(tool_actions, ALL_CAPS)
}

pub fn contract_with(tool_actions: u32, caps: &[&str]) -> (Contract, Digest) {
    contract_full(tool_actions, caps, 600)
}

pub const MODEL_CAPS: &[&str] = &[
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "artifact.export",
    "model.request",
];

/// `contract_full` with `MODEL_CAPS` and the given number of model requests.
pub fn contract_model(model_requests: u32, tool_actions: u32) -> (Contract, Digest) {
    contract_limits(model_requests, tool_actions, MODEL_CAPS, 600)
}

pub fn contract_full(
    tool_actions: u32,
    caps: &[&str],
    deadline_seconds: u32,
) -> (Contract, Digest) {
    contract_limits(1, tool_actions, caps, deadline_seconds)
}

fn contract_limits(
    model_requests: u32,
    tool_actions: u32,
    caps: &[&str],
    deadline_seconds: u32,
) -> (Contract, Digest) {
    let caps = serde_json::to_string(caps).unwrap();
    let json = format!(
        r#"{{
        "goal": "fix the parser",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "python-stdlib-v1",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": {model_requests},
            "max_output_tokens_per_request": 1000,
            "tool_actions": {tool_actions},
            "deadline_seconds": {deadline_seconds},
            "worker_vcpus": 1,
            "worker_memory_mib": 256
        }}
    }}"#
    );
    (Contract::parse(&json).unwrap(), Digest::of(json.as_bytes()))
}

/// An approved task whose contract grants `MODEL_CAPS` and `model_requests` model requests.
pub fn model_env(model_requests: u32, tool_actions: u32) -> Env {
    Env::with_model(model_requests, tool_actions)
}

pub struct Env {
    pub dir: TempDir,
    pub db: Db,
    pub blobs: BlobStore,
    pub exec: FixtureExecutor,
    pub task: TaskId,
    pub contract: Contract,
}

impl Env {
    pub fn new(tool_actions: u32) -> Env {
        Env::with_caps(tool_actions, ALL_CAPS)
    }

    /// An approved task (the owner said yes; handles are issued).
    pub fn with_caps(tool_actions: u32, caps: &[&str]) -> Env {
        let env = Env::unapproved(tool_actions, caps);
        env.db.approve_task(&env.task).unwrap();
        env
    }

    /// An approved task whose contract grants `MODEL_CAPS` and `model_requests` model requests.
    pub fn with_model(model_requests: u32, tool_actions: u32) -> Env {
        Env::build(contract_model(model_requests, tool_actions), true)
    }

    /// A task the owner has not approved yet: no handles, no deadline.
    pub fn unapproved(tool_actions: u32, caps: &[&str]) -> Env {
        Env::build(contract_with(tool_actions, caps), false)
    }

    fn build((contract, digest): (Contract, Digest), approve: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let exec = FixtureExecutor::new(
            dir.path().join("snapshot"),
            dir.path().join("profile"),
            dir.path().join("work"),
        );
        let task = db.create_task(&contract, &digest).unwrap();
        if approve {
            db.approve_task(&task).unwrap();
        }
        Env {
            dir,
            db,
            blobs,
            exec,
            task,
            contract,
        }
    }

    /// A second connection to the same database, as a concurrent writer would hold.
    pub fn second_db(&self) -> Db {
        Db::open(&self.dir.path().join("agentos.db")).unwrap()
    }

    /// Another executor over the same snapshot, profile and work root.
    pub fn fixture_exec(&self) -> FixtureExecutor {
        FixtureExecutor::new(
            self.snapshot_dir(),
            self.profile_dir(),
            self.dir.path().join("work"),
        )
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.dir.path().join("profile")
    }

    pub fn snapshot_dir(&self) -> PathBuf {
        self.dir.path().join("snapshot")
    }

    pub fn ws(&self) -> PathBuf {
        self.exec.workspace(&self.task)
    }

    pub fn ws_digest(&self) -> Digest {
        workspace_digest(&self.ws()).unwrap()
    }

    pub fn events(&self) -> Vec<StoredEvent> {
        self.db.events(&self.task).unwrap()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events().into_iter().map(|e| e.event_type).collect()
    }

    pub fn count(&self, event_type: &str) -> usize {
        self.events()
            .iter()
            .filter(|e| e.event_type == event_type)
            .count()
    }

    /// `Denied` audit events whose payload `reason` equals `reason`.
    pub fn denials(&self, reason: &str) -> Vec<serde_json::Value> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "Denied" && e.payload["reason"] == reason)
            .map(|e| e.payload)
            .collect()
    }

    /// Effects of the given kind tag ("ReadSnapshot", "ApplyPatch", ...), in intent order.
    pub fn effects(&self, kind: &str) -> Vec<EffectRecord> {
        self.events()
            .into_iter()
            .filter(|e| e.event_type == "EffectIntended")
            .filter(|e| e.payload["kind"] == kind || e.payload["kind"].get(kind).is_some())
            .map(|e| {
                let id: EffectId = serde_json::from_value(e.payload["effect_id"].clone()).unwrap();
                self.db.effect(&id).unwrap()
            })
            .collect()
    }

    pub fn blob_json(&self, d: &Digest) -> serde_json::Value {
        serde_json::from_slice(&self.blobs.get(d).unwrap()).unwrap()
    }
}

/// Agent driven by a closure, for tests that act on the world while the agent "thinks".
pub struct FnAgent<F: FnMut(&Observation) -> AgentAction>(pub F);

impl<F: FnMut(&Observation) -> AgentAction> Agent for FnAgent<F> {
    fn next(&mut self, obs: &Observation) -> AgentAction {
        (self.0)(obs)
    }
}

/// Wraps the fixture executor with hooks that run around each attempt, so a test can act
/// on the world while an effect is in flight (after dispatch, before completion).
pub struct HookExec<B, A> {
    pub inner: FixtureExecutor,
    pub before: B,
    pub after: A,
}

impl<B, A> Executor for HookExec<B, A>
where
    B: Fn(&EffectRequest) + Send + Sync,
    A: Fn(&EffectRequest) + Send + Sync,
{
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        (self.before)(req);
        let out = self.inner.run(req, ctx).await;
        (self.after)(req);
        out
    }
}

/// `fixtures/transcripts/<name>.json`.
pub fn transcript(name: &str) -> PathBuf {
    fixtures().join("transcripts").join(format!("{name}.json"))
}

/// A routing executor over `root`: model calls through `provider`, reads from the shadow
/// workspace of `root`'s snapshot and database, everything else to `jobs`.
pub fn routing_over<J: Executor>(
    root: &Path,
    jobs: J,
    provider: Option<Box<dyn ModelProvider>>,
    counts: &ExecCounts,
    hook: Option<CrashHook>,
) -> RoutingExecutor<J> {
    let model =
        ModelExecutor::new(root.join("model"), provider, counts.clone()).with_crash(hook.clone());
    let reads = ShadowReader::new(
        root.join("agentos.db"),
        root.join("snapshot"),
        root.join("shadow"),
    )
    .with_crash(hook);
    RoutingExecutor::new(jobs, model, reads)
}

/// A model-driven task over `model_env(model_requests, tool_actions)`, with the routing
/// executor answering model calls from `provider` (the returned handle shares its counter).
pub fn flow_with(
    provider: FakeProvider,
    model_requests: u32,
    tool_actions: u32,
) -> (Env, RoutingExecutor<FixtureExecutor>, FakeProvider) {
    let env = model_env(model_requests, tool_actions);
    let counts = ExecCounts::default();
    let exec = routing_over(
        env.dir.path(),
        env.fixture_exec(),
        Some(Box::new(provider.clone_handle())),
        &counts,
        None,
    );
    (env, exec, provider)
}

/// [`flow_with`] a provider answering from `fixtures/transcripts/<transcript>.json`.
pub fn flow(
    transcript_name: &str,
    model_requests: u32,
    tool_actions: u32,
) -> (Env, RoutingExecutor<FixtureExecutor>, FakeProvider) {
    flow_with(
        FakeProvider::from_file(&transcript(transcript_name)).unwrap(),
        model_requests,
        tool_actions,
    )
}

/// A fresh `ModelAgent` for `env`'s contract.
pub fn model_agent(env: &Env) -> ModelAgent {
    ModelAgent::new(env.contract.clone(), "claude-opus-5-5")
}

/// Runs the task with a fresh `ModelAgent`.
pub async fn run_model<E: Executor>(env: &Env, exec: &E) -> TaskState {
    let mut agent = model_agent(env);
    agentos_engine::runner::run_task(&env.db, &env.blobs, exec, &mut agent, &env.task)
        .await
        .unwrap()
}
