//! The jail the official `jailer` builds around Firecracker: chroot, uid/gid drop, cgroup v2
//! limits. The mode is decided by the controller and carried in `FirecrackerConfig`; the
//! worker never probes or decides, it only executes the mode it was given.
//!
//! This module holds the types and constants; planning, staging, the probe and collection
//! come with the jailed launch path.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The uid/gid Firecracker runs as when jailed (unless `--jail-uid`/`--jail-gid`).
pub const JAIL_UID: u32 = 61000;
pub const JAIL_GID: u32 = 61000;
/// The parent cgroup of every jailed VM: `<cgroup_root>/agentos/<id>`.
pub const JAIL_PARENT_CGROUP: &str = "agentos";
/// `memory.max` = guest memory plus this, for Firecracker's own footprint.
pub const JAIL_MEMORY_OVERHEAD_MIB: u64 = 128;
pub const JAIL_PIDS_MAX: u64 = 64;
/// `RLIMIT_FSIZE` of the jailed process (= `WS_IMAGE_BYTES`).
pub const JAIL_FSIZE_BYTES: u64 = 1 << 30;
/// `cpu.max` period; the quota is `vcpus × CPU_PERIOD_US`.
pub const CPU_PERIOD_US: u64 = 100_000;

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
