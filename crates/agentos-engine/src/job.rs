//! The job directory: the only channel between the controller and a supervisor.
//!
//! A job lives in `<jobs_root>/<effect_id>-<attempt_id>/`:
//!
//! ```text
//! request.json   what to run (written once, before the supervisor starts)
//! lock           advisory lock held by the supervisor for its whole life
//! status.json    supervisor progress, rewritten atomically
//! groups         append-only process-group ids, one per line
//! cancel         marker: the controller asks the supervisor to stop the worker
//! receipt.json + output.bin   the worker's receipt, once it is durable
//! outcome.json + outcome.bin  the controller-visible outcome
//! ```
//!
//! Every file has exactly one writer, which is why temp-file names are fixed:
//! `request.json`/`lock`/`cancel` belong to the controller, `status.json`/`receipt.json`/
//! `output.bin` to the supervisor, `outcome.json`/`outcome.bin`/`groups` to the worker.
//!
//! Liveness is the advisory lock, never a pid: pids are reused, a lock dies with the
//! process (and with every process that inherited the descriptor).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::{Digest, TaskId};
use serde::{Deserialize, Serialize};

use agentos_core::guest::is_attempt_token;

use crate::executor::ExecOutcome;
use crate::firecracker::FirecrackerConfig;
use crate::guestlink::GuestLauncher;
use crate::jail::JailMode;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRequest {
    pub effect_id: EffectId,
    pub task_id: TaskId,
    pub kind: EffectKind,
    pub payload: Vec<u8>,
    pub contract: Contract,
    pub attempt_id: AttemptId,
    pub lease_generation: u64,
    pub lease_expiry_ms: i64,
    pub task_deadline_ms: i64,
    pub worker: WorkerConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "a handful per process, serialized into request.json"
)]
pub enum WorkerConfig {
    Host(HostConfig),
    Scripted(ScriptedConfig),
    Firecracker(FirecrackerConfig),
}

impl WorkerConfig {
    /// Every path must be absolute (the supervisor and the worker run with their own
    /// working directory) and a Firecracker attempt token must be well formed.
    pub fn check_paths(&self) -> io::Result<()> {
        let mut paths: Vec<(&str, &Path)> = Vec::new();
        match self {
            WorkerConfig::Host(h) => {
                paths.extend([
                    ("snapshot_dir", h.snapshot_dir.as_path()),
                    ("profile_dir", h.profile_dir.as_path()),
                    ("work_root", h.work_root.as_path()),
                ]);
            }
            WorkerConfig::Scripted(_) => {}
            WorkerConfig::Firecracker(f) => {
                paths.extend([
                    ("firecracker_bin", f.firecracker_bin.as_path()),
                    ("image_dir", f.image_dir.as_path()),
                    ("snapshot_dir", f.snapshot_dir.as_path()),
                    ("profile_dir", f.profile_dir.as_path()),
                    ("work_root", f.work_root.as_path()),
                ]);
                if let JailMode::Jailed(j) = &f.jail {
                    paths.extend([
                        ("jailer_bin", j.jailer_bin.as_path()),
                        ("cgroup_root", j.cgroup_root.as_path()),
                    ]);
                }
                if let GuestLauncher::Real { firecracker_bin } = &f.launcher {
                    paths.push(("launcher firecracker_bin", firecracker_bin.as_path()));
                }
                if !is_attempt_token(&f.attempt_token) {
                    return Err(invalid(
                        "attempt_token must be 32 lowercase hex characters".into(),
                    ));
                }
            }
        }
        for (name, p) in paths {
            if !p.is_absolute() {
                return Err(invalid(format!("{name} {} must be absolute", p.display())));
            }
        }
        // One binary: the launcher's copy of the path must not point elsewhere.
        if let WorkerConfig::Firecracker(f) = self
            && let GuestLauncher::Real { firecracker_bin } = &f.launcher
            && *firecracker_bin != f.firecracker_bin
        {
            return Err(invalid(format!(
                "launcher firecracker_bin {} differs from firecracker_bin {}",
                firecracker_bin.display(),
                f.firecracker_bin.display()
            )));
        }
        Ok(())
    }
}

