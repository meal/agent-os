//! The Firecracker worker: one microVM per job, booted fresh from a pinned guest image,
//! driven over the guest control protocol, destroyed when the job ends. Every outcome byte
//! is built here, on the host, from the guest's structured reply (`outcomes`), so the
//! artifacts equal the host worker's.
//!
//! Also: the worker configuration, the `vm.json` renderer, exit-code naming, the guest image
//! manifest and the preflight the controller and the worker both run.

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agentos_core::effect::{AttemptId, EffectKind};
use agentos_core::guest::{
    FILE_LIMIT, GUEST_CID, GUEST_MIN_MEMORY_MIB, GUEST_PROTOCOL, MAX_VCPUS, Message, Mode,
    OUTPUT_LIMIT, PATCH_LIMIT, PROFILE_LIMIT, SNAPSHOT_BYTES_LIMIT, SNAPSHOT_FILES_LIMIT,
    mint_attempt_token, unb64,
};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::resources::VmResources;
use agentos_core::workspace::{list_files, workspace_digest};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};
use serde::{Deserialize, Serialize};

use crate::executor::{AttemptCtx, EffectRequest, ExecOutcome, Reconciliation};
use crate::guestlink::{GuestLauncher, GuestLink, LinkError, fake_command, guest_text, type_name};
use crate::jail::{self, CgroupOverrides, JailMode, StageSources};
use crate::job::JobDir;
use crate::outcomes::{self, Check};
use crate::worker::{TEST_WORKERS_ENV, Worker};

pub use agentos_guest::handlers::PatchStateIs;

/// From spawn to `Ready`.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(15);
/// The whole of one inspection boot.
pub const INSPECT_TIMEOUT: Duration = Duration::from_secs(60);
/// From `Shutdown` (or a lost connection) to Firecracker's exit, before it is SIGKILLed.
pub const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
/// The guest kernel command line (defined next to the protocol, where the guest sees it).
pub use agentos_core::guest::BOOT_ARGS;
/// Test hook (with `AGENTOS_TEST_WORKERS=1`): SIGKILL the VM right after `ApplyPatch` was sent.
pub const KILL_VM_AFTER_REQUEST_ENV: &str = "AGENTOS_TEST_KILL_VM_AFTER_REQUEST";
/// What `firecracker --version` must start with.
pub const FIRECRACKER_VERSION_PREFIX: &str = "Firecracker v1.17.";
/// The file names of a registered image (fixed, as the jail stages them under these names).
pub const KERNEL_FILE: &str = "vmlinux";
pub const ROOTFS_FILE: &str = "rootfs.squashfs";

/// How long a `ReadSnapshot`/`ApplyPatch` reply may take once the request was sent. The
/// job's lease (the supervisor's kill) normally ends a stuck guest first.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);
/// A `RunVerification` reply may take the check's own timeout plus this.
const VERIFY_REPLY_MARGIN: Duration = Duration::from_secs(60);
/// Bounds each single write to the guest (a peer that stops reading).
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);
const EXIT_POLL: Duration = Duration::from_millis(10);
/// How long `firecracker --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const WS_BUSY: &str = "workspace image is attached to another VM";
const WORKSPACE_MISSING: &str = "workspace missing: no snapshot was read";

/// The Firecracker worker's part of `request.json`. Every path is absolute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirecrackerConfig {
    pub firecracker_bin: PathBuf,
    /// `<registry>/images/<id>@<digest>/`.
    pub image_dir: PathBuf,
    /// Pinned; the image is re-digested before every launch.
    pub image_digest: Digest,
    pub snapshot_dir: PathBuf,
    pub profile_dir: PathBuf,
    pub profile_digest: Option<Digest>,
    /// `ws.img` lives at `<work_root>/<task>/ws.img`.
    pub work_root: PathBuf,
    pub verify_timeout_secs: u64,
    /// The contract's `worker_vcpus`, 1..=32.
    pub vcpus: u32,
    /// The contract's `worker_memory_mib`, at least `GUEST_MIN_MEMORY_MIB`.
    pub memory_mib: u32,
    /// 32 lowercase hex, minted per job; identity (sent only in `Hello`), not authority.
    pub attempt_token: String,
    pub launcher: GuestLauncher,
    /// Decided by the controller; the worker only executes it.
    pub jail: JailMode,
    /// The task's drive sizes and rate limits, as recorded at submission. A `request.json`
    /// written before they were recorded has none: version 0.
    #[serde(default = "version_zero")]
    pub resources: VmResources,
}

impl FirecrackerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_VCPUS).contains(&self.vcpus) {
            return Err(format!("worker_vcpus must be between 1 and {MAX_VCPUS}"));
        }
        if self.memory_mib < GUEST_MIN_MEMORY_MIB {
            return Err(format!(
                "worker_memory_mib must be at least {GUEST_MIN_MEMORY_MIB}"
            ));
        }
        Ok(())
    }
}

/// The six strings that go into `vm.json`: host paths (unjailed) or chroot paths (jailed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmView {
    pub kernel: String,
    pub rootfs: String,
    pub ws_img: String,
    pub scratch_img: String,
    pub uds: String,
    pub log: String,
}

/// Where a job's VM files live on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmPaths {
    pub dir: PathBuf,
    pub ws_img: PathBuf,
    pub scratch_img: PathBuf,
    pub vm_json: PathBuf,
    pub uds: PathBuf,
    pub console_log: PathBuf,
    pub firecracker_log: PathBuf,
    pub stderr_log: PathBuf,
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl VmPaths {
    /// The files of a VM run from `dir` (a job directory) on `task`'s workspace image.
    pub fn new(dir: &Path, work_root: &Path, task: &TaskId) -> VmPaths {
        VmPaths {
            dir: dir.to_path_buf(),
            ws_img: work_root.join(task.as_str()).join("ws.img"),
            scratch_img: dir.join("scratch.img"),
            vm_json: dir.join("vm.json"),
            uds: dir.join("v.sock"),
            console_log: dir.join("console.log"),
            firecracker_log: dir.join("firecracker.log"),
            stderr_log: dir.join("stderr.log"),
        }
    }

    /// The unjailed view: absolute host paths, except `uds`, which is `v.sock` relative to
    /// Firecracker's working directory (the job directory): `<job>/v.sock` is longer than a
    /// Unix socket address can be (`sun_path`, 108 bytes) since a job directory is named
    /// `<64-hex effect>-<uuid>`.
    pub fn host_view(&self, image_dir: &Path) -> VmView {
        VmView {
            kernel: text(&image_dir.join(KERNEL_FILE)),
            rootfs: text(&image_dir.join(ROOTFS_FILE)),
            ws_img: text(&self.ws_img),
            scratch_img: text(&self.scratch_img),
            uds: self
                .uds
                .file_name()
                .map_or_else(|| text(&self.uds), |n| text(Path::new(n))),
            log: text(&self.firecracker_log),
        }
    }
}

// `vm.json` as ordered structs, so the file keeps the documented field order (a
// `serde_json::Value` sorts its keys).
#[derive(Serialize)]
struct VmConfig<'a> {
    #[serde(rename = "boot-source")]
    boot_source: BootSource<'a>,
    drives: [Drive<'a>; 3],
    #[serde(rename = "machine-config")]
    machine_config: MachineConfig,
    vsock: Vsock<'a>,
    #[serde(rename = "network-interfaces")]
    network_interfaces: [(); 0],
    logger: Logger<'a>,
}

#[derive(Serialize)]
struct BootSource<'a> {
    kernel_image_path: &'a str,
    boot_args: &'a str,
}

fn version_zero() -> VmResources {
    VmResources::V0
}

#[derive(Serialize)]
struct Drive<'a> {
    drive_id: &'a str,
    is_root_device: bool,
    is_read_only: bool,
    path_on_host: &'a str,
    cache_type: &'a str,
    /// Absent without a contracted limit, so such a `vm.json` is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    rate_limiter: Option<RateLimiter>,
}

/// Firecracker v1.17.0's `RateLimiter`: independent bytes/s and operations/s token buckets.
#[derive(Serialize, Clone, Copy)]
struct RateLimiter {
    #[serde(skip_serializing_if = "Option::is_none")]
    bandwidth: Option<TokenBucket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ops: Option<TokenBucket>,
}

/// `size` tokens, refilled over `refill_time` milliseconds. No `one_time_burst`: the bucket
/// starts full (one second's worth) and then refills at the contracted rate.
#[derive(Serialize, Clone, Copy)]
struct TokenBucket {
    size: u64,
    refill_time: u64,
}

/// The limiter of each writable drive, or `None` when the contract sets no rate.
fn rate_limiter(r: &VmResources) -> Option<RateLimiter> {
    let per_second = |size: u64| TokenBucket {
        size,
        refill_time: 1000,
    };
    let limiter = RateLimiter {
        bandwidth: r.bandwidth_mib_s.map(|b| per_second(u64::from(b) << 20)),
        ops: r.iops.map(|o| per_second(u64::from(o))),
    };
    (limiter.bandwidth.is_some() || limiter.ops.is_some()).then_some(limiter)
}

#[derive(Serialize)]
struct MachineConfig {
    vcpu_count: u32,
    mem_size_mib: u32,
    smt: bool,
    huge_pages: &'static str,
}

#[derive(Serialize)]
struct Vsock<'a> {
    guest_cid: u32,
    uds_path: &'a str,
}

#[derive(Serialize)]
struct Logger<'a> {
    log_path: &'a str,
    level: &'a str,
}

