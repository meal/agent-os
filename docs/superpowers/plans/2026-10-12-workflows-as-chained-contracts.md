# Workflows as Chained Contracts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `agentos workflow ...` runs a pipeline of 2 to 8 task contracts on one workspace, approved once, where each step starts from the verified output of the step before it.

**Architecture:** A chain layer above tasks. A new `workflows` / `workflow_steps` pair of tables links ordinary tasks; core, the effect journal and the task state machine are unchanged. After step N succeeds the controller rebuilds N's final workspace from N's snapshot plus its exported patch, checks it against N's verified digest, and submits step N+1 on it. Chain state is derived from the steps' task states.

**Tech Stack:** Rust workspace (`agentos-core`, `agentos-store`, `agentos-engine`, `agentos-cli`), SQLite via rusqlite, clap, git (`git apply`), tests through `docker compose run --rm test ...`.

**Spec:** `docs/superpowers/specs/2026-10-12-workflows-as-chained-contracts-design.md`. Read it first.

## Global Constraints

- Steps: 2 to 8; step names unique, plain (`[a-z0-9-]`, 1..=40 chars, no leading `-`).
- Steps run strictly one at a time under the single driver lock (`Home::lock`).
- Steps 2..n must not set `repository` in the chain file. Step 1 takes the chain's top-level `repository`.
- `--model`, `--agent-cli` and `--worker` apply chain-wide. The fake agent takes one patch per step (`--step-patch`, repeated), a test hook.
- No retries, branching, gates, per-step agents or chain-wide deadline.
- A digest mismatch at a handoff fails the chain. No directory on disk is trusted: each handoff directory is re-digested by the ordinary submit path.
- Never ship or commit the agent CLI binary or request bodies of an agent CLI (see memory: evidence scan).
- Project rules (`AGENTS.md`): use docker compose; write tests and verify; **do not add Co-Authored-By lines to commits**; host `cargo` cannot build (root-owned `target/`), always `docker compose run --rm test cargo test -p <crate> ...`.
- Subagent rules: red/green with the failure shown; stop or kill only containers named `agent-os-test-run-*`; run `df -h /` before large builds (cargo volumes filled the disk once; delete only our `*_target` volumes).
- No new dependencies are planned. If one becomes necessary, check its latest version online first.

## Review Focus

Inputs and failure modes the spec implies but no obvious task covers; each has a test in the task that owns the code.

1. A chain file whose step 2 sets `repository`, whose names repeat, or with 1 or 9 steps is refused with exit 2 and records nothing (Task 1, Task 5).
2. A step whose patch is empty (final workspace equals base) hands off correctly (Task 3).
3. Step 1 fails: step 2 never gets a task, the chain is FAILED naming `implement` (Task 5).
4. The controller dies after the step task row exists but before its `Submitted` event: resume fails that task cleanly and the chain with it, no second task (Task 6).
5. `workflow resume` with a different `--worker` than recorded, or on a chain never approved, is refused (Task 5).
6. An existing home at schema version 2 still opens after the schema bump (Task 2).

---

### Task 1: Chain spec, validation and state derivation (core)

**Files:**
- Create: `crates/agentos-core/src/workflow.rs`
- Modify: `crates/agentos-core/src/lib.rs` (add `pub mod workflow;`)
- Modify: `crates/agentos-core/src/ids.rs` (add `WorkflowId`)

**Interfaces:**
- Produces:
  - `WorkflowId` (`new()`, `as_str()`, `Display`, serde, same shape as `TaskId`)
  - `ChainSpec { name: String, repository: RepoRef, steps: Vec<Step> }`, `Step { name: String, contract: Contract }`
  - `ChainSpec::parse(json: &str) -> Result<ChainSpec, WorkflowError>`
  - `ChainSpec::digest(&self) -> Digest`
  - `PREVIOUS_STEP_SOURCE: &str = "(previous step)"`
  - `StepStatus { Pending, Task(TaskState), Skipped }`, `WorkflowState { Running, Succeeded, Failed { step: usize }, Cancelled { step: usize } }`, `derive_state(&[StepStatus]) -> WorkflowState`
  - `WorkflowState::label(&self) -> &'static str` (`RUNNING`, `SUCCEEDED`, `FAILED`, `CANCELLED`)

