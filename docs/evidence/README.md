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

Image `python-stdlib-v1`; Firecracker v1.17.0; kernel 6.18.51. The live runs predate the test
fix, which changes only a test assertion. Recordings (schema version 2) are in
`fixtures/transcripts/live/`; each worker's manifest, `patch.diff`, success report and replay
report are under `2026-10-08/host` and `2026-10-08/firecracker`. Every file was scanned for the
key bytes, `sk-ant` and `x-api-key`: none found. The host run recorded one `Denied` event
that has not been examined. The candidate `python-stdlib-py314-v1` image, two-snapshot
fresh-host evidence, VM resource measurements and the component ABI remain open.

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
