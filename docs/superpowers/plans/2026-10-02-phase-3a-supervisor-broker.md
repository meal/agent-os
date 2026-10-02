# Phase 3a: Capability Broker, Per-Job Supervisor and Leases — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the contract-list capability check with an opaque-handle broker (with revocation), run every effect under a detached per-job supervisor that enforces a bounded lease and the task deadline and keeps the durable receipt, and add a pinned profile registry — all with host-process workers (no VM).

**Architecture:** `agentos-core` gets the pure rules (handles, scopes, `authorize`, lease math). `agentos-store` persists handles and journals decisions. `agentos-engine` gains the job-directory protocol, a `Worker` trait with a host-process worker, the supervisor logic (also a `[[bin]] agentos-supervisor` in the engine crate), and `SupervisedExecutor`, which implements the existing `Executor` trait so `runner`, `steps` and `recover` keep their shape. The `agentos` CLI re-executes itself through hidden `supervise` subcommands. `DurableExecutor` is removed and its tests migrated. Liveness is decided by an `flock` held by the supervisor, never by pids.

**Tech Stack:** Rust 1.98.1 (pinned), Tokio 1.53, rusqlite 0.40 (bundled), rustix 1.1.5 (`process` feature: `setsid`, `kill_process_group`, `set_child_subreaper`; no `unsafe`), std `File::lock`/`try_lock` for `flock`, getrandom 0.4.3 (handle ids), assert_cmd 2.2.2 (dev; already used). Versions checked with `cargo search` on 2026-10-02; re-check before pinning. All builds and tests run through `docker compose`.

**Spec:** `docs/superpowers/specs/2026-10-02-phase-3a-supervisor-broker-design.md` (parent: `Agent_OS_v1_Build_Plan.md`, Phase 3). This plan was revised after an adversarial review; the spec carries the matching `[rev]` changes. Read the spec first.

## Global Constraints

