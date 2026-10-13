//! `agentos workflow`: a chain of task contracts run one after the other on one workspace.
//! Every step is an ordinary task. It is created only once the previous step has succeeded,
//! from the approved chain alone, on the previous step's verified result (its handoff).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use agentos_core::ids::{Digest, TaskId, WorkflowId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_core::workflow::{ChainSpec, StepStatus, WorkflowState, derive_state};
use agentos_engine::export::{handoff, manifest};
use agentos_engine::handoff::materialize;
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::supervised::SupervisedExecutor;
use agentos_engine::workspace::workspace_digest;
use agentos_store::db::DbError;
use agentos_store::workflows::WorkflowRecord;
use serde_json::{Value, json};

use super::control::{cancel_task, resume_task};
use super::export::export_task;
use super::submit::{
    RECORDED_AT_SUBMISSION, check_agent_cli_contract, check_agent_cli_flags, record_step_task,
    terms, validate_contract,
};
use super::{print, workflow_id};
use crate::args::{WorkerKind, WorkflowCommand};
use crate::crash::CrashSpec;
use crate::drive::{AgentCli, Driver, FAKE_AGENT, ModelSpec, drive};
use crate::error::CliError;
use crate::home::{DriverLock, Home, Store, TEST_WORKERS_ENV};

/// The workflow's own copy of a `fake:` transcript, which every step replays.
const TRANSCRIPT: &str = "transcript.json";
/// Test hook: exit 137 once step N (1-based) has succeeded, before the next step's task exists.
const EXIT_AFTER_STEP_ENV: &str = "AGENTOS_TEST_WORKFLOW_EXIT_AFTER_STEP";
/// How a step task that was created but never recorded as submitted ends.
const NOT_CREATED: &str = "step was not completely created";

fn step_patch(idx: usize) -> String {
    format!("step-{idx}.patch")
}

pub async fn dispatch(home: Home, command: WorkflowCommand) -> Result<(), CliError> {
    match command {
        WorkflowCommand::Submit {
            chain,
            yes,
            step_patch,
            model,
            agent_cli,
            crash_at,
        } => {
            submit(
                home,
                &chain,
                yes,
                &step_patch,
                model.as_deref(),
                agent_cli,
                crash_at.as_ref(),
            )
            .await
        }
        WorkflowCommand::Resume { id, crash_at } => {
            resume(home, &workflow_id(&id)?, crash_at.as_ref()).await
        }
        WorkflowCommand::Status { id } => {
            let (_, store, w) = open(home, &workflow_id(&id)?)?;
            print(&status(&store, &w)?);
            Ok(())
        }
        WorkflowCommand::Cancel { id } => cancel(home, &workflow_id(&id)?).await,
        WorkflowCommand::Export { id, dir } => export(home, &workflow_id(&id)?, &dir),
    }
}

/// The store and the workflow, with `home.worker` set to the worker recorded at submission;
/// a `--worker` naming another one is refused (exit 2).
fn open(mut home: Home, id: &WorkflowId) -> Result<(Home, Store, WorkflowRecord), CliError> {
    let store = home.open()?;
    let w = store.db.workflow(id).map_err(|e| match e {
        DbError::Workflow(_) => CliError::other(format!("unknown workflow {id}")),
        e => e.into(),
    })?;
    let recorded = match w.options["worker"].as_str() {
        Some("host") => WorkerKind::Host,
        Some("firecracker") => WorkerKind::Firecracker,
        other => {
            return Err(CliError::other(format!(
                "workflow {id} records an unknown worker {other:?}"
            )));
        }
    };
    if home.worker.is_some_and(|flag| flag != recorded) {
        return Err(CliError::usage(format!(
            "workflow was submitted with worker {}",
            recorded.as_str()
        )));
    }
    home.worker = Some(recorded);
    Ok((home, store, w))
}

