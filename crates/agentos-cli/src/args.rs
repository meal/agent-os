use std::ffi::OsString;
use std::path::PathBuf;

use agentos_engine::jail::{JAIL_GID, JAIL_UID};
use clap::builder::FalseyValueParser;
use clap::{Parser, Subcommand, ValueEnum};

use crate::crash::CrashSpec;

/// Where a task's effects run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WorkerKind {
    /// The fixture worker on the host (not sandboxed).
    Host,
    /// One Firecracker microVM per job, booted from a registered guest image.
    Firecracker,
}

impl WorkerKind {
    /// The name recorded in `Submitted.worker`.
    pub fn as_str(self) -> &'static str {
        match self {
            WorkerKind::Host => "host",
            WorkerKind::Firecracker => "firecracker",
        }
    }
}

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
    /// The worker `submit` runs the task on [default: host]; later commands use the one
    /// recorded at submission and refuse a different one.
    #[arg(long, global = true, env = "AGENTOS_WORKER", value_enum)]
    pub worker: Option<WorkerKind>,
    /// The Firecracker binary [default: <home>/bin/firecracker].
    #[arg(long, global = true, env = "AGENTOS_FIRECRACKER", value_name = "PATH")]
    pub firecracker: Option<PathBuf>,
    /// The jailer binary [default: `jailer` next to the Firecracker binary].
    #[arg(long, global = true, env = "AGENTOS_JAILER", value_name = "PATH")]
    pub jailer: Option<PathBuf>,
    /// The uid the jailed Firecracker runs as.
    #[arg(long, global = true, env = "AGENTOS_JAIL_UID", default_value_t = JAIL_UID)]
    pub jail_uid: u32,
    /// The gid the jailed Firecracker runs as.
    #[arg(long, global = true, env = "AGENTOS_JAIL_GID", default_value_t = JAIL_GID)]
    pub jail_gid: u32,
    /// Run Firecracker without the jailer, as the current user, when the jail is unavailable
    /// (recorded as `jailed: false`).
    #[arg(long, global = true, env = "AGENTOS_ALLOW_UNJAILED", value_parser = FalseyValueParser::new())]
    pub allow_unjailed: bool,
    /// File holding the Anthropic API key (one line); else ANTHROPIC_API_KEY. The key is never
    /// taken from the command line.
    #[arg(long, global = true, env = "AGENTOS_API_KEY_FILE", value_name = "FILE")]
    pub api_key_file: Option<PathBuf>,
    /// The Anthropic API base URL [default: https://api.anthropic.com] (tests point it at a local fake).
    #[arg(long, global = true, env = "AGENTOS_ANTHROPIC_BASE_URL", value_name = "URL")]
    pub anthropic_base_url: Option<String>,
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
        /// Patch text the fake agent applies.
        #[arg(long, value_name = "FILE")]
        fake_agent_patch: Option<PathBuf>,
        /// anthropic:<model> for a real model, fake:<transcript file> for the scripted provider;
        /// without it the fake agent runs (it needs --fake-agent-patch).
        #[arg(long, value_name = "SPEC")]
        model: Option<String>,
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
    /// Manage the verification profile registry.
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Manage the guest image registry.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// Revoke a task's capabilities (all, or one by its contract name, e.g. verification.run);
    /// running jobs that depend on a revoked capability are stopped.
    Revoke {
        id: String,
        #[arg(long, value_name = "NAME")]
        capability: Option<String>,
    },
    /// Write the export bundle of a finished task to DIR.
    Export { id: String, dir: PathBuf },
    /// Internal: the per-job supervisor and its worker (`run|worker JOB_DIR`), and the fake
    /// guest of the test tier (`fake-guest UDS ROOT`, only with `AGENTOS_TEST_WORKERS=1`).
    #[command(hide = true)]
    Supervise {
        verb: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProfileCommand {
    /// Copy a profile directory (profile.json plus its check files) into the read-only,
    /// content-addressed registry; the same bytes twice change nothing.
    Register { dir: PathBuf },
    /// List registered profiles.
    List,
}

#[derive(Debug, Subcommand)]
pub enum ImageCommand {
    /// Copy a guest image directory (image.json, vmlinux, rootfs.squashfs) into the read-only,
    /// content-addressed registry; the same bytes twice change nothing.
    Register { dir: PathBuf },
    /// List registered guest images.
    List,
}
