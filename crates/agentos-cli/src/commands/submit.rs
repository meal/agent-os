//! `submit`: validate the contract, resolve and record its inputs immutably, show the
//! owner what they approve, and (with `--yes`) run the task.

use std::fs;
use std::path::{Path, PathBuf};

use agentos_core::contract::Contract;
use agentos_core::guest::{GUEST_MIN_MEMORY_MIB, MAX_VCPUS};
use agentos_core::ids::{Digest, TaskId};
use agentos_core::resources::VmResources;
use agentos_engine::firecracker::{check_host_space, firecracker_version};
use agentos_engine::guestlink::GuestLauncher;
use agentos_engine::model::fake::Transcript;
use agentos_engine::model::provider::ApiKey;
use agentos_engine::workspace::{copy_tree, workspace_digest};
use serde_json::json;

use super::{print, print_state};
use crate::args::WorkerKind;
use crate::crash::CrashSpec;
use crate::drive::{AGENT_PATCH, FAKE_AGENT, ModelSpec, TRANSCRIPT, agent_for, drive};
use crate::error::CliError;
use crate::home::{ANALYZER_DIR, Home};

/// A `repository.revision` asking submission to record the source's workspace digest.
pub const RECORDED_AT_SUBMISSION: &str = "recorded-at-submission";
/// The execution environment recorded for the host worker.
const GUEST_IMAGE: &str = "fixture-executor-v0";
/// `Submitted.firecracker_version` when the fake guest stands in for Firecracker.
const FAKE_FIRECRACKER_VERSION: &str = "fake";

/// The contract and inputs, checked before anything is written.
struct Request {
    contract: Contract,
    source: PathBuf,
    profile: PathBuf,
    /// Set when the contract names a digest instead of `recorded-at-submission`.
    expected_revision: Option<Digest>,
    patch: Option<String>,
    /// `--model`, parsed.
    model: Option<ModelSpec>,
    /// The bytes of a `fake:` transcript, read and parsed once: what is recorded and run.
    transcript: Option<Vec<u8>>,
    /// The Anthropic key, resolved once here (with `--yes`) and handed to the executor.
    api_key: Option<ApiKey>,
    /// `None` for the host worker.
    firecracker: Option<FirecrackerRecord>,
}

/// What `Submitted` records about a Firecracker task, decided before anything is written.
struct FirecrackerRecord {
    image_id: String,
    image_digest: Digest,
    version: String,
    jailed: bool,
    resources: VmResources,
}

/// The Firecracker worker's checks, in order: the contract's limits (exit 2), the guest image
/// from `contract.profile` and its optional pin (exit 2), the preflight over the registry
/// entry and the jail decision (exit 1).
fn check_firecracker(home: &Home, contract: &Contract) -> Result<FirecrackerRecord, CliError> {
    let l = &contract.limits;
    if l.worker_vcpus > MAX_VCPUS {
        return Err(CliError::usage(format!(
            "limit worker_vcpus must be at most {MAX_VCPUS} for the firecracker worker"
        )));
    }
    if l.worker_memory_mib < GUEST_MIN_MEMORY_MIB {
        return Err(CliError::usage(format!(
            "limit worker_memory_mib must be at least {GUEST_MIN_MEMORY_MIB} for the firecracker worker"
        )));
    }
    let image = home
        .resolve_image(&contract.profile, contract.guest_image_digest.as_deref())?
        .ok_or_else(|| {
            CliError::usage(format!(
                "guest image {} not found in the registry {}; build and register it first",
                contract.profile,
                home.images_dir().display()
            ))
        })?;
    // The preflight reads only the launcher and the image: the task's own paths do not exist yet.
    let placeholder = std::path::absolute(home.tasks_dir())?;
    let resources = VmResources::resolve(l);
    let prepared = home.prepare_firecracker(&image, l, &placeholder, None, None, resources)?;
    // Advisory: the images are sparse, so this is what the VMs may still write. The nearest
    // existing directory stands in for a work root that does not exist yet.
    let existing = placeholder
        .ancestors()
        .find(|p| p.is_dir())
        .unwrap_or(&placeholder);
    check_host_space(
        existing,
        resources.disk_bytes() + resources.scratch_bytes(),
        &[],
    )
    .map_err(CliError::other)?;
    let version = match &prepared.cfg.launcher {
        GuestLauncher::Fake { .. } => FAKE_FIRECRACKER_VERSION.to_string(),
        GuestLauncher::Real { .. } => {
            firecracker_version(&prepared.cfg.firecracker_bin).map_err(|e| {
                CliError::other(format!(
                    "firecracker worker unavailable: firecracker --version: {e}"
                ))
            })?
        }
    };
    Ok(FirecrackerRecord {
        image_id: image.id,
        image_digest: prepared.cfg.image_digest,
        version,
        jailed: prepared.jailed,
        resources,
    })
}

