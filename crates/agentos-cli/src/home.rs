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
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use agentos_core::contract::Limits;
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::firecracker::{FirecrackerConfig, preflight};
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::jail::{self, JAIL_GID, JAIL_UID, JailConfig, JailDecision, JailMode};
use agentos_engine::job::{HostConfig, WorkerConfig};
use agentos_engine::model::anthropic::AnthropicProvider;
use agentos_engine::model::executor::ModelExecutor;
use agentos_engine::model::fake::FakeProvider;
use agentos_engine::model::provider::{ApiKey, ModelProvider};
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::shadow::ShadowReader;
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

/// The one rule for `--anthropic-base-url`: `https` with any host, or `http` only to the local
/// machine (`localhost`, 127.0.0.0/8, `::1`); no user info, query or fragment. The message
/// names the rule and never quotes the URL, which may hold a secret.
pub fn validate_base_url(raw: &str) -> Result<String, CliError> {
    use reqwest::Url;
    use std::net::IpAddr;
    let refuse = || {
        CliError::usage(
            "invalid --anthropic-base-url: it must be https://HOST, or http:// to localhost, 127.0.0.0/8 or [::1] only, \
             with no user info, query or fragment",
        )
    };
    let url = Url::parse(raw).map_err(|_| refuse())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(refuse());
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let local = host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => return Err(refuse()),
    }
    Ok(url.to_string())
}

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
    /// `--api-key-file`: where the Anthropic key is read from (else `ANTHROPIC_API_KEY`).
    pub api_key_file: Option<PathBuf>,
    /// `--anthropic-base-url` [default: the provider's own].
    pub anthropic_base_url: Option<String>,
    /// How many times [`Home::api_key`] resolved a key in this process (unit tests only).
    #[cfg(test)]
    pub(crate) key_reads: Arc<AtomicUsize>,
}

