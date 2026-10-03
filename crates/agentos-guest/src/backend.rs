//! Where the guest's effects happen: the workspace and scratch locations, how `git` and the
//! check are started, and what the guest reports about itself. The VM backend (Task 11)
//! works on mounted drives as dedicated uids; the fake backend works on two directories.

use std::ffi::CStr;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentos_core::guest::INIT_PATH;
use rustix::fs::{chownat, syncfs, AtFlags, Gid, Uid, CWD};
use rustix::mount::{mount, mount_remount, unmount, MountFlags, UnmountFlags};

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
    /// Hands a tree the agent wrote (as root) to the uid that runs `git` on it: the snapshot
    /// in the workspace, the reverse-check copy on scratch. The fake runs everything as the
    /// current user, so it has nothing to do.
    fn own_tree(&self, _dir: &Path) -> Result<(), String> {
        Ok(())
    }
    /// A `git` invocation with a scrubbed environment that never discovers a repository
    /// above `cwd`.
    fn git(&self, cwd: &Path) -> Command;
    /// The check command `program args… workspace`, run from `cwd` (the staged profile).
    fn check_command(&self, program: &str, args: &[String], workspace: &Path, cwd: &Path, pycache: &Path) -> Command;
    /// Whether `program` can be started as the check from `cwd`, checked before the run
    /// where the start itself cannot report it: the VM starts the trampoline, whose `exec`
    /// failure would look like the check's own exit 127, so a missing or non-executable
    /// program is reported here as `spawn` reports it (3a: `cannot run profile command: …`).
    fn check_program(&self, _program: &str, _cwd: &Path) -> io::Result<()> {
        Ok(())
    }
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

    /// Hanging, it first drops `<scratch>/inspect-hung`, so a test can tell the inspection
    /// is under way (connected, `PatchState` received).
    fn hang_inspect(&self) -> bool {
        let hang = crate::fake::test_hook("AGENTOS_TEST_FAKE_GUEST_HANG_INSPECT");
        if hang {
            let _ = fs::write(self.scratch.join("inspect-hung"), b"");
        }
        hang
    }

    fn memory_mib(&self) -> u32 {
        visible_memory_mib()
    }
}

/// `builder`: owns the workspace and runs `git`.
pub const BUILDER_UID: u32 = 1000;
/// `check`: runs the verification command, reads the workspace, writes only `/scratch/check`.
pub const CHECK_UID: u32 = 1001;
/// The check's `RLIMIT_NPROC` and `RLIMIT_NOFILE`, and its `oom_score_adj` (the agent runs
/// at `AGENT_OOM_SCORE_ADJ`).
pub const CHECK_NPROC: u64 = 256;
pub const CHECK_NOFILE: u64 = 1024;
pub const CHECK_OOM_SCORE_ADJ: i32 = 1000;
pub const AGENT_OOM_SCORE_ADJ: i32 = -1000;
/// The drives as the guest kernel names them (`vda` is the read-only root).
pub const WORKSPACE_DEVICE: &str = "/dev/vdb";
pub const SCRATCH_DEVICE: &str = "/dev/vdc";
pub const WORKSPACE_DIR: &str = "/workspace";
pub const SCRATCH_DIR: &str = "/scratch";
/// Absolute: PID 1 has no `PATH`, and `mkfs.ext4` lives in `sbin`.
pub const MKFS_EXT4: &str = "/sbin/mkfs.ext4";
/// What `prepare_workspace` zeroes before formatting, so no old superblock survives.
const WIPE_BYTES: usize = 1 << 20;
/// The longest tool output quoted in a reason.
const QUOTED_OUTPUT: usize = 1024;

/// How the drives are mounted: no device nodes, no set-uid binaries.
pub fn drive_flags() -> MountFlags {
    MountFlags::NOSUID | MountFlags::NODEV
}

