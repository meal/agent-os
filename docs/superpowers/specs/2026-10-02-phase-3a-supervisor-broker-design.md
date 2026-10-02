# Phase 3a: capability broker, per-job supervisor and leases — design

Date: 2026-10-02
Status: revised after an adversarial plan review (2026-10-02); revisions marked **[rev]** change text the owner approved and need re-approval.
Parent spec: `Agent_OS_v1_Build_Plan.md` (Phase 3, "Authority and isolation").
Builds on: `docs/superpowers/plans/2026-10-01-agent-os-phases-1-2.md` (merged to `main`).

## Purpose and scope

Phase 3 of the build plan makes repository code run under bounded authority. It is split in two:

- **3a (this document):** the capability broker, per-job supervisor processes, leases, deadline,
  cancellation and revocation, and profile registration. Workers are still host processes, so
  no VM, guest image, network or secret isolation yet.
- **3b (separate spec and plan):** a Firecracker worker behind the same `Worker` trait, the guest
  image, per-VM CPU and memory limits, no guest network, no host secrets.

3a closes these documented v0.1 limits: orphaned verification process after a controller kill,
`deadline_ts` never enforced, verification profile digest not pinned, export not capability-checked,
capability revocation not covered, lease fencing for out-of-process workers.

Out of scope for 3a (non-goals): Firecracker and guest images; network and host-secret isolation;
per-worker CPU/memory limits; the model broker (Phase 4); making export a full journaled effect
(effects cannot start on a terminal task, so export stays an authorized, audited action);
heartbeat leases; a long-lived supervisor daemon; multi-owner or remote identity; garbage collection of old job directories (a known limit listed in the README).

Assumptions: x86-64 Linux, one local owner, one active controller (`driver.lock`), the fixture
Python profile only. Builds and tests run through `docker compose`; 3a needs no KVM.

## Architecture

New and changed units. The controller, journal, reducer and recovery logic keep their shape.

| Unit | Kind | Responsibility |
| --- | --- | --- |
| `agentos-supervisor` | new binary target `src/bin/agentos-supervisor.rs` in `agentos-engine` **[rev]** (engine integration tests get `CARGO_BIN_EXE_agentos-supervisor`, so no test runs a nested `cargo build`); the `agentos` CLI re-executes itself through hidden `supervise run|worker` subcommands, so no second installed binary is needed | Owns one worker for one job; enforces the lease and deadline; writes the status and the durable receipt; exits. |
| `SupervisedExecutor<W>` | new, `agentos-engine` | Controller-side client implementing the existing `Executor` trait; launches supervisors, reads job directories; replaces `DurableExecutor`. |
| `Worker` trait | new, `agentos-engine` | What runs one effect attempt inside a supervisor. 3a: `HostProcessWorker` (the current `FixtureExecutor` logic). 3b: a Firecracker worker. |
| broker | new module in `agentos-core` (pure rules) and `agentos-store` (persistence) | Issues opaque capability handles, authorizes requests, journals decisions, revokes. |
| profile registry | `agentos-cli` + `agentos-store` | Content-addressed, read-only profile registry; digest pinning. |

The supervisor is a per-job process, not a daemon. It is launched detached in its own session
(setsid) by the controller for each effect, so killing the controller does not kill it.

### Job directory

`<home>/jobs/<effect_id>-<attempt_id>/` (names are validated as before: effect ids are hex,
attempt ids are UUIDs):

| File | Writer | Content |
| --- | --- | --- |
| `request.json` | controller, before launch | `EffectRequest` fields (effect id, task id, kind, payload, contract), attempt id, lease generation, `lease_expiry_ms` and `task_deadline_ms` (unix milliseconds) **[rev: ms; the expiry lives in the request so a job without a status still has a lease]**, worker configuration (absolute paths only). |
| `status.json` | supervisor, atomic (temp, fsync, rename) | `state`: `Starting`, `Running`, `Exited`, `Killed`; for `Killed` a `reason` of `lease`, `deadline` or `cancel`; `supervisor_pid`, `worker_pgid`, timestamps. |
| `receipt.json`, `output.bin` | supervisor | Same format and durability rules as the Phase 2 `DurableExecutor` receipt (temp file, fsync, rename, fsync of the directory), written before `status.json` becomes terminal. |
| `cancel` | controller | Empty marker file. The supervisor polls for it (every 50 ms), checks it once more before spawning the worker, and kills the worker's process groups. |
| `lock` | controller creates and `flock`s it before launching; the supervisor inherits the locked descriptor as its stdin **[rev]** | Liveness: held from job creation until the supervisor exits, so "nobody holds the lock" means no supervisor lives. Pids are never used for liveness. |
| `groups` | the worker (append-only, one pgid per line) | Process groups the worker created (a verification check runs in its own group); the supervisor kills them with the worker's own group. |
| `outcome.json` | the worker | The worker's result; only the supervisor turns it into `receipt.json`, after validating it. |
| `supervisor.log` | the supervisor | Diagnostics; supervisor and worker stdio are otherwise null. |