/// Whether an executor may send model requests.
enum Dispatch {
    /// It may; the key, if the caller resolved one already.
    Model(Option<ApiKey>),
    /// It never does (cancel, recovery): no provider is built.
    Never,
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
pub fn probe_hook(
    get: impl Fn(&str) -> Option<String>,
) -> Result<Option<Result<(), String>>, CliError> {
    if get(TEST_WORKERS_ENV).as_deref() != Some("1") {
        return Ok(None);
    }
    match get(JAIL_PROBE_ENV).as_deref() {
        None => Ok(None),
        Some("ok") => Ok(Some(Ok(()))),
        Some(v) => match v.strip_prefix("fail:") {
            Some(reason) => Ok(Some(Err(reason.to_string()))),
            None => Err(CliError::usage(format!(
                "{JAIL_PROBE_ENV}={v:?}: expected ok or fail:<reason>"
            ))),
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
    proc_mounts
        .and_then(jail::find_cgroup2_root)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CGROUP_ROOT))
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
                let user = std::env::var_os("HOME")
                    .ok_or_else(|| CliError::usage("HOME is not set; pass --home DIR"))?;
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
            api_key_file: None,
            anthropic_base_url: None,
            #[cfg(test)]
            key_reads: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The Firecracker binary: `--firecracker`, else `<home>/bin/firecracker`.
    pub fn firecracker_bin(&self) -> PathBuf {
        self.firecracker
            .clone()
            .unwrap_or_else(|| self.root.join("bin/firecracker"))
    }

    /// The jailer: `--jailer`, else the file `jailer` next to the Firecracker binary.
    pub fn jailer_bin(&self) -> PathBuf {
        self.jailer
            .clone()
            .unwrap_or_else(|| self.firecracker_bin().with_file_name("jailer"))
    }

    /// The registry directory of profile `id`, which must be one plain name naming a
    /// directory really inside the registry (a symlink leading out of it is refused).
    /// `Ok(None)` when there is no such profile.
    pub fn profile_dir(&self, id: &str) -> Result<Option<PathBuf>, CliError> {
        let mut parts = Path::new(id).components();
        if !matches!(
            (parts.next(), parts.next()),
            (Some(Component::Normal(_)), None)
        ) || id.contains(['/', '\\', '\0'])
        {
            return Err(CliError::usage(format!(
                "verification profile id {id:?} is not a single plain name"
            )));
        }
        let dir = self.profiles.join(id);
        let Ok(real) = dir.canonicalize() else {
            return Ok(None);
        };
        let root = self.profiles.canonicalize()?;
        if real.parent() != Some(root.as_path()) {
            return Err(CliError::usage(format!(
                "verification profile {id} resolves to {}, outside the profile registry {}",
                real.display(),
                root.display()
            )));
        }
        Ok(real.join("profile.json").is_file().then_some(real))
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.root.join("registry")
    }

    /// Registered entries of profile `id`, oldest registration first.
    pub fn registry_entries(&self, id: &str) -> Vec<RegistryEntry> {
        let mut found: Vec<RegistryEntry> = self
            .registry_list()
            .into_iter()
            .filter(|e| e.id == id)
            .collect();
        found.sort_by(|a, b| (a.registered_ms, &a.digest).cmp(&(b.registered_ms, &b.digest)));
        found
    }

    /// Every registered profile, unordered.
    pub fn registry_list(&self) -> Vec<RegistryEntry> {
        list_entries(&self.registry_dir(), |dir| {
            dir.join("profile.json").is_file()
        })
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
        let mut found: Vec<RegistryEntry> = self
            .image_list()
            .into_iter()
            .filter(|e| e.id == id)
            .collect();
        found.sort_by(|a, b| (a.registered_ms, &a.digest).cmp(&(b.registered_ms, &b.digest)));
        found
    }

    /// Where guest image `id` comes from: the registry entry with exactly the digest `pin`
    /// (none is an error: a pin never falls back to anything else); else the newest entry;
    /// there is no legacy location. `Ok(None)` when there is none.
    pub fn resolve_image(
        &self,
        id: &str,
        pin: Option<&str>,
    ) -> Result<Option<RegistryEntry>, CliError> {
        check_id("guest image", id)?;
        if let Some(pin) = pin {
            return match self.image_entries(id).into_iter().find(|e| e.digest == pin) {
                Some(entry) => Ok(Some(entry)),
                None => Err(CliError::usage(format!(
                    "guest image {id}@{pin} is not in the registry; `agentos image register` it first"
                ))),
            };
        }
        Ok(self.image_entries(id).pop())
    }

    /// Where profile `id` comes from, in order: the registry entry with exactly the digest
    /// `pin` (none is an error: a pin never falls back to anything else); else the newest
    /// registry entry for `id`; else the legacy `<profiles>/<id>/` directory. `Ok(None)`
    /// when there is none.
    pub fn resolve_profile(
        &self,
        id: &str,
        pin: Option<&str>,
    ) -> Result<Option<PathBuf>, CliError> {
        if let Some(pin) = pin {
            return match self
                .registry_entries(id)
                .into_iter()
                .find(|e| e.digest == pin)
            {
                Some(entry) => Ok(Some(entry.dir)),
                None => Err(CliError::usage(format!(
                    "verification profile {id}@{pin} is not in the registry; `agentos profile register` it first"
                ))),
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
        Ok(Store {
            db: Db::open(&self.root.join("agentos.db"))?,
            blobs: BlobStore::open(self.root.join("blobs"))?,
        })
    }

    /// The driver lock, or `None` if another process holds it.
    pub fn try_lock(&self) -> Result<Option<DriverLock>, CliError> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join("driver.lock"))?;
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
        fs::read_to_string(self.root.join("driver.lock"))
            .ok()
            .filter(|s| !s.is_empty())
    }

    pub fn lock(&self) -> Result<DriverLock, CliError> {
        self.try_lock()?.ok_or_else(|| {
            CliError::other(format!(
                "another agentos process is driving tasks in {}; try again when it finishes",
                self.root.display()
            ))
        })
    }

    /// The profile digest recorded when `task` was submitted: what every verification is
    /// pinned to, whatever later happens to the staged copy.
    fn pinned_profile(&self, store: &Store, task: &TaskId) -> Result<Digest, CliError> {
        let events = store.db.events(task)?;
        let recorded = events
            .iter()
            .find(|e| e.event_type == "Submitted")
            .and_then(|e| e.payload["profile_digest"].as_str().map(str::to_string));
        recorded
            .and_then(|d| Digest::from_hex(&d).ok())
            .ok_or_else(|| {
                CliError::other(format!(
                    "task {task} has no recorded profile digest; it was not completely submitted"
                ))
            })
    }

    /// The worker recorded at `task`'s submission. No `worker` field (a 3a task, or no
    /// `Submitted` event at all) is the host worker.
    pub fn recorded_worker(
        &self,
        store: &Store,
        task: &TaskId,
    ) -> Result<RecordedWorker, CliError> {
        let events = store.db.events(task)?;
        let host = RecordedWorker {
            kind: WorkerKind::Host,
            image: None,
            jailed: None,
        };
        let Some(submitted) = events.iter().find(|e| e.event_type == "Submitted") else {
            return Ok(host);
        };
        let p = &submitted.payload;
        match p.get("worker").and_then(Value::as_str) {
            None | Some("host") => Ok(host),
            Some("firecracker") => {
                let broken = |what: &str| {
                    CliError::other(format!(
                        "task {task} was submitted to the firecracker worker without a valid recorded {what}"
                    ))
                };
                let id = p["guest_image_id"]
                    .as_str()
                    .ok_or_else(|| broken("guest_image_id"))?
                    .to_string();
                let digest = p["guest_image_digest"]
                    .as_str()
                    .and_then(|d| Digest::from_hex(d).ok())
                    .ok_or_else(|| broken("guest_image_digest"))?;
                let jailed = p["jailed"].as_bool().ok_or_else(|| broken("jailed"))?;
                Ok(RecordedWorker {
                    kind: WorkerKind::Firecracker,
                    image: Some((id, digest)),
                    jailed: Some(jailed),
                })
            }
            Some(other) => Err(CliError::other(format!(
                "task {task} was submitted with an unknown worker {:?}",
                other
            ))),
        }
    }

    /// `task`'s recorded worker, checked against `--worker`: a flag naming another worker
    /// exits 2 (an unknown task is reported as such first).
    pub fn task_worker(&self, store: &Store, task: &TaskId) -> Result<RecordedWorker, CliError> {
        store.db.task(task)?;
        let recorded = self.recorded_worker(store, task)?;
        match self.worker {
            Some(flag) if flag != recorded.kind => Err(CliError::usage(format!(
                "task was submitted with worker {}",
                recorded.kind.as_str()
            ))),
            _ => Ok(recorded),
        }
    }

    /// The guest launcher: the fake guest (this executable, `supervise fake-guest`) with
    /// `AGENTOS_TEST_WORKERS=1` and `AGENTOS_TEST_FAKE_GUEST=1`, else Firecracker.
    fn launcher(&self) -> Result<GuestLauncher, CliError> {
        if env_on(TEST_WORKERS_ENV) && env_on(FAKE_GUEST_ENV) {
            return Ok(GuestLauncher::Fake {
                program: std::env::current_exe()?,
                prefix_args: vec!["supervise".into()],
            });
        }
        Ok(GuestLauncher::Real {
            firecracker_bin: std::path::absolute(self.firecracker_bin())?,
        })
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
        let image_digest = Digest::from_hex(&image.digest).map_err(|e| {
            CliError::other(format!("guest image {}@{}: {e}", image.id, image.digest))
        })?;
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
        preflight(&cfg)
            .map_err(|e| CliError::other(format!("firecracker worker unavailable: {e}")))?;
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
    fn decide_jail(
        &self,
        cfg: &FirecrackerConfig,
        root: &Path,
        recorded: Option<bool>,
    ) -> Result<(JailMode, bool), CliError> {
        if recorded == Some(false) {
            return Ok((JailMode::Unjailed, false));
        }
        let jail_cfg = self.jail_config()?;
        let probe = match (probe_hook(|k| std::env::var(k).ok())?, &cfg.launcher) {
            (Some(answer), _) => Some(answer),
            (None, GuestLauncher::Fake { .. }) => None,
            (None, GuestLauncher::Real { .. }) => Some(jail::probe(
                &jail_cfg,
                &root.join("jobs"),
                &root.join("inspect"),
                &root.join("work"),
                &cfg.image_dir,
            )),
        };
        let jailed_mode = || match cfg.launcher {
            GuestLauncher::Real { .. } => JailMode::Jailed(jail_cfg.clone()),
            GuestLauncher::Fake { .. } => JailMode::Unjailed,
        };
        if recorded == Some(true) {
            return match probe {
                Some(Ok(())) => Ok((jailed_mode(), true)),
                Some(Err(reason)) => Err(CliError::other(format!(
                    "task was submitted jailed: jailer unavailable: {reason}"
                ))),
                None => Err(CliError::other(
                    "task was submitted jailed: jailer unavailable: the fake guest launcher is never jailed",
                )),
            };
        }
        let Some(probe) = probe else {
            return Ok((JailMode::Unjailed, false));
        };
        match jail::decide(probe, self.allow_unjailed) {
            Err(refusal) => Err(CliError::other(format!(
                "firecracker worker unavailable: {refusal}"
            ))),
            Ok(JailDecision::Unjailed { reason }) => {
                eprintln!("warning: running Firecracker unjailed: {reason}");
                Ok((JailMode::Unjailed, false))
            }
            Ok(JailDecision::Jailed) => Ok((jailed_mode(), true)),
        }
    }

    /// The `Submitted` payload of `task`, if it has one.
    fn submitted_payload(&self, store: &Store, task: &TaskId) -> Result<Option<Value>, CliError> {
        Ok(store
            .db
            .events(task)?
            .into_iter()
            .find(|e| e.event_type == "Submitted")
            .map(|e| e.payload))
    }

    /// The model recorded at `task`'s submission (`Submitted.model`); `None` when the task has
    /// no `Submitted` event or no such field (a 3a task).
    pub fn recorded_model(&self, store: &Store, task: &TaskId) -> Result<Option<String>, CliError> {
        Ok(self
            .submitted_payload(store, task)?
            .and_then(|p| p["model"].as_str().map(str::to_string)))
    }

    /// The Anthropic key: the whole of `--api-key-file` (else `ANTHROPIC_API_KEY`), trimmed;
    /// it must then be one line of printable ASCII, so a multi-line file is refused, not
    /// truncated. It is never taken from argv (so it never shows in `/proc/*/cmdline`), and
    /// no message here quotes it. Each call reads the source again: a command resolves it
    /// once and passes the [`ApiKey`] on ([`Home::executor`]).
    pub fn api_key(&self) -> Result<ApiKey, CliError> {
        #[cfg(test)]
        self.key_reads.fetch_add(1, Ordering::SeqCst);
        let raw = match &self.api_key_file {
            Some(path) => crate::secrets::read_key_file(path)
                .map_err(|e| CliError::usage(format!("cannot read {}: {e}", path.display())))?,
            None => match std::env::var("ANTHROPIC_API_KEY") {
                Ok(v) if !v.trim().is_empty() => v,
                _ => {
                    return Err(CliError::usage(
                        "no API key: pass --api-key-file FILE or set ANTHROPIC_API_KEY",
                    ));
                }
            },
        };
        let source = self.api_key_file.as_ref().map_or_else(
            || "ANTHROPIC_API_KEY".to_string(),
            |p| p.display().to_string(),
        );
        let key = ApiKey::new(&raw).map_err(|e| CliError::usage(format!("{source}: {e}")))?;
        // One line of printable ASCII: anything else is not a key and is no header value.
        if !key.expose().bytes().all(|b| b.is_ascii_graphic()) {
            return Err(CliError::usage(format!(
                "{source}: the API key must be one line of printable characters"
            )));
        }
        Ok(key)
    }

    /// `--anthropic-base-url`, validated ([`validate_base_url`]); `None` when not given.
    pub fn checked_base_url(&self) -> Result<Option<String>, CliError> {
        self.anthropic_base_url
            .as_deref()
            .map(validate_base_url)
            .transpose()
    }

    pub fn recorded_model_endpoint(
        &self,
        store: &Store,
        task: &TaskId,
    ) -> Result<Option<String>, CliError> {
        if !self
            .recorded_model(store, task)?
            .is_some_and(|m| m.starts_with("anthropic:"))
        {
            return Ok(None);
        }
        let payload = self.submitted_payload(store, task)?;
        let endpoint = payload
            .as_ref()
            .and_then(|p| p["model_endpoint"].as_str())
            .unwrap_or(agentos_engine::model::anthropic::ANTHROPIC_BASE_URL);
        Ok(Some(
            validate_base_url(endpoint)?
                .trim_end_matches('/')
                .to_string(),
        ))
    }

    /// Resolve before reading credentials or mutating the task. Legacy tasks with no
    /// recorded endpoint may only use the official endpoint.
    pub fn model_endpoint(&self, store: &Store, task: &TaskId) -> Result<Option<String>, CliError> {
        let recorded = self.recorded_model_endpoint(store, task)?;
        if let Some(endpoint) = &recorded
            && let Some(requested) = self.checked_base_url()?
            && requested.trim_end_matches('/') != endpoint
        {
            return Err(CliError::usage(
                "provider override differs from the recorded endpoint; submit a new task to change providers",
            ));
        }
        Ok(recorded)
    }

    /// The provider for `task`'s recorded model: Anthropic (over `key`, else the key resolved
    /// now), the scripted fake over the task's own copy of the transcript, or none for the
    /// fake agent.
    fn provider(
        &self,
        store: &Store,
        task: &TaskId,
        key: Option<ApiKey>,
    ) -> Result<Option<Box<dyn ModelProvider>>, CliError> {
        let Some(model) = self.recorded_model(store, task)? else {
            return Ok(None);
        };
        if model.starts_with("anthropic:") {
            let base = self.model_endpoint(store, task)?;
            let key = match key {
                Some(key) => key,
                None => self.api_key()?,
            };
            let provider = AnthropicProvider::new(key);
            let provider = match base {
                Some(url) => provider.with_base_url(url),
                None => provider,
            };
            Ok(Some(Box::new(provider) as Box<dyn ModelProvider>))
        } else if model.starts_with("fake:") {
            self.fake_provider(store, task).map(Some)
        } else {
            Ok(None)
        }
    }

    /// The scripted provider over `<task>/transcript.json`, which must still be the file whose
    /// digest `Submitted` recorded: a resume replays the same transcript or none.
    fn fake_provider(
        &self,
        store: &Store,
        task: &TaskId,
    ) -> Result<Box<dyn ModelProvider>, CliError> {
        let path = self.task_dir(task).join(crate::drive::TRANSCRIPT);
        let bytes = fs::read(&path).map_err(|e| {
            CliError::other(format!(
                "task {task}: cannot read its recorded transcript {}: {e}",
                path.display()
            ))
        })?;
        let recorded = self
            .submitted_payload(store, task)?
            .and_then(|p| p["transcript_digest"].as_str().map(str::to_string));
        let actual = Digest::of(&bytes).to_string();
        if recorded.as_deref() != Some(actual.as_str()) {
            return Err(CliError::other(format!(
                "task {task}: the recorded transcript changed: expected {}, found {actual}",
                recorded.as_deref().unwrap_or("none")
            )));
        }
        // From the bytes that were just digested, not from a second read of the file.
        let provider = FakeProvider::from_bytes(&bytes, &path.display().to_string())
            .map_err(|e| CliError::other(format!("task {task}: {e}")))?;
        Ok(Box::new(provider))
    }

    /// The executor for `task`, over the inputs and the worker recorded at its submission.
    /// For the Firecracker worker the preflight and the jail are checked here, before the
    /// caller touches the task. A model task needs its provider: for Anthropic, `key` (the
    /// one the caller already resolved) or, when `None`, the key resolved here, once.
    pub fn executor(
        &self,
        store: &Store,
        task: &TaskId,
        key: Option<ApiKey>,
    ) -> Result<RoutingExecutor<SupervisedExecutor>, CliError> {
        self.build_executor(store, task, Dispatch::Model(key))
    }

    /// Like [`Home::executor`] for paths that never send a model request (cancel, recovery of
    /// a finished task). Structurally so: its model executor has no provider, so no key or
    /// transcript is read and a model dispatch fails with `no model provider configured`.
    pub fn recovery_executor(
        &self,
        store: &Store,
        task: &TaskId,
    ) -> Result<RoutingExecutor<SupervisedExecutor>, CliError> {
        self.build_executor(store, task, Dispatch::Never)
    }

    fn build_executor(
        &self,
        store: &Store,
        task: &TaskId,
        dispatch: Dispatch,
    ) -> Result<RoutingExecutor<SupervisedExecutor>, CliError> {
        if matches!(dispatch, Dispatch::Model(_)) {
            self.model_endpoint(store, task)?;
        }
        let recorded = self.task_worker(store, task)?;
        let dir = self.task_dir(task);
        if !dir.join("snapshot").is_dir() || !dir.join("profile").is_dir() {
            return Err(CliError::other(format!(
                "task {task} has no recorded inputs in {}; it was not completely submitted",
                dir.display()
            )));
        }
        // The supervisor runs in its own working directory: every path is absolute.
        let root = std::path::absolute(&self.root)?;
        let task_dir = root.join("tasks").join(task.as_str());
        let profile_digest = Some(self.pinned_profile(store, task)?);
        let worker = match recorded.kind {
            WorkerKind::Firecracker => {
                let (id, digest) = recorded.image.ok_or_else(|| {
                    CliError::other(format!("task {task} has no recorded guest image"))
                })?;
                let entry = self
                    .image_entries(&id)
                    .into_iter()
                    .find(|e| e.digest == digest.to_string())
                    .ok_or_else(|| {
                        CliError::other(format!(
                            "recorded guest image {id}@{digest} is no longer registered"
                        ))
                    })?;
                let limits = store.db.contract(task)?.limits;
                WorkerConfig::Firecracker(
                    self.prepare_firecracker(
                        &entry,
                        &limits,
                        &task_dir,
                        profile_digest,
                        recorded.jailed,
                    )?
                    .cfg,
                )
            }
            WorkerKind::Host => WorkerConfig::Host(HostConfig {
                snapshot_dir: task_dir.join("snapshot"),
                profile_dir: task_dir.join("profile"),
                work_root: root.join("work"),
                verify_timeout_secs: VERIFY_TIMEOUT_SECS,
                profile_digest,
            }),
        };
        let provider = match dispatch {
            Dispatch::Model(key) => self.provider(store, task, key)?,
            Dispatch::Never => None,
        };
        let mut jobs = SupervisedExecutor::new(
            root.join("jobs"),
            supervisor_cmd()?,
            worker,
            ExecCounts::default(),
        )?;
        // The supervisor's environment is cleared: the test switches (and only they, and
        // only in the test tier) travel explicitly. The API key travels nowhere: it lives in
        // the provider, in this process.
        if env_on(TEST_WORKERS_ENV) {
            for (k, v) in std::env::vars().filter(|(k, _)| k.starts_with("AGENTOS_TEST_")) {
                jobs = jobs.with_env(k, v);
            }
        }
        let model = ModelExecutor::new(root.join("model"), provider, ExecCounts::default());
        let reads = ShadowReader::new(
            root.join("agentos.db"),
            task_dir.join("snapshot"),
            root.join("tasks"),
        );
        Ok(RoutingExecutor::new(jobs, model, reads))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::contract::Contract;
    use agentos_core::effect::{AttemptId, EffectId, EffectKind, Outcome};
    use agentos_engine::executor::{AttemptCtx, EffectRequest, Executor};

    #[test]
    fn base_urls_must_be_https_or_loopback_http_without_userinfo_query_or_fragment() {
        for bad in [
            "http://example.com",
            "http://10.0.0.1",
            "http://127.0.0.1.evil.com",
            "https://uSeR:pw@api.example/",
            "https://uSeR@api.example/",
            "https://api.example/?k=v",
            "https://api.example/#f",
            "http://localhost.evil.com:1",
            "ftp://x",
            "garbage",
            "",
        ] {
            let err = validate_base_url(bad).unwrap_err();
            assert_eq!(err.code, 2, "{bad}");
            for secret in ["pw", "k=v", "uSeR"] {
                assert!(
                    !err.message.contains(secret),
                    "the message echoes {secret:?}: {}",
                    err.message
                );
            }
        }
        for good in [
            "https://api.anthropic.com",
            "https://proxy.example:8443/prefix",
            "http://127.0.0.1:8080",
            "http://127.9.9.9",
            "http://localhost:9",
            "http://[::1]:9",
        ] {
            validate_base_url(good).unwrap_or_else(|e| panic!("{good}: {e}"));
        }
    }

    const KEY_CANARY: &str = "sk-ant-unit-SECRET";

    /// A home with one submitted, non-started `model` task: its recorded inputs and a
    /// `Submitted` event naming the model, the endpoint and the profile digest.
    fn home_with_task(
        dir: &Path,
        model: &str,
        key_file: Option<PathBuf>,
    ) -> (Home, Store, TaskId, EffectRequest) {
        let mut home = Home::new(Some(dir.join("home")), None).unwrap();
        home.api_key_file = key_file;
        let store = home.open().unwrap();
        let contract = Contract::parse(
            &serde_json::json!({
                "goal": "g",
                "repository": { "source": dir, "revision": "recorded-at-submission" },
                "profile": "python-stdlib-v1",
                "editable_paths": ["src/**"],
                "verification_profile": "parser-checks-v1",
                "capabilities": ["snapshot.read", "model.request"],
                "limits": {
                    "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 3,
                    "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256
                }
            })
            .to_string(),
        )
        .unwrap();
        let task = store
            .db
            .create_task(&contract, &Digest::of(b"contract"))
            .unwrap();
        store
            .db
            .append_audit(
                &task,
                "Submitted",
                &serde_json::json!({
                    "model": model,
                    "model_endpoint": "https://api.anthropic.com",
                    "profile_digest": Digest::of(b"profile"),
                }),
            )
            .unwrap();
        for d in ["snapshot", "profile"] {
            fs::create_dir_all(home.task_dir(&task).join(d)).unwrap();
        }
        let kind = EffectKind::ModelCall {
            model: model.into(),
            turn: 1,
        };
        let payload = b"{\"messages\":[]}".to_vec();
        let request = EffectRequest {
            effect_id: EffectId::derive(&task, 0, &kind, &Digest::of(&payload)),
            task_id: task.clone(),
            kind,
            payload,
            contract,
            deadline_ts: 0,
        };
        (home, store, task, request)
    }

    #[tokio::test]
    async fn recovery_never_builds_a_provider_or_reads_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        fs::write(&key, KEY_CANARY).unwrap();
        for key_file in [Some(key), Some(dir.path().join("absent")), None] {
            let (home, store, task, request) =
                home_with_task(dir.path(), "anthropic:claude-x", key_file);
            let exec = home.recovery_executor(&store, &task).unwrap();
            assert_eq!(home.key_reads.load(Ordering::SeqCst), 0);
            let ctx = AttemptCtx {
                attempt_id: AttemptId::new(),
                lease_generation: 1,
                worker: "model".into(),
            };
            let out = exec.run(&request, &ctx).await;
            assert_eq!(
                out.receipt.outcome,
                Outcome::Failure("no model provider configured".into())
            );
            fs::remove_dir_all(dir.path().join("home")).unwrap();
        }
    }

    #[tokio::test]
    async fn a_fake_task_recovers_without_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (home, store, task, _) = home_with_task(dir.path(), "fake:/nowhere.json", None);
        // No transcript file: a dispatching executor refuses, the recovery one does not.
        assert!(home.executor(&store, &task, None).is_err());
        home.recovery_executor(&store, &task).unwrap();
    }

    #[test]
    fn a_resolved_key_is_used_without_reading_the_source_again() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        fs::write(&key, KEY_CANARY).unwrap();
        let (home, store, task, _) =
            home_with_task(dir.path(), "anthropic:claude-x", Some(key.clone()));
        // The caller resolves the key once...
        let resolved = home.api_key().unwrap();
        assert_eq!(resolved.expose(), KEY_CANARY);
        assert_eq!(home.key_reads.load(Ordering::SeqCst), 1);
        // ...the source then changes, and the executor is built from the resolved key.
        fs::write(&key, "a-different-key").unwrap();
        home.executor(&store, &task, Some(resolved)).unwrap();
        fs::remove_file(&key).unwrap();
        assert_eq!(home.key_reads.load(Ordering::SeqCst), 1);
        // Without a resolved key the executor resolves one itself, exactly once (here it
        // fails: the file is gone).
        assert!(home.executor(&store, &task, None).is_err());
        assert_eq!(home.key_reads.load(Ordering::SeqCst), 2);
    }

