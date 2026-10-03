//! The guest's PID 1: `agentos-guest` with no arguments, started by the kernel
//! (`init=/sbin/agentos-guest`). It prepares the runtime (mounts, scratch drive, device
//! permissions, hostname), listens on vsock port 5200, serves the bound session with the
//! `VmBackend`, and powers the VM off when the session ends, when no `Hello` came within
//! `HELLO_WATCHDOG` of boot, or when anything before the listener fails (the host then sees
//! "guest did not come up"). Every step is a plain syscall through rustix or a child process
//! with an absolute path: PID 1 has no `PATH`.
//!
//! Networking: the VM has no NIC; only the kernel's own `lo` exists, and init configures
//! nothing (`ip` is not in the image). That is enough for any connection attempt by the check
//! to fail with `ENETUNREACH`.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Mutex;
use std::thread;

use agentos_core::guest::{HELLO_WATCHDOG, TMPFS_SIZE, VSOCK_PORT};
use rustix::fs::{syncfs, Mode};
use rustix::mount::{mount, unmount, MountFlags, UnmountFlags};
use rustix::system::{reboot, sethostname, RebootCommand};
use vsock::{VsockListener, VsockStream, VMADDR_CID_ANY};

use crate::agent::{spawn_watchdog, Exit, Session};
use crate::backend::{
    chown_tree, is_mount_point, mkfs_ext4, mount_ext4, VmBackend, AGENT_OOM_SCORE_ADJ, CHECK_UID, SCRATCH_DEVICE,
    SCRATCH_DIR, WORKSPACE_DEVICE, WORKSPACE_DIR,
};

pub const HOSTNAME: &str = "agentos-guest";
/// The check's own directory on the scratch drive (0700, `check`).
pub const CHECK_SCRATCH: &str = "/scratch/check";
/// The control channel: root only, so the check cannot talk to the host.
pub const VSOCK_DEVICE: &str = "/dev/vsock";

/// One runtime mount, as data, so the table can be checked without mounting anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub source: &'static str,
    pub target: &'static str,
    pub fstype: &'static str,
    pub flags: MountFlags,
    /// The filesystem options (`mount -o`), empty for none.
    pub data: String,
}

/// The five runtime mounts, in order: `/proc`, `/sys`, `/dev` (devtmpfs), `/run` and `/tmp`
/// (tmpfs, `TMPFS_SIZE` each, `/tmp` world-writable and sticky for the check).
pub fn mount_plan() -> Vec<MountSpec> {
    let kernel = MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC;
    let tmpfs = MountFlags::NOSUID | MountFlags::NODEV;
    vec![
        MountSpec { source: "proc", target: "/proc", fstype: "proc", flags: kernel, data: String::new() },
        MountSpec { source: "sysfs", target: "/sys", fstype: "sysfs", flags: kernel, data: String::new() },
        // Device nodes are the point of /dev: no `nodev` here.
        MountSpec {
            source: "devtmpfs",
            target: "/dev",
            fstype: "devtmpfs",
            flags: MountFlags::NOSUID | MountFlags::NOEXEC,
            data: "mode=0755".into(),
        },
        MountSpec { source: "tmpfs", target: "/run", fstype: "tmpfs", flags: tmpfs, data: format!("{TMPFS_SIZE},mode=0755") },
        MountSpec { source: "tmpfs", target: "/tmp", fstype: "tmpfs", flags: tmpfs, data: format!("{TMPFS_SIZE},mode=1777") },
    ]
}

/// Writes one line to the serial console (`console.log` on the host), falling back to
/// stderr before `/dev` exists. Callers pass our own wording plus error texts; the host
/// escapes everything it shows from the console.
pub fn log(msg: &str) {
    let line = format!("agentos-guest: {msg}\n");
    match File::options().write(true).open("/dev/console") {
        Ok(mut console) => {
            let _ = console.write_all(line.as_bytes());
        }
        Err(_) => eprint!("{line}"),
    }
}

