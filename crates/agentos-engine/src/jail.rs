//! The jail the official `jailer` builds around Firecracker: chroot, uid/gid drop, cgroup v2
//! limits. The mode is decided by the controller and carried in `FirecrackerConfig`; the
//! worker never probes or decides, it only executes the mode it was given.
//!
//! In order: `plan` (the chroot and cgroup paths), `stage` (hard links into the chroot, the
//! chroot's `vm.json`, the cgroup marker), the jailer's argv (`jailer_args`), and, after
//! Firecracker has exited, `collect` (the cgroup and the chroot). `probe`/`probe_with` and
//! `decide` are the controller's: whether this host can jail at all.
//!
//! Only `collect` removes anything, and it never follows the marker outside
//! `<cgroup_root>/agentos/`.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use rustix::fs::{chown, statvfs, Gid, StatVfsMountFlags, Uid};
use rustix::io::Errno;
use rustix::process::geteuid;
use serde::{Deserialize, Serialize};

use crate::firecracker::VmView;
use crate::guestlink::guest_text;

/// The uid/gid Firecracker runs as when jailed (unless `--jail-uid`/`--jail-gid`).
pub const JAIL_UID: u32 = 61000;
pub const JAIL_GID: u32 = 61000;
/// The parent cgroup of every jailed VM: `<cgroup_root>/agentos/<id>`.
pub const JAIL_PARENT_CGROUP: &str = "agentos";
/// `memory.max` = guest memory plus this, for Firecracker's own footprint.
pub const JAIL_MEMORY_OVERHEAD_MIB: u32 = 128;
pub const JAIL_PIDS_MAX: u32 = 64;
/// `RLIMIT_FSIZE` of the jailed process (= `WS_IMAGE_BYTES`).
pub const JAIL_FSIZE_BYTES: u64 = 1 << 30;
/// `cpu.max` period; the quota is `vcpus × CPU_PERIOD_US`.
pub const CPU_PERIOD_US: u64 = 100_000;
/// The jailer's `--chroot-base-dir`, under the job (or inspect) directory.
pub const JAIL_DIR: &str = "jail";
/// The file naming the VM's cgroup, under the job (or inspect) directory.
pub const JAIL_MARKER: &str = "jail/cgroup";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JailMode {
    Jailed(JailConfig),
    Unjailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JailConfig {
    /// Absolute; `--version` must print `Jailer v1.17.`.
    pub jailer_bin: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// The cgroup v2 mount point found by the probe (`/sys/fs/cgroup`).
    pub cgroup_root: PathBuf,
}

/// What `jailer --version` must start with.
pub const JAILER_VERSION_PREFIX: &str = "Jailer v1.17.";
/// How long `jailer --version` may take.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// The controllers the jail's limits need.
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
/// The longest marker read (a cgroup path).
const MARKER_LIMIT: u64 = 4096;

/// Where one VM's jail lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailPlan {
    /// The jailer's `--id`: the attempt id, or `inspect-<uuid>`.
    pub id: String,
    /// The file name of the Firecracker binary (the jailer's chroot level).
    pub exec_name: String,
    /// `<dir>/jail`, the jailer's `--chroot-base-dir`.
    pub base: PathBuf,
    /// `<base>/<exec_name>/<id>/root`.
    pub chroot: PathBuf,
    /// `<cgroup_root>/agentos/<id>`.
    pub cgroup: PathBuf,
}

/// The jailer's id rule: `[A-Za-z0-9-]{1,64}`.
fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// The jail of the VM `id` run from `dir` (a job or an inspect directory).
pub fn plan(cfg: &JailConfig, firecracker_bin: &Path, dir: &Path, id: &str) -> Result<JailPlan, String> {
    if !valid_id(id) {
        return Err(format!("invalid jail id {}", guest_text(&format!("{id:?}"))));
    }
    let exec_name = firecracker_bin
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no UTF-8 file name", firecracker_bin.display()))?
        .to_string();
    let base = dir.join(JAIL_DIR);
    let chroot = base.join(&exec_name).join(id).join("root");
    let cgroup = cfg.cgroup_root.join(JAIL_PARENT_CGROUP).join(id);
    Ok(JailPlan { id: id.to_string(), exec_name, base, chroot, cgroup })
}

