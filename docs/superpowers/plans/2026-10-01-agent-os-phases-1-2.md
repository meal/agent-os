# Agent OS v0.1 — Phases 1–2 (Contracts, Execution Model, Durable Execution) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the first milestone: a task with stable identity that runs a deterministic fake agent against a fixture repo, survives forced controller termination at every persistence boundary, and exports a patch plus evidence.

**Architecture:** Rust workspace. `agentos-core` is a pure library: contract types, task reducer, effect model, with no I/O. `agentos-store` holds SQLite (task state + event in one transaction) and the BLAKE3 blob store. `agentos-engine` runs the effect protocol (intent, dispatch, receipt, reconcile) against an `Executor` trait, so the VM and Wasm backends in later plans plug in unchanged. `agentos-cli` is a thin clap front end.

**Tech Stack:** Rust 1.98 (rustc installed: 1.98.1), Tokio 1.53, Serde 1.0 / serde_json, rusqlite 0.40 (bundled SQLite), blake3 1.8, clap 4.6, thiserror 2, tracing 0.1, jsonschema 0.58, uuid 1.26, tempfile 3.27, proptest 1.11. (Versions checked with `cargo search` on 2026-10-01; re-check before pinning.) All builds and tests run through `docker compose` per user preference.

**Spec:** `Agent_OS_v1_Build_Plan.md`

**Scope note:** The spec covers six phases and several independent subsystems (Firecracker, Wasmtime, real model adapter). This plan covers Phases 1–2 only, which is the spec's "first milestone". Phases 3–6 each get their own plan once this lands. The spec itself says to re-estimate after the first two milestones, so planning them now would be speculative. See "Follow-on plans" at the end.

## Global Constraints