A torn or unparseable `status.json` counts as no status; if a valid receipt exists the status is
repaired from it. A leftover `.tmp` file is ignored.

### Lease and deadline rules

- Lease expiry = `now + min(effect_timeout, task_deadline − now)` in milliseconds. `effect_timeout` is 70 s for verification (the fixture's own 60 s check timeout plus a 10 s margin, so the check's own "timeout" reason normally wins and the lease is the backstop) and 30 s for the other effects (constants in the engine, overridable in tests). A launch is refused (a `Failure("deadline exceeded")` outcome, no job directory) when the computed expiry is not in the future.
- The supervisor makes itself a child subreaper (rustix `set_child_subreaper`, no `unsafe`) and reaps with `waitpid(-1)`, so no zombie outlives it. It SIGKILLs the worker's process group and every group listed in `groups` on lease expiry, deadline, a `cancel` marker, and after a normal worker exit. Checks run in the order cancel, deadline, lease, so the recorded reason is deterministic when several apply.
- **[rev] Kill receipts.** When the supervisor kills: (1) if the worker already wrote a valid `outcome.json`, that outcome is published as the receipt (the work finished); (2) otherwise, for kinds with retry policy `Retry` (`ReadSnapshot`, `RunVerification`) it writes a `Failure` receipt carrying the reason (`lease expired`, `deadline exceeded`, `cancelled`), which is truthful because those effects leave no lasting change to the workspace; (3) for kinds with `ReconcileThenRetry` (`ApplyPatch`, `ExportBundle`) it writes NO receipt, only the terminal `Killed{reason}` status, because the patch may or may not have been applied. A receipt-less killed or dead job of such a kind is routed to `reconcile`, never recorded as Failed. The live path follows the same rule: `SupervisedExecutor::run` reconciles it in process and returns the reconciled outcome, a truthful `Failure` if the patch provably did not apply, or an outcome flagged `unresolved` when reconciliation cannot tell; the runner then marks the effect UNKNOWN and fails the task `unreconcilable effect`.
- No heartbeat: a running job may finish while the controller is down; its receipt then waits for recovery. The lease is a bound, not a liveness signal.
- A new attempt gets a strictly higher lease generation (existing store rule).

### Liveness and fencing **[rev]**

- A job is **dead** if and only if its status is terminal, or a valid receipt exists, or no process holds its `lock` (`try_lock` succeeds). Process ids are not used for liveness: a pid can be reused, and a zombie still answers `kill(pid, 0)`. `compose.yaml` gains `init: true` so the container's PID 1 reaps orphans.
- Recovery never kills a job still inside its lease: it **waits** for a job whose lock is held, bounded by that job's `lease_expiry_ms` plus a 5 s grace, then re-reads the receipt. (This replaces the earlier "Running with no receipt: fence", which would have cancelled a running job that the spec says may finish while the controller is down.)
- **Fence** runs only for a job still alive after that bound, or when the owner cancels or revokes: drop `cancel`; after 2 s, if the lock is still held, the controller SIGKILLs the recorded worker groups and `supervisor_pid` (a stopped supervisor cannot kill anything); the job is dead once the lock is free. If the lock is somehow still held after that, the effect is marked UNKNOWN and the task fails `unreconcilable effect`.
- The live path of `SupervisedExecutor::run` is bounded the same way (lease plus grace, then fence). It never returns a `Failure` for a job that merely vanished; it follows the kill-receipt rule above.
- Late or duplicate receipts remain filtered by `accept_receipt` (effect identity and lease generation).