/// Whether `path` is the root of a mount (its device differs from its parent's).
pub fn is_mount_point(path: &Path) -> bool {
    let parent = path.parent().unwrap_or(Path::new("/"));
    match (fs::metadata(path), fs::metadata(parent)) {
        (Ok(a), Ok(b)) => a.dev() != b.dev(),
        _ => false,
    }
}

/// `mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 DEVICE`, with nothing
/// inherited.
pub fn mkfs_ext4(device: &str) -> Result<(), String> {
    let out = Command::new(MKFS_EXT4)
        .args(["-q", "-F", "-E", "lazy_itable_init=0,lazy_journal_init=0", device])
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .output()
        .map_err(|e| format!("cannot run {MKFS_EXT4}: {e}"))?;
    if !out.status.success() {
        let mut err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        err.truncate(err.floor_char_boundary(QUOTED_OUTPUT));
        return Err(format!("mkfs.ext4 {device} failed ({}): {err}", out.status));
    }
    Ok(())
}

/// Mounts an ext4 `device` on `target` with `drive_flags()`.
pub fn mount_ext4(device: &str, target: &Path) -> Result<(), String> {
    mount(device, target, "ext4", drive_flags(), None::<&CStr>)
        .map_err(|e| format!("mount {device} on {}: {e}", target.display()))
}

/// Zeroes the start of `device` (an old superblock must not survive a failed `mkfs`).
fn wipe(device: &str) -> io::Result<()> {
    let mut f = File::options().write(true).open(device)?;
    f.write_all(&vec![0u8; WIPE_BYTES])?;
    f.sync_data()
}

/// `chown -hR uid:gid dir`: every entry, symlinks themselves (never their targets).
pub fn chown_tree(dir: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let (uid, gid) = (Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)));
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        chownat(CWD, &path, uid, gid, AtFlags::SYMLINK_NOFOLLOW)?;
        if fs::symlink_metadata(&path)?.is_dir() {
            for entry in fs::read_dir(&path)? {
                stack.push(entry?.path());
            }
        }
    }
    Ok(())
}

/// The check's `PATH`.
pub const CHECK_PATH: &str = "/usr/bin:/bin";

/// `execvp`'s view of `program` under `CHECK_PATH` from `cwd`: the first candidate that is a
/// file with an execute bit, else `ENOENT` (nothing found) or `EACCES` (found, not
/// executable). A check for the common mistakes, not a security boundary: the kernel decides.
pub fn find_program(program: &str, cwd: &Path) -> io::Result<PathBuf> {
    let candidates: Vec<PathBuf> = if program.contains('/') {
        vec![cwd.join(program)]
    } else {
        CHECK_PATH.split(':').map(|dir| Path::new(dir).join(program)).collect()
    };
    let mut found_unexecutable = false;
    for c in candidates {
        if let Ok(meta) = fs::metadata(&c)
            && meta.is_file()
        {
            if meta.permissions().mode() & 0o111 != 0 {
                return Ok(c);
            }
            found_unexecutable = true;
        }
    }
    Err(io::Error::from_raw_os_error(if found_unexecutable { rustix::io::Errno::ACCESS } else { rustix::io::Errno::NOENT }.raw_os_error()))
}

/// The real guest: `/workspace` on `/dev/vdb`, `/scratch` on `/dev/vdc`, `git` as `builder`,
/// the check as `check` through the `exec-check` trampoline. Stateless: what is mounted is
/// read from the mount table itself, so connections can each hold a clone.
#[derive(Debug, Clone)]
pub struct VmBackend {
    workspace: PathBuf,
    scratch: PathBuf,
}

impl Default for VmBackend {
    fn default() -> VmBackend {
        VmBackend { workspace: PathBuf::from(WORKSPACE_DIR), scratch: PathBuf::from(SCRATCH_DIR) }
    }
}

impl Backend for VmBackend {
    fn workspace_dir(&self) -> &Path {
        &self.workspace
    }

    fn scratch_dir(&self) -> &Path {
        &self.scratch
    }