/// The host kernel release (`uname -r`), recorded for attribution.
fn host_kernel() -> String {
    rustix::system::uname()
        .release()
        .to_string_lossy()
        .into_owned()
}

fn validate(
    home: &Home,
    task: &Path,
    yes: bool,
    patch: Option<&Path>,
    model: Option<&str>,
) -> Result<Request, CliError> {
    let text = fs::read_to_string(task)
        .map_err(|e| CliError::usage(format!("cannot read {}: {e}", task.display())))?;
    let contract = Contract::parse(&text).map_err(|e| CliError::usage(e.to_string()))?;
    let source = Path::new(&contract.repository.source)
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| {
            CliError::usage(format!(
                "repository source {} is not a directory",
                contract.repository.source
            ))
        })?;
    let profile = home
        .resolve_profile(
            &contract.verification_profile,
            contract.profile_digest.as_deref(),
        )?
        .ok_or_else(|| {
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
            let actual = workspace_digest(&source)
                .map_err(|e| CliError::usage(format!("cannot digest {}: {e}", source.display())))?;
            if actual != wanted {
                return Err(CliError::usage(format!(
                    "repository revision {wanted} does not match the source, which is {actual}"
                )));
            }
            Some(wanted)
        }
    };
    let mut api_key = None;
    let (patch, model, transcript) = match (yes, patch, model) {
        (_, Some(_), Some(_)) => {
            return Err(CliError::usage("pass either --fake-agent-patch or --model"));
        }
        (true, None, None) => {
            return Err(CliError::usage(
                "--yes needs --fake-agent-patch FILE or --model anthropic:<model>|fake:<transcript>",
            ));
        }
        (_, Some(p), None) => (
            Some(
                fs::read_to_string(p)
                    .map_err(|e| CliError::usage(format!("cannot read {}: {e}", p.display())))?,
            ),
            None,
            None,
        ),
        (_, None, Some(spec)) => {
            let spec: ModelSpec = spec.parse().map_err(CliError::usage)?;
            match &spec {
                ModelSpec::Fake(path) => {
                    let bytes = fs::read(path).map_err(|e| {
                        CliError::usage(format!("cannot read {}: {e}", path.display()))
                    })?;
                    serde_json::from_slice::<Transcript>(&bytes)
                        .map_err(|e| CliError::usage(format!("{}: {e}", path.display())))?;
                    (None, Some(spec), Some(bytes))
                }
                // The key is resolved before anything is written: a missing one is exit 2
                // with the task untouched.
                ModelSpec::Anthropic(_) => {
                    home.checked_base_url()?;
                    if yes {
                        api_key = Some(home.api_key()?);
                    }
                    (None, Some(spec), None)
                }
            }
        }
        (false, None, None) => (None, None, None),
    };
    let firecracker = match home.worker.unwrap_or(WorkerKind::Host) {
        WorkerKind::Host => None,
        WorkerKind::Firecracker => Some(check_firecracker(home, &contract)?),
    };
    Ok(Request {
        contract,
        source,
        profile,
        expected_revision,
        patch,
        model,
        transcript,
        api_key,
        firecracker,
    })
}

fn capability_names(contract: &Contract) -> Vec<String> {
    contract
        .capabilities
        .iter()
        .map(|c| {
            serde_json::to_value(c)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        })
        .collect()
}