/// All paths must be absolute: the supervisor runs with its own working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    pub snapshot_dir: PathBuf,
    pub profile_dir: PathBuf,
    pub work_root: PathBuf,
    pub verify_timeout_secs: u64,
    pub profile_digest: Option<Digest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptedConfig {
    pub script: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    Starting,
    Running,
    Exited,
    Killed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KillReason {
    Lease,
    Deadline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStatus {
    pub state: JobState,
    pub reason: Option<KillReason>,
    pub supervisor_pid: Option<u32>,
    pub worker_pgid: Option<i32>,
    /// Unix milliseconds, filled by the writer.
    pub updated_ms: i64,
}

#[derive(Debug, Clone)]
pub struct JobDir {
    pub path: PathBuf,
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Same rule as the contract's plain names: one path component, nothing that escapes.
pub(crate) fn check_plain_name(field: &str, v: &str) -> io::Result<()> {
    let plain = !v.is_empty()
        && v != "."
        && v != ".."
        && !v.starts_with('-')
        && !v.contains(['/', '\\', '\0']);
    if plain {
        Ok(())
    } else {
        Err(invalid(format!(
            "{field} {v:?} must be a single plain name"
        )))
    }
}

pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Write `bytes` to `path` so a reader sees the old file or the new one, never a mix,
/// and so the new one survives a crash once this returns.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| invalid(format!("{} has no parent", path.display())))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid(format!("{} has no file name", path.display())))?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    sync_dir(dir)
}

fn json<T: Serialize>(v: &T) -> io::Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(io::Error::other)
}

impl JobDir {
    /// Creates the job directory, writes `request.json`, and takes the exclusive lock
    /// on `lock`. The returned file holds that lock: hand it to the supervisor, which
    /// keeps it for its whole life.
    pub fn create(jobs_root: &Path, req: &JobRequest) -> io::Result<(JobDir, File)> {
        check_plain_name("effect id", req.effect_id.as_str())?;
        let attempt = req.attempt_id.to_string();
        check_plain_name("attempt id", &attempt)?;
        req.worker.check_paths()?;
        JobDir::create_with(jobs_root, req, |_| Ok(()))
    }

    /// `create` with a hook run after the directory exists; any failure from there on
    /// removes the directory again, so a failed create leaves nothing behind.
    fn create_with(
        jobs_root: &Path,
        req: &JobRequest,
        after_dir: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<(JobDir, File)> {
        fs::create_dir_all(jobs_root)?;
        let path = jobs_root.join(format!("{}-{}", req.effect_id, req.attempt_id));
        fs::create_dir(&path)?;
        let built = (|| {
            sync_dir(jobs_root)?;
            after_dir(&path)?;
            let lock = File::create(path.join("lock"))?;
            lock.lock()?;
            let job = JobDir { path: path.clone() };
            atomic_write(&job.path.join("request.json"), &json(req)?)?;
            Ok((job, lock))
        })();
        if built.is_err() {
            let _ = fs::remove_dir_all(&path);
        }
        built
    }

    pub fn open(path: &Path) -> io::Result<JobDir> {
        if !path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a job directory", path.display()),
            ));
        }
        Ok(JobDir {
            path: path.to_path_buf(),
        })
    }

    pub fn request(&self) -> io::Result<JobRequest> {
        let bytes = fs::read(self.path.join("request.json"))?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }

    pub fn write_status(&self, status: &JobStatus) -> io::Result<()> {
        atomic_write(&self.path.join("status.json"), &json(status)?)
    }

    /// `None` when absent, torn or unparseable.
    pub fn read_status(&self) -> Option<JobStatus> {
        serde_json::from_slice(&fs::read(self.path.join("status.json")).ok()?).ok()
    }

    pub fn write_receipt(&self, out: &ExecOutcome) -> io::Result<()> {
        self.write_pair("output.bin", "receipt.json", out)
    }

    pub fn read_receipt(&self) -> Option<ExecOutcome> {
        self.read_pair("output.bin", "receipt.json")
    }

    pub fn write_outcome(&self, out: &ExecOutcome) -> io::Result<()> {
        self.write_pair("outcome.bin", "outcome.json", out)
    }

    pub fn read_outcome(&self) -> Option<ExecOutcome> {
        self.read_pair("outcome.bin", "outcome.json")
    }

    /// The output bytes go first, then the JSON (with an empty `output`) that vouches
    /// for them by digest: the JSON is the commit point.
    fn write_pair(&self, bin: &str, meta: &str, out: &ExecOutcome) -> io::Result<()> {
        atomic_write(&self.path.join(bin), &out.output)?;
        let mut head = out.clone();
        head.output = Vec::new();
        atomic_write(&self.path.join(meta), &json(&head)?)
    }

    fn read_pair(&self, bin: &str, meta: &str) -> Option<ExecOutcome> {
        let head = match fs::read(self.path.join(meta)) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!(job = %self.path.display(), file = meta, error = %e, "unreadable job file");
                return None;
            }
        };
        let mut out: ExecOutcome = match serde_json::from_slice(&head) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!(job = %self.path.display(), file = meta, error = %e, "unparseable job file");
                return None;
            }
        };
        out.output = match fs::read(self.path.join(bin)) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(job = %self.path.display(), file = bin, error = %e, "missing job output");
                return None;
            }
        };
        if out.receipt.result_digest != Some(Digest::of(&out.output)) {
            tracing::warn!(job = %self.path.display(), file = bin, "job output does not match its digest");
            return None;
        }
        Some(out)
    }

    pub fn drop_cancel(&self) -> io::Result<()> {
        atomic_write(&self.path.join("cancel"), b"")
    }

    pub fn cancel_requested(&self) -> bool {
        self.path.join("cancel").exists()
    }

    pub fn record_group(&self, pgid: i32) -> io::Result<()> {
        append_group(&self.path.join("groups"), pgid)
    }

    pub fn groups(&self) -> Vec<i32> {
        fs::read_to_string(self.path.join("groups"))
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Whether some process holds the job's lock. Probes through a fresh descriptor with
    /// a shared lock (so concurrent observers never see each other as holders; the
    /// supervisor's exclusive lock still conflicts). Only a missing lock file means
    /// "nobody"; any other failure to probe answers held, because an unknown answer must
    /// never let recovery redispatch beside a live worker.
    pub fn lock_held(&self) -> bool {
        probe_lock(
            &self.path.join("lock"),
            |p| File::open(p),
            |f| f.try_lock_shared(),
        )
    }

    /// No further effect can come from this job: it reported a terminal state, left a
    /// valid receipt, or nobody holds its lock. No pid is consulted.
    pub fn is_dead(&self) -> bool {
        self.is_dead_with(|| self.lock_held())
    }

    fn is_dead_with(&self, lock_held: impl Fn() -> bool) -> bool {
        if matches!(
            self.read_status().map(|s| s.state),
            Some(JobState::Exited | JobState::Killed)
        ) {
            return true;
        }
        self.read_receipt().is_some() || !lock_held()
    }

    /// Every attempt for `effect`, ascending by lease generation (unreadable requests
    /// first), ignoring half-created `.tmp` entries. Fails closed: a root (or an entry) that
    /// cannot be read is an error, never "no job", since a job may be running in it.
    pub fn list(jobs_root: &Path, effect: &EffectId) -> io::Result<Vec<JobDir>> {
        let prefix = format!("{effect}-");
        let mut found: Vec<(Option<u64>, JobDir)> = Vec::new();
        for entry in fs::read_dir(jobs_root)? {
            let entry = entry?;
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|n| n.starts_with(&prefix) && !n.contains(".tmp"))
            {
                continue;
            }
            let job = JobDir { path: entry.path() };
            match job.request().ok() {
                // The prefix alone also matches effect `a-b` when asked for `a`.
                Some(req) if req.effect_id != *effect => {}
                req => found.push((req.map(|r| r.lease_generation), job)),
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        Ok(found.into_iter().map(|(_, j)| j).collect())
    }
}

/// Appends `pgid` to a `groups` file and makes it durable before returning, so the
/// supervisor can kill the group even if this process dies right after.
pub fn append_group(path: &Path, pgid: i32) -> io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{pgid}\n").as_bytes())?;
    f.sync_all()
}