- Task states are exactly: READY, RUNNING, WAITING, PAUSED, VERIFYING, SUCCEEDED, FAILED, CANCELLED.
- SUCCEEDED requires verification evidence attached to the final workspace revision.
- Cancellation records a request, blocks new effects, and records CANCELLED only after active work has stopped or been reconciled.
- Commit the task state update and its event in one SQLite transaction. Task history lives in a separate application `events` table, not the WAL.
- Publish artifact bytes durably before committing metadata that references them. Blob IDs are BLAKE3 digests.
- Supported effects: read snapshot, apply patch against an expected base version, run registered test job, export immutable result bundle.
- Replay never reissues completed effects. Outstanding operations may become UNKNOWN. No global exactly-once claim.
- An uncertain model request retains its budget reservation.
- Workspace updates use expected-version checks. The agent cannot alter the trusted verification profile.
- Contract is schema-validated JSON with the fields shown in the spec's illustrative contract.
- No co-author trailers on commits (user's CLAUDE.md). Use docker compose. Write tests and verify.

## Review Focus

- Contract JSON with unknown fields, empty `editable_paths`, zero limits, or `..`/absolute paths in `editable_paths`: rejected with a clear error, never silently accepted.
- Duplicate `submit` of an identical contract: yields a new task ID, not a corrupted or merged one.
- Two events appended concurrently for one task: sequence numbers stay gapless and unique per task.
- Crash after blob write but before metadata commit: a blob exists with no reference. Restart must not treat it as a result, and GC reclaims it.
- Crash between "intent persisted" and "dispatch": recovery finds an effect in INTENDED and handles it within the original limits and budget.
- A late or duplicate receipt carrying a stale lease generation: ignored and recorded, state unchanged.
- Patch whose paths fall outside `editable_paths` (including via `..` or a symlink): denied and logged.
- `export` on a non-terminal task: refused, no partial bundle left behind.

---

## File Structure

```
Cargo.toml                         workspace
rust-toolchain.toml                pin stable
Dockerfile, compose.yaml           build/test in containers
crates/agentos-core/src/
  lib.rs
  contract.rs                      Contract, Limits, validation
  state.rs                         TaskState, TaskEvent, Task, reduce()
  effect.rs                        EffectKind, EffectState, EffectId derivation
  budget.rs                        Reservation ledger (pure)
crates/agentos-store/src/
  lib.rs
  blob.rs                          BlobStore (BLAKE3, atomic publish, gc)
  db.rs                            Db: schema, task/event/effect/usage tables
crates/agentos-engine/src/
  lib.rs
  executor.rs                      Executor trait + FixtureExecutor
  agent.rs                         Agent trait + FakeAgent
  runner.rs                        run_task(), recover()
  crash.rs                         CrashPoint injection
crates/agentos-cli/src/main.rs
fixtures/parser-repo/              failing parser project
fixtures/profiles/parser-checks-v1/ protected verification profile
```

---

### Task 1: Workspace scaffold and containerised test loop

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `Dockerfile`, `compose.yaml`, `.gitignore`
- Create: `crates/agentos-core/{Cargo.toml,src/lib.rs}` (plus empty `agentos-store`, `agentos-engine`, `agentos-cli`)

**Interfaces:**
- Produces: `docker compose run --rm test` runs `cargo test --workspace`.

- [ ] **Step 1: Write workspace files**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
edition = "2024"
version = "0.1.0"

[workspace.dependencies]
serde = { version = "1.0.229", features = ["derive"] }
serde_json = "1.0.151"
thiserror = "2.0.21"
blake3 = "1.8.7"
rusqlite = { version = "0.40.2", features = ["bundled"] }
tokio = { version = "1.53.1", features = ["rt-multi-thread", "macros", "time", "process", "fs"] }
clap = { version = "4.6.7", features = ["derive"] }
tracing = "0.1.44"
uuid = { version = "1.26.1", features = ["v4", "v7", "serde"] }
jsonschema = "0.58.4"
tempfile = "3.27.0"
proptest = "1.11.0"
```
`Dockerfile`:
```dockerfile
FROM rust:1.98-bookworm
RUN apt-get update && apt-get install -y python3 git && rm -rf /var/lib/apt/lists/*
WORKDIR /work
```
`compose.yaml`:
```yaml
services:
  test:
    build: .
    volumes: [".:/work", "cargo-cache:/usr/local/cargo/registry", "target:/work/target"]
    command: cargo test --workspace
volumes: { cargo-cache: {}, target: {} }
```
Each crate gets a minimal `Cargo.toml` (`edition.workspace = true`) and empty `lib.rs` (`main.rs` for the CLI).

- [ ] **Step 2: Verify the loop**

Run: `docker compose run --rm test`
Expected: builds, `0 tests` pass. If the `rust:1.98` tag does not exist yet, use the latest stable tag and update the Tech Stack line.

- [ ] **Step 3: Commit**

```bash
git add -A && git commit -m "chore: workspace scaffold with dockerised test loop"
```

---

### Task 2: Contract schema and validation

**Files:**
- Create: `crates/agentos-core/src/contract.rs`
- Test: inline `#[cfg(test)]` in `contract.rs`

**Interfaces:**
- Produces:
  - `struct Contract { goal: String, repository: RepoRef, profile: String, editable_paths: Vec<String>, verification_profile: String, capabilities: Vec<Capability>, limits: Limits }`
  - `struct Limits { model_requests: u32, max_output_tokens_per_request: u32, tool_actions: u32, deadline_seconds: u32, worker_vcpus: u32, worker_memory_mib: u32 }`
  - `enum Capability { SnapshotRead, WorkspaceApplyPatch, VerificationRun, ArtifactExport }` serialised as `snapshot.read` etc.
  - `Contract::parse(json: &str) -> Result<Contract, ContractError>`
  - `Contract::path_allowed(&self, rel: &str) -> bool`

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    const OK: &str = r#"{"goal":"g","repository":{"source":"/r","revision":"abc"},"profile":"python-stdlib-v1",
      "editable_paths":["src/**"],"verification_profile":"parser-checks-v1",
      "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export"],
      "limits":{"model_requests":12,"max_output_tokens_per_request":4096,"tool_actions":50,
                "deadline_seconds":1200,"worker_vcpus":2,"worker_memory_mib":2048}}"#;

    #[test] fn parses_spec_example() { assert!(Contract::parse(OK).is_ok()); }
    #[test] fn rejects_unknown_field() {
        let j = OK.replacen("{\"goal\"", "{\"extra\":1,\"goal\"", 1);
        assert!(Contract::parse(&j).is_err());
    }
    #[test] fn rejects_zero_limit() {
        assert!(Contract::parse(&OK.replace("\"tool_actions\":50", "\"tool_actions\":0")).is_err());
    }
    #[test] fn rejects_empty_editable_paths() {
        assert!(Contract::parse(&OK.replace("[\"src/**\"]", "[]")).is_err());
    }
    #[test] fn rejects_escaping_glob() {
        assert!(Contract::parse(&OK.replace("src/**", "../x/**")).is_err());
        assert!(Contract::parse(&OK.replace("src/**", "/etc/**")).is_err());
    }
    #[test] fn path_allowed_matches_glob_and_blocks_traversal() {
        let c = Contract::parse(OK).unwrap();
        assert!(c.path_allowed("src/parser.py"));
        assert!(!c.path_allowed("tests/test_parser.py"));
        assert!(!c.path_allowed("src/../tests/x.py"));
    }
}
```

- [ ] **Step 2: Run, expect FAIL** — `docker compose run --rm test cargo test -p agentos-core` → compile error (`Contract` undefined).

- [ ] **Step 3: Implement**

Use `#[serde(deny_unknown_fields)]` on every struct. `parse` deserialises, then validates: all limits > 0, `editable_paths` non-empty, no pattern that is absolute or contains a `..` component. `path_allowed` rejects any rel path containing `..` or starting with `/`, then matches patterns where `dir/**` means prefix `dir/` and an exact string matches exactly (a small hand-written matcher; no glob crate, YAGNI).

- [ ] **Step 4: Run, expect PASS.**

- [ ] **Step 5: Commit** — `git commit -am "feat(core): contract schema and validation"`

---

### Task 3: Task state reducer

**Files:**
- Create: `crates/agentos-core/src/state.rs`
- Test: inline plus `crates/agentos-core/tests/state_props.rs` (proptest)

**Interfaces:**
- Produces:
  - `enum TaskState { Ready, Running, Waiting, Paused, Verifying, Succeeded, Failed, Cancelled }` with `is_terminal()`
  - `struct Task { id: TaskId, state: TaskState, cancel_requested: bool, workspace_digest: Digest, verified_digest: Option<Digest>, actions_used: u32, step: u32 }`
  - `enum TaskEvent { Started, Waiting, Woken, Paused, Resumed, VerifyStarted, VerifyPassed { digest: Digest }, VerifyFailed, WorkspaceUpdated { digest: Digest }, ActionUsed, CancelRequested, CancelCompleted, Failed { reason: String } }`
  - `fn reduce(task: &Task, ev: &TaskEvent, limits: &Limits) -> Result<Task, TransitionError>` (pure; returns a new value)
  - `Task::may_dispatch(&self) -> bool` (false when `cancel_requested` or not RUNNING/VERIFYING)

- [ ] **Step 1: Write failing tests**

```rust
#[test] fn happy_path_to_succeeded() {
    let l = limits(); let t = Task::new(id(), d("base"));
    let t = reduce(&t, &Started, &l).unwrap();
    let t = reduce(&t, &WorkspaceUpdated { digest: d("w1") }, &l).unwrap();
    let t = reduce(&t, &VerifyStarted, &l).unwrap();
    let t = reduce(&t, &VerifyPassed { digest: d("w1") }, &l).unwrap();
    assert_eq!(t.state, TaskState::Succeeded);
}
#[test] fn success_requires_evidence_for_final_revision() {
    // verify passes for w1, then workspace changes to w2 before completion
    let t = running_with_workspace("w2"); let t = reduce(&t, &VerifyStarted, &l()).unwrap();
    assert!(reduce(&t, &VerifyPassed { digest: d("w1") }, &l()).is_err());
}
#[test] fn failed_verification_returns_to_running_within_limits() { /* VerifyFailed -> Running while actions_used < tool_actions */ }
#[test] fn failed_verification_with_exhausted_limits_goes_failed() { /* actions_used == tool_actions -> Failed */ }
#[test] fn cancel_request_blocks_dispatch_but_state_not_cancelled_until_completed() {
    let t = reduce(&running(), &CancelRequested, &l()).unwrap();
    assert!(!t.may_dispatch()); assert_ne!(t.state, TaskState::Cancelled);
    assert_eq!(reduce(&t, &CancelCompleted, &l()).unwrap().state, TaskState::Cancelled);
}
#[test] fn terminal_states_reject_every_event() { /* for each terminal state and each event variant: Err */ }
#[test] fn cancel_completed_without_request_is_rejected() {}
```
Proptest: for any random event sequence, `reduce` never panics, terminal states never change, and `Succeeded` implies `verified_digest == Some(workspace_digest)`.

- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** the transition table exactly as the spec's states and Global Constraints say. `VerifyFailed` goes to RUNNING if `actions_used < limits.tool_actions`, else FAILED. `WorkspaceUpdated` clears `verified_digest`. `ActionUsed` increments `actions_used` and errors past the limit.
- [ ] **Step 4: Run, expect PASS** (including proptest).
- [ ] **Step 5: Commit** — `git commit -am "feat(core): task reducer with success/cancel invariants"`

---

### Task 4: Effect model and logical IDs

**Files:**
- Create: `crates/agentos-core/src/effect.rs`
- Test: inline

**Interfaces:**
- Produces:
  - `enum EffectKind { ReadSnapshot, ApplyPatch { expected_base: Digest }, RunVerification, ExportBundle }` with `fn capability(&self) -> Capability` and `fn retry_policy(&self) -> RetryPolicy` (`ReadSnapshot`: Retry; `ApplyPatch`: ReconcileThenRetry (safe because of the expected-version check); `RunVerification`: Retry; `ExportBundle`: ReconcileThenRetry)
  - `enum EffectState { Intended, Dispatched, Completed, Failed, Unknown }`
  - `struct EffectId(String)`, derived as `blake3(task_id ‖ step ‖ kind_tag ‖ request_digest)`, deterministic so a restarted controller recomputes the same ID
  - `struct Receipt { effect_id, attempt_id, lease_generation: u64, outcome: Outcome, result_digest: Option<Digest> }`
  - `fn accept_receipt(effect: &EffectRecord, r: &Receipt) -> ReceiptVerdict { Apply, DuplicateIgnored, StaleLeaseIgnored, WrongEffect }`

- [ ] **Step 1: Write failing tests** for: same inputs give the same ID, any changed field gives a different ID; `accept_receipt` returns `DuplicateIgnored` for a second receipt on a COMPLETED effect, `StaleLeaseIgnored` when `r.lease_generation < effect.lease_generation`, and `WrongEffect` for an ID mismatch; each kind maps to its capability.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(core): effect model, stable IDs, receipt verdicts"`

