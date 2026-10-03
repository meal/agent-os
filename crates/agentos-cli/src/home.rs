//! The agentos home directory:
//!
//! ```text
//! <home>/agentos.db          journal (SQLite, WAL)
//! <home>/blobs/              content-addressed artifacts
//! <home>/jobs/               one directory per effect attempt: request, status, receipt (see agentos-engine job.rs)
//! <home>/work/<task>/ws      task workspaces
//! <home>/tasks/<task>/       inputs recorded at submission: snapshot/, profile/, agent.patch
//! <home>/driver.lock         held by the one process driving tasks (running or recovering)
//! <home>/registry/<id>@<digest>/   registered verification profiles, read-only, content-addressed
//! <home>/registry/<id>@<digest>.meta.json   registration time (outside the digest)
//! <home>/registry/images/<id>@<digest>/   registered guest images, read-only, content-addressed
//! <home>/profiles/<id>/      legacy profile directories (default for --profiles)
//! <home>/inspect/<task>/     the Firecracker inspector's per-boot directories
//! <home>/bin/firecracker     default Firecracker binary (`jailer` next to it)
//! ```

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use agentos_core::contract::Limits;
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::firecracker::{preflight, FirecrackerConfig};
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::jail::{self, JailConfig, JailDecision, JailMode, JAIL_GID, JAIL_UID};
use agentos_engine::job::{HostConfig, WorkerConfig};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;
use serde_json::Value;

use crate::args::WorkerKind;
use crate::commands::registry::{check_id, list_entries};
use crate::commands::supervise::supervisor_cmd;
use crate::error::CliError;

/// How long a verification check may run (the fixture executor's default).
const VERIFY_TIMEOUT_SECS: u64 = 60;
/// Test switches, each honoured only together with `AGENTOS_TEST_WORKERS=1`.
const TEST_WORKERS_ENV: &str = "AGENTOS_TEST_WORKERS";
/// The Firecracker worker launches the fake guest (`agentos supervise fake-guest`) instead of
/// Firecracker.
const FAKE_GUEST_ENV: &str = "AGENTOS_TEST_FAKE_GUEST";
/// `ok` | `fail:<reason>`: the jail probe's answer, instead of looking at the host.
const JAIL_PROBE_ENV: &str = "AGENTOS_TEST_JAIL_PROBE";
/// Where the cgroup v2 hierarchy is when `/proc/mounts` names none (the probe then refuses).
const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// A profile registered with `agentos profile register`.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: String,
    pub digest: String,
    pub dir: PathBuf,
    pub registered_ms: i64,
}

pub struct Home {
    pub root: PathBuf,
    pub profiles: PathBuf,
    /// `--worker`: `submit` uses it (host when absent); every other command checks it
    /// against the recorded worker.
    pub worker: Option<WorkerKind>,
    /// `--firecracker` [default: `<home>/bin/firecracker`].
    pub firecracker: Option<PathBuf>,
    /// `--jailer` [default: `jailer` next to the Firecracker binary].
    pub jailer: Option<PathBuf>,
    pub jail_uid: u32,
    pub jail_gid: u32,
    /// `--allow-unjailed`: consulted at `submit` only; later commands follow the record.
    pub allow_unjailed: bool,
}

/// The worker recorded in a task's `Submitted` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedWorker {
    pub kind: WorkerKind,
    /// `(guest_image_id, guest_image_digest)`; `None` for host (and 3a) tasks.
    pub image: Option<(String, Digest)>,
    /// `None` for host (and 3a) tasks.
    pub jailed: Option<bool>,
}

/// A Firecracker worker configuration that passed the preflight and the jail decision.
pub struct PreparedFirecracker {
    pub cfg: FirecrackerConfig,
    /// The decision: true only when the probe passed.
    pub jailed: bool,
}

fn env_on(name: &str) -> bool {
    std::env::var(name).as_deref() == Ok("1")
}