    fn entry(home: &Home, id: &str, digest: &str, ms: i64) {
        let name = format!("{id}@{digest}");
        let dir = home.images_dir().join(&name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("image.json"), "{}").unwrap();
        fs::write(
            home.images_dir().join(format!("{name}.meta.json")),
            format!("{{\"registered_ms\":{ms}}}"),
        )
        .unwrap();
    }

    #[test]
    fn resolve_image_pins_exactly_else_newest_and_never_falls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::new(Some(tmp.path().to_path_buf()), None).unwrap();
        let (old, new) = ("a".repeat(64), "b".repeat(64));
        assert!(home.resolve_image("img", None).unwrap().is_none());
        entry(&home, "img", &old, 1);
        entry(&home, "img", &new, 2);
        assert_eq!(
            home.resolve_image("img", None).unwrap().unwrap().digest,
            new
        );
        assert_eq!(
            home.resolve_image("img", Some(&old))
                .unwrap()
                .unwrap()
                .digest,
            old
        );
        let err = home
            .resolve_image("img", Some(&"c".repeat(64)))
            .unwrap_err();
        assert_eq!(err.code, 2);
        assert!(
            err.message
                .contains("is not in the registry; `agentos image register` it first"),
            "{}",
            err.message
        );
        assert_eq!(home.resolve_image("a@b", None).unwrap_err().code, 2);
        // Profiles and images do not see each other.
        assert!(home.registry_list().is_empty());
    }

    #[test]
    fn jailer_defaults_to_the_sibling_of_the_firecracker_binary() {
        let home = Home {
            firecracker: Some("/x/firecracker".into()),
            jailer: None,
            ..Home::new(Some("/h".into()), None).unwrap()
        };
        assert_eq!(home.jailer_bin(), Path::new("/x/jailer"));
        let home = Home {
            firecracker: None,
            jailer: None,
            ..Home::new(Some("/h".into()), None).unwrap()
        };
        assert_eq!(home.firecracker_bin(), Path::new("/h/bin/firecracker"));
        assert_eq!(home.jailer_bin(), Path::new("/h/bin/jailer"));
        let home = Home {
            firecracker: Some("/x/firecracker".into()),
            jailer: Some("/y/my-jailer".into()),
            ..Home::new(Some("/h".into()), None).unwrap()
        };
        assert_eq!(home.jailer_bin(), Path::new("/y/my-jailer"));
    }

    #[test]
    fn jail_cgroup_root_is_the_cgroup2_mount_point_from_proc_mounts() {
        let host = "sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
                    cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate 0 0\n";
        assert_eq!(cgroup_root_from(Some(host)), Path::new("/sys/fs/cgroup"));
        let elsewhere = "proc /proc proc rw 0 0\nnone /mnt/cg\\040two cgroup2 rw 0 0\n";
        assert_eq!(
            cgroup_root_from(Some(elsewhere)),
            Path::new("/mnt/cg two"),
            "the mount point, unescaped"
        );
        let v1_only = "cgroup /sys/fs/cgroup/cpu cgroup rw,cpu 0 0\n";
        assert_eq!(
            cgroup_root_from(Some(v1_only)),
            Path::new(DEFAULT_CGROUP_ROOT),
            "none: the default, which the probe then refuses"
        );
        assert_eq!(cgroup_root_from(None), Path::new(DEFAULT_CGROUP_ROOT));
        // And it is what the jail is built with on this host.
        let mounts = fs::read_to_string("/proc/mounts").ok();
        let home = Home {
            jailer: Some("/j/jailer".into()),
            jail_uid: 7,
            jail_gid: 8,
            ..Home::new(Some("/h".into()), None).unwrap()
        };
        let cfg = home.jail_config().unwrap();
        assert_eq!(cfg.cgroup_root, cgroup_root_from(mounts.as_deref()));
        assert_eq!(
            (cfg.jailer_bin.as_path(), cfg.uid, cfg.gid),
            (Path::new("/j/jailer"), 7, 8)
        );
    }

    #[test]
    fn the_jail_probe_hook_answers_only_with_test_workers() {
        let env = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| name == k)
                    .map(|(_, v)| v.clone())
            }
        };
        assert_eq!(
            probe_hook(env(&[("AGENTOS_TEST_JAIL_PROBE", "ok")])).unwrap(),
            None,
            "ignored without AGENTOS_TEST_WORKERS=1"
        );
        let on = |v: &str| {
            probe_hook(env(&[
                ("AGENTOS_TEST_WORKERS", "1"),
                ("AGENTOS_TEST_JAIL_PROBE", v),
            ]))
        };
        assert_eq!(on("ok").unwrap(), Some(Ok(())));
        assert_eq!(
            on("fail:needs root").unwrap(),
            Some(Err("needs root".into()))
        );
        assert_eq!(on("maybe").unwrap_err().code, 2);
        assert_eq!(
            probe_hook(env(&[("AGENTOS_TEST_WORKERS", "1")])).unwrap(),
            None
        );
    }
}
