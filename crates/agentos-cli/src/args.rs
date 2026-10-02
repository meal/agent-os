use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::crash::CrashSpec;

/// Agent OS controller. Every command prints JSON on stdout; errors go to stderr.
#[derive(Debug, Parser)]
#[command(name = "agentos", version)]
pub struct Args {
    /// Home directory (journal, blobs, jobs, workspaces); created on first use.
    #[arg(long, global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,
    /// Verification profile registry, one profile per `<id>/` directory [default: <home>/profiles].
    #[arg(long, global = true, value_name = "DIR")]
    pub profiles: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate a task contract, record its inputs and, with --yes, run it.
    Submit {
        /// The task contract (JSON).
        task: PathBuf,
        /// Approve the permissions shown and start the task now.
        #[arg(long)]
        yes: bool,
        /// Patch text the fake agent applies (the only agent until the model broker exists).
        #[arg(long, value_name = "FILE")]
        fake_agent_patch: Option<PathBuf>,
        /// Debug: kill this process at an engine crash point, POINT[:KIND][:N].
        #[arg(long, value_name = "POINT[:KIND][:N]")]
        crash_at: Option<CrashSpec>,
    },
    /// Show a task's state, digests, usage and outstanding effects.
    Status { id: String },
    /// Print a task's journal, one JSON event per line.
    Events { id: String },
    /// Pause a running task; its runner stops at the next step.
    Pause { id: String },
    /// Approve a READY task, resume a PAUSED one, or recover one whose controller died.
    Resume {
        id: String,
        /// Patch for the fake agent [default: the one given at submission].
        #[arg(long, value_name = "FILE")]
        fake_agent_patch: Option<PathBuf>,
        /// Debug: kill this process at an engine crash point, POINT[:KIND][:N].
        #[arg(long, value_name = "POINT[:KIND][:N]")]
        crash_at: Option<CrashSpec>,
    },
    /// Cancel a task; in-flight effects are reconciled first.
    Cancel { id: String },
    /// Write the export bundle of a finished task to DIR.
    Export { id: String, dir: PathBuf },
    /// Internal: the per-job supervisor and its worker (`run|worker JOB_DIR`).
    #[command(hide = true)]
    Supervise {
        verb: String,
        job_dir: PathBuf,
    },
}