/// What the owner approves, on stderr.
fn summarize(
    task: &TaskId,
    contract: &Contract,
    repo: &Digest,
    profile: &Digest,
    agent: &str,
    fc: Option<&FirecrackerRecord>,
    endpoint: Option<&str>,
) {
    let l = &contract.limits;
    eprintln!("task {task} submitted; approve these permissions before it runs:");
    eprintln!("  goal:                 {}", contract.goal);
    eprintln!(
        "  repository:           {} at {repo}",
        contract.repository.source
    );
    eprintln!(
        "  capabilities:         {}",
        capability_names(contract).join(", ")
    );
    eprintln!(
        "  editable paths:       {}",
        contract.editable_paths.join(", ")
    );
    eprintln!(
        "  acceptance:           protected verification profile {} ({profile})",
        contract.verification_profile
    );
    eprintln!(
        "  limits:               model_requests={} max_output_tokens_per_request={} tool_actions={} deadline_seconds={} worker_vcpus={} worker_memory_mib={}",
        l.model_requests,
        l.max_output_tokens_per_request,
        l.tool_actions,
        l.deadline_seconds,
        l.worker_vcpus,
        l.worker_memory_mib
    );
    if let Some(a) = &contract.analyzer {
        eprintln!(
            "  analyzer:             {}@{}, reads the snapshot once before the first turn; its report is exported, never verified",
            a.id, a.digest
        );
    }
    eprintln!("  agent:                {agent}");
    if let Some(endpoint) = endpoint {
        eprintln!("  model endpoint:       {endpoint}");
    }
    match fc {
        None => eprintln!("  worker:               host (not sandboxed)"),
        Some(fc) => eprintln!(
            "  worker:               firecracker, guest image {}@{}, {}",
            fc.image_id,
            fc.image_digest,
            if fc.jailed { "jailed" } else { "unjailed" }
        ),
    }
    if let Some(fc) = fc {
        let r = &fc.resources;
        let rates = match (r.bandwidth_mib_s, r.iops) {
            (None, None) => "no rate limit".to_string(),
            (Some(b), None) => format!("each writable drive at most {b} MiB/s"),
            (None, Some(o)) => format!("each writable drive at most {o} operations/s"),
            (Some(b), Some(o)) => {
                format!("each writable drive at most {b} MiB/s and {o} operations/s")
            }
        };
        eprintln!(
            "  vm disks:             workspace {} MiB, scratch {} MiB, {rates}",
            r.disk_mib, r.scratch_mib
        );
    }
}

