# Agent OS v0.1 completion design

Date: 2026-10-04. Status: proposed delivery design; planning only.

## Intent and success

Complete the original Linux v0.1 scope using the existing runtime. The owner requested a repository review followed by a plan. The review at `8c273b1` establishes the starting point: durable execution, authority, Firecracker, and an offline-tested model agent are present; live acceptance, components, and release engineering remain incomplete.

Success means a fresh supported x86-64 Linux host can run a real model against two registered repository snapshots, use jailed Firecracker for repository execution, recover after controller termination, and export a patch whose final workspace has protected verification evidence. One Wasm analyzer must use granted snapshot objects without ambient access. Required checks must be repeatable in CI.

Preserve `core`, `store`, `engine`, `guest`, and `cli`. Introduce small modules for new responsibilities, rather than restructuring the recovery engine while fixing reliability defects.

## Fixed constraints

- One local owner, one driver per home, serialized workspace changes.
- Docker Compose for build/test; offline tests make no paid API calls.
- `SUCCEEDED` requires protected verification of exactly the final workspace.
- One model send per attempt; requests possibly billed remain counted as uncertain.
- Capability checks, expected workspace versions, and durable receipts remain mandatory.
- Latest stable library versions are checked online before additions/updates, then resolved versions are recorded in `Cargo.lock`.
- Python development uses pyenv and a pinned current stable version. A new interpreter uses a new guest profile/image; old image digests remain immutable. Ruby is absent; use RVM if Ruby is introduced.
- Tests cover behavior, failure boundaries, and recovery. Commits have no co-author trailer.

## Milestone A: reliable live coding workflow

### Deadlines and bounded input

Apply the existing focused deadline plan. A request interrupted after dispatch returns unresolved; the runner forfeits it with uncertain usage and ends the expired task before another send.

Initial limits are proposed explicit defaults:

| Boundary | Limit | Exceeded behavior |
| --- | --- | --- |
| Successful HTTP response body | 4 MiB, raw bytes | Stop reading; unresolved possibly billed request |
| Rejected-response excerpt | 4096 raw bytes | Keep prefix; retain definite HTTP status; do not drain the body |
| API key file | 4096 bytes | Usage error before submitting/running |
| Serialized model request/history | 8 MiB | Fail with a specific context-size reason before a new send/reservation |

Read HTTP bodies incrementally and enforce actual bytes, not only `Content-Length`. Files holding keys must be regular files opened with nonblocking/no-follow behavior and bounded reads from the opened descriptor. Existing ASCII validation and redaction remain intact.

### Endpoint and policy identity

New Anthropic tasks record normalized `model_endpoint`, `model_policy_version: 1`, and `model_limits_version: 1` in `Submitted`; status/export expose these nonsecret settings. No API key is stored. Resume uses the recorded endpoint when there is no override and refuses a different explicit/env override before reading the key or mutating the task.

Old submissions without a policy version retain legacy policy 0 when replaying existing sessions. Old Anthropic submissions without an endpoint may use only the official endpoint; a legacy custom-endpoint task needs a new submission. Explain this compatibility rule in the CLI error and documentation. Completed old effects remain replayable without sending again.

### Failure classification and retries

Define `ModelFailureClass::{Permanent, Transient}` in the engine. Keep transport loss represented by the existing unresolved/forfeit semantics. A definite rejection records status, bounded message, and optional `Retry-After` alongside the retained outcome; observations carry optional typed metadata with serde defaults for legacy observations.

- Permanent: 400, 401, 403, 404, other nontransient 4xx, redirects, malformed complete response. Fail with the specific reason after one rejected send.
- Transient: 408, 429, and 5xx, including 529. Each retry is a new effect/reservation.
- Backoff: 2, 4, 8, 16, 32, then 60 seconds for consecutive transient failures. No random jitter in this single-driver alpha.
- Support integer and HTTP-date `Retry-After`; use the larger of provider delay and backoff. Invalid headers use backoff. A retry that cannot start before the deadline ends the task without another send.
- Transport loss: count uncertain usage and use the same durable bounded scheduling mechanism before any new request, subject to remaining budget.

The runner records `ModelRetryScheduled` with failed effect ID, retry turn, absolute not-before timestamp, and policy version. The store owns this event type. Retry timing is not recomputed during replay. Both normal flow and recovery of a journaled-but-undispatched retry consult the schedule. While waiting, poll task interruption/deadline at most every 100 ms; pause/cancel/revoke stops further dispatch. Never sleep while holding a SQLite write transaction.