/// The cgroup v2 files the jailer writes, from the contract's `worker_vcpus` and
/// `worker_memory_mib`.
pub fn cgroup_values(vcpus: u32, memory_mib: u32) -> [(&'static str, String); 4] {
    let quota = u64::from(vcpus) * CPU_PERIOD_US;
    let memory = (u64::from(memory_mib) + u64::from(JAIL_MEMORY_OVERHEAD_MIB)) * 1024 * 1024;
    [
        ("cpu.max", format!("{quota} {CPU_PERIOD_US}")),
        ("memory.max", memory.to_string()),
        ("memory.swap.max", "0".to_string()),
        ("pids.max", JAIL_PIDS_MAX.to_string()),
    ]
}

/// The jailer's argv, exactly: never `--new-pid-ns` (the jailer would fork and its parent
/// exit), `--daemonize` or `--netns`; the jailer passes `--id` to Firecracker itself.
pub fn jailer_args(cfg: &JailConfig, plan: &JailPlan, firecracker_bin: &Path, vcpus: u32, memory_mib: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "--id".into(),
        plan.id.clone().into(),
        "--exec-file".into(),
        firecracker_bin.into(),
        "--uid".into(),
        cfg.uid.to_string().into(),
        "--gid".into(),
        cfg.gid.to_string().into(),
        "--chroot-base-dir".into(),
        plan.base.clone().into(),
        "--cgroup-version".into(),
        "2".into(),
        "--parent-cgroup".into(),
        JAIL_PARENT_CGROUP.into(),
    ];
    for (file, value) in cgroup_values(vcpus, memory_mib) {
        args.push("--cgroup".into());
        args.push(format!("{file}={value}").into());
    }
    args.extend(["--resource-limit".into(), format!("fsize={JAIL_FSIZE_BYTES}").into()]);
    args.extend(["--", "--no-api", "--config-file", "/vm.json"].map(OsString::from));
    args
}

/// The files `stage` puts into the chroot.
pub struct StageSources<'a> {
    /// `<image_dir>/vmlinux` (registry: root-owned `0444`; never re-owned here).
    pub kernel: &'a Path,
    /// `<image_dir>/rootfs.squashfs` (as `kernel`).
    pub rootfs: &'a Path,
    /// `<work_root>/<task>/ws.img`.
    pub ws_img: &'a Path,
    /// `<dir>/scratch.img`.
    pub scratch_img: &'a Path,
    /// The chroot view's `vm.json`.
    pub vm_json: &'a serde_json::Value,
}

fn prepare_err(step: &str, e: io::Error) -> String {
    format!("cannot prepare the jail: {step}: {e}")
}

/// Hard-links `src` to `dst`: never a copy (the image would be duplicated and `ws.img` would
/// no longer be the task's).
/// Only a regular file is linked: `hard_link` would link a symlink itself, and the chown and
/// chmod that follow would then reach whatever it points at.
fn link(src: &Path, dst: &Path) -> Result<(), String> {
    match fs::symlink_metadata(src) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err(format!("cannot prepare the jail: {} is not a regular file", src.display())),
        Err(e) => return Err(prepare_err(&format!("stat {}", src.display()), e)),
    }
    fs::hard_link(src, dst).map_err(|e| match e.kind() {
        io::ErrorKind::CrossesDevices => {
            format!("cannot prepare the jail: {} and {} are on different filesystems", src.display(), dst.display())
        }
        _ => prepare_err(&format!("link {} to {}", src.display(), dst.display()), e),
    })?;
    // The link itself, so a source swapped for a symlink after the check is refused too.
    if !fs::symlink_metadata(dst).is_ok_and(|m| m.is_file()) {
        let _ = fs::remove_file(dst);
        return Err(format!("cannot prepare the jail: {} is not a regular file", src.display()));
    }
    Ok(())
}

/// Gives `path` to the jail's uid/gid with `mode`.
fn hand_over(cfg: &JailConfig, path: &Path, mode: u32) -> Result<(), String> {
    chown(path, Some(Uid::from_raw(cfg.uid)), Some(Gid::from_raw(cfg.gid)))
        .map_err(io::Error::from)
        .map_err(|e| prepare_err(&format!("chown {}", path.display()), e))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| prepare_err(&format!("chmod {}", path.display()), e))
}