- Supervisor is a per-job process, not a daemon: launched detached by the controller per effect (it calls `setsid` itself after a plain spawn); it owns the worker, enforces lease and deadline, writes the status and receipt, exits.
- Job directory `<home>/jobs/<effect_id>-<attempt_id>/`: `request.json` (includes `lease_expiry_ms`, `task_deadline_ms`, absolute-path worker config), `status.json` (`state`, `reason`, `supervisor_pid`, `worker_pgid`), `receipt.json` + `output.bin`, `outcome.json` (worker output), `cancel` marker, `lock`, `groups`, `supervisor.log`. Receipt durability: temp file, fsync, rename, fsync of the directory, written BEFORE `status.json` goes terminal.
- Times in job files are unix milliseconds. Lease expiry = `now + min(effect_timeout, task_deadline − now)`; `effect_timeout` = 70 s for verification, 30 s for other effects; a launch whose computed expiry is not in the future is refused (`Failure("deadline exceeded")`, no job directory).
- Liveness: a job is dead iff its status is terminal, or a valid receipt exists, or nobody holds its `lock`. The controller creates and `flock`s `lock` before launch and passes the locked file as the supervisor's stdin; pids are never used for liveness. `compose.yaml` gains `init: true`.
- Kill receipts: on kill, a valid `outcome.json` is published as the receipt; else `Retry` kinds (`ReadSnapshot`, `RunVerification`) get a `Failure(reason)` receipt; `ReconcileThenRetry` kinds (`ApplyPatch`, `ExportBundle`) get NO receipt, only `Killed{reason}` status, and are reconciled (never recorded as Failed). `ExecOutcome` gains `unresolved: bool` (serde default false); the runner turns an unresolved outcome into mark-UNKNOWN plus `Failed{reason: "unreconcilable effect …"}`.
- Recovery waits for a live job (lock held) until `lease_expiry_ms` + 5 s, then re-reads the receipt; it fences (drop `cancel`, after 2 s SIGKILL the recorded groups and `supervisor_pid`) only past that bound or on owner cancel/revoke. It never kills a job still inside its lease.
- Handles are opaque 128-bit ids (32 lowercase hex, `getrandom`), stored in the existing `capabilities` table (+ `scope` JSON, `created_ts`); issued by `Db::approve_task` (called by the CLI at `--yes` and on `resume` of a READY task); scope: `workspace.apply_patch` = `editable_paths`, `snapshot.read` = task, `verification.run` = profile id, `artifact.export` = task. Expiry = `deadline_ts` except `artifact.export` (no expiry, still revocable). `deadline_ts` is set at approval (0 = not started, never "passed"). The store resolves handles itself in 3a; journals show 8-char prefixes only; no full handle may appear in any journal payload, log or bundle.
- `Db::check` (pure, non-journaling) serves the runner's pre-checks; only the decision inside `record_intent` (once per new effect) and `mark_dispatched` is journaled. `mark_dispatched` re-authorizes; a denial there or in recovery's redispatch closes the task (`Failed{reason: "capability revoked: <op>"}`) and recovery runs in closing mode.
- `PRAGMA user_version` becomes 2; refuse any database with `user_version < 2` that already has a `tasks` table (and anything newer than 2) with a clear error; no migration.
- Deadline passed: wait for or fence live jobs and publish their receipts, append `Failed{reason: "deadline exceeded"}`, then run recovery in closing mode; a pending cancel wins.
- `agentos export` is authorized through the broker (`artifact.export`), journals the decision, and stays an `Exported` audit row.
- Non-goals: Firecracker, guest images, network/host-secret isolation, per-worker CPU/memory limits (3b); the model broker; export as a full effect; heartbeat leases; a supervisor daemon; job-directory garbage collection (README known limit).
- Commits carry NO Co-Authored-By trailer (user's CLAUDE.md). Use docker compose. Write tests and verify. No network request may carry the user's email or any identity (an earlier implementer leaked it in a crates.io User-Agent header; use `cargo search` only). `std::env::set_var` is `unsafe` in edition 2024 and the repo has no `unsafe`: pass test-only environment through `Command::env`.

## Review Focus

- A forked grandchild that would outlive the lease, including a descendant of the verification check (which runs in its own process group): every such process dies and no file it would write afterwards appears.
- A killed or dead `ApplyPatch` job is never recorded as Failed (workspace digest must keep matching the disk); a killed verification is.
- A controller killed (exit 75 and real SIGKILL) while a job runs: the job finishes under its supervisor, `resume` publishes its receipt, exactly one job directory exists for the effect, and no second attempt runs concurrently with the first.
- Torn/missing `status.json`, a job directory with `request.json` only, a supervisor SIGKILLed mid-job, a SIGSTOPped supervisor, a receipt written but status not yet terminal, zombies under docker: each resolves to one deterministic decision through the lock-based liveness rule.
- A handle that is expired, revoked, of another task or out of scope is denied with its reason, journaled once, creates no effect, and never appears in full in any journal, log or bundle; a revocation cannot be bypassed by an intent authorized earlier or by recovery's redispatch.
- A deadline passing between turns, during an effect and during recovery: task FAILED `deadline exceeded`, no worker running, no stranded `Reserved` usage, no launch with an expired lease, and the task still exportable.
- A tampered or swapped registered profile never yields `passed=true`.

## File Structure

```
crates/agentos-core/src/
  broker.rs        Handle, Scope, Resource, Denial, CapabilityGrant, authorize(), scope_for()
  lease.rs         EffectTimeouts, lease_expiry_ms()
  contract.rs      + optional profile_digest; path_matches() extracted; '@' rejected in verification_profile
crates/agentos-store/src/
  caps.rs          approve/authorize_in/check/revoke/grants
  db.rs            schema v2, user_version, reserved names, with_clock, deadline_ts
crates/agentos-engine/src/
  job.rs           JobDir, JobRequest, JobStatus, liveness, atomic IO
  worker.rs        Worker, HostProcessWorker, ScriptedWorker, run_worker()
  supervisor.rs    run_supervisor(), kill-receipt rules
  supervised.rs    SupervisedExecutor, SupervisorCmd, ExecCounts
  bin/agentos-supervisor.rs   [[bin]] run|worker
  process.rs       + groups_file
  durable.rs       REMOVED
crates/agentos-cli/src/
  commands/{revoke,profile,supervise}.rs   new; home.rs, drive.rs, submit/control/export wiring
README.md, compose.yaml (init: true), scripts/demo.sh
```

---

### Task 1: Broker rules and lease math (pure, `agentos-core`)

**Files:**
- Create: `crates/agentos-core/src/broker.rs`, `crates/agentos-core/src/lease.rs`
- Modify: `crates/agentos-core/src/contract.rs` (extract `pub fn path_matches(patterns: &[String], rel: &str) -> bool`, used by `Contract::path_allowed`; add `#[serde(default, skip_serializing_if = "Option::is_none")] profile_digest: Option<String>` validated as 64 lowercase hex when present; reject `@` in `verification_profile`), `crates/agentos-core/src/lib.rs`, root `Cargo.toml` + `agentos-core/Cargo.toml` (`getrandom = "0.4.3"`)
- Test: inline `#[cfg(test)]` modules

**Interfaces:**
- Produces:
  - `struct Handle(String)`: `Handle::generate()` (16 bytes from `getrandom::fill`, 32 hex), `Handle::parse(&str) -> Result<Handle, BrokerError>` (exactly 32 lowercase hex), `Display` full id, `Debug` first 8 hex chars + `…`, `fn prefix(&self) -> &str` (8 chars).
  - `enum Scope { Paths(Vec<String>), Profile(String), Task }`; `enum Resource { Paths(Vec<String>), Profile(String), Task }` (`Paths` granted only if EVERY path matches; mismatched kinds ⇒ `OutOfScope`).
  - `struct CapabilityGrant { handle: Handle, task: TaskId, operation: Capability, scope: Scope, expires_ts: Option<i64>, revoked: bool }` (`expires_ts` in unix seconds; `None` = no expiry).
  - `enum Denial { UnknownHandle, WrongTask, WrongOperation, OutOfScope, Expired, Revoked }`, `fn reason(&self) -> &'static str` (`"unknown_handle"`, `"wrong_task"`, `"wrong_operation"`, `"out_of_scope"`, `"expired"`, `"revoked"`).
  - `fn authorize(grant: &CapabilityGrant, task: &TaskId, op: Capability, resource: &Resource, now: i64) -> Result<(), Denial>`; check order WrongTask, WrongOperation, Revoked, Expired (`now >= expires`), OutOfScope.
  - `fn scope_for(op: Capability, contract: &Contract) -> Scope`: `WorkspaceApplyPatch` ⇒ `Paths(editable_paths)`; `SnapshotRead` ⇒ `Task`; `VerificationRun` ⇒ `Profile(verification_profile)`; `ArtifactExport` ⇒ `Task`.
  - `struct EffectTimeouts { verification: Duration, other: Duration }` (default 70 s / 30 s); `fn lease_expiry_ms(now_ms: i64, timeout_ms: i64, task_deadline_ms: i64) -> i64` = `now_ms + min(timeout_ms, task_deadline_ms − now_ms)`; `task_deadline_ms == 0` means "no deadline" ⇒ `now_ms + timeout_ms`; a result `<= now_ms` means "do not launch".

- [ ] **Step 1: Write failing tests** (`broker.rs`): `handles_are_32_lowercase_hex_and_unique`; `debug_never_prints_the_full_handle`; `parse_rejects_empty_31_33_chars_uppercase_and_non_hex`; `authorize_checks_each_denial_in_order` (grant for T, op ApplyPatch, scope `Paths(["src/**"])`, expires `Some(100)`: other task ⇒ WrongTask; other op ⇒ WrongOperation; revoked and expired ⇒ Revoked; now=100 ⇒ Expired (boundary `>=`); `Paths(["tests/x.py"])` ⇒ OutOfScope; `Paths(["src/../tests/x"])` ⇒ OutOfScope; `Paths(["src/a.py","tests/x.py"])` ⇒ OutOfScope (all-or-nothing); `Paths(["src/a.py"])` at now=99 ⇒ Ok); `no_expiry_never_expires` (`expires_ts: None`, now = `i64::MAX`); `profile_and_task_scopes_are_exact` (Profile("p1") grants only `Resource::Profile("p1")`; Task scope only `Resource::Task`; Task scope with `Resource::Paths` ⇒ OutOfScope); `scope_for_maps_contract_fields` (ApplyPatch ⇒ Paths(editable_paths); SnapshotRead and ArtifactExport ⇒ Task; VerificationRun ⇒ Profile). `lease.rs`: `lease_is_capped_by_timeout_and_deadline` (`lease_expiry_ms(1_000_000, 70_000, 1_030_000) == 1_030_000`; `(1_000_000, 70_000, 2_000_000) == 1_070_000`; `(1_000_000, 70_000, 900_000) <= 1_000_000`; deadline `0` ⇒ `1_070_000`). `contract.rs`: `profile_digest_must_be_64_hex_when_present`; `contract_without_profile_digest_parses_and_reserializes_byte_identically` (no `profile_digest` key emitted); `verification_profile_with_at_sign_is_rejected`; `path_matches_is_shared_with_path_allowed` (table: `src/**`, exact name, `src/../x`, absolute, `srcfoo/x`, `src` vs `src/**`).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-core` (compile errors).
- [ ] **Step 3: Implement.** `authorize` is pure; `path_matches` is the old `path_allowed` matcher over a slice.
- [ ] **Step 4: Run, expect PASS** (`cargo test -p agentos-core`).
- [ ] **Step 5: Commit** — `git commit -am "feat(core): broker rules, handles, lease math, optional profile_digest"`

---

### Task 2: Broker persistence, approval, call-site migration (`agentos-store`, engine, CLI)

**Files:**
- Create: `crates/agentos-store/src/caps.rs`
- Modify: `crates/agentos-store/src/db.rs` (capabilities table: `scope TEXT NOT NULL`, `created_ts INTEGER NOT NULL`, `expires_ts INTEGER` nullable; `SCHEMA_VERSION = 2`; open refuses `user_version < 2` when a `tasks` table already exists and anything `> 2`; reserved names `CapabilitiesIssued`, `CapabilityGranted`, `CapabilityDenied`, `CapabilityRevoked`; `Db::with_clock(Box<dyn Fn() -> i64 + Send>)` where the clock returns unix SECONDS and affects ONLY `authorize`/`check`/`approve_task`/`deadline_passed`, not the other `now_ts()` uses; `pub fn deadline_ts(&self, task: &TaskId) -> Result<i64>`; `create_task` stores `deadline_ts = 0`), `crates/agentos-store/src/effects.rs` (`record_intent` takes a `&Resource` and calls `authorize_in` inside its existing transaction; `mark_dispatched` re-authorizes), `crates/agentos-engine/src/runner.rs` + `crates/agentos-engine/src/journal.rs` (pre-checks call `Db::check`; keep the existing `Denied` audit row and `capability {cap:?} not granted` observation text; journal.rs matches the string `"CapabilityDenied"` inside `Denied` payloads, it does not use the `DbError` variant), `crates/agentos-cli/src/commands/submit.rs` and `commands/control.rs` (call `db.approve_task` after the owner approves: in `submit --yes` before `drive`, and in `resume` when the task is READY), all existing test helpers in `crates/agentos-store/tests/*`, `crates/agentos-engine/tests/*` and `crates/agentos-cli/tests/*` (call `approve_task` after `create_task`)
- Test: `crates/agentos-store/tests/caps.rs`

**Interfaces:**
- Consumes: `broker::{Handle, Scope, Resource, Denial, CapabilityGrant, authorize, scope_for}`.
- Produces on `Db`:
  - `approve_task(&self, task: &TaskId) -> Result<Vec<Capability>, DbError>`: idempotent; sets `deadline_ts = now + deadline_seconds` (only the first time), issues one handle per capability in the stored contract (`expires_ts = deadline_ts`, `None` for `ArtifactExport`), journals ONE `CapabilitiesIssued` event listing operations and 8-char prefixes. A second call returns the same set and writes nothing.
  - `check(&self, task: &TaskId, op: Capability, resource: &Resource) -> Result<(), DbError>`: pure read, no journal; `DbError::CapabilityDenied { capability: Capability, reason: String }` with reasons from `Denial::reason()` plus `"not_approved"` (no handles yet) and `"unknown_handle"` (approved, no handle for `op`).
  - `pub(crate) fn authorize_in(tx, task, op, resource, now) -> Result<Handle, DbError>`: the journaled variant used by `record_intent`/`mark_dispatched`; journals `CapabilityGranted` or `CapabilityDenied {operation, resource, reason, handle_prefix}`; a denial is committed (separate committed transaction after the main one rolls back, the pattern the existing `Denied` audit uses) while the call returns `Err`.
  - `revoke(&self, task: &TaskId, only: Option<Capability>) -> Result<Vec<Capability>, DbError>`: sets `revoked=1`, journals `CapabilityRevoked`, returns the operations actually revoked (empty and no event if nothing changed).
  - `grants(&self, task: &TaskId) -> Result<Vec<CapabilityGrant>, DbError>`.
  - `deadline_passed(&self, task: &TaskId) -> Result<bool>`: `deadline_ts != 0 && clock() >= deadline_ts`.
- `DbError::CapabilityDenied` becomes `{ capability, reason }`.

- [ ] **Step 1: Write failing tests** (`tests/caps.rs`): `approve_issues_one_handle_per_contract_capability_and_journals_prefixes_only`; `approve_sets_the_deadline_once`; `approve_is_idempotent`; `no_full_handle_appears_in_any_journal_payload` (read the real handles from the `capabilities.id` column through a raw connection and assert none of them occurs in any event payload; do NOT scan for 32-hex strings — effect ids and digests are 64-hex); `record_intent_before_approval_is_denied_not_approved_and_journaled`; `authorize_granted_is_journaled_with_operation_and_resource`; `each_denial_reason_is_journaled_exactly_once_and_creates_no_effect` (revoked, expired via `with_clock`, out_of_scope, unknown_handle); `idempotent_reintent_does_not_journal_a_second_grant`; `check_does_not_journal`; `mark_dispatched_denies_after_a_revoke_and_journals_it`; `revoke_blocks_new_intents_but_keeps_completed_effects`; `revoke_one_capability_leaves_others`; `revoke_twice_second_is_a_noop_without_event`; `artifact_export_handle_has_no_expiry` (clock far in the future, export still authorized); `reopening_a_user_version_0_database_with_tables_is_refused`, `reopening_a_version_3_database_is_refused`, `a_fresh_database_gets_version_2`; `reserved_event_names_cannot_be_forged_via_append_audit` (all four new names); fault injection `trigger aborting the CapabilityGranted insert makes record_intent write no effect, no usage, no task change`. Existing tests that must change (no assertion weakened, say so in the report): `store/tests/db.rs` (~236-246) appends an audit named `"CapabilityDenied"` — rename the label to a non-reserved name; `store/tests/effects.rs` (~335-340) asserts `DbError::CapabilityDenied(Capability::VerificationRun)` and an event count of `+1` — update to the struct variant and `+2` (the existing `Denied` row plus the new `CapabilityDenied` row; the pair is intentional).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-store`.
- [ ] **Step 3: Implement.** Resources the runner passes: `ApplyPatch` ⇒ `Resource::Paths(<the patch's paths>)` (reuse the path parser the runner's broker pre-check already uses; one decision per patch); `ReadSnapshot` ⇒ `Resource::Task`; `RunVerification` ⇒ `Resource::Profile(<profile id from the stored contract>)`.
- [ ] **Step 4: Run, expect PASS** — `docker compose run --rm test cargo test --workspace` (everything that passed before still passes).
- [ ] **Step 5: Commit** — `git commit -am "feat(store,engine,cli): capability broker persistence, approval, authorize at intent and dispatch"`

---

### Task 3: Job-directory protocol and liveness (`agentos-engine/src/job.rs`)

**Files:**
- Create: `crates/agentos-engine/src/job.rs`
- Modify: `crates/agentos-engine/src/lib.rs`
- Test: inline plus `crates/agentos-engine/tests/job.rs`

**Interfaces:**
- Produces:
  - `struct JobRequest { effect_id: EffectId, task_id: TaskId, kind: EffectKind, payload: Vec<u8>, contract: Contract, attempt_id: AttemptId, lease_generation: u64, lease_expiry_ms: i64, task_deadline_ms: i64, worker: WorkerConfig }` (serde).
  - `enum WorkerConfig { Host(HostConfig), Scripted(ScriptedConfig) }`, `struct HostConfig { snapshot_dir: PathBuf, profile_dir: PathBuf, work_root: PathBuf, verify_timeout_secs: u64, profile_digest: Option<Digest> }` (all paths MUST be absolute; `JobDir::create` rejects relative ones), `struct ScriptedConfig { script: String }`.
  - `enum JobState { Starting, Running, Exited, Killed }`, `enum KillReason { Lease, Deadline, Cancel }`, `struct JobStatus { state, reason: Option<KillReason>, supervisor_pid: Option<u32>, worker_pgid: Option<i32>, updated_ms: i64 }`.
  - `struct JobDir { path: PathBuf }`: `JobDir::create(jobs_root, req) -> io::Result<(JobDir, File)>` (creates `<root>/<effect>-<attempt>/`, validates both ids with the existing plain-name rule, writes `request.json` atomically, creates `lock` and takes `File::lock()` on it, and RETURNS the locked file so the caller can hand it to the supervisor as stdin; refuses an existing directory), `open(path)`, `request()`, `write_status(&JobStatus)` (atomic), `read_status() -> Option<JobStatus>` (torn/unparseable ⇒ `None`), `write_receipt(&ExecOutcome)` (writes `output.bin` then `receipt.json`, fsync/rename/fsync-dir), `read_receipt() -> Option<ExecOutcome>` (both files present and `Digest::of(output)` equals `result_digest`, else `None` with a `tracing::warn!`), `write_outcome`/`read_outcome` (same validation, for `outcome.json` + `outcome.bin`), `drop_cancel()`, `cancel_requested()`, `record_group(pgid)` (append) / `groups() -> Vec<i32>`, `lock_held() -> bool` (`try_lock` on a freshly opened descriptor: held elsewhere ⇒ `true`), `list(jobs_root, effect) -> Vec<JobDir>` (all attempts, ascending lease generation, ignoring `.tmp` names).
  - `fn is_dead(&self) -> bool` = terminal status OR valid receipt OR `!lock_held()`. No pid is consulted.
  - Shared helper `atomic_write(path, bytes)` and `sync_dir`.

- [ ] **Step 1: Write failing tests**: request/status/receipt/outcome round trips; `torn_status_reads_as_none`; `status_write_is_atomic` (a reader thread never sees a partial file across 2 000 rewrites); `receipt_requires_both_files_and_a_matching_digest`; `create_refuses_existing_directory_and_relative_paths_and_traversal_ids`; `create_returns_a_held_lock_and_dropping_it_frees_it` (`lock_held()` true while held, false after drop); `a_job_with_only_request_json_and_a_held_lock_is_alive` (the DuringExecute-right-after-launch race); `a_job_with_a_free_lock_and_no_status_is_dead`; `terminal_status_or_receipt_means_dead_even_if_the_lock_is_held`; `list_orders_attempts_by_lease_generation_and_ignores_tmp`; `cancel_marker_roundtrip`; `groups_file_append_and_read`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): job directory protocol with lock-based liveness"`

---

### Task 4: Worker trait, host-process worker, worker entry point

**Files:**
- Create: `crates/agentos-engine/src/worker.rs`
- Modify: `crates/agentos-engine/src/process.rs` (`run_in_group` gains `groups_file: Option<&Path>`: after spawning the child group it appends the pgid to that file BEFORE waiting; the child STAYS in its own group, so the Phase 2 grandchild guarantee is unchanged), `crates/agentos-engine/src/fixture.rs` (`FixtureExecutor::inside_worker(groups_file)` builder passes it through; before `RunVerification` re-digest the staged profile copy and, if `HostConfig.profile_digest` is `Some(d)` and the digest differs, return `Failure("profile digest mismatch: pinned <d>, found <x>")` without running anything), `crates/agentos-engine/src/lib.rs`
- Test: `crates/agentos-engine/tests/worker.rs`

**Interfaces:**
- Produces:
  - `trait Worker { fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = ExecOutcome> + Send; fn reconcile(&self, req: &EffectRequest, ctx: &AttemptCtx) -> impl Future<Output = Reconciliation> + Send; fn current_workspace(&self, task: &TaskId) -> Option<Result<Digest, String>>; }`.
  - `struct HostProcessWorker(FixtureExecutor)` built from `HostConfig` (`inside_worker`).
  - `struct ScriptedWorker { script: String }`: runs `sh -c <script>` in its own group (recorded in `groups`), stdout is the success output; returns `Failure("scripted workers are disabled")` unless the environment variable `AGENTOS_TEST_WORKERS=1` is present in ITS environment (the supervisor passes the environment through; tests set it with `Command::env`, never `set_var`).
  - `async fn run_worker(job: &JobDir) -> io::Result<()>`: reads `request.json`, builds the configured worker, runs the effect, writes `outcome.json`/`outcome.bin` via `JobDir::write_outcome` — NEVER `receipt.json`.
- Consumes: `JobDir`, `JobRequest`, `WorkerConfig` (Task 3).

- [ ] **Step 1: Write failing tests**: `host_worker_matches_fixture_executor_on_a_full_patch_and_verify_run` (same outputs/digests as `FixtureExecutor` directly, for all three kinds, on the real fixtures); `verification_with_a_wrong_pinned_digest_fails_before_running_the_profile` (the check script would write a marker file; assert none, and `passed` never true); `verification_with_the_right_pinned_digest_passes`; `scripted_worker_is_refused_without_the_env_guard` (run via `tokio::process::Command` of a tiny test binary or the supervisor `worker` subcommand with and without `.env("AGENTOS_TEST_WORKERS","1")` — Task 5 provides the binary, so this test lives in Task 5's file; here test the refusal path by calling `ScriptedWorker::run` with the guard absent from the process environment); `run_worker_writes_outcome_and_not_receipt`; `check_children_still_get_their_own_process_group_and_are_recorded` (a verification whose check forks `sleep 30 &`: the recorded pgid in `groups` equals the check's group, the sleep is in that group, and after the run no member of the group survives — the existing `process.rs` tests keep passing unchanged); `a_grandchild_holding_stdout_open_does_not_block_verification_past_the_check_timeout` (regression for the own-group design).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-engine --test worker`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS**, then `cargo test -p agentos-engine` (existing process-group tests unchanged).
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): Worker trait, host-process worker, scripted test worker, worker entry point"`

---

### Task 5: Supervisor logic and the `agentos-supervisor` binary target

**Files:**
- Create: `crates/agentos-engine/src/supervisor.rs`, `crates/agentos-engine/src/bin/agentos-supervisor.rs`
- Modify: `crates/agentos-engine/Cargo.toml` (`[[bin]] name = "agentos-supervisor"`), `crates/agentos-engine/src/lib.rs`
- Test: `crates/agentos-engine/tests/supervisor.rs` (uses `env!("CARGO_BIN_EXE_agentos-supervisor")`)

**Interfaces:**
- Produces:
  - `pub async fn run_supervisor(job: &JobDir, worker_cmd: &SupervisorCmd) -> io::Result<JobState>` where `struct SupervisorCmd { program: PathBuf, prefix_args: Vec<String> }` (the supervisor re-executes `program prefix_args… worker <job_dir>` as the worker; the binary passes `(current_exe, [])`, the CLI passes `(current_exe, ["supervise"])`). Steps: `setsid` (ignore EPERM), `set_child_subreaper`, the lock is already held through inherited stdin (verify with `lock_held()` from another descriptor in tests); write `status Starting{supervisor_pid}`; if `cancel` exists, kill nothing and finish as `Killed{Cancel}` WITHOUT spawning; spawn the worker with `process_group(0)`, stdin/stdout/stderr null (stderr to `supervisor.log` for the supervisor itself), write `status Running{worker_pgid}`; poll every 50 ms (`AGENTOS_SUPERVISOR_POLL_MS` overrides, tests only): worker exited ⇒ read and VALIDATE `outcome.json` (effect id, attempt id and lease generation equal the request's, digest matches) else `Failure("worker produced an invalid outcome")`; write the receipt; kill the worker group and every group in `groups`; reap with `waitpid(-1)` until nothing is left; status `Exited`. `now >= lease_expiry_ms`, `now >= task_deadline_ms` (when non-zero) or a `cancel` marker ⇒ kill groups, then apply the KILL-RECEIPT RULE: valid `outcome.json` present ⇒ publish it; else `Retry` kinds ⇒ `Failure` receipt (`lease expired` / `deadline exceeded` / `cancelled`); `ReconcileThenRetry` kinds ⇒ NO receipt; status `Killed{reason}`. Check order: cancel, deadline, lease. The receipt (when there is one) is always written BEFORE the terminal status.
  - Binary `agentos-supervisor`: `run <job_dir>`, `worker <job_dir>`; exit 0 for any terminal state, 1 for supervisor-internal errors (also logged to `supervisor.log`). Test-only hook: when `AGENTOS_TEST_WORKERS=1` and `AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT=1`, the supervisor exits with code 3 right after reading a valid worker `outcome.json` and before writing `receipt.json` (preserves the Phase 2 "executed but not durable" case).
- Consumes: `JobDir`, `run_worker`, `ExecOutcome::failure`, `EffectKind::retry_policy`.

- [ ] **Step 1: Write failing tests** (binary level; `ScriptedWorker`; `.env("AGENTOS_TEST_WORKERS","1")`; leases of a few hundred ms using real clocks): `normal_exit_writes_receipt_before_terminal_status` (poll: whenever status is `Exited` the receipt is already there); `lease_expiry_kills_a_forked_grandchild_and_its_files_never_appear` (script `(sleep 30; touch marker) & sleep 30`; assert `Killed{Lease}`, a Failure receipt `lease expired` for a ReadSnapshot-kind request, then wait 2 s and assert no `marker`, and that every pid the script recorded is gone — check by reading `/proc/<pid>/stat` state: missing or `Z` counts as gone); `a_verification_check_group_is_killed_with_the_worker` (HostProcessWorker, test profile whose check forks a background child in its own group; lease expiry kills it; marker never appears); `killed_apply_patch_writes_no_receipt_only_killed_status`; `kill_publishes_a_valid_outcome_json_as_the_receipt` (worker wrote its outcome and then hung); `deadline_beats_lease_in_the_reason`; `cancel_wins_over_both`; `cancel_marker_present_before_start_spawns_no_worker`; `cancel_marker_kills_within_500ms`; `normal_exit_reaps_a_background_child` (child would write a marker after 1 s); `invalid_outcome_from_the_worker_becomes_a_failure_receipt` (forged effect id); `supervisor_survives_the_launcher_dying`; `lock_is_held_for_the_whole_life_of_the_supervisor_and_free_after_exit`; `supervisor_sigkill_leaves_running_status_no_receipt_and_a_free_lock` (kill -9 the supervisor mid-job; `is_dead()` is true; the orphaned worker may still run — the controller's fence kills it, tested in Task 7); `exit_before_receipt_hook_leaves_outcome_but_no_receipt`.
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-engine --test supervisor`.
- [ ] **Step 3: Implement.** The launcher hands the locked file over as stdin (Task 6); in tests, create the job with `JobDir::create` and spawn the binary with `Stdio::from(lock_file)`.
- [ ] **Step 4: Run, expect PASS**, then 20 repetitions in one container: `docker compose run --rm test bash -c 'for i in $(seq 20); do cargo test -p agentos-engine --test supervisor || exit 1; done'`.
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): per-job supervisor enforcing lease, deadline and cancel"`

---

### Task 6: `SupervisedExecutor`, `unresolved` outcomes, `deadline_ts` plumbing

**Files:**
- Create: `crates/agentos-engine/src/supervised.rs` (also hosts `ExecCounts`, moved from `durable.rs`)
- Modify: `crates/agentos-engine/src/executor.rs` (`EffectRequest.deadline_ts: i64` in unix seconds, 0 = none; `ExecOutcome.unresolved: bool` with `#[serde(default)]`; `Executor::fence_job` default `async { true }`), `crates/agentos-engine/src/steps.rs` (`request(rec, payload, contract, deadline_ts)`; `execute` handles an unresolved outcome: `db.mark_unknown`, then `Failed{reason: "unreconcilable effect <id>"}`), every other `EffectRequest { .. }` literal and `steps::request` call site (recover.rs, steps.rs tests, `engine/tests/executor.rs`, `engine/tests/crash_matrix.rs` — all in THIS task so the workspace compiles), `crates/agentos-engine/src/lib.rs`
- Test: `crates/agentos-engine/tests/supervised.rs`

**Interfaces:**
- Produces:
  - `struct SupervisedExecutor { jobs_root, supervisor: SupervisorCmd, worker: WorkerConfig, timeouts: EffectTimeouts, counts: ExecCounts, crash: Option<CrashHook>, extra_env: Vec<(String,String)>, reconciler: Option<FixtureExecutor> }`; `SupervisedExecutor::new(jobs_root, supervisor, worker, counts) -> io::Result<Self>`, `.with_timeouts`, `.with_crash`, `.with_env(k, v)`. `reconciler` is `Some` only for `WorkerConfig::Host` (built from its `HostConfig`; read-only, documented as the 3a stand-in until 3b moves the workspace into the guest).
  - `impl Executor for SupervisedExecutor`:
    - `run`: `counts.record`; compute `lease_expiry_ms` (refuse with `Failure("deadline exceeded")` and no job directory when not in the future); `JobDir::create`; spawn `<program> <prefix_args…> run <job_dir>` with stdin = the returned locked file, stdio otherwise null, `extra_env` applied, no wait handle kept; poll `read_receipt()` every 25 ms; bounded by `lease_expiry_ms` + 5 s, after which fence (below). If the hook fires right after the launch (`CrashPoint::DuringExecute`), return immediately with a throwaway failure outcome; the runner discards it because the tripped hook becomes `Err(Crashed)` before the outcome is used (exactly as `DurableExecutor` did). Job dead without a receipt: for `Retry` kinds return `Failure("supervisor died without a receipt")`; for `ReconcileThenRetry` kinds call `reconcile` in process and return `Applied(outcome)`, a truthful `Failure("patch provably not applied")` for `NotApplied`, or an outcome with `unresolved: true` for `Unknown`. Launch failure ⇒ `Failure("supervisor launch failed: …")`.
    - `retained_outcome(effect)`: the highest lease generation job with a valid receipt.
    - `reconcile`/`current_workspace`: delegate to `reconciler` (`Unknown`/`None` if absent).
    - `fence_job(effect) -> bool` (trait method, async): for every attempt that is not dead: drop `cancel`; poll 2 s for death; if still alive SIGKILL the recorded worker groups, `groups`, and `supervisor_pid` (rustix, ESRCH fine) and re-check; returns whether every attempt `is_dead()`.
    - `wait_for_job(effect, bound) -> JobWait { Receipt(ExecOutcome), Dead, StillAlive }` (inherent): waits for a live job until `lease_expiry_ms` + 5 s (or `bound`), used by recovery in Task 7.
- Consumes: `JobDir`, `SupervisorCmd`, `EffectTimeouts`, `lease_expiry_ms`, `WorkerConfig`.

- [ ] **Step 1: Write failing tests** (real supervisor binary through `CARGO_BIN_EXE_agentos-supervisor`, `HostConfig` on the fixtures): `run_matches_fixture_executor_for_all_three_kinds`; `launch_is_refused_when_the_deadline_has_passed_and_creates_no_job_dir`; `retained_outcome_finds_the_highest_lease_receipt`; `a_new_executor_on_the_same_jobs_root_still_finds_receipts_of_jobs_the_old_one_launched`; `killed_verification_returns_the_failure_receipt`; `killed_apply_patch_without_a_receipt_is_reconciled_not_failed` (kill the supervisor after the patch applied via the exit-before-receipt hook: the outcome is the reconciled `Applied` one and the workspace digest matches); `killed_apply_patch_that_provably_did_not_apply_returns_a_truthful_failure`; `unresolvable_apply_patch_returns_an_unresolved_outcome_and_the_runner_marks_it_unknown_and_fails_the_task`; `fence_job_waits_for_a_cooperative_job_and_reports_dead`; `fence_job_kills_a_sigstopped_supervisor_and_its_worker_groups` (SIGSTOP the supervisor with rustix; fence returns true; no process of the job survives — zombie-safe check via `/proc/<pid>/stat`); `wait_for_job_returns_the_receipt_of_a_job_that_finishes_while_waiting`; `launch_failure_is_a_failure_outcome` (bad program path); `counts_record_each_launch`; `a_job_with_only_request_json_is_waited_for_not_redispatched` (lock held, no status yet).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** (`cargo test -p agentos-engine`).
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): SupervisedExecutor, unresolved outcomes, deadline_ts in requests"`

---

### Task 7: Recovery with waiting and fencing; remove `DurableExecutor`; migrate engine crash tests

**Files:**
- Modify: `crates/agentos-engine/src/recover.rs`, `steps.rs`, `crash.rs` (`DuringExecute` doc), `runner.rs`
- Delete: `crates/agentos-engine/src/durable.rs`
- Modify tests: `crates/agentos-engine/tests/{crash_matrix,happy_path,common/mod}.rs`
- Test: new cases in `crates/agentos-engine/tests/crash_matrix.rs`

**Interfaces:**
- Consumes: `SupervisedExecutor::{wait_for_job, fence_job}`, `Executor::fence_job`.
- Behavior (see spec "Liveness and fencing"): for a DISPATCHED/UNKNOWN effect: (1) `retained_outcome` ⇒ publish without re-running; (2) else `wait_for_job` (executors without jobs return `Dead` immediately): `Receipt` ⇒ publish; `StillAlive` ⇒ `fence_job`, and if it returns false ⇒ `mark_unknown` + `Failed{reason: "unreconcilable effect <id>"}` with `RecoveryDecision` `FenceFailed`; (3) after `Dead`, re-read `retained_outcome` once, then decide by retry policy exactly as before (`Retry` ⇒ redispatch lease+1; `ReconcileThenRetry` ⇒ `reconcile`). `RecoveryDecision` journals one new decision value `WaitedForJob` when a wait changed the outcome.
- Migration of the existing crash tests (current line numbers in `crash_matrix.rs`, verify at HEAD; each keeps all its assertions except as stated):
  - `*_during_execute` rows for all three kinds: now `PublishRetained`, ONE execution, lease 1 (was `PublishReconciled` for `apply_patch`, and `Redispatch`/2 executions/lease 2 for the others); `expected_executions` and lease expectations updated.
  - New rows using the exit-before-receipt hook (`.with_env("AGENTOS_TEST_WORKERS","1")`, `.with_env("AGENTOS_TEST_SUPERVISOR_EXIT_BEFORE_RECEIPT","1")`): `patch_supervisor_exits_before_receipt` ⇒ `PublishReconciled`, one patch application; `verification_supervisor_exits_before_receipt` ⇒ `Redispatch`, 2 executions, lease 2. These preserve the Phase 2 "executed but not durable" semantics.
  - Re-base on the exit-before-receipt hook, keeping their assertions: `a_crash_during_recovery_still_converges` (~362-381), `repeated_crashes_of_a_retried_verification_converge` (~385-404), `cancel_after_a_crash_leaves_an_unprovable_verification_unknown` (~528-538), `an_unreconcilable_patch_fails_the_task_and_keeps_the_uncertain_reservation` (~542-567; make it unreconcilable by exit-before-receipt plus tampering the workspace so neither forward nor reverse patch applies).
  - Rewrite to write receipts through `JobDir::write_receipt` instead of `receipts/<effect>-<attempt>.json`: `stale_and_duplicate_receipts_are_ignored_and_audited` (~426-475) and `the_executor_retains_the_highest_lease_outcome` (~714-735).
  - `common/mod.rs` builds a `SupervisedExecutor` (jobs root in the temp dir, `CARGO_BIN_EXE_agentos-supervisor`); `ExecCounts` keeps its meaning (launches).
- New tests: `recovery_waits_for_a_running_job_and_publishes_its_receipt_without_a_second_attempt` (controller "killed" right after launch; slow-but-finishing verification via a test profile; exactly one job directory, lease 1, final SUCCEEDED, usage equals the uncrashed baseline); `recovery_after_a_supervisor_sigkill_redispatches_a_verification_after_fencing_the_orphan_worker` (two job dirs; first dead; no live process of the first); `recovery_with_a_sigstopped_supervisor_fences_then_redispatches`; `recovery_marks_unknown_when_fencing_cannot_free_the_lock` (inject a fence that returns false via a test executor wrapper) ⇒ `FenceFailed`, UNKNOWN, task FAILED, uncertain reservation visible; `no_live_process_remains_after_any_recovery_case` (scan `/proc/*/cmdline` for the temp jobs root); `two_attempts_of_the_same_effect_never_run_concurrently` (instrument the scripted worker to record start/stop times; intervals of the same effect never overlap).
- [ ] **Step 1: Write the failing tests** (migrated expectations first, then the new ones).
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-engine`.
- [ ] **Step 3: Implement**; delete `durable.rs`; update the docs in `recover.rs` ("receipts are job directories").
- [ ] **Step 4: Run, expect PASS** (`cargo test -p agentos-engine`), then the crash matrix 20 times in one container.
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): recovery waits for and fences supervised jobs; DurableExecutor replaced; crash tests migrated"`