/// The jail probe test hook: `Ok(None)` unless `AGENTOS_TEST_WORKERS=1` and
/// `AGENTOS_TEST_JAIL_PROBE` are both set; then `ok` ⇒ `Ok(())`, `fail:<reason>` ⇒
/// `Err(reason)`, anything else is a usage error. `get` reads the environment.
pub fn probe_hook(get: impl Fn(&str) -> Option<String>) -> Result<Option<Result<(), String>>, CliError> {
    if get(TEST_WORKERS_ENV).as_deref() != Some("1") {
        return Ok(None);
    }
    match get(JAIL_PROBE_ENV).as_deref() {
        None => Ok(None),
        Some("ok") => Ok(Some(Ok(()))),
        Some(v) => match v.strip_prefix("fail:") {
            Some(reason) => Ok(Some(Err(reason.to_string()))),
            None => Err(CliError::usage(format!("{JAIL_PROBE_ENV}={v:?}: expected ok or fail:<reason>"))),
        },
    }
}

/// The cgroup v2 mount point `/proc/mounts` names: the one root the probe checks, the
/// worker's `collect` cleans and the real jailer writes to.
fn cgroup_root() -> PathBuf {
    cgroup_root_from(fs::read_to_string("/proc/mounts").ok().as_deref())
}

/// `cgroup_root` over `/proc/mounts` text (`None`: unreadable).
fn cgroup_root_from(proc_mounts: Option<&str>) -> PathBuf {
    proc_mounts.and_then(jail::find_cgroup2_root).unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_ROOT))
}

pub struct Store {
    pub db: Db,
    pub blobs: BlobStore,
}

/// Exclusive right to drive tasks in a home, released when dropped or when the process dies.
///
/// Recovery collects unreferenced blobs, which is only safe while nothing else writes to the
/// blob store, so every run, resume and recovery holds this lock.
///
/// The holder writes the id of the task it drives into the lock file ([`DriverLock::driving`]),
/// so `cancel` can tell whether the live driver will see its request.
pub struct DriverLock(File);

impl DriverLock {
    /// Records that the holder now drives `task`.
    pub fn driving(&self, task: &TaskId) -> std::io::Result<()> {
        let mut f = &self.0;
        f.set_len(0)?;
        std::io::Seek::rewind(&mut f)?;
        f.write_all(task.as_str().as_bytes())
    }
}

impl Home {
    pub fn new(home: Option<PathBuf>, profiles: Option<PathBuf>) -> Result<Home, CliError> {
        let root = match home {
            Some(h) => h,
            None => {
                let user = std::env::var_os("HOME").ok_or_else(|| CliError::usage("HOME is not set; pass --home DIR"))?;
                Path::new(&user).join(".agentos")
            }
        };
        let profiles = profiles.unwrap_or_else(|| root.join("profiles"));
        Ok(Home {
            root,
            profiles,
            worker: None,
            firecracker: None,
            jailer: None,
            jail_uid: JAIL_UID,
            jail_gid: JAIL_GID,
            allow_unjailed: false,
        })
    }

    /// The Firecracker binary: `--firecracker`, else `<home>/bin/firecracker`.
    pub fn firecracker_bin(&self) -> PathBuf {
        self.firecracker.clone().unwrap_or_else(|| self.root.join("bin/firecracker"))
    }

    /// The jailer: `--jailer`, else the file `jailer` next to the Firecracker binary.
    pub fn jailer_bin(&self) -> PathBuf {
        self.jailer.clone().unwrap_or_else(|| self.firecracker_bin().with_file_name("jailer"))
    }

