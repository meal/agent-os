# Agent OS v0.1 — build plan

Date: 2026-10-01  
Status: proposed implementation scope; no software has been built as part of this plan.

## Objective

Build a durable, capability-controlled agent runtime on one Linux machine. The first workflow is a coding task: inspect a repository snapshot, propose and apply a patch in an isolated workspace, run a registered verification suite, and return a reviewable patch with evidence.

The central demonstration is recovery. Stop the controller during a task, restart it, and continue from recorded state while reconciling outstanding operations.

The agent-facing interfaces should remain independent of the host backend so the task engine and policy model can later support a different OS substrate.

## First workflow and scope

Start with a Python repository profile using standard-library tests and a prepared guest image. This avoids package installation and network access during jobs. Support one hosted model adapter and a deterministic fake model for repeatable recovery tests.

A task produces a patch, the base revision and final workspace digest, verification results, a usage ledger, and an action history. A person reviews and applies the exported patch separately.

The initial platform is an x86-64 Linux host with working KVM, one local owner, one active controller, and serialized mutations within each task workspace. Separate tasks have separate workspaces.

Keep deployments, browser control, fleet scheduling, distributed storage, remote identity, seL4 integration, GPU model serving, and a web UI in subsequent versions.

## Components

| Component | Initial implementation | Responsibility |
| --- | --- | --- |
| Controller and CLI | Rust, Tokio, Serde; JSON contracts | Task submission, execution, pause, resume, cancellation, inspection and export |
| Durable task engine | Rust state reducer and application event journal | State transitions, checkpoints, receipts, retries, deadlines and recovery |
| Metadata | SQLite on local disk, WAL mode, explicit durability settings | Journal, current task state, capabilities, effect records and usage reservations |
| Artifacts | Local immutable blob store with BLAKE3 identifiers | Repository inputs, model responses, patches, test outputs and manifests |
| Policy broker | Rust, local owner identity and opaque capability handles | Authorize requests and enforce scope, expiry, revocation and budgets |
| Native execution | Supervised Firecracker microVM jobs and a prepared guest image | Execute repository code with bounded CPU, memory, storage and runtime |
| Extension runtime | Wasmtime and a pinned WIT interface set | Execute one reference component with explicitly granted imports |
| Model broker | One real provider adapter plus a deterministic fake | Record requests and responses, enforce request/output limits and report usage |
| Diagnostics | Structured Rust tracing | Connect task, step, effect and attempt identifiers to outcomes |

SQLite WAL is an implementation journal. Maintain a separate application events table for durable task history; do not use the database WAL itself as task history.

The first Wasm component is a repository analyzer. It can read authorized snapshot objects and return a report. It gets no ambient filesystem or network access. Native repository code executes in the VM worker.

## Task contract

Accept a schema-validated contract through the CLI. The owner approves its permissions and acceptance criteria before execution. For v0.1, the owner writes or reviews the contract; natural-language contract generation can come later.

Illustrative contract:

    {
      "goal": "Repair the failing parser while preserving the public interface",
      "repository": {
        "source": "/path/to/repository",
        "revision": "recorded-at-submission"
      },
      "profile": "python-stdlib-v1",
      "editable_paths": ["src/**"],
      "verification_profile": "parser-checks-v1",
      "capabilities": [
        "snapshot.read",
        "workspace.apply_patch",
        "verification.run",
        "artifact.export"
      ],
      "limits": {
        "model_requests": 12,
        "max_output_tokens_per_request": 4096,
        "tool_actions": 50,
        "deadline_seconds": 1200,
        "worker_vcpus": 2,
        "worker_memory_mib": 2048
      }
    }

Record immutable repository, guest image, verification profile and contract versions at submission. The agent cannot alter the trusted acceptance profile.

Dollar spending is estimated from configured provider pricing and observed usage. Request counts and requested output limits are locally enforced. An uncertain model request retains its budget reservation; recovery must account for a possible billed request. A hard account-level spending ceiling requires support from the provider.

## Persistence and action semantics

Use task states READY, RUNNING, WAITING, PAUSED, VERIFYING, SUCCEEDED, FAILED and CANCELLED. A failed verification can return to RUNNING within the task's remaining limits. SUCCEEDED requires verification evidence attached to the final workspace revision.

Cancellation first records a request, blocks new effects, and asks the supervisor to stop active work. Record CANCELLED after active work is stopped or its outcome is reconciled.

Minimum metadata records:

| Record | Required information |
| --- | --- |
| Task | Contract version, owner, state, current step, checkpoint, deadline |
| Event | Task-local sequence, event type, payload, timestamp, references |
| Effect | Stable logical effect ID, request digest, lifecycle and result |
| Attempt | Execution attempt ID, worker identity, lease generation and timestamps |
| Capability | Owner/task binding, permitted resource/operation, expiry, revocation |
| Artifact | Content digest, size, type, producing effect and provenance |
| Usage | Reservations, settled usage and uncertain charges |
| Observation | Source, observed revision/time and evidence reference |

Commit the task state update and corresponding event in one SQLite transaction. Publish artifact bytes durably before committing metadata that references them; reclaim unreferenced blobs separately.

Action sequence:

1. Validate the typed request, current capability and budget.
2. Persist the effect intent, expected workspace version, and resource reservation.
3. Dispatch the operation with a stable effect ID and unique attempt ID.
4. Have the supervisor retain its result receipt independently of the controller.
5. Store output artifacts, verify the result, and commit the receipt and next task state.
6. On restart, reconcile outstanding effects with recorded receipts and worker state.

A separate supervisor enforces a bounded lease and deadline even if the controller stops. Late receipts are checked against the task state and lease generation. Workspace updates use expected-version checks.

