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
//! ```

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use agentos_core::ids::{Digest, TaskId};
use agentos_engine::job::{HostConfig, WorkerConfig};
use agentos_engine::supervised::{ExecCounts, SupervisedExecutor};
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;

use crate::commands::registry::{check_id, list_entries};
use crate::commands::supervise::supervisor_cmd;
use crate::error::CliError;

/// How long a verification check may run (the fixture executor's default).
const VERIFY_TIMEOUT_SECS: u64 = 60;

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
        Ok(Home { root, profiles })
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

    // Consumed by the submit preflight (the next task).
    #[allow(dead_code)]
    /// Registered entries of guest image `id`, oldest registration first.
    pub fn image_entries(&self, id: &str) -> Vec<RegistryEntry> {
        let mut found: Vec<RegistryEntry> = self.image_list().into_iter().filter(|e| e.id == id).collect();
        found.sort_by(|a, b| (a.registered_ms, &a.digest).cmp(&(b.registered_ms, &b.digest)));
        found
    }

    // Consumed by the submit preflight (the next task).
    #[allow(dead_code)]
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

    /// The executor for `task`, over the inputs recorded at its submission.
    pub fn executor(&self, store: &Store, task: &TaskId) -> Result<SupervisedExecutor, CliError> {
        let dir = self.task_dir(task);
        if !dir.join("snapshot").is_dir() || !dir.join("profile").is_dir() {
            return Err(CliError::other(format!("task {task} has no recorded inputs in {}; it was not completely submitted", dir.display())));
        }
        // The supervisor runs in its own working directory: every path is absolute.
        let root = std::path::absolute(&self.root)?;
        let task_dir = root.join("tasks").join(task.as_str());
        let host = HostConfig {
            snapshot_dir: task_dir.join("snapshot"),
            profile_dir: task_dir.join("profile"),
            work_root: root.join("work"),
            verify_timeout_secs: VERIFY_TIMEOUT_SECS,
            profile_digest: Some(self.pinned_profile(store, task)?),
        };
        Ok(SupervisedExecutor::new(root.join("jobs"), supervisor_cmd()?, WorkerConfig::Host(host), ExecCounts::default())?)
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
}