/// Builds the chroot before the jailer runs: exactly `vmlinux`, `rootfs.squashfs`, `ws.img`,
/// `scratch.img` (hard links), `firecracker.log` (hard-linked to `<dir>/firecracker.log`) and
/// `vm.json`; then the marker `<dir>/jail/cgroup` naming the VM's cgroup. `ws.img` and
/// `scratch.img` (shared inodes, so at their own paths too) and the new files belong to the
/// jail's uid; the registry files keep their owner and mode.
pub fn stage(cfg: &JailConfig, plan: &JailPlan, dir: &Path, src: StageSources) -> Result<(), String> {
    // The jailer copies the binary there itself and refuses an existing file.
    let exec_copy = plan.chroot.join(&plan.exec_name);
    if fs::symlink_metadata(&exec_copy).is_ok() {
        return Err(format!("cannot prepare the jail: stale jail: {} exists", exec_copy.display()));
    }
    fs::create_dir_all(&plan.chroot).map_err(|e| prepare_err(&format!("create {}", plan.chroot.display()), e))?;
    link(src.kernel, &plan.chroot.join("vmlinux"))?;
    link(src.rootfs, &plan.chroot.join("rootfs.squashfs"))?;
    for (from, name) in [(src.ws_img, "ws.img"), (src.scratch_img, "scratch.img")] {
        let to = plan.chroot.join(name);
        link(from, &to)?;
        hand_over(cfg, &to, 0o600)?;
    }
    let log = plan.chroot.join("firecracker.log");
    File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| prepare_err(&format!("create {}", log.display()), e))?;
    hand_over(cfg, &log, 0o600)?;
    link(&log, &dir.join("firecracker.log"))?;
    let vm_json = plan.chroot.join("vm.json");
    // A `Value` serializes its keys alphabetically, not in the documented order: harmless,
    // Firecracker reads the document by key.
    let mut bytes = serde_json::to_vec_pretty(src.vm_json).map_err(|e| prepare_err("vm.json", io::Error::other(e)))?;
    bytes.push(b'\n');
    fs::write(&vm_json, bytes).map_err(|e| prepare_err(&format!("write {}", vm_json.display()), e))?;
    hand_over(cfg, &vm_json, 0o644)?;
    let marker = dir.join(JAIL_MARKER);
    fs::write(&marker, format!("{}\n", plan.cgroup.display())).map_err(|e| prepare_err(&format!("write {}", marker.display()), e))
}

/// The `vm.json` paths inside the chroot. The socket is `v.sock` relative to Firecracker's
/// working directory, the chroot root (as unjailed, relative to the job directory).
pub fn chroot_view(_plan: &JailPlan) -> VmView {
    VmView {
        kernel: "/vmlinux".into(),
        rootfs: "/rootfs.squashfs".into(),
        ws_img: "/ws.img".into(),
        scratch_img: "/scratch.img".into(),
        uds: "v.sock".into(),
        log: "/firecracker.log".into(),
    }
}

/// The VM's socket as the host reaches it.
pub fn host_uds(plan: &JailPlan) -> PathBuf {
    plan.chroot.join("v.sock")
}

/// `/proc/mounts`' octal escapes (`\040` for a space).
fn unescape_mount_field(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = (b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)))
            .then(|| b[i + 1..i + 4].iter().fold(0u32, |v, c| v * 8 + u32::from(c - b'0')))
            .and_then(|v| u8::try_from(v).ok());
        if let Some(v) = octal {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount point of the first `cgroup2` line of `/proc/mounts`.
pub fn find_cgroup2_root(proc_mounts: &str) -> Option<PathBuf> {
    proc_mounts.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.len() >= 3 && fields[2] == "cgroup2").then(|| PathBuf::from(unescape_mount_field(fields[1])))
    })
}

/// What the probe looked at, in its order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFacts {
    pub euid: u32,
    /// The first line of a successful `jailer --version`, or why there is none.
    pub jailer_version: Result<String, String>,
    pub cgroup_root: Option<PathBuf>,
    /// `<cgroup_root>/cgroup.controllers`.
    pub controllers: Vec<String>,
    /// Creating `<root>/agentos` and enabling `+cpu +memory +pids` in both
    /// `cgroup.subtree_control`s: `(errno, message)` of the first failure.
    pub delegation: Result<(), (i32, String)>,
    /// The directory `nodev`/`noexec` are about (`<home>/jobs` or `<home>/inspect`).
    pub jail_base: PathBuf,
    pub nodev: bool,
    pub noexec: bool,
    /// Two of jobs/inspect, work and the image on different devices.
    pub same_device: Option<(PathBuf, PathBuf)>,
}

