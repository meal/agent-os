# Agent OS

A controller that runs a coding agent against a repository under a signed-off task
contract. Every side effect goes through a journal: it is intended, dispatched, executed,
published and committed as separate durable steps. A task can only reach SUCCEEDED
through a protected verification of exactly the final workspace. The controller can be
killed at any of those boundaries, and a restarted one recovers the same task without
repeating completed effects.

This is milestone **v0.1, Phases 1-2**. The agent is a deterministic fake that applies a
given patch. Code runs in a local-directory executor. The plan for later phases is in
[`Agent_OS_v1_Build_Plan.md`](Agent_OS_v1_Build_Plan.md).

## Build and test

Everything runs in Docker through compose (Rust toolchain, `python3` and `git` are in the
image):

```sh
docker compose run --rm test cargo test --workspace          # all tests
docker compose run --rm test cargo test -p agentos-engine --test crash_matrix   # crash/recovery matrix
docker compose run --rm test cargo build -p agentos-cli     # the `agentos` binary
```

## Demo: kill the controller, restart it, recover the task

The fixtures contain a small Python repository with a buggy `parse_kv`
(`fixtures/parser-repo`), the patch that fixes it (`fixtures/parser-repo.fix.patch`), and a
protected verification profile (`fixtures/profiles/parser-checks-v1`).

The commands:

```sh
agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at after-dispatch:apply_patch
agentos status <id>
agentos resume <id>
agentos events <id>
agentos export <id> bundle
```

`--crash-at POINT[:KIND][:N]` is a debug flag. It really kills the process (exit code 75)
at an engine crash point:
- `after-agent-turn-journaled`, `after-intent`, `after-dispatch`, `during-execute`,
  `after-execute-before-publish`, `after-blob-put`, `after-register` or `after-complete`;
- optionally for one effect kind (`read_snapshot`, `apply_patch`, `run_verification`);
- on the N-th pass.

`resume` without `--fake-agent-patch` reuses the patch recorded at submission.

To reproduce, run [`scripts/demo.sh`](scripts/demo.sh) with
`docker compose run --rm test sh scripts/demo.sh`, or run these steps yourself:

1. Write `task.json`, with `repository.source` pointing at `fixtures/parser-repo` and
   `"revision": "recorded-at-submission"`.
2. Pass `--profiles fixtures/profiles`, or copy the profile into `<home>/profiles/`.

Below is a real transcript from `docker compose run` (home `/tmp/demo/home`, profiles
`/work/fixtures/profiles`; the `agentos` invocations omit the `--home/--profiles` flags):

```text
$ agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at after-dispatch:apply_patch
task 01a0f9dc-e355-7693-a819-a1253b43eeef submitted; approve these permissions before it runs:
  goal:                 fix the parser
  repository:           /work/fixtures/parser-repo at be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8
  capabilities:         snapshot.read, workspace.apply_patch, verification.run, artifact.export
  editable paths:       src/**
  acceptance:           protected verification profile parser-checks-v1 (9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c)
  limits:               model_requests=1 max_output_tokens_per_request=1000 tool_actions=10 deadline_seconds=600 worker_vcpus=1 worker_memory_mib=256
  agent:                fake-agent (patch 5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5)
2026-10-01T23:46:38.058538Z  WARN agentos_engine::crash: injected crash point=AfterDispatch kind=Some("apply_patch") occurrence=0
{"crashed":"after-dispatch","task_id":"01a0f9dc-e355-7693-a819-a1253b43eeef"}
(exit code 75)

$ agentos status 01a0f9dc-e355-7693-a819-a1253b43eeef
{"actions_used":2,"cancel_requested":false,"outstanding_effects":[{"effect_id":"e1350b3cb065fd28caab0166352dfca88b91e9eec453ec15ae624fa147bace5a","kind":"apply_patch","lease_generation":1,"state":"Dispatched"}],"state":"RUNNING","step":4,"task_id":"01a0f9dc-e355-7693-a819-a1253b43eeef","usage":{"reserved_model_requests":0,"reserved_tool_actions":1,"settled_model_requests":0,"settled_tool_actions":1,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":null,"workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8"}

$ agentos resume 01a0f9dc-e355-7693-a819-a1253b43eeef
{"state":"SUCCEEDED","task_id":"01a0f9dc-e355-7693-a819-a1253b43eeef"}

$ agentos status 01a0f9dc-e355-7693-a819-a1253b43eeef
{"actions_used":2,"cancel_requested":false,"outstanding_effects":[],"state":"SUCCEEDED","step":7,"task_id":"01a0f9dc-e355-7693-a819-a1253b43eeef","usage":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ agentos events 01a0f9dc-e355-7693-a819-a1253b43eeef      # one JSON object per line; shown here as seq + type
1 TaskCreated
2 Submitted
3 Started
4 EffectIntended
5 ActionUsed
6 EffectDispatched
7 ArtifactRegistered
8 EffectCompleted
9 WorkspaceUpdated
10 AgentTurn
11 EffectIntended
12 ActionUsed
13 ArtifactRegistered
14 EffectDispatched
15 RecoveryDecision
16 EffectDispatched
17 ArtifactRegistered
18 EffectCompleted
19 WorkspaceUpdated
20 AgentTurn
21 VerifyStarted
22 EffectIntended
23 EffectDispatched
24 ArtifactRegistered
25 EffectCompleted
26 VerifyPassed

$ agentos export 01a0f9dc-e355-7693-a819-a1253b43eeef bundle
{"base_revision":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","base_workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","contract_digest":"a8d51f26113c60185b2921eaa2c02c76cfbfa24b21b7c3872b1515832adcfb90","final_workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","generated_events":26,"model":"fake-agent","patch_digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","patches":[{"digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","effect_id":"e1350b3cb065fd28caab0166352dfca88b91e9eec453ec15ae624fa147bace5a","file":"patches/0001-5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5.patch"}],"state":"SUCCEEDED","task_id":"01a0f9dc-e355-7693-a819-a1253b43eeef","usage_summary":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verification_profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","verification_results":[{"accepted_for_final_workspace":true,"completed":true,"effect_id":"42495d55e062f072b46a5bedff541b29d99646d88a886a152752dbc066661332","evidence_digest":"b00dcca26345f586f6981baf74cf6fb8a16fe0aacd790c278421d2d0a42c4303","exit_code":0,"passed":true,"profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}],"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ ls bundle
evidence
manifest.json
patch.diff
patches
```