- [ ] **Step 1: Write the failing tests** at the bottom of `workflow.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn contract(goal: &str) -> Value {
        json!({
            "goal": goal, "profile": "python-stdlib-v1", "editable_paths": ["src/**"],
            "verification_profile": "parser-checks-v1",
            "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
            "limits": { "model_requests": 3, "max_output_tokens_per_request": 100, "tool_actions": 4,
                        "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
        })
    }

    fn chain(steps: Vec<(&str, Value)>) -> String {
        json!({
            "name": "fix-and-review",
            "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
            "steps": steps.into_iter().map(|(n, c)| json!({ "name": n, "contract": c })).collect::<Vec<_>>()
        })
        .to_string()
    }

    #[test]
    fn a_two_step_chain_parses_and_later_steps_point_at_the_previous_one() {
        let spec = ChainSpec::parse(&chain(vec![
            ("implement", contract("fix it")),
            ("review", contract("review it")),
        ]))
        .unwrap();
        assert_eq!(spec.steps.len(), 2);
        assert_eq!(spec.steps[0].contract.repository.source, "/work/repo");
        assert_eq!(spec.steps[1].contract.repository.source, PREVIOUS_STEP_SOURCE);
        assert_eq!(spec.digest(), ChainSpec::parse(&chain(vec![
            ("implement", contract("fix it")),
            ("review", contract("review it")),
        ])).unwrap().digest());
    }

    #[test]
    fn the_digest_changes_with_any_step() {
        let a = ChainSpec::parse(&chain(vec![("a", contract("x")), ("b", contract("y"))])).unwrap();
        let b = ChainSpec::parse(&chain(vec![("a", contract("x")), ("b", contract("z"))])).unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn invalid_chains_are_refused() {
        let ok = |n| (n, contract("g"));
        let mut with_repo = contract("g");
        with_repo["repository"] = json!({ "source": "/x", "revision": "recorded-at-submission" });
        let nine: Vec<(String, Value)> = (0..9).map(|i| (format!("s{i}"), contract("g"))).collect();
        let nine: Vec<(&str, Value)> = nine.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
        for (what, text) in [
            ("one step", chain(vec![ok("a")])),
            ("nine steps", chain(nine)),
            ("duplicate names", chain(vec![ok("a"), ok("a")])),
            ("bad name", chain(vec![ok("a"), ("Not Plain", contract("g"))])),
            ("leading dash", chain(vec![ok("a"), ("-b", contract("g"))])),
            ("repository on step 2", chain(vec![ok("a"), ("b", with_repo)])),
            ("not json", "nope".to_string()),
        ] {
            assert!(ChainSpec::parse(&text).is_err(), "{what} must be refused");
        }
    }

    #[test]
    fn an_invalid_step_contract_names_the_step() {
        let mut bad = contract("g");
        bad["limits"]["tool_actions"] = json!(0);
        let e = ChainSpec::parse(&chain(vec![("a", contract("g")), ("b", bad)])).unwrap_err();
        assert!(e.to_string().contains("step \"b\""), "{e}");
    }

    #[test]
    fn state_is_derived_from_the_steps() {
        use StepStatus::*;
        use crate::state::TaskState as T;
        assert_eq!(derive_state(&[Pending, Pending]), WorkflowState::Running);
        assert_eq!(derive_state(&[Task(T::Running), Pending]), WorkflowState::Running);
        assert_eq!(derive_state(&[Task(T::Succeeded), Pending]), WorkflowState::Running);
        assert_eq!(derive_state(&[Task(T::Succeeded), Task(T::Succeeded)]), WorkflowState::Succeeded);
        assert_eq!(derive_state(&[Task(T::Failed), Pending]), WorkflowState::Failed { step: 0 });
        assert_eq!(derive_state(&[Task(T::Succeeded), Task(T::Cancelled)]), WorkflowState::Cancelled { step: 1 });
        // A cancel between steps skips the rest without any cancelled task.
        assert_eq!(derive_state(&[Task(T::Succeeded), Skipped]), WorkflowState::Cancelled { step: 1 });
        assert_eq!(WorkflowState::Failed { step: 0 }.label(), "FAILED");
    }
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `docker compose run --rm test cargo test -p agentos-core workflow`
Expected: FAIL to compile (`ChainSpec`, `derive_state` not defined).

- [ ] **Step 3: Implement**

```rust
//! A workflow: a chain of task contracts run one after the other on one workspace. This
//! module is the pure part: parsing and validating the chain file, its digest, and the
//! chain's state derived from its steps' task states.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::contract::{Contract, RepoRef};
use crate::ids::Digest;
use crate::state::TaskState;

pub const MIN_STEPS: usize = 2;
pub const MAX_STEPS: usize = 8;
/// The `repository.source` of every step after the first: the real input is the previous step's
/// verified result, supplied when the step starts.
pub const PREVIOUS_STEP_SOURCE: &str = "(previous step)";
const RECORDED: &str = "recorded-at-submission";

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid chain: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChainFile {
    name: String,
    repository: RepoRef,
    steps: Vec<StepFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    name: String,
    contract: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub name: String,
    pub contract: Contract,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainSpec {
    pub name: String,
    pub repository: RepoRef,
    pub steps: Vec<Step>,
}

fn check_name(what: &str, v: &str) -> Result<(), WorkflowError> {
    let ok = (1..=40).contains(&v.len())
        && !v.starts_with('-')
        && v.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(WorkflowError::Invalid(format!(
            "{what} {v:?} must be 1..=40 characters of [a-z0-9-] and not start with '-'"
        )))
    }
}

