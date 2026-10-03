//! The KVM tier. Every test here begins with `let Some(kvm) = kvm::require() else { return };`
//! (Task 13 fills it); this file starts with the gate's own tests, which run in every tier.
//!
//! The gate is exercised in child processes of this very test binary (`--exact` on the
//! entry below), so each child gets exactly the environment the test gives it.

#[path = "common/kvm.rs"]
mod kvm;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Set only in the children: makes `gate_child_entry` call `require()`.
const CHILD: &str = "AGENTOS_KVM_GATE_CHILD";
const GATE_VARS: [&str; 4] = ["AGENTOS_KVM_TESTS", "AGENTOS_FIRECRACKER", "AGENTOS_JAILER", "AGENTOS_GUEST_IMAGE"];

/// Not a test of its own: in a child it reports what `require()` returned.
#[test]
fn gate_child_entry() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let got = kvm::require();
    println!("GATE_RESULT: {}", if got.is_some() { "available" } else { "skipped" });
}

fn gate_child(env: &[(&str, &Path)], kvm_tests: bool) -> Output {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "gate_child_entry", "--nocapture", "--test-threads=1"]).env(CHILD, "1");
    for var in GATE_VARS {
        cmd.env_remove(var);
    }
    if kvm_tests {
        cmd.env("AGENTOS_KVM_TESTS", "1");
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn script(path: &Path, body: &str) -> PathBuf {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

/// Stand-ins that pass every check but `/dev/kvm` and the jail's environment: a
/// `firecracker` and a `jailer` that print their v1.17 versions, and an image directory
/// with the three files.
struct StandIns {
    _dir: tempfile::TempDir,
    firecracker: PathBuf,
    jailer: PathBuf,
    image: PathBuf,
}

fn stand_ins() -> StandIns {
    let dir = tempfile::tempdir().unwrap();
    let firecracker = script(&dir.path().join("firecracker"), "echo 'Firecracker v1.17.0'");
    let jailer = script(&dir.path().join("jailer"), "echo 'Jailer v1.17.0'");
    let image = dir.path().join("image");
    fs::create_dir_all(&image).unwrap();
    for f in ["image.json", "vmlinux", "rootfs.squashfs"] {
        fs::write(image.join(f), b"x").unwrap();
    }
    StandIns { _dir: dir, firecracker, jailer, image }
}

#[test]
fn kvm_gate_skips_loudly_without_the_variable() {
    let out = gate_child(&[], false);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout.contains("SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)"),
        "{stdout}"
    );
    assert!(stdout.contains("GATE_RESULT: skipped"), "{stdout}");
}

#[test]
fn kvm_gate_panics_when_requested_but_unavailable() {
    let s = stand_ins();
    let missing = Path::new("/nonexistent");

    let out = gate_child(&[("AGENTOS_FIRECRACKER", missing), ("AGENTOS_JAILER", &s.jailer), ("AGENTOS_GUEST_IMAGE", &s.image)], true);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains("AGENTOS_FIRECRACKER=/nonexistent"), "{stderr}");
    assert!(!stderr.contains("AGENTOS_JAILER="), "{stderr}");
    assert!(!text(&out.stdout).contains("GATE_RESULT"), "the gate returned instead of panicking");

    let out = gate_child(&[("AGENTOS_FIRECRACKER", &s.firecracker), ("AGENTOS_JAILER", missing), ("AGENTOS_GUEST_IMAGE", &s.image)], true);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains("AGENTOS_JAILER=/nonexistent"), "{stderr}");
    assert!(!stderr.contains("AGENTOS_FIRECRACKER="), "{stderr}");

    let out = gate_child(&[("AGENTOS_FIRECRACKER", &s.firecracker), ("AGENTOS_JAILER", &s.jailer), ("AGENTOS_GUEST_IMAGE", missing)], true);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains("AGENTOS_GUEST_IMAGE=/nonexistent"), "{stderr}");
    assert!(stderr.contains("image.json"), "{stderr}");

    // Everything else in place: the jail's own environment still decides. The `test`
    // service is root with a read-only cgroup tree, so the gate cannot pass without the
    // jail; a non-root run fails earlier on root.
    let mounts = fs::read_to_string("/proc/mounts").unwrap();
    let cgroup_ro = mounts.lines().any(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        f.len() >= 4 && f[2] == "cgroup2" && f[3].split(',').any(|o| o == "ro")
    });
    let root = fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .is_some_and(|l| l.split_whitespace().nth(2) == Some("0"));
    let expected = match (root, cgroup_ro) {
        (false, _) => "needs root",
        (true, true) => "read-only",
        (true, false) => {
            println!("cgroup tree is writable here (test-kvm): the jail-environment case belongs to the `test` service");
            return;
        }
    };
    let out = gate_child(&[("AGENTOS_FIRECRACKER", &s.firecracker), ("AGENTOS_JAILER", &s.jailer), ("AGENTOS_GUEST_IMAGE", &s.image)], true);
    let stderr = text(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(stderr.contains(expected), "{stderr}");
    assert!(stderr.contains("jail probe"), "{stderr}");
}
