# Phase 3a: Capability Broker, Per-Job Supervisor and Leases — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the contract-list capability check with an opaque-handle broker (with revocation), run every effect under a detached per-job supervisor that enforces a bounded lease and the task deadline and keeps the durable receipt, and add a pinned profile registry — all with host-process workers (no VM).

**Architecture:** `agentos-core` gets the pure rules (handles, scopes, `authorize`, lease math). `agentos-store` persists handles and journals every decision. `agentos-engine` gains the job-directory protocol, a `Worker` trait with a host-process worker, the supervisor logic, and a `SupervisedExecutor` that implements the existing `Executor` trait, so `runner`, `steps` and `recover` keep their shape. A new `agentos-supervisor` binary is launched detached (own session) once per effect and owns the worker process group. The Phase 2 `DurableExecutor` is removed and its crash tests migrated.

**Tech Stack:** Rust 1.98.1 (pinned), Tokio 1.53, rusqlite 0.40 (bundled), rustix 1.1.5 (`process` feature, no `unsafe`), getrandom 0.4.3 (handle ids), assert_cmd 2.2.2 (dev), existing workspace crates. Versions checked with `cargo search` on 2026-10-02; re-check before pinning. All builds and tests run through `docker compose`.

**Spec:** `docs/superpowers/specs/2026-10-02-phase-3a-supervisor-broker-design.md` (parent: `Agent_OS_v1_Build_Plan.md`, Phase 3). Read the spec first; this plan implements it task by task.

## Global Constraints