fn vm_config<'a>(cfg: &FirecrackerConfig, view: &'a VmView) -> VmConfig<'a> {
    let limiter = rate_limiter(&cfg.resources);
    VmConfig {
        boot_source: BootSource {
            kernel_image_path: &view.kernel,
            boot_args: BOOT_ARGS,
        },
        drives: [
            Drive {
                drive_id: "rootfs",
                is_root_device: true,
                is_read_only: true,
                path_on_host: &view.rootfs,
                cache_type: "Unsafe",
                rate_limiter: None,
            },
            // Writeback: a guest fsync reaches ws.img before the guest reports success.
            Drive {
                drive_id: "workspace",
                is_root_device: false,
                is_read_only: false,
                path_on_host: &view.ws_img,
                cache_type: "Writeback",
                rate_limiter: limiter,
            },
            Drive {
                drive_id: "scratch",
                is_root_device: false,
                is_read_only: false,
                path_on_host: &view.scratch_img,
                cache_type: "Unsafe",
                rate_limiter: limiter,
            },
        ],
        machine_config: MachineConfig {
            vcpu_count: cfg.vcpus,
            mem_size_mib: cfg.memory_mib,
            smt: false,
            huge_pages: "None",
        },
        vsock: Vsock {
            guest_cid: GUEST_CID,
            uds_path: &view.uds,
        },
        network_interfaces: [],
        logger: Logger {
            log_path: &view.log,
            level: "Warning",
        },
    }
}

/// The Firecracker configuration for `view`: three drives in the order that fixes the guest
/// names `vda`, `vdb`, `vdc`; no network interface; no balloon, MMDS or CPU template.
pub fn render_vm_json(cfg: &FirecrackerConfig, view: &VmView) -> serde_json::Value {
    serde_json::to_value(vm_config(cfg, view)).expect("vm.json serializes")
}

/// Writes `vm.json` with the documented field order.
pub fn write_vm_json(path: &Path, cfg: &FirecrackerConfig, view: &VmView) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(&vm_config(cfg, view)).map_err(io::Error::other)?;
    bytes.push(b'\n');
    fs::write(path, bytes)
}

/// `firecracker exit code N` or `firecracker killed by signal N`.
pub fn exit_code_text(status: &ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("firecracker exit code {code}"),
        (None, Some(signal)) => format!("firecracker killed by signal {signal}"),
        (None, None) => format!("firecracker ended with {status}"),
    }
}

/// `image.json` of a registered guest image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterpreterProvenance {
    pub version: String,
    pub source_sha256: String,
    pub pyenv_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageManifest {
    pub id: String,
    pub protocol: u32,
    pub kernel: String,
    pub rootfs: String,
    pub agent_version: String,
    pub kernel_sha256: String,
    pub built_from: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<InterpreterProvenance>,
}

/// Parses `<image_dir>/image.json` and checks that it speaks `GUEST_PROTOCOL` and names
/// `vmlinux` and `rootfs.squashfs`, both present as regular files.
pub fn read_image(image_dir: &Path) -> Result<ImageManifest, String> {
    let path = image_dir.join("image.json");
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let image: ImageManifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(interpreter) = &image.interpreter {
        let hex = |s: &str, n: usize| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if interpreter.version.is_empty()
            || interpreter.version.len() > 32
            || !interpreter
                .version
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
            || !hex(&interpreter.source_sha256, 64)
            || !hex(&interpreter.pyenv_commit, 40)
        {
            return Err("invalid interpreter provenance".into());
        }
    }
    if image.protocol != GUEST_PROTOCOL {
        return Err(format!(
            "{}: guest image speaks protocol {}, expected {GUEST_PROTOCOL}",
            path.display(),
            image.protocol
        ));
    }
    for (field, name, want) in [
        ("kernel", &image.kernel, KERNEL_FILE),
        ("rootfs", &image.rootfs, ROOTFS_FILE),
    ] {
        if name != want {
            return Err(format!(
                "{}: {field} is {name:?}, expected {want:?}",
                path.display()
            ));
        }
        let file = image_dir.join(want);
        if !file.is_file() {
            return Err(format!(
                "{}: {field} {} is missing",
                path.display(),
                file.display()
            ));
        }
    }
    Ok(image)
}

fn executable(path: &Path) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not an executable file", path.display()));
    }
    Ok(())
}

/// The first line of `firecracker --version`, which must start with `Firecracker v1.17.`.
/// Bounded by `VERSION_TIMEOUT` (the preflight runs before every launch).
pub fn firecracker_version(firecracker_bin: &Path) -> Result<String, String> {
    let mut cmd = Command::new(firecracker_bin);
    cmd.arg("--version");
    version_line(cmd, VERSION_TIMEOUT)
}

/// `first_stdout_line`, checked: a successful exit and a `Firecracker v1.17.` line.
fn version_line(cmd: Command, timeout: Duration) -> Result<String, String> {
    let (status, first) = first_stdout_line(cmd, timeout)?;
    if !status.success() || !first.starts_with(FIRECRACKER_VERSION_PREFIX) {
        return Err(format!(
            "expected {FIRECRACKER_VERSION_PREFIX}…, got {} ({status})",
            guest_text(&format!("{first:?}"))
        ));
    }
    Ok(first)
}

/// Runs `cmd` (stdin null, environment cleared, stdout captured up to 4 KiB) for at most
/// `timeout`, killing it after that, and returns its exit status and trimmed first stdout
/// line (unchecked, raw: the caller escapes it before it enters any text).
pub(crate) fn first_stdout_line(
    mut cmd: Command,
    timeout: Duration,
) -> Result<(ExitStatus, String), String> {
    use std::io::Read;
    use std::sync::mpsc;
    let mut child = cmd
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    // Detached: a descendant holding the pipe open must not hold this up.
    thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut stdout).take(4096).read_to_end(&mut kept);
        let _ = tx.send(kept);
    });
    let until = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < until => thread::sleep(EXIT_POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no answer within {} ms", timeout.as_millis()));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    let out = rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    let stdout = String::from_utf8_lossy(&out);
    let first = stdout.lines().next().unwrap_or("").trim();
    Ok((status, first.to_string()))
}

/// Whether this host can run `cfg`'s VMs: `/dev/kvm` opens read-write, the Firecracker
/// binary is a v1.17 build (`Real`), or the fake guest's program is executable (`Fake`);
/// and the image parses, speaks protocol 1 and still has the pinned digest. The caller
/// prefixes errors with `firecracker worker unavailable: `.
pub fn preflight(cfg: &FirecrackerConfig) -> Result<(), String> {
    cfg.validate()?;
    match &cfg.launcher {
        GuestLauncher::Real { .. } => {
            File::options()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .map_err(|e| format!("/dev/kvm: {e}"))?;
            executable(&cfg.firecracker_bin)?;
            firecracker_version(&cfg.firecracker_bin)
                .map_err(|e| format!("firecracker --version: {e}"))?;
        }
        GuestLauncher::Fake { program, .. } => executable(program)?,
    }
    read_image(&cfg.image_dir)?;
    let found = workspace_digest(&cfg.image_dir)
        .map_err(|e| format!("cannot digest {}: {e}", cfg.image_dir.display()))?;
    if found != cfg.image_digest {
        return Err(format!(
            "guest image digest mismatch: pinned {}, found {found}",
            cfg.image_digest
        ));
    }
    Ok(())
}

/// What the worker entry point does with a job: write this outcome, or write none and exit
/// 1 (a request was sent and its effect is unknown: the controller reconciles by inspection).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant, reason = "one value per job, moved once")]
pub enum WorkerResult {
    Outcome(ExecOutcome),
    NoOutcome(String),
}

#[derive(Debug, Clone)]
pub struct FirecrackerWorker {
    cfg: FirecrackerConfig,
    job_dir: PathBuf,
    /// `<home>/inspect`, whose dead inspections of the task are collected before a boot.
    inspect_root: Option<PathBuf>,
    boot_timeout: Duration,
    env: Vec<(String, String)>,
    /// Test seams (`with_jail_memory_max_mib`, `with_jail_cpu_quota_us`).
    cgroup_overrides: CgroupOverrides,
}

/// The request to send, prepared (and limit-checked) before anything is launched.
enum Plan {
    Snapshot {
        files: u64,
        bytes: u64,
    },
    Patch {
        expected_base: Digest,
        editable_paths: Vec<String>,
    },
    Verify {
        files: u64,
        bytes: u64,
        source: Option<Digest>,
    },
}

/// The reply that decides the outcome, held until the VM is down.
enum Reply {
    Refused(String),
    Snapshot(Vec<String>, Digest),
    Patch(Vec<String>, Digest),
    Verified(Check),
}

/// Why serving a request failed after it (or part of it) was sent.
enum Served {
    Lost,
    Violation(String),
}

/// Removes `scratch.img` however the job ends inside this process.
struct ScratchGuard(PathBuf);

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The spawned Firecracker (or jailer, or fake guest). A job's VM runs in the worker's
/// process group, so the group kill here misses (the pid leads no group); an inspector's VM
/// leads its own group, which the kill reaches whole. Dropping an unreaped VM kills it.
struct Vm {
    child: Child,
    status: Option<ExitStatus>,
    /// Leads its own process group (the inspector's): the group is swept once at the end,
    /// even when the leader exited by itself, so no descendant (the fake guest's `git`)
    /// outlives it.
    own_group: bool,
    swept: bool,
}

impl Vm {
    /// Whether it has exited, and how.
    fn exited(&mut self) -> Option<String> {
        if self.status.is_none() {
            self.status = self.child.try_wait().ok().flatten();
        }
        self.status.as_ref().map(exit_code_text)
    }

    /// SIGKILLs the process (and the group it leads, if it leads one; never the worker's own) and reaps it.
    fn kill(&mut self) -> ExitStatus {
        let pid = i32::try_from(self.child.id()).ok().and_then(Pid::from_raw);
        let status = match self.status {
            Some(status) => status,
            None => {
                if let Some(pid) = pid {
                    let _ = kill_process(pid, Signal::KILL);
                    // Only a VM that leads its own group: a job VM is a member of the
                    // worker's group, and a group id equal to its pid (after pid reuse)
                    // would be an unrelated group.
                    if self.own_group {
                        let _ = kill_process_group(pid, Signal::KILL);
                    }
                }
                let status = self
                    .child
                    .wait()
                    .unwrap_or_else(|_| ExitStatus::from_raw(9));
                self.status = Some(status);
                status
            }
        };
        self.sweep(pid);
        status
    }

    /// For a VM leading its own group: SIGKILLs what is left of the group, once. The group
    /// id stays reserved while any member lives, so this reaches only the VM's descendants.
    fn sweep(&mut self, pid: Option<Pid>) {
        if self.own_group && !self.swept {
            self.swept = true;
            if let Some(pid) = pid {
                let _ = kill_process_group(pid, Signal::KILL);
            }
        }
    }