    /// A snapshot was read: `/workspace` is a mounted filesystem (the boot mounts an existing
    /// `ws.img`; a blank one does not mount).
    fn workspace_present(&self) -> bool {
        is_mount_point(&self.workspace)
    }

    /// Unmount whatever is there, zero the first 1 MiB of `/dev/vdb`, `mkfs.ext4`, mount it
    /// `rw,nosuid,nodev`, owned by `builder`, 0755.
    fn prepare_workspace(&mut self) -> Result<(), String> {
        if is_mount_point(&self.workspace) {
            unmount(&self.workspace, UnmountFlags::empty()).map_err(|e| format!("unmount {}: {e}", self.workspace.display()))?;
        }
        wipe(WORKSPACE_DEVICE).map_err(|e| format!("cannot wipe {WORKSPACE_DEVICE}: {e}"))?;
        mkfs_ext4(WORKSPACE_DEVICE)?;
        mount_ext4(WORKSPACE_DEVICE, &self.workspace)?;
        let ws = &self.workspace;
        // mkfs's empty `lost+found` (root, 0700) is outside the digest (empty directories are)
        // but would be an unreadable directory in the tree the check walks.
        match fs::remove_dir(ws.join("lost+found")) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("rmdir {}/lost+found: {e}", ws.display())),
        }
        chown_tree(ws, BUILDER_UID, BUILDER_UID).map_err(|e| format!("chown {}: {e}", ws.display()))?;
        fs::set_permissions(ws, fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {}: {e}", ws.display()))
    }

    fn sync_workspace(&self) -> Result<(), String> {
        let dir = File::open(&self.workspace).map_err(|e| format!("open {}: {e}", self.workspace.display()))?;
        syncfs(&dir).map_err(|e| format!("syncfs {}: {e}", self.workspace.display()))
    }

    /// `mount -o remount,ro`; nothing mounted is not an error (`Digest` then answers
    /// "workspace missing").
    fn remount_workspace_ro(&self) -> Result<(), String> {
        if !is_mount_point(&self.workspace) {
            return Ok(());
        }
        mount_remount(&self.workspace, MountFlags::RDONLY | drive_flags(), "")
            .map_err(|e| format!("remount {} read-only: {e}", self.workspace.display()))
    }

    fn own_tree(&self, dir: &Path) -> Result<(), String> {
        chown_tree(dir, BUILDER_UID, BUILDER_UID).map_err(|e| format!("cannot hand {} to builder: {e}", dir.display()))
    }

    fn git(&self, cwd: &Path) -> Command {
        let mut cmd = scrubbed_git(cwd);
        cmd.uid(BUILDER_UID).gid(BUILDER_UID);
        cmd
    }

    /// The trampoline applies the rlimits and the OOM priority, then `exec`s the check
    /// (spec issue 6: no `pre_exec`).
    fn check_command(&self, program: &str, args: &[String], workspace: &Path, cwd: &Path, pycache: &Path) -> Command {
        let (nproc, nofile, oom) = (CHECK_NPROC.to_string(), CHECK_NOFILE.to_string(), CHECK_OOM_SCORE_ADJ.to_string());
        let mut cmd = Command::new(INIT_PATH);
        cmd.args(["exec-check", "--nproc", &nproc, "--nofile", &nofile, "--oom", &oom, "--"])
            .arg(program)
            .args(args)
            .arg(workspace)
            .current_dir(cwd)
            .uid(CHECK_UID)
            .gid(CHECK_UID)
            .process_group(0)
            .env_clear()
            .env("PATH", CHECK_PATH)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PYTHONPYCACHEPREFIX", pycache);
        cmd
    }

    fn check_program(&self, program: &str, cwd: &Path) -> io::Result<()> {
        find_program(program, cwd).map(|_| ())
    }

    fn vcpus(&self) -> u32 {
        visible_vcpus()
    }

    fn memory_mib(&self) -> u32 {
        visible_memory_mib()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::symlink;

    #[test]
    fn vm_check_command_goes_through_the_trampoline_with_the_contract_limits() {
        let b = VmBackend::default();
        let cmd = b.check_command(
            "python3",
            &["check.py".to_string()],
            Path::new("/workspace"),
            Path::new("/scratch/profile"),
            Path::new("/scratch/check/pycache"),
        );
        assert_eq!(cmd.get_program(), OsStr::new("/sbin/agentos-guest"));
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            ["exec-check", "--nproc", "256", "--nofile", "1024", "--oom", "1000", "--", "python3", "check.py", "/workspace"]
                .map(OsStr::new)
        );
        assert_eq!(cmd.get_current_dir(), Some(Path::new("/scratch/profile")));
        let mut envs: Vec<(String, String)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.unwrap().to_string_lossy().into_owned()))
            .collect();
        envs.sort();
        assert_eq!(
            envs,
            [
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
                ("PYTHONPYCACHEPREFIX".to_string(), "/scratch/check/pycache".to_string()),
            ]
        );
        // The trampoline's own usage agrees with the arguments the agent builds.
        let parsed = crate::trampoline::parse(&args.iter().skip(1).map(|a| a.to_os_string()).collect::<Vec<_>>()).unwrap();
        assert_eq!((parsed.nproc, parsed.nofile, parsed.oom_score_adj), (CHECK_NPROC, CHECK_NOFILE, CHECK_OOM_SCORE_ADJ));
        assert_eq!(parsed.program, "python3");
    }

    #[test]
    fn vm_git_is_the_scrubbed_git() {
        let cmd = VmBackend::default().git(Path::new("/workspace"));
        assert_eq!(cmd.get_program(), OsStr::new("git"));
        let envs: Vec<String> = cmd.get_envs().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
        assert!(envs.contains(&"GIT_CEILING_DIRECTORIES".to_string()), "{envs:?}");
        assert!(envs.contains(&"GIT_CONFIG_NOSYSTEM".to_string()), "{envs:?}");
    }

    #[test]
    fn find_program_mirrors_execvp_failures() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_program("sh", dir.path()).unwrap().parent().map(|p| p.to_path_buf()), Some(PathBuf::from("/usr/bin")));
        let err = find_program("no-such-program-xyz", dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        fs::write(dir.path().join("run.sh"), "#!/bin/sh\n").unwrap();
        let err = find_program("./run.sh", dir.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        fs::set_permissions(dir.path().join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_program("./run.sh", dir.path()).unwrap(), dir.path().join("./run.sh"));
        assert_eq!(find_program("/nonexistent/prog", dir.path()).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn is_mount_point_tells_a_mount_root_from_a_directory() {
        assert!(is_mount_point(Path::new("/proc")));
        assert!(!is_mount_point(Path::new("/proc/self")));
        assert!(!is_mount_point(Path::new("/nonexistent")));
    }

    #[test]
    fn chown_tree_owns_every_entry_and_never_follows_a_symlink() {
        if !rustix::process::geteuid().is_root() {
            println!("SKIPPED: chown_tree needs root (the compose test service runs as root)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "x").unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(tree.join("a/b")).unwrap();
        fs::write(tree.join("a/b/f"), "x").unwrap();
        symlink(&outside, tree.join("a/link")).unwrap();
        chown_tree(&tree, BUILDER_UID, BUILDER_UID).unwrap();
        for p in [tree.clone(), tree.join("a"), tree.join("a/b"), tree.join("a/b/f"), tree.join("a/link")] {
            let m = fs::symlink_metadata(&p).unwrap();
            assert_eq!((m.uid(), m.gid()), (BUILDER_UID, BUILDER_UID), "{}", p.display());
        }
        assert_eq!(fs::metadata(&outside).unwrap().uid(), 0, "the symlink's target was chowned");
    }
}
