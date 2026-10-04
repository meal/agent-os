# Repository review and roadmap — 2026-10-04

Reviewed commit: `8c273b1`. The checkout was clean when the review started.

Agent OS is a substantial working prototype of a durable coding-agent controller. Its strongest work is recovery, authority checks, and binding success to protected verification of the final workspace. The next milestone should be a reliable, demonstrated model workflow. The full v0.1 described in the original build plan is not complete: live-model acceptance remains unproven, the Wasm component is absent, and release engineering remains unfinished.

## Current progress

| Milestone | Implementation found | Current assessment |
| --- | --- | --- |
| Phases 1–2: contracts and durability | Five-crate Rust workspace; typed contracts; reducer and property tests; SQLite WAL journal; BLAKE3 artifacts; budgets; deterministic agent; CLI; export; crash/recovery matrix | Implemented. Default Compose tests passed in this review. |
| Phase 3a: authority and supervision | Opaque capability handles; approval/revocation; authorization at intent and dispatch; per-job supervisor; leases; cancellation; deadlines; profile registry | Implemented with documented limitations, including a revoke/job-start race and host process-group escape limitations. |
| Phase 3b-1: Firecracker | Real worker and guest agent; vsock protocol; guest image registry; jailer/chroot/cgroup support; inspectors; reproducible rootfs recipe; hostile-profile and conformance tests | Implemented. Fake guest/fake jailer suite passed here. Real KVM behavior was not revalidated: this environment has no `/dev/kvm`. |
| Phase 3b-2 | Disk and I/O limits, image/job collection, kernel source build, additional jail isolation | Not implemented; explicitly listed as future work. |
| Phase 4: model workflow | Anthropic Messages adapter; deterministic scripted provider; recordings; five tools; journaled model/read effects; shadow reads; uncertain-request accounting; crash matrix; CLI/status/export support | Implementation present and tested offline. The phase exit condition is open: README states the real API acceptance test has never run. Current live harness uses the host worker. |
| Phase 5: component ABI | No Wasmtime dependency, WIT files, component runner, or reference analyzer found | Not started. |
| Phase 6: release | Extensive recovery tests, guest-image build/verification scripts, demos and documentation | Partially implemented. No checked-in CI workflow, fresh-host installer, release workflow, or demonstration set across two registered repository snapshots found. |

The crate boundaries are useful: `core` owns types/policy, `store` owns durable state, `engine` drives effects and recovery, `guest` serves VM work, and `cli` owns user operations. Preserve those boundaries. A rewrite would discard the best-tested parts without addressing the remaining acceptance gaps.

## Verification performed

| Check | Result |
| --- | --- |
| `docker compose run --rm test cargo test --workspace --locked` | Exit 0. Default host/offline suite passed. |
| `docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test cargo test --workspace --locked` | Exit 0; 876 reported passes, 0 failures, 2 ignored across 50 test targets. |
| `docker compose run --rm test cargo fmt --all -- --check` | Failed; default rustfmt would change many files. No checked-in rustfmt configuration found. |
| `docker compose run --rm test cargo clippy --workspace --all-targets --locked -- -D warnings` | Exit 101. Diagnostics include `single_match` in `core/tests/state_props.rs:46`, `type_complexity` in `engine/src/crash.rs:73`, `large_enum_variant` in `engine/src/executor.rs:108`, and `collapsible_if` in `engine/src/journal.rs:98`, `:139`, and `engine/src/runner.rs:577`. Compilation stops before checking every downstream target. |
| New model-deadline regression probe | Failed as expected: deadline one second away, provider response after three seconds, executor returned `Success` after 3.004 seconds. |
| Real KVM tier | Not run: `/dev/kvm` is absent. |
| Live Anthropic tier | Not run; no paid API calls were made. |

The 876 number includes tests whose gated bodies return early. Rust's harness reports those as passed. It is not a count of 876 independently exercised real VM/model behaviors. Fake jail tests prove orchestration and protocol behavior, not real chroot, UID, cgroup, OOM, or network isolation.

The regression source is preserved in [model-deadline-regression.rs.txt](model-deadline-regression.rs.txt). It was temporarily compiled as `crates/agentos-engine/tests/review_deadline_probe.rs`, then moved out of the test tree. The review leaves no failing test in the normal suite and changes no runtime source.

## Findings, ordered by priority

### 1. Model requests can overrun the task deadline — fix before alpha

