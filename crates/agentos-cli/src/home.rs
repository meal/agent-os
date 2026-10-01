//! The agentos home directory:
//!
//! ```text
//! <home>/agentos.db          journal (SQLite, WAL)
//! <home>/blobs/              content-addressed artifacts
//! <home>/receipts/           executor receipts retained across controller restarts
//! <home>/work/<task>/ws      task workspaces
//! <home>/tasks/<task>/       inputs recorded at submission: snapshot/, profile/, agent.patch
//! <home>/driver.lock         held by the one process driving tasks (running or recovering)
//! <home>/profiles/<id>/      verification profile registry (default for --profiles)
//! ```

use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use agentos_core::ids::TaskId;
use agentos_engine::durable::{DurableExecutor, ExecCounts};
use agentos_engine::fixture::FixtureExecutor;
use agentos_store::blob::BlobStore;
use agentos_store::db::Db;

use crate::error::CliError;

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

    /// The executor for `task`, over the inputs recorded at its submission.
    pub fn executor(&self, task: &TaskId) -> Result<DurableExecutor<FixtureExecutor>, CliError> {
        let dir = self.task_dir(task);
        if !dir.join("snapshot").is_dir() || !dir.join("profile").is_dir() {
            return Err(CliError::other(format!("task {task} has no recorded inputs in {}; it was not completely submitted", dir.display())));
        }
        let fixture = FixtureExecutor::new(dir.join("snapshot"), dir.join("profile"), self.root.join("work"));
        Ok(DurableExecutor::new(fixture, self.root.join("receipts"), ExecCounts::default())?)
    }
}