/// The chain's state from its steps, and each step's status. A chain failed by
/// `fail_workflow` (its handoff could not be built) is failed at its first skipped step.
fn state_of(
    store: &Store,
    w: &WorkflowRecord,
) -> Result<(WorkflowState, Vec<StepStatus>), CliError> {
    let mut steps = Vec::new();
    for s in &w.steps {
        steps.push(match &s.task_id {
            Some(task) => StepStatus::Task(store.db.task(task)?.state),
            None if s.skipped => StepStatus::Skipped,
            None => StepStatus::Pending,
        });
    }
    let state = match w.options.get("failure") {
        Some(_) => WorkflowState::Failed {
            step: steps
                .iter()
                .position(|s| *s == StepStatus::Skipped)
                .unwrap_or(steps.len() - 1),
        },
        None => derive_state(&steps),
    };
    Ok((state, steps))
}

/// `READY` for a chain not approved yet, else the derived state's label.
fn label(w: &WorkflowRecord, state: WorkflowState) -> &'static str {
    if state == WorkflowState::Running && !w.approved {
        "READY"
    } else {
        state.label()
    }
}

/// What `workflow status` prints. A step that never got a task in a finished chain is
/// `SKIPPED`, in a running one `PENDING`.
fn status(store: &Store, w: &WorkflowRecord) -> Result<Value, CliError> {
    let (state, statuses) = state_of(store, w)?;
    let finished = state != WorkflowState::Running;
    let steps: Vec<Value> = w
        .steps
        .iter()
        .zip(&statuses)
        .map(|(s, st)| {
            let state = match st {
                StepStatus::Task(t) => t.label(),
                StepStatus::Skipped => "SKIPPED",
                StepStatus::Pending if finished => "SKIPPED",
                StepStatus::Pending => "PENDING",
            };
            let mut step = json!({ "idx": s.idx, "name": s.name, "state": state });
            if let Some(task) = &s.task_id {
                step["task_id"] = json!(task);
            }
            step
        })
        .collect();
    let mut out = json!({
        "workflow_id": w.id,
        "name": w.spec.name,
        "state": label(w, state),
        "steps": steps,
    });
    if let WorkflowState::Failed { step } = state {
        out["failed_step"] = json!(w.steps[step].name);
    }
    if let Some(reason) = w.options.get("failure") {
        out["failure"] = reason.clone();
    }
    Ok(out)
}

