# Agent OS

A controller that runs a coding agent against a repository under a signed-off task
contract. Every side effect goes through a journal: it is intended, dispatched, executed,
published and committed as separate durable steps. A task can only reach SUCCEEDED
through a protected verification of exactly the final workspace. The controller can be
killed at any of those boundaries, and a restarted one recovers the same task without
repeating completed effects.

This is milestone **v0.1, Phases 1-3a**. The agent is a deterministic fake that applies a
given patch. Effects run as host processes, each under its own supervisor, and every
capability is an opaque, revocable handle checked by a broker. The VM sandbox is Phase 3b. The plan for later phases is in
[`Agent_OS_v1_Build_Plan.md`](Agent_OS_v1_Build_Plan.md).

## Build and test

Everything runs in Docker through compose (Rust toolchain, `python3` and `git` are in the
image):

```sh
docker compose run --rm test cargo test --workspace          # all tests
docker compose run --rm test cargo test -p agentos-engine --test crash_matrix   # crash/recovery matrix
docker compose run --rm test cargo test -p agentos-engine --test supervisor     # supervisor, leases, kills
docker compose run --rm test cargo build -p agentos-cli     # the `agentos` binary
```

### The KVM tier (Firecracker)

Needs `/dev/kvm` on the host. Nothing is installed on the host: Firecracker and the jailer go
to `build/firecracker/v1.17.0/` (git-ignored), the guest image to the `guest-images` compose
volume.

```sh
docker compose run --rm test sh scripts/fetch-firecracker.sh    # pinned v1.17.0, sha256-verified
docker compose run --rm test-kvm sh scripts/build-guest-image.sh \
    guest/python-stdlib-v1 build/guest-images/python-stdlib-v1 --verify   # builds twice, cmp
docker compose run --rm test-kvm sh scripts/demo.sh --worker firecracker
```

`test-kvm` extends `test` with what the jailer needs inside a container, and nothing more
(each setting is justified in `compose.yaml` by the spec's jailer experiments): `/dev/kvm`;
`CAP_SYS_ADMIN` (mount namespace, `pivot_root`, the cgroup remount);
`scripts/kvm-entrypoint.sh`, which remounts the container's cgroup tree read-write, moves
every process into a leaf `init/` and delegates `+cpu +memory +pids`; `apparmor=unconfined`;
and the seccomp profile `scripts/kvm-seccomp.json`. Docker's default profile is an allow-list
without `pivot_root`, so the jailer cannot run under it. `kvm-seccomp.json` is the reverse:
it allows every call except 46 host-dangerous ones, which it fails with `EPERM`. Docker's
default profile also refuses these unless an extra capability enables them; here they stay
refused even though the container has `CAP_SYS_ADMIN`. They cover kernel module and
kexec loading, clock and hostname setting, keyrings, `bpf`, `perf_event_open`,
`process_vm_*`, `setns`, `reboot`, swap, `userfaultfd`, NUMA policy and the obsolete
`vm86`/`uselib`/`ustat` calls. `pivot_root`, `mount`, `umount2` and `unshare` stay allowed.

## Demo: kill the controller, restart it, recover the task

The fixtures contain a small Python repository with a buggy `parse_kv`
(`fixtures/parser-repo`), the patch that fixes it (`fixtures/parser-repo.fix.patch`), and a
protected verification profile (`fixtures/profiles/parser-checks-v1`).

The commands:

```sh
agentos profile register fixtures/profiles/parser-checks-v1
agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at during-execute:apply_patch
agentos status <id>
agentos resume <id>
agentos events <id>
agentos export <id> bundle
agentos revoke <id> --capability artifact.export
```

`--crash-at POINT[:KIND][:N]` is a debug flag. It really kills the process (exit code 75)
at an engine crash point:
- `after-agent-turn-journaled`, `after-intent`, `after-dispatch`, `during-execute`,
  `after-execute-before-publish`, `after-blob-put`, `after-register` or `after-complete`;
- optionally for one effect kind (`read_snapshot`, `apply_patch`, `run_verification`);
- on the N-th pass.

`resume` without `--fake-agent-patch` reuses the patch recorded at submission.

`agentos revoke <id> [--capability NAME]` withdraws a task's capability handles (all, or one by
its contract name) and stops the running jobs that need them; it works while another process
drives the task. `agentos profile register DIR` copies a verification profile into the
read-only, content-addressed registry (`<home>/registry/<id>@<digest>/`); `profile list`
shows the entries. A contract may pin one with `"profile_digest"`.