    /// Waits up to `within` for the exit, then kills.
    fn wait_or_kill(&mut self, within: Duration) -> ExitStatus {
        let until = Instant::now() + within;
        while Instant::now() < until {
            if self.exited().is_some() {
                return self.kill();
            }
            thread::sleep(EXIT_POLL);
        }
        self.kill()
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        self.kill();
    }
}

fn not_up(e: &LinkError) -> String {
    match e {
        LinkError::BootTimeout | LinkError::Exited(_) => e.to_string(),
        // A refusal at Hello is a handshake failure, not a 3a reason: escaped and capped.
        LinkError::Refused(reason) => format!("guest did not come up: {}", guest_text(reason)),
        other => format!("guest did not come up: {other}"),
    }
}

/// `(files, bytes)` of the regular files under `root`, within the protocol limits.
fn tree_within(root: &Path, bytes_limit: u64) -> Result<(u64, u64), String> {
    let entries = list_files(root).map_err(|e| e.to_string())?;
    let files = entries.len() as u64;
    if files > SNAPSHOT_FILES_LIMIT {
        return Err(format!(
            "{files} files, over the {SNAPSHOT_FILES_LIMIT} limit"
        ));
    }
    let mut bytes = 0u64;
    for (rel, path) in &entries {
        let len = path.metadata().map_err(|e| format!("{rel}: {e}"))?.len();
        if len > FILE_LIMIT {
            return Err(format!(
                "file {rel} is {len} bytes, over the {FILE_LIMIT} limit"
            ));
        }
        bytes += len;
    }
    if bytes > bytes_limit {
        return Err(format!("{bytes} bytes, over the {bytes_limit} limit"));
    }
    Ok((files, bytes))
}

/// At most `OUTPUT_LIMIT` bytes; cut if the guest says so or sent more.
fn capped(mut bytes: Vec<u8>, truncated: bool) -> (Vec<u8>, bool) {
    let over = bytes.len() > OUTPUT_LIMIT;
    bytes.truncate(OUTPUT_LIMIT);
    (bytes, truncated || over)
}

/// How long a busy `ws.lock` is retried before it counts as held. A descriptor of it lives
/// on for a moment in a child that another thread of this process forked (until that
/// child's `exec` closes it), which must not read as "attached to another VM"; a VM that
/// really has the image holds it for the whole boot.
const LOCK_PATIENCE: Duration = Duration::from_millis(500);

/// `try_lock` on `ws.lock`, retried for up to `LOCK_PATIENCE` while it is busy.
fn lock_ws(lock: &File) -> Result<(), TryLockError> {
    let until = Instant::now() + LOCK_PATIENCE;
    loop {
        match lock.try_lock() {
            Err(TryLockError::WouldBlock) if Instant::now() < until => thread::sleep(EXIT_POLL),
            other => return other,
        }
    }
}

/// The guest kernel's line for a failed block request on one of the VM's drives (`vdb` the
/// workspace, `vdc` scratch): the drive's host file could not be written, which with sparse
/// images means the host disk is full. Only the kernel prints it with this `] ` prefix; the
/// check cannot write the console (a KVM test checks that).
const GUEST_BLOCK_IO_ERROR: &[u8] = b"] I/O error, dev vd";
/// The most of `console.log` read when looking for it.
const CONSOLE_SCAN_LIMIT: u64 = 4 << 20;

/// Whether the VM's serial console logged a guest block I/O error.
fn guest_block_io_errors(console_log: &Path) -> bool {
    let Ok(mut file) = File::open(console_log) else {
        return false;
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let mut tail = Vec::new();
    let skip = len.saturating_sub(CONSOLE_SCAN_LIMIT);
    if io::Seek::seek(&mut file, io::SeekFrom::Start(skip)).is_err()
        || io::Read::read_to_end(
            &mut io::Read::take(&mut file, CONSOLE_SCAN_LIMIT),
            &mut tail,
        )
        .is_err()
    {
        return false;
    }
    tail.windows(GUEST_BLOCK_IO_ERROR.len())
        .any(|w| w == GUEST_BLOCK_IO_ERROR)
}

/// The reason of an effect whose guest saw block I/O errors.
const HOST_DISK_IO: &str = "host disk: the VM's drives returned I/O errors (see console.log)";

/// `reason`, said to be the host disk's failure when the guest logged block I/O errors
/// (formatting scratch at boot on a full host fails the boot).
fn host_disk_hint(console_log: &Path, reason: String) -> String {
    if guest_block_io_errors(console_log) {
        format!("{HOST_DISK_IO}: {reason}")
    } else {
        reason
    }
}

fn sparse(path: &Path, len: u64) -> io::Result<()> {
    File::create(path)?.set_len(len)
}

/// An existing `ws.img` must have the task's recorded size: the first snapshot fixed it, and
/// nothing ever resizes or recreates it for a later effect.
pub fn check_ws_img_len(ws_img: &Path, resources: &VmResources) -> Result<(), String> {
    let len = fs::metadata(ws_img)
        .map_err(|e| format!("workspace image {}: {e}", ws_img.display()))?
        .len();
    if len == resources.disk_bytes() {
        return Ok(());
    }
    Err(format!(
        "workspace image {} is {len} bytes, not the task's recorded size of {} MiB; it is left as is",
        ws_img.display(),
        resources.disk_mib
    ))
}

/// The advisory host disk check: free space under `dir` must cover `need` bytes. Space can
/// still run out afterwards. The test seam `AGENTOS_TEST_HOST_FREE_MIB` (honoured only with
/// `AGENTOS_TEST_WORKERS=1`, from `extra` or the process) replaces the free figure.
pub fn check_host_space(dir: &Path, need: u64, extra: &[(String, String)]) -> Result<(), String> {
    let seam = env_value(extra, TEST_WORKERS_ENV).as_deref() == Some("1");
    let free = match env_value(extra, "AGENTOS_TEST_HOST_FREE_MIB").filter(|_| seam) {
        Some(mib) => {
            mib.parse::<u64>()
                .map_err(|e| format!("AGENTOS_TEST_HOST_FREE_MIB: {e}"))?
                << 20
        }
        None => {
            let st = rustix::fs::statvfs(dir)
                .map_err(|e| format!("host disk: cannot stat {}: {e}", dir.display()))?;
            st.f_bavail.saturating_mul(st.f_frsize)
        }
    };
    if free >= need {
        return Ok(());
    }
    Err(format!(
        "host disk: {} MiB free under {}, the VM may write {} MiB",
        free >> 20,
        dir.display(),
        need.div_ceil(1 << 20)
    ))
}

/// What a VM may still allocate on the host: the unallocated part of `ws.img` (all of it
/// when a snapshot recreates it) plus the scratch image.
fn vm_host_bytes(resources: &VmResources, ws_img: &Path, fresh_ws: bool) -> u64 {
    let allocated = if fresh_ws {
        0
    } else {
        fs::metadata(ws_img).map_or(0, |m| m.blocks() * 512)
    };
    resources.disk_bytes().saturating_sub(allocated) + resources.scratch_bytes()
}

/// `name` from `extra` (a test seam's environment, last entry wins) or else the process's.
fn env_value(extra: &[(String, String)], name: &str) -> Option<String> {
    extra
        .iter()
        .rev()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var(name).ok())
}

/// A test hook: `name=1` together with `AGENTOS_TEST_WORKERS=1`.
fn test_hook(extra: &[(String, String)], name: &str) -> bool {
    env_value(extra, TEST_WORKERS_ENV).as_deref() == Some("1")
        && env_value(extra, name).as_deref() == Some("1")
}

/// The environment of a spawned VM process: the test switches of this process
/// (`AGENTOS_TEST_WORKERS`, `AGENTOS_TEST_FAKE_GUEST_*`) and the `extra` entries.
fn guest_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .filter(|(k, _)| k == TEST_WORKERS_ENV || k.starts_with("AGENTOS_TEST_FAKE_GUEST_"))
        .collect();
    env.extend(extra.iter().cloned());
    env
}

/// The fake launcher runs only in tests.
fn fake_gate(cfg: &FirecrackerConfig, extra: &[(String, String)]) -> Result<(), String> {
    if matches!(cfg.launcher, GuestLauncher::Fake { .. })
        && env_value(extra, TEST_WORKERS_ENV).as_deref() != Some("1")
    {
        return Err(format!(
            "firecracker worker unavailable: the fake guest launcher needs {TEST_WORKERS_ENV}=1"
        ));
    }
    Ok(())
}

/// The VM's files in `paths.dir` before the launch: `scratch.img` (sparse), empty
/// `console.log` and `stderr.log`, and, unjailed, `firecracker.log` and `vm.json` (jailed,
/// `jail::stage` makes `firecracker.log` as a link to the chroot's, and the chroot's
/// `vm.json`).
fn prepare_vm_files(cfg: &FirecrackerConfig, paths: &VmPaths) -> io::Result<()> {
    sparse(&paths.scratch_img, cfg.resources.scratch_bytes())?;
    File::create(&paths.console_log)?;
    File::create(&paths.stderr_log)?;
    if matches!(cfg.jail, JailMode::Unjailed) {
        File::create(&paths.firecracker_log)?;
        write_vm_json(&paths.vm_json, cfg, &paths.host_view(&cfg.image_dir))?;
    }
    Ok(())
}

/// Which process group a VM is spawned in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// The caller's: the worker's, so every 3a kill path (`worker_pgid`) reaches the VM.
    Caller,
    /// A new one the VM leads: the inspector's VM, a child of the controller, is killed as
    /// a group (it never shares the controller's group).
    Own,
}