---

### Task 8: CLI wiring and CLI crash-test migration

**Files:**
- Create: `crates/agentos-cli/src/commands/supervise.rs` (hidden subcommands `supervise run <job_dir>` and `supervise worker <job_dir>` calling `run_supervisor` / `run_worker`)
- Modify: `crates/agentos-cli/src/{args,commands/mod,home,drive}.rs` (`home.executor(task)` builds a `SupervisedExecutor` with `SupervisorCmd { program: current_exe, prefix_args: ["supervise"] }`, jobs root `<home>/jobs`, `HostConfig` from the task dir with ABSOLUTE paths; `<home>/receipts` is no longer created), `commands/inspect.rs` (`status` gains `jobs`: per outstanding effect its latest job state), `compose.yaml` (`init: true` on the `test` service), `scripts/demo.sh` (nothing else to build: the supervisor is the CLI itself), `crates/agentos-cli/tests/cli.rs`
- Test: `crates/agentos-cli/tests/cli.rs`

**Interfaces:**
- Consumes: Task 6/7 executor and recovery.
- Behavior: the CLI crash demo table keeps all 10 rows; the `during-execute:*` rows now resume to SUCCEEDED by publishing the job's receipt (assert exactly one job directory for that effect and that the manifest equals the uncrashed run's); `normalized()` in the test helpers additionally strips `capabilities[*].handle_prefix` from manifests (Task 12 adds that field; add the stripping now so the order of tasks does not matter).

