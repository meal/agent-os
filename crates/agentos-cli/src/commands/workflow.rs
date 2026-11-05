//! `agentos workflow`: a chain of task contracts run one after the other on one workspace.
//! Every step is an ordinary task. It is created only once the previous step has succeeded,
//! from the approved chain alone, on the previous step's verified result (its handoff).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use agentos_core::ids::{Digest, TaskId, WorkflowId};
use agentos_core::state::{TaskEvent, TaskState};
use agentos_core::workflow::{ChainSpec, StepStatus, WorkflowState, display_label, state_of_steps};
use agentos_engine::export::{ExportError, handoff, manifest};
use agentos_engine::handoff::{HandoffError, materialize};
use agentos_engine::model::anthropic::ANTHROPIC_BASE_URL;
use agentos_engine::routing::RoutingExecutor;
use agentos_engine::supervised::SupervisedExecutor;
use agentos_engine::workspace::workspace_digest;
use agentos_store::db::DbError;
use agentos_store::workflows::WorkflowRecord;
use serde_json::{Value, json};

use super::control::{cancel_task, resume_task};
use super::export::export_task_with;
use super::submit::{
    RECORDED_AT_SUBMISSION, StepRefusal, check_agent_cli_contract, check_agent_cli_flags,
    record_step_task, terms, validate_contract,
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
/// Test hook: exit 137 once a submission's inputs are in place, before the workflow is recorded.
const EXIT_BEFORE_RECORD_ENV: &str = "AGENTOS_TEST_WORKFLOW_EXIT_BEFORE_RECORD";
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
            let (_, store, w) = open(home, &workflow_id(&id)?, false)?;
            print(&status(&store, &w)?);
            Ok(())
        }
        WorkflowCommand::Cancel { id } => cancel(home, &workflow_id(&id)?).await,
        WorkflowCommand::Export { id, dir } => export(home, &workflow_id(&id)?, &dir),
    }
}