/// Spawns the VM `id` for `paths` and says which socket reaches it: Firecracker against
/// `<dir>/vm.json` (unjailed), the fake guest (unjailed, test tier), or, jailed (whatever
/// the launcher), the jailer after `jail::plan` and `jail::stage`, reached through the
/// chroot's socket. The jailer `exec`s Firecracker in place, so the child is Firecracker
/// either way. Errors are effect failure reasons. Used by the worker (`Group::Caller`) and
/// the inspector (`Group::Own`).
fn launch(
    cfg: &FirecrackerConfig,
    paths: &VmPaths,
    task_dir: &Path,
    id: &str,
    extra_env: &[(String, String)],
    group: Group,
    overrides: CgroupOverrides,
) -> Result<(Child, PathBuf), String> {
    let start = |e: io::Error| format!("cannot start firecracker: {e}");
    let stdio = || -> io::Result<(File, File)> {
        Ok((
            File::options().append(true).open(&paths.console_log)?,
            File::options().append(true).open(&paths.stderr_log)?,
        ))
    };
    let env = guest_env(extra_env);
    let (mut cmd, uds) = match (&cfg.jail, &cfg.launcher) {
        (JailMode::Jailed(jc), _) => {
            let plan = jail::plan(jc, &cfg.firecracker_bin, &paths.dir, id)
                .map_err(|e| format!("cannot prepare the jail: {e}"))?;
            let vm_json = render_vm_json(cfg, &jail::chroot_view(&plan));
            let sources = StageSources {
                kernel: &cfg.image_dir.join(KERNEL_FILE),
                rootfs: &cfg.image_dir.join(ROOTFS_FILE),
                ws_img: &paths.ws_img,
                scratch_img: &paths.scratch_img,
                vm_json: &vm_json,
            };
            jail::stage(jc, &plan, &paths.dir, sources)?;
            let mut cmd = Command::new(&jc.jailer_bin);
            // The test seams' lowered bounds count only in a test run.
            let overrides = if env_value(extra_env, TEST_WORKERS_ENV).as_deref() == Some("1") {
                overrides
            } else {
                CgroupOverrides::default()
            };
            cmd.args(jail::jailer_args_with(
                jc,
                &plan,
                &cfg.firecracker_bin,
                cfg.vcpus,
                cfg.memory_mib,
                &cfg.resources,
                overrides,
            ));
            (cmd, jail::host_uds(&plan))
        }
        (JailMode::Unjailed, GuestLauncher::Real { .. }) => {
            let mut cmd = Command::new(&cfg.firecracker_bin);
            cmd.args(["--no-api", "--config-file"])
                .arg(&paths.vm_json)
                .arg("--id")
                .arg(id);
            (cmd, paths.uds.clone())
        }
        (JailMode::Unjailed, fake @ GuestLauncher::Fake { .. }) => {
            let mut cmd = fake_command(fake, &paths.uds, task_dir, &env).map_err(start)?;
            if group == Group::Own {
                cmd.process_group(0);
            }
            return Ok((cmd.spawn().map_err(start)?, paths.uds.clone()));
        }
    };
    let (console, stderr) = stdio().map_err(start)?;
    cmd.env_clear()
        .envs(env)
        .current_dir(&paths.dir)
        .stdin(Stdio::null())
        .stdout(console)
        .stderr(stderr);
    if group == Group::Own {
        cmd.process_group(0);
    }
    let child = cmd.spawn().map_err(start)?;
    Ok((child, uds))
}

impl FirecrackerWorker {
    pub fn new(cfg: &FirecrackerConfig, job: &JobDir) -> FirecrackerWorker {
        // `<home>/jobs/<job>` ⇒ `<home>/inspect`, as `SupervisedExecutor` derives it.
        let inspect_root = job
            .path
            .parent()
            .and_then(Path::parent)
            .map(|home| home.join("inspect"));
        FirecrackerWorker {
            cfg: cfg.clone(),
            job_dir: job.path.clone(),
            inspect_root,
            boot_timeout: BOOT_TIMEOUT,
            env: Vec::new(),
            cgroup_overrides: CgroupOverrides::default(),
        }
    }

    /// Test seam (honoured only with `AGENTOS_TEST_WORKERS=1`): the jail's `memory.max` is
    /// `mib` MiB instead of `memory_mib + JAIL_MEMORY_OVERHEAD_MIB`, so a test can watch the
    /// cgroup (not the guest) kill Firecracker.
    pub fn with_jail_memory_max_mib(mut self, mib: u32) -> FirecrackerWorker {
        self.cgroup_overrides.memory_max_mib = Some(mib);
        self
    }

    /// Test seam (honoured only with `AGENTOS_TEST_WORKERS=1`): the jail's `cpu.max` quota is
    /// `quota_us` per `CPU_PERIOD_US` instead of `vcpus × CPU_PERIOD_US`, so a test can watch
    /// the cgroup throttle Firecracker.
    pub fn with_jail_cpu_quota_us(mut self, quota_us: u64) -> FirecrackerWorker {
        self.cgroup_overrides.cpu_quota_us = Some(quota_us);
        self
    }

    /// Test seam: replaces `BOOT_TIMEOUT`.
    pub fn with_boot_timeout(mut self, timeout: Duration) -> FirecrackerWorker {
        self.boot_timeout = timeout;
        self
    }