- [ ] **Step 1: Write failing tests**: migrate the existing 23+ CLI tests' helpers (they build the CLI binary only; the supervisor is the CLI re-exec) and add `supervise_subcommands_are_hidden_from_help`; `during_execute_rows_resume_by_publishing_the_receipt_with_one_job_per_effect`; `controller_sigkill_while_a_slow_verification_runs_then_resume_publishes_the_receipt` (a test profile with a ~3 s check via `--profiles`; `kill -9` of the CLI process while the job runs; the job finishes under its supervisor; `resume` ends SUCCEEDED; exactly one job directory for that effect; no process referencing the jobs dir remains after the final command); `status_shows_job_state_for_outstanding_effects`; `home_has_no_receipts_dir`.
- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-cli`.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** — `docker compose run --rm test cargo test --workspace`.
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): supervised execution through hidden supervise subcommands; crash demo migrated"`

---

### Task 9: Deadline enforcement

**Files:**
- Modify: `crates/agentos-engine/src/runner.rs`, `recover.rs`, `steps.rs`, `crates/agentos-store/src/db.rs` (uses `deadline_passed`, `deadline_ts` from Task 2)
- Test: `crates/agentos-engine/tests/deadline.rs`

**Interfaces:**
- Consumes: `Db::deadline_passed`, `Db::with_clock`, `lease_expiry_ms`, `SupervisedExecutor::wait_for_job`.
- Behavior: checked before each agent turn, before `intend`, before `dispatch`. When passed: wait for or fence live jobs and publish their receipts, append `Failed{reason: "deadline exceeded"}`, then run recovery in closing mode (publish, reconcile, mark unknown or abandon; dispatch nothing). A pending cancel wins (CANCELLED). `SupervisedExecutor::run` already refuses to launch with an expired lease.