impl ChainSpec {
    pub fn parse(json: &str) -> Result<ChainSpec, WorkflowError> {
        let file: ChainFile =
            serde_json::from_str(json).map_err(|e| WorkflowError::Invalid(e.to_string()))?;
        check_name("chain name", &file.name)?;
        if !(MIN_STEPS..=MAX_STEPS).contains(&file.steps.len()) {
            return Err(WorkflowError::Invalid(format!(
                "a chain has {MIN_STEPS} to {MAX_STEPS} steps, got {}",
                file.steps.len()
            )));
        }
        let mut steps: Vec<Step> = Vec::new();
        for (i, s) in file.steps.into_iter().enumerate() {
            check_name("step name", &s.name)?;
            if steps.iter().any(|p| p.name == s.name) {
                return Err(WorkflowError::Invalid(format!("duplicate step name {:?}", s.name)));
            }
            let mut value = s.contract;
            let obj = value.as_object_mut().ok_or_else(|| {
                WorkflowError::Invalid(format!("step {:?}: contract must be an object", s.name))
            })?;
            if i > 0 && obj.contains_key("repository") {
                return Err(WorkflowError::Invalid(format!(
                    "step {:?}: only the first step sets a repository; this one starts from the previous step's result",
                    s.name
                )));
            }
            let repo = if i == 0 {
                serde_json::to_value(&file.repository).expect("serializes")
            } else {
                serde_json::json!({ "source": PREVIOUS_STEP_SOURCE, "revision": RECORDED })
            };
            obj.insert("repository".into(), repo);
            let contract = Contract::parse(&value.to_string())
                .map_err(|e| WorkflowError::Invalid(format!("step {:?}: {e}", s.name)))?;
            steps.push(Step { name: s.name, contract });
        }
        Ok(ChainSpec { name: file.name, repository: file.repository, steps })
    }

    /// What the owner approves: the digest of the parsed chain, every step's contract included.
    pub fn digest(&self) -> Digest {
        Digest::of(&serde_json::to_vec(self).expect("a chain serializes"))
    }
}

/// What the chain knows about one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// No task yet.
    Pending,
    Task(TaskState),
    /// Never started because the chain was cancelled.
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowState {
    Running,
    Succeeded,
    Failed { step: usize },
    Cancelled { step: usize },
}

impl WorkflowState {
    pub fn label(&self) -> &'static str {
        match self {
            WorkflowState::Running => "RUNNING",
            WorkflowState::Succeeded => "SUCCEEDED",
            WorkflowState::Failed { .. } => "FAILED",
            WorkflowState::Cancelled { .. } => "CANCELLED",
        }
    }
}

/// The first failed step fails the chain, then the first cancelled or skipped one cancels it;
/// all steps succeeded is success; anything else is still running.
pub fn derive_state(steps: &[StepStatus]) -> WorkflowState {
    let first = |want: &dyn Fn(&StepStatus) -> bool| steps.iter().position(want);
    if let Some(step) = first(&|s| *s == StepStatus::Task(TaskState::Failed)) {
        return WorkflowState::Failed { step };
    }
    if let Some(step) =
        first(&|s| matches!(s, StepStatus::Task(TaskState::Cancelled) | StepStatus::Skipped))
    {
        return WorkflowState::Cancelled { step };
    }
    if !steps.is_empty() && steps.iter().all(|s| *s == StepStatus::Task(TaskState::Succeeded)) {
        return WorkflowState::Succeeded;
    }
    WorkflowState::Running
}
```

Add `WorkflowId` to `ids.rs` as a copy of `TaskId` (`WorkflowId(String)` with `new()` from `Uuid::now_v7()`, `as_str`, `Default`, `Display`), plus a test like `task_id_is_unique_uuid_v7`. Add `pub mod workflow;` to `lib.rs`.

- [ ] **Step 4: Run to verify they pass**

Run: `docker compose run --rm test cargo test -p agentos-core`
Expected: PASS (all of agentos-core).

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-core
git commit -m "feat: chain spec, validation and derived state for workflows"
```

---

### Task 2: Workflow tables and store API

**Files:**
- Create: `crates/agentos-store/src/workflows.rs` (`impl Db`, follow the style of `caps.rs` / `effects.rs`)
- Modify: `crates/agentos-store/src/lib.rs` (add `pub mod workflows;`)
- Modify: `crates/agentos-store/src/db.rs` (`SCHEMA_VERSION` 2 to 3, the DDL, a 2-to-3 migration, one `DbError` variant, factor the INSERT of `create_task` into a helper both callers use)