[`ModelExecutor::run`](../../crates/agentos-engine/src/model/executor.rs) checks `deadline_ts` before sending, then awaits `provider.complete` without a task-bound timeout. The Anthropic client's independent timeout is 600 seconds. The runner checks cancellation/deadline between actions, so a request started just before its deadline can delay task closure for much longer than the contract allows. The regression above confirms the executor accepts a late response; the runner can subsequently fail the task, so this is not evidence of false task success.

Wrap the sent request in the remaining task duration. If interrupted after dispatch, preserve uncertain accounting through `ExecOutcome::unresolved` and the existing forfeit path. Do not release the reservation or silently retry under the same logical effect. A concrete implementation plan is [model deadline enforcement](../superpowers/plans/2026-10-04-model-deadline-enforcement.md).

### 2. Provider bytes and key-file reads are unbounded — fix before alpha

[`AnthropicProvider::complete`](../../crates/agentos-engine/src/model/anthropic.rs) calls `resp.bytes().await` before truncating error text. Both success and error responses can consume arbitrary memory; the 4096-byte error excerpt is not a download limit. [`Home::api_key`](../../crates/agentos-cli/src/home.rs) uses `read_to_string` on any supplied path, so a FIFO can block and a huge file/device can exhaust memory.

Use bounded chunked body reads, including responses without `Content-Length`. Set separate, documented body limits for success and errors. For keys, open with nonblocking/no-follow semantics where appropriate, verify the opened descriptor is a regular file, and limit bytes read. Test oversized fixed-length and chunked bodies, oversized keys, FIFOs, and non-regular paths. Treat an interrupted success body conservatively as possibly billed.

### 3. HTTP failures consume the model budget without useful recovery — fix before live acceptance

[`ModelAgent::next`](../../crates/agentos-engine/src/agent.rs) reissues the same request on every `ModelCallFailed`. Definite 400/401/403/404 errors spend the remaining request budget; 429/529/5xx repeat immediately. This is already documented and tested, but is poor behavior for a real account.

Propagate a typed failure classification. End the task on permanent request/auth/model errors. Schedule bounded retries for transient statuses, respecting `Retry-After`, cancellation, deadline, and request budget. Journal retry decisions/timing so recovery does not recompute a new policy or bypass a wait. Keep client-level retries disabled: every actual send must retain its own accounting.

### 4. Provider identity is not bound to the submitted task — address before distributing the CLI

The CLI validates the base URL, but `Submitted` records the model without the endpoint. A later environment change can send a resumed task's key and repository content to a different HTTPS host. This is configuration drift in the existing single-owner model, not a demonstrated guest escape.

Record the normalized endpoint, show custom endpoints before approval, and refuse endpoint changes on resume unless they go through an explicit recorded policy. Test submit/resume with differing CLI and environment overrides. Define compatibility for older tasks with no endpoint record.

### 5. Automated checks and progress documents disagree with the code

The test coverage is extensive, but there is no checked-in `.github` workflow. Formatting and strict lint gates are currently red. [`docs/index.html`](../index.html) still says the real model arrives in Phase 4 and only a fake agent exists, while README describes the implemented model path. The original build plan says no software has been built; the Phase 4 design still labels itself draft and its plan retains unchecked steps.

Establish a repeatable CI baseline and separate implementation status from acceptance evidence. Update the landing page and milestone statuses, preserving the explicit live/KVM evidence qualifications. Choose default rustfmt or a documented config; fix lint findings without changing recovery semantics just to satisfy a lint.

### 6. Storage and VM resource lifecycle remain unsuitable for sustained use

Model retention lookups scan the entire retention directory; retained model responses, job directories, workspace images, and scratch images accumulate. VM disks remain fixed at 1 GiB and 512 MiB. Jail `memory.max` uses guest memory plus 128 MiB, and README records an observed page-cache-related Firecracker OOM failure.

Implement collection under the existing driver/workspace locks, preserving data required by outstanding effects and export. Test collection after crashes, cancel, unsettled receipts, and repeated invocation. Add contract disk limits and I/O limits before running broader task sets. Measure the cgroup/page-cache behavior under disk-heavy load before selecting memory overhead. Add host publication ENOSPC tests; current SQLite trigger tests and guest disk-fill checks do not exercise host blob publication under a full filesystem.

## Dependency/runtime check

Checked the latest online package documentation for all 21 declared workspace dependencies on 2026-10-04. Most manifest versions match the latest published stable versions. These declared versions lag:

| Dependency | Manifest / lock | Latest checked | Action |
| --- | --- | --- | --- |
| Tokio | 1.53.1 / 1.53.1 | [1.53.2](https://docs.rs/crate/tokio/latest) | Scoped update, then host/fake jail and deadline/crash checks. |
| UUID | 1.26.1 / 1.26.1 | [1.27.0](https://docs.rs/crate/uuid/latest) | Scoped update, then identity/store/replay tests. |
| jsonschema | 0.58.4 / absent | [0.58.5](https://docs.rs/crate/jsonschema/latest) | Currently unused by member crates. Remove the unused declaration or adopt it only for an actual schema requirement. |

Rust [1.98.1](https://doc.rust-lang.org/stable/releases.html), Firecracker [1.17.0](https://github.com/firecracker-microvm/firecracker/releases), reqwest [0.13.5](https://docs.rs/crate/reqwest/latest), and rusqlite [0.40.2](https://docs.rs/crate/rusqlite/latest) match the checked latest releases. Cargo manifest version strings are compatible version requirements; the lockfile supplies the resolved reproducible versions.

Python is currently selected through Debian bookworm packages, not pyenv. The latest checked stable Python is [3.14.8](https://www.python.org/downloads/). To follow the owner's runtime policy, add a pyenv-managed, explicitly pinned current Python to the development image and introduce a versioned guest profile/image for the new interpreter; run both host and real guest conformance before replacing the old profile. Record the interpreter version in image provenance. Ruby is not part of this repository, so RVM has no current role.

Phase 5 would introduce Wasmtime; its checked current release is [49.0.2](https://docs.rs/crate/wasmtime/latest). Recheck at implementation time, pin the selected version and WIT package versions, and configure execution interruption and memory limits explicitly. Wasmtime documents [fuel and epoch interruption](https://docs.wasmtime.dev/examples-interrupting-wasm.html).

## Proposed delivery plan

This is a prioritized roadmap. Each independent subsystem should receive its own detailed spec and implementation plan when execution begins; the immediate deadline fix already has one.

| Order | Deliverable and files | Acceptance gate |
| --- | --- | --- |
| 1 | Model-path stabilization: `engine/src/model/{executor,anthropic,provider}.rs`, `engine/src/agent.rs`, `cli/src/home.rs`, provider/model/CLI tests | Deadline regression green; bounded key/body reads; permanent failures stop; transient waits survive replay; one reservation per send; no secret in artifacts or child environments. |
| 2 | Repeatable checks and accurate status: `.github/workflows/ci.yml`, formatting configuration if needed, manifests/lockfile, README, build-plan status, `docs/index.html` | Required host and fake-jail jobs pass with `--locked`; fmt and Clippy pass; gated KVM/live evidence is reported separately; scoped dependency updates pass the same suites. |
| 3 | Close Phase 4 acceptance: `engine/tests/live_model.rs`, versioned transcripts/evidence, README | Real Anthropic fixture repair within budget, protected final verification, export, and offline replay of the recording. Extend the host-only live harness to cover jailed Firecracker. On a KVM host, run the full real-worker crash/conformance suite; capture model/endpoint, guest digest, host kernel, usage and failure cases. |
| 4 | Resource lifecycle and high-value 3b-2 work: `core/src/contract.rs`, `engine/src/{firecracker,jail,job,recover}.rs`, `engine/src/model/executor.rs`, `store/src/blob.rs`, KVM/store tests | Safe idempotent GC; live/outstanding work retained; bounded disks/I/O; disk-heavy cgroup checks; ENOSPC during publication cannot produce a successful checkpoint. Preserve executable file modes or explicitly reject unsupported profiles at registration. |
| 5 | Phase 5 minimal ABI: proposed `wit/agentos-v1.wit`, `crates/agentos-component/`, reference analyzer fixture, broker/effect integration | One analyzer reads only granted snapshot objects and returns a journaled report; denied reads fail; infinite loop and memory growth stop; component execution recovers without repeating completed effects. No ambient filesystem/network imports. |
| 6 | Phase 6 alpha release: install/release scripts, fresh-host smoke test, kernel-source build recipe, supported-host matrix, two-snapshot reference task set | Fresh supported host can install, register images/profiles, run a jailed task, kill/resume, and export reproducible evidence. Publish checksums, provenance, measured budgets, model versions, failures, and supported-host limits. |

Orders 1–3 have the highest immediate value. They test whether the current runtime can support an actual coding workflow and prevent a new ABI from being built around unfinished model error handling. Phase 5 is still required for the full original v0.1 scope. VM reuse, a second provider, streaming, web UI, distributed execution, per-job jail UID/PID/network namespaces, and seL4 remain later projects unless acceptance measurements establish a need.

Before setting delivery dates, obtain one real KVM run and one live-model run. Provider behavior, jail memory sizing, and fresh-host cgroup setup are still estimation risks. Keep the existing recovery/state invariants as mandatory regression gates for every change.