- [ ] **Step 1: Write failing tests** (injected `Db::with_clock` for the controller side; real short deadlines for supervisor-enforced cases — all lease/deadline arithmetic in milliseconds): `deadline_between_turns_fails_the_task_without_new_effects`; `deadline_during_a_verification_kills_the_job_and_fails_the_task` (slow test-profile check, `deadline_seconds` 2: task FAILED `deadline exceeded`, the verification effect FAILED with `deadline exceeded`, no `Reserved` usage, no live process); `deadline_during_apply_patch_reconciles_before_failing` (patch applied, supervisor killed at the deadline without a receipt ⇒ the effect is published via reconcile, workspace digest matches, THEN the task fails); `deadline_expired_before_recovery_runs_still_reconciles_then_fails_and_dispatches_nothing` (assert no job directory is created after the deadline); `cancel_wins_over_deadline`; `unapproved_task_has_no_deadline_and_a_late_approval_starts_a_fresh_one` (deadline 0 before approval, `now + deadline_seconds` after).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** (`cargo test --workspace`).
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): enforce the task deadline"`

---

### Task 10: Revocation and cancellation of running jobs

**Files:**
- Create: `crates/agentos-cli/src/commands/revoke.rs`
- Modify: `crates/agentos-cli/src/{args,commands/mod,commands/control}.rs`, `crates/agentos-engine/src/supervised.rs` (`cancel_jobs(effects)` — drops markers only), `crates/agentos-engine/src/recover.rs` and `runner.rs` (a `CapabilityDenied` at dispatch or at recovery redispatch ⇒ `Failed{reason: "capability revoked: <op>"}`, then recovery in closing mode: abandon INTENDED effects, reconcile/mark unknown DISPATCHED ones)
- Test: `crates/agentos-cli/tests/cli.rs`, `crates/agentos-engine/tests/revoke.rs`

