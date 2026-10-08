# Acceptance evidence

Real-provider and real-KVM acceptance evidence for the v0.1 completion branch was collected
on 2026-10-08 (see [2026-10-08/](2026-10-08/)). Skipped gated tests and fake-jail runs
still do not establish provider or isolation acceptance.

## 2026-10-08: Task 7 results

| Run | Commit | Result |
| --- | --- | --- |
| `acceptance.sh kvm`, first attempt | `825d360` | **Failed**: 114/115 in `agentos-cli` under the real worker. `api_key_reaches_only_the_provider…` asserted `PATH=` in an environment dump that is empty in a real guest (the parent is init). Not a key leak: `api_key_never_reaches_the_guest` passed. Kept in `kvm-failed-attempt/`. |
| `acceptance.sh kvm`, after the test fix | `9de2618` | Passed: full workspace suite and the `AGENTOS_TEST_WORKER=firecracker` suite (`kvm-pass/`). |
| `acceptance.sh live`, host worker | `825d360-dirty` (the dirty change is the test fix above) | `claude-opus-5-5`, `SUCCEEDED`, 5 model calls, final workspace digest equals verified digest; offline replay matched. |
| `acceptance.sh live`, jailed Firecracker worker | `825d360-dirty` | Same model, `SUCCEEDED`, 4 model calls; offline replay matched. |

Candidate image `python-stdlib-py314-v1` (Task 5), run with
`AGENTOS_ACCEPTANCE_IMAGE=python-stdlib-py314-v1 sh scripts/acceptance.sh kvm`:

| Run | Commit | Result |
| --- | --- | --- |
| First attempt | `97d0f54` | **Failed**: 113/115 in `agentos-cli`; the CLI tests hard-coded the default image id as the contract `profile`. The engine suites never ran. |
| Second attempt | `2ee9c45` | Every stage passed, but **invalid**: checks ran Debian's Python 3.11.2, because the guest's check `PATH` is `/usr/bin:/bin` and the recipe linked 3.14.8 only into `/usr/local/bin`. |
| After the recipe fix | `f175de3` | Passed: two byte-identical builds (digest `2ccceaa0…fefeb`), the full workspace suite and the `AGENTOS_TEST_WORKER=firecracker` suite, 986 passed and 0 failed in each. The guest reports Python 3.14.8, matching `image.json` and `runtime/python.lock`. |

Both earlier attempts are in `kvm-py314-failed-attempts/`, the passing run with its
`image.json` in `kvm-py314-pass/`. A new KVM test,
`the_guest_interpreter_is_the_one_the_image_manifest_records`, now checks the interpreter
the guest actually runs against the manifest for every image. The candidate is accepted but
not the default; the interpreter is copied from the pinned pyenv build, not rebuilt
independently from source (see the recipe README).

Task 9 (contract-driven VM disks and I/O):

| Run | Commit | Result |
| --- | --- | --- |
| `acceptance.sh kvm` | `b58087a` | **Failed**: one engine unit test raced a lock release against another test's process spawn (`kvm-task9-failed-attempt/`). Not a Task 9 change. |
| `acceptance.sh kvm`, after the test fix | `7955b96` | Passed: both suites, 1018 passed and 0 failed in each (`kvm-task9-pass/`). |

The memory measurements behind the jail's 128 MiB overhead are in `vm-memory/`: the
hostile disk-fill check at 256 and 1024 MiB of guest memory, alone and four at once. The
smallest headroom of memory reclaim cannot drop was 61 MiB, and no case recorded an OOM kill.

Task 10 (the component analyzer), `2026-10-09/`:

| Run | Commit | Result |
| --- | --- | --- |
| `acceptance.sh kvm` | `d33e82e` | **Failed** before any test ran: the host's root disk was full (`kvm-task10-failed-attempt/`). |
| `acceptance.sh kvm`, after freeing disk space | `d33e82e` | Passed: both suites, 1045 passed and 0 failed in each, including the analyzer end to end on the real jailed worker (`kvm-task10-pass/`). |