To reproduce, run [`scripts/demo.sh`](scripts/demo.sh) with
`docker compose run --rm test sh scripts/demo.sh`, or run these steps yourself:

1. Write `task.json`, with `repository.source` pointing at `fixtures/parser-repo` and
   `"revision": "recorded-at-submission"`.
2. Register the profile (`agentos profile register`), or pass `--profiles fixtures/profiles`
   (the legacy layout: one `<id>/` directory per profile).

Below is a real transcript from `docker compose run --rm test sh scripts/demo.sh` (home
`/tmp/demo/home`; the `agentos` invocations omit the `--home` flag). The controller is killed
(exit code 75) right after it launched the patch job. That job runs under its own supervisor,
which outlives the controller, so `resume` finds the finished job's receipt and publishes it
without running the patch a second time:

```text
$ agentos profile register /work/fixtures/profiles/parser-checks-v1
{"digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","id":"parser-checks-v1"}

$ agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at during-execute:apply_patch
task 01a0fce7-b255-712c-89e4-a1a60142861c submitted; approve these permissions before it runs:
  goal:                 fix the parser
  repository:           /work/fixtures/parser-repo at be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8
  capabilities:         snapshot.read, workspace.apply_patch, verification.run, artifact.export
  editable paths:       src/**
  acceptance:           protected verification profile parser-checks-v1 (9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c)
  limits:               model_requests=1 max_output_tokens_per_request=1000 tool_actions=10 deadline_seconds=600 worker_vcpus=1 worker_memory_mib=256
  agent:                fake-agent (patch 5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5)
2026-10-02T13:57:18.132403Z  WARN agentos_engine::crash: injected crash point=DuringExecute kind=Some("apply_patch") occurrence=0
{"crashed":"during-execute","task_id":"01a0fce7-b255-712c-89e4-a1a60142861c"}
(exit code 75)

$ agentos status 01a0fce7-b255-712c-89e4-a1a60142861c
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1790950038,"handle_prefix":"ef5ac276","operation":"snapshot.read","revoked":false},{"expires_ts":1790950038,"handle_prefix":"1a34a560","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1790950038,"handle_prefix":"d3546963","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"a6d33b87","operation":"artifact.export","revoked":false}],"jobs":[{"alive":true,"attempt_id":"42e42d01-5a4b-41f7-b221-3da5f5d7f098","effect_id":"d890d2564cf7f5ecb330a9dff45b7585c8a485d1f86be83fc2f56e04cce1f96b","lease_generation":1,"receipt":false,"state":"Running"}],"outstanding_effects":[{"effect_id":"d890d2564cf7f5ecb330a9dff45b7585c8a485d1f86be83fc2f56e04cce1f96b","kind":"apply_patch","lease_generation":1,"state":"Dispatched"}],"state":"RUNNING","step":4,"task_id":"01a0fce7-b255-712c-89e4-a1a60142861c","usage":{"reserved_model_requests":0,"reserved_tool_actions":1,"settled_model_requests":0,"settled_tool_actions":1,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":null,"workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8"}

$ agentos resume 01a0fce7-b255-712c-89e4-a1a60142861c
{"state":"SUCCEEDED","task_id":"01a0fce7-b255-712c-89e4-a1a60142861c"}

$ agentos status 01a0fce7-b255-712c-89e4-a1a60142861c
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1790950038,"handle_prefix":"ef5ac276","operation":"snapshot.read","revoked":false},{"expires_ts":1790950038,"handle_prefix":"1a34a560","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1790950038,"handle_prefix":"d3546963","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"a6d33b87","operation":"artifact.export","revoked":false}],"jobs":[],"outstanding_effects":[],"state":"SUCCEEDED","step":7,"task_id":"01a0fce7-b255-712c-89e4-a1a60142861c","usage":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ agentos events 01a0fce7-b255-712c-89e4-a1a60142861c      # one JSON object per line; shown here as seq + type
1 TaskCreated
2 Submitted
3 CapabilitiesIssued
4 Started
5 CapabilityGranted
6 EffectIntended
7 ActionUsed
8 CapabilityGranted
9 EffectDispatched
10 ArtifactRegistered
11 EffectCompleted
12 WorkspaceUpdated
13 AgentTurn
14 CapabilityGranted
15 EffectIntended
16 ActionUsed
17 ArtifactRegistered
18 CapabilityGranted
19 EffectDispatched
20 RecoveryDecision
21 ArtifactRegistered
22 EffectCompleted
23 WorkspaceUpdated
24 AgentTurn
25 VerifyStarted
26 CapabilityGranted
27 EffectIntended
28 CapabilityGranted
29 EffectDispatched
30 ArtifactRegistered
31 EffectCompleted
32 VerifyPassed

$ agentos export 01a0fce7-b255-712c-89e4-a1a60142861c bundle
{"base_revision":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","base_workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","capabilities":[{"handle_prefix":"ef5ac276","operation":"snapshot.read","revoked":false},{"handle_prefix":"1a34a560","operation":"workspace.apply_patch","revoked":false},{"handle_prefix":"d3546963","operation":"verification.run","revoked":false},{"handle_prefix":"a6d33b87","operation":"artifact.export","revoked":false}],"contract_digest":"a8d51f26113c60185b2921eaa2c02c76cfbfa24b21b7c3872b1515832adcfb90","final_workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","generated_events":33,"model":"fake-agent","patch_digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","patches":[{"digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","effect_id":"d890d2564cf7f5ecb330a9dff45b7585c8a485d1f86be83fc2f56e04cce1f96b","file":"patches/0001-5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5.patch"}],"state":"SUCCEEDED","task_id":"01a0fce7-b255-712c-89e4-a1a60142861c","usage_summary":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verification_profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","verification_results":[{"accepted_for_final_workspace":true,"completed":true,"effect_id":"6110cae47f3aeb8bf059ec17c65465020fe0c3b850c60ffdfb2d59c91659ec01","evidence_digest":"b00dcca26345f586f6981baf74cf6fb8a16fe0aacd790c278421d2d0a42c4303","exit_code":0,"passed":true,"profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}],"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ ls bundle
evidence
manifest.json
patch.diff
patches
$ ls home/jobs      # one directory per effect attempt
6110cae4…-535cc752…
756a2e8d…-8a62cf41…
d890d256…-42e42d01…

$ agentos revoke 01a0fce7-b255-712c-89e4-a1a60142861c --capability artifact.export
{"cancelled_jobs":0,"revoked":["artifact.export"],"task_id":"01a0fce7-b255-712c-89e4-a1a60142861c"}

$ agentos export 01a0fce7-b255-712c-89e4-a1a60142861c bundle-after-revoke
agentos: export denied: capability artifact.export is not usable (revoked)
(exit code 1)
```