The request-size guard runs before registering the new request blob and journaling its turn. Policy 0 keeps its old action decisions during replay. Policy 1 fail/retry decisions depend on typed observations and recorded schedules, never transient environment state.

### Automated checks and live evidence

Pull requests run required host and fake-jail suites, default rustfmt, and strict Clippy with `--locked`. Real KVM tests run on a designated isolated runner, not an untrusted fork with privileged access. Live-provider tests are a manually triggered evidence job with mounted key files and explicit budgets; never infer live success from default-tier pass counts.

Parameterize the current host-only live harness by recorded worker. Preserve host live acceptance, add jailed Firecracker acceptance, and replay each saved recording offline. Evidence contains commit, model, endpoint, profile/image digests, host/kernel/Firecracker versions, call counts, uncertain requests, task state, and exported artifact digests. Evidence never contains credentials.

## Milestone B: safe repeated operation

### Collection and failure durability

Add `agentos gc --dry-run` and `agentos gc` under the driver lock. First collect only reconstructible transient data: settled model retention directories, settled dead job directories, scratch images, and terminal workspaces whose required results are already in blobs. Keep task inputs, journal, linked blobs, immutable registries, and all data needed by outstanding effects or pending cancellation. (Revised 2026-10-07: job directories themselves are kept with their logs, status and receipts; only their redundant `output.bin`, `scratch.img` and `v.sock` are removed. See the [focused GC design](2026-10-04-conservative-gc-design.md).)

Do not infer safety from terminal task state alone: terminal tasks can still have outstanding effects. Require no outstanding effects, no live job/inspection, and the workspace lock before removing task images. Plan deletion, revalidate locks/references, then remove only owned paths. Refuse symlink/traversal candidates. Repeated collection is idempotent and preserves export.

Exercise real host blob publication ENOSPC using a bounded disposable filesystem or fault seam, in addition to existing database trigger faults and guest disk-fill checks. Publication failure must never commit a success reference. Restart either publishes an intact retained receipt or reports a recoverable/uncertain outcome, with no false checkpoint.

### VM resources

Add optional recorded limits `worker_disk_mib`, `worker_scratch_mib`, and drive bandwidth/IOPS settings. Preserve defaults of 1024 MiB workspace and 512 MiB scratch for old contracts; validate minimums, maximums, integer overflow, and image sizing before execution. Apply rate limiters to writable drives and include settings in the submission/export provenance.

Run disk-heavy checks with host I/O contention and capture Firecracker cgroup memory statistics, including page cache. Select/document memory overhead from measurements; do not choose a larger constant without demonstrating the bound works. An infrastructure OOM must fail visibly and cannot produce accepted verification evidence.

For the initial alpha, reject executable-bit-dependent verification commands at registration when their guest profile cannot preserve modes; document interpreter-based commands. Full mode transport and digest changes require a separately versioned guest protocol and compatibility plan.

## Milestone C: full v0.1 and release

### Minimal component ABI

Use a new `agentos-component` crate and pinned `wit/agentos-v1.wit`. The analyzer consumes granted immutable snapshot objects and emits a bounded report. It gets no general WASI filesystem/network environment. Each import checks owner/task binding and resource scope; revocation is checked again when a read occurs. Component execution is a journaled effect with retained output, stable request identity, and recovery behavior equivalent to existing effects.

Bound component memory, report bytes, and execution with fuel and epoch interruption. Test infinite loops, allocation growth, forbidden imports/resources, stale/revoked handles, malformed output, and recovery after result publication. The report cannot assert task success or substitute for protected verification.

### Runtime and release provenance

Use the current stable Python through pyenv in development; publish a separate reproducible guest recipe/image for it. Build the guest kernel from pinned source/config/toolchain with checksums and reproducibility verification. Record interpreter/kernel/component versions in provenance.

Add a Docker-based installer that refuses to overwrite an existing home, verifies release checksums, stages files atomically, and validates KVM/cgroup requirements. Smoke-test it on a fresh supported host. Publish checksums, provenance, supported-host matrix, known limits, recovery demo, and measured task outcomes across two distinct registered snapshots. Release evidence must demonstrate both model and component behavior.

## Scope boundary

Do not add fleet scheduling, remote workers, multiple owners, a web UI, streaming, a second hosted provider, VM snapshot reuse, GPU inference, or a seL4 port to this completion effort. Extra PID/network namespaces and per-job jail UIDs are follow-on isolation projects unless real-tier measurements establish a release blocker.

New defaults above are proposed engineering choices. The plan creates reviewable work; it does not mark any implementation or acceptance gate completed.
