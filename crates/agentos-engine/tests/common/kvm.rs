//! The KVM test gate. Every KVM test begins with
//! `let Some(kvm) = kvm::require() else { return };`.
//!
//! Without `AGENTOS_KVM_TESTS` the test is skipped loudly; with it, the tier was promised and
//! the gate **panics** with every reason it cannot run (never a silent skip): `/dev/kvm`, the
//! Firecracker binary, the jailer, the guest image and the jail probe. The KVM tier must be
//! able to jail.
//!
//! Self-contained (no `super::`/`common::` items): the CLI tests include this very file with
//! `#[path = "../../agentos-engine/tests/common/kvm.rs"] mod kvm;`.
#![allow(dead_code)]

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use agentos_core::guest::mint_attempt_token;
use agentos_core::workspace::workspace_digest;
use agentos_engine::firecracker::FirecrackerConfig;
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::jail::{self, find_cgroup2_root, JailConfig, JailMode, JAIL_GID, JAIL_UID};

pub const SKIP_MESSAGE: &str = "SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)";
/// Defaults, relative to the workspace root.
pub const DEFAULT_FIRECRACKER: &str = "build/firecracker/v1.17.0/firecracker";
pub const DEFAULT_JAILER: &str = "build/firecracker/v1.17.0/jailer";
pub const DEFAULT_GUEST_IMAGE: &str = "build/guest-images/python-stdlib-v1";
/// The files a guest image directory must hold.
pub const IMAGE_FILES: [&str; 3] = ["image.json", "vmlinux", "rootfs.squashfs"];

/// What the KVM tier runs with; every path is absolute.
#[derive(Debug, Clone)]
pub struct Kvm {
    pub firecracker_bin: PathBuf,
    pub jailer_bin: PathBuf,
    pub image_dir: PathBuf,
    pub cgroup_root: PathBuf,
}

/// The cargo workspace root (both including crates live in `crates/<name>`).
fn workspace_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    fs::canonicalize(&root).unwrap_or(root)
}

/// `var`, or `default` under the workspace root; a relative value is taken from the root too.
fn setting(var: &str, default: &str) -> PathBuf {
    let value = std::env::var_os(var).map_or_else(|| PathBuf::from(default), PathBuf::from);
    if value.is_absolute() { value } else { workspace_root().join(value) }
}

fn executable(var: &str, path: &Path) -> Result<(), String> {
    match fs::metadata(path) {
        Ok(m) if m.is_file() && m.permissions().mode() & 0o111 != 0 => Ok(()),
        Ok(_) => Err(format!("{var}={}: not an executable file", path.display())),
        Err(e) => Err(format!("{var}={}: {e}", path.display())),
    }
}

fn image_complete(path: &Path) -> Result<(), String> {
    let missing: Vec<&str> = IMAGE_FILES.into_iter().filter(|f| !path.join(f).is_file()).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("AGENTOS_GUEST_IMAGE={}: missing {}", path.display(), missing.join(", ")))
    }
}