---

### Task 5: Fixture repository and protected verification profile

**Files:**
- Create: `fixtures/parser-repo/src/parser.py`, `fixtures/parser-repo/tests/test_parser.py` (visible tests)
- Create: `fixtures/profiles/parser-checks-v1/{profile.json,check_parser.py}` (the protected acceptance check)
- Create: `crates/agentos-engine/tests/fixture_sanity.rs`

**Interfaces:**
- Produces: a `parse_kv(text: str) -> dict` in `parser.py` with a seeded bug (for example, it fails to strip whitespace around `=`); `profile.json` is `{ "id": "parser-checks-v1", "command": ["python3","check_parser.py"], "protected": true }`; a known-good patch at `fixtures/parser-repo.fix.patch`.

- [ ] **Step 1: Write the failing sanity test**: running `python3 -m unittest` in the fixture exits non-zero; after `git apply fixtures/parser-repo.fix.patch` in a temp copy, both `unittest` and `check_parser.py` exit 0.
- [ ] **Step 2: Run, expect FAIL** (files absent).
- [ ] **Step 3: Write the fixture, the bug, the protected check and the fix patch.**
- [ ] **Step 4: Run, expect PASS** (needs `python3`, which the Dockerfile installs).
- [ ] **Step 5: Commit** — `git commit -am "test: failing parser fixture and protected profile"`