    /// The registry directory of profile `id`, which must be one plain name naming a
    /// directory really inside the registry (a symlink leading out of it is refused).
    /// `Ok(None)` when there is no such profile.
    pub fn profile_dir(&self, id: &str) -> Result<Option<PathBuf>, CliError> {
        let mut parts = Path::new(id).components();
        if !matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None)) || id.contains(['/', '\\', '\0']) {
            return Err(CliError::usage(format!("verification profile id {id:?} is not a single plain name")));
        }
        let dir = self.profiles.join(id);
        let Ok(real) = dir.canonicalize() else { return Ok(None) };
        let root = self.profiles.canonicalize()?;
        if real.parent() != Some(root.as_path()) {
            return Err(CliError::usage(format!("verification profile {id} resolves to {}, outside the profile registry {}", real.display(), root.display())));
        }
        Ok(real.join("profile.json").is_file().then_some(real))
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.root.join("registry")
    }

    /// Registered entries of profile `id`, oldest registration first.
    pub fn registry_entries(&self, id: &str) -> Vec<RegistryEntry> {
        let mut found: Vec<RegistryEntry> = self.registry_list().into_iter().filter(|e| e.id == id).collect();
        found.sort_by(|a, b| (a.registered_ms, &a.digest).cmp(&(b.registered_ms, &b.digest)));
        found
    }

    /// Every registered profile, unordered.
    pub fn registry_list(&self) -> Vec<RegistryEntry> {
        list_entries(&self.registry_dir(), |dir| dir.join("profile.json").is_file())
    }

    /// The guest image registry: `<home>/registry/images`.
    pub fn images_dir(&self) -> PathBuf {
        self.registry_dir().join("images")
    }

    /// Every registered guest image, unordered.
    pub fn image_list(&self) -> Vec<RegistryEntry> {
        list_entries(&self.images_dir(), |dir| dir.join("image.json").is_file())
    }

    /// Registered entries of guest image `id`, oldest registration first.
    pub fn image_entries(&self, id: &str) -> Vec<RegistryEntry> {
        let mut found: Vec<RegistryEntry> = self.image_list().into_iter().filter(|e| e.id == id).collect();
        found.sort_by(|a, b| (a.registered_ms, &a.digest).cmp(&(b.registered_ms, &b.digest)));
        found
    }

    /// Where guest image `id` comes from: the registry entry with exactly the digest `pin`
    /// (none is an error: a pin never falls back to anything else); else the newest entry;
    /// there is no legacy location. `Ok(None)` when there is none.
    pub fn resolve_image(&self, id: &str, pin: Option<&str>) -> Result<Option<RegistryEntry>, CliError> {
        check_id("guest image", id)?;
        if let Some(pin) = pin {
            return match self.image_entries(id).into_iter().find(|e| e.digest == pin) {
                Some(entry) => Ok(Some(entry)),
                None => Err(CliError::usage(format!("guest image {id}@{pin} is not in the registry; `agentos image register` it first"))),
            };
        }
        Ok(self.image_entries(id).pop())
    }

    /// Where profile `id` comes from, in order: the registry entry with exactly the digest
    /// `pin` (none is an error: a pin never falls back to anything else); else the newest
    /// registry entry for `id`; else the legacy `<profiles>/<id>/` directory. `Ok(None)`
    /// when there is none.
    pub fn resolve_profile(&self, id: &str, pin: Option<&str>) -> Result<Option<PathBuf>, CliError> {
        if let Some(pin) = pin {
            return match self.registry_entries(id).into_iter().find(|e| e.digest == pin) {
                Some(entry) => Ok(Some(entry.dir)),
                None => Err(CliError::usage(format!("verification profile {id}@{pin} is not in the registry; `agentos profile register` it first"))),
            };
        }
        if let Some(newest) = self.registry_entries(id).pop() {
            return Ok(Some(newest.dir));
        }
        self.profile_dir(id)
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.root.join("tasks")
    }

    pub fn task_dir(&self, task: &TaskId) -> PathBuf {
        self.tasks_dir().join(task.as_str())
    }

    /// Opens the journal and blob store, creating the home on first use.
    pub fn open(&self) -> Result<Store, CliError> {
        fs::create_dir_all(self.tasks_dir())?;
        Ok(Store { db: Db::open(&self.root.join("agentos.db"))?, blobs: BlobStore::open(self.root.join("blobs"))? })
    }

    /// The driver lock, or `None` if another process holds it.
    pub fn try_lock(&self) -> Result<Option<DriverLock>, CliError> {
        let file = File::options().create(true).truncate(false).write(true).open(self.root.join("driver.lock"))?;
        match file.try_lock() {
            Ok(()) => {
                file.set_len(0)?;
                Ok(Some(DriverLock(file)))
            }
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// The task the current lock holder said it drives, if any.
    pub fn driven_task(&self) -> Option<String> {
        fs::read_to_string(self.root.join("driver.lock")).ok().filter(|s| !s.is_empty())
    }

    pub fn lock(&self) -> Result<DriverLock, CliError> {
        self.try_lock()?.ok_or_else(|| {
            CliError::other(format!("another agentos process is driving tasks in {}; try again when it finishes", self.root.display()))
        })
    }

    /// The profile digest recorded when `task` was submitted: what every verification is
    /// pinned to, whatever later happens to the staged copy.
    fn pinned_profile(&self, store: &Store, task: &TaskId) -> Result<Digest, CliError> {
        let events = store.db.events(task)?;
        let recorded = events.iter().find(|e| e.event_type == "Submitted").and_then(|e| e.payload["profile_digest"].as_str().map(str::to_string));
        recorded
            .and_then(|d| Digest::from_hex(&d).ok())
            .ok_or_else(|| CliError::other(format!("task {task} has no recorded profile digest; it was not completely submitted")))
    }

    /// The worker recorded at `task`'s submission. No `worker` field (a 3a task, or no
    /// `Submitted` event at all) is the host worker.
    pub fn recorded_worker(&self, store: &Store, task: &TaskId) -> Result<RecordedWorker, CliError> {
        let events = store.db.events(task)?;
        let host = RecordedWorker { kind: WorkerKind::Host, image: None, jailed: None };
        let Some(submitted) = events.iter().find(|e| e.event_type == "Submitted") else { return Ok(host) };
        let p = &submitted.payload;
        match p.get("worker").and_then(Value::as_str) {
            None | Some("host") => Ok(host),
            Some("firecracker") => {
                let broken = |what: &str| CliError::other(format!("task {task} was submitted to the firecracker worker without a valid recorded {what}"));
                let id = p["guest_image_id"].as_str().ok_or_else(|| broken("guest_image_id"))?.to_string();
                let digest = p["guest_image_digest"].as_str().and_then(|d| Digest::from_hex(d).ok()).ok_or_else(|| broken("guest_image_digest"))?;
                let jailed = p["jailed"].as_bool().ok_or_else(|| broken("jailed"))?;
                Ok(RecordedWorker { kind: WorkerKind::Firecracker, image: Some((id, digest)), jailed: Some(jailed) })
            }
            Some(other) => Err(CliError::other(format!("task {task} was submitted with an unknown worker {:?}", other))),
        }
    }

    /// `task`'s recorded worker, checked against `--worker`: a flag naming another worker
    /// exits 2 (an unknown task is reported as such first).
    pub fn task_worker(&self, store: &Store, task: &TaskId) -> Result<RecordedWorker, CliError> {
        store.db.task(task)?;
        let recorded = self.recorded_worker(store, task)?;
        match self.worker {
            Some(flag) if flag != recorded.kind => Err(CliError::usage(format!("task was submitted with worker {}", recorded.kind.as_str()))),
            _ => Ok(recorded),
        }
    }

    /// The guest launcher: the fake guest (this executable, `supervise fake-guest`) with
    /// `AGENTOS_TEST_WORKERS=1` and `AGENTOS_TEST_FAKE_GUEST=1`, else Firecracker.
    fn launcher(&self) -> Result<GuestLauncher, CliError> {
        if env_on(TEST_WORKERS_ENV) && env_on(FAKE_GUEST_ENV) {
            return Ok(GuestLauncher::Fake { program: std::env::current_exe()?, prefix_args: vec!["supervise".into()] });
        }
        Ok(GuestLauncher::Real { firecracker_bin: std::path::absolute(self.firecracker_bin())? })
    }

    /// The Firecracker worker configuration for a task whose recorded inputs are (or will be)
    /// in `task_dir`, on the registered `image` with the contract's `limits`: the preflight
    /// (`firecracker worker unavailable: …`, exit 1) and then the jail. `recorded_jailed` is
    /// `None` at `submit` (decided here from the probe and `--allow-unjailed`) and the
    /// recorded value for every later command, which it binds: a task submitted jailed never
    /// runs unjailed, one submitted unjailed stays so without the flag or the warning.
    pub fn prepare_firecracker(
        &self,
        image: &RegistryEntry,
        limits: &Limits,
        task_dir: &Path,
        profile_digest: Option<Digest>,
        recorded_jailed: Option<bool>,
    ) -> Result<PreparedFirecracker, CliError> {
        let root = std::path::absolute(&self.root)?;
        let image_digest = Digest::from_hex(&image.digest).map_err(|e| CliError::other(format!("guest image {}@{}: {e}", image.id, image.digest)))?;
        let launcher = self.launcher()?;
        let mut cfg = FirecrackerConfig {
            firecracker_bin: std::path::absolute(self.firecracker_bin())?,
            image_dir: std::path::absolute(&image.dir)?,
            image_digest,
            snapshot_dir: task_dir.join("snapshot"),
            profile_dir: task_dir.join("profile"),
            profile_digest,
            work_root: root.join("work"),
            verify_timeout_secs: VERIFY_TIMEOUT_SECS,
            vcpus: limits.worker_vcpus,
            memory_mib: limits.worker_memory_mib,
            // Minted per job by the executor (and per boot by the inspector).
            attempt_token: String::new(),
            launcher,
            jail: JailMode::Unjailed,
        };
        // The registry entry itself is validated: manifest, files, and its digest now.
        preflight(&cfg).map_err(|e| CliError::other(format!("firecracker worker unavailable: {e}")))?;
        let (mode, jailed) = self.decide_jail(&cfg, &root, recorded_jailed)?;
        cfg.jail = mode;
        Ok(PreparedFirecracker { cfg, jailed })
    }

    /// The jail configuration: the jailer, the jail uid/gid, and the cgroup root `/proc/mounts`
    /// names (the one the probe checks, `collect` cleans and the real jailer writes to).
    fn jail_config(&self) -> std::io::Result<JailConfig> {
        Ok(JailConfig {
            jailer_bin: std::path::absolute(self.jailer_bin())?,
            uid: self.jail_uid,
            gid: self.jail_gid,
            cgroup_root: cgroup_root(),
        })
    }

    /// The jail of `cfg`'s VMs and whether it counts as jailed (what `Submitted.jailed`
    /// records). The `Fake` launcher is never probed and never really jailed: without the
    /// probe hook it is unjailed; with it, the hook's answer drives the decision and the record
    /// exactly as a real probe would, while the configuration stays unjailed.
    fn decide_jail(&self, cfg: &FirecrackerConfig, root: &Path, recorded: Option<bool>) -> Result<(JailMode, bool), CliError> {
        if recorded == Some(false) {
            return Ok((JailMode::Unjailed, false));
        }
        let jail_cfg = self.jail_config()?;
        let probe = match (probe_hook(|k| std::env::var(k).ok())?, &cfg.launcher) {
            (Some(answer), _) => Some(answer),
            (None, GuestLauncher::Fake { .. }) => None,
            (None, GuestLauncher::Real { .. }) => {
                Some(jail::probe(&jail_cfg, &root.join("jobs"), &root.join("inspect"), &root.join("work"), &cfg.image_dir))
            }
        };
        let jailed_mode = || match cfg.launcher {
            GuestLauncher::Real { .. } => JailMode::Jailed(jail_cfg.clone()),
            GuestLauncher::Fake { .. } => JailMode::Unjailed,
        };
        if recorded == Some(true) {
            return match probe {
                Some(Ok(())) => Ok((jailed_mode(), true)),
                Some(Err(reason)) => Err(CliError::other(format!("task was submitted jailed: jailer unavailable: {reason}"))),
                None => Err(CliError::other("task was submitted jailed: jailer unavailable: the fake guest launcher is never jailed")),
            };
        }
        let Some(probe) = probe else { return Ok((JailMode::Unjailed, false)) };
        match jail::decide(probe, self.allow_unjailed) {
            Err(refusal) => Err(CliError::other(format!("firecracker worker unavailable: {refusal}"))),
            Ok(JailDecision::Unjailed { reason }) => {
                eprintln!("warning: running Firecracker unjailed: {reason}");
                Ok((JailMode::Unjailed, false))
            }
            Ok(JailDecision::Jailed) => Ok((jailed_mode(), true)),
        }
    }

    /// The executor for `task`, over the inputs and the worker recorded at its submission.
    /// For the Firecracker worker the preflight and the jail are checked here, before the
    /// caller touches the task.
    pub fn executor(&self, store: &Store, task: &TaskId) -> Result<SupervisedExecutor, CliError> {
        let recorded = self.task_worker(store, task)?;
        let dir = self.task_dir(task);
        if !dir.join("snapshot").is_dir() || !dir.join("profile").is_dir() {
            return Err(CliError::other(format!("task {task} has no recorded inputs in {}; it was not completely submitted", dir.display())));
        }
        // The supervisor runs in its own working directory: every path is absolute.
        let root = std::path::absolute(&self.root)?;
        let task_dir = root.join("tasks").join(task.as_str());
        let profile_digest = Some(self.pinned_profile(store, task)?);
        let worker = match recorded.kind {
            WorkerKind::Firecracker => {
                let (id, digest) = recorded.image.ok_or_else(|| CliError::other(format!("task {task} has no recorded guest image")))?;
                let entry = self
                    .image_entries(&id)
                    .into_iter()
                    .find(|e| e.digest == digest.to_string())
                    .ok_or_else(|| CliError::other(format!("recorded guest image {id}@{digest} is no longer registered")))?;
                let limits = store.db.contract(task)?.limits;
                WorkerConfig::Firecracker(self.prepare_firecracker(&entry, &limits, &task_dir, profile_digest, recorded.jailed)?.cfg)
            }
            WorkerKind::Host => WorkerConfig::Host(HostConfig {
                snapshot_dir: task_dir.join("snapshot"),
                profile_dir: task_dir.join("profile"),
                work_root: root.join("work"),
                verify_timeout_secs: VERIFY_TIMEOUT_SECS,
                profile_digest,
            }),
        };
        Ok(SupervisedExecutor::new(root.join("jobs"), supervisor_cmd()?, worker, ExecCounts::default())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(home: &Home, id: &str, digest: &str, ms: i64) {
        let name = format!("{id}@{digest}");
        let dir = home.images_dir().join(&name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("image.json"), "{}").unwrap();
        fs::write(home.images_dir().join(format!("{name}.meta.json")), format!("{{\"registered_ms\":{ms}}}")).unwrap();
    }

    #[test]
    fn resolve_image_pins_exactly_else_newest_and_never_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::new(Some(tmp.path().to_path_buf()), None).unwrap();
        let (old, new) = ("a".repeat(64), "b".repeat(64));
        assert!(home.resolve_image("img", None).unwrap().is_none());
        entry(&home, "img", &old, 1);
        entry(&home, "img", &new, 2);
        assert_eq!(home.resolve_image("img", None).unwrap().unwrap().digest, new);
        assert_eq!(home.resolve_image("img", Some(&old)).unwrap().unwrap().digest, old);
        let err = home.resolve_image("img", Some(&"c".repeat(64))).unwrap_err();
        assert_eq!(err.code, 2);
        assert!(err.message.contains("is not in the registry; `agentos image register` it first"), "{}", err.message);
        assert_eq!(home.resolve_image("a@b", None).unwrap_err().code, 2);
        // Profiles and images do not see each other.
        assert!(home.registry_list().is_empty());
    }

    #[test]
    fn jailer_defaults_to_the_sibling_of_the_firecracker_binary() {
        let home = Home { firecracker: Some("/x/firecracker".into()), jailer: None, ..Home::new(Some("/h".into()), None).unwrap() };
        assert_eq!(home.jailer_bin(), Path::new("/x/jailer"));
        let home = Home { firecracker: None, jailer: None, ..Home::new(Some("/h".into()), None).unwrap() };
        assert_eq!(home.firecracker_bin(), Path::new("/h/bin/firecracker"));
        assert_eq!(home.jailer_bin(), Path::new("/h/bin/jailer"));
        let home = Home { firecracker: Some("/x/firecracker".into()), jailer: Some("/y/my-jailer".into()), ..Home::new(Some("/h".into()), None).unwrap() };
        assert_eq!(home.jailer_bin(), Path::new("/y/my-jailer"));
    }

    #[test]
    fn jail_cgroup_root_is_the_cgroup2_mount_point_from_proc_mounts() {
        let host = "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
                    cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate 0 0\n";
        assert_eq!(cgroup_root_from(Some(host)), Path::new("/sys/fs/cgroup"));
        let elsewhere = "proc /proc proc rw 0 0\nnone /mnt/cg\\040two cgroup2 rw 0 0\n";
        assert_eq!(cgroup_root_from(Some(elsewhere)), Path::new("/mnt/cg two"), "the mount point, unescaped");
        let v1_only = "cgroup /sys/fs/cgroup/cpu cgroup rw,cpu 0 0\n";
        assert_eq!(cgroup_root_from(Some(v1_only)), Path::new(DEFAULT_CGROUP_ROOT), "none: the default, which the probe then refuses");
        assert_eq!(cgroup_root_from(None), Path::new(DEFAULT_CGROUP_ROOT));
        // And it is what the jail is built with on this host.
        let mounts = fs::read_to_string("/proc/mounts").ok();
        let home = Home { jailer: Some("/j/jailer".into()), jail_uid: 7, jail_gid: 8, ..Home::new(Some("/h".into()), None).unwrap() };
        let cfg = home.jail_config().unwrap();
        assert_eq!(cfg.cgroup_root, cgroup_root_from(mounts.as_deref()));
        assert_eq!((cfg.jailer_bin.as_path(), cfg.uid, cfg.gid), (Path::new("/j/jailer"), 7, 8));
    }

    #[test]
    fn the_jail_probe_hook_answers_only_with_test_workers() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            move |k: &str| pairs.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone())
        };
        assert_eq!(probe_hook(env(&[("AGENTOS_TEST_JAIL_PROBE", "ok")])).unwrap(), None, "ignored without AGENTOS_TEST_WORKERS=1");
        let on = |v: &str| probe_hook(env(&[("AGENTOS_TEST_WORKERS", "1"), ("AGENTOS_TEST_JAIL_PROBE", v)]));
        assert_eq!(on("ok").unwrap(), Some(Ok(())));
        assert_eq!(on("fail:needs root").unwrap(), Some(Err("needs root".into())));
        assert_eq!(on("maybe").unwrap_err().code, 2);
        assert_eq!(probe_hook(env(&[("AGENTOS_TEST_WORKERS", "1")])).unwrap(), None);
    }
}