Replay consumes stored model responses and operation results. It does not reissue completed model calls or execute recorded effects. Outstanding operations can become UNKNOWN; retries depend on the operation's declared reconciliation and retry behavior. The runtime makes no global exactly-once claim for external systems.

For the first workflow, supported effects have narrow behavior: reading a snapshot, applying a patch against an expected base version, running a registered test job, and exporting an immutable result bundle.

## Milestones

Planning estimate: 6–8 engineer-weeks, approximately 240–320 hours, assuming experience with Rust and Linux virtualization. The week ranges below describe a full-time sequence. Re-estimate after the first two milestones.

| Phase | Indicative timing | Deliverable | Exit condition |
| --- | --- | --- | --- |
| 1. Contracts and execution model | Week 1 | Rust workspace, contract schema, task/effect model, CLI, deterministic fake agent and fixture repository | A task advances through the defined states and returns a fixture patch and evidence |
| 2. Durable execution | Week 2 | SQLite journal, immutable artifacts, checkpoints, budget reservations and recovery | Forced controller termination recovers the same task and does not repeat completed effects |
| 3. Authority and isolation | Weeks 3–4 | Capability broker, worker leases, Firecracker supervisor, image/profile registration, cancellation | Repository code stays within its workspace and configured resources; forbidden effects are denied |
| 4. Real agent workflow | Week 5 | One real model adapter, structured tool requests, bounded repair loop and export bundle | A real model fixes the fixture through the broker and produces independently collected test evidence |
| 5. Component ABI | Week 6 | Minimal WIT interfaces and one Wasmtime repository analyzer | The component accesses granted resources and fails when attempting unauthorized access |
| 6. Recovery and release | Weeks 7–8 | Fault-injection suite, fresh-host installer, reproducible guest image build, reference tasks and documentation | All runtime invariants pass and the reference workflow works on a fresh supported host |

Begin with deterministic execution and fault injection. Add the real model after recovery and capability boundaries are working.

## Acceptance and failure tests

These tests establish runtime behavior. Model task quality is evaluated separately.

| Scenario | Required behavior |
| --- | --- |
| Controller dies before dispatch | Recovery identifies an undispatched effect and handles it within the original limits |
| Job completes before its controller acknowledgement | Recovery finds or reconciles the supervisor's receipt and avoids blindly repeating the completed effect |
| Controller dies during a model call | The request remains uncertain; its reservation and retry decision are visible |
| Agent attempts an unauthorized path or action | The broker denies it and records the denial |
| Capability is revoked during execution | New requests fail; active jobs are stopped where supported; existing results remain auditable |
| Task is cancelled or exceeds its deadline | New work stops; the supervisor terminates active execution and reports the outcome |
| Guest attempts network access or host secret access | The reference worker configuration provides neither resource |
| Patch targets a stale workspace version | The update is rejected and the agent receives a version conflict |
| Wasm component loops indefinitely or grows memory | Configured execution and memory limits stop it |
| Host runs out of disk space during publication | The task cannot report a successful durable checkpoint; restart handles partial/unreferenced artifacts |
| Verification fails | The task continues within limits or ends FAILED; it cannot report SUCCEEDED |
| The agent edits acceptance checks | Protected verification artifacts and manifests remain outside its write authority |
| Duplicate or late receipt arrives | The controller checks effect identity and lease generation before changing state |

Use a trusted test runner and profile tied to immutable input references. Passing a suite demonstrates those declared checks; review of the exported patch remains part of the coding workflow.

Release an alpha with a fixture workflow plus a small documented task set across at least two registered repository snapshots. Publish per-task outcomes, budgets, model versions and failure cases. This is a demonstration set, not a claim of general reliability.

## Proposed CLI and exported result

Proposed commands:

    agentos submit task.json
    agentos status TASK_ID
    agentos events TASK_ID
    agentos pause TASK_ID
    agentos resume TASK_ID
    agentos cancel TASK_ID
    agentos export TASK_ID ./result

Export a result manifest containing the base revision, patch digest, final workspace digest, verification profile digest, verification results, and usage summary. Include the patch and linked evidence artifacts.

## First implementation tickets

1. Write the task/effect types and invariants before selecting detailed worker interfaces.
2. Create one fixture repository with a known failing parser and protected verification profile.
3. Implement the reducer and deterministic fake agent; test bounded retries and terminal states.
4. Implement the event journal, artifact publication and logical effect IDs.
5. Add forced-termination tests around every persistence and dispatch boundary.
6. Add the capability broker and supervised VM execution.
7. Connect the real model through the same typed request path.
8. Add the minimal component ABI and release packaging.

The first milestone is a task with a stable identity, exported patch and evidence, and recoverable execution history. The full v0.1 adds real-model execution, VM isolation, capability enforcement, and one Wasm component.

## Follow-on decisions

Use the alpha results to decide whether the task ABI merits a native OS port. Then prioritize the next capability by actual demand: richer repository profiles, browser adapters, local inference, remote workers, or the seL4 substrate.

A seL4 port entails runtime and device integration work; the Linux prototype's host dependencies will need adaptation or placement in compatibility services.

## Primary references

- [Wasmtime sandbox and host-interface model](https://docs.wasmtime.dev/security.html)
- [Wasmtime execution interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html)
- [WIT interface and resource types](https://component-model.bytecodealliance.org/design/wit.html)
- [SQLite write-ahead logging](https://sqlite.org/wal.html)
- [Firecracker implementation and Linux KVM requirements](https://github.com/firecracker-microvm/firecracker)
- [Rust support in seL4 userspace](https://docs.sel4.systems/projects/rust/)

The scope, milestones, protocol and effort estimate above are proposed engineering choices.