---

### Task 6: Blob store

**Files:**
- Create: `crates/agentos-store/src/blob.rs`
- Test: inline with `tempfile`

**Interfaces:**
- Produces: `BlobStore::open(dir) -> io::Result<BlobStore>`; `put(&self, bytes: &[u8]) -> io::Result<Digest>`; `get(&self, d: &Digest) -> io::Result<Vec<u8>>` (re-hashes and errors on mismatch); `exists`; `gc(&self, referenced: &HashSet<Digest>) -> io::Result<usize>`.
- `put` writes to `tmp/<uuid>`, fsyncs the file, renames to `objects/ab/cdef…`, then fsyncs the directory. It is idempotent.

- [ ] **Step 1: Write failing tests**: round-trip; `put` twice gives the same digest and one file; a corrupted blob file makes `get` return an integrity error; a leftover `tmp/` file (simulated crash) is not visible to `exists` and `gc` removes it; `gc` removes unreferenced blobs and keeps referenced ones.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(store): atomic content-addressed blob store"`

---

### Task 7: SQLite journal (state plus event in one transaction)

**Files:**
- Create: `crates/agentos-store/src/db.rs`
- Test: `crates/agentos-store/tests/db.rs`

**Interfaces:**
- Consumes: `reduce`, `Task`, `TaskEvent` (Task 3).
- Produces:
  - `Db::open(path)` sets `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON`, `busy_timeout`, and creates tables `tasks`, `events(task_id, seq, type, payload, ts, refs, PRIMARY KEY(task_id, seq))`, `effects`, `attempts`, `artifacts`, `usage`, `capabilities`, `observations` (the spec's minimum records).
  - `create_task(&self, contract: &Contract, contract_digest: &Digest) -> Result<TaskId>`
  - `append(&self, id: &TaskId, ev: &TaskEvent) -> Result<Task>`: in one `BEGIN IMMEDIATE` transaction it loads the task, calls `reduce`, updates `tasks`, and inserts the event with `seq = last + 1`. On a reducer error nothing is written (but a denial event may be recorded by a separate call).
  - `events(&self, id) -> Vec<StoredEvent>`; `task(&self, id) -> Task`.

- [ ] **Step 1: Write failing tests**: `append` of an invalid transition leaves both tables unchanged; sequences are gapless 1..n; 8 threads appending to one task yield unique gapless sequences; reopening the DB file returns identical state and events; two tasks with the same contract get distinct IDs.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(store): sqlite journal with atomic state+event commits"`

---

### Task 8: Effect lifecycle, budget reservations and publish ordering

**Files:**
- Create: `crates/agentos-core/src/budget.rs`, extend `db.rs`
- Test: `crates/agentos-store/tests/effects.rs`

**Interfaces:**
- Produces on `Db`:
  - `record_intent(&self, task, kind, request_digest, expected_workspace: &Digest, reserve: Reservation) -> Result<EffectRecord>`: one transaction writes the effect (INTENDED), the usage reservation, and an event; returns the existing record if the same `EffectId` already exists (idempotent).
  - `mark_dispatched(&self, effect, attempt_id, lease_generation) -> Result<()>`
  - `complete_effect(&self, effect, receipt, result_artifact: &Digest) -> Result<ReceiptVerdict>`: uses `accept_receipt`; on `Apply` it commits the effect result, settles usage and advances the task in one transaction; otherwise it records an audit event and changes nothing else.
  - `mark_unknown(&self, effect)`, which keeps the reservation.
  - `outstanding_effects(&self, task) -> Vec<EffectRecord>` (INTENDED, DISPATCHED, UNKNOWN).
- `complete_effect` refuses a `result_artifact` that is not present in the `artifacts` table, so blobs must be published and registered first.

- [ ] **Step 1: Write failing tests**: `record_intent` twice returns one row and one reservation; the reservation is rejected when it would exceed `limits`; a duplicate receipt and a stale-lease receipt leave state unchanged and add an audit event; `complete_effect` with an unregistered artifact is rejected; an UNKNOWN effect keeps its reservation in the usage summary.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(store): effect lifecycle, reservations, receipt checks"`

---

### Task 9: Agent, Executor and the run loop (Phase 1 exit)

**Files:**
- Create: `crates/agentos-engine/src/{agent.rs,executor.rs,runner.rs}`
- Test: `crates/agentos-engine/tests/happy_path.rs`

**Interfaces:**
- Consumes: Tasks 2–8.
- Produces:
  - `trait Agent { fn next(&mut self, obs: &Observation) -> AgentAction }`; `AgentAction::{ApplyPatch(String), Verify, Finish}`; `FakeAgent::scripted(Vec<AgentAction>)` and `FakeAgent::from_fixture_patch()`.
  - `trait Executor { async fn run(&self, req: &EffectRequest, ctx: &AttemptCtx) -> Receipt; }`; `FixtureExecutor` works in a temp directory copy of the repo, applies patches with `git apply` after checking every touched path against `Contract::path_allowed` and the expected workspace digest, and runs the protected profile from a read-only path outside the workspace.
  - `run_task(db, blobs, executor, agent, task_id) -> Result<TaskState>`.
  - Workspace digest = BLAKE3 over a sorted `(path, file digest)` listing.

- [ ] **Step 1: Write failing tests**: (a) the happy path ends SUCCEEDED with a patch blob, verification evidence for the final workspace digest, and events covering every state transition; (b) a patch touching `tests/test_parser.py` is denied, a denial event is recorded, and the task still progresses; (c) a patch built against a stale base digest returns a version conflict to the agent; (d) a patch that edits the protected check cannot change the profile digest, because the profile is outside the workspace; (e) a failing fix leads to FAILED once `tool_actions` is exhausted; (f) a patch path through a symlink out of the workspace is denied.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** the loop: validate (`may_dispatch`, capability, budget), `record_intent`, dispatch, `put` the blob and register the artifact, `complete_effect`, feed the observation to the agent.
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): fake agent, fixture executor, run loop"`