- Supervisor is a per-job process, not a daemon: launched detached (setsid) by the controller per effect; owns the worker; enforces the lease and deadline; writes `status.json` and the durable receipt; exits.
- Job directory is `<home>/jobs/<effect_id>-<attempt_id>/` with `request.json`, `status.json`, `receipt.json`, `output.bin`, and a `cancel` marker. Receipt durability rule: temp file, fsync, rename, fsync of the directory, written BEFORE `status.json` becomes terminal.
- Lease expiry = `now + min(effect_timeout, task_deadline − now)`; `effect_timeout` is 60 s for verification and 30 s for other effects. No heartbeat. The supervisor SIGKILLs the worker's whole process group (rustix, no `unsafe`) on lease expiry, deadline, a `cancel` marker, and after a normal worker exit.
- A new attempt gets a strictly higher lease generation. Before redispatch the controller fences the old attempt: drop `cancel`, wait up to 10 s for a terminal `status.json`; if the supervisor is gone and its lease expiry has passed the attempt is dead; otherwise mark UNKNOWN and fail the task as `unreconcilable effect`.
- Handles are opaque 128-bit random ids (32 lowercase hex), stored in the existing `capabilities` table (new `scope` JSON text and `created_ts` columns); issued at owner approval (`submit --yes`, or `resume` on a READY task); scope: `snapshot.read` and `workspace.apply_patch` = the contract's `editable_paths`, `verification.run` = the pinned profile id, `artifact.export` = the task; expiry = `deadline_ts`. The agent never sees handles. Every authorize decision (granted or denied) is journaled; the handle id appears in the journal, nothing secret-bearing leaves the store.
- `PRAGMA user_version` becomes 2; a version 1 database is refused with a clear error (pre-release, no migration).
- Deadline passed: after reconciling in-flight effects append `Failed{reason: "deadline exceeded"}` (existing event, no reducer change).
- `agentos revoke TASK_ID [--capability NAME]` revokes; new requests fail at the next `authorize`; a running job for that capability gets the `cancel` marker; existing results stay auditable.
- `agentos export` is authorized through the broker (`artifact.export`) and journals the decision; it stays an `Exported` audit row, not an effect.
- Profile registry: `agentos profile register DIR` copies to `<home>/profiles/<id>@<digest>/` read-only; contracts may carry an optional `profile_digest`; the worker re-digests its staged copy against the pinned digest before every verification and voids evidence on mismatch.
- Non-goals: Firecracker, guest images, network/host-secret isolation, per-worker CPU/memory limits (all 3b); the model broker; making export a full effect; heartbeat leases; a supervisor daemon.
- Commits carry NO Co-Authored-By trailer (user's CLAUDE.md). Use docker compose. Write tests and verify. No network request may carry the user's email or any identity (an earlier implementer leaked the email in a crates.io User-Agent header; use `cargo search` only).

## Review Focus

- A worker that forks a background grandchild which would outlive the lease: the whole process group must die (including a verification check's own children), and no file the grandchild would write after the kill may appear.
- A controller killed (real exit 75 and SIGKILL) while a job is running: the job finishes or is killed on its own; `resume` publishes the receipt without re-running, never leaves a live orphan, and never runs two attempts of the same effect at once.
- A torn `status.json`, a stale `Running` status after a supervisor crash, a receipt file written but status not yet terminal, and a leftover `.tmp`: each must resolve to one deterministic decision.
- A handle that is expired, revoked, belongs to another task, or is out of scope: denied with the specific reason, journaled once, no effect created, and no handle value in logs or exports.
- A deadline that passes between agent turns, during an effect, and during recovery: the task ends FAILED `deadline exceeded` with no worker still running and no stranded `Reserved` usage.
- A tampered or swapped registered profile (bytes changed after registration, or a different digest under the same id): evidence must never be `passed=true`.

## File Structure

```
crates/agentos-core/src/
  broker.rs        Handle, Scope, Resource, Denial, CapabilityGrant, authorize()
  lease.rs         EffectTimeouts, lease_expiry()
  contract.rs      + optional profile_digest; path_matches() extracted
crates/agentos-store/src/
  caps.rs          issue/authorize/revoke persistence, schema v2 helpers
  db.rs            schema v2, user_version 2, reserved names
crates/agentos-engine/src/
  job.rs           JobDir, JobRequest, JobStatus, atomic IO, fencing helpers
  worker.rs        Worker trait, HostProcessWorker, ScriptedWorker (test-guarded), run_worker()
  supervisor.rs    run_supervisor(): lease/deadline/cancel enforcement, receipts
  supervised.rs    SupervisedExecutor (implements Executor)
  process.rs       + own_group flag
  durable.rs       REMOVED (ExecCounts moves to supervised.rs)
crates/agentos-supervisor/      binary: `agentos-supervisor run|worker <job_dir>`
crates/agentos-cli/src/
  commands/revoke.rs, commands/profile.rs   new commands
  home.rs, drive.rs, commands/{submit,control,export}.rs   wiring
README.md
```

---

### Task 1: Broker rules and lease math (pure, `agentos-core`)

**Files:**
- Create: `crates/agentos-core/src/broker.rs`, `crates/agentos-core/src/lease.rs`
- Modify: `crates/agentos-core/src/contract.rs` (extract `pub fn path_matches(patterns: &[String], rel: &str) -> bool`, reused by `Contract::path_allowed`; add optional `profile_digest: Option<String>` field, validated as 64 lowercase hex when present), `crates/agentos-core/src/lib.rs`
- Test: inline `#[cfg(test)]` modules

**Interfaces:**
- Produces:
  - `struct Handle(String)` — `Handle::generate() -> Handle` (32 hex from `getrandom`), `Handle::parse(&str) -> Result<Handle, BrokerError>` (exactly 32 lowercase hex), `Display` prints the full id, `Debug` prints only the first 8 hex chars plus `…`.
  - `enum Scope { Paths(Vec<String>), Profile(String), Task }` (serde, tagged).
  - `enum Resource { Paths(Vec<String>), Profile(String), Task }` (`Paths` is granted only if EVERY path matches the scope, and journals as one decision).
  - `struct CapabilityGrant { handle: Handle, task: TaskId, operation: Capability, scope: Scope, expires_ts: i64, revoked: bool }`.
  - `enum Denial { UnknownHandle, WrongTask, WrongOperation, OutOfScope, Expired, Revoked }` with `fn reason(&self) -> &'static str` (`"unknown_handle"`, `"wrong_task"`, `"wrong_operation"`, `"out_of_scope"`, `"expired"`, `"revoked"`).
  - `fn authorize(grant: &CapabilityGrant, task: &TaskId, op: Capability, resource: &Resource, now: i64) -> Result<(), Denial>` — check order: WrongTask, WrongOperation, Revoked, Expired (`now >= expires_ts`), OutOfScope.
  - `fn scope_for(op: Capability, contract: &Contract) -> Scope` — `WorkspaceApplyPatch` ⇒ `Paths(editable_paths)`, `SnapshotRead` ⇒ `Task` (a snapshot read has no path to check; the spec's "editable paths" scope applies to patches), `VerificationRun` ⇒ `Profile(verification_profile)`, `ArtifactExport` ⇒ `Task`.
  - `struct EffectTimeouts { verification: Duration, other: Duration }` (default 60 s / 30 s) and `fn lease_expiry(now: i64, timeout_secs: u64, task_deadline: i64) -> i64` = `now + min(timeout, max(task_deadline − now, 0))`.
- Consumes: `Capability`, `Contract`, `TaskId` from earlier crates.

- [ ] **Step 1: Write failing tests** (in `broker.rs`):

```rust
#[test] fn handles_are_32_hex_and_unique() {
    let a = Handle::generate(); let b = Handle::generate();
    assert_ne!(a, b);
    assert_eq!(a.to_string().len(), 32);
    assert!(a.to_string().bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}
#[test] fn debug_never_prints_the_full_handle() {
    let h = Handle::generate();
    assert!(!format!("{h:?}").contains(&h.to_string()));
}
#[test] fn parse_rejects_wrong_length_and_uppercase() { /* "" , 31 chars, 33 chars, "A"*32, "g"*32 all Err */ }
#[test] fn authorize_checks_each_denial_in_order() {
    // grant for task T, op ApplyPatch, scope Paths(["src/**"]), expires 100
    // wrong task => WrongTask; wrong op => WrongOperation; revoked+expired => Revoked (revoked first);
    // now=100 => Expired (boundary is >=); Paths(["tests/x.py"]) => OutOfScope; Paths(["src/../tests/x"]) => OutOfScope; Paths(["src/a.py","tests/x.py"]) => OutOfScope (all-or-nothing); Paths(["src/a.py"]) at now=99 => Ok
}
#[test] fn profile_and_task_scopes() { /* Profile("p1") grants Resource::Profile("p1") only; Task scope grants Resource::Task only; mismatched resource kinds (e.g. Task scope with Resource::Paths) => OutOfScope */ }
#[test] fn scope_for_maps_contract_fields() { /* ApplyPatch/SnapshotRead => Paths(editable_paths); VerificationRun => Profile(verification_profile); ArtifactExport => Task */ }
```
and in `lease.rs`: `lease_is_capped_by_timeout_and_deadline` (`lease_expiry(1000, 60, 1030)==1030`, `(1000,60,2000)==1060`, `(1000,60,900)==1000`, i.e. a past deadline gives an already-expired lease).
and in `contract.rs`: `profile_digest_must_be_64_hex_when_present`, `contract_without_profile_digest_still_parses`, `path_matches_is_shared_with_path_allowed` (table test of `src/**`, exact name, `src/../x`, absolute, `srcfoo/x`).

- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-core` → compile errors.
- [ ] **Step 3: Implement.** Add `getrandom = "0.4.3"` to workspace deps and `agentos-core`. `Handle::generate` fills 16 bytes with `getrandom::fill`. `authorize` is pure.
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(core): broker rules, handles, lease math, profile_digest field"`

---

### Task 2: Broker persistence and call-site migration (`agentos-store`, engine call sites)

**Files:**
- Create: `crates/agentos-store/src/caps.rs`
- Modify: `crates/agentos-store/src/db.rs` (capabilities table gains `scope TEXT NOT NULL` and `created_ts INTEGER NOT NULL`; `user_version` 2 and refusal of 1; reserved names `CapabilitiesIssued`, `CapabilityGranted`, `CapabilityDenied`, `CapabilityRevoked`), `crates/agentos-store/src/effects.rs` (`record_intent` uses `authorize` instead of `contract.capabilities.contains`), `crates/agentos-engine/src/runner.rs` and `journal.rs` (keep the existing `Denied` audit and `capability {cap:?} not granted` observation wording; add the reason), existing test helpers in `crates/agentos-store/tests/*` and `crates/agentos-engine/tests/*` (call `approve_task` after `create_task`)
- Test: `crates/agentos-store/tests/caps.rs`

**Interfaces:**
- Consumes: `broker::{Handle, Scope, Resource, Denial, CapabilityGrant, authorize, scope_for}`.
- Produces on `Db`:
  - `approve_task(&self, task: &TaskId) -> Result<Vec<Capability>, DbError>` — idempotent; one handle per capability in the stored contract, `expires_ts = deadline_ts`, journals one `CapabilitiesIssued` event listing operations and the 8-char handle prefixes (never full handles). Second call returns the same set and writes nothing.
  - `authorize(&self, task: &TaskId, op: Capability, resource: &Resource) -> Result<Handle, DbError>` — looks up the task's handle for `op` (none ⇒ `DbError::CapabilityDenied { capability: op, reason: "not_approved" }` for an unapproved task, `"unknown_handle"` if approved but no handle for `op`), runs `broker::authorize` with the wall clock, and in the same write transaction journals `CapabilityGranted` or `CapabilityDenied` `{operation, resource, reason, handle_prefix}`. A denial is committed even though the call returns `Err` (same pattern as the existing `CapabilityDenied` audit).
  - `revoke(&self, task: &TaskId, only: Option<Capability>) -> Result<Vec<Capability>, DbError>` — sets `revoked=1`, journals `CapabilityRevoked`, returns the revoked operations (empty if nothing changed; no event then).
  - `grants(&self, task: &TaskId) -> Result<Vec<CapabilityGrant>, DbError>` (for status output; `Debug` of grants shows prefixes only).
  - `DbError::CapabilityDenied` changes to `{ capability: Capability, reason: String }`; update `journal.rs` accordingly.
- `record_intent` takes the `Resource` as a new parameter and calls `authorize` once per NEW effect inside its transaction (an idempotent re-intent must not journal a second grant).

- [ ] **Step 1: Write failing tests** (`tests/caps.rs`): `approve_issues_one_handle_per_contract_capability_and_journals_prefixes_only` (no 32-hex string appears in any journal payload — scan all events), `approve_is_idempotent`, `record_intent_before_approval_is_denied_not_approved_and_journaled`, `authorize_granted_is_journaled_with_operation_and_resource`, `each_denial_reason_is_journaled_exactly_once_and_creates_no_effect` (revoked, expired via a contract with `deadline_seconds: 1` and a sleep-free clock injection — add `Db::with_clock(Box<dyn Fn() -> i64 + Send>)` for tests —, out of scope, unknown handle), `revoke_blocks_new_intents_but_keeps_completed_effects`, `revoke_one_capability_leaves_others`, `revoke_twice_second_is_a_noop_without_event`, `idempotent_reintent_does_not_journal_a_second_grant`, `reopening_a_v1_database_is_refused_with_a_clear_error`, `reserved_event_names_cannot_be_forged_via_append_audit` (extend the existing list test), fault-injection `trigger on CapabilityGranted aborts record_intent: no effect, no usage, task unchanged`.
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-store`.
- [ ] **Step 3: Implement.** Resources the runner passes: `ApplyPatch` ⇒ `Resource::Paths(<the patch's paths>)` (reuse the path parser the runner's broker pre-check already uses; one decision and one journal event per patch, not per path); `ReadSnapshot` ⇒ `Resource::Task`; `RunVerification` ⇒ `Resource::Profile(<profile id from the stored contract>)`. The runner's pre-check keeps producing the existing `Denied` audit row and `PatchRejected` observation, now carrying the `authorize` denial reason. Update all existing test helpers (store and engine) to call `approve_task` after `create_task`; no existing assertion may be weakened.
- [ ] **Step 4: Run, expect PASS** — `docker compose run --rm test cargo test --workspace` (everything that passed before still passes).
- [ ] **Step 5: Commit** — `git commit -am "feat(store,engine): capability broker persistence; authorize replaces contract-list checks"`

---

### Task 3: Job-directory protocol (`agentos-engine/src/job.rs`)

**Files:**
- Create: `crates/agentos-engine/src/job.rs`
- Modify: `crates/agentos-engine/src/lib.rs`
- Test: inline plus `crates/agentos-engine/tests/job.rs`

**Interfaces:**
- Produces:
  - `struct JobRequest { effect_id: EffectId, task_id: TaskId, kind: EffectKind, payload: Vec<u8>, contract: Contract, attempt_id: AttemptId, lease_generation: u64, lease_expiry: i64, task_deadline: i64, worker: WorkerConfig }` (serde); `WorkerConfig`, `HostConfig { snapshot_dir: PathBuf, profile_dir: PathBuf, work_root: PathBuf, verify_timeout_secs: u64, profile_digest: Option<Digest> }` and `ScriptedConfig { script: String }` are declared here in `job.rs` (Task 4 builds the workers from them): `enum WorkerConfig { Host(HostConfig), Scripted(ScriptedConfig) }`.
  - `enum JobState { Starting, Running, Exited, Killed }`, `enum KillReason { Lease, Deadline, Cancel }`, `struct JobStatus { state: JobState, reason: Option<KillReason>, pid: Option<u32>, lease_expiry: i64, updated_ts: i64 }`.
  - `struct JobDir { path: PathBuf }`: `JobDir::create(jobs_root: &Path, req: &JobRequest) -> io::Result<JobDir>` (creates `<root>/<effect>-<attempt>/`, validates both ids with the existing plain-name rule, writes `request.json` atomically; refuses if the directory exists), `JobDir::open(path)`, `request() -> io::Result<JobRequest>`, `write_status(&JobStatus)` (atomic), `read_status() -> Option<JobStatus>` (a torn or unparseable file ⇒ `None`), `write_receipt(&ExecOutcome)` (writes `output.bin` then `receipt.json` with the fsync/rename/fsync-dir rule), `read_receipt() -> Option<ExecOutcome>` (requires both files; checks `Digest::of(output)` equals the receipt's `result_digest`, otherwise `None` and a `tracing::warn!`), `drop_cancel()`, `cancel_requested() -> bool`, `list(jobs_root: &Path, effect: &EffectId) -> Vec<JobDir>` (all attempts of an effect, sorted by lease generation ascending; ignores names with `.tmp`).
  - `fn pid_alive(pid: u32) -> bool` via `rustix::process::test_kill_process`-style check (`kill(pid, None)`), returning false on ESRCH.
  - `fn is_dead(status: Option<&JobStatus>, receipt_present: bool, now: i64) -> bool` — dead when the state is `Exited`/`Killed`, or a receipt exists, or (`Running`/`Starting`/missing and `lease_expiry < now` and pid not alive).

- [ ] **Step 1: Write failing tests**: round trip of request/status/receipt; `torn_status_reads_as_none` (truncate the file mid-JSON); `status_write_is_atomic` (a concurrent reader thread never sees a partial file across 2 000 rewrites); `receipt_requires_both_files_and_matching_digest` (corrupt `output.bin` ⇒ `None`); `create_refuses_existing_directory`; `create_rejects_traversal_ids`; `list_orders_attempts_by_lease_generation_and_ignores_tmp`; `is_dead_table` covering all rows including a live pid with an unexpired lease (not dead) and an expired lease with a live pid (not dead: the supervisor still has to kill it); `cancel_marker_roundtrip`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** (shared `sync_dir`/atomic-write helper lives here; later tasks reuse it, and the existing duplicates in `blob.rs`/`export.rs` are left alone).
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): job directory protocol"`

---

### Task 4: Worker trait, host-process worker and the worker entry point

**Files:**
- Create: `crates/agentos-engine/src/worker.rs`
- Modify: `crates/agentos-engine/src/process.rs` (`run_in_group` gains `own_group: bool`; when `false` the child stays in the caller's process group so a supervisor's `killpg` reaches it), `crates/agentos-engine/src/fixture.rs` (take `own_group` from a `FixtureExecutor::inside_worker()` builder; add the pinned-profile digest check), `crates/agentos-engine/src/lib.rs`
- Test: `crates/agentos-engine/tests/worker.rs`

**Interfaces:**
- Produces:
  - `trait Worker { fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send; fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = Reconciliation> + Send; fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>>; }` — same shape as the relevant `Executor` methods.
  - `struct HostProcessWorker(FixtureExecutor)` built from `HostConfig` with `inside_worker()`; delegates to the existing fixture logic; before `RunVerification` it re-digests the staged profile copy and, when `HostConfig.profile_digest` is `Some(d)` and the digest differs, returns `Failure("profile digest mismatch: pinned <d>, found <x>")` without running anything.
  - `struct ScriptedWorker { script: String }` — runs `sh -c <script>` in the worker's own group (inherited), stdout becomes the success `output`; refuses to run (Failure `"scripted workers are disabled"`) unless the environment variable `AGENTOS_TEST_WORKERS=1` is set. Test facility for the supervisor tests, documented as such.
  - `async fn run_worker(job: &JobDir) -> io::Result<()>` — reads `request.json`, builds the configured worker, runs the effect, and writes the outcome to `<job>/outcome.json` (atomic) — NOT `receipt.json`; only the supervisor writes the receipt.
- Consumes: `JobDir`, `JobRequest`, `WorkerConfig` (Task 3).

- [ ] **Step 1: Write failing tests**: `host_worker_matches_fixture_executor_on_a_full_patch_and_verify_run` (same outputs/digests as calling `FixtureExecutor` directly for ReadSnapshot, ApplyPatch, RunVerification on the fixtures); `verification_with_a_wrong_pinned_digest_fails_before_running_the_profile` (the check script, if run, would write a marker file; assert no marker); `verification_with_the_right_pinned_digest_passes`; `scripted_worker_is_refused_without_the_env_guard`; `scripted_worker_runs_with_the_guard` (set the env var only inside that test via a serial lock); `run_worker_writes_outcome_json_and_not_receipt`; `inside_worker_verification_child_stays_in_the_callers_process_group` (spawn a helper that prints its pgid and the child's pgid and compare; a normal `FixtureExecutor` keeps `own_group` true and the existing process-group tests still pass).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-engine --test worker`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS**, then the whole engine suite (`cargo test -p agentos-engine`) to confirm the existing process-group tests are unchanged.
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): Worker trait, host-process worker, scripted test worker, worker entry point"`

---

### Task 5: Supervisor logic and the `agentos-supervisor` binary

**Files:**
- Create: `crates/agentos-engine/src/supervisor.rs`, `crates/agentos-supervisor/{Cargo.toml,src/main.rs}`
- Modify: root `Cargo.toml` (`rustix` features gain `fs` for `flock`; `process` already covers `setsid` and `kill_process_group`)
- Modify: `crates/agentos-engine/src/lib.rs`, root `Cargo.toml` (workspace member via the existing `crates/*` glob)
- Test: `crates/agentos-supervisor/tests/supervisor.rs` (uses `env!("CARGO_BIN_EXE_agentos-supervisor")` and `AGENTOS_TEST_WORKERS=1`)

**Interfaces:**
- Produces:
  - `pub async fn run_supervisor(job: &JobDir, worker_exe: &Path) -> io::Result<JobState>` — steps: `setsid` (rustix, ignore EPERM), write `status Starting`, spawn `<worker_exe> worker <job_dir>` with `process_group(0)` (own group, stdio null), write `status Running{pid, lease_expiry}`, then poll every 50 ms: child exited ⇒ read and VALIDATE `outcome.json` (effect id, attempt id and lease generation must equal the request's; the result digest must match its output; otherwise a `Failure("worker produced an invalid outcome")` receipt), write receipt, `killpg` the group, status `Exited`; `now >= lease_expiry` ⇒ `killpg`, Failure receipt reason `"lease expired"`, status `Killed{Lease}`; `now >= task_deadline` ⇒ same with `"deadline exceeded"`, `Killed{Deadline}`; `cancel` marker ⇒ `"cancelled"`, `Killed{Cancel}`. The receipt is always written BEFORE the terminal status. Checks run in the order cancel, deadline, lease so the reason is deterministic when several apply.
  - Binary `agentos-supervisor`: `run <job_dir>` (calls `run_supervisor` with `current_exe()` as the worker exe, exit code 0 for any terminal state, 1 for supervisor-internal errors) and `worker <job_dir>` (calls `run_worker`). Supports `AGENTOS_SUPERVISOR_LEASE_POLL_MS` for tests (default 50).
- Consumes: `JobDir`, `run_worker`, `ExecOutcome::failure`.

- [ ] **Step 1: Write failing tests** (binary-level, scripted worker, tiny leases using real clocks, each under 5 s): `normal_exit_writes_receipt_before_terminal_status` (observe by polling: whenever `status=Exited` is visible `receipt.json` already is); `lease_expiry_kills_a_forked_grandchild` (script: `(sleep 30; touch marker) & sleep 30`, lease 1 s; assert `Killed{Lease}`, receipt Failure `lease expired`, then wait 2 s and assert `marker` never appears and `pid_alive` is false for every descendant — collect descendants by writing `$$` of each into files); `deadline_beats_lease_reason_when_both_expired`; `cancel_marker_kills_within_500ms`; `worker_that_exits_normally_but_leaves_a_background_child_has_the_child_reaped` (child would write a marker after 1 s); `invalid_outcome_from_the_worker_becomes_a_failure_receipt` (script writes a forged `outcome.json` with another effect id — the supervisor must not publish it); `supervisor_survives_the_launcher_dying` (launch via `setsid`-less spawn, kill the parent test helper process, the job still completes); `supervisor_crash_leaves_running_status_and_no_receipt` (SIGKILL the supervisor itself mid-job; the status stays `Running` — this is the input for recovery tests later); `second_supervisor_on_the_same_job_directory_refuses` (an `flock` on `<job>/lock` taken with rustix `flock`; guards double launch).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-supervisor`.
- [ ] **Step 3: Implement.** The lock file is created by the supervisor at startup and held for its lifetime; `JobDir::is_dead` (Task 3) treats an unlocked, non-terminal job whose lease expired as dead.
- [ ] **Step 4: Run, expect PASS**, then run the supervisor tests 20 times in one container to check for timing flakiness: `docker compose run --rm test bash -c 'for i in $(seq 20); do cargo test -p agentos-supervisor || exit 1; done'`.
- [ ] **Step 5: Commit** — `git commit -am "feat(supervisor): per-job supervisor enforcing lease, deadline and cancel"`

---

### Task 6: `SupervisedExecutor`

**Files:**
- Create: `crates/agentos-engine/src/supervised.rs` (also hosts `ExecCounts`, moved from `durable.rs`)
- Modify: `crates/agentos-engine/src/lib.rs`
- Test: `crates/agentos-engine/tests/supervised.rs`

**Interfaces:**
- Produces:
  - `struct SupervisedExecutor { jobs_root: PathBuf, supervisor_bin: PathBuf, worker: WorkerConfig, timeouts: EffectTimeouts, counts: ExecCounts, crash: Option<CrashHook>, reconciler: FixtureExecutor }` with `SupervisedExecutor::new(jobs_root, supervisor_bin, worker: WorkerConfig, counts) -> io::Result<Self>`, `.with_timeouts(EffectTimeouts)`, `.with_crash(Option<CrashHook>)`.
  - `impl Executor for SupervisedExecutor`:
    - `run(req, ctx)`: `counts.record`; compute `lease_expiry` via `lease_expiry(now, timeout, deadline_ts)` (the task deadline is passed in `EffectRequest` — add `deadline_ts: i64` to `EffectRequest`, set by `steps::request` from `Db`); `JobDir::create`; spawn `<supervisor_bin> run <job_dir>` detached (stdio null, no wait handle kept, `process_group` untouched because the supervisor calls `setsid` itself); poll `read_receipt()` every 25 ms until present or the job `is_dead` without a receipt (then return `ExecOutcome::failure(.., "supervisor died without a receipt")`); return the outcome. A launch failure returns `ExecOutcome::failure(.., "supervisor launch failed: <err>")`.
    - `retained_outcome(effect)`: highest lease generation job with a valid receipt (via `JobDir::list`/`read_receipt`).
    - `reconcile`/`current_workspace`: delegate to the in-process `reconciler` (read-only, built from the `HostConfig`; documented as the 3a stand-in until 3b moves the workspace into the guest).
    - `fence(&self, effect: &EffectId, wait: Duration) -> Fence` where `enum Fence { Dead, Unknown }` — drops `cancel` in every non-terminal job of the effect, polls up to `wait` (10 s default) for terminal status, uses `JobDir::is_dead`, returns `Dead` when every attempt is dead.
  - `CrashPoint::DuringExecute` now means "the controller dies after launching the job and before reading its receipt": when the hook fires right after the launch, `run` returns immediately with a throwaway failure outcome and does NOT wait; the runner discards it because the tripped hook turns into `Err(Crashed)` before the outcome is used, exactly as the Phase 2 `DurableExecutor` behaved. The job keeps running under its supervisor.
- Consumes: `JobDir`, `EffectTimeouts`, `lease_expiry`, `WorkerConfig`.

- [ ] **Step 1: Write failing tests** (real supervisor binary via the shared `supervisor_bin()` test helper, `HostConfig` on the fixtures): full happy path through `run` for all three kinds equals `FixtureExecutor` outputs; `retained_outcome_finds_the_highest_lease_receipt`; `run_after_the_controller_forgot_the_job_still_finds_the_receipt` (drop the executor, construct a new one on the same `jobs_root`); `fence_waits_for_a_running_job_then_reports_dead` (scripted 3 s sleep, fence with wait 5 s ⇒ `Dead` and the job's status `Killed{Cancel}`); `fence_unknown_when_the_supervisor_is_alive_and_ignores_cancel` (a scripted worker traps nothing, so test fence timeout with a supervisor stopped by SIGSTOP ⇒ `Unknown`); `supervisor_launch_failure_is_a_failure_outcome` (bad binary path); `counts_record_each_launch`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): SupervisedExecutor over per-job supervisors"`

---

### Task 7: Recovery with fencing; remove `DurableExecutor`; migrate the crash tests and the CLI

**Files:**
- Modify: `crates/agentos-engine/src/recover.rs`, `steps.rs`, `crash.rs` (`DuringExecute` semantics), `runner.rs`, `executor.rs` (add `fn fence(&self, _effect: &EffectId) -> impl Future<Output = bool>` with default `true`, implemented by `SupervisedExecutor` as `fence(..) == Fence::Dead`), `crates/agentos-cli/src/home.rs`, `drive.rs`, README demo script `scripts/demo.sh`
- Delete: `crates/agentos-engine/src/durable.rs`
- Modify tests: `crates/agentos-engine/tests/{crash_matrix,happy_path,common/mod}.rs`, `crates/agentos-cli/tests/cli.rs`
- Create: `crates/agentos-engine/tests/common/supervisor_bin.rs` and the same helper for the CLI tests: `supervisor_bin() -> PathBuf` runs `cargo build -p agentos-supervisor` once per test process (`std::sync::Once`) and returns `<target dir>/debug/agentos-supervisor`, so any `docker compose run --rm test cargo test -p <crate>` invocation works without a separate build step. `compose.yaml` and the `Dockerfile` stay unchanged.

**Interfaces:**
- Consumes: `SupervisedExecutor`, `Fence`.
- Behavior changes:
  - `recover` for a DISPATCHED/UNKNOWN effect: (1) `executor.retained_outcome` ⇒ publish without re-running (unchanged); (2) otherwise `executor.fence(effect)`; if it returns false ⇒ `mark_unknown` and fail the task with `unreconcilable effect <id>` (a new `RecoveryDecision` decision `FenceFailed`); if true ⇒ re-check `retained_outcome` once (the job may have finished while fencing), then decide by retry policy exactly as before (Retry ⇒ redispatch with lease+1; ReconcileThenRetry ⇒ `reconcile`).
  - `CrashPoint::DuringExecute` becomes "the controller dies after the job was launched and before it read the receipt" — the worker keeps running under its supervisor, so the matrix rows `*_during_execute` now expect `PublishRetained` (not `PublishReconciled`); keep one new row `patch_supervisor_killed_mid_apply` where the supervisor is SIGKILLed after the worker applied the patch (ScriptedWorker is not used here; use `HostProcessWorker` plus a test hook that kills the supervisor after `outcome.json` appears but before `receipt.json`) to keep the `PublishReconciled` path covered; the lease/status logic must classify that job as dead.
  - `home.executor(task)` builds a `SupervisedExecutor` (`<home>/jobs`, supervisor binary from `AGENTOS_SUPERVISOR_BIN` or `agentos-supervisor` next to `current_exe()`, `HostConfig` from the task dir). `<home>/receipts` is no longer created; README says so.
  - A `status` output field `jobs`: per outstanding effect the latest job state, for visibility.

- [ ] **Step 1: Write failing tests**: first migrate the crash matrix helpers to build a `SupervisedExecutor` (tests fail to compile/behave until the executor exists in the helpers); then add `recover_fences_a_running_job_before_redispatching` (scripted-free: a verification job with a long lease is left `Running` by killing the controller; recovery fences, re-runs, final state SUCCEEDED, two jobs exist, first is `Killed{Cancel}`, usage equals the uncrashed baseline), `recover_marks_unknown_when_fence_fails` (SIGSTOP the supervisor ⇒ `FenceFailed` decision, effect UNKNOWN, task FAILED `unreconcilable effect`), `no_live_process_remains_after_recovery` (scan `/proc` for any process whose cwd/cmdline references the home's jobs dir after the run), CLI test `controller_sigkill_while_a_job_runs_then_resume_publishes_the_receipt` (real `kill -9` of the CLI process while a slow verification runs; the job completes under its supervisor; `resume` publishes it and executes the effect exactly once — assert via the jobs dir: exactly one job directory for that effect).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test --workspace`.
- [ ] **Step 3: Implement**; delete `durable.rs`, move `ExecCounts`, update docs in `recover.rs` ("receipts are job directories").
- [ ] **Step 4: Run, expect PASS**: whole workspace; then the crash matrix 20 times in one container (`for i in $(seq 20); do cargo test -p agentos-engine --test crash_matrix || exit 1; done`).
- [ ] **Step 5: Commit** — `git commit -am "feat(engine,cli): recovery fences supervisors; DurableExecutor replaced; crash tests migrated"`

---

### Task 8: Deadline enforcement

**Files:**
- Modify: `crates/agentos-store/src/db.rs` (`pub fn deadline_ts(&self, task: &TaskId) -> Result<i64>`), `crates/agentos-engine/src/runner.rs`, `recover.rs`, `steps.rs` (`request()` fills `EffectRequest.deadline_ts`), `crates/agentos-core/src/lease.rs` consumers
- Test: `crates/agentos-engine/tests/deadline.rs`

**Interfaces:**
- Consumes: `Db::with_clock` (Task 2), `lease_expiry`.
- Produces: `runner::deadline_passed(db, task) -> Result<bool>` used before each agent turn, before `intend`, before `dispatch`; when true: `recover` first (reconciles in-flight effects; the supervisor already killed jobs at the deadline), then `Failed{reason: "deadline exceeded"}`; recovery honors the same rule; a deadline-killed job's `Failure("deadline exceeded")` receipt completes its effect normally.

- [ ] **Step 1: Write failing tests** with the injected clock and short real deadlines: `deadline_between_turns_fails_the_task_without_new_effects`; `deadline_during_a_verification_kills_the_job_and_fails_the_task` (scripted slow check via a test profile, `deadline_seconds: 2` clock real) with assertions: task FAILED reason `deadline exceeded`, effect FAILED with the kill reason, usage has no `Reserved` rows, no process left; `deadline_expired_before_recovery_runs_still_reconciles_then_fails`; `past_deadline_task_dispatches_nothing` (lease_expiry gives an already-expired lease; assert no job directory is created); `cancel_wins_over_deadline_when_both_pending` (CANCELLED, not FAILED).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** (whole workspace).
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): enforce the task deadline"`

---

### Task 9: Revocation and cancellation of running jobs

**Files:**
- Create: `crates/agentos-cli/src/commands/revoke.rs`
- Modify: `crates/agentos-cli/src/{args,commands/mod,commands/control}.rs`, `crates/agentos-engine/src/supervised.rs` (`cancel_jobs(effects: impl Iterator<Item = EffectId>)`), `recover.rs`
- Test: `crates/agentos-cli/tests/cli.rs`, `crates/agentos-engine/tests/revoke.rs`

**Interfaces:**
- Produces: CLI `agentos revoke TASK_ID [--capability NAME]` (names as in the contract, e.g. `verification.run`; unknown name ⇒ exit 2): calls `Db::revoke`, then for every outstanding effect whose kind's capability was revoked drops the `cancel` marker in its running jobs; prints `{"revoked":[...],"cancelled_jobs":N}`. `cancel` (existing) also drops markers for all outstanding jobs before reconciling. The killed effect completes as Failure `"cancelled"` through the normal receipt path.
- Consumes: `Db::revoke`, `JobDir::drop_cancel`, `JobDir::list`.

- [ ] **Step 1: Write failing tests**: `revoke_verification_run_kills_the_running_check_and_denies_the_next_request` (engine-level with a slow check; afterwards `authorize` journals a `CapabilityDenied` reason `revoked`, earlier effects still listed in `events`); `revoke_one_capability_keeps_other_effects_running`; `revoke_unknown_capability_name_exits_2`; `revoke_on_a_terminal_task_is_allowed_and_cancels_nothing`; CLI `cancel_drops_markers_for_running_jobs_and_ends_cancelled_with_no_live_process`; `revoked_snapshot_read_stops_a_resumed_task_with_failed_not_stuck` (resume after revoke ⇒ the task fails cleanly with the denial reason, no loop).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** (whole workspace).
- [ ] **Step 5: Commit** — `git commit -am "feat(cli,engine): revoke capabilities and cancel running jobs"`

---

### Task 10: Profile registry and digest pinning

**Files:**
- Create: `crates/agentos-cli/src/commands/profile.rs`
- Modify: `crates/agentos-cli/src/{args,home,commands/submit}.rs`, `crates/agentos-engine/src/workspace.rs` (reuse `workspace_digest`), `fixture.rs` pinned check (Task 4 wired the config; here `submit` supplies the digest)
- Test: `crates/agentos-cli/tests/cli.rs`

**Interfaces:**
- Produces: `agentos profile register DIR` ⇒ validates `profile.json` (`id` plain name, `command` non-empty array), copies to `<home>/profiles/<id>@<digest>/` (read-only perms, recursive), prints `{"id","digest"}`; same bytes twice is a no-op; `agentos profile list`. `submit` resolves `verification_profile` to the newest registered entry for the id (or the `profile_digest` pin if present; mismatch/none ⇒ exit 2), copies it into the task dir, records `profile_digest` in `Submitted` and sets `HostConfig.profile_digest` for the worker. `--profiles DIR` keeps working as an extra registry root (tests).
- Consumes: `Contract.profile_digest` (Task 1), `HostConfig.profile_digest` (Task 3/4).

- [ ] **Step 1: Write failing tests**: `register_twice_is_a_noop_and_changed_bytes_are_a_new_entry`; `registered_entries_are_read_only`; `submit_with_a_pin_for_a_missing_digest_exits_2`; `submit_with_the_pin_uses_exactly_that_digest_even_if_a_newer_entry_exists`; `profile_bytes_changed_after_submission_voids_evidence` (modify the staged copy in the task dir ⇒ the verification effect is Failure `profile digest mismatch`, evidence never `passed`, task not SUCCEEDED); `traversal_ids_in_profile_json_are_rejected_at_register`; `registered_profile_runs_end_to_end` (the demo flow with a registered fixture profile ⇒ SUCCEEDED).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): profile registry and digest pinning"`

---

### Task 11: Export authorization, CLI status, README and final verification

**Files:**
- Modify: `crates/agentos-cli/src/commands/{export,inspect}.rs`, `crates/agentos-engine/src/export.rs` (bundle `manifest.json` gains `capabilities: [{operation, handle_prefix, revoked}]` — prefixes only), `README.md`, `scripts/demo.sh`
- Test: `crates/agentos-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `Db::authorize`, `Db::grants`.
- Behavior: `agentos export` calls `authorize(ArtifactExport, Resource::Task)` first; denial ⇒ exit 1 with the reason, nothing written, decision journaled; `status` prints `capabilities` (operation, prefix, revoked, expires). README: rewrite "Known limits" (remove the orphan, deadline, profile-pinning, export-capability items; keep same-UID host execution, model stand-in, single-driver, single-patch resume; add the 3b list), document the new commands (`revoke`, `profile register|list`), the job directory layout, how to run the demo with docker compose (including a SIGKILL-the-controller transcript from `scripts/demo.sh`, real output).

- [ ] **Step 1: Write failing tests**: `export_without_artifact_export_capability_exits_1_and_writes_nothing` (contract without it; journal has `CapabilityDenied` reason `unknown_handle`); `export_after_revoking_artifact_export_is_denied_with_reason_revoked`; `export_journals_the_granted_decision`; `manifest_lists_capabilities_with_prefixes_only` (no 32-hex substring anywhere in the bundle); `status_lists_capabilities_without_full_handles`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement**; regenerate the README transcript from `scripts/demo.sh` run under docker compose.
- [ ] **Step 4: Run the full verification**: `docker compose run --rm test cargo test --workspace` (all green, no warnings), then the flakiness loops for the supervisor and crash-matrix suites (20 runs each in one container), then `docker compose run --rm test cargo clippy --workspace --all-targets` and report any NEW warnings.
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): broker-authorized export, capability status, README for Phase 3a"`

---

## Follow-on

Phase 3b (separate spec and plan): Firecracker worker behind `Worker`, guest image build and registration, per-VM vCPU/memory limits, no guest network, no host secrets, moving the workspace into the guest (replacing the in-process `reconciler`). Needs a KVM check and the Firecracker v1.17.0 binary (checked 2026-10-02) first.

## Self-Review

- **Spec coverage:** supervisor shape and job directory (T3, T5, T6); lease and deadline rules (T1, T5, T8); fencing and recovery (T6, T7); broker handles, authorize, journaling, revocation (T1, T2, T9); export authorization (T11); profile registry and pinning (T4, T10); schema v2 and reserved names (T2); README limits (T11); testing list from the spec is distributed across T3–T11 (grandchild kill T5, controller SIGKILL T7, redispatch/stale receipt T7, broker denials T2, revoke mid-job T9, deadline T8, profile tamper T4/T10).
- **Deliberate deviations from the spec text (surface to the human):** (1) handle issuance is the explicit `Db::approve_task` called by the CLI at `--yes`/`resume`, matching the spec, but all existing test helpers must now call it; (2) `snapshot.read` uses `Scope::Task` (the spec said editable paths for both snapshot.read and apply_patch; a snapshot read has no path to check, so Task 2 widens it); (3) `EffectRequest` gains `deadline_ts`; (4) `ScriptedWorker` ships in the engine behind an env-var guard for tests.
- **Known 3a limits to keep in the README:** host-process workers are same-UID; `reconcile`/`current_workspace` still run in the controller process against the workspace directory (3b moves them into the guest); a descendant that calls `setsid` escapes the group kill.