    /// Test seam: environment the worker treats as its own on top of the process's (test
    /// hooks such as `AGENTOS_TEST_WORKERS`) and passes to the spawned guest, without
    /// touching the test process's environment.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> FirecrackerWorker {
        self.env = env;
        self
    }

    fn test_hook(&self, name: &str) -> bool {
        test_hook(&self.env, name)
    }

    /// Runs the job (blocking work on a blocking thread). What `run_worker` calls.
    pub async fn run_job(&self, req: &EffectRequest, ctx: &AttemptCtx) -> WorkerResult {
        let (this, req, ctx) = (self.clone(), req.clone(), ctx.clone());
        match tokio::task::spawn_blocking(move || this.run_blocking(&req, &ctx)).await {
            Ok(result) => result,
            Err(e) => WorkerResult::NoOutcome(format!("firecracker worker failed: {e}")),
        }
    }

    /// The pre-launch part of a request: limits, the workspace precondition, the unpinned
    /// profile's source digest. Errors are effect failures (nothing was sent).
    fn plan(&self, req: &EffectRequest) -> Result<Plan, String> {
        match &req.kind {
            EffectKind::ReadSnapshot => {
                let (files, bytes) = tree_within(&self.cfg.snapshot_dir, SNAPSHOT_BYTES_LIMIT)
                    .map_err(|e| format!("snapshot failed: {e}"))?;
                Ok(Plan::Snapshot { files, bytes })
            }
            EffectKind::ApplyPatch { expected_base } => {
                if req.payload.len() > PATCH_LIMIT {
                    return Err(format!(
                        "invalid patch: {} bytes, over the {PATCH_LIMIT} limit",
                        req.payload.len()
                    ));
                }
                Ok(Plan::Patch {
                    expected_base: *expected_base,
                    editable_paths: req.contract.editable_paths.clone(),
                })
            }
            EffectKind::RunVerification => {
                let (files, bytes) = tree_within(&self.cfg.profile_dir, PROFILE_LIMIT)
                    .map_err(|e| format!("cannot stage profile: {e}"))?;
                // Unpinned, the source must not change while the run is under way (the
                // guest only sees the staged copy, so the host checks the source).
                let source = match self.cfg.profile_digest {
                    Some(_) => None,
                    None => Some(
                        workspace_digest(&self.cfg.profile_dir)
                            .map_err(|e| format!("cannot digest profile: {e}"))?,
                    ),
                };
                Ok(Plan::Verify {
                    files,
                    bytes,
                    source,
                })
            }
            EffectKind::ExportBundle => Err("not implemented in this milestone".into()),
            EffectKind::ModelCall { .. }
            | EffectKind::ListFiles { .. }
            | EffectKind::ReadFile { .. } => {
                Err(format!("not a worker effect: {}", req.kind.tag()))
            }
        }
    }

    /// `run_vm`, then, jailed and with an outcome (the VM is reaped by then), the jail's
    /// collection. Without an outcome (a request was sent and the VM was lost) the jail is
    /// left for the controller, which collects once the job is settled.
    fn run_blocking(&self, req: &EffectRequest, ctx: &AttemptCtx) -> WorkerResult {
        let result = self.run_vm(req, ctx);
        if let (JailMode::Jailed(jc), WorkerResult::Outcome(_)) = (&self.cfg.jail, &result)
            && let Err(e) = jail::collect(&self.job_dir, &jc.cgroup_root)
        {
            tracing::warn!(job = %self.job_dir.display(), error = %e, "the jail was not collected");
        }
        result
    }

    /// Preflight ⇒ prepare ⇒ spawn ⇒ boot ⇒ one request ⇒ reply ⇒ shutdown ⇒ outcome. The VM
    /// is reaped (`Vm` kills and waits when dropped) before this returns.
    fn run_vm(&self, req: &EffectRequest, ctx: &AttemptCtx) -> WorkerResult {
        let fail = |reason: String| WorkerResult::Outcome(ExecOutcome::failure(req, ctx, reason));
        let is_patch = matches!(req.kind, EffectKind::ApplyPatch { .. });
        if matches!(req.kind, EffectKind::ExportBundle) {
            return fail("not implemented in this milestone".into());
        }

        // Preflight, before every launch.
        if let Err(e) = fake_gate(&self.cfg, &self.env) {
            return fail(e);
        }
        if let Err(e) = preflight(&self.cfg) {
            return fail(format!("firecracker worker unavailable: {e}"));
        }

        // Prepare: ws.lock, ws.img, the request, scratch.img, the logs, vm.json.
        let task_dir = self.cfg.work_root.join(req.task_id.as_str());
        let paths = VmPaths::new(&self.job_dir, &self.cfg.work_root, &req.task_id);
        let lock = match fs::create_dir_all(&task_dir).and_then(|()| {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(task_dir.join("ws.lock"))
        }) {
            Ok(f) => f,
            Err(e) => return fail(format!("cannot prepare the VM: {e}")),
        };
        match lock_ws(&lock) {
            Ok(()) => {}
            // Another VM has the image: for a patch it may be applying it right now.
            Err(TryLockError::WouldBlock) if is_patch => {
                return WorkerResult::Outcome(ExecOutcome::unresolved(req, ctx, WS_BUSY));
            }
            Err(TryLockError::WouldBlock) => return fail(WS_BUSY.into()),
            Err(TryLockError::Error(e)) => {
                return fail(format!(
                    "cannot prepare the VM: cannot lock {}: {e}",
                    task_dir.join("ws.lock").display()
                ));
            }
        }
        // A dead inspector's VM may still have the image (it held the lock and died).
        if let Some(root) = &self.inspect_root
            && let Err(busy) = collect_dead_inspections(&self.cfg, &root.join(req.task_id.as_str()))
        {
            return if is_patch {
                WorkerResult::Outcome(ExecOutcome::unresolved(req, ctx, busy))
            } else {
                fail(busy)
            };
        }
        let snapshot = matches!(req.kind, EffectKind::ReadSnapshot);
        if !snapshot && !paths.ws_img.is_file() {
            return fail(WORKSPACE_MISSING.into());
        }
        if !snapshot && let Err(why) = check_ws_img_len(&paths.ws_img, &self.cfg.resources) {
            return fail(why);
        }
        let need = vm_host_bytes(&self.cfg.resources, &paths.ws_img, snapshot);
        let ws_dir = paths.ws_img.parent().unwrap_or(&paths.ws_img);
        if let Err(why) = check_host_space(ws_dir, need, &self.env) {
            return fail(why);
        }
        let plan = match self.plan(req) {
            Ok(p) => p,
            Err(reason) => return fail(reason),
        };
        if [&self.cfg.image_dir, &paths.dir, &paths.ws_img]
            .iter()
            .any(|p| p.to_str().is_none())
        {
            return fail("cannot prepare the VM: a VM path is not valid UTF-8".into());
        }
        // ws.img is created by ReadSnapshot only, from zero on every attempt, so a retry
        // starts clean.
        if snapshot && let Err(e) = sparse(&paths.ws_img, self.cfg.resources.disk_bytes()) {
            return fail(format!(
                "cannot prepare the VM: {}: {e}",
                paths.ws_img.display()
            ));
        }
        let _scratch = ScratchGuard(paths.scratch_img.clone());
        if let Err(e) = prepare_vm_files(&self.cfg, &paths) {
            return fail(format!("cannot prepare the VM: {e}"));
        }

        // Spawn and boot.
        let attempt = ctx.attempt_id.to_string();
        let (mut vm, uds) = match launch(
            &self.cfg,
            &paths,
            &task_dir,
            &attempt,
            &self.env,
            Group::Caller,
            self.cgroup_overrides,
        ) {
            Ok((child, uds)) => (
                Vm {
                    child,
                    status: None,
                    own_group: false,
                    swept: false,
                },
                uds,
            ),
            Err(reason) => return fail(reason),
        };
        let boot_deadline = Instant::now() + self.boot_timeout;
        let connected = GuestLink::connect_until(&uds, boot_deadline, || vm.exited());
        let mut link = match connected {
            Ok(link) => link,
            Err(e) => {
                vm.kill();
                return fail(host_disk_hint(&paths.console_log, not_up(&e)));
            }
        };
        let hello = Message::Hello {
            protocol: GUEST_PROTOCOL,
            attempt_token: self.cfg.attempt_token.clone(),
            task_id: req.task_id.as_str().to_string(),
            effect_id: req.effect_id.as_str().to_string(),
            attempt_id: attempt.clone(),
            lease_generation: ctx.lease_generation,
            mode: Mode::Job,
        };
        let ready = link
            .hello(hello, boot_deadline)
            .and_then(|ready| match ready {
                Message::Ready {
                    mode: Mode::Job, ..
                } => link.set_write_timeout(Some(WRITE_TIMEOUT)),
                _ => Err(LinkError::Protocol("guest is not in job mode".into())),
            });
        if let Err(e) = ready {
            drop(link);
            vm.kill();
            return fail(host_disk_hint(&paths.console_log, not_up(&e)));
        }

        // Serve: one request, one reply. From the first byte of the request on, a lost
        // connection or a violation leaves the effect unknown for a patch.
        let served = self.serve(&mut link, &plan, req, &mut vm);
        let reply = match served {
            Ok(reply) => reply,
            Err(served) => {
                drop(link);
                let status = vm.wait_or_kill(SHUTDOWN_WAIT);
                let reason = match served {
                    Served::Lost => {
                        format!("guest exited before reporting: {}", exit_code_text(&status))
                    }
                    Served::Violation(why) => why,
                };
                return if is_patch {
                    WorkerResult::NoOutcome(reason)
                } else {
                    fail(reason)
                };
            }
        };

        // Done: Shutdown/Bye, wait for the exit, then the outcome.
        if link.send(&Message::Shutdown).is_ok() {
            let _ = link.recv(Instant::now() + SHUTDOWN_WAIT);
        }
        drop(link);
        vm.wait_or_kill(SHUTDOWN_WAIT);
        drop(vm);
        // With I/O errors from the drives, a refusal or a failed check says nothing about the
        // workspace or the code: it is the host's failure, and the agent is told so.
        let io_errors = guest_block_io_errors(&paths.console_log);
        let out = match reply {
            Reply::Refused(reason) if io_errors => {
                ExecOutcome::failure(req, ctx, format!("{HOST_DISK_IO}: {reason}"))
            }
            Reply::Refused(reason) => ExecOutcome::failure(req, ctx, reason),
            Reply::Snapshot(files, digest) => outcomes::snapshot_manifest(req, ctx, files, digest),
            Reply::Patch(touched, digest) => outcomes::patch_applied(req, ctx, touched, digest),
            Reply::Verified(check) => match self.check_profile(&plan, &check) {
                Ok(()) if io_errors && check.exit_code != Some(0) => ExecOutcome::failure(
                    req,
                    ctx,
                    format!(
                        "{HOST_DISK_IO}; the check's failure (exit code {:?}) is not evidence",
                        check.exit_code
                    ),
                ),
                Ok(()) => outcomes::evidence(req, ctx, &check),
                Err(reason) => ExecOutcome::failure(req, ctx, reason),
            },
        };
        drop(_scratch);
        drop(lock);
        WorkerResult::Outcome(out)
    }

    /// The profile the guest ran must be the pinned one, or (unpinned) the source as it was
    /// when the run started and still is now.
    fn check_profile(&self, plan: &Plan, check: &Check) -> Result<(), String> {
        if let Some(pinned) = self.cfg.profile_digest
            && check.profile_digest != pinned
        {
            return Err(format!(
                "profile digest mismatch: pinned {pinned}, found {}",
                check.profile_digest
            ));
        }
        if let Plan::Verify {
            source: Some(source),
            ..
        } = plan
        {
            let unchanged = workspace_digest(&self.cfg.profile_dir).is_ok_and(|d| d == *source);
            if check.profile_digest != *source || !unchanged {
                return Err("protected profile changed during verification".into());
            }
        }
        Ok(())
    }

    fn serve(
        &self,
        link: &mut GuestLink,
        plan: &Plan,
        req: &EffectRequest,
        vm: &mut Vm,
    ) -> Result<Reply, Served> {
        let link_err = |e: LinkError| match e {
            LinkError::Protocol(_) => Served::Violation(e.to_string()),
            _ => Served::Lost,
        };
        let reply_until = match plan {
            Plan::Snapshot { files, bytes } => {
                link.send(&Message::ReadSnapshot {
                    file_count: *files,
                    total_bytes: *bytes,
                })
                .map_err(link_err)?;
                link.send_tree(&self.cfg.snapshot_dir).map_err(link_err)?;
                Instant::now() + REPLY_TIMEOUT
            }
            Plan::Patch {
                expected_base,
                editable_paths,
            } => {
                link.send(&Message::ApplyPatch {
                    expected_base: *expected_base,
                    editable_paths: editable_paths.clone(),
                })
                .map_err(link_err)?;
                link.send_patch(&req.payload).map_err(link_err)?;
                if self.test_hook(KILL_VM_AFTER_REQUEST_ENV) {
                    vm.kill();
                    return Err(Served::Lost);
                }
                Instant::now() + REPLY_TIMEOUT
            }
            Plan::Verify { files, bytes, .. } => {
                link.send(&Message::RunVerification {
                    profile_digest: self.cfg.profile_digest,
                    timeout_secs: self.cfg.verify_timeout_secs,
                    file_count: *files,
                    total_bytes: *bytes,
                })
                .map_err(link_err)?;
                link.send_tree(&self.cfg.profile_dir).map_err(link_err)?;
                Instant::now()
                    + Duration::from_secs(self.cfg.verify_timeout_secs)
                    + VERIFY_REPLY_MARGIN
            }
        };
        let violation = |why: String| Served::Violation(LinkError::Protocol(why).to_string());
        let expected = match plan {
            Plan::Snapshot { .. } => "SnapshotDone",
            Plan::Patch { .. } => "PatchApplied",
            Plan::Verify { .. } => "Verified",
        };
        match (plan, link.recv(reply_until).map_err(link_err)?) {
            (_, Message::Refused { reason }) => Ok(Reply::Refused(reason)),
            (
                Plan::Snapshot { .. },
                Message::SnapshotDone {
                    files,
                    workspace_digest,
                },
            ) => Ok(Reply::Snapshot(files, workspace_digest)),
            (
                Plan::Patch { .. },
                Message::PatchApplied {
                    paths,
                    workspace_digest,
                },
            ) => Ok(Reply::Patch(paths, workspace_digest)),
            (
                Plan::Verify { .. },
                Message::Verified {
                    profile_id,
                    command,
                    profile_digest,
                    workspace_digest,
                    exit_code,
                    stdout_b64,
                    stdout_truncated,
                    stderr_b64,
                    stderr_truncated,
                },
            ) => {
                let stdout = unb64(&stdout_b64)
                    .map_err(|e| violation(guest_text(&format!("stdout_b64: {e}"))))?;
                let stderr = unb64(&stderr_b64)
                    .map_err(|e| violation(guest_text(&format!("stderr_b64: {e}"))))?;
                let (stdout, stdout_truncated) = capped(stdout, stdout_truncated);
                let (stderr, stderr_truncated) = capped(stderr, stderr_truncated);
                Ok(Reply::Verified(Check {
                    profile_id,
                    command,
                    profile_digest,
                    workspace_digest,
                    exit_code,
                    stdout,
                    stdout_truncated,
                    stderr,
                    stderr_truncated,
                }))
            }
            (_, other) => Err(violation(format!(
                "expected {expected}, got {}",
                type_name(&other)
            ))),
        }
    }
}