fn probe_lock(
    path: &Path,
    open: fn(&Path) -> io::Result<File>,
    try_lock: fn(&File) -> Result<(), TryLockError>,
) -> bool {
    let file = match open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(e) => {
            tracing::warn!(job = %path.display(), kind = ?e.kind(), "cannot open job lock; assuming held");
            return true;
        }
    };
    match try_lock(&file) {
        Ok(()) => false, // released again when `file` drops
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(e)) => {
            tracing::warn!(job = %path.display(), kind = ?e.kind(), "cannot probe job lock; assuming held");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> JobRequest {
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
            worker: WorkerConfig::Scripted(ScriptedConfig { script: "x".into() }),
        }
    }

    #[test]
    fn an_unknown_probe_answer_is_held_never_nobody() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lock");
        let denied: fn(&Path) -> io::Result<File> =
            |_| Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(probe_lock(&p, denied, |_| Ok(())));
        let missing: fn(&Path) -> io::Result<File> =
            |_| Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(!probe_lock(&p, missing, |_| Ok(())));
        File::create(&p).unwrap();
        let enolck: fn(&File) -> Result<(), TryLockError> =
            |_| Err(TryLockError::Error(io::Error::from_raw_os_error(37)));
        assert!(probe_lock(&p, |p| File::open(p), enolck));
        assert!(!probe_lock(&p, |p| File::open(p), |f| f.try_lock_shared()));
    }

    #[test]
    fn is_dead_is_false_when_the_probe_cannot_tell() {
        let root = tempfile::tempdir().unwrap();
        let (job, lock) = JobDir::create(root.path(), &req()).unwrap();
        drop(lock);
        // A process another test spawns in this window inherits the lock's descriptor until
        // it execs, holding the flock that long: wait for the release instead of racing it.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !job.is_dead() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(job.is_dead());
        let unknown = || {
            probe_lock(
                &job.path.join("lock"),
                |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                |_| Ok(()),
            )
        };
        assert!(!job.is_dead_with(unknown));
    }

    #[test]
    fn a_failed_create_leaves_no_directory() {
        let root = tempfile::tempdir().unwrap();
        let r = req();
        let err = JobDir::create_with(root.path(), &r, |_| Err(io::Error::other("boom")))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        JobDir::create(root.path(), &r).unwrap();
    }
}