What happened in that run:
- Every capability was issued as a handle at `--yes` (`CapabilitiesIssued`, prefixes only), and
  every intent and dispatch was authorized by the broker (`CapabilityGranted`).
- The controller died during the patch job (seq 19, `EffectDispatched`). The job's supervisor
  kept running and wrote the receipt into its job directory.
- `resume` found that receipt and published it (seq 20 `RecoveryDecision`, `PublishRetained`;
  there is exactly one job directory per effect), then the run went on to a verified SUCCEEDED.
- The export bundle is the same as an uncrashed run's (the CLI tests check this for every crash
  point). Its manifest lists the task's capabilities by 8-character prefix only.
- After `revoke`, `export` is denied (`revoked`) and the denial is journaled.

## Home layout

```text
<home>/agentos.db          journal (SQLite, WAL; PRAGMA user_version = schema version 2)
<home>/blobs/              content-addressed artifacts (patches, results, evidence)
<home>/jobs/<effect>-<attempt>/   one directory per effect attempt (see below)
<home>/work/<task>/ws      task workspaces
<home>/tasks/<task>/       inputs recorded at submission: snapshot/, profile/, agent.patch
<home>/registry/<id>@<digest>/    registered verification profiles (read-only)
<home>/registry/<id>@<digest>.meta.json   registration time, outside the digest
<home>/driver.lock         held by the one process driving tasks (running or recovering)
<home>/profiles/<id>/      legacy profile directories (default for --profiles)
```

`<home>` defaults to `~/.agentos`; override it with `--home`.

A job directory holds the whole life of one attempt: `request.json` (written by the
controller; includes the lease and deadline), `status.json` (`Starting`, `Running`, `Exited`,
`Killed` with a reason), `receipt.json` and `output.bin` (the durable receipt, written before
the status turns terminal), `outcome.json` (what the worker produced), `cancel` (a marker that
asks the supervisor to stop), `lock`, `groups` and `supervisor.log`. Liveness is decided by an
`flock` the supervisor holds, never by pids.