---

### Task 10: Crash injection and recovery (Phase 2 exit)

**Files:**
- Create: `crates/agentos-engine/src/crash.rs`, extend `runner.rs` with `recover()`
- Test: `crates/agentos-engine/tests/crash_matrix.rs`

**Interfaces:**
- Produces:
  - `enum CrashPoint { AfterIntent, AfterDispatch, AfterExecutorReceiptBeforeBlob, AfterBlobBeforeCommit, AfterCommit, DuringModelCall }`; a `CrashHook` is consulted at each boundary and returns `Err(Crashed)` to simulate a kill (the in-process database handle is dropped, then the DB is reopened from disk).
  - The executor retains receipts in its own on-disk receipt log, independent of the controller (stand-in for the Phase 3 supervisor).
  - `recover(db, blobs, executor, task_id)`: reconciles each outstanding effect. If a receipt exists, it applies it via `complete_effect`. If the effect is INTENDED and was never dispatched, it re-dispatches within the original reservation. If DISPATCHED with no receipt, it consults the retry policy, either retrying or marking UNKNOWN.

- [ ] **Step 1: Write the failing matrix test.** For each `CrashPoint`, run the happy-path task, crash there, reopen, `recover`, then run to completion. Assertions: the final task is SUCCEEDED; no effect has a `Completed` result recorded twice; the executor's call count for completed effects is exactly 1 (counting executions, not receipts); events have gapless sequences; usage reservations are never lost or doubled; the pre-crash leftover blob is removed by `gc`. For `DuringModelCall`: the reservation stays held, the effect is UNKNOWN, and the retry decision is visible in `events`.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement** the hooks and `recover`.
- [ ] **Step 4: Run, expect PASS.** Also run the matrix 50 times in a loop to look for flakiness: `for i in $(seq 50); do docker compose run --rm test cargo test -p agentos-engine --test crash_matrix || break; done`.
- [ ] **Step 5: Commit** — `git commit -am "feat(engine): crash injection and effect reconciliation"`