/// The spec's five steps in order; the first failure, worded as the spec words it.
pub fn probe_with(f: &ProbeFacts) -> Result<(), String> {
    if f.euid != 0 {
        return Err(format!("needs root (euid 0), running as uid {}", f.euid));
    }
    match &f.jailer_version {
        Err(e) => return Err(format!("jailer --version: {e}")),
        Ok(v) if !v.starts_with(JAILER_VERSION_PREFIX) => {
            return Err(format!("jailer --version: expected {JAILER_VERSION_PREFIX}, got {}", guest_text(v)));
        }
        Ok(_) => {}
    }
    let Some(root) = &f.cgroup_root else {
        return Err("no cgroup v2 hierarchy in /proc/mounts".into());
    };
    let missing: Vec<&str> = CONTROLLERS.into_iter().filter(|c| !f.controllers.iter().any(|have| have == c)).collect();
    if !missing.is_empty() {
        return Err(format!("controllers missing in {}: {}", root.join("cgroup.controllers").display(), missing.join(", ")));
    }
    if let Err((errno, msg)) = &f.delegation {
        let root = root.display();
        return Err(if *errno == Errno::ROFS.raw_os_error() {
            format!("cgroup v2 hierarchy {root} is read-only")
        } else if *errno == Errno::BUSY.raw_os_error() {
            format!(
                "cannot delegate cpu, memory, pids in {root}: {msg} (the root cgroup has processes of its own; scripts/kvm-entrypoint.sh shows the delegation)"
            )
        } else {
            format!("cannot delegate cpu, memory, pids in {root}: {msg}")
        });
    }
    if f.nodev {
        return Err(format!("jail base {} is on a nodev filesystem", f.jail_base.display()));
    }
    if f.noexec {
        return Err(format!("jail base {} is on a noexec filesystem", f.jail_base.display()));
    }
    if let Some((a, b)) = &f.same_device {
        return Err(format!("{} and {} are on different filesystems: the jail hard-links them", a.display(), b.display()));
    }
    Ok(())
}

/// `e`'s message without std's ` (os error N)` suffix.
fn os_message(e: &io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// `path`, or its nearest ancestor that exists (a home's `jobs` may not exist yet).
fn existing(path: &Path) -> &Path {
    let mut p = path;
    while fs::symlink_metadata(p).is_err() {
        match p.parent() {
            Some(parent) => p = parent,
            None => break,
        }
    }
    p
}

fn jailer_version(jailer_bin: &Path) -> Result<String, String> {
    let meta = fs::metadata(jailer_bin).map_err(|e| format!("{}: {e}", jailer_bin.display()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} is not an executable file", jailer_bin.display()));
    }
    let mut cmd = Command::new(jailer_bin);
    cmd.arg("--version");
    let (status, first) = crate::firecracker::first_stdout_line(cmd, VERSION_TIMEOUT)?;
    if !status.success() {
        return Err(format!("{} ({status})", guest_text(&first)));
    }
    Ok(first)
}

/// The writes the jailer will make: `<root>/agentos`, `+cpu +memory +pids` in the root's and
/// in `agentos`'s `cgroup.subtree_control`.
fn delegate(root: &Path) -> Result<(), (i32, String)> {
    let err = |e: io::Error| (e.raw_os_error().unwrap_or(0), os_message(&e));
    let parent = root.join(JAIL_PARENT_CGROUP);
    fs::create_dir_all(&parent).map_err(err)?;
    for dir in [root, parent.as_path()] {
        fs::write(dir.join("cgroup.subtree_control"), "+cpu +memory +pids").map_err(err)?;
    }
    Ok(())
}

/// Collects the facts for real and applies `probe_with`. The delegation writes are only
/// attempted once every earlier step passed (as root, with a v1.17 jailer and the
/// controllers present), so a probe that fails earlier never writes to the cgroup tree.
pub fn probe(cfg: &JailConfig, jobs: &Path, inspect: &Path, work: &Path, image_dir: &Path) -> Result<(), String> {
    let euid = geteuid().as_raw();
    let jailer_version = jailer_version(&cfg.jailer_bin);
    let cgroup_root = fs::read_to_string("/proc/mounts").ok().as_deref().and_then(find_cgroup2_root);
    let controllers: Vec<String> = cgroup_root
        .as_ref()
        .and_then(|r| fs::read_to_string(r.join("cgroup.controllers")).ok())
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let mut facts = ProbeFacts {
        euid,
        jailer_version,
        cgroup_root,
        controllers,
        delegation: Ok(()),
        jail_base: jobs.to_path_buf(),
        nodev: false,
        noexec: false,
        same_device: None,
    };
    // With the later facts still at their passing defaults, this is steps 1-3.
    if probe_with(&facts).is_ok()
        && let Some(root) = &facts.cgroup_root
    {
        facts.delegation = delegate(root);
    }
    for base in [jobs, inspect] {
        let flags = statvfs(existing(base)).map(|s| s.f_flag).unwrap_or(StatVfsMountFlags::empty());
        if flags.intersects(StatVfsMountFlags::NODEV | StatVfsMountFlags::NOEXEC) {
            facts.jail_base = base.to_path_buf();
            facts.nodev = flags.contains(StatVfsMountFlags::NODEV);
            facts.noexec = flags.contains(StatVfsMountFlags::NOEXEC);
            break;
        }
    }
    let dev = |p: &Path| fs::metadata(existing(p)).map(|m| m.dev()).ok();
    facts.same_device = [(jobs, work), (jobs, image_dir), (inspect, work), (inspect, image_dir)]
        .into_iter()
        .find(|(a, b)| dev(a) != dev(b))
        .map(|(a, b)| (a.to_path_buf(), b.to_path_buf()));
    probe_with(&facts)
}

/// The controller's decision from the probe's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailDecision {
    Jailed,
    Unjailed { reason: String },
}