pub async fn submit(
    home: &Home,
    task_file: &Path,
    yes: bool,
    patch: Option<&Path>,
    model: Option<&str>,
    crash: Option<&CrashSpec>,
) -> Result<(), CliError> {
    let Request {
        mut contract,
        source,
        profile,
        expected_revision,
        patch,
        model,
        transcript,
        api_key,
        firecracker,
    } = validate(home, task_file, yes, patch, model)?;

    let store = home.open()?;
    let lock = if yes { Some(home.lock()?) } else { None };
    // Inputs are copied into the home, so the task runs on exactly what was digested here
    // whatever later happens to the source or the profile registry.
    let staging = tempfile::Builder::new()
        .prefix(".submit-")
        .tempdir_in(home.tasks_dir())?;
    copy_tree(&source, &staging.path().join("snapshot"))?;
    copy_tree(&profile, &staging.path().join("profile"))?;
    let repo_digest = workspace_digest(&staging.path().join("snapshot"))?;
    let profile_digest = workspace_digest(&staging.path().join("profile"))?;
    if let Some(pin) = contract
        .profile_digest
        .as_ref()
        .filter(|pin| **pin != profile_digest.to_string())
    {
        return Err(CliError::usage(format!(
            "verification profile changed while it was recorded: pinned {pin}, found {profile_digest}"
        )));
    }
    // The analyzer's registry entry is copied like the profile and must still be the pinned one.
    if let Some(a) = &contract.analyzer {
        let entry = home.resolve_component(&a.id, &a.digest)?;
        let staged = staging.path().join(ANALYZER_DIR);
        copy_tree(&entry.dir, &staged)?;
        let found = workspace_digest(&staged)?;
        if found.to_string() != a.digest {
            return Err(CliError::usage(format!(
                "analyzer changed while it was recorded: pinned {}, found {found}",
                a.digest
            )));
        }
    }
    if expected_revision.is_some_and(|d| d != repo_digest) {
        return Err(CliError::usage(format!(
            "repository source changed while it was recorded (now {repo_digest})"
        )));
    }
    if let Some(p) = &patch {
        fs::write(staging.path().join(AGENT_PATCH), p)?;
    }
    if let Some(bytes) = &transcript {
        fs::write(staging.path().join(TRANSCRIPT), bytes)?;
    }
    contract.repository.revision = repo_digest.to_string();
    contract.repository.source = source.display().to_string();
    let contract_digest = Digest::of(&serde_json::to_vec(&contract)?);

    let task = store.db.create_task(&contract, &contract_digest)?;
    fs::rename(staging.keep(), home.task_dir(&task))?;
    let mut submitted = json!({
        "contract_digest": contract_digest,
        "repository_source": contract.repository.source,
        "repository_digest": repo_digest,
        "profile_id": contract.verification_profile,
        "profile_digest": profile_digest,
        "model": model.as_ref().map_or_else(|| FAKE_AGENT.to_string(), ModelSpec::recorded),
        "model_policy_version": 1,
        "model_limits_version": 1,
        "model_endpoint": match &model {
            Some(ModelSpec::Anthropic(_)) => Some(home.checked_base_url()?
                .unwrap_or_else(|| agentos_engine::model::anthropic::ANTHROPIC_BASE_URL.into())
                .trim_end_matches('/').to_string()),
            _ => None,
        },
        "fake_agent_patch_digest": patch.as_ref().map(|p| Digest::of(p.as_bytes())),
    });
    if let Some(a) = &contract.analyzer {
        let fields = submitted.as_object_mut().expect("an object");
        fields.insert("analyzer_id".into(), json!(a.id));
        fields.insert("analyzer_digest".into(), json!(a.digest));
    }
    let fields = submitted.as_object_mut().expect("an object");
    let transcript_digest = transcript.as_ref().map(|b| Digest::of(b));
    if let Some(d) = &transcript_digest {
        fields.insert("transcript_digest".into(), json!(d));
    }
    match &firecracker {
        None => {
            fields.insert("worker".into(), json!(WorkerKind::Host.as_str()));
            fields.insert("guest_image".into(), json!(GUEST_IMAGE));
        }
        Some(fc) => {
            fields.insert("worker".into(), json!(WorkerKind::Firecracker.as_str()));
            fields.insert("guest_image_id".into(), json!(fc.image_id));
            fields.insert("guest_image_digest".into(), json!(fc.image_digest));
            fields.insert("firecracker_version".into(), json!(fc.version));
            fields.insert("host_kernel".into(), json!(host_kernel()));
            fields.insert("jailed".into(), json!(fc.jailed));
            fields.insert("vm_resources".into(), json!(fc.resources));
        }
    }
    store.db.append_audit(&task, "Submitted", &submitted)?;
    tracing::info!(task_id = %task, %contract_digest, %repo_digest, %profile_digest, "task submitted");
    let agent = match (&patch, &model, &transcript_digest) {
        (Some(p), _, _) => format!("{FAKE_AGENT} (patch {})", Digest::of(p.as_bytes())),
        (None, Some(m), Some(d)) => format!("{} (transcript {d})", m.recorded()),
        (None, Some(m), None) => m.recorded(),
        (None, None, _) => "none yet".to_string(),
    };
    summarize(
        &task,
        &contract,
        &repo_digest,
        &profile_digest,
        &agent,
        firecracker.as_ref(),
        submitted["model_endpoint"].as_str(),
    );

    match lock.filter(|_| patch.is_some() || model.is_some()) {
        Some(lock) => {
            // The executor (preflight and jail included) before the approval: a host that
            // changed since the checks above leaves the task READY, not approved.
            let exec = home.executor(&store, &task, api_key)?;
            let agent = agent_for(home, &store, &task, None)?;
            // `--yes` is the owner's approval: issue the task's capability handles.
            store.db.approve_task(&task)?;
            let state = drive(home, &store, &lock, &task, agent, crash, exec).await?;
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
