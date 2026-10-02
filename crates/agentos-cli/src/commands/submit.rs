//! `submit`: validate the contract, resolve and record its inputs immutably, show the
//! owner what they approve, and (with `--yes`) run the task.

use std::fs;
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::ids::{Digest, TaskId};
use agentos_engine::workspace::{copy_tree, workspace_digest};
use serde_json::json;

use super::{print, print_state};
use crate::crash::CrashSpec;
use crate::drive::{drive, AGENT_PATCH};
use crate::error::CliError;
use crate::home::Home;

/// A `repository.revision` asking submission to record the source's workspace digest.
pub const RECORDED_AT_SUBMISSION: &str = "recorded-at-submission";
/// The execution environment recorded for this milestone's executor.
const GUEST_IMAGE: &str = "fixture-executor-v0";
const MODEL: &str = "fake-agent";

/// The contract and inputs, checked before anything is written.
struct Request {
    contract: Contract,
    source: PathBuf,
    profile: PathBuf,
    /// Set when the contract names a digest instead of `recorded-at-submission`.
    expected_revision: Option<Digest>,
    patch: Option<String>,
}

fn validate(home: &Home, task: &Path, yes: bool, patch: Option<&Path>) -> Result<Request, CliError> {
    let text = fs::read_to_string(task).map_err(|e| CliError::usage(format!("cannot read {}: {e}", task.display())))?;
    let contract = Contract::parse(&text).map_err(|e| CliError::usage(e.to_string()))?;
    let source = Path::new(&contract.repository.source)
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| CliError::usage(format!("repository source {} is not a directory", contract.repository.source)))?;
    let profile = home.resolve_profile(&contract.verification_profile, contract.profile_digest.as_deref())?.ok_or_else(|| {
        CliError::usage(format!(
            "verification profile {} not found in the registry {} or in {}",
            contract.verification_profile,
            home.registry_dir().display(),
            home.profiles.display()
        ))
    })?;
    let expected_revision = match contract.repository.revision.as_str() {
        RECORDED_AT_SUBMISSION => None,
        rev => {
            let wanted = Digest::from_hex(rev).map_err(|_| {
                CliError::usage(format!(
                    "repository revision must be {RECORDED_AT_SUBMISSION:?} or the source's workspace digest, got {rev:?}"
                ))
            })?;
            let actual = workspace_digest(&source).map_err(|e| CliError::usage(format!("cannot digest {}: {e}", source.display())))?;
            if actual != wanted {
                return Err(CliError::usage(format!("repository revision {wanted} does not match the source, which is {actual}")));
            }
            Some(wanted)
        }
    };
    let patch = match (yes, patch) {
        (true, None) => {
            return Err(CliError::usage(
                "--yes needs --fake-agent-patch FILE: the fake agent is the only agent until the model broker exists",
            ))
        }
        (_, Some(p)) => Some(fs::read_to_string(p).map_err(|e| CliError::usage(format!("cannot read {}: {e}", p.display())))?),
        (false, None) => None,
    };
    Ok(Request { contract, source, profile, expected_revision, patch })
}

fn capability_names(contract: &Contract) -> Vec<String> {
    contract.capabilities.iter().map(|c| serde_json::to_value(c).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()).collect()
}

/// What the owner approves, on stderr.
fn summarize(task: &TaskId, contract: &Contract, repo: &Digest, profile: &Digest, patch: Option<&str>) {
    let l = &contract.limits;
    eprintln!("task {task} submitted; approve these permissions before it runs:");
    eprintln!("  goal:                 {}", contract.goal);
    eprintln!("  repository:           {} at {repo}", contract.repository.source);
    eprintln!("  capabilities:         {}", capability_names(contract).join(", "));
    eprintln!("  editable paths:       {}", contract.editable_paths.join(", "));
    eprintln!("  acceptance:           protected verification profile {} ({profile})", contract.verification_profile);
    eprintln!(
        "  limits:               model_requests={} max_output_tokens_per_request={} tool_actions={} deadline_seconds={} worker_vcpus={} worker_memory_mib={}",
        l.model_requests, l.max_output_tokens_per_request, l.tool_actions, l.deadline_seconds, l.worker_vcpus, l.worker_memory_mib
    );
    let agent = patch.map_or_else(|| "none yet".to_string(), |p| format!("{MODEL} (patch {})", Digest::of(p.as_bytes())));
    eprintln!("  agent:                {agent}");
}

pub async fn submit(home: &Home, task_file: &Path, yes: bool, patch: Option<&Path>, crash: Option<&CrashSpec>) -> Result<(), CliError> {
    let Request { mut contract, source, profile, expected_revision, patch } = validate(home, task_file, yes, patch)?;

    let store = home.open()?;
    let lock = if yes { Some(home.lock()?) } else { None };
    // Inputs are copied into the home, so the task runs on exactly what was digested here
    // whatever later happens to the source or the profile registry.
    let staging = tempfile::Builder::new().prefix(".submit-").tempdir_in(home.tasks_dir())?;
    copy_tree(&source, &staging.path().join("snapshot"))?;
    copy_tree(&profile, &staging.path().join("profile"))?;
    let repo_digest = workspace_digest(&staging.path().join("snapshot"))?;
    let profile_digest = workspace_digest(&staging.path().join("profile"))?;
    if let Some(pin) = contract.profile_digest.as_ref().filter(|pin| **pin != profile_digest.to_string()) {
        return Err(CliError::usage(format!("verification profile changed while it was recorded: pinned {pin}, found {profile_digest}")));
    }
    if expected_revision.is_some_and(|d| d != repo_digest) {
        return Err(CliError::usage(format!("repository source changed while it was recorded (now {repo_digest})")));
    }
    if let Some(p) = &patch {
        fs::write(staging.path().join(AGENT_PATCH), p)?;
    }
    contract.repository.revision = repo_digest.to_string();
    contract.repository.source = source.display().to_string();
    let contract_digest = Digest::of(&serde_json::to_vec(&contract)?);

    let task = store.db.create_task(&contract, &contract_digest)?;
    fs::rename(staging.keep(), home.task_dir(&task))?;
    let submitted = json!({
        "contract_digest": contract_digest,
        "repository_source": contract.repository.source,
        "repository_digest": repo_digest,
        "profile_id": contract.verification_profile,
        "profile_digest": profile_digest,
        "guest_image": GUEST_IMAGE,
        "model": MODEL,
        "fake_agent_patch_digest": patch.as_ref().map(|p| Digest::of(p.as_bytes())),
    });
    store.db.append_audit(&task, "Submitted", &submitted)?;
    tracing::info!(task_id = %task, %contract_digest, %repo_digest, %profile_digest, "task submitted");
    summarize(&task, &contract, &repo_digest, &profile_digest, patch.as_deref());

    match (lock, patch) {
        (Some(lock), Some(patch)) => {
            // `--yes` is the owner's approval: issue the task's capability handles.
            store.db.approve_task(&task)?;
            let state = drive(home, &store, &lock, &task, patch, crash).await?;
            print_state(&task, state);
        }
        _ => print(&json!({
            "task_id": task,
            "state": "READY",
            "note": format!("not started; run `agentos resume {task}` to approve and start"),
        })),
    }
    Ok(())
}