pub fn decide(probe: Result<(), String>, allow_unjailed: bool) -> Result<JailDecision, String> {
    match probe {
        Ok(()) => Ok(JailDecision::Jailed),
        Err(reason) if allow_unjailed => Ok(JailDecision::Unjailed { reason }),
        Err(reason) => Err(format!(
            "jailer unavailable: {reason}; pass --allow-unjailed to run Firecracker without a jail as the current user"
        )),
    }
}

/// What `collect` removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collected {
    pub cgroup_removed: bool,
    pub jail_removed: bool,
}

/// The cgroup the marker names, if it is exactly `<cgroup_root>/agentos/<one name>`.
fn cgroup_under(cgroup_root: &Path, named: &str) -> Option<PathBuf> {
    let path = Path::new(named);
    let rest = path.strip_prefix(cgroup_root.join(JAIL_PARENT_CGROUP)).ok()?;
    let mut parts = rest.components();
    match (path.is_absolute(), parts.next(), parts.next()) {
        (true, Some(Component::Normal(_)), None) => Some(path.to_path_buf()),
        _ => None,
    }
}

/// The marker's text (bounded, one trailing newline dropped); `None` if there is none.
fn read_marker(marker: &Path) -> io::Result<Option<String>> {
    let file = match File::open(marker) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.take(MARKER_LIMIT).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(Some(text.strip_suffix('\n').unwrap_or(&text).to_string()))
}