### Task deadline

**[rev]** `deadline_ts` is set at owner approval (`approve_task`: `now + deadline_seconds`; `0` means not yet started and is never "passed"), not at task creation, so a task submitted without `--yes` and approved later does not start expired. The controller checks it before each agent turn, intent and dispatch. When it has passed, live jobs are waited for or fenced and their receipts published, then `Failed{reason: "deadline exceeded"}` is appended (existing event, no reducer change), and recovery runs in its closing mode (publish retained receipts, reconcile, mark unknown or abandon; dispatch nothing). Cancellation keeps the Phase 2 meaning: CANCELLED only after active work is stopped or reconciled, and a pending cancel wins over the deadline.

## Capability broker

Handles are opaque 128-bit random ids (hex), stored in the existing `capabilities` table
(`id`, `task_id`, `resource`, `operation`, `expires_ts`, `revoked`); no new columns beyond a
`scope` JSON text column and a `created_ts`. The schema version (`PRAGMA user_version`) bumps to 2;
version 1 databases are refused with a clear error (pre-release, no migration).

- **Issue:** at owner approval (`submit --yes`, or `resume` on a READY task) one handle per
  capability in the contract. Scope: `snapshot.read` and `workspace.apply_patch` = the contract's
  `editable_paths`; `verification.run` = the pinned profile id; `artifact.export` = the task.
  Expiry = `deadline_ts` for every capability except `artifact.export`, which has no expiry (a task that failed on its deadline, or finished before it, must stay exportable) but stays revocable.
- **Authorize:** the pure broker rule `authorize(grant, task, operation, resource, now)` checks that the handle
  exists and belongs to the task, matches the operation, covers the resource, is unexpired and
  unrevoked. Budgets keep their existing enforcement (reducer and `record_intent`); the broker
  calls them but does not duplicate them. Every decision, granted or denied, is journaled as an
  engine-owned event (`CapabilityGranted` / `CapabilityDenied` with the handle id, operation,
  resource and reason). The agent never sees handles, only the resulting observation. **[rev]** The store resolves the task's handle for an operation itself (the engine holds handles on the task's behalf), so no caller presents a handle in 3a; the wrong-task, wrong-operation and unknown-handle denials guard the broker rule and become reachable when 3b hands a handle to the guest. Journaling the 8-character prefix is acceptable for that reason: a handle is not a bearer secret in 3a. A pure, non-journaling `Db::check` serves the runner's pre-checks; only the decision taken inside `record_intent` (once per new effect) and `mark_dispatched` is journaled.
- **Call sites:** the runner and `record_intent` use `authorize` instead of the contract list. The
  existing `Denied` audit rows and observations keep their meaning and wording.
- **Re-authorization at dispatch [rev]:** `mark_dispatched` re-checks the handle (revoked or expired) in its transaction and journals a denial; a denial there, or in recovery's redispatch path, closes the task (`Failed{reason: "capability revoked: <operation>"}`) and recovery runs in closing mode, so a revocation cannot be bypassed by an intent authorized before it.
- **Revoke:** `agentos revoke TASK_ID [--capability NAME]` sets `revoked` on the matching handles
  (all if none named) and journals `CapabilityRevoked`. New requests fail from the next
  `authorize`. If a job for that capability is running, the controller drops its `cancel` marker;
  the effect then completes as failed or UNKNOWN through the normal receipt and recovery path.
  Existing results remain in the journal.
- **Export:** `agentos export` calls `authorize(.., ArtifactExport, task)` first and journals the
  decision; denial exits 1 with the reason and writes nothing.
- **Reserved names:** the new engine-owned event names join the reserved list that
  `append_audit` refuses.

## Profile registry

