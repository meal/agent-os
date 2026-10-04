//! `agentos-supervisor run <job_dir>` supervises one job; `agentos-supervisor worker <job_dir>`
//! is the worker it re-executes. See `agentos_engine::supervisor`.

use agentos_engine::supervisor::{SupervisorCmd, main_with_args};

fn main() {
    let program = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("agentos-supervisor: cannot locate own executable: {e}");
            std::process::exit(1);
        }
    };
    let cmd = SupervisorCmd {
        program,
        prefix_args: Vec::new(),
    };
    std::process::exit(main_with_args(std::env::args_os().skip(1), &cmd));
}
