//! The hidden `supervise` subcommands: the per-job supervisor and its worker are this very
//! binary, re-executed (`agentos supervise run|worker JOB_DIR`).

use std::path::Path;

use agentos_engine::supervisor::{main_with_args, SupervisorCmd};

/// The command a supervisor uses to start its worker: this executable, `supervise worker`.
pub fn supervisor_cmd() -> std::io::Result<SupervisorCmd> {
    Ok(SupervisorCmd { program: std::env::current_exe()?, prefix_args: vec!["supervise".into()] })
}

/// Runs the supervisor (`run`) or the worker (`worker`) for `job_dir`; returns the exit code.
pub fn run(verb: &str, job_dir: &Path) -> i32 {
    let cmd = match supervisor_cmd() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentos: cannot locate own executable: {e}");
            return 1;
        }
    };
    main_with_args([verb.into(), job_dir.into()], &cmd)
}