/// What an inspection boot asks the guest (inspect mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// The workspace digest (`current_workspace`).
    Digest,
    /// Whether `patch` was applied on top of `expected_base` (`reconcile`).
    PatchState {
        expected_base: Digest,
        patch: Vec<u8>,
    },
}

/// The guest's answer to a `Query`. Guest-controlled: `PatchStateIs.reason` must be
/// escaped (`guest_text`) before it reaches a log, and `paths` only ever goes into JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Digest(Digest),
    PatchState(PatchStateIs),
}

/// The controller's inspection boots: the task's guest image booted in inspect mode over
/// the task's `ws.img`, one query, then shut down. Runs in the controller process, in a
/// process group of its own; bounded by `INSPECT_TIMEOUT`; jailed like a job VM.
#[derive(Clone)]
pub struct Inspector {
    cfg: FirecrackerConfig,
    /// `<home>/inspect`: inspections of a task live in `<inspect_root>/<task>/<uuid>/`.
    inspect_root: PathBuf,
    inspect_timeout: Duration,
    boot_timeout: Duration,
    env: Vec<(String, String)>,
    /// Test seams, as `FirecrackerWorker`'s.
    cgroup_overrides: CgroupOverrides,
}

/// Never prints the config's attempt token.
impl std::fmt::Debug for Inspector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cfg = FirecrackerConfig {
            attempt_token: "<redacted>".into(),
            ..self.cfg.clone()
        };
        f.debug_struct("Inspector")
            .field("cfg", &cfg)
            .field("inspect_root", &self.inspect_root)
            .field("inspect_timeout", &self.inspect_timeout)
            .field("boot_timeout", &self.boot_timeout)
            .field("env", &self.env)
            .field("cgroup_overrides", &self.cgroup_overrides)
            .finish()
    }
}

fn inspect_failed(why: impl std::fmt::Display) -> String {
    format!("workspace inspection failed: {why}")
}

/// `60s`, or `1500ms` for a duration that is not whole seconds.
fn duration_text(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Collects the dead inspections of a task (`<inspect_root>/<task>/*`). Called only with
/// the task's `ws.lock` held, by the inspector and by the worker before it boots: every
/// inspection holds that lock while its VM runs, so a directory found here is a dead
/// inspector's. Its jail is collected, then the directory removed. A jail whose cgroup still
/// has processes is proof that the dead inspector's VM lives on with `ws.img` attached (the
/// lock died with its holder): that is `Err(WS_BUSY)`, and no second VM may boot on the
/// image. Any other failure is a warning and keeps the directory for the next try.
pub(crate) fn collect_dead_inspections(
    cfg: &FirecrackerConfig,
    task_root: &Path,
) -> Result<(), String> {
    let entries = match fs::read_dir(task_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            tracing::warn!(dir = %task_root.display(), error = %e, "cannot list dead inspections");
            return Ok(());
        }
    };
    let mut live = false;
    for entry in entries.flatten() {
        let dir = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            tracing::warn!(path = %dir.display(), "not an inspect directory; left alone");
            continue;
        }
        match jail::collect_jail(&dir, collect_root(cfg)) {
            Ok(_) => {
                if let Err(e) = fs::remove_dir_all(&dir) {
                    tracing::warn!(dir = %dir.display(), error = %e, "cannot remove a dead inspection");
                }
            }
            Err(jail::CollectError::Busy(e)) => {
                tracing::warn!(dir = %dir.display(), error = %e, "a dead inspection's VM is still alive on the workspace image");
                live = true;
            }
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "a dead inspection's jail was not collected; kept")
            }
        }
    }
    if live { Err(WS_BUSY.into()) } else { Ok(()) }
}

/// The cgroup root `jail::collect` checks markers against: the jail's, or (unjailed, where
/// no marker is ever written) a root no marker can name.
pub(crate) fn collect_root(cfg: &FirecrackerConfig) -> &Path {
    match &cfg.jail {
        JailMode::Jailed(jc) => &jc.cgroup_root,
        JailMode::Unjailed => Path::new("/nonexistent"),
    }
}

impl Inspector {
    /// `inspect_root` is `<home>/inspect`; the engine takes it explicitly.
    pub fn new(cfg: FirecrackerConfig, inspect_root: PathBuf) -> Inspector {
        Inspector {
            cfg,
            inspect_root,
            inspect_timeout: INSPECT_TIMEOUT,
            boot_timeout: BOOT_TIMEOUT,
            env: Vec::new(),
            cgroup_overrides: CgroupOverrides::default(),
        }
    }

    /// Test seam, as `FirecrackerWorker::with_jail_memory_max_mib`.
    pub fn with_jail_memory_max_mib(mut self, mib: u32) -> Inspector {
        self.cgroup_overrides.memory_max_mib = Some(mib);
        self
    }

    /// Test seam, as `FirecrackerWorker::with_jail_cpu_quota_us`.
    pub fn with_jail_cpu_quota_us(mut self, quota_us: u64) -> Inspector {
        self.cgroup_overrides.cpu_quota_us = Some(quota_us);
        self
    }

    /// Test seam: replaces `INSPECT_TIMEOUT`.
    pub fn with_inspect_timeout(mut self, timeout: Duration) -> Inspector {
        self.inspect_timeout = timeout;
        self
    }

    /// Test seam: replaces `BOOT_TIMEOUT`.
    pub fn with_boot_timeout(mut self, timeout: Duration) -> Inspector {
        self.boot_timeout = timeout;
        self
    }

    /// Test seam: environment treated as this process's own (test hooks) and passed to the
    /// spawned guest, as `FirecrackerWorker::with_env`.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Inspector {
        self.env = env;
        self
    }

    pub(crate) fn push_env(&mut self, key: String, value: String) {
        self.env.push((key, value));
    }

    pub fn config(&self) -> &FirecrackerConfig {
        &self.cfg
    }

    /// One inspection boot (blocking): `ws.img` must exist; preflight; dead inspections of
    /// the task collected; `ws.lock` taken; `<inspect_root>/<task>/<uuid>/` prepared; the
    /// VM (`inspect-<uuid>`) spawned in a new process group, asked `query` in inspect mode,
    /// shut down; its jail collected; the directory removed on success and kept (without
    /// its jail and scratch image) on failure. Bounded by the inspect timeout.
    pub fn query(&self, task: &TaskId, query: Query) -> Result<Answer, String> {
        let task_dir = self.cfg.work_root.join(task.as_str());
        let ws_img = task_dir.join("ws.img");
        if !ws_img.is_file() {
            return Err(format!("workspace image {} is missing", ws_img.display()));
        }
        check_ws_img_len(&ws_img, &self.cfg.resources).map_err(inspect_failed)?;
        let deadline = Instant::now() + self.inspect_timeout;
        fake_gate(&self.cfg, &self.env).map_err(inspect_failed)?;
        preflight(&self.cfg)
            .map_err(|e| inspect_failed(format!("firecracker worker unavailable: {e}")))?;
        let task_root = self.inspect_root.join(task.as_str());

        let lock_path = task_dir.join("ws.lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| inspect_failed(format!("cannot lock {}: {e}", lock_path.display())))?;
        match lock_ws(&lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(WS_BUSY.into()),
            Err(TryLockError::Error(e)) => {
                return Err(inspect_failed(format!(
                    "cannot lock {}: {e}",
                    lock_path.display()
                )));
            }
        }
        // Under the lock: no VM of this process owns any of the task's inspect directories.
        collect_dead_inspections(&self.cfg, &task_root)?;

        let uuid = AttemptId::new().to_string();
        let id = format!("inspect-{uuid}");
        let dir = task_root.join(&uuid);
        let paths = VmPaths::new(&dir, &self.cfg.work_root, task);
        let result = self
            .boot(&paths, &task_dir, &id, task, &query, deadline)
            .map_err(|why| {
                if Instant::now() >= deadline {
                    inspect_failed(format!(
                        "timeout after {}",
                        duration_text(self.inspect_timeout)
                    ))
                } else {
                    inspect_failed(why)
                }
            });

        // The VM is reaped by now.
        let collected = match jail::collect(&dir, collect_root(&self.cfg)) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e, "the inspection's jail was not collected");
                false
            }
        };
        let _ = fs::remove_file(&paths.scratch_img);
        match &result {
            Ok(_) if collected => {
                if let Err(e) = fs::remove_dir_all(&dir) {
                    tracing::warn!(dir = %dir.display(), error = %e, "cannot remove the inspect directory");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(task = %task, dir = %dir.display(), error = %e, "inspection failed; its directory is kept")
            }
        }
        drop(lock);
        result
    }

    /// Prepare, spawn, connect, `Hello{mode: inspect}`, the query, `Shutdown`. The VM is
    /// reaped (`Vm` kills its group and waits when dropped) before this returns. Errors are
    /// the reasons after `workspace inspection failed: `.
    fn boot(
        &self,
        paths: &VmPaths,
        task_dir: &Path,
        id: &str,
        task: &TaskId,
        query: &Query,
        deadline: Instant,
    ) -> Result<Answer, String> {
        fs::create_dir_all(&paths.dir)
            .map_err(|e| format!("cannot prepare the VM: {}: {e}", paths.dir.display()))?;
        if [&self.cfg.image_dir, &paths.dir, &paths.ws_img]
            .iter()
            .any(|p| p.to_str().is_none())
        {
            return Err("cannot prepare the VM: a VM path is not valid UTF-8".into());
        }
        prepare_vm_files(&self.cfg, paths).map_err(|e| format!("cannot prepare the VM: {e}"))?;
        let (child, uds) = launch(
            &self.cfg,
            paths,
            task_dir,
            id,
            &self.env,
            Group::Own,
            self.cgroup_overrides,
        )?;
        let mut vm = Vm {
            child,
            status: None,
            own_group: true,
            swept: false,
        };

        let boot_deadline = deadline.min(Instant::now() + self.boot_timeout);
        let mut link = GuestLink::connect_until(&uds, boot_deadline, || vm.exited())
            .map_err(|e| not_up(&e))?;
        let hello = Message::Hello {
            protocol: GUEST_PROTOCOL,
            // Fresh per inspection: identity of this boot, never a job's token.
            attempt_token: mint_attempt_token(),
            task_id: task.as_str().to_string(),
            effect_id: String::new(),
            attempt_id: id.to_string(),
            lease_generation: 0,
            mode: Mode::Inspect,
        };
        match link.hello(hello, boot_deadline) {
            Ok(Message::Ready {
                mode: Mode::Inspect,
                ..
            }) => {}
            Ok(_) => {
                return Err(not_up(&LinkError::Protocol(
                    "guest is not in inspect mode".into(),
                )));
            }
            Err(e) => return Err(not_up(&e)),
        }
        let left = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        link.set_write_timeout(Some(left))
            .map_err(|e| e.to_string())?;

        let violation = |why: String| LinkError::Protocol(why).to_string();
        let answer = match query {
            Query::Digest => {
                link.send(&Message::Digest).map_err(|e| e.to_string())?;
                match link.recv(deadline).map_err(|e| e.to_string())? {
                    Message::DigestIs { workspace_digest } => Answer::Digest(workspace_digest),
                    Message::Refused { reason } => return Err(guest_text(&reason)),
                    other => {
                        return Err(violation(format!(
                            "expected DigestIs, got {}",
                            type_name(&other)
                        )));
                    }
                }
            }
            Query::PatchState {
                expected_base,
                patch,
            } => {
                link.send(&Message::PatchState {
                    expected_base: *expected_base,
                })
                .map_err(|e| e.to_string())?;
                link.send_patch(patch).map_err(|e| e.to_string())?;
                match link.recv(deadline).map_err(|e| e.to_string())? {
                    Message::PatchStateIs {
                        state,
                        paths,
                        workspace_digest,
                        reason,
                    } => Answer::PatchState(PatchStateIs {
                        state,
                        paths,
                        workspace_digest,
                        reason,
                    }),
                    Message::Refused { reason } => return Err(guest_text(&reason)),
                    other => {
                        return Err(violation(format!(
                            "expected PatchStateIs, got {}",
                            type_name(&other)
                        )));
                    }
                }
            }
        };

        // The answer is in hand: shut down, bounded by what is left of the inspection.
        let wait = SHUTDOWN_WAIT.min(deadline.saturating_duration_since(Instant::now()));
        if link.send(&Message::Shutdown).is_ok() {
            let _ = link.recv(Instant::now() + wait);
        }
        drop(link);
        vm.wait_or_kill(wait);
        Ok(answer)
    }
}

