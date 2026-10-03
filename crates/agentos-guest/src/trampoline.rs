//! `agentos-guest exec-check --nproc N --nofile N --oom N -- PROGRAM ARGS…` (spec issue 6).
//!
//! The agent starts the check through this trampoline (already as `check`, in its own
//! process group, with the environment it should have); the trampoline lowers its own
//! `RLIMIT_NPROC` and `RLIMIT_NOFILE`, raises its `oom_score_adj`, then replaces itself with
//! the program. Everything is inherited across `exec`, and no `pre_exec` (`unsafe`) is
//! needed.

use std::ffi::OsString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::Command;

use rustix::process::{setrlimit, Resource, Rlimit};

pub const USAGE: &str = "usage: agentos-guest exec-check --nproc N --nofile N --oom -1000..1000 -- PROGRAM [ARGS...]";

/// The parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecCheck {
    pub nproc: u64,
    pub nofile: u64,
    pub oom_score_adj: i32,
    pub program: OsString,
    pub args: Vec<OsString>,
}

/// Each of the three options exactly once, in any order, then `--` and a program.
pub fn parse(args: &[OsString]) -> Result<ExecCheck, String> {
    let (mut nproc, mut nofile, mut oom) = (None, None, None);
    let mut it = args.iter();
    loop {
        let Some(flag) = it.next() else {
            return Err("missing `-- PROGRAM`".into());
        };
        if flag == "--" {
            break;
        }
        let flag = flag.to_str().ok_or("non-UTF-8 option")?;
        let value = it.next().and_then(|v| v.to_str()).ok_or_else(|| format!("{flag} needs a value"))?;
        let slot_taken = match flag {
            "--nproc" => nproc.replace(value.parse::<u64>().map_err(|_| format!("--nproc {value:?}"))?).is_some(),
            "--nofile" => nofile.replace(value.parse::<u64>().map_err(|_| format!("--nofile {value:?}"))?).is_some(),
            "--oom" => {
                let v = value.parse::<i32>().ok().filter(|v| (-1000..=1000).contains(v)).ok_or_else(|| format!("--oom {value:?}"))?;
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
        nproc: nproc.ok_or("missing --nproc")?,
        nofile: nofile.ok_or("missing --nofile")?,
        oom_score_adj: oom.ok_or("missing --oom")?,
        program,
        args: it.cloned().collect(),
    })
}

/// Applies the limits, then `exec`s. Returns only on failure: 2 for a malformed command line
/// (nothing applied), 126 if a limit cannot be applied (nothing run), 127 if `exec` failed.
pub fn main(args: &[OsString]) -> i32 {
    let req = match parse(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agentos-guest exec-check: {e}\n{USAGE}");
            return 2;
        }
    };
    let limits = [(Resource::Nproc, req.nproc, "RLIMIT_NPROC"), (Resource::Nofile, req.nofile, "RLIMIT_NOFILE")];
    for (resource, n, name) in limits {
        if let Err(e) = setrlimit(resource, Rlimit { current: Some(n), maximum: Some(n) }) {
            eprintln!("agentos-guest exec-check: cannot set {name} to {n}: {e}");
            return 126;
        }
    }
    if let Err(e) = fs::write("/proc/self/oom_score_adj", req.oom_score_adj.to_string()) {
        eprintln!("agentos-guest exec-check: cannot set oom_score_adj to {}: {e}", req.oom_score_adj);
        return 126;
    }
    let err = Command::new(&req.program).args(&req.args).exec();
    eprintln!("agentos-guest exec-check: cannot run {}: {err}", req.program.to_string_lossy().escape_debug());
    127
}
