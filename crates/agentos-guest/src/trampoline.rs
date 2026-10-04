//! `agentos-guest exec-check --uid U --gid G --nproc N --nofile N --oom N -- PROGRAM ARGS…`
//! (spec issue 6).
//!
//! The agent starts the check through this trampoline **as root**, in its own process
//! group, with the environment and working directory the check should have. The trampoline,
//! single-threaded, in this order:
//! 1. writes `oom_score_adj` while still privileged: with `CAP_SYS_RESOURCE` the kernel also
//!    makes that value the process's floor (`oom_score_adj_min`), so the check can never
//!    lower it again (written unprivileged, the agent's −1000 floor would stay inherited and
//!    the check could reset itself to −1000);
//! 2. sets `RLIMIT_NPROC` and `RLIMIT_NOFILE` (soft = hard);
//! 3. drops to `--uid`/`--gid` for good: no supplementary groups, real, effective and saved
//!    ids all set; then `no_new_privs`, so no set-uid binary can give anything back;
//! 4. verifies the drop (ids, groups, and that `setresuid(0)` now fails);
//! 5. `exec`s the program (safe `CommandExt::exec`; no `pre_exec`, no `unsafe`).
//!
//! Any failure before the `exec` runs nothing.

use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::process::{
    Gid, Resource, Rlimit, Uid, getegid, geteuid, getgid, getgroups, getuid, setrlimit,
};
use rustix::thread::{set_no_new_privs, set_thread_groups, set_thread_res_gid, set_thread_res_uid};

pub const USAGE: &str = "usage: agentos-guest exec-check --uid U --gid G --nproc N --nofile N --oom -1000..1000 -- PROGRAM [ARGS...]";

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCheck {
    /// Never 0: the trampoline exists to drop root.
    pub uid: u32,
    pub gid: u32,
    pub nproc: u64,
    pub nofile: u64,
    pub oom_score_adj: i32,
    pub program: OsString,
    pub args: Vec<OsString>,
}

/// Each of the five options exactly once, in any order, then `--` and a program.
pub fn parse(args: &[OsString]) -> Result<ExecCheck, String> {
    let (mut uid, mut gid, mut nproc, mut nofile, mut oom) = (None, None, None, None, None);
    // A non-root id (`u32::MAX` is the kernel's "no change").
    let id = |flag: &str, value: &str| -> Result<u32, String> {
        value
            .parse::<u32>()
            .ok()
            .filter(|v| *v != 0 && *v != u32::MAX)
            .ok_or_else(|| format!("{flag} {value:?}"))
    };
    let mut it = args.iter();
    loop {
        let Some(flag) = it.next() else {
            return Err("missing `-- PROGRAM`".into());
        };
        if flag == "--" {
            break;
        }
        let flag = flag.to_str().ok_or("non-UTF-8 option")?;
        let value = it
            .next()
            .and_then(|v| v.to_str())
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let slot_taken = match flag {
            "--uid" => uid.replace(id(flag, value)?).is_some(),
            "--gid" => gid.replace(id(flag, value)?).is_some(),
            "--nproc" => nproc
                .replace(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("--nproc {value:?}"))?,
                )
                .is_some(),
            "--nofile" => nofile
                .replace(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("--nofile {value:?}"))?,
                )
                .is_some(),
            "--oom" => {
                let v = value
                    .parse::<i32>()
                    .ok()
                    .filter(|v| (-1000..=1000).contains(v))
                    .ok_or_else(|| format!("--oom {value:?}"))?;
                oom.replace(v).is_some()
            }
            other => return Err(format!("unknown option {other:?}")),
        };
        if slot_taken {
            return Err(format!("{flag} given twice"));
        }
    }
    let program = it.next().ok_or("missing PROGRAM after --")?.clone();
    Ok(ExecCheck {
        uid: uid.ok_or("missing --uid")?,
        gid: gid.ok_or("missing --gid")?,
        nproc: nproc.ok_or("missing --nproc")?,
        nofile: nofile.ok_or("missing --nofile")?,
        oom_score_adj: oom.ok_or("missing --oom")?,
        program,
        args: it.cloned().collect(),
    })
}

/// Steps 3 and 4: the permanent drop, `no_new_privs`, and the proof that root is gone.
/// rustix's `set_thread_*` calls are the raw per-thread syscalls; the trampoline has only its
/// main thread (checked first), so they apply to the whole process.
fn drop_privileges(uid: u32, gid: u32) -> Result<(), String> {
    let threads = fs::read_dir("/proc/self/task")
        .map_err(|e| format!("/proc/self/task: {e}"))?
        .count();
    if threads != 1 {
        return Err(format!("{threads} threads, expected 1"));
    }
    let (u, g) = (Uid::from_raw(uid), Gid::from_raw(gid));
    set_thread_groups(&[]).map_err(|e| format!("setgroups: {e}"))?;
    set_thread_res_gid(g, g, g).map_err(|e| format!("setresgid {gid}: {e}"))?;
    set_thread_res_uid(u, u, u).map_err(|e| format!("setresuid {uid}: {e}"))?;
    set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
    if (getuid(), geteuid(), getgid(), getegid()) != (u, u, g, g) {
        return Err("ids did not change".into());
    }
    if !getgroups()
        .map_err(|e| format!("getgroups: {e}"))?
        .is_empty()
    {
        return Err("supplementary groups remain".into());
    }
    if set_thread_res_uid(Uid::ROOT, Uid::ROOT, Uid::ROOT).is_ok() {
        return Err("root could be regained".into());
    }
    Ok(())
}

/// Applies the priority, the limits and the drop, then `exec`s. Returns only on failure: 2
/// for a malformed command line, 126 if any step before the `exec` failed, 127 if `exec`
/// failed. In every case nothing was run.
pub fn main(args: &[OsString]) -> i32 {
    let req = match parse(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agentos-guest exec-check: {e}\n{USAGE}");
            return 2;
        }
    };
    if let Err(e) = fs::write("/proc/self/oom_score_adj", req.oom_score_adj.to_string()) {
        eprintln!(
            "agentos-guest exec-check: cannot set oom_score_adj to {}: {e}",
            req.oom_score_adj
        );
        return 126;
    }
    let limits = [
        (Resource::Nproc, req.nproc, "RLIMIT_NPROC"),
        (Resource::Nofile, req.nofile, "RLIMIT_NOFILE"),
    ];
    for (resource, n, name) in limits {
        if let Err(e) = setrlimit(
            resource,
            Rlimit {
                current: Some(n),
                maximum: Some(n),
            },
        ) {
            eprintln!("agentos-guest exec-check: cannot set {name} to {n}: {e}");
            return 126;
        }
    }
    if let Err(e) = drop_privileges(req.uid, req.gid) {
        eprintln!(
            "agentos-guest exec-check: cannot drop privileges to {}:{}: {e}",
            req.uid, req.gid
        );
        return 126;
    }
    let err = Command::new(&req.program).args(&req.args).exec();
    eprintln!(
        "agentos-guest exec-check: cannot run {}: {err}",
        req.program.to_string_lossy().escape_debug()
    );
    127
}