Image `python-stdlib-v1`; Firecracker v1.17.0; kernel 6.18.51. The live runs predate the test
fix, which changes only a test assertion. Recordings (schema version 2) are in
`fixtures/transcripts/live/`; each worker's manifest, `patch.diff`, success report and replay
report are under `2026-10-08/host` and `2026-10-08/firecracker`.

The host run's one `Denied` event is the broker refusing the model's first patch as
`InvalidPatch`: its hunk header counted 8 old and 9 new lines where the body had 7 and 8,
so `git apply` reported a corrupt patch. The refusal created no effect and used no tool
action (both runs show 4 `ActionUsed`; the host run has one more model call and one more
`EffectIntended` than the jailed run, for the corrected patch). The model fixed the hunk
on its next turn. `happy_path::a_miscounted_hunk_is_denied_before_any_effect_and_costs_no_action`
replays that patch verbatim and reproduces the same request digest.

Secret exclusion and consistency are now checked by `cargo test --test evidence` in the
default tier. Every file under `docs/evidence/` (except prose) and `fixtures/transcripts/`
is scanned for `sk-ant-`, `x-api-key`, `authorization`, `bearer ` and full 32-digit
capability handles. Recordings store provider response bodies as byte arrays, so the scan
decodes them; a text search would not see them. Each promoted run's success report,
replay report, manifest, `patch.diff` (by BLAKE3), recording and setup commit must agree.
The live harness also scans its recording and bundle for the exact key bytes before it
writes a success report. The method of the first manual scan was not
recorded; the decoded scan above finds nothing in these runs. Two-snapshot
fresh-host evidence remains open.

Run from a normal checkout on the provisioned Linux x86_64 Docker host:

```sh
sh scripts/check.sh
sh scripts/acceptance.sh offline
sh scripts/acceptance.sh kvm
sh scripts/acceptance.sh live /absolute/path/to/anthropic-key
# Separately accept the candidate interpreter; the legacy image remains the default.
AGENTOS_ACCEPTANCE_IMAGE=python-stdlib-py314-v1 sh scripts/acceptance.sh kvm
```

KVM/live require usable `/dev/kvm` and the cgroup/jailer setup described in the README.
They build and verify the selected guest twice before running tests. Live uses the
existing bounded parser-repair contract (12 calls, 16000 output tokens/call, 900 seconds)
and mounts the regular key file read-only; it never passes key contents in arguments or
environment variables. Offline tests use clearly fake keys and a loopback fake API.

Each invocation writes private logs and setup metadata to a distinct `build/evidence/`
directory. `AGENTOS_ACCEPTANCE_OUTPUT` can select a durable runner path. Live additionally
writes recordings, an export bundle, a structured success report, and an offline replay
report per worker. Recordings use schema version 2, preserving every ordered provider attempt, raw response,
usage and rejection/retry metadata; legacy response-only fixtures retain their depth semantics.
Replay asserts exact request digests, call count, final workspace,
protected profile, and exported patch. A failed test exits nonzero; its log and any
completed recording/export remain for diagnosis. A replay report exists only after its
assertions pass. Preserve failed-attempt logs alongside successful results.

The manual GitHub workflow runs only at `refs/heads/main` on the self-hosted label
`agentos-kvm`, through the protected `agentos-acceptance` environment. Configure that
environment's approval rules and `AGENTOS_LIVE_KEY_FILE` repository variable with the
runner's absolute key-file path. Provision the runner's durable evidence storage before
invoking the workflow. These jobs never run for pull requests. Branch-protection required
checks and environment approval rules require repository administrator configuration;
workflow files alone do not configure them.

Before promoting evidence into this directory or recordings into fixtures, check secret
exclusion, record image/kernel/Firecracker and commit provenance, inspect failures, and
re-estimate packages 8–12. Fresh-host release evidence across two snapshots, real VM
resource measurements, and the new component ABI remain separate gates in the master
plan. A copied pyenv interpreter plus two identical guest outputs does not prove an
independent Python source rebuild; that provenance limit remains documented in the
candidate recipe.
