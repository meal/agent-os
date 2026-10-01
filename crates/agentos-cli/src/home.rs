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
use std::path::{Path, PathBuf};

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
pub struct DriverLock(#[allow(dead_code)] File);

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

    pub fn profile_dir(&self, id: &str) -> PathBuf {
        self.profiles.join(id)
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
            Ok(()) => Ok(Some(DriverLock(file))),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
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