What happened in that run:
- The process died with the patch DISPATCHED (seq 14).
- `resume` found no retained receipt, so it reconciled the patch against the workspace. The patch was proven not applied, so it was re-dispatched (seq 15 `RecoveryDecision`, seq 16).
- The agent's journaled turn was replayed, and the run went on to a verified SUCCEEDED.
- The export bundle is the same as an uncrashed run's (the CLI tests check this for every crash point).

## Home layout

```text
<home>/agentos.db          journal (SQLite, WAL; PRAGMA user_version = schema version)
<home>/blobs/              content-addressed artifacts (patches, results, evidence)
<home>/receipts/           executor receipts retained across controller restarts
<home>/work/<task>/ws      task workspaces
<home>/tasks/<task>/       inputs recorded at submission: snapshot/, profile/, agent.patch
<home>/driver.lock         held by the one process driving tasks (running or recovering)
<home>/profiles/<id>/      verification profile registry (default for --profiles)
```

`<home>` defaults to `~/.agentos`; override it with `--home`.

## Architecture

- **`agentos-core`**: the task contract, the pure task state machine (cancel always wins;
  success needs evidence for the final workspace), the effect model (stable effect ids,
  retry policies, receipt verdicts) and budgets.
- **`agentos-store`**: the SQLite journal. Every state change and its event commit in one
  transaction; effects have reservations, leases and receipts. Also the content-addressed
  blob store.
- **`agentos-engine`**:
  - the run loop, with agent turns journaled and replayed;
  - the effect steps;
  - crash injection and `recover`;
  - export;
  - the fixture executor.
- **`agentos-cli`**: the `agentos` binary (`submit`, `status`, `events`, `pause`, `resume`,
  `cancel`, `export`). Each command is one short-lived controller process.

The **`Executor` trait** (`run`, `retained_outcome`, `reconcile`, `current_workspace`) is
the backend seam. The fixture executor runs effects in local directories, and the Phase 3
VM/Wasm backends plug in at the same place.

## Known limits (v0.1, Phases 1-2)

- **Not sandboxed.** Repository and verification code runs on the host as the same UID.
  Do not run untrusted repositories or patches until the Phase 3 VM sandbox exists.
- **Orphaned verification processes.** A verification process can survive a real
  controller kill (SIGKILL or exit): the process-group kill runs only when the check
  completes or times out. Recovery may then start a second check on the same workspace.
- **No deadline enforcement.** The task deadline (`deadline_ts`) is stored but not
  enforced.
- **No profile pinning.** Verification profile digest pinning and registration are
  deferred to Phase 3. The profile is copied and digested at submission, and every run
  records its digest.
- **Export bypasses the effect model.** It is not capability-checked (`artifact.export`)
  and is not a journaled effect; it leaves only an `Exported` audit row.
- **Host paths can leak into exports.** Exported verification evidence (stdout/stderr) can
  contain absolute host paths.
- **No real model.** The model call is a stand-in: only the fake agent exists, and the real
  model adapter is Phase 4.
- **One driver per home.** Only one process drives a home at a time (`driver.lock`).
  Recovery's blob garbage collection assumes no concurrent writers.
- **Resume needs the same patch.** `resume --fake-agent-patch` must be given the same
  patch again. A different one makes the journal replay diverge, which fails the task.
- **Reserved tables.** `tasks.checkpoint` is always NULL, because journal replay is the
  checkpoint mechanism. The `capabilities` and `observations` tables are reserved for
  Phase 3.
