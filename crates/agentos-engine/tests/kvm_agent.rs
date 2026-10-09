//! The agent session on the real microVM (KVM tier, gated exactly like `kvm_tier.rs`: each
//! test begins with `let Some(kvm) = kvm::require() else { return };`, skips loudly without
//! `AGENTOS_KVM_TESTS`, and panics when it is requested but unavailable).
//!
//! The guest image is the agent image (`AGENTOS_GUEST_IMAGE`, `agent-cli-py314-v1`): the
//! Claude Code native binary at `/opt/agent-cli/claude`, started by the session through the
//! `exec-check` trampoline as uid 1001.
//!
//! 1. `no_nic_no_key_and_lo_down_after_the_session`: a scripted sh session (not the CLI) proves
//!    inside the VM that nothing but the proxy is reachable, no secret is in its environment,
//!    and `lo` is down again once the session ended (a second verification after the session
//!    still fails every connection).
//! 2. `the_real_cli_fixes_the_fixture_and_the_task_succeeds`: the real CLI through the runner,
//!    with the fake provider scripted to a `Bash` `sed -i` fix and then `end_turn`.
//! 3. `cancel_and_lease_end_the_real_cli_session_and_leave_nothing`: the real CLI blocked in a
//!    model call is cancelled (cancel marker) or outlives its lease; no process survives in
//!    the VM or on the host, and the CLI's own grandchildren are measured while they run.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::contract::Contract;
use agentos_core::effect::{AgentSessionSpec, AttemptId, EffectId, EffectKind, Outcome};
use agentos_core::guest::unb64;
use agentos_core::ids::{Digest, TaskId};
use agentos_core::state::TaskState;
use agentos_engine::agent::SessionAgent;
use agentos_engine::crash::RunOptions;
use agentos_engine::executor::{AttemptCtx, EffectRequest, ExecOutcome};
use agentos_engine::firecracker::{FirecrackerConfig, FirecrackerWorker};
use agentos_engine::job::{JobDir, JobRequest, Mailbox, WorkerConfig};
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::ProviderResult;
use agentos_engine::runner::run_task_with;
use agentos_engine::supervised::ExecCounts;
use agentos_engine::worker::Worker;
use agentos_engine::workspace::workspace_digest;
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use common::{
    HomeGuard, TEST_WORKERS_ENV, copy_dir, fixtures, home_firecrackers, kvm, processes_naming,
    routing_over, supervised,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// The guest's memory for an agent session: the native CLI is a ~250 MB binary whose pages
/// count against the VM, and its own heap is larger than the checks'. Measured in the
/// evidence; the contract's `worker_memory_mib` says the same.
const MEMORY_MIB: u32 = 1024;
const IMAGE_ID: &str = "agent-cli-py314-v1";
const CLI: &str = "/opt/agent-cli/claude";
const CAPS: &[&str] = &[
    "snapshot.read",
    "workspace.apply_patch",
    "verification.run",
    "model.request",
    "agent.session",
];
/// The session's report: a new file under `src/`, editable, so it comes back in the patch.
const REPORT: &str = "src/session-report.txt";

/// The VM tests run one at a time: each boots a 1 GiB guest, and the process checks measure
/// the host.
static ONE_VM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn contract_json(model_requests: u32, deadline_seconds: u32) -> String {
    let caps = serde_json::to_string(CAPS).unwrap();
    format!(
        r#"{{
        "goal": "fix the parser",
        "repository": {{"source": "fixtures/parser-repo", "revision": "rev-1"}},
        "profile": "{IMAGE_ID}",
        "editable_paths": ["src/**"],
        "verification_profile": "parser-checks-v1",
        "capabilities": {caps},
        "limits": {{
            "model_requests": {model_requests},
            "max_output_tokens_per_request": 1000,
            "tool_actions": 10,
            "deadline_seconds": {deadline_seconds},
            "worker_vcpus": 1,
            "worker_memory_mib": {MEMORY_MIB}
        }}
    }}"#
    )
}