/// `jail::probe` over a temporary home. The home sits next to the image directory, on its
/// filesystem: the jail hard-links the image, so a home on another filesystem would fail the
/// probe for a reason the KVM tests (whose homes hold their images) never meet.
fn probe_jail(jailer_bin: &Path, cgroup_root: &Path, image_dir: &Path) -> Result<(), String> {
    let near_image = image_dir.parent().and_then(|p| tempfile::Builder::new().prefix(".kvm-gate-").tempdir_in(p).ok());
    let home = match near_image {
        Some(home) => home,
        None => tempfile::tempdir().map_err(|e| format!("temporary home: {e}"))?,
    };
    let cfg = JailConfig { jailer_bin: jailer_bin.to_path_buf(), uid: JAIL_UID, gid: JAIL_GID, cgroup_root: cgroup_root.to_path_buf() };
    let (jobs, inspect, work) = (home.path().join("jobs"), home.path().join("inspect"), home.path().join("work"));
    for d in [&jobs, &inspect, &work] {
        fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    jail::probe(&cfg, &jobs, &inspect, &work, image_dir)
}

/// The gate. See the module documentation.
pub fn require() -> Option<Kvm> {
    if std::env::var_os("AGENTOS_KVM_TESTS").is_none() {
        println!("{SKIP_MESSAGE}");
        return None;
    }
    let firecracker_bin = setting("AGENTOS_FIRECRACKER", DEFAULT_FIRECRACKER);
    let jailer_bin = setting("AGENTOS_JAILER", DEFAULT_JAILER);
    let image_dir = setting("AGENTOS_GUEST_IMAGE", DEFAULT_GUEST_IMAGE);
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let cgroup_root = find_cgroup2_root(&mounts);

    let mut reasons = Vec::new();
    if cgroup_root.is_none() {
        reasons.push("no cgroup v2 hierarchy in /proc/mounts".to_string());
    }
    if let Err(e) = File::options().read(true).write(true).open("/dev/kvm") {
        reasons.push(format!("/dev/kvm: {e}"));
    }
    reasons.extend(executable("AGENTOS_FIRECRACKER", &firecracker_bin).err());
    let jailer = executable("AGENTOS_JAILER", &jailer_bin);
    reasons.extend(jailer.clone().err());
    reasons.extend(image_complete(&image_dir).err());
    // The probe runs whenever there is a jailer and a cgroup tree to probe, whatever else
    // failed.
    if jailer.is_ok()
        && let Some(root) = &cgroup_root
        && let Err(e) = probe_jail(&jailer_bin, root, &image_dir)
    {
        reasons.push(format!("jail probe: {e}"));
    }
    let Some(cgroup_root) = cgroup_root.filter(|_| reasons.is_empty()) else {
        panic!(
            "AGENTOS_KVM_TESTS is set but the KVM tier cannot run (docker compose run --rm test-kvm …):\n  - {}",
            reasons.join("\n  - ")
        );
    };
    Some(Kvm { firecracker_bin, jailer_bin, image_dir, cgroup_root })
}

fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|m| m.dev())
}

impl Kvm {
    /// A scratch root on the guest image's filesystem: the jail hard-links the image into
    /// the chroot, so a root on another filesystem (`/tmp` is the container's overlay, the
    /// image lives on the `guest-images` volume) could not stage it.
    pub fn root(&self) -> tempfile::TempDir {
        let parent = self.image_dir.parent().expect("the image directory has a parent");
        tempfile::Builder::new().prefix(".kvm-test-").tempdir_in(parent).expect("a scratch root next to the guest image")
    }

    /// The image a job under `root` can hard-link: the tier's own when `root` is on its
    /// filesystem, else a copy in `<root>/guest-image` (made once; as the CLI's registry
    /// copies an image into its home).
    pub fn image_for(&self, root: &Path) -> PathBuf {
        fs::create_dir_all(root).expect("create the root");
        if device(root).is_some() && device(root) == device(&self.image_dir) {
            return self.image_dir.clone();
        }
        let copy = root.join("guest-image");
        if !copy.join("image.json").is_file() {
            let tmp = root.join(".guest-image.tmp");
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp).expect("create the image copy");
            for f in IMAGE_FILES {
                fs::copy(self.image_dir.join(f), tmp.join(f)).unwrap_or_else(|e| panic!("copy {f} of the guest image: {e}"));
            }
            fs::rename(&tmp, &copy).expect("install the image copy");
        }
        copy
    }

    /// A Firecracker worker config over `root`'s `snapshot`, `profile` and `work`: the real
    /// launcher, jailed as `JAIL_UID`/`JAIL_GID` in this tier's cgroup tree, the image pinned
    /// by its digest (`image_for(root)`), 1 vCPU and 256 MiB.
    pub fn jailed_config(&self, root: &Path) -> FirecrackerConfig {
        let image_dir = self.image_for(root);
        FirecrackerConfig {
            firecracker_bin: self.firecracker_bin.clone(),
            image_digest: workspace_digest(&image_dir).expect("digest the guest image"),
            image_dir,
            snapshot_dir: root.join("snapshot"),
            profile_dir: root.join("profile"),
            profile_digest: None,
            work_root: root.join("work"),
            verify_timeout_secs: 60,
            vcpus: 1,
            memory_mib: 256,
            attempt_token: mint_attempt_token(),
            launcher: GuestLauncher::Real { firecracker_bin: self.firecracker_bin.clone() },
            jail: JailMode::Jailed(JailConfig {
                jailer_bin: self.jailer_bin.clone(),
                uid: JAIL_UID,
                gid: JAIL_GID,
                cgroup_root: self.cgroup_root.clone(),
            }),
        }
    }

    /// `jailed_config(root)` run without the jail: Firecracker as the current user, in the
    /// caller's cgroup (`--allow-unjailed`).
    pub fn unjailed_config(&self, root: &Path) -> FirecrackerConfig {
        FirecrackerConfig { jail: JailMode::Unjailed, ..self.jailed_config(root) }
    }
}