/// The store and the workflow, with `home.worker` set to the worker recorded at submission;
/// a `--worker` naming another one is refused (exit 2).
fn open(
    mut home: Home,
    id: &WorkflowId,
    gate_endpoint: bool,
) -> Result<(Home, Store, WorkflowRecord), CliError> {
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
    // Every step uses the endpoint approved with the chain, as a task uses its recorded one;
    // an override naming another is refused before anything else happens. Only commands that
    // may contact the model gate on it; `status`, `cancel` and `export` must always work.
    if let Some(endpoint) = w.options["model_endpoint"].as_str() {
        if gate_endpoint
            && home
                .checked_base_url()?
                .is_some_and(|requested| requested.trim_end_matches('/') != endpoint)
        {
            return Err(CliError::usage(
                "provider override differs from the recorded endpoint; submit a new workflow to change providers",
            ));
        }
        home.anthropic_base_url = Some(endpoint.to_string());
    }
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
        let task = s.task_id.as_ref().map(|t| store.db.task(t)).transpose()?;
        steps.push(StepStatus::of(task.map(|t| t.state), s.skipped));
    }
    let state = state_of_steps(&steps, w.failure.is_some());
    Ok((state, steps))
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
        "state": display_label(state, w.approved),
        "steps": steps,
    });
    if let WorkflowState::Failed { step } = state {
        out["failed_step"] = json!(w.steps[step].name);
    }
    if let Some(reason) = &w.failure {
        out["failure"] = json!(reason);
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
        check_agent_cli_flags(
            &home,
            "--step-patch",
            step_patches.first().map(PathBuf::as_path),
            model,
        )?;
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
    // Step 1's patch is the one `validate_contract` read and checked: never read it twice.
    let mut patches: Vec<String> = first.patch().map(str::to_string).into_iter().collect();
    for p in step_patches.iter().skip(1) {
        patches.push(
            fs::read_to_string(p)
                .map_err(|e| CliError::usage(format!("cannot read {}: {e}", p.display())))?,
        );
    }
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
    // The transcript `validate_contract` read and parsed, not a second read of the file.
    let transcript = first.transcript().map(<[u8]>::to_vec);
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
    // The provider endpoint is approved with the chain, normalized as `Submitted` records it.
    let model_endpoint = match &model_spec {
        Some(ModelSpec::Anthropic(_)) => Some(
            home.checked_base_url()?
                .unwrap_or_else(|| ANTHROPIC_BASE_URL.into())
                .trim_end_matches('/')
                .to_string(),
        ),
        _ => None,
    };
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
        "model_endpoint": model_endpoint,
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
    // The inputs are in place before the row that names them exists: a crash in between
    // leaves a directory no workflow refers to, never a workflow without its inputs. A failed
    // rename leaves `staging` to be removed on drop.
    let id = WorkflowId::new();
    let dest = home.workflow_dir(&id);
    fs::rename(staging.path(), &dest)?;
    let _ = staging.keep();
    test_exit(EXIT_BEFORE_RECORD_ENV, "exited_before_record");
    if let Err(e) = store.db.create_workflow_as(&id, &spec, &options) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e.into());
    }
    tracing::info!(workflow_id = %id, spec_digest = %spec.digest(), "workflow submitted");
    summarize(
        &id,
        &spec,
        &profile_digests,
        &agent,
        model_endpoint.as_deref(),
        &first.worker_line(),
    );

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
fn summarize(
    id: &WorkflowId,
    spec: &ChainSpec,
    profiles: &[Digest],
    agent: &str,
    endpoint: Option<&str>,
    worker: &str,
) {
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
    if let Some(endpoint) = endpoint {
        eprintln!("  model endpoint:       {endpoint}");
    }
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
    let (home, store, w) = open(home, id, true)?;
    // A cancel whose canceller died before it reached the running step is applied first, so
    // the step cannot run on to SUCCEEDED; a step still live then finishes its cancel below.
    reconcile_cancel(&store, &w)?;
    let (state, _) = state_of(&store, &w)?;
    let mut live = false;
    for task in w.steps.iter().filter_map(|s| s.task_id.as_ref()) {
        live |= !store.db.task(task)?.state.is_terminal();
    }
    if state != WorkflowState::Running && !live {
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
        reconcile_cancel(store, &w)?;
        let step = &w.steps[idx];
        let state = match &step.task_id {
            // One read of the journal answers both whether the task was submitted and with
            // which profile.
            Some(task) => match home.submitted_payload(store, task)? {
                None => {
                    let t = store.db.task(task)?;
                    if t.state.is_terminal() {
                        t.state
                    } else {
                        let reason = NOT_CREATED.to_string();
                        store.db.append(task, &TaskEvent::Failed { reason })?.state
                    }
                }
                Some(submitted) => {
                    // A step task runs only on the profile the chain approved.
                    let approved = approved_profile(&w, idx)?;
                    if !store.db.task(task)?.state.is_terminal()
                        && submitted["profile_digest"].as_str() != Some(&approved.to_string())
                    {
                        return Err(CliError::usage(format!(
                            "workflow {id}, step {:?}: task {task} was recorded with a verification profile that is not the one the workflow approved ({approved})",
                            step.name
                        )));
                    }
                    resume_task(home, store, Some(lock), task, None, crash).await?
                }
            },
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

/// A chain being cancelled has every live step task's cancel requested: the cancel may have
/// committed the chain's flag and then died before it reached the task.
fn reconcile_cancel(store: &Store, w: &WorkflowRecord) -> Result<(), CliError> {
    if !w.cancel_requested {
        return Ok(());
    }
    for task in w.steps.iter().filter_map(|s| s.task_id.as_ref()) {
        let t = store.db.task(task)?;
        if !t.state.is_terminal() && !t.cancel_requested {
            store.db.append(task, &TaskEvent::CancelRequested)?;
        }
    }
    Ok(())
}

/// The verification profile digest approved for step `idx`.
fn approved_profile(w: &WorkflowRecord, idx: usize) -> Result<Digest, CliError> {
    w.options["profile_digests"][idx]
        .as_str()
        .and_then(|d| Digest::from_hex(d).ok())
        .ok_or_else(|| {
            CliError::other(format!(
                "workflow {} has no approved profile digest for step {:?}",
                w.id, w.steps[idx].name
            ))
        })
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

/// Exits 137 when `env` is `1`; honoured only with `AGENTOS_TEST_WORKERS=1`.
fn test_exit(env: &str, what: &str) {
    if std::env::var(TEST_WORKERS_ENV).as_deref() == Ok("1")
        && std::env::var(env).as_deref() == Ok("1")
    {
        eprintln!("{}", json!({ what: true }));
        std::process::exit(137);
    }
}

type Started = (TaskId, RoutingExecutor<SupervisedExecutor>, Driver);

/// Removes a staged handoff whatever happens: the step's task holds its own recorded copy, and
/// a later attempt rebuilds it.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates step `idx`'s task from the approved spec, on the previous step's handoff for a
/// later step, and builds its executor and agent (before its approval, as `submit` does).
/// `None` when the chain was failed here (a handoff that does not rebuild the verified
/// workspace, changed or missing recorded inputs) or cancelled meanwhile. A changed
/// verification profile is refused (exit 2), and an I/O or database failure is an error (exit
/// 1); both leave the chain as it was.
async fn start_step(
    home: &Home,
    store: &Store,
    w: &WorkflowRecord,
    idx: usize,
) -> Result<Option<Started>, CliError> {
    let dir = home.workflow_dir(&w.id);
    let name = &w.spec.steps[idx].name;
    let mut contract = w.spec.steps[idx].contract.clone();
    let in_step = |e: CliError| CliError {
        code: e.code,
        message: format!("workflow {}, step {name:?}: {}", w.id, e.message),
    };
    let fail = |reason: String| -> Result<Option<Started>, CliError> {
        tracing::warn!(workflow_id = %w.id, %reason, "workflow failed");
        store.db.fail_workflow(&w.id, &reason)?;
        Ok(None)
    };
    let inputs = match step_inputs(&dir, w, idx)? {
        Ok(inputs) => inputs,
        Err(reason) => return fail(reason),
    };
    // A profile from the legacy `--profiles` directory is not pinned in the contract: it must
    // still be the one approved, checked here before any handoff is built and again on the
    // staged copy before the task exists.
    let approved = approved_profile(w, idx)?;
    if contract.profile_digest.is_none() {
        let id = &contract.verification_profile;
        if let Some(dir) = home.resolve_profile(id, None)? {
            let found = workspace_digest(&dir)
                .map_err(|e| CliError::usage(format!("cannot digest {}: {e}", dir.display())))?;
            if found != approved {
                return Err(in_step(CliError::usage(format!(
                    "verification profile {id} changed since the workflow was approved: approved {approved}, found {found}"
                ))));
            }
        }
    }
    let mut staged = None;
    if idx > 0 {
        let prev = w.steps[idx - 1].task_id.as_ref().ok_or_else(|| {
            CliError::other(format!("workflow {}: step {} has no task", w.id, idx - 1))
        })?;
        fs::create_dir_all(&dir)?;
        let handoff_dir = dir.join(format!("handoff-{idx}"));
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
            // A blob that is gone is a lost record, not a passing failure.
            Err(ExportError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                Err(HandoffError::Mismatch(format!("io: {e}")))
            }
            Err(e @ (ExportError::Db(_) | ExportError::Io(_))) => {
                Err(HandoffError::Io(e.to_string()))
            }
            Err(e) => Err(HandoffError::Mismatch(e.to_string())),
        };
        match built {
            Ok(digest) => {
                contract.repository.source = handoff_dir.display().to_string();
                contract.repository.revision = digest.to_string();
                staged = Some(Staged(handoff_dir));
            }
            // Only a handoff that does not rebuild the verified workspace ends the chain; a
            // failure of the machine on the way leaves it as it was, for `resume` to retry.
            Err(HandoffError::Mismatch(reason)) => {
                return fail(format!("handoff to step {name:?} failed: {reason}"));
            }
            Err(HandoffError::Io(reason)) => {
                return Err(in_step(CliError::other(format!(
                    "cannot build the handoff yet: {reason}; resume the workflow to try again"
                ))));
            }
        }
    }
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
    // The bytes just read are the ones recorded: they must be the approved ones.
    if inputs.patch.is_some()
        && req.patch_digest().map(|d| d.to_string()).as_deref()
            != w.options["step_patch_digests"][idx].as_str()
    {
        return fail(format!("recorded {} changed", step_patch(idx)));
    }
    if inputs.transcript
        && req.transcript_digest().map(|d| d.to_string()).as_deref()
            != w.options["transcript_digest"].as_str()
    {
        return fail(format!("recorded {TRANSCRIPT} changed"));
    }
    let rec = match record_step_task(home, store, req, &w.id, idx, approved).await? {
        Ok(rec) => rec,
        // A cancel that landed since the chain was read wins, and the step stays skipped; any
        // other refusal by the store is an error.
        Err(StepRefusal::Store(_)) if store.db.workflow(&w.id)?.cancel_requested => {
            return Ok(None);
        }
        Err(StepRefusal::Store(refusal)) => return Err(CliError::other(refusal)),
        Err(StepRefusal::Profile(refusal)) => return Err(in_step(CliError::usage(refusal))),
        // The handoff changed after it was verified: the chain cannot go on from it.
        Err(StepRefusal::Source(refusal)) if idx > 0 => {
            return fail(format!("handoff to step {name:?} failed: {refusal}"));
        }
        Err(StepRefusal::Source(refusal)) => return Err(in_step(CliError::usage(refusal))),
    };
    drop(staged);
    let task = rec.task.clone();
    let (exec, agent) = rec.runner(home, store)?;
    Ok(Some((task, exec, agent)))
}

/// What `validate_contract` needs for step `idx` beyond its contract, from the workflow's
/// recorded inputs.
struct Inputs {
    patch: Option<PathBuf>,
    model: Option<String>,
    /// The model replays the workflow's `fake:` transcript.
    transcript: bool,
    agent_cli: Option<AgentCli>,
}

/// The step's recorded inputs; `Ok(Err(reason))` when one is missing. `validate_contract`
/// reads each once, and those bytes are checked against the approved digests.
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
    let present = |file: &Path| file.is_file();
    let (patch, model, transcript) = match o["model"].as_str() {
        None => {
            let patch = dir.join(step_patch(idx));
            if !present(&patch) {
                return Ok(Err(format!("recorded {} is missing", step_patch(idx))));
            }
            (Some(patch), None, false)
        }
        Some(m) if m.starts_with("fake:") => {
            let transcript = dir.join(TRANSCRIPT);
            if !present(&transcript) {
                return Ok(Err(format!("recorded {TRANSCRIPT} is missing")));
            }
            (None, Some(format!("fake:{}", transcript.display())), true)
        }
        Some(m) => (None, Some(m.to_string()), false),
    };
    Ok(Ok(Inputs {
        patch,
        model,
        transcript,
        agent_cli,
    }))
}

/// Stops the chain: no further step starts, and a step task still running is cancelled as
/// `agentos cancel` cancels a task.
async fn cancel(home: Home, id: &WorkflowId) -> Result<(), CliError> {
    let (home, store, _) = open(home, id, false)?;
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
    let (_, store, w) = open(home, id, false)?;
    let (state, _) = state_of(&store, &w)?;
    match state {
        WorkflowState::Succeeded => {}
        WorkflowState::Running => {
            return Err(CliError::other(format!(
                "workflow {id} is {}; only a succeeded workflow can be exported",
                display_label(state, w.approved)
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
            "base_workspace_digest": m.base_workspace_digest,
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
    // `chain.json` is written with the bundle, so the export appears whole or not at all.
    let chain_json = serde_json::to_vec_pretty(&chain)?;
    export_task_with(&store, last, dir, &[("chain.json", &chain_json)])?;
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
        assert!(home.submitted_payload(&store, &task).unwrap().is_none());
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