**Interfaces:**
- Consumes: `ChainSpec`, `WorkflowId` (Task 1); `Db::immediate`, `now_ts`, `load_task` (existing, `pub(crate)`).
- Produces (all on `Db`):
  - `create_workflow(&self, spec: &ChainSpec, options: &serde_json::Value) -> Result<WorkflowId>`: inserts the workflow and one `workflow_steps` row per step in one transaction.
  - `approve_workflow(&self, id: &WorkflowId) -> Result<()>`: sets `approved_ts` once; idempotent.
  - `workflow(&self, id: &WorkflowId) -> Result<WorkflowRecord>`
  - `create_step_task(&self, id: &WorkflowId, idx: usize, contract: &Contract, contract_digest: &Digest) -> Result<TaskId>`
  - `request_workflow_cancel(&self, id: &WorkflowId) -> Result<()>`
  - `pub struct WorkflowRecord { pub id: WorkflowId, pub spec: ChainSpec, pub spec_digest: Digest, pub options: Value, pub approved: bool, pub cancel_requested: bool, pub steps: Vec<StepRow> }`, `pub struct StepRow { pub idx: usize, pub name: String, pub task_id: Option<TaskId>, pub skipped: bool }`
  - `DbError::Workflow(String)` for the refused cases below.

DDL (add to `SCHEMA`, and run on its own for a version-2 database):

```sql
CREATE TABLE IF NOT EXISTS workflows(
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    spec_digest TEXT NOT NULL,
    spec_json TEXT NOT NULL,
    options_json TEXT NOT NULL,
    approved_ts INTEGER,
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    created_ts INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workflow_steps(
    workflow_id TEXT NOT NULL REFERENCES workflows(id),
    idx INTEGER NOT NULL,
    name TEXT NOT NULL,
    task_id TEXT UNIQUE REFERENCES tasks(id),
    skipped INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(workflow_id, idx)
);
```

Rules `create_step_task` enforces in its one `immediate()` transaction (each a `DbError::Workflow` otherwise): the workflow is approved and not cancel-requested; the step has no task and is not skipped; for `idx > 0` the previous step's task exists and is `SUCCEEDED`. It then inserts the task exactly as `create_task` does and sets `task_id`.

`request_workflow_cancel` sets `cancel_requested = 1` and marks every step without a task as `skipped = 1`, in one transaction.

