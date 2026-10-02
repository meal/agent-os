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
//! Liveness is the advisory lock, never a pid: pids are reused, a lock dies with the
//! process (and with every process that inherited the descriptor).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::effect::{AttemptId, EffectId, EffectKind};
use agentos_core::ids::{Digest, TaskId};
use serde::{Deserialize, Serialize};

use crate::executor::ExecOutcome;

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
pub enum WorkerConfig {
    Host(HostConfig),
    Scripted(ScriptedConfig),
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
fn check_plain_name(field: &str, v: &str) -> io::Result<()> {
    let plain = !v.is_empty()
        && v != "."
        && v != ".."
        && !v.starts_with('-')
        && !v.contains(['/', '\\', '\0']);
    if plain {
        Ok(())
    } else {
        Err(invalid(format!("{field} {v:?} must be a single plain name")))
    }
}

pub fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Write `bytes` to `path` so a reader sees the old file or the new one, never a mix,
/// and so the new one survives a crash once this returns.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| invalid(format!("{} has no parent", path.display())))?;
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
        if let WorkerConfig::Host(h) = &req.worker {
            for (name, p) in [
                ("snapshot_dir", &h.snapshot_dir),
                ("profile_dir", &h.profile_dir),
                ("work_root", &h.work_root),
            ] {
                if !p.is_absolute() {
                    return Err(invalid(format!("{name} {} must be absolute", p.display())));
                }
            }
        }
        fs::create_dir_all(jobs_root)?;
        let path = jobs_root.join(format!("{}-{attempt}", req.effect_id));
        fs::create_dir(&path)?;
        let lock = File::create(path.join("lock"))?;
        lock.lock()?;
        let job = JobDir { path };
        atomic_write(&job.path.join("request.json"), &json(req)?)?;
        Ok((job, lock))
    }

    pub fn open(path: &Path) -> io::Result<JobDir> {
        if !path.is_dir() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not a job directory", path.display())));
        }
        Ok(JobDir { path: path.to_path_buf() })
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
        let mut f = OpenOptions::new().create(true).append(true).open(self.path.join("groups"))?;
        f.write_all(format!("{pgid}\n").as_bytes())?;
        f.sync_all()
    }

    pub fn groups(&self) -> Vec<i32> {
        fs::read_to_string(self.path.join("groups"))
            .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
            .unwrap_or_default()
    }

    /// Whether some process holds the job's lock. Probes through a fresh descriptor, so
    /// a lock held by this very process through another descriptor still counts.
    pub fn lock_held(&self) -> bool {
        let Ok(file) = File::open(self.path.join("lock")) else { return false };
        match file.try_lock() {
            Ok(()) => false, // released again when `file` drops
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(e)) => {
                tracing::warn!(job = %self.path.display(), error = %e, "cannot probe job lock");
                false
            }
        }
    }

    /// No further effect can come from this job: it reported a terminal state, left a
    /// valid receipt, or nobody holds its lock. No pid is consulted.
    pub fn is_dead(&self) -> bool {
        if matches!(self.read_status().map(|s| s.state), Some(JobState::Exited | JobState::Killed)) {
            return true;
        }
        self.read_receipt().is_some() || !self.lock_held()
    }

    /// Every attempt for `effect`, ascending by lease generation (unreadable requests
    /// first), ignoring half-created `.tmp` entries.
    pub fn list(jobs_root: &Path, effect: &EffectId) -> Vec<JobDir> {
        let prefix = format!("{effect}-");
        let Ok(entries) = fs::read_dir(jobs_root) else { return Vec::new() };
        let mut found: Vec<(Option<u64>, JobDir)> = entries
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                name.to_str().is_some_and(|n| n.starts_with(&prefix) && !n.contains(".tmp"))
            })
            .map(|e| JobDir { path: e.path() })
            .map(|j| (j.request().ok().map(|r| r.lease_generation), j))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        found.into_iter().map(|(_, j)| j).collect()
    }
}