/// One home on the guest image's filesystem: the fixture's snapshot and profile, and a
/// jailed worker config for the agent image with `MEMORY_MIB`.
struct Home {
    /// First: dropped (VMs killed, jails collected) before `dir` is removed.
    _guard: HomeGuard,
    dir: TempDir,
    cfg: FirecrackerConfig,
    json: String,
    contract: Contract,
    task: TaskId,
    counts: ExecCounts,
    step: AtomicU32,
}

impl Home {
    fn new(kvm: &kvm::Kvm, model_requests: u32, deadline_seconds: u32) -> Home {
        let dir = kvm.root();
        copy_dir(
            &fixtures().join("parser-repo"),
            &dir.path().join("snapshot"),
        );
        copy_dir(
            &fixtures().join("profiles/parser-checks-v1"),
            &dir.path().join("profile"),
        );
        let mut cfg = kvm.jailed_config(dir.path());
        cfg.memory_mib = MEMORY_MIB;
        let json = contract_json(model_requests, deadline_seconds);
        let contract = Contract::parse(&json).unwrap();
        Home {
            _guard: HomeGuard::new(dir.path(), &kvm.cgroup_root),
            dir,
            cfg,
            json,
            contract,
            task: TaskId::new(),
            counts: ExecCounts::default(),
            step: AtomicU32::new(0),
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    /// The snapshot's digest: the workspace before any change (the VM's digest must equal it).
    fn base(&self) -> Digest {
        workspace_digest(&self.path("snapshot")).unwrap()
    }

    fn request(&self, kind: EffectKind, payload: &[u8]) -> EffectRequest {
        let step = self.step.fetch_add(1, Ordering::Relaxed);
        EffectRequest {
            effect_id: EffectId::derive(&self.task, step, &kind, &Digest::of(payload)),
            task_id: self.task.clone(),
            kind,
            payload: payload.to_vec(),
            contract: self.contract.clone(),
            deadline_ts: 0,
        }
    }

    /// A new job for `kind` (its lease `lease_in_ms` from now, or none) and its worker.
    fn job(
        &self,
        kind: EffectKind,
        payload: &[u8],
        lease_in_ms: Option<i64>,
    ) -> (EffectRequest, AttemptCtx, JobDir) {
        let req = self.request(kind, payload);
        let ctx = AttemptCtx {
            attempt_id: AttemptId::new(),
            lease_generation: u64::from(self.step.load(Ordering::Relaxed)) + 1,
            worker: "kvm".into(),
        };
        let job_req = JobRequest {
            effect_id: req.effect_id.clone(),
            task_id: req.task_id.clone(),
            kind: req.kind.clone(),
            payload: req.payload.clone(),
            contract: req.contract.clone(),
            attempt_id: ctx.attempt_id.clone(),
            lease_generation: ctx.lease_generation,
            lease_expiry_ms: lease_in_ms.map_or(i64::MAX, |l| now_ms() + l),
            task_deadline_ms: 0,
            worker: WorkerConfig::Firecracker(self.cfg.clone()),
        };
        let (job, _lock) = JobDir::create(&self.path("jobs"), &job_req).unwrap();
        (req, ctx, job)
    }

    fn worker(&self, job: &JobDir) -> FirecrackerWorker {
        FirecrackerWorker::new(&self.cfg, job).with_env(vec![(TEST_WORKERS_ENV.into(), "1".into())])
    }

    /// Runs `kind` to its outcome in a new job, with no model answers.
    async fn run(&self, kind: EffectKind, payload: &[u8]) -> ExecOutcome {
        let (req, ctx, job) = self.job(kind, payload, None);
        self.worker(&job).run(&req, &ctx).await
    }

    /// `ReadSnapshot`, which must succeed: the workspace is in the VM.
    async fn snapshot(&self) {
        let out = self.run(EffectKind::ReadSnapshot, b"").await;
        assert_eq!(out.receipt.outcome, Outcome::Success, "{}", text(&out));
    }

    /// A check of `script` (the profile is `python3 -c script`, protected), as the VM's
    /// `RunVerification` runs it against the workspace.
    fn use_script(&self, script: &str) {
        let profile =
            json!({ "id": "kvm", "command": ["python3", "-c", script], "protected": true });
        fs::write(self.path("profile/profile.json"), profile.to_string()).unwrap();
    }

    /// The session's `RunAgentSession` payload: the argv and the environment the controller
    /// names (`session_env`); nothing of the host's.
    fn session_payload(argv: &[&str]) -> Vec<u8> {
        AgentSessionSpec {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: session_env(),
        }
        .to_payload()
    }

    /// Whether no Firecracker and no process naming this home is left.
    fn gone(&self) -> bool {
        home_firecrackers(self.root()).is_empty() && processes_naming(self.root()).is_empty()
    }
}

/// The environment the controller gives a session beyond the guest's own (`PATH`, `HOME`,
/// `TMPDIR`, the proxy and the placeholder key are the guest's): none. The real-CLI test relies
/// on the guest's `PATH`, without which the CLI's shell cannot find `sed` (`exit 127`).
fn session_env() -> Vec<(String, String)> {
    Vec::new()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn text(out: &ExecOutcome) -> String {
    String::from_utf8_lossy(&out.output).into_owned()
}

/// The outcome of a session that ended (`AgentDone` and its patch), as the worker reports it.
fn session_end(out: &ExecOutcome) -> Value {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", text(out));
    serde_json::from_slice(&out.output).unwrap()
}

fn patch_of(end: &Value) -> String {
    let bytes = unb64(end["patch_b64"].as_str().unwrap()).unwrap();
    String::from_utf8(bytes).unwrap()
}

/// The added lines of the one new file `path` in `patch`, as `key=value` pairs.
fn report_of(patch: &str, path: &str) -> Vec<(String, String)> {
    let header = format!("+++ b/{path}");
    assert!(patch.contains(&header), "no new file {path} in:\n{patch}");
    let mut in_file = false;
    let mut pairs = Vec::new();
    for line in patch.lines() {
        if line.starts_with("diff --git") {
            in_file = false;
        }
        if line == header {
            in_file = true;
            continue;
        }
        if in_file
            && let Some(added) = line.strip_prefix('+')
            && let Some((k, v)) = added.split_once('=')
        {
            pairs.push((k.to_string(), v.to_string()));
        }
    }
    pairs
}

fn value(pairs: &[(String, String)], key: &str) -> String {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .unwrap_or_else(|| panic!("no {key}= in the report: {pairs:?}"))
        .1
        .clone()
}

/// The guest's sh session. It records what it can observe about its own world, one
/// `key=value` per line, in `REPORT`: uid, environment names and whether a value looks like a
/// secret, the loopback flags and listening sockets, and what each connection attempt gives.
/// Only the proxy's port may be connected to; the interface list must be `lo`.
const PROBE: &str = r#"set -u
{
echo "env_names=$(env | cut -d= -f1 | sort | tr '\n' ',' | sed 's/,$//')"
echo "path=$PATH"
python3 - <<'PY'
import errno, os, re, socket
def attempt(addr):
    try:
        socket.create_connection(addr, timeout=2).close()
        return "connected"
    except OSError as e:
        return errno.errorcode.get(e.errno, repr(e))
base = os.environ.get("ANTHROPIC_BASE_URL", "")
port = int(base.rsplit(":", 1)[1]) if base else -1
env = dict(os.environ)
print("uid=" + str(os.getuid()))
print("api_key=" + env.get("ANTHROPIC_API_KEY", "<unset>"))
print("secret_like=" + str(sum(1 for k, v in env.items() if k != "ANTHROPIC_API_KEY" and re.fullmatch(r"[A-Za-z0-9_+/=-]{24,}", v))))
print("base_url_port=" + str(port))
print("lo_flags=" + open("/sys/class/net/lo/flags").read().strip())
print("ifaces=" + ",".join(sorted(os.listdir("/sys/class/net"))))
listen = []
for f in ("/proc/net/tcp", "/proc/net/tcp6"):
    for line in open(f).read().splitlines()[1:]:
        cols = line.split()
        if cols[3] == "0A":
            listen.append(cols[1])
print("listeners=" + ",".join(listen))
print("net_10_0_0_1=" + attempt(("10.0.0.1", 80)))
print("public_1_1_1_1=" + attempt(("1.1.1.1", 443)))
print("loopback_proxy=" + attempt(("127.0.0.1", port)))
print("loopback_other=" + attempt(("127.0.0.1", 9)))
PY
} > src/session-report.txt
"#;

/// The check run after the session: the loopback interface must be down, and nothing can be
/// connected to, not even the proxy's port the session used.
fn after_probe(port: i64) -> String {
    format!(
        r#"import errno, json, socket, sys
def attempt(addr):
    try:
        socket.create_connection(addr, timeout=2).close()
        return "connected"
    except OSError as e:
        return errno.errorcode.get(e.errno, repr(e))
flags = int(open("/sys/class/net/lo/flags").read().strip(), 16)
f = {{"lo_up": bool(flags & 1), "loopback_proxy": attempt(("127.0.0.1", {port})), "net": attempt(("10.0.0.1", 80)), "public": attempt(("1.1.1.1", 443))}}
print(json.dumps(f, sort_keys=True))
held = (not f["lo_up"]) and f["loopback_proxy"] == "ENETUNREACH" and f["net"] == "ENETUNREACH" and f["public"] == "ENETUNREACH"
sys.exit(0 if held else 1)
"#
    )
}

/// A `RunVerification` outcome: the evidence's findings line (`{...}`) and whether it passed.
fn verification(out: &ExecOutcome) -> (Value, Value) {
    assert_eq!(out.receipt.outcome, Outcome::Success, "{}", text(out));
    let evidence: Value = serde_json::from_slice(&out.output).unwrap();
    let stdout = evidence["stdout"].as_str().unwrap_or_default();
    let line = stdout
        .lines()
        .find(|l| l.starts_with('{'))
        .unwrap_or_else(|| panic!("no findings line in {stdout:?}"));
    (
        evidence["passed"].clone(),
        serde_json::from_str(line).unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn no_nic_no_key_and_lo_down_after_the_session() {
    let Some(kvm) = kvm::require() else { return };
    let _one = ONE_VM.lock().await;
    let home = Home::new(&kvm, 4, 600);
    home.snapshot().await;

    let argv = ["/bin/sh", "-c", PROBE];
    let (req, ctx, job) = home.job(
        EffectKind::RunAgentSession {
            expected_base: home.base(),
        },
        &Home::session_payload(&argv),
        None,
    );
    let out = home.worker(&job).run(&req, &ctx).await;
    let end = session_end(&out);
    assert_eq!(end["exit_code"], 0, "{end}");
    let report = report_of(&patch_of(&end), REPORT);

    assert_eq!(
        value(&report, "uid"),
        "1001",
        "the CLI runs as the agent uid"
    );
    assert_eq!(value(&report, "api_key"), "placeholder", "{report:?}");
    assert_eq!(value(&report, "secret_like"), "0", "{report:?}");
    assert_eq!(value(&report, "path"), "/usr/bin:/bin", "{report:?}");
    let allowed = [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "DISABLE_AUTOUPDATER",
        "DISABLE_TELEMETRY",
        "HOME",
        "PATH",
        "PWD",
        "PYTHONDONTWRITEBYTECODE",
        "TMPDIR",
    ];
    for name in value(&report, "env_names").split(',') {
        assert!(
            allowed.contains(&name),
            "unexpected variable {name}: {report:?}"
        );
    }
    let flags =
        u32::from_str_radix(value(&report, "lo_flags").trim_start_matches("0x"), 16).unwrap();
    assert_eq!(flags & 1, 1, "lo is up while the session runs: {report:?}");
    assert_eq!(value(&report, "ifaces"), "lo", "{report:?}");

    let port: i64 = value(&report, "base_url_port").parse().unwrap();
    assert_eq!(
        value(&report, "listeners"),
        format!("0100007F:{port:04X}"),
        "the proxy is the only listener"
    );
    assert_eq!(value(&report, "net_10_0_0_1"), "ENETUNREACH", "{report:?}");
    assert_eq!(
        value(&report, "public_1_1_1_1"),
        "ENETUNREACH",
        "{report:?}"
    );
    assert_eq!(value(&report, "loopback_proxy"), "connected", "{report:?}");
    assert_eq!(
        value(&report, "loopback_other"),
        "ECONNREFUSED",
        "{report:?}"
    );

    // After the session: `lo` is down again, and no connection succeeds (the old port too).
    home.use_script(&after_probe(port));
    let (passed, findings) = verification(&home.run(EffectKind::RunVerification, b"").await);
    assert_eq!(passed, true, "{findings}");
    assert_eq!(findings["lo_up"], false, "{findings}");
    assert_eq!(findings["loopback_proxy"], "ENETUNREACH", "{findings}");
}

/// The bodies of the model requests a session's CLI sent (`session/<n>.req` in its jobs), with
/// the text after each `tool_result`, for a failure message.
fn tool_results(dir: &Path) -> String {
    let mut found = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "req")
                && path.to_string_lossy().contains("session")
            {
                let body =
                    String::from_utf8_lossy(&fs::read(&path).unwrap_or_default()).into_owned();
                let shown: String = match body.find("tool_result") {
                    Some(at) => body[at..].chars().take(700).collect(),
                    None => "(no tool_result)".into(),
                };
                found.push_str(&format!("{}: {shown}\n", path.display()));
            }
        }
    }
    found
}

/// The fixture's fix, as the CLI's own `sed -i` (relative path: the session's work directory
/// is the workspace copy).
const FIX_COMMAND: &str = r#"sed -i -e 's/if not line.strip() or line.startswith/line = line.strip()\n        if not line or line.startswith/' -e 's/result\[key\] = value/result[key.strip()] = value.strip()/' src/parser.py"#;

fn message(content: Value, stop_reason: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "msg_kvm",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1},
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_real_cli_fixes_the_fixture_and_the_task_succeeds() {
    let Some(kvm) = kvm::require() else { return };
    let _one = ONE_VM.lock().await;
    let home = Home::new(&kvm, 4, 600);
    let provider = FakeProvider::scripted(vec![
        ProviderResult::Response(
            message(
                json!([{"type": "tool_use", "id": "toolu_fix", "name": "Bash",
                        "input": {"command": FIX_COMMAND, "description": "apply the parser fix"}}]),
                "tool_use",
            ),
            Default::default(),
        ),
        ProviderResult::Response(
            message(
                json!([{"type": "text", "text": "The parser is fixed."}]),
                "end_turn",
            ),
            Default::default(),
        ),
    ]);

    let db = Db::open(&home.path("agentos.db")).unwrap();
    let blobs = BlobStore::open(home.path("blobs")).unwrap();
    let task = db
        .create_task(&home.contract, &Digest::of(home.json.as_bytes()))
        .unwrap();
    db.append_audit(
        &task,
        "Submitted",
        &json!({"model_policy_version": 1, "model_limits_version": 1, "model": "fake:test"}),
    )
    .unwrap();
    db.approve_task(&task).unwrap();

    let jobs = supervised(
        &home.path("jobs"),
        WorkerConfig::Firecracker(home.cfg.clone()),
        &home.counts,
        None,
        &[],
    );
    let exec = routing_over(
        home.root(),
        jobs,
        Some(Box::new(provider.clone_handle())),
        &home.counts,
        None,
    );
    let argv: Vec<String> = [
        CLI,
        "-p",
        "fix the parser",
        "--permission-mode",
        "bypassPermissions",
        "--disallowedTools",
        "WebFetch",
        "WebSearch",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut agent = SessionAgent::new(argv, session_env(), "claude-opus-5-5");
    let state = run_task_with(
        &db,
        &blobs,
        &exec,
        &mut agent,
        &task,
        &RunOptions::default(),
    )
    .await
    .unwrap();
    let events: Vec<String> = db
        .events(&task)
        .unwrap()
        .iter()
        .map(|e| e.payload.to_string())
        .collect();
    assert_eq!(
        state,
        TaskState::Succeeded,
        "{events:?}\n{}",
        tool_results(home.root())
    );
    assert_eq!(provider.calls(), 2, "one tool turn and one end turn");
    assert!(
        events.iter().any(|p| p.contains("RunVerification")),
        "{events:?}"
    );
}

/// The responder for a session's mailbox, as the controller would be: request 1 is answered
/// with `first`; request 2 is recorded (the tool result of the first answer is in its body)
/// and never answered, so the CLI stays inside a model call.
struct Responder {
    stop: Arc<AtomicBool>,
    held: Arc<Mutex<Option<Vec<u8>>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Responder {
    fn start(session_dir: PathBuf, first: Vec<u8>) -> Responder {
        let stop = Arc::new(AtomicBool::new(false));
        let held = Arc::new(Mutex::new(None));
        let (stop2, held2) = (stop.clone(), held.clone());
        let handle = thread::spawn(move || {
            let mailbox = Mailbox::new(session_dir);
            let mut after = 0;
            while !stop2.load(Ordering::Relaxed) {
                match mailbox.controller_next_request(after) {
                    Ok(Some((id, body))) => {
                        after = id;
                        if id == 1 {
                            mailbox.controller_post_reply(1, 200, &first).unwrap();
                        } else {
                            *held2.lock().unwrap() = Some(body);
                        }
                    }
                    _ => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Responder {
            stop,
            held,
            handle: Some(handle),
        }
    }

    /// The body of the held (second) model request, once it has arrived.
    async fn wait_held(held: &Mutex<Option<Vec<u8>>>, within: Duration) -> Vec<u8> {
        let until = Instant::now() + within;
        loop {
            if let Some(body) = held.lock().unwrap().clone() {
                return body;
            }
            assert!(
                Instant::now() < until,
                "the CLI never made its second model request"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The CLI's first turn: processes it leaves running (one in the CLI's process group, one in
/// its own session), then a listing of every process, which the second request carries back.
const GRANDCHILDREN: &str = r#"sleep 600 >/dev/null 2>&1 & setsid sleep 601 >/dev/null 2>&1 & sleep 1; for p in /proc/[0-9]*; do echo "${p#/proc/} $(tr '\0' ' ' < $p/cmdline 2>/dev/null)"; done; python3 -I -c '
import os
def rd(p):
    try:
        with open(p) as f:
            return f.read()
    except OSError:
        return ""
mem = {l.split(":")[0]: l.split()[1] for l in rd("/proc/meminfo").splitlines() if ":" in l}
print("mem_total_kib", mem.get("MemTotal"), "mem_available_kib", mem.get("MemAvailable"))
for path in ("/scratch", "/tmp"):
    st = os.statvfs(path)
    print("fs", path, "size_kib", st.f_blocks * st.f_frsize // 1024, "free_kib", st.f_bavail * st.f_frsize // 1024)
def tree(d):
    total = 0
    for root, dirs, files in os.walk(d):
        for f in files:
            try:
                total += os.lstat(os.path.join(root, f)).st_size
            except OSError:
                pass
    return total // 1024
for d in ("/scratch/agent/home", "/scratch/agent/tmp", "/scratch/agent/work"):
    print("du_kib", d, tree(d))
threads = 0
for pid in os.listdir("/proc"):
    if not pid.isdigit():
        continue
    fields = dict(l.split(":\t", 1) for l in rd("/proc/" + pid + "/status").splitlines() if ":\t" in l)
    if not fields:
        continue
    if fields.get("Uid", "").split()[:1] == ["1001"]:
        threads += int(fields["Threads"])
    if fields.get("Name", "").strip() == "claude":
        print("claude", pid, "VmHWM", fields.get("VmHWM"), "VmRSS", fields.get("VmRSS"), "Threads", fields.get("Threads"))
        print("claude_limits", " | ".join(l for l in rd("/proc/" + pid + "/limits").splitlines() if "processes" in l or "open files" in l))
print("threads_uid_1001", threads)
'"#;

/// The text of the last `tool_result` in a model request body (the output of the CLI's Bash).
fn tool_result_text(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    let mut texts = Vec::new();
    for message in v["messages"].as_array().into_iter().flatten().rev() {
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_result" {
                match &block["content"] {
                    Value::String(s) => texts.push(s.clone()),
                    Value::Array(parts) => texts.extend(
                        parts
                            .iter()
                            .filter_map(|p| p["text"].as_str().map(str::to_string)),
                    ),
                    _ => {}
                }
            }
        }
        if !texts.is_empty() {
            break;
        }
    }
    texts.join("\n")
}

/// How a running session is ended.
#[derive(Clone, Copy)]
enum End {
    /// The controller cancels the job (the cancel marker).
    Cancel,
    /// The job's lease runs out.
    Lease,
}

/// Runs the real CLI in a session, blocked in its second model call, and ends it as `how`.
/// Returns the session's outcome, the time it took after the end, and whether nothing was left.
async fn end_a_running_session(home: &Home, how: End) -> (String, Duration, bool, Vec<u8>) {
    let lease_ms = match how {
        End::Cancel => None,
        End::Lease => Some(60_000),
    };
    let argv = [
        CLI,
        "-p",
        "list the processes",
        "--permission-mode",
        "bypassPermissions",
        "--disallowedTools",
        "WebFetch",
        "WebSearch",
    ];
    let (req, ctx, job) = home.job(
        EffectKind::RunAgentSession {
            expected_base: home.base(),
        },
        &Home::session_payload(&argv),
        lease_ms,
    );
    let first = message(
        json!([{"type": "tool_use", "id": "toolu_ps", "name": "Bash",
                "input": {"command": GRANDCHILDREN, "description": "list the processes"}}]),
        "tool_use",
    );
    let responder = Responder::start(job.session_dir(), first);
    let held = responder.held.clone();
    let job_dir = job.path.clone();
    let canceller = tokio::spawn(async move {
        let body = Responder::wait_held(&held, Duration::from_secs(240)).await;
        if let End::Cancel = how {
            JobDir { path: job_dir }.drop_cancel().unwrap();
        }
        (Instant::now(), body)
    });
    let started = Instant::now();
    let out = home.worker(&job).run(&req, &ctx).await;
    let returned = Instant::now();
    let (asked_at, body) = canceller.await.unwrap();
    let gone_at_return = home.gone();
    let outcome = match &out.receipt.outcome {
        Outcome::Failure(reason) => reason.clone(),
        Outcome::Success => format!("success: {}", text(&out)),
    };
    let since = match how {
        End::Cancel => returned.saturating_duration_since(asked_at),
        End::Lease => returned.saturating_duration_since(started),
    };
    drop(responder);
    (outcome, since, gone_at_return, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_and_lease_end_the_real_cli_session_and_leave_nothing() {
    let Some(kvm) = kvm::require() else { return };
    let _one = ONE_VM.lock().await;

    let home = Home::new(&kvm, 4, 600);
    home.snapshot().await;
    let (outcome, took, gone, body) = end_a_running_session(&home, End::Cancel).await;
    let listing = tool_result_text(&body);
    println!(
        "cancel: outcome {outcome:?}, returned {took:?} after the cancel, gone at return {gone}"
    );
    println!("in the VM before the cancel:\n{listing}");
    assert!(
        listing.contains("sleep 600") && listing.contains("sleep 601"),
        "the CLI's grandchildren were alive in the VM at the second request"
    );
    assert!(outcome.contains("cancelled"), "{outcome}");
    assert!(gone, "a process or a VM of the session outlived its cancel");
    drop(home);

    let home = Home::new(&kvm, 4, 600);
    home.snapshot().await;
    let (outcome, took, gone, _) = end_a_running_session(&home, End::Lease).await;
    println!("lease: outcome {outcome:?}, returned after {took:?}, gone at return {gone}");
    assert!(outcome.contains("lease"), "{outcome}");
    assert!(gone, "a process or a VM of the session outlived its lease");
}