impl Worker for FirecrackerWorker {
    /// `run_job` for callers that need an outcome (the conformance suite): a job whose
    /// effect is unknown is an unresolved outcome.
    ///
    /// Production never comes through here: `run_worker` calls `run_job`, and a
    /// `NoOutcome` there writes no `outcome.json` and exits 1 (spec issue 1), so the
    /// supervisor writes no receipt and the controller reconciles by inspection. This
    /// method must never be used to write `outcome.json`, least of all for `ApplyPatch`:
    /// the unresolved placeholder would stand in for an effect that may have happened.
    async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> ExecOutcome {
        match self.run_job(req, ctx).await {
            WorkerResult::Outcome(out) => out,
            WorkerResult::NoOutcome(reason) => ExecOutcome::unresolved(req, ctx, reason),
        }
    }

    /// The inspector answers this from the controller.
    async fn reconcile(&self, _req: &EffectRequest, _ctx: &AttemptCtx) -> Reconciliation {
        Reconciliation::Unknown
    }

    fn current_workspace(&self, _task: &TaskId) -> Option<Result<Digest, String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jail::JailConfig;
    use crate::job::{JobRequest, WorkerConfig};
    use agentos_core::contract::Contract;
    use agentos_core::effect::{AttemptId, EffectKind};

    fn config() -> FirecrackerConfig {
        FirecrackerConfig {
            resources: agentos_core::resources::VmResources::V0,
            firecracker_bin: "/home/x/bin/firecracker".into(),
            image_dir: "/home/x/registry/images/python-stdlib-v1@0545ba17".into(),
            image_digest: Digest::of(b"image"),
            snapshot_dir: "/home/x/snapshot".into(),
            profile_dir: "/home/x/profile".into(),
            profile_digest: None,
            work_root: "/home/x/work".into(),
            verify_timeout_secs: 60,
            vcpus: 2,
            memory_mib: 512,
            attempt_token: "0123456789abcdef0123456789abcdef".into(),
            launcher: GuestLauncher::Real {
                firecracker_bin: "/home/x/bin/firecracker".into(),
            },
            jail: JailMode::Unjailed,
        }
    }

    #[test]
    fn vm_json_matches_the_golden_file() {
        let cfg = config();
        let task: TaskId = serde_json::from_str("\"task-1\"").unwrap();
        let paths = VmPaths::new(
            Path::new("/home/x/jobs/effect-1-attempt-1"),
            &cfg.work_root,
            &task,
        );
        let rendered = render_vm_json(&cfg, &paths.host_view(&cfg.image_dir));
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("../tests/golden/vm.json")).unwrap();
        assert_eq!(rendered, golden);
        assert_eq!(rendered["network-interfaces"], serde_json::json!([]));
        assert_eq!(rendered["machine-config"]["smt"], false);
        let ids: Vec<&str> = rendered["drives"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["drive_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["rootfs", "workspace", "scratch"]);
        assert_eq!(rendered["vsock"]["guest_cid"], 3);

        // The written file keeps the documented field order.
        let dir = tempfile::tempdir().unwrap();
        write_vm_json(
            &dir.path().join("vm.json"),
            &cfg,
            &paths.host_view(&cfg.image_dir),
        )
        .unwrap();
        let written = fs::read_to_string(dir.path().join("vm.json")).unwrap();
        let order: Vec<usize> = [
            "boot-source",
            "drives",
            "machine-config",
            "vsock",
            "network-interfaces",
            "logger",
        ]
        .iter()
        .map(|k| written.find(&format!("\"{k}\"")).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{written}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&written).unwrap(),
            golden
        );
    }

