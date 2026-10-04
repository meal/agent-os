# Acceptance evidence

No real-provider or real-KVM acceptance evidence has been collected for the v0.1
completion branch. Offline suites and local fake-provider recordings exercise the harness;
skipped gated tests and fake-jail runs do not establish provider or isolation acceptance.
The user deferred live/KVM setup on 2026-10-04.

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
report per worker. Replay asserts exact request digests, call count, final workspace,
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