fn mount_one(m: &MountSpec) -> Result<(), String> {
    // The kernel may have mounted devtmpfs on /dev already (CONFIG_DEVTMPFS_MOUNT).
    if is_mount_point(Path::new(m.target)) {
        return Ok(());
    }
    let data = (!m.data.is_empty()).then(|| CString::new(m.data.as_str())).transpose().map_err(|e| e.to_string())?;
    mount(m.source, m.target, m.fstype, m.flags, data.as_deref()).map_err(|e| format!("mount {} on {}: {e}", m.fstype, m.target))
}

/// Everything before the listener, in the spec's order.
fn boot() -> Result<VsockListener, String> {
    rustix::process::umask(Mode::from_raw_mode(0o022));
    for m in mount_plan() {
        mount_one(&m)?;
    }
    fs::write("/proc/self/oom_score_adj", AGENT_OOM_SCORE_ADJ.to_string()).map_err(|e| format!("oom_score_adj: {e}"))?;

    mkfs_ext4(SCRATCH_DEVICE)?;
    let scratch = Path::new(SCRATCH_DIR);
    mount_ext4(SCRATCH_DEVICE, scratch)?;
    fs::set_permissions(scratch, fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {SCRATCH_DIR}: {e}"))?;
    let check = Path::new(CHECK_SCRATCH);
    fs::DirBuilder::new().mode(0o700).create(check).map_err(|e| format!("mkdir {CHECK_SCRATCH}: {e}"))?;
    chown_tree(check, CHECK_UID, CHECK_UID).map_err(|e| format!("chown {CHECK_SCRATCH}: {e}"))?;
    fs::set_permissions(check, fs::Permissions::from_mode(0o700)).map_err(|e| format!("chmod {CHECK_SCRATCH}: {e}"))?;

    fs::set_permissions(VSOCK_DEVICE, fs::Permissions::from_mode(0o600)).map_err(|e| format!("chmod {VSOCK_DEVICE}: {e}"))?;
    sethostname(HOSTNAME.as_bytes()).map_err(|e| format!("sethostname: {e}"))?;

    // A job after `ReadSnapshot`, or an inspection, finds the workspace on its drive; a blank
    // `ws.img` does not mount, and the workspace stays "missing" until `ReadSnapshot`.
    if let Err(e) = mount_ext4(WORKSPACE_DEVICE, Path::new(WORKSPACE_DIR)) {
        log(&format!("no workspace yet ({e})"));
    }

    VsockListener::bind_with_cid_port(VMADDR_CID_ANY, VSOCK_PORT).map_err(|e| format!("vsock listen on port {VSOCK_PORT}: {e}"))
}

/// Ends the VM: `syncfs` and unmount `/workspace` and `/scratch`, then `reboot` (with
/// `reboot=k` Firecracker exits 0). Called once; concurrent callers wait here for the end.
pub fn power_off(why: &str) -> ! {
    static ONCE: Mutex<()> = Mutex::new(());
    let _only_one = ONCE.lock().unwrap_or_else(|p| p.into_inner());
    log(&format!("powering off: {why}"));
    for dir in [WORKSPACE_DIR, SCRATCH_DIR] {
        let path = Path::new(dir);
        if !is_mount_point(path) {
            continue;
        }
        if let Ok(f) = File::open(path)
            && let Err(e) = syncfs(&f)
        {
            log(&format!("syncfs {dir}: {e}"));
        }
        if let Err(e) = unmount(path, UnmountFlags::empty()) {
            // Still busy (a session thread mid-request): synced already, detach it.
            log(&format!("unmount {dir}: {e}"));
            let _ = unmount(path, UnmountFlags::DETACH);
        }
    }
    rustix::fs::sync();
    let err = reboot(RebootCommand::Restart);
    // Unreachable unless the kernel refused; PID 1 exiting panics the kernel, and `panic=1`
    // reboots it, which ends Firecracker just the same.
    log(&format!("reboot failed: {err:?}"));
    std::process::exit(1)
}

fn handle(stream: VsockStream) {
    let mut backend = VmBackend::default();
    // A panic would end only this thread and leave the VM running without its session.
    let served = panic::catch_unwind(AssertUnwindSafe(|| Session::serve(&mut backend, stream, None)));
    match served {
        Ok(Exit::Shutdown) => power_off("Shutdown"),
        Ok(Exit::Lost) => power_off("control connection lost"),
        Err(_) => power_off("the session panicked"),
        // Not the bound attempt: keep listening.
        Ok(Exit::Rejected) => {}
    }
}

/// PID 1. Never returns: the VM ends with `power_off`.
pub fn main() -> ! {
    // Armed first: the 10 s count from boot, whatever the preparation costs.
    spawn_watchdog(HELLO_WATCHDOG, || power_off("no Hello within the boot watchdog"));
    let listener = match boot() {
        Ok(l) => l,
        Err(e) => {
            log(&format!("boot failed: {e}"));
            power_off("boot failed");
        }
    };
    loop {
        match listener.accept() {
            // One thread per connection: a silent connection never blocks the next.
            Ok((stream, _)) => {
                thread::spawn(move || handle(stream));
            }
            Err(e) => power_off(&format!("vsock accept: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::guest::{BOOT_ARGS, INIT_PATH};

    #[test]
    fn mount_plan_names_the_five_runtime_mounts_in_order() {
        let plan = mount_plan();
        let rows: Vec<(&str, &str, &str)> = plan.iter().map(|m| (m.source, m.target, m.fstype)).collect();
        assert_eq!(
            rows,
            [
                ("proc", "/proc", "proc"),
                ("sysfs", "/sys", "sysfs"),
                ("devtmpfs", "/dev", "devtmpfs"),
                ("tmpfs", "/run", "tmpfs"),
                ("tmpfs", "/tmp", "tmpfs"),
            ]
        );
        for m in &plan {
            assert!(m.flags.contains(MountFlags::NOSUID), "{m:?}");
            assert!(!m.flags.contains(MountFlags::RDONLY), "{m:?}");
        }
        for m in &plan[..2] {
            assert!(m.flags.contains(MountFlags::NODEV | MountFlags::NOEXEC), "{m:?}");
        }
        assert!(!plan[2].flags.contains(MountFlags::NODEV), "/dev needs its device nodes");
        let tmpfs: Vec<&MountSpec> = plan.iter().filter(|m| m.fstype == "tmpfs").collect();
        assert_eq!(tmpfs.len(), 2);
        for m in &tmpfs {
            assert!(m.flags.contains(MountFlags::NODEV), "{m:?}");
            assert_eq!(m.data.split(',').next(), Some("size=64m"), "{m:?}");
            assert_eq!(m.data.split(',').next(), Some(TMPFS_SIZE));
        }
        assert_eq!(plan[4].data, "size=64m,mode=1777");
        assert_eq!(plan[3].data, "size=64m,mode=0755");
    }

    #[test]
    fn boot_args_in_core_match_the_init_path() {
        assert!(BOOT_ARGS.contains("init=/sbin/agentos-guest"));
        assert!(BOOT_ARGS.split_whitespace().any(|a| a == format!("init={INIT_PATH}")));
        // A guest reboot must end Firecracker, and a panic must reboot.
        for arg in ["reboot=k", "panic=1", "console=ttyS0"] {
            assert!(BOOT_ARGS.split_whitespace().any(|a| a == arg), "{arg}");
        }
    }

    #[test]
    fn the_check_and_the_agent_get_the_contract_priorities() {
        use crate::backend::{CHECK_NOFILE, CHECK_NPROC, CHECK_OOM_SCORE_ADJ};
        assert_eq!((CHECK_NPROC, CHECK_NOFILE, CHECK_OOM_SCORE_ADJ, AGENT_OOM_SCORE_ADJ), (256, 1024, 1000, -1000));
        assert_eq!(HELLO_WATCHDOG, std::time::Duration::from_secs(10));
    }
}