async fn submit(
    home: Home,
    chain: &Path,
    yes: bool,
    step_patches: &[PathBuf],
    model: Option<&str>,
    agent_cli: Option<AgentCli>,
    crash: Option<&CrashSpec>,
) -> Result<(), CliError> {
    // Everything is checked before anything is written.
    let text = fs::read_to_string(chain)
        .map_err(|e| CliError::usage(format!("cannot read {}: {e}", chain.display())))?;
    let mut spec = ChainSpec::parse(&text).map_err(|e| CliError::usage(e.to_string()))?;
    let n = spec.steps.len();
    if agent_cli.is_some() {
        check_agent_cli_flags(&home, step_patches.first().map(PathBuf::as_path), model)?;
    }
    if model.is_some() {
        if !step_patches.is_empty() {
            return Err(CliError::usage("pass either --step-patch or --model"));
        }
    } else if step_patches.len() != n {
        return Err(CliError::usage(format!(
            "the fake agent needs one --step-patch per step: the chain has {n} steps, got {}",
            step_patches.len()
        )));
    }
    let patches = step_patches
        .iter()
        .map(|p| {
            fs::read_to_string(p)
                .map_err(|e| CliError::usage(format!("cannot read {}: {e}", p.display())))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Step 1 is checked in full; the later steps' input does not exist yet.
    let first = validate_contract(
        &home,
        spec.steps[0].contract.clone(),
        yes,
        step_patches.first().map(PathBuf::as_path),
        model,
        agent_cli,
        None,
    )?;
    let worker = home.worker.unwrap_or(WorkerKind::Host);
    let profile_digests = pin_steps(&home, &mut spec, worker)?;
    if let Some(cli) = agent_cli {
        for step in &spec.steps[1..] {
            check_agent_cli_contract(&step.contract, cli)?;
        }
    }
    let model_spec = model
        .map(str::parse::<ModelSpec>)
        .transpose()
        .map_err(CliError::usage)?;
    let transcript = match &model_spec {
        Some(ModelSpec::Fake(path)) => Some(
            fs::read(path)
                .map_err(|e| CliError::usage(format!("cannot read {}: {e}", path.display())))?,
        ),
        _ => None,
    };
    // The approved chain names the source by its canonical path, so a later `resume` from
    // another directory finds it; and `recorded-at-submission` means this submission: the
    // chain names the source's digest now, so step 1 runs on what was approved however late
    // the chain is resumed.
    let source = Path::new(&spec.repository.source).canonicalize()?;
    spec.repository.source = source.display().to_string();
    spec.steps[0].contract.repository.source = spec.repository.source.clone();
    if spec.repository.revision == RECORDED_AT_SUBMISSION {
        let digest = workspace_digest(&source)
            .map_err(|e| CliError::usage(format!("cannot digest {}: {e}", source.display())))?
            .to_string();
        spec.repository.revision = digest.clone();
        spec.steps[0].contract.repository.revision = digest;
    }
    let transcript_digest = transcript.as_deref().map(Digest::of);
    let options = json!({
        "worker": worker.as_str(),
        // The jail decision approved with the chain binds every step, as `Submitted.jailed`
        // binds a task's later runs.
        "jailed": first.jailed(),
        "profile_digests": profile_digests,
        "model": model,
        "agent_cli": agent_cli.map(AgentCli::name),
        "step_patch_digests": patches.iter().map(|p| Digest::of(p.as_bytes())).collect::<Vec<_>>(),
        "transcript_digest": transcript_digest,
    });
    let agent = match (agent_cli, &model_spec) {
        (Some(cli), Some(m)) => format!(
            "{} session in the microVM, model {}, for every step",
            cli.name(),
            m.recorded()
        ),
        (None, Some(m)) => match &transcript_digest {
            Some(d) => format!("{} (transcript {d}), for every step", m.recorded()),
            None => format!("{}, for every step", m.recorded()),
        },
        (_, None) => format!(
            "{FAKE_AGENT}, one patch per step: {}",
            patches
                .iter()
                .map(|p| Digest::of(p.as_bytes()).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };

    let store = home.open()?;
    let lock = if yes { Some(home.lock()?) } else { None };
    fs::create_dir_all(home.workflows_dir())?;
    let staging = tempfile::Builder::new()
        .prefix(".submit-")
        .tempdir_in(home.workflows_dir())?;
    for (idx, patch) in patches.iter().enumerate() {
        fs::write(staging.path().join(step_patch(idx)), patch)?;
    }
    if let Some(bytes) = &transcript {
        fs::write(staging.path().join(TRANSCRIPT), bytes)?;
    }
    let id = store.db.create_workflow(&spec, &options)?;
    fs::rename(staging.keep(), home.workflow_dir(&id))?;
    tracing::info!(workflow_id = %id, spec_digest = %spec.digest(), "workflow submitted");
    summarize(&id, &spec, &profile_digests, &agent, &first.worker_line());

    match lock {
        Some(lock) => {
            // `--yes` is the owner's approval of the whole chain.
            store.db.approve_workflow(&id)?;
            run_workflow(&home, &store, &lock, &id, crash).await?;
            print(&status(&store, &store.db.workflow(&id)?)?);
        }
        None => print(&json!({
            "workflow_id": id,
            "state": "READY",
            "note": format!("not started; run agentos workflow resume {id}"),
        })),
    }
    Ok(())
}

/// Pins every step's verification profile and, for the Firecracker worker, its guest image to
/// what they resolve to now, so the approval names exact bytes; returns each step's approved
/// profile digest. A registry profile is pinned in the contract (and so in the spec digest).
/// A profile from the legacy `--profiles` directory cannot be (a pin names a registry entry):
/// its digest is checked when the step starts.
fn pin_steps(
    home: &Home,
    spec: &mut ChainSpec,
    worker: WorkerKind,
) -> Result<Vec<Digest>, CliError> {
    let mut digests = Vec::new();
    for step in &mut spec.steps {
        let c = &mut step.contract;
        let id = c.verification_profile.clone();
        let digest = match &c.profile_digest {
            Some(pin) => {
                home.resolve_profile(&id, Some(pin))?;
                pin.clone()
            }
            None => match home.registry_entries(&id).pop() {
                Some(entry) => {
                    c.profile_digest = Some(entry.digest.clone());
                    entry.digest
                }
                None => {
                    let dir = home.profile_dir(&id)?.ok_or_else(|| {
                        CliError::usage(format!(
                            "step {:?}: verification profile {id} not found in the registry {} or in {}",
                            step.name,
                            home.registry_dir().display(),
                            home.profiles.display()
                        ))
                    })?;
                    workspace_digest(&dir)
                        .map_err(|e| {
                            CliError::usage(format!("cannot digest {}: {e}", dir.display()))
                        })?
                        .to_string()
                }
            },
        };
        digests.push(
            Digest::from_hex(&digest).map_err(|e| {
                CliError::usage(format!("step {:?}: profile digest: {e}", step.name))
            })?,
        );
        if worker == WorkerKind::Firecracker {
            let image = home
                .resolve_image(&c.profile, c.guest_image_digest.as_deref())?
                .ok_or_else(|| {
                    CliError::usage(format!(
                        "step {:?}: guest image {} not found in the registry {}; build and register it first",
                        step.name,
                        c.profile,
                        home.images_dir().display()
                    ))
                })?;
            c.guest_image_digest = Some(image.digest);
        }
    }
    Ok(digests)
}

/// What the owner approves, on stderr: every step's terms, the agent, the worker and the
/// summed budget.
fn summarize(id: &WorkflowId, spec: &ChainSpec, profiles: &[Digest], agent: &str, worker: &str) {
    eprintln!(
        "workflow {id} submitted; approve this chain of {} steps before it runs:",
        spec.steps.len()
    );
    eprintln!(
        "  repository:           {} at {}",
        spec.repository.source, spec.repository.revision
    );
    for (i, (step, profile)) in spec.steps.iter().zip(profiles).enumerate() {
        eprintln!("  step {} {}:", i + 1, step.name);
        eprintln!("    goal:                 {}", step.contract.goal);
        for (label, value) in terms(&step.contract, Some(profile)) {
            eprintln!("    {:<22}{value}", format!("{label}:"));
        }
        if let Some(image) = &step.contract.guest_image_digest {
            eprintln!(
                "    guest image:          {}@{image}",
                step.contract.profile
            );
        }
    }
    eprintln!("  agent:                {agent}");
    eprintln!("  worker:               {worker}");
    let sum = |f: &dyn Fn(&agentos_core::contract::Limits) -> u64| -> u64 {
        spec.steps.iter().map(|s| f(&s.contract.limits)).sum()
    };
    eprintln!(
        "  total budget:         model_requests={} tool_actions={} deadline_seconds={}",
        sum(&|l| u64::from(l.model_requests)),
        sum(&|l| u64::from(l.tool_actions)),
        sum(&|l| u64::from(l.deadline_seconds)),
    );
}

async fn resume(home: Home, id: &WorkflowId, crash: Option<&CrashSpec>) -> Result<(), CliError> {
    let (home, store, w) = open(home, id)?;
    let (state, _) = state_of(&store, &w)?;
    if state != WorkflowState::Running {
        return Err(CliError::other(format!(
            "workflow {id} is {}; nothing to resume",
            state.label()
        )));
    }
    let lock = home.lock()?;
    // Resuming is the owner's approval of the whole chain; approving again changes nothing.
    store.db.approve_workflow(id)?;
    run_workflow(&home, &store, &lock, id, crash).await?;
    print(&status(&store, &store.db.workflow(id)?)?);
    Ok(())
}

/// Runs the chain from its durable state, one step at a time, as far as it goes: a step's
/// task is driven like `agentos resume` drives a task, and the next step's task is created only
/// once it has succeeded and the chain is not being cancelled. The caller holds `lock`.
pub(crate) async fn run_workflow(
    home: &Home,
    store: &Store,
    lock: &DriverLock,
    id: &WorkflowId,
    crash: Option<&CrashSpec>,
) -> Result<(), CliError> {
    let n = store.db.workflow(id)?.steps.len();
    for idx in 0..n {
        let w = store.db.workflow(id)?;
        let step = &w.steps[idx];
        let state = match &step.task_id {
            Some(task) if !submitted(store, task)? => {
                let t = store.db.task(task)?;
                if t.state.is_terminal() {
                    t.state
                } else {
                    let reason = NOT_CREATED.to_string();
                    store.db.append(task, &TaskEvent::Failed { reason })?.state
                }
            }
            Some(task) => resume_task(home, store, Some(lock), task, None, crash).await?,
            None => {
                if step.skipped || w.cancel_requested {
                    return Ok(());
                }
                if idx > 0 {
                    exit_hook(idx);
                }
                let Some((task, exec, agent)) = start_step(home, store, &w, idx).await? else {
                    return Ok(());
                };
                // The chain's approval covers each step: issue this task's own handles.
                store.db.approve_task(&task)?;
                drive(home, store, lock, &task, agent, crash, exec).await?
            }
        };
        if state != TaskState::Succeeded {
            return Ok(());
        }
    }
    Ok(())
}

fn submitted(store: &Store, task: &TaskId) -> Result<bool, CliError> {
    Ok(store
        .db
        .events(task)?
        .iter()
        .any(|e| e.event_type == "Submitted"))
}

/// Honoured only with `AGENTOS_TEST_WORKERS=1`, like every test hook.
fn exit_hook(done: usize) {
    if std::env::var(TEST_WORKERS_ENV).as_deref() != Ok("1") {
        return;
    }
    let wanted = std::env::var(EXIT_AFTER_STEP_ENV)
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    if wanted == Some(done) {
        eprintln!("{}", json!({ "exited_after_step": done }));
        std::process::exit(137);
    }
}

type Started = (TaskId, RoutingExecutor<SupervisedExecutor>, Driver);

/// Creates step `idx`'s task from the approved spec, on the previous step's handoff for a
/// later step, and builds its executor and agent (before its approval, as `submit` does).
/// `None` when the chain was failed here (no handoff, changed inputs) or cancelled meanwhile.
async fn start_step(
    home: &Home,
    store: &Store,
    w: &WorkflowRecord,
    idx: usize,
) -> Result<Option<Started>, CliError> {
    let dir = home.workflow_dir(&w.id);
    let name = &w.spec.steps[idx].name;
    let mut contract = w.spec.steps[idx].contract.clone();
    let handoff_dir = dir.join(format!("handoff-{idx}"));
    if idx > 0 {
        let prev = w.steps[idx - 1].task_id.as_ref().ok_or_else(|| {
            CliError::other(format!("workflow {}: step {} has no task", w.id, idx - 1))
        })?;
        fs::create_dir_all(&dir)?;
        // A directory left by an earlier attempt is never trusted: it is rebuilt.
        match fs::symlink_metadata(&handoff_dir) {
            Ok(m) if m.is_dir() => fs::remove_dir_all(&handoff_dir)?,
            Ok(_) => fs::remove_file(&handoff_dir)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let built = match handoff(&store.db, &store.blobs, prev) {
            Ok(h) => materialize(&home.task_dir(prev).join("snapshot"), &h, &handoff_dir)
                .await
                .map(|()| h.final_digest),
            Err(e) => Err(e.to_string()),
        };
        match built {
            Ok(digest) => {
                contract.repository.source = handoff_dir.display().to_string();
                contract.repository.revision = digest.to_string();
            }
            Err(reason) => {
                let reason = format!("handoff to step {name:?} failed: {reason}");
                tracing::warn!(workflow_id = %w.id, %reason, "workflow failed");
                store.db.fail_workflow(&w.id, &reason)?;
                return Ok(None);
            }
        }
    }
    let inputs = match step_inputs(&dir, w, idx)? {
        Ok(inputs) => inputs,
        Err(reason) => {
            store.db.fail_workflow(&w.id, &reason)?;
            return Ok(None);
        }
    };
    // A profile from the legacy `--profiles` directory is not pinned in the contract: it must
    // still be the one approved (checked again on the recorded copy below).
    let approved_profile = w.options["profile_digests"][idx]
        .as_str()
        .and_then(|d| Digest::from_hex(d).ok())
        .ok_or_else(|| {
            CliError::other(format!(
                "workflow {} has no approved profile digest for step {name:?}",
                w.id
            ))
        })?;
    let unpinned = contract.profile_digest.is_none();
    if unpinned {
        let id = &contract.verification_profile;
        if let Some(dir) = home.resolve_profile(id, None)? {
            let found = workspace_digest(&dir)
                .map_err(|e| CliError::usage(format!("cannot digest {}: {e}", dir.display())))?;
            if found != approved_profile {
                return Err(CliError::usage(format!(
                    "step {name:?}: verification profile {id} changed since the workflow was approved: approved {approved_profile}, found {found}"
                )));
            }
        }
    }
    let in_step = |e: CliError| CliError {
        code: e.code,
        message: format!("workflow {}, step {name:?}: {}", w.id, e.message),
    };
    // The jail decision approved with the chain binds this step as `Submitted.jailed` binds a
    // task: approved jailed never runs unjailed, approved unjailed stays so without the flag.
    let req = validate_contract(
        home,
        contract,
        true,
        inputs.patch.as_deref(),
        inputs.model.as_deref(),
        inputs.agent_cli,
        w.options["jailed"].as_bool(),
    )
    .map_err(in_step)?;
    let rec = match record_step_task(home, store, req, &w.id, idx).await? {
        Ok(rec) => rec,
        // The store refused: a cancel that landed since the chain was read wins, and the step
        // stays skipped; any other refusal is an error.
        Err(_) if store.db.workflow(&w.id)?.cancel_requested => return Ok(None),
        Err(refusal) => return Err(CliError::other(refusal)),
    };
    if unpinned && rec.profile_digest != approved_profile {
        // Changed between the check above and the copy: the task never runs on it.
        let reason = format!(
            "verification profile changed since the workflow was approved: approved {approved_profile}, recorded {}",
            rec.profile_digest
        );
        store.db.append(&rec.task, &TaskEvent::Failed { reason })?;
        return Ok(None);
    }
    if idx > 0 {
        // The task holds its own recorded copy of the handoff.
        fs::remove_dir_all(&handoff_dir)?;
    }
    let task = rec.task.clone();
    let (exec, agent) = rec.runner(home, store)?;
    Ok(Some((task, exec, agent)))
}

/// What `validate_contract` needs for step `idx` beyond its contract, from the workflow's
/// recorded inputs.
struct Inputs {
    patch: Option<PathBuf>,
    model: Option<String>,
    agent_cli: Option<AgentCli>,
}

/// The step's recorded inputs, which must still be the files whose digests were approved;
/// `Ok(Err(reason))` when one changed.
fn step_inputs(
    dir: &Path,
    w: &WorkflowRecord,
    idx: usize,
) -> Result<Result<Inputs, String>, CliError> {
    let o = &w.options;
    let agent_cli = o["agent_cli"]
        .as_str()
        .map(str::parse::<AgentCli>)
        .transpose()
        .map_err(|e| CliError::other(format!("workflow {}: {e}", w.id)))?;
    let unchanged = |file: &Path, recorded: &Value| -> Result<bool, CliError> {
        let bytes = fs::read(file).map_err(|e| {
            CliError::other(format!("cannot read recorded {}: {e}", file.display()))
        })?;
        Ok(recorded.as_str() == Some(Digest::of(&bytes).to_string().as_str()))
    };
    let (patch, model) = match o["model"].as_str() {
        None => {
            let patch = dir.join(step_patch(idx));
            if !unchanged(&patch, &o["step_patch_digests"][idx])? {
                return Ok(Err(format!("recorded {} changed", step_patch(idx))));
            }
            (Some(patch), None)
        }
        Some(m) if m.starts_with("fake:") => {
            let transcript = dir.join(TRANSCRIPT);
            if !unchanged(&transcript, &o["transcript_digest"])? {
                return Ok(Err(format!("recorded {TRANSCRIPT} changed")));
            }
            (None, Some(format!("fake:{}", transcript.display())))
        }
        Some(m) => (None, Some(m.to_string())),
    };
    Ok(Ok(Inputs {
        patch,
        model,
        agent_cli,
    }))
}

/// Stops the chain: no further step starts, and a step task still running is cancelled as
/// `agentos cancel` cancels a task.
async fn cancel(home: Home, id: &WorkflowId) -> Result<(), CliError> {
    let (home, store, _) = open(home, id)?;
    store.db.request_workflow_cancel(id)?;
    // Read after the request: from now on no step task can be created, so this is every one.
    let w = store.db.workflow(id)?;
    for task in w.steps.iter().filter_map(|s| s.task_id.as_ref()) {
        if !store.db.task(task)?.state.is_terminal() {
            cancel_task(&home, &store, task).await?;
        }
    }
    print(&status(&store, &store.db.workflow(id)?)?);
    Ok(())
}

/// The last step's bundle of a succeeded chain, plus `chain.json`: every step's task and
/// digests in order.
fn export(home: Home, id: &WorkflowId, dir: &Path) -> Result<(), CliError> {
    let (_, store, w) = open(home, id)?;
    let (state, _) = state_of(&store, &w)?;
    match state {
        WorkflowState::Succeeded => {}
        WorkflowState::Running => {
            return Err(CliError::other(format!(
                "workflow {id} is {}; only a succeeded workflow can be exported",
                label(&w, state)
            )));
        }
        WorkflowState::Failed { step } | WorkflowState::Cancelled { step } => {
            return Err(CliError::other(format!(
                "workflow {id} is {} at step {:?}; only a succeeded workflow can be exported",
                state.label(),
                w.steps[step].name
            )));
        }
    }
    let mut steps = Vec::new();
    let mut last = None;
    for s in &w.steps {
        let task = s
            .task_id
            .as_ref()
            .ok_or_else(|| CliError::other(format!("workflow {id}: step {} has no task", s.idx)))?;
        let m = manifest(&store.db, &store.blobs, task)?;
        steps.push(json!({
            "idx": s.idx,
            "name": s.name,
            "task_id": task,
            "contract_digest": m.contract_digest,
            "patch_digest": m.patch_digest,
            "verified_digest": m.verified_digest,
        }));
        last = Some(task);
    }
    let chain = json!({
        "workflow_id": id,
        "name": w.spec.name,
        "spec_digest": w.spec_digest,
        "state": state.label(),
        "steps": steps,
    });
    let last = last.ok_or_else(|| CliError::other(format!("workflow {id} has no steps")))?;
    export_task(&store, last, dir)?;
    let mut f = File::create_new(dir.join("chain.json"))?;
    f.write_all(&serde_json::to_vec_pretty(&chain)?)?;
    f.sync_all()?;
    print(&chain);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chain(source: &Path) -> ChainSpec {
        let contract = json!({
            "goal": "fix the parser", "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        });
        let steps: Vec<_> = ["implement", "review"]
            .iter()
            .map(|name| json!({ "name": name, "contract": contract }))
            .collect();
        ChainSpec::parse(
            &json!({
                "name": "chain",
                "repository": { "source": source, "revision": "recorded-at-submission" },
                "steps": steps
            })
            .to_string(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_step_task_without_a_submitted_event_fails_the_chain_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(Some(dir.path().join("home")), None).unwrap();
        let store = home.open().unwrap();
        let spec = chain(dir.path());
        let id = store.db.create_workflow(&spec, &json!({})).unwrap();
        store.db.approve_workflow(&id).unwrap();
        // The controller died after linking step 1's task, before recording its submission.
        let contract = &spec.steps[0].contract;
        let task = store
            .db
            .create_step_task(&id, 0, contract, &Digest::of(b"contract"))
            .unwrap();
        assert!(!submitted(&store, &task).unwrap());
        let lock = home.lock().unwrap();

        run_workflow(&home, &store, &lock, &id, None).await.unwrap();

        let w = store.db.workflow(&id).unwrap();
        assert_eq!(w.steps[0].task_id.as_ref(), Some(&task));
        assert_eq!(w.steps[1].task_id, None, "no second task");
        let t = store.db.task(&task).unwrap();
        assert_eq!(t.state, TaskState::Failed);
        let failed: Vec<_> = store
            .db
            .events(&task)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == "Failed")
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(
            failed[0].payload.to_string().contains(NOT_CREATED),
            "{:?}",
            failed[0]
        );
        let (state, _) = state_of(&store, &w).unwrap();
        assert_eq!(state, WorkflowState::Failed { step: 0 });

        // Running again changes nothing: the failed task is not failed twice.
        run_workflow(&home, &store, &lock, &id, None).await.unwrap();
        assert_eq!(store.db.events(&task).unwrap().len(), 2);
        assert_eq!(store.db.workflow(&id).unwrap().steps[1].task_id, None);
    }
}
