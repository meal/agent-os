# Phase 3a: capability broker, per-job supervisor and leases — design

Date: 2026-10-02
Status: proposed; approved in conversation section by section, awaiting written-spec review.
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
heartbeat leases; a long-lived supervisor daemon; multi-owner or remote identity.

Assumptions: x86-64 Linux, one local owner, one active controller (`driver.lock`), the fixture
Python profile only. Builds and tests run through `docker compose`; 3a needs no KVM.

## Architecture

New and changed units. The controller, journal, reducer and recovery logic keep their shape.

| Unit | Kind | Responsibility |
| --- | --- | --- |
| `agentos-supervisor` | new binary crate | Owns one worker for one job; enforces the lease and deadline; writes the status and the durable receipt; exits. |
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
| `request.json` | controller, before launch | `EffectRequest` fields (effect id, task id, kind, payload, contract), attempt id, lease generation, lease expiry (unix seconds), task deadline. |
| `status.json` | supervisor, atomic (temp, fsync, rename) | `state`: `Starting`, `Running`, `Exited`, `Killed`; for `Killed` a `reason` of `lease`, `deadline` or `cancel`; `pid`, `lease_expiry`, timestamps. |
| `receipt.json`, `output.bin` | supervisor | Same format and durability rules as the Phase 2 `DurableExecutor` receipt (temp file, fsync, rename, fsync of the directory), written before `status.json` becomes terminal. |
| `cancel` | controller | Empty marker file. The supervisor polls for it (every 100 ms) and kills the worker's process group. |

A torn or unparseable `status.json` counts as no status; if a valid receipt exists the status is
repaired from it. A leftover `.tmp` file is ignored.

### Lease and deadline rules

- Lease expiry = `now + min(effect_timeout, task_deadline − now)`. `effect_timeout` is 60 s for
  verification and 30 s for the other effects (constants in the engine, overridable in tests).
- The supervisor kills the worker's whole process group (SIGKILL via rustix, no `unsafe`) on
  lease expiry, on deadline, on a `cancel` marker, and after the worker exits normally (to reap
  stragglers). This closes the orphaned-verification limit for the host-process worker.
- No heartbeat: a running job may finish while the controller is down; its receipt then waits for
  recovery. The lease is a bound, not a liveness signal.
- A new attempt gets a strictly higher lease generation (existing store rule). Before redispatch the
  controller fences the old attempt: it drops `cancel` in the old job directory and waits, bounded
  (10 s), for a terminal `status.json`. If the supervisor is gone (host reboot) and the status is
  stale, the attempt is dead once its `lease_expiry` has passed. If neither holds, the effect is
  marked UNKNOWN (existing path) and the task fails with `unreconcilable effect`.
- Late or duplicate receipts remain filtered by `accept_receipt` (effect identity and lease generation).

### Task deadline

The controller checks `deadline_ts` before each agent turn, intent and dispatch. When it has passed
it appends `Failed{reason: "deadline exceeded"}` (existing event, no reducer change) after
reconciling in-flight effects, so no worker keeps running past it (the supervisor already kills
jobs at the deadline). Cancellation keeps the Phase 2 meaning: CANCELLED only after active work is
stopped or reconciled.

## Capability broker

Handles are opaque 128-bit random ids (hex), stored in the existing `capabilities` table
(`id`, `task_id`, `resource`, `operation`, `expires_ts`, `revoked`); no new columns beyond a
`scope` JSON text column and a `created_ts`. The schema version (`PRAGMA user_version`) bumps to 2;
version 1 databases are refused with a clear error (pre-release, no migration).

- **Issue:** at owner approval (`submit --yes`, or `resume` on a READY task) one handle per
  capability in the contract. Scope: `snapshot.read` and `workspace.apply_patch` = the contract's
  `editable_paths`; `verification.run` = the pinned profile id; `artifact.export` = the task.
  Expiry = `deadline_ts`.
- **Authorize:** `authorize(task, handle, operation, resource) -> Decision` checks that the handle
  exists and belongs to the task, matches the operation, covers the resource, is unexpired and
  unrevoked. Budgets keep their existing enforcement (reducer and `record_intent`); the broker
  calls them but does not duplicate them. Every decision, granted or denied, is journaled as an
  engine-owned event (`CapabilityGranted` / `CapabilityDenied` with the handle id, operation,
  resource and reason). The agent never sees handles, only the resulting observation.
- **Call sites:** the runner and `record_intent` use `authorize` instead of the contract list. The
  existing `Denied` audit rows and observations keep their meaning and wording.
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

- `agentos profile register DIR` validates `profile.json`, copies the directory into
  `<home>/profiles/<id>@<digest>/` (digest = existing `workspace_digest` rules), makes it
  read-only, and prints `{id, digest}`. Registering the same bytes twice is a no-op; a different
  digest under the same id is a new entry.
- A contract names `verification_profile` (existing validated id) and an optional `profile_digest`
  field. `submit` resolves the id to the newest registered entry (or the pinned digest), fails with
  exit 2 if none matches, and records the digest in `Submitted`. The `--profiles DIR` flag keeps
  working as a registry root for tests.
- The worker re-digests its staged copy against the pinned digest before every verification; a
  mismatch fails the effect and voids the evidence (existing "never passed=true" rule).

## Recovery changes

`recover` keeps its structure and decisions; "retained outcome" and "is the old attempt dead"
now come from job directories:

- job with a valid receipt: publish without re-running (unchanged);
- job `Running` with no receipt: fence (above), re-read, then decide by retry policy as before;
- job `Starting` and stale, or missing: treated as never started (attempt dead) if its lease
  expiry has passed;
- the `DurableExecutor` receipt directory is removed; the crash-matrix tests are migrated to the
  supervised executor rather than keeping both mechanisms.

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
- The Phase 2 crash matrix and CLI crash demo table pass unchanged in meaning against the
  supervised executor (same normalized end-state comparison).

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