**Interfaces:**
- Produces: `agentos revoke TASK_ID [--capability NAME]` (names as in the contract, e.g. `verification.run`; unknown name ⇒ exit 2): `Db::revoke`, then drop `cancel` markers for running jobs whose kind's capability was revoked; prints `{"revoked":[...],"cancelled_jobs":N}`. `cancel` (existing) also drops markers for all outstanding jobs before reconciling.
- Consumes: `Db::revoke`, `JobDir::{drop_cancel, list}`.

- [ ] **Step 1: Write failing tests**: `revoke_verification_run_kills_the_running_check_and_the_next_request_is_denied_revoked_while_results_stay_in_the_journal`; `revoke_one_capability_keeps_other_running_effects`; `an_intent_authorized_before_a_revoke_is_denied_at_dispatch` (record the intent, revoke, then dispatch ⇒ `CapabilityDenied` reason `revoked`, no job directory, task FAILED `capability revoked`); `recovery_redispatch_of_a_revoked_capability_closes_the_task_instead_of_launching` (crash after a verification job died without a receipt, revoke, resume ⇒ no second job, task FAILED); `revoke_unknown_capability_name_exits_2`; `revoke_on_a_terminal_task_is_allowed_and_cancels_nothing`; `cancel_drops_markers_for_running_jobs_and_ends_cancelled_with_no_live_process`; `revoked_snapshot_read_stops_a_resumed_task_with_failed_not_stuck`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS** (`cargo test --workspace`).
- [ ] **Step 5: Commit** — `git commit -am "feat(cli,engine): revoke capabilities and cancel running jobs; revocation cannot be bypassed"`

