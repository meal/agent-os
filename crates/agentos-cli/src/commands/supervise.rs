//! The hidden `supervise` subcommands: the per-job supervisor and its worker are this very
//! binary, re-executed (`agentos supervise run|worker JOB_DIR`); so is the test tier's fake
//! guest (`agentos supervise fake-guest UDS ROOT`, honoured only with `AGENTOS_TEST_WORKERS=1`).

use std::ffi::OsString;

use agentos_engine::supervisor::{SupervisorCmd, main_with_args};

/// The command a supervisor uses to start its worker: this executable, `supervise worker`.
pub fn supervisor_cmd() -> std::io::Result<SupervisorCmd> {
    Ok(SupervisorCmd {
        program: std::env::current_exe()?,
        prefix_args: vec!["supervise".into()],
    })
}

/// Runs the supervisor (`run`), the worker (`worker`) or the fake guest (`fake-guest`) with
/// `args`; returns the exit code.
pub fn run(verb: &str, args: &[OsString]) -> i32 {
    let cmd = match supervisor_cmd() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agentos: cannot locate own executable: {e}");
            return 1;
        }
    };
    main_with_args(
        std::iter::once(OsString::from(verb)).chain(args.iter().cloned()),
        &cmd,
    )
}