- [ ] **Step 1: Write the failing tests** (in `workflows.rs`, using the existing `Db::open(tempdir)` pattern from the store's tests)

Cover: `create_workflow` then `workflow` round-trips the spec, digest and options and returns n pending steps; `create_step_task` before `approve_workflow` is refused; step 1 before step 0 succeeded is refused; step 0 twice is refused (the second call returns an error and the task count stays 1); after `append(Started)`/`Failed` on the task, `create_step_task(1)` is refused; `request_workflow_cancel` skips only task-less steps and later `create_step_task` is refused. A real success path needs a task that reaches SUCCEEDED: copy the helper the existing store tests use to drive a task to `Succeeded`, or reduce a task through `TaskEvent`s directly in SQL with `UPDATE tasks SET state='Succeeded'` using the same string `state_to_str` produces (use `state_to_str(TaskState::Succeeded)` in the test, not a literal).

Migration test: build a file with the version-2 schema (open with a `Db` from the previous `SCHEMA` text copied into the test, set `user_version` 2), then `Db::open` it and assert it opens at version 3, keeps its task row, and `create_workflow` works.

- [ ] **Step 2: Run them to see them fail**

Run: `docker compose run --rm test cargo test -p agentos-store workflows`
Expected: FAIL (module missing).

- [ ] **Step 3: Implement** the DDL, the migration, and the methods.

Migration shape in `Db::open`, inside the existing write transaction, replacing the `version < SCHEMA_VERSION` branch:

```rust
if version < SCHEMA_VERSION {
    let has_tasks: bool = /* unchanged query */;
    if has_tasks && version == 2 {
        // Version 3 only adds the workflow tables.
        tx.execute_batch(WORKFLOW_SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    } else if has_tasks {
        return Err(DbError::OldSchema { found: version, supported: SCHEMA_VERSION });
    } else {
        tx.execute_batch(SCHEMA)?; // SCHEMA includes WORKFLOW_SCHEMA
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
}
```

Define `WORKFLOW_SCHEMA` as its own `const` and splice it into `SCHEMA` with `concat!`. Update the existing doc comment on `SCHEMA_VERSION` and any existing test that asserts version 2.

- [ ] **Step 4: Run the store tests**

Run: `docker compose run --rm test cargo test -p agentos-store`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-store
git commit -m "feat: workflow tables and store API, schema version 3"
```

---

### Task 3: Handoff — the verified final workspace of a task

**Files:**
- Modify: `crates/agentos-engine/src/export.rs` (add `Handoff` and `handoff`)
- Create: `crates/agentos-engine/src/handoff.rs` (`materialize`)
- Modify: `crates/agentos-engine/src/lib.rs` (`pub mod handoff;`)

**Interfaces:**
- Produces:
  - `export::Handoff { pub base: Digest, pub final_digest: Digest, pub patch: Vec<u8> }`
  - `export::handoff(db: &Db, blobs: &BlobStore, task: &TaskId) -> Result<Handoff>`: runs the same `collect` as `export_bundle` (so every digest is re-verified from bytes), requires state `SUCCEEDED`, takes `base_workspace_digest` and `final_workspace_digest` from the manifest, `patch` from `patch_diff`. Journals nothing. Errors with `ExportError` (add a variant `NotSucceeded(TaskState)` or reuse `NotTerminal` for non-terminal and a new one for failed/cancelled).
  - `handoff::materialize(snapshot: &Path, h: &Handoff, out: &Path) -> Result<(), String>` (async): `out` must not exist; copies `snapshot` to `out`; checks `workspace_digest(out) == h.base` ("handoff base mismatch: ..."); if `h.patch` is non-empty writes it to a scratch directory **outside** `out`, purges excluded components (as `fixture.rs` does before `git apply`), runs `git apply --check` then `git apply` through `patch::git(out)`; checks `workspace_digest(out) == h.final_digest` ("handoff result mismatch: ..."). On any error removes `out`.

- [ ] **Step 1: Write the failing tests**

In `handoff.rs`: build a small snapshot dir (`src/a.py` with two lines) and a unified diff changing one line; compute the expected final digest by applying the diff yourself in a second copy with `git apply` (use the same helper the engine tests use to run git) or by writing the expected file and calling `workspace_digest`. Tests:

1. `materialize` produces a directory whose digest equals `final_digest`; the source snapshot is unchanged.
2. An empty patch gives a copy equal to the base (`final_digest == base`).
3. A wrong `final_digest` returns an error containing `handoff result mismatch` and leaves no `out`.
4. A snapshot that differs from `h.base` returns an error containing `handoff base mismatch`.
5. A patch that does not apply returns an error and leaves no `out`.
6. `out` already existing is refused and left untouched.

In `export.rs` tests: an end-to-end `handoff` for a task driven by the existing test helpers in that file (find the helper that builds a SUCCEEDED task for the `export_bundle` tests and reuse it): its `patch` equals the bundle's `patch.diff`, its `final_digest` equals `manifest.final_workspace_digest`; a FAILED task is refused.

- [ ] **Step 2: Run to see failure**

Run: `docker compose run --rm test cargo test -p agentos-engine handoff`
Expected: FAIL (not defined).

- [ ] **Step 3: Implement** as specified. Reuse `copy_tree`, `workspace_digest` and the purge helper from wherever `fixture.rs` imports them. Do not weaken any check `fixture.rs` makes before `git apply`.

- [ ] **Step 4: Run the engine tests**

Run: `docker compose run --rm test cargo test -p agentos-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/agentos-engine
git commit -m "feat: rebuild and verify a task's final workspace for a handoff"
```

---

### Task 4: Make `submit` reusable without changing it

**Files:**
- Modify: `crates/agentos-cli/src/commands/submit.rs`

**Interfaces:**
- Produces (all `pub(crate)` in `submit.rs`):
  - `validate_contract(home: &Home, contract: Contract, yes: bool, patch: Option<&Path>, model: Option<&str>, agent_cli: Option<AgentCli>) -> Result<Request, CliError>`: everything `validate` does after reading and parsing the file, including the `--agent-cli` checks.
  - `validate(...)` becomes: `check_agent_cli_flags` when needed, read and parse the file, then `validate_contract`.
  - `async fn record_task(home: &Home, store: &Store, req: Request) -> Result<Recorded, CliError>`: from the staging copy through `append_audit(.., "Submitted", ..)`; returns `Recorded { task: TaskId, contract: Contract, repo_digest: Digest, profile_digest: Digest, agent: String, submitted: Value }`. It does not print, approve or drive.
  - `Request` and `Recorded` are `pub(crate)`.
  - `summarize` takes what it needs from `Recorded` and `Request`, unchanged output.

This task changes no behavior. The existing CLI tests are the safety net.

- [ ] **Step 1: Run the CLI submit tests first to record a baseline**

Run: `docker compose run --rm test cargo test -p agentos-cli --test cli submit`
Expected: PASS (note the count).

- [ ] **Step 2: Refactor** by moving code, not rewriting it: `submit()` becomes `validate` → lock → `record_task` → `summarize` → approve and drive. Keep the order of the lock, the executor construction, `approve_task` and `drive` exactly as in the current `submit()` (the executor is built before the approval on purpose; see the comment there).

- [ ] **Step 3: Re-run**

Run: `docker compose run --rm test cargo test -p agentos-cli`
Expected: PASS with the same count as Step 1 for submit tests; the whole crate green; `docker compose run --rm test cargo clippy -p agentos-cli --all-targets -- -D warnings` clean.

- [ ] **Step 4: Commit**

```bash
git add crates/agentos-cli
git commit -m "refactor: submit exposes validate_contract and record_task for workflows"
```

---

### Task 5: `agentos workflow` — submit, run, status, cancel, export

**Files:**
- Create: `crates/agentos-cli/src/commands/workflow.rs`
- Modify: `crates/agentos-cli/src/args.rs` (a `Workflow { command: WorkflowCommand }` subcommand), `crates/agentos-cli/src/commands/mod.rs` (dispatch), `crates/agentos-cli/src/home.rs` (`workflows_dir()`, `workflow_dir(&WorkflowId)`)
- Test: `crates/agentos-cli/tests/cli.rs` (new section; reuse `Cli`, `fixtures()`, `fix_patch()`, `repo_copy()`, `submit_yes` helpers and their fake-guest setup)

**Interfaces:**
- Consumes: Tasks 1-4.
- Produces CLI:

```
agentos workflow submit <chain.json> [--yes] [--step-patch FILE]... [--model SPEC] [--agent-cli NAME] [--crash-at ...]
agentos workflow resume <id>
agentos workflow status <id>
agentos workflow cancel <id>
agentos workflow export <id> <dir>
```

Behavior:

- **submit**: refuses (exit 2, nothing recorded) a chain that fails `ChainSpec::parse`; step 1's contract with `validate_contract` in full (its repository exists, its profile and image resolve, the worker checks pass); steps 2..n only through `ChainSpec::parse` (their input does not exist yet) plus `check_agent_cli_contract` when `--agent-cli` is given; and the flag combinations `submit` already refuses (reuse `check_agent_cli_flags`). A fake agent needs exactly one `--step-patch` per step and rejects `--model`. Records the workflow (`create_workflow`) with `options` = `{ "worker": ..., "model": ..., "agent_cli": ... }`, copies each step patch to `<home>/workflows/<id>/step-<idx>.patch` and the `fake:` transcript (if any) to `<home>/workflows/<id>/transcript.json`, prints the approval screen on stderr (see below). Without `--yes` it prints `{"workflow_id", "state": "READY", "note": "not started; run agentos workflow resume <id>"}`. With `--yes` it approves and runs.
- **Approval screen**: one block per step (name, goal, capabilities, editable paths, verification profile, limits, same wording as `summarize`), then the agent, the worker, and a `total budget` line summing `model_requests`, `tool_actions` and `deadline_seconds` over the steps.
- **resume**: refuses a workflow whose recorded worker differs from `--worker`; calls `approve_workflow` (idempotent) then runs the driver loop below.
- **Driver loop** (`run_workflow(home, id) -> Result<WorkflowState>`; holds `home.lock()` for the whole run):
  1. Load the record. For each step in order:
     - if the step has a task: if it is not terminal, drive it exactly like `control::resume` does for a task (reuse that function's body: factor it into `pub(crate) async fn resume_task(home, task, patch, crash)` in `control.rs`, with no behavior change); if the task has no `Submitted` event, fail it with `TaskEvent::Failed { reason: "step was not completely created" }`.
     - if it is `SUCCEEDED`, continue to the next step. If it is FAILED or CANCELLED, stop.
     - if it has no task and the previous step succeeded: build the handoff (below), `validate_contract` with the staged directory as the contract's `repository.source` and the handoff digest as its `revision`, `create_step_task`, then the `Submitted` record, `approve_task`, executor, agent and `drive`.
  2. Honor `cancel_requested` before creating each task: if set, do not create it.
  3. Return `derive_state` over the steps' statuses.
- **Handoff for step i > 0**: `handoff(db, blobs, prev_task)` (Task 3), `materialize(<home>/tasks/<prev>/snapshot, &h, <home>/workflows/<id>/handoff-<i>)`, remove any directory left by an earlier attempt first. A failure of either fails the chain: append `Failed` is not possible (no task yet), so record a `workflow_steps` skip for the remaining steps with `request_workflow_cancel`-style marking via a new `Db::fail_workflow(id, reason)` that marks task-less steps skipped and stores `reason` in the workflow's options; `status` shows it as `FAILED`. Add `fail_workflow` to the store in this task with a store test.
- **Test hook**: if `AGENTOS_TEST_WORKFLOW_EXIT_AFTER_STEP=<n>` is set, the driver calls `std::process::exit(137)` right after step `n` (1-based) is SUCCEEDED and before the next task is created.
- **status**: JSON `{ "workflow_id", "name", "state", "failed_step"?, "steps": [{ "idx", "name", "task_id"?, "state" }] }` where a step's `state` is the task's label, `PENDING` or `SKIPPED`.
- **cancel**: `request_workflow_cancel`, then if a step task is running, the same code as `control::cancel` for it (factor `cancel_task` like `resume_task`); prints `status`.
- **export**: requires a terminal workflow; exports the last SUCCEEDED step's bundle into `<dir>` via `export_bundle` (the `artifact.export` check as in `export.rs`) and writes `<dir>/chain.json`:

```json
{ "workflow_id": "...", "name": "...", "spec_digest": "...", "state": "SUCCEEDED",
  "steps": [ { "idx": 0, "name": "implement", "task_id": "...", "contract_digest": "...",
               "patch_digest": "...", "verified_digest": "..." } ] }
```
  taking each digest from that step's manifest (export each step's manifest into a scratch directory, or add an engine function returning the `Manifest` without writing; prefer the latter if `collect` can expose it). A non-succeeded workflow exports nothing and exits 1 with the failing step named.

- [ ] **Step 1: Write the failing end-to-end tests** (fake guest, as the neighbouring tests in `cli.rs`)

Fixtures: a repo copy where step 1's patch (`fix_patch()`) fixes the parser and step 2's patch is a second, small patch against the *fixed* file (add a comment line via `COMMENT_PATCH`-style diff; create it so it applies on top of the fix). Helper `cli.chain(repo, &[("implement", ..), ("review", ..)]) -> PathBuf` writes the chain file using `cli.contract(&repo)` as the template contract (strip its `repository` for step 2).

1. `a_two_step_chain_runs_and_hands_the_verified_workspace_to_the_second_step`: `workflow submit chain.json --yes --step-patch fix.patch --step-patch comment.patch` → state `SUCCEEDED`; `status` lists two SUCCEEDED tasks; task 2's `Submitted` event has `repository_digest` equal to task 1's `final_workspace_digest` (from its export manifest); two distinct task ids.
2. `workflow export` writes `chain.json` whose step digests match each task's own `export`, and `patch.diff` of the last step.
3. `a_failing_first_step_stops_the_chain`: step 1's patch does not apply → workflow `FAILED`, `failed_step` names `implement`, and `status` shows step 2 `SKIPPED` with no task id; export exits 1.
4. `workflow_submit_without_yes_issues_no_task_and_resume_approves`: `READY`, `status` shows PENDING steps with no tasks; `resume` runs it to SUCCEEDED.
5. `invalid_chains_exit_2_and_record_nothing`: one step, nine steps, duplicate names, `repository` on step 2, wrong number of `--step-patch`; each exits 2, and the home has no workflow (`assert_nothing_recorded`-style: no `workflows` row; add a small helper reading `workflow status` of nothing, or inspect the home with the store).
6. `workflow_resume_refuses_a_different_worker_and_an_unknown_id`.
7. `cancel_before_the_chain_runs_skips_every_step`: `submit` without `--yes`, `cancel` → `CANCELLED`, all steps `SKIPPED`, `resume` afterwards runs nothing and exits 1.
8. `each_step_holds_its_own_capability_handles`: after a SUCCEEDED chain, `status` of the two tasks shows distinct capability `handle_prefix`es, and `revoke <task1> artifact.export` leaves task 2's export allowed.

- [ ] **Step 2: Run to see failure**

Run: `docker compose run --rm test cargo test -p agentos-cli --test cli workflow`
Expected: FAIL (`unrecognized subcommand 'workflow'`).

- [ ] **Step 3: Implement** in the order: `home.rs` helpers, `Db::fail_workflow` (+ store test), `control::resume_task` / `cancel_task` extraction (run the existing control tests after), `args.rs`, `workflow.rs`, dispatch. Show the first failing assertion turning green after each piece.

- [ ] **Step 4: Run the whole CLI crate and clippy**

Run: `docker compose run --rm test cargo test -p agentos-cli && docker compose run --rm test cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, clean. Also run `cargo fmt --check`.

- [ ] **Step 5: Commit**

```bash
git add crates
git commit -m "feat: agentos workflow runs chained contracts on one workspace"
```

---

### Task 6: Crash, resume and `gc` tests

**Files:**
- Modify: `crates/agentos-cli/tests/cli.rs`, `crates/agentos-cli/src/commands/workflow.rs` (only if a test finds a defect)

**Interfaces:** Consumes Task 5. Uses the existing `--crash-at POINT[:KIND][:N]` flag (passed through `workflow submit`) and the `AGENTOS_TEST_WORKFLOW_EXIT_AFTER_STEP` hook.

- [ ] **Step 1: Write the tests**

1. `a_crash_mid_first_step_resumes_to_one_task_per_step`: `workflow submit --yes --crash-at after-dispatch:apply_patch ...` (the point the neighbouring crash tests use); then `workflow resume <id>` → SUCCEEDED; `status` shows exactly two task ids; each task's event types contain no repeated `TaskCreated`.
2. `a_crash_between_the_steps_resumes_without_rerunning_step_one`: run with `AGENTOS_TEST_WORKFLOW_EXIT_AFTER_STEP=1`; the process dies with 137, `status` shows step 1 SUCCEEDED and step 2 PENDING with no task; `resume` creates the second task, ends SUCCEEDED; step 1's task still has exactly one `VerifyPassed` event.
3. `a_crash_mid_second_step_resumes_the_same_task`: crash during step 2 (use `--crash-at after-dispatch:apply_patch:2`, the second pass of that point); `resume` finishes it; still two tasks.
4. `a_step_task_without_a_submitted_event_fails_the_chain_cleanly`: in a unit test inside `workflow.rs` (pattern: `drive.rs`'s `submitted()` helper), create an approved workflow, call `create_step_task` for step 0 and nothing else, then `run_workflow`; assert the task is FAILED with reason `step was not completely created`, the chain `FAILED`, and no second task.
5. `gc_between_the_steps_does_not_break_the_handoff`: exit after step 1 (hook), run `agentos gc` (it removes step 1's workspace image if eligible; assert it removed something, or assert the image dir is absent), then `resume` → SUCCEEDED.

- [ ] **Step 2: Run them**

Run: `docker compose run --rm test cargo test -p agentos-cli --test cli workflow`
Expected: some may FAIL first; fix the defect in `workflow.rs` (not the test), re-run to PASS. Report which failed before the fix.

- [ ] **Step 3: Full verification**

Run: `docker compose run --rm test cargo test --workspace && docker compose run --rm test cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, clean. Run the CLI suite 3 times: `for i in 1 2 3; do docker compose run --rm test cargo test -p agentos-cli --test cli workflow; done`; report any flake (memory: one unexplained flaky timing test exists in `supervised.rs`; do not mask others).

- [ ] **Step 4: Commit**

```bash
git add crates
git commit -m "test: workflows survive a killed controller and a collected workspace"
```

---

### Task 7: Documentation, release notes draft, Codex review

**Files:**
- Modify: `README.md` (a "Workflows" section after the agent CLI section: chain file, approval, handoff, resume, cancel, export, limits of v1; `agentos workflow` in the command list), `docs/index.html` if it lists commands, `docs/superpowers/plans/2026-10-12-workflows-as-chained-contracts.md` (tick tasks and add commit ids)
- Modify: `/home/meal/.claude/projects/-home-meal-code-agent-os/memory/project-guest-agent-runner.md` and `MEMORY.md` (state: workflows done, unpushed or pushed; next: read-only cockpit)

- [ ] **Step 1: Write the README section.** Include a working example chain file and the output of a real fake-guest run pasted from the tests' fixtures (run it, do not invent output). State the v1 non-goals from the spec.

- [ ] **Step 2: Run the docs-related tests** (the repo has tests that scan docs and evidence, such as `tests/evidence.rs`): `docker compose run --rm test cargo test --workspace`. Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add README.md docs
git commit -m "docs: workflows as chained contracts"
```

- [ ] **Step 4: Codex review of the whole branch (the owner asked for this at the end).** Dispatch the `codex:codex-rescue` agent with a read-only brief: "Review `git diff main...HEAD` in /home/meal/code/agent-os (design: docs/superpowers/specs/2026-10-12-workflows-as-chained-contracts-design.md). Do not edit files. Look for: a step run twice or skipped across a crash, a handoff that trusts an unverified directory, capability handles usable by another step, a way for a failed or cancelled step to let the chain continue, schema-version-2 homes broken by the bump, and tests that cannot fail. Report findings with file:line and a concrete failure scenario." Read each finding against the code; fix real ones with a failing test first; list the ones you rejected and why.

- [ ] **Step 5: Re-verify and commit fixes**

Run: `docker compose run --rm test cargo test --workspace && docker compose run --rm test cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS, clean.

```bash
git add -A
git commit -m "fix: address Codex review of workflows"
```

- [ ] **Step 6: Update the memory files and report.** Do not push or tag: the owner decides (and the live run through the real VM is still pending, as for `--agent-cli`).