---

### Task 11: Profile registry and digest pinning

**Files:**
- Create: `crates/agentos-cli/src/commands/profile.rs`
- Modify: `crates/agentos-cli/src/{args,home,commands/submit}.rs`
- Test: `crates/agentos-cli/tests/cli.rs`, `crates/agentos-engine/tests/worker.rs`

**Interfaces:**
- Produces: `agentos profile register DIR` ⇒ validates `profile.json` (`id` plain name without `@`, `command` non-empty), copies to `<home>/registry/<id>@<digest>/` (recursively read-only), writes `<id>@<digest>.meta.json` beside it, prints `{"id","digest"}`; same bytes twice is a no-op; `agentos profile list`. `submit` resolution order (spec "Profile registry"): `profile_digest` pin ⇒ exact registry entry else exit 2; newest registry entry for the id; legacy `<profiles>/<id>/` as an unpinned entry with its digest computed at submit. The chosen digest goes into `Submitted` and `HostConfig.profile_digest`.
- Consumes: `Contract.profile_digest`, `HostConfig.profile_digest`.

- [ ] **Step 1: Write failing tests**: `register_twice_is_a_noop_and_changed_bytes_are_a_new_entry`; `registered_entries_have_no_write_bits` (assert the mode bits, not a failed write: tests run as root); `submit_with_a_pin_for_a_missing_digest_exits_2`; `submit_with_the_pin_uses_exactly_that_digest_even_if_a_newer_entry_exists`; `legacy_profiles_dir_still_works_with_the_profiles_flag`; `registry_wins_over_legacy_when_both_exist`; `ids_with_at_sign_or_traversal_are_rejected_at_register`; `registered_profile_runs_end_to_end` (the demo flow ⇒ SUCCEEDED); at engine level `profile_bytes_changed_after_hostconfig_is_built_voids_the_evidence` (modify `profile_dir` after the pinned digest was taken ⇒ verification effect Failure `profile digest mismatch`, never `passed`; through the CLI the same tampering is caught earlier by `check_inputs` — add `cli_tampered_staged_profile_fails_the_task_before_any_verification`).
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): profile registry and digest pinning"`