## Architecture

- **`agentos-core`**: the task contract, the pure task state machine (cancel always wins;
  success needs evidence for the final workspace), the effect model (stable effect ids,
  retry policies, receipt verdicts), budgets, and the broker's pure rules (opaque handles,
  scopes, `authorize`, lease arithmetic).
- **`agentos-store`**: the SQLite journal. Every state change and its event commit in one
  transaction; effects have reservations, leases and receipts. Also the persisted capability
  handles (issued at approval, checked and journaled at intent, dispatch and export, revocable)
  and the content-addressed blob store.
- **`agentos-engine`**:
  - the run loop, with agent turns journaled and replayed;
  - the effect steps;
  - crash injection and `recover`, which waits for and fences live jobs;
  - export;
  - the job-directory protocol, the `Worker` trait (a host-process worker and a scripted
    test worker), the per-job supervisor (`agentos-supervisor`) and `SupervisedExecutor`;
  - the fixture executor the host worker runs.
- **`agentos-cli`**: the `agentos` binary (`submit`, `status`, `events`, `pause`, `resume`,
  `cancel`, `revoke`, `profile`, `export`). Each command is one short-lived controller
  process. The supervisor and its worker are this same binary, re-executed through hidden
  `supervise run|worker` subcommands.

The **`Executor` trait** (`run`, `retained_outcome`, `reconcile`, `current_workspace`,
`await_job`, `fence_job`) is the backend seam; `SupervisedExecutor` implements it over job
directories. The **`Worker` trait** is the seam below it: Phase 3b puts a Firecracker worker
there.

Every effect attempt is a job owned by a per-job supervisor. It is launched detached (its own
session), enforces the lease (`now + min(effect timeout, deadline - now)`) and the task
deadline by killing the worker's process groups, honours the `cancel` marker, writes the
receipt, and exits. A controller that dies leaves its jobs running; recovery waits for a live
job until its lease plus a grace period, and only then fences it. A verification killed at its
lease or deadline yields a failure receipt; a killed patch yields none and is reconciled, never
recorded as failed.

## Known limits (v0.1, Phases 1-3a)

- **Not sandboxed.** Repository and verification code runs on the host as the same UID.
  Do not run untrusted repositories or patches until the Phase 3b VM sandbox exists. The
  supervisor kills process groups it knows about; a process that escapes with `setsid` is not
  tracked.
- **`reconcile` runs in the controller.** Deciding whether a receipt-less patch applied reads
  the host workspace from the controller process; Phase 3b moves it into the guest.
- **A revoke can miss a job that is just starting.** A revoke that lands between a dispatch
  being authorized and the job directory being created finds no job to stop; that job runs to
  completion, and the next request is denied.
- **`Denied` rows are forgeable.** They are audit rows any caller can append; only the
  `Capability*` rows are written by the broker itself.
- **No job-directory garbage collection.** `<home>/jobs` grows with every attempt.
- **Export bypasses the effect model.** It is authorized through the broker
  (`artifact.export`, journaled, revocable) but is not a journaled effect; it leaves an
  `Exported` audit row.
- **Host paths can leak into exports.** Exported verification evidence (stdout/stderr) can
  contain absolute host paths.
- **No real model.** The model call is a stand-in: only the fake agent exists, and the real
  model adapter is Phase 4.
- **One driver per home.** Only one process drives a home at a time (`driver.lock`).
  Recovery's blob garbage collection assumes no concurrent writers.
- **Resume needs the same patch.** `resume --fake-agent-patch` must be given the same
  patch again. A different one makes the journal replay diverge, which fails the task.
- **No migration.** A database written by an earlier build (`user_version < 2`) is refused.
- **Reserved tables.** `tasks.checkpoint` is always NULL, because journal replay is the
  checkpoint mechanism. The `observations` table is reserved.

## Phase 3b (not built yet)

A Firecracker worker behind the `Worker` trait: guest image build and registration, per-VM
vCPU and memory limits, no guest network, no host secrets, the workspace (and `reconcile`)
moved into the guest, and handles passed to the guest. Needs a KVM check and the Firecracker
binary. Until then the build plan's rows "guest attempts network or host secret access" and
"repository code stays within configured CPU and memory" are not met.