    #[test]
    fn the_scratch_image_has_the_contracted_size() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = config();
        cfg.resources = VmResources {
            version: 1,
            scratch_mib: 768,
            ..VmResources::V0
        };
        let task: TaskId = serde_json::from_str("\"task-1\"").unwrap();
        let paths = VmPaths::new(dir.path(), &cfg.work_root, &task);
        prepare_vm_files(&cfg, &paths).unwrap();
        let scratch = fs::metadata(&paths.scratch_img).unwrap();
        assert_eq!(scratch.len(), 768 << 20);
        assert!(scratch.blocks() * 512 < 1 << 20, "scratch.img stays sparse");
    }

    #[test]
    fn only_the_kernel_block_error_line_counts_as_a_guest_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let console = dir.path().join("console.log");
        let check = |text: &str| {
            fs::write(&console, text).unwrap();
            guest_block_io_errors(&console)
        };
        assert!(check(
            "[    0.593152] I/O error, dev vdc, sector 278528 op 0x1:(WRITE) flags 0x4000\n"
        ));
        assert!(!check(
            "agentos-guest: snapshot failed: syncfs /workspace: I/O error (os error 5)\n"
        ));
        assert!(!check(
            "Buffer I/O error on device vdc, logical block 34816\n"
        ));
        assert!(!guest_block_io_errors(&dir.path().join("missing.log")));
        // Only the last CONSOLE_SCAN_LIMIT bytes are read.
        let mut long = "[ 1.0] I/O error, dev vdb, sector 1\n".to_string();
        long.push_str(&"x".repeat(CONSOLE_SCAN_LIMIT as usize));
        assert!(!check(&long));
    }

    /// A `request.json` written before resources were recorded still parses, as version 0.
    #[test]
    fn a_config_without_resources_parses_as_version_zero() {
        let mut json = serde_json::to_value(config()).unwrap();
        json.as_object_mut().unwrap().remove("resources").unwrap();
        let cfg: FirecrackerConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.resources, VmResources::V0);
    }

    #[test]
    fn rate_limits_go_on_the_writable_drives_only_and_are_absent_by_default() {
        let task: TaskId = serde_json::from_str("\"task-1\"").unwrap();
        let mut cfg = config();
        let paths = VmPaths::new(Path::new("/home/x/jobs/e-a"), &cfg.work_root, &task);
        let drives = |cfg: &FirecrackerConfig| {
            render_vm_json(cfg, &paths.host_view(&cfg.image_dir))["drives"]
                .as_array()
                .unwrap()
                .clone()
        };
        assert!(
            drives(&cfg).iter().all(|d| d.get("rate_limiter").is_none()),
            "no limit, no key: the rendered file is unchanged"
        );
        cfg.resources = VmResources {
            version: 1,
            disk_mib: 2048,
            scratch_mib: 768,
            bandwidth_mib_s: Some(64),
            iops: Some(5000),
        };
        let both = serde_json::json!({
            "bandwidth": { "size": 64u64 << 20, "refill_time": 1000 },
            "ops": { "size": 5000, "refill_time": 1000 }
        });
        let d = drives(&cfg);
        assert!(
            d[0].get("rate_limiter").is_none(),
            "the read-only rootfs is never limited"
        );
        assert_eq!(d[1]["rate_limiter"], both);
        assert_eq!(d[2]["rate_limiter"], both);

        cfg.resources.iops = None;
        let d = drives(&cfg);
        assert_eq!(
            d[1]["rate_limiter"],
            serde_json::json!({ "bandwidth": { "size": 64u64 << 20, "refill_time": 1000 } })
        );
        cfg.resources.bandwidth_mib_s = None;
        cfg.resources.iops = Some(5000);
        assert_eq!(
            drives(&cfg)[2]["rate_limiter"],
            serde_json::json!({ "ops": { "size": 5000, "refill_time": 1000 } })
        );
    }

    #[test]
    fn firecracker_config_round_trips_with_jail_unjailed_and_jailed() {
        let unjailed = config();
        let mut jailed = config();
        jailed.jail = JailMode::Jailed(JailConfig {
            jailer_bin: "/home/x/bin/jailer".into(),
            uid: crate::jail::JAIL_UID,
            gid: crate::jail::JAIL_GID,
            cgroup_root: "/sys/fs/cgroup".into(),
        });
        jailed.launcher = GuestLauncher::Fake {
            program: "/bin/agentos".into(),
            prefix_args: vec!["supervise".into()],
        };
        jailed.profile_digest = Some(Digest::of(b"p"));
        for cfg in [unjailed, jailed] {
            let json = serde_json::to_string(&cfg).unwrap();
            assert_eq!(
                serde_json::from_str::<FirecrackerConfig>(&json).unwrap(),
                cfg
            );
            let worker = WorkerConfig::Firecracker(cfg.clone());
            let json = serde_json::to_string(&worker).unwrap();
            assert_eq!(serde_json::from_str::<WorkerConfig>(&json).unwrap(), worker);
        }
    }

    #[test]
    fn exit_codes_are_named() {
        for code in [0, 1, 2, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157] {
            assert_eq!(
                exit_code_text(&ExitStatus::from_raw(code << 8)),
                format!("firecracker exit code {code}")
            );
        }
        assert_eq!(
            exit_code_text(&ExitStatus::from_raw(9)),
            "firecracker killed by signal 9"
        );
    }

    #[test]
    fn config_validation_rejects_vcpus_0_and_33_and_memory_127() {
        let with = |vcpus, memory_mib| {
            FirecrackerConfig {
                vcpus,
                memory_mib,
                ..config()
            }
            .validate()
        };
        assert_eq!(
            with(0, 256).unwrap_err(),
            "worker_vcpus must be between 1 and 32"
        );
        assert_eq!(
            with(33, 256).unwrap_err(),
            "worker_vcpus must be between 1 and 32"
        );
        assert_eq!(
            with(1, 127).unwrap_err(),
            "worker_memory_mib must be at least 128"
        );
        for (v, m) in [(1, 128), (32, 128), (1, 65536)] {
            with(v, m).unwrap();
        }
    }

    fn request(cfg: FirecrackerConfig) -> JobRequest {
        let json = r#"{"goal":"g","repository":{"source":"s","revision":"r"},"profile":"p","editable_paths":["src/**"],"verification_profile":"v","capabilities":["snapshot.read"],"limits":{"model_requests":1,"max_output_tokens_per_request":1,"tool_actions":1,"deadline_seconds":1,"worker_vcpus":1,"worker_memory_mib":1}}"#;
        JobRequest {
            effect_id: serde_json::from_str("\"abc\"").unwrap(),
            task_id: TaskId::new(),
            kind: EffectKind::ReadSnapshot,
            payload: vec![],
            contract: Contract::parse(json).unwrap(),
            attempt_id: AttemptId::new(),
            lease_generation: 1,
            lease_expiry_ms: 1,
            task_deadline_ms: 1,
            worker: WorkerConfig::Firecracker(cfg),
        }
    }

    #[test]
    fn relative_paths_and_bad_tokens_are_rejected_by_job_dir_create() {
        let root = tempfile::tempdir().unwrap();
        type Field = fn(&mut FirecrackerConfig) -> &mut PathBuf;
        let fields: [(&str, Field); 5] = [
            ("firecracker_bin", |c| &mut c.firecracker_bin),
            ("image_dir", |c| &mut c.image_dir),
            ("snapshot_dir", |c| &mut c.snapshot_dir),
            ("profile_dir", |c| &mut c.profile_dir),
            ("work_root", |c| &mut c.work_root),
        ];
        for (name, field) in fields {
            let mut cfg = config();
            *field(&mut cfg) = PathBuf::from("relative/path");
            let err = JobDir::create(root.path(), &request(cfg)).err().unwrap();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name}");
            assert!(err.to_string().starts_with(name), "{err}");
        }
        for token in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789ABCDEF0123456789ABCDEF",
        ] {
            let cfg = FirecrackerConfig {
                attempt_token: token.into(),
                ..config()
            };
            let err = JobDir::create(root.path(), &request(cfg)).err().unwrap();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{token:?}");
            assert!(err.to_string().contains("attempt_token"), "{err}");
        }
        let mut jailed = config();
        jailed.jail = JailMode::Jailed(JailConfig {
            jailer_bin: "jailer".into(),
            uid: 1,
            gid: 1,
            cgroup_root: "/sys/fs/cgroup".into(),
        });
        assert!(JobDir::create(root.path(), &request(jailed)).is_err());
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            0,
            "nothing was created"
        );
        JobDir::create(root.path(), &request(config())).unwrap();
        // A Real launcher must name the same (absolute) binary as the config.
        let mut two = config();
        two.launcher = GuestLauncher::Real {
            firecracker_bin: "/elsewhere/firecracker".into(),
        };
        let err = JobDir::create(root.path(), &request(two)).err().unwrap();
        assert!(
            err.to_string().contains("differs from firecracker_bin"),
            "{err}"
        );
        // run_worker re-checks the same rule.
        assert!(
            WorkerConfig::Firecracker(FirecrackerConfig {
                work_root: "w".into(),
                ..config()
            })
            .check_paths()
            .is_err()
        );
    }

    #[test]
    fn firecracker_version_is_bounded_and_checked() {
        let sh = |script: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", script]);
            cmd
        };
        let started = Instant::now();
        let err = version_line(sh("sleep 30"), Duration::from_millis(300)).unwrap_err();
        assert_eq!(err, "no answer within 300 ms");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            version_line(
                sh("echo 'Firecracker v1.17.0'; echo x"),
                Duration::from_secs(5)
            )
            .unwrap(),
            "Firecracker v1.17.0"
        );
        let err = version_line(
            sh("printf 'Firecracker v1.16.2\\n'"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(
            err.starts_with("expected Firecracker v1.17.…, got \"Firecracker v1.16.2\""),
            "{err}"
        );
        assert!(firecracker_version(Path::new("/nonexistent/firecracker")).is_err());
    }

    #[test]
    fn guest_text_is_one_bounded_line() {
        use crate::guestlink::{GUEST_TEXT_LIMIT, escape_controls, guest_text};
        assert_eq!(
            escape_controls("a\nb\u{1b}c\u{2028}"),
            "a\\nb\\u{1b}c\\u{2028}"
        );
        assert_eq!(guest_text("plain"), "plain");
        let long = guest_text(&"é\n".repeat(10_000));
        assert!(
            long.len() <= GUEST_TEXT_LIMIT + " [truncated]".len() && long.ends_with(" [truncated]"),
            "{long}"
        );
        assert!(!long.chars().any(|c| c.is_control()));
    }

    #[test]
    fn a_refusal_at_hello_is_escaped_and_bounded() {
        let why = not_up(&LinkError::Refused(format!(
            "bad\n\u{1b}[31m{}",
            "x".repeat(5000)
        )));
        assert!(
            why.starts_with("guest did not come up: bad\\n\\u{1b}[31m"),
            "{why}"
        );
        assert!(
            !why.chars().any(|c| c.is_control()) && why.len() < 700,
            "{why}"
        );
    }

    #[test]
    fn image_manifest_rejects_unknown_fields_and_wrong_protocol() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(KERNEL_FILE), b"k").unwrap();
        fs::write(dir.path().join(ROOTFS_FILE), b"r").unwrap();
        let good = r#"{"id":"python-stdlib-v1","protocol":1,"kernel":"vmlinux","rootfs":"rootfs.squashfs","agent_version":"0.1.0","kernel_sha256":"0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447","built_from":"test"}"#;
        let write = |s: &str| fs::write(dir.path().join("image.json"), s).unwrap();
        write(good);
        assert_eq!(read_image(dir.path()).unwrap().id, "python-stdlib-v1");

        write(&good.replace(r#""built_from":"test""#, r#""built_from":"test","extra":1"#));
        assert!(
            read_image(dir.path())
                .unwrap_err()
                .contains("unknown field `extra`")
        );
        write(&good.replace(r#""protocol":1"#, r#""protocol":2"#));
        assert!(
            read_image(dir.path())
                .unwrap_err()
                .contains("protocol 2, expected 1")
        );
        write(&good.replace(r#""kernel":"vmlinux""#, r#""kernel":"../../etc/vmlinux""#));
        assert!(
            read_image(dir.path())
                .unwrap_err()
                .contains("expected \"vmlinux\"")
        );
        write(good);
        fs::remove_file(dir.path().join(ROOTFS_FILE)).unwrap();
        assert!(read_image(dir.path()).unwrap_err().contains("is missing"));
    }

    #[test]
    fn image_manifest_records_optional_interpreter_provenance() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(KERNEL_FILE), b"k").unwrap();
        fs::write(dir.path().join(ROOTFS_FILE), b"r").unwrap();
        let mut value = serde_json::json!({
            "id": "python-stdlib-py314-v1", "protocol": 1, "kernel": "vmlinux",
            "rootfs": "rootfs.squashfs", "agent_version": "0.1.0",
            "kernel_sha256": "0545ba1781fc06cfa1d7699069057f4538103fd1644100cf0da434899a1ed447",
            "built_from": "test",
            "interpreter": {
                "version": "3.14.8",
                "source_sha256": "c2215904f02b175596dc49351585104f4bc20341e1c47378b26a2c274360ce73",
                "pyenv_commit": "3787bacc9188d76ba7ca24c23e26afb1a841a543"
            }
        });
        let write = |v: &serde_json::Value| {
            fs::write(
                dir.path().join("image.json"),
                serde_json::to_vec(v).unwrap(),
            )
            .unwrap()
        };
        write(&value);
        let manifest = read_image(dir.path()).unwrap();
        assert_eq!(
            serde_json::to_value(manifest).unwrap()["interpreter"],
            value["interpreter"]
        );
        value["interpreter"]["source_sha256"] = serde_json::json!("invalid");
        write(&value);
        assert!(read_image(dir.path()).is_err());
    }
}