- `agentos profile register DIR` validates `profile.json`, copies the directory into `<home>/registry/<id>@<digest>/` (digest = existing `workspace_digest` rules), makes it read-only, and writes `<home>/registry/<id>@<digest>.meta.json` (registration time, outside the digest). Registering the same bytes twice is a no-op; a different digest under the same id is a new entry. Verification profile ids may not contain `@`. **[rev: registry layout and resolution rules]**
- Resolution at `submit`, in order: (1) the contract's `profile_digest` pin ⇒ the registry entry with exactly that digest, else exit 2; (2) the newest registry entry for the id; (3) a legacy `<profiles>/<id>/` directory (the Phase 2 layout, still used by `--profiles DIR` in tests) as an unpinned entry whose digest is computed at submit. The chosen digest is recorded in `Submitted` and given to the worker as the pin.
- `profile_digest` is serialized only when present (`skip_serializing_if`), so existing contract digests do not change.
- The worker re-digests its staged copy against the pinned digest before every verification; a mismatch fails the effect and voids the evidence. Through the CLI the existing `check_inputs` also fails the task earlier on a changed staged copy; the worker check is defense in depth and is tested at the engine level.

## Recovery changes

`recover` keeps its structure; "retained outcome" and "is the old attempt dead" now come from job directories and the lock:

- job with a valid receipt: publish without re-running (unchanged);
- job alive (lock held): wait, bounded, then re-read; fence only past the bound (see Liveness and fencing);
- job dead without a receipt: decide by retry policy as before (`Retry` ⇒ redispatch with lease+1; `ReconcileThenRetry` ⇒ `reconcile`; unreconcilable ⇒ UNKNOWN and a failed task);
- redispatch passes through the broker (a revoked capability closes the task);
- the `DurableExecutor` receipt directory is removed; the crash-matrix and CLI crash tests are migrated to the supervised executor (the migration list is in the plan). `CrashPoint::DuringExecute` now means "the controller dies after launching the job"; the Phase 2 "executed but not durable" case is preserved by a test hook that makes the supervisor exit after `outcome.json` is written and before `receipt.json`.

## Errors

A supervisor that fails to launch fails the effect with `Failure("supervisor launch failed: ...")`.
A worker killed for lease, deadline or cancel yields a `Failure` receipt carrying the reason. A
supervisor crash with no receipt leaves the job `Running` or `Starting`; recovery handles it by the
fencing rules. Unknown or unreconcilable cases still end in UNKNOWN plus a failed task.

## Testing

All tests run under `docker compose` and need no KVM. The real `agentos-supervisor` binary is used
with fake and real workers (sleep, forking grandchild, slow verification).

- Supervisor: lease expiry kills the process group including a forked grandchild; deadline kill;
  cancel marker kill; normal exit reaps stragglers; status transitions; torn status repaired from
  receipt; receipt written before terminal status.
- Controller kill (real process death) while a job runs: the job finishes, `resume` publishes its
  receipt without re-running; the orphaned-verification scenario from the Phase 2 limits no longer
  leaves a live process.
- Redispatch after a lease expiry: higher generation, old receipt ignored and audited.
- Broker: handle issue; authorize grant and each denial reason (unknown, wrong task, wrong
  operation, out of scope, expired, revoked); revoke mid-job kills the job and later requests are
  denied while earlier results remain; export denied without `artifact.export`; no handle secret
  appears in the journal or exports.
- Deadline: expiry between turns fails the task with `deadline exceeded`; expiry during a job kills
  it and the task fails after reconciliation.
- Profile registry: register, dedupe, pin by digest, tampered staged profile voids evidence,
  traversal ids still rejected.
- The Phase 2 crash matrix and CLI crash demo table are migrated to the supervised executor and keep their normalized end-state comparison; expected recovery decisions change as listed in the plan (the `during-execute` rows now publish a retained receipt with one execution and lease 1).
- Kill-receipt rule: a killed `ApplyPatch` never produces a Failure receipt (reconciled instead); a killed verification does; a valid `outcome.json` present at kill time is published.
- Liveness: a SIGSTOPped supervisor is fenced by the controller; no zombie keeps a job "alive"; a job directory with `request.json` but no `status.json` counts as alive while its lock is held.

## Acceptance for 3a

The build plan's Phase 3 rows that 3a can satisfy: forbidden effects are denied and recorded;
capability revoked during execution (new requests fail, active jobs stopped where supported,
results auditable); task cancelled or past its deadline (new work stops, supervisor terminates
active execution, outcome reported); duplicate or late receipt after redispatch. Rows left to 3b:
"guest attempts network access or host secret access" and "repository code stays within configured
CPU and memory".

## Updates to the README

Known limits are rewritten: the orphan, deadline, profile-pinning and export-capability items are
removed or narrowed; same-UID host execution remains until 3b.
