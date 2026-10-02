//! Where the guest's effects happen: the workspace and scratch locations, how `git` and the
//! check are started, and what the guest reports about itself. The VM backend (Task 11)
//! works on mounted drives as dedicated uids; the fake backend works on two directories.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait Backend: Send {
    /// The workspace root (`/workspace` in a VM).
    fn workspace_dir(&self) -> &Path;
    /// Per-boot scratch space outside the workspace (`/scratch` in a VM).
    fn scratch_dir(&self) -> &Path;
    /// Whether a snapshot has been read into the workspace (fake: the directory exists;
    /// VM: the workspace is mounted).
    fn workspace_present(&self) -> bool {
        self.workspace_dir().is_dir()
    }
    /// Makes the workspace empty and writable before a snapshot is written into it.
    fn prepare_workspace(&mut self) -> Result<(), String>;
    /// Makes everything written to the workspace durable before a success is reported.
    fn sync_workspace(&self) -> Result<(), String>;
    /// Inspection never writes: the workspace is remounted read-only before answering.
    fn remount_workspace_ro(&self) -> Result<(), String>;
    /// A `git` invocation with a scrubbed environment that never discovers a repository
    /// above `cwd`.
    fn git(&self, cwd: &Path) -> Command;
    /// The check command `program args… workspace`, run from `cwd` (the staged profile).
    fn check_command(&self, program: &str, args: &[String], workspace: &Path, cwd: &Path, pycache: &Path) -> Command;
    fn vcpus(&self) -> u32;
    fn memory_mib(&self) -> u32;
    /// Test hook (fake guest only, with `AGENTOS_TEST_WORKERS=1`): `PatchState` is never
    /// answered; the session ends when the host closes the connection.
    fn hang_inspect(&self) -> bool {
        false
    }
}

/// The scrubbed `git` both backends start from (the VM backend adds the uid drop).
pub fn scrubbed_git(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env_clear()
        .env("GIT_CEILING_DIRECTORIES", cwd.parent().unwrap_or(cwd))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("PATH", "/usr/bin:/bin");
    cmd
}

/// CPUs as the guest sees them.
pub fn visible_vcpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// `MemTotal` from `/proc/meminfo`, in MiB (0 if it cannot be read).
pub fn visible_memory_mib() -> u32 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("MemTotal:"))?;
            let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            u32::try_from(kib / 1024).ok()
        })
        .unwrap_or(0)
}

/// The fake guest: `<root>/workspace` and `<root>/scratch` instead of block devices, the
/// current user instead of `builder`/`check`.
#[derive(Debug, Clone)]
pub struct FakeBackend {
    root: PathBuf,
    workspace: PathBuf,
    scratch: PathBuf,
}

impl FakeBackend {
    /// Recreates `<root>/scratch` empty, as a VM formats its scratch drive at boot. The
    /// workspace persists across starts, as `ws.img` does.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<FakeBackend> {
        let root = root.into();
        let (workspace, scratch) = (root.join("workspace"), root.join("scratch"));
        match fs::remove_dir_all(&scratch) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::create_dir_all(&scratch)?;
        Ok(FakeBackend { root, workspace, scratch })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Backend for FakeBackend {
    fn workspace_dir(&self) -> &Path {
        &self.workspace
    }

    fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    fn prepare_workspace(&mut self) -> Result<(), String> {
        match fs::remove_dir_all(&self.workspace) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        fs::create_dir_all(&self.workspace).map_err(|e| e.to_string())
    }

    fn sync_workspace(&self) -> Result<(), String> {
        Ok(())
    }

    fn remount_workspace_ro(&self) -> Result<(), String> {
        Ok(())
    }

    fn git(&self, cwd: &Path) -> Command {
        scrubbed_git(cwd)
    }

    /// The 3a environment: nothing inherited but `PATH`.
    fn check_command(&self, program: &str, args: &[String], workspace: &Path, cwd: &Path, pycache: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .arg(workspace)
            .current_dir(cwd)
            .env_clear()
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONPYCACHEPREFIX", pycache);
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        cmd
    }

    fn vcpus(&self) -> u32 {
        visible_vcpus()
    }

    fn hang_inspect(&self) -> bool {
        crate::fake::test_hook("AGENTOS_TEST_FAKE_GUEST_HANG_INSPECT")
    }

    fn memory_mib(&self) -> u32 {
        visible_memory_mib()
    }
}