/// After the VM has exited (never before: `rmdir` of a cgroup with a live process fails and a
/// chroot under a live VM must not be touched): removes the cgroup named by `<dir>/jail/cgroup`
/// (which must lie under `<cgroup_root>/agentos/`, else nothing at all is removed), then
/// `<dir>/jail`. A cgroup that still has processes is an error and leaves `jail/` alone.
pub fn collect(dir: &Path, cgroup_root: &Path) -> Result<Collected, String> {
    let marker = dir.join(JAIL_MARKER);
    let mut cgroup_removed = false;
    match read_marker(&marker) {
        Ok(None) => {}
        Ok(Some(named)) => {
            let Some(cgroup) = cgroup_under(cgroup_root, &named) else {
                return Err(format!("marker names a path outside the cgroup root: {}", guest_text(&named)));
            };
            match fs::remove_dir(&cgroup) {
                Ok(()) => cgroup_removed = true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) if matches!(e.kind(), io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::ResourceBusy) => {
                    return Err(format!("cgroup {} still has processes", guest_text(&cgroup.display().to_string())));
                }
                Err(e) => return Err(format!("cannot remove cgroup {}: {e}", guest_text(&cgroup.display().to_string()))),
            }
        }
        Err(e) => return Err(format!("cannot read {}: {e}", marker.display())),
    }
    let jail = dir.join(JAIL_DIR);
    let jail_removed = match fs::remove_dir_all(&jail) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("cannot remove {}: {e}", jail.display())),
    };
    Ok(Collected { cgroup_removed, jail_removed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firecracker::{render_vm_json, FirecrackerConfig};
    use crate::guestlink::GuestLauncher;
    use agentos_core::ids::Digest;
    use std::ffi::OsString;
    use std::path::Path;

    fn jail_config() -> JailConfig {
        JailConfig { jailer_bin: "/home/x/bin/jailer".into(), uid: JAIL_UID, gid: JAIL_GID, cgroup_root: "/sys/fs/cgroup".into() }
    }

    const UUID: &str = "0b8a5f3e-7c1d-4e2a-9f6b-3d5c7e9a1b2c";

    #[test]
    fn plan_lays_out_chroot_and_cgroup_under_the_dir() {
        let p = plan(&jail_config(), Path::new("/home/x/bin/firecracker"), Path::new("/home/x/jobs/e-a"), UUID).unwrap();
        assert_eq!(p.id, UUID);
        assert_eq!(p.exec_name, "firecracker");
        assert_eq!(p.base, Path::new("/home/x/jobs/e-a/jail"));
        assert_eq!(p.chroot, Path::new("/home/x/jobs/e-a/jail/firecracker").join(UUID).join("root"));
        assert_eq!(p.cgroup, Path::new("/sys/fs/cgroup/agentos").join(UUID));
        assert_eq!(host_uds(&p), p.chroot.join("v.sock"));
        // The executable's own name names the chroot level.
        let p = plan(&jail_config(), Path::new("/opt/fc/firecracker-v1.17.0"), Path::new("/d"), "x").unwrap();
        assert_eq!(p.exec_name, "firecracker-v1.17.0");
        assert_eq!(p.chroot, Path::new("/d/jail/firecracker-v1.17.0/x/root"));
    }

    #[test]
    fn plan_validates_the_jailer_id_rule() {
        let fc = Path::new("/bin/firecracker");
        let ok = |id: &str| plan(&jail_config(), fc, Path::new("/d"), id);
        ok(UUID).unwrap();
        ok(&format!("inspect-{UUID}")).unwrap();
        ok(&"a".repeat(64)).unwrap();
        for bad in ["a".repeat(65), "a_b".into(), "a/b".into(), "a.b".into(), String::new()] {
            let err = ok(&bad).unwrap_err();
            assert_eq!(err, format!("invalid jail id {bad:?}"));
        }
    }

    #[test]
    fn cgroup_values_for_the_contract() {
        assert_eq!(
            cgroup_values(1, 128),
            [
                ("cpu.max", "100000 100000".to_string()),
                ("memory.max", "268435456".to_string()),
                ("memory.swap.max", "0".to_string()),
                ("pids.max", "64".to_string()),
            ]
        );
        let v = cgroup_values(4, 2048);
        assert_eq!(v[0].1, "400000 100000");
        assert_eq!(v[1].1, "2281701376");
        let v = cgroup_values(32, 65536);
        assert_eq!(v[0].1, "3200000 100000");
        assert_eq!(v[1].1, ((65536u64 + 128) * 1048576).to_string());
    }

    #[test]
    fn jailer_argv_is_exactly_the_documented_one() {
        let cfg = JailConfig { uid: 61001, gid: 61002, ..jail_config() };
        let fc = Path::new("/home/x/bin/firecracker");
        let p = plan(&cfg, fc, Path::new("/home/x/jobs/e-a"), UUID).unwrap();
        let args = jailer_args(&cfg, &p, fc, 2, 512);
        let expected: Vec<OsString> = [
            "--id",
            UUID,
            "--exec-file",
            "/home/x/bin/firecracker",
            "--uid",
            "61001",
            "--gid",
            "61002",
            "--chroot-base-dir",
            "/home/x/jobs/e-a/jail",
            "--cgroup-version",
            "2",
            "--parent-cgroup",
            "agentos",
            "--cgroup",
            "cpu.max=200000 100000",
            "--cgroup",
            "memory.max=671088640",
            "--cgroup",
            "memory.swap.max=0",
            "--cgroup",
            "pids.max=64",
            "--resource-limit",
            "fsize=1073741824",
            "--",
            "--no-api",
            "--config-file",
            "/vm.json",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args, expected);
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(sep, 24, "24 elements before --");
        assert_eq!(&args[sep + 1..], ["--no-api", "--config-file", "/vm.json"].map(OsString::from));
        for banned in ["--new-pid-ns", "--daemonize", "--netns"] {
            assert!(!args.iter().any(|a| a == banned), "{banned}");
        }
        assert_eq!(args.iter().filter(|a| *a == "--id").count(), 1);
    }

    #[test]
    fn find_cgroup2_root_over_host_container_and_none() {
        let host = "proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\n\
                    sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0\n\
                    cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime,nsdelegate,memory_recursiveprot 0 0\n";
        assert_eq!(find_cgroup2_root(host), Some(PathBuf::from("/sys/fs/cgroup")));
        let container = "overlay / overlay rw,relatime,lowerdir=/a,upperdir=/b,workdir=/c 0 0\n\
                         cgroup /sys/fs/cgroup cgroup2 ro,nosuid,nodev,noexec,relatime,nsdelegate 0 0\n";
        assert_eq!(find_cgroup2_root(container), Some(PathBuf::from("/sys/fs/cgroup")));
        let v1 = "tmpfs /sys/fs/cgroup tmpfs ro,nosuid,nodev,noexec,mode=755 0 0\n\
                  cgroup /sys/fs/cgroup/memory cgroup rw,nosuid,nodev,noexec,relatime,memory 0 0\n\
                  cgroup /sys/fs/cgroup/pids cgroup rw,nosuid,nodev,noexec,relatime,pids 0 0\n";
        assert_eq!(find_cgroup2_root(v1), None);
        assert_eq!(find_cgroup2_root(""), None);
    }

    /// Facts that pass every step.
    fn good() -> ProbeFacts {
        ProbeFacts {
            euid: 0,
            jailer_version: Ok("Jailer v1.17.0".into()),
            cgroup_root: Some("/sys/fs/cgroup".into()),
            controllers: ["cpuset", "cpu", "io", "memory", "hugetlb", "pids", "rdma"].map(String::from).to_vec(),
            delegation: Ok(()),
            jail_base: "/home/x/jobs".into(),
            nodev: false,
            noexec: false,
            same_device: None,
        }
    }

    /// Facts that fail every step from `step` (1-based) on, so a row also proves that the
    /// earlier step is reported before every later one.
    fn failing_from(step: u32) -> ProbeFacts {
        let mut f = good();
        if step <= 1 {
            f.euid = 1000;
        }
        if step <= 2 {
            f.jailer_version = Ok("Jailer v1.16.0".into());
        }
        if step <= 3 {
            f.cgroup_root = None;
        }
        if step <= 4 {
            f.delegation = Err((30, "Read-only file system".into()));
        }
        if step <= 5 {
            f.nodev = true;
            f.noexec = true;
            f.same_device = Some(("/home/x/jobs".into(), "/mnt/images".into()));
        }
        f
    }

    #[test]
    fn probe_with_reports_the_first_failing_step_in_order() {
        let hint = "(the root cgroup has processes of its own; scripts/kvm-entrypoint.sh shows the delegation)";
        let rows: Vec<(ProbeFacts, Result<(), String>)> = vec![
            (failing_from(1), Err("needs root (euid 0), running as uid 1000".into())),
            (
                ProbeFacts { jailer_version: Err("/x/jailer: No such file or directory".into()), ..failing_from(3) },
                Err("jailer --version: /x/jailer: No such file or directory".into()),
            ),
            (failing_from(2), Err("jailer --version: expected Jailer v1.17., got Jailer v1.16.0".into())),
            (failing_from(3), Err("no cgroup v2 hierarchy in /proc/mounts".into())),
            (
                ProbeFacts { controllers: ["cpu", "memory"].map(String::from).to_vec(), ..failing_from(4) },
                Err("controllers missing in /sys/fs/cgroup/cgroup.controllers: pids".into()),
            ),
            (
                ProbeFacts { controllers: vec!["io".into()], ..failing_from(4) },
                Err("controllers missing in /sys/fs/cgroup/cgroup.controllers: cpu, memory, pids".into()),
            ),
            (failing_from(4), Err("cgroup v2 hierarchy /sys/fs/cgroup is read-only".into())),
            (
                ProbeFacts { delegation: Err((16, "Device or resource busy".into())), ..failing_from(5) },
                Err(format!("cannot delegate cpu, memory, pids in /sys/fs/cgroup: Device or resource busy {hint}")),
            ),
            (
                ProbeFacts { delegation: Err((95, "Operation not supported".into())), ..failing_from(5) },
                Err("cannot delegate cpu, memory, pids in /sys/fs/cgroup: Operation not supported".into()),
            ),
            (failing_from(5), Err("jail base /home/x/jobs is on a nodev filesystem".into())),
            (ProbeFacts { nodev: false, ..failing_from(5) }, Err("jail base /home/x/jobs is on a noexec filesystem".into())),
            (
                ProbeFacts { nodev: false, noexec: false, ..failing_from(5) },
                Err("/home/x/jobs and /mnt/images are on different filesystems: the jail hard-links them".into()),
            ),
            (good(), Ok(())),
        ];
        for (facts, expected) in rows {
            assert_eq!(probe_with(&facts), expected, "{facts:?}");
        }
    }

    #[test]
    fn decide_table() {
        assert_eq!(decide(Ok(()), false), Ok(JailDecision::Jailed));
        assert_eq!(decide(Ok(()), true), Ok(JailDecision::Jailed));
        assert_eq!(decide(Err("needs root".into()), true), Ok(JailDecision::Unjailed { reason: "needs root".into() }));
        assert_eq!(
            decide(Err("needs root (euid 0), running as uid 1000".into()), false),
            Err("jailer unavailable: needs root (euid 0), running as uid 1000; pass --allow-unjailed to run Firecracker without a jail as the current user".into())
        );
    }

    #[test]
    fn chroot_view_is_the_golden_jailed_vm_json() {
        let cfg = FirecrackerConfig {
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
            launcher: GuestLauncher::Real { firecracker_bin: "/home/x/bin/firecracker".into() },
            jail: JailMode::Jailed(jail_config()),
        };
        let p = plan(&jail_config(), &cfg.firecracker_bin, Path::new("/home/x/jobs/effect-1-attempt-1"), UUID).unwrap();
        let rendered = render_vm_json(&cfg, &chroot_view(&p));
        let golden: serde_json::Value = serde_json::from_str(include_str!("../tests/golden/vm.jailed.json")).unwrap();
        assert_eq!(rendered, golden);
        let mut paths = vec![rendered["boot-source"]["kernel_image_path"].clone(), rendered["logger"]["log_path"].clone()];
        paths.extend(rendered["drives"].as_array().unwrap().iter().map(|d| d["path_on_host"].clone()));
        for p in paths {
            let p = p.as_str().unwrap();
            assert!(p.starts_with('/') && !p[1..].contains('/'), "{p} is not a chroot-root name");
        }
        // The socket is relative to Firecracker's working directory, the chroot root
        // (Task 5 ruling: the same rendering as unjailed).
        assert_eq!(rendered["vsock"]["uds_path"], "v.sock");
    }

    #[test]
    fn collect_refuses_a_marker_outside_the_cgroup_root() {
        let dir = tempfile::tempdir().unwrap();
        let cg = dir.path().join("cgroup");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(cg.join("agentos/x")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let job = dir.path().join("job");
        std::fs::create_dir_all(job.join("jail/firecracker/x/root")).unwrap();
        for marker in [
            "/tmp/elsewhere".to_string(),
            format!("{}", elsewhere.display()),
            format!("{}/agentos/../../elsewhere", cg.display()),
            format!("{}/agentos", cg.display()),
            format!("{}/agentos/x/y", cg.display()),
            "agentos/x".to_string(),
        ] {
            std::fs::write(job.join(JAIL_MARKER), format!("{marker}\n")).unwrap();
            let err = collect(&job, &cg).unwrap_err();
            assert!(err.contains("outside the cgroup root"), "{marker}: {err}");
            assert!(elsewhere.is_dir() && cg.join("agentos/x").is_dir(), "{marker}: something was removed");
            assert!(job.join("jail/firecracker/x/root").is_dir(), "{marker}: jail/ was touched");
        }
        // Control characters in the marker are escaped in the error.
        std::fs::write(job.join(JAIL_MARKER), "/x\n\u{1b}[2J").unwrap();
        let err = collect(&job, &cg).unwrap_err();
        assert!(!err.chars().any(char::is_control), "{err:?}");
    }
}