---

### Task 11: CLI and export bundle

**Files:**
- Create: `crates/agentos-cli/src/main.rs`; `crates/agentos-engine/src/export.rs`
- Test: `crates/agentos-cli/tests/cli.rs` (uses `assert_cmd`; check its latest version first)

**Interfaces:**
- Produces: `agentos submit task.json | status ID | events ID | pause ID | resume ID | cancel ID | export ID DIR`, with `--home DIR` (default `~/.agentos`) and `--fake-agent` for this milestone.
- Export writes `manifest.json` (base revision, patch digest, final workspace digest, verification profile digest, verification results, usage summary), `patch.diff`, and `evidence/*`. It writes to a temp directory and renames, so a failure leaves no partial bundle. It is refused for non-terminal tasks.

- [ ] **Step 1: Write failing tests**: `submit` prints a task ID and an invalid contract exits non-zero with the validation message; the full flow `submit` → `status` → `export` yields a manifest whose digests match the stored ones, and the exported patch applies to the fixture; `export` on a RUNNING or PAUSED task fails and leaves no directory; `cancel` on a running task ends CANCELLED.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run, expect PASS.**
- [ ] **Step 5: Run the milestone demo and commit.** Start a task with a `--crash-at after-dispatch` debug flag, restart the CLI, and confirm it resumes and exports. `git commit -am "feat(cli): submit/status/events/pause/resume/cancel/export"`

