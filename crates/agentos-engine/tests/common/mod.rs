#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{EffectId, EffectRecord};
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::agent::{Agent, AgentAction, Observation};
use agentos_engine::crash::CrashHook;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome, Executor};
use agentos_core::guest::mint_attempt_token;
use agentos_engine::firecracker::FirecrackerConfig;
use agentos_engine::fixture::FixtureExecutor;
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::jail::{JailConfig, JailMode};
use agentos_engine::job::{HostConfig, WorkerConfig};
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
        launcher: GuestLauncher::Fake { program: SUPERVISOR_BIN.into(), prefix_args: Vec::new() },
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
printf '%s\n' "$all" > "$base/argv.txt"
mkdir -p "$CGROUP_ROOT/$parent/$id"
root=$(dirname "$(find "$WORK_ROOT" -samefile "$chroot/ws.img" -print -quit)")   # the hard link finds the task
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

/// A supervised executor running `worker` jobs under `jobs_root` with the real supervisor
/// binary, counting launches in `counts`, consulting `crash` right after each launch, and
/// with `env` set in the supervisor's (and worker's) environment.
pub fn supervised(
    jobs_root: &Path,
    worker: WorkerConfig,
    counts: &ExecCounts,
    crash: Option<CrashHook>,
    env: &[(&str, &str)],
) -> SupervisedExecutor {
    let cmd = SupervisorCmd { program: SUPERVISOR_BIN.into(), prefix_args: Vec::new() };
    let mut exec = SupervisedExecutor::new(jobs_root.to_path_buf(), cmd, worker, counts.clone()).unwrap().with_crash(crash);
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

pub const ALL_CAPS: &[&str] = &["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"];

pub fn contract(tool_actions: u32) -> (Contract, Digest) {
    contract_with(tool_actions, ALL_CAPS)
}

pub fn contract_with(tool_actions: u32, caps: &[&str]) -> (Contract, Digest) {
    contract_full(tool_actions, caps, 600)
}

pub fn contract_full(tool_actions: u32, caps: &[&str], deadline_seconds: u32) -> (Contract, Digest) {
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
            "model_requests": 1,
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

    /// A task the owner has not approved yet: no handles, no deadline.
    pub fn unapproved(tool_actions: u32, caps: &[&str]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("parser-repo"), &dir.path().join("snapshot"));
        copy_dir(&fixtures().join("profiles/parser-checks-v1"), &dir.path().join("profile"));
        let db = Db::open(&dir.path().join("agentos.db")).unwrap();
        let blobs = BlobStore::open(dir.path().join("blobs")).unwrap();
        let exec = FixtureExecutor::new(
            dir.path().join("snapshot"),
            dir.path().join("profile"),
            dir.path().join("work"),
        );
        let (contract, digest) = contract_with(tool_actions, caps);
        let task = db.create_task(&contract, &digest).unwrap();
        Env { dir, db, blobs, exec, task, contract }
    }

    /// A second connection to the same database, as a concurrent writer would hold.
    pub fn second_db(&self) -> Db {
        Db::open(&self.dir.path().join("agentos.db")).unwrap()
    }

    /// Another executor over the same snapshot, profile and work root.
    pub fn fixture_exec(&self) -> FixtureExecutor {
        FixtureExecutor::new(self.snapshot_dir(), self.profile_dir(), self.dir.path().join("work"))
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
        self.events().iter().filter(|e| e.event_type == event_type).count()
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
