//! The `agentos` command line: submit, inspect, steer and export tasks. Every command is
//! one short-lived controller process over the on-disk home ([`home`]); a run that dies is
//! picked up by the next `resume` or `cancel`.

mod args;
mod commands;
pub mod crash;
mod drive;
mod error;
mod home;

use clap::Parser;

/// Runs the command line; returns the process exit code.
pub fn run() -> i32 {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(ansi).init();
    let args = args::Args::parse();
    // The supervisor and its worker are plain synchronous processes (the worker builds its
    // own runtime), so they are entered before ours exists.
    if let args::Command::Supervise { verb, args } = &args.command {
        return commands::supervise::run(verb, args);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("agentos: cannot start the runtime: {e}");
            return 1;
        }
    };
    match runtime.block_on(commands::dispatch(args)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("agentos: {e}");
            e.code
        }
    }
}