---

## Follow-on plans (not part of this plan)

Each gets its own plan after a re-estimate, as the spec requires:

1. **Phase 3, authority and isolation:** capability broker, supervisor with leases, Firecracker executor behind the `Executor` trait, guest image and profile registration, cancellation and revocation. Needs a KVM-capable host check before starting.
2. **Phase 4, real model:** model broker, typed tool requests, bounded repair loop, usage accounting.
3. **Phase 5, component ABI:** WIT interfaces and the Wasmtime repository analyzer.
4. **Phase 6, release:** fault-injection suite (including disk-full during publication, which this plan does not cover), installer, reproducible guest image, reference tasks.

## Self-Review

- **Spec coverage (Phases 1–2):** contract schema (T2), states and invariants (T3), effect/receipt/lease semantics (T4, T8), fixture and protected profile (T5), artifacts (T6), event journal and atomic commit (T7), budget reservations and UNKNOWN handling (T8, T10), fake agent and terminal states (T9), forced-termination recovery (T10), CLI and export (T11). Not covered by design: Phases 3–6, and the disk-full acceptance row, which belongs to Phase 6.
- **Known simplification:** the executor receipt log in T10 stands in for the Phase 3 supervisor. Lease generations are exercised only as numbers here and become enforced in Phase 3.
- **Open assumption:** the exact fixture bug and the workspace-digest format are chosen in T5 and T9, and the spec doesn't fix them.