---

### Task 12: Export authorization, capability status, README, final verification

**Files:**
- Modify: `crates/agentos-cli/src/commands/{export,inspect}.rs`, `crates/agentos-engine/src/export.rs` (manifest gains `capabilities: [{operation, handle_prefix, revoked}]`), `README.md`, `scripts/demo.sh`
- Test: `crates/agentos-cli/tests/cli.rs`

**Interfaces:**
- Consumes: `Db::grants`, `authorize_in` (Task 2). Produces: `Db::authorize(&self, task: &TaskId, op: Capability, resource: &Resource) -> Result<Handle, DbError>` — journaled, in its own write transaction, reusing `authorize_in`; for callers outside `record_intent`/`mark_dispatched` (export).
- Behavior: `agentos export` authorizes `ArtifactExport` on `Resource::Task` first and journals the decision; denial ⇒ exit 1 with the reason, nothing written. `status` prints `capabilities` (operation, prefix, revoked, expires). README: rewrite "Known limits" (drop the orphan, deadline, profile-pinning and export-capability items; keep same-UID host execution, model stand-in, single-driver, same-patch resume; add job-directory GC, `setsid` escapes, `reconcile` still runs in the controller process, `Denied` rows forgeable via audit; add the 3b list), document `revoke`, `profile register|list`, the job directory layout and a real SIGKILL-the-controller transcript from `scripts/demo.sh`.

- [ ] **Step 1: Write failing tests**: `export_without_artifact_export_capability_exits_1_writes_nothing_and_journals_the_denial` (reason `unknown_handle`); `export_after_revoking_artifact_export_is_denied_revoked`; `export_journals_the_granted_decision`; `a_task_failed_on_its_deadline_can_still_be_exported`; `manifest_lists_capabilities_with_prefixes_only` (read the real handles from the database and assert none occurs anywhere in the bundle or in `status`/`events` output); `status_lists_capabilities_without_full_handles`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement**; regenerate the README transcript by running `scripts/demo.sh` under docker compose and pasting real output.
- [ ] **Step 4: Final verification:** `docker compose run --rm test cargo test --workspace` (all green, no warnings); the supervisor and crash-matrix suites 20 times each in one container; `cargo clippy --workspace --all-targets` and report any NEW warnings.
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): broker-authorized export, capability status, README for Phase 3a"`

---

## Follow-on

Phase 3b (separate spec and plan): Firecracker worker behind `Worker`, guest image build and registration, per-VM vCPU/memory limits, no guest network, no host secrets, moving the workspace (and `reconcile`/`current_workspace`) into the guest, passing handles to the guest. Needs a KVM check and the Firecracker v1.17.0 binary (latest as of 2026-10-02).

## Self-Review

- **Spec coverage:** supervisor shape, job directory, lock liveness, kill-receipt rule, fencing and waiting (T3, T5, T6, T7); lease/deadline rules and deadline-at-approval (T1, T2, T5, T9); broker handles/authorize/journaling/revocation/re-authorization at dispatch (T1, T2, T10); export authorization and unexpiring export handle (T2, T12); profile registry layout, resolution and pinning (T4, T11); schema v2 and reserved names (T2); README (T12); the test list in the spec is distributed across T3–T12 (grandchild and check-group kill T5, controller SIGKILL T8, redispatch/stale receipt T7, broker denials T2, revoke mid-job T10, deadline T9, tamper T4/T11).
- **Deviations from the approved spec text (surface to the human):** (1) recovery WAITS for a live job instead of fencing it (the earlier text contradicted the spec's own "a running job may finish while the controller is down"); (2) the supervisor is a `[[bin]]` of `agentos-engine`, re-executed by the CLI as `supervise`, not a separate crate; (3) `deadline_ts` is set at approval; (4) `artifact.export` handles have no expiry; (5) `snapshot.read` is scoped to the task; (6) liveness is the supervisor's `flock`, not pids, and the container gets `init: true`; (7) the store resolves handles itself in 3a (nothing presents one), so journaling prefixes is acceptable; (8) times in job files are milliseconds; (9) `ScriptedWorker` ships in the engine behind an env-var guard, passed with `Command::env`.
- **Known 3a limits kept for the README:** host-process workers are same-UID; `reconcile`/`current_workspace` run in the controller process; a descendant that calls `setsid` escapes the group kill; job directories are never garbage-collected; `Denied` audit rows remain forgeable through `append_audit`.
