# Agent OS

A controller that runs a coding agent against a repository under a signed-off task
contract. Every side effect goes through a journal: it is intended, dispatched, executed,
published and committed as separate durable steps. A task can only reach SUCCEEDED
through a protected verification of exactly the final workspace. The controller can be
killed at any of those boundaries, and a restarted one recovers the same task without
repeating completed effects.

This is milestone **v0.1, Phases 1-3b-1**. The agent is a deterministic fake that applies a
given patch. Every effect is a job under its own supervisor, and every capability is an
opaque, revocable handle checked by a broker. By default effects run as host processes (the
`host` worker, not sandboxed). The VM sandbox is opt-in per task with `--worker firecracker`:
one Firecracker microVM per effect, booted from a registered guest image, with no network
device, and **jailed by default** (the official `jailer`: chroot, uid 61000, cgroup v2
limits). The plan for later phases is in [`Agent_OS_v1_Build_Plan.md`](Agent_OS_v1_Build_Plan.md).

## Build and test

Everything runs in Docker through compose (Rust toolchain, `python3` and `git` are in the
image):

```sh
docker compose run --rm test cargo test --workspace          # all tests (default tier)
docker compose run --rm test cargo test -p agentos-engine --test crash_matrix   # crash/recovery matrix
docker compose run --rm test cargo test -p agentos-engine --test supervisor     # supervisor, leases, kills
docker compose run --rm test cargo build -p agentos-cli     # the `agentos` binary
```

The default tier needs neither KVM nor privilege beyond the container's own. Every KVM test
prints `SKIPPED: set AGENTOS_KVM_TESTS=1 and pass /dev/kvm (docker compose run --rm test-kvm …)`
and returns.

### Test tiers

The engine and CLI tests pick the worker they drive from `AGENTOS_TEST_WORKER`:

| Command | Worker under test | What it proves |
| --- | --- | --- |
| `docker compose run --rm test cargo test --workspace` | `host` (default) | the 3a behaviour, unchanged |
| `… -e AGENTOS_TEST_WORKER=firecracker-fake test cargo test --workspace` | `FirecrackerWorker` with the **fake guest** (`agentos-supervisor fake-guest`: the real guest agent's session code over a Unix socket, the workspace in a host directory) | the host side only: the vsock protocol and its limits, `vm.json`, the worker's state machine and failure mapping, `ws.lock`, inspection (`reconcile`, `current_workspace`), the crash matrix, kill/lease/deadline/revoke paths, byte-identical outcomes. It proves nothing about the real guest (mounts, uid separation, OOM, no NIC, read-only root) or the real jail. |
| `… -e AGENTOS_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test cargo test --workspace` | as above, wrapped in `JailMode::Jailed` with a **fake jailer** (a `/bin/sh` script that parses the documented argv, records it, creates a directory standing in for the cgroup and execs the fake guest) | engine: the jailed launch path (staging by hard links, ownership to the test's own uid, the chroot `vm.json`, the argv, collection after settlement, leftovers after a SIGKILL). CLI: only the jail **decision and its record** (the probe answers `ok`, `jailed: true` is recorded and enforced on every later command) — the CLI's fake guest still runs unjailed. No real chroot, uid drop or cgroup is involved. |
| `docker compose run --rm test-kvm cargo test --workspace` | every KVM-gated test runs for real (`kvm_tier`, the Real column of `worker_conformance`, two CLI tests); the rest as in the first row | the real guest and the real jail; see below |
| `docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker test-kvm cargo test --workspace` | the real, **jailed** Firecracker worker for the crash matrix, conformance, deadline, revoke, supervised and all CLI tests | every 3a guarantee against real VMs |

Only the KVM tier proves the guest and jail properties: no NIC (`net-probe`), no host secret
(`secret-probe`), vCPU and memory bounds (`cpu-burn`, `mem-hog`), `fork-bomb`, `disk-fill` and a
read-only root, uid separation of `git` (1000) and the check (1001), the jailed process's
uid/chroot/cgroup, and the cgroup bounds (lowered through test seams to show they bite). When
`AGENTOS_KVM_TESTS=1` is set but `/dev/kvm`, Firecracker, the jailer, the image or the jail
probe is unusable, the KVM gate **panics** rather than skipping.

### Worker selection and flags

```sh
agentos --worker firecracker --firecracker PATH --jailer PATH submit task.json --yes
```

| Flag (global) | Env | Meaning |
| --- | --- | --- |
| `--worker host\|firecracker` | `AGENTOS_WORKER` | `submit` uses it (default `host`); every later command uses the worker recorded in `Submitted`, and a `--worker` that disagrees with the record exits 2 (`task was submitted with worker host`) |
| `--firecracker PATH` | `AGENTOS_FIRECRACKER` | the Firecracker binary (default `<home>/bin/firecracker`); `--version` must print `Firecracker v1.17.` |
| `--jailer PATH` | `AGENTOS_JAILER` | the jailer (default: `jailer` next to the Firecracker binary); `--version` must print `Jailer v1.17.` |
| `--jail-uid N`, `--jail-gid N` | `AGENTOS_JAIL_UID`, `AGENTOS_JAIL_GID` | the uid/gid the jailed Firecracker runs as (default 61000, which Debian reserves and never allocates) |
| `--allow-unjailed` | `AGENTOS_ALLOW_UNJAILED=1` | when the jail probe fails, run Firecracker unjailed as the current user instead of refusing; records `jailed: false` and warns on stderr |

`submit` resolves the guest image from the contract's `profile` (e.g. `python-stdlib-v1`): an
optional contract pin `guest_image_digest` selects exactly that registry entry, otherwise the
newest entry for the id. Before anything is journaled, the **preflight** checks `/dev/kvm`
(read-write), `firecracker --version`, the image (`image.json`, `protocol == 1`, the digest),
then the **jail probe** (euid 0, `jailer --version`, a cgroup v2 tree with `cpu memory pids`
that can be delegated, the home on one filesystem without `nodev`/`noexec`). A failure exits 1
with the reason and the task is untouched, e.g. `firecracker worker unavailable: jailer
unavailable: needs root (euid 0), running as uid 1000; pass --allow-unjailed to run Firecracker
without a jail as the current user`. `Submitted` records `worker`, `guest_image_id`,
`guest_image_digest`, `firecracker_version`, `host_kernel` and `jailed`. A task recorded
`jailed: true` is always run jailed (a later probe failure exits 1 `task was submitted jailed:
jailer unavailable: …`, task untouched); a task recorded `jailed: false` stays unjailed.

`agentos image register DIR` copies a guest image directory (`image.json`, `vmlinux`,
`rootfs.squashfs`) into the read-only, content-addressed registry
(`<home>/registry/images/<id>@<digest>/`, root `0444`), dedupes by bytes, refuses ids with `@`,
`/` or traversal, and prints `{"id","digest"}`; `agentos image list` lists the entries. Both
mirror `profile register|list`. The image is re-digested before every launch.

**Running `--worker firecracker` as a non-root user on a host.** The jail needs root and a
writable, delegated cgroup v2 tree, so a plain user gets the "needs root" refusal above. Either
run it the way the KVM tier does (`docker compose run --rm test-kvm …`, below), or pass
`--allow-unjailed`: Firecracker then runs as **your** UID with no chroot and no cgroup, and the
journal records `jailed: false`. The VM boundary (no NIC, only its drives, the vCPU and memory
of the contract, Firecracker's seccomp filter) is the same, but a VM escape (a Firecracker or
KVM bug) lands as your UID with everything it can reach.

### The KVM tier (Firecracker)

Needs `/dev/kvm` on the host. Nothing is installed on the host: Firecracker and the jailer go
to `build/firecracker/v1.17.0/` (git-ignored), the guest image to the `guest-images` compose
volume.

```sh
docker compose build
docker compose run --rm test sh scripts/fetch-firecracker.sh    # pinned v1.17.0 firecracker + jailer, sha256-verified
docker compose run --rm test-kvm sh scripts/build-guest-image.sh \
    guest/python-stdlib-v1 build/guest-images/python-stdlib-v1 --verify   # builds twice, cmp
docker compose run --rm test-kvm cargo test --workspace                            # KVM-gated tests run
docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker test-kvm cargo test --workspace
docker compose run --rm test-kvm cargo test -p agentos-engine --test kvm_tier -- --ignored   # image_build_is_reproducible (slow)
docker compose run --rm test-kvm sh scripts/demo.sh --worker firecracker
```

- `scripts/fetch-firecracker.sh [DEST]` downloads the v1.17.0 release tarball over https,
  verifies the tarball, `firecracker` and `jailer` sha256s and installs both into `DEST`
  (default `build/firecracker/v1.17.0/`). A run with matching files is a no-op.
- `scripts/build-guest-image.sh RECIPE OUT_DIR [--verify]` builds `image.json`, `vmlinux` and
  `rootfs.squashfs` from `guest/python-stdlib-v1/`: the pinned kernel (`kernel.lock`: Firecracker
  CI `vmlinux-6.18.51`, sha256-verified), `agentos-guest` as a static musl binary, `mmdebstrap`
  of Debian bookworm from a pinned `snapshot.debian.org` timestamp, a zstd squashfs with fixed
  times and owners. `--verify` builds twice (the second time from a fresh target directory) and
  fails unless the three files are byte-identical. It needs root and network in the container.
  **Run it in `test-kvm`**: that service mounts the `guest-images` volume on
  `build/guest-images`, so an image built in `test` (into the host's `build/`) is not what the
  KVM tier sees. The `OUT_DIR` guard: since `OUT_DIR` is replaced with `rm -rf`, the script
  refuses (exit 2) an `OUT_DIR` that is `/`, the repository root or an ancestor of it, a symlink,
  a mount point, not a directory, or a directory holding anything but the three image files.
  `--self-test` checks the guard and the `--verify` comparison without building.
- `build/` is created by the container's root; the scripts give it back to the owner of the
  checkout, but anything the container leaves root-owned is removed with
  `docker compose run --rm test rm -rf build/…`.

`test-kvm` extends `test` with what the jailer needs inside a container, and nothing more. Each
setting is justified in `compose.yaml` by the spec's jailer experiments (E1-E12 in
[the design](docs/superpowers/specs/2026-10-02-phase-3b-firecracker-worker-design.md)):

- `devices: /dev/kvm` — the VM (E1; not visible in `test`).
- `cap_add: SYS_ADMIN` — the jailer's `unshare(CLONE_NEWNS)`, `mount` and `pivot_root`, and the
  entrypoint's cgroup remount (E2b/E3). Not `privileged` (E5: that alone does not work either).
- `seccomp=./scripts/kvm-seccomp.json` — Docker's default profile denies `pivot_root` for every
  container, whatever the capabilities (E3/E11). Docker's default profile is an allow-list
  without `pivot_root`; `kvm-seccomp.json` is the reverse: it allows every call except 49
  host-dangerous ones, which it fails with `EPERM` even though the container has
  `CAP_SYS_ADMIN`. They are a subset of what Docker's default profile gates (behind a
  capability, a kernel version or outright): kernel module and kexec loading, clock and
  hostname setting, keyrings, `bpf`, `perf_event_open`, `kcmp`, `process_vm_*`, `setns`,
  `reboot`, swap, `userfaultfd`, `io_uring_*`, NUMA policy and the obsolete
  `vm86`/`uselib`/`ustat` calls. `pivot_root`, `mount`, `umount2` and `unshare` stay allowed.
- `apparmor=unconfined` — for hosts where Docker's AppArmor profile denies `mount` (E12; a
  no-op on a host without AppArmor).
- `entrypoint: scripts/kvm-entrypoint.sh` — cgroup v2 delegation (E2/E4/E5): it remounts the
  container's cgroup tree read-write, moves every process into a leaf `init/` (the "no
  internal process" rule: a cgroup with processes of its own cannot enable controllers for its
  children, `EBUSY`) and writes `+cpu +memory +pids` to the root's `cgroup.subtree_control`. The
  cgroup namespace stays private: the jail's cgroups live under the container's own and die
  with it.
- `environment`: `AGENTOS_KVM_TESTS=1` and the paths `AGENTOS_FIRECRACKER`, `AGENTOS_JAILER`,
  `AGENTOS_GUEST_IMAGE`; `volumes`: the `guest-images` volume.

The `test` service is unchanged; in it the jail probe answers `cgroup v2 hierarchy
/sys/fs/cgroup is read-only`, and no default-tier test writes to `/sys/fs/cgroup`.

Test-only environment variables (all honoured only together with `AGENTOS_TEST_WORKERS=1`,
which the test harness sets for the processes it spawns): `AGENTOS_TEST_JAIL_PROBE=ok|fail:<reason>`
(the CLI's probe answer), `AGENTOS_TEST_FAKE_GUEST=1`, `AGENTOS_TEST_FAKE_GUEST_NEVER_LISTEN=1`,
`AGENTOS_TEST_FAKE_GUEST_HANG_INSPECT=1`, `AGENTOS_TEST_KILL_VM_AFTER_REQUEST=1`, and
`AGENTOS_TEST_FAKE_GUEST_WATCHDOG_MS` (added in Task 11, outside the plan's original list). The
cgroup seams `with_jail_memory_max_mib`/`with_jail_cpu_quota_us` (used by the KVM tier to lower
a bound and watch it bite) are honoured only under the same switch; a KVM test proves they are
ignored without it.

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
`docker compose run --rm test sh scripts/demo.sh` (host worker) or
`docker compose run --rm test-kvm sh scripts/demo.sh --worker firecracker` (jailed Firecracker;
it also registers `$AGENTOS_GUEST_IMAGE`), or run these steps yourself:

1. Write `task.json`, with `repository.source` pointing at `fixtures/parser-repo` and
   `"revision": "recorded-at-submission"`.
2. Register the profile (`agentos profile register`), or pass `--profiles fixtures/profiles`
   (the legacy layout: one `<id>/` directory per profile).

### Host worker

Below is a real transcript from `docker compose run --rm test sh scripts/demo.sh`, run on
2026-10-03 (home `/tmp/demo/home`; the `agentos` invocations omit the `--home` flag; only the two
`Container … Creating/Created` lines docker compose prints were trimmed). The controller is
killed (exit code 75) right after it launched the patch job. That job runs under its own
supervisor, which outlives the controller, so `resume` finds the finished job's receipt and
publishes it without running the patch a second time:

```text
$ agentos profile register /work/fixtures/profiles/parser-checks-v1
{"digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","id":"parser-checks-v1"}

$ agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at during-execute:apply_patch
task 01a10214-8fd8-707b-91ed-8d0bc32062eb submitted; approve these permissions before it runs:
  goal:                 fix the parser
  repository:           /work/fixtures/parser-repo at be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8
  capabilities:         snapshot.read, workspace.apply_patch, verification.run, artifact.export
  editable paths:       src/**
  acceptance:           protected verification profile parser-checks-v1 (9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c)
  limits:               model_requests=1 max_output_tokens_per_request=1000 tool_actions=10 deadline_seconds=600 worker_vcpus=1 worker_memory_mib=256
  agent:                fake-agent (patch 5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5)
  worker:               host (not sandboxed)
2026-10-03T14:04:24.507151Z  WARN agentos_engine::crash: injected crash point=DuringExecute kind=Some("apply_patch") occurrence=0
{"crashed":"during-execute","task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb"}
(exit code 75)

$ agentos status 01a10214-8fd8-707b-91ed-8d0bc32062eb
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1791036864,"handle_prefix":"e7302959","operation":"snapshot.read","revoked":false},{"expires_ts":1791036864,"handle_prefix":"811c930c","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1791036864,"handle_prefix":"8689a287","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"33d86fe4","operation":"artifact.export","revoked":false}],"jobs":[{"alive":true,"attempt_id":"b94ebb51-85b2-40ec-a0b8-87df013d2219","effect_id":"99afe2fbad2030b8e75bb353537cb5c976ba0fb97e01db9de007aa6b9f277696","lease_generation":1,"receipt":false,"state":"Running"}],"outstanding_effects":[{"effect_id":"99afe2fbad2030b8e75bb353537cb5c976ba0fb97e01db9de007aa6b9f277696","kind":"apply_patch","lease_generation":1,"state":"Dispatched"}],"state":"RUNNING","step":4,"task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb","usage":{"reserved_model_requests":0,"reserved_tool_actions":1,"settled_model_requests":0,"settled_tool_actions":1,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":null,"worker":"host","workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8"}

$ agentos resume 01a10214-8fd8-707b-91ed-8d0bc32062eb
{"state":"SUCCEEDED","task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb"}

$ agentos status 01a10214-8fd8-707b-91ed-8d0bc32062eb
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1791036864,"handle_prefix":"e7302959","operation":"snapshot.read","revoked":false},{"expires_ts":1791036864,"handle_prefix":"811c930c","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1791036864,"handle_prefix":"8689a287","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"33d86fe4","operation":"artifact.export","revoked":false}],"jobs":[],"outstanding_effects":[],"state":"SUCCEEDED","step":7,"task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb","usage":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","worker":"host","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ agentos events 01a10214-8fd8-707b-91ed-8d0bc32062eb      # one JSON object per line; shown here as seq + type
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

$ agentos export 01a10214-8fd8-707b-91ed-8d0bc32062eb bundle
{"base_revision":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","base_workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","capabilities":[{"handle_prefix":"e7302959","operation":"snapshot.read","revoked":false},{"handle_prefix":"811c930c","operation":"workspace.apply_patch","revoked":false},{"handle_prefix":"8689a287","operation":"verification.run","revoked":false},{"handle_prefix":"33d86fe4","operation":"artifact.export","revoked":false}],"contract_digest":"a8d51f26113c60185b2921eaa2c02c76cfbfa24b21b7c3872b1515832adcfb90","final_workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","generated_events":33,"model":"fake-agent","patch_digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","patches":[{"digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","effect_id":"99afe2fbad2030b8e75bb353537cb5c976ba0fb97e01db9de007aa6b9f277696","file":"patches/0001-5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5.patch"}],"state":"SUCCEEDED","task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb","usage_summary":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verification_profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","verification_results":[{"accepted_for_final_workspace":true,"completed":true,"effect_id":"8ad8a8b22a70af9470723544f2fa7c5def04a2eb99f870a0a27f3aef03f0d110","evidence_digest":"b00dcca26345f586f6981baf74cf6fb8a16fe0aacd790c278421d2d0a42c4303","exit_code":0,"passed":true,"profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}],"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ ls bundle
evidence
manifest.json
patch.diff
patches
$ ls home/jobs      # one directory per effect attempt
429b6025…-abcec392…
8ad8a8b2…-61df457f…
99afe2fb…-b94ebb51…

$ agentos revoke 01a10214-8fd8-707b-91ed-8d0bc32062eb --capability artifact.export
{"cancelled_jobs":0,"revoked":["artifact.export"],"task_id":"01a10214-8fd8-707b-91ed-8d0bc32062eb"}

$ agentos export 01a10214-8fd8-707b-91ed-8d0bc32062eb bundle-after-revoke
agentos: export denied: capability artifact.export is not usable (revoked)
(exit code 1)
```

What happened in that run:
- Every capability was issued as a handle at `--yes` (`CapabilitiesIssued`, prefixes only), and
  every intent and dispatch was authorized by the broker (`CapabilityGranted`).
- The controller died during the patch job (seq 19, `EffectDispatched`). The job's supervisor
  kept running and wrote the receipt into its job directory.
- `resume` found that receipt and published it (seq 20 `RecoveryDecision`, `PublishRetained`;
  there is exactly one job directory per effect: three effects, three directories), then the run
  went on to a verified SUCCEEDED.
- The export bundle is the same as an uncrashed run's (the CLI tests check this for every crash
  point). Its manifest lists the task's capabilities by 8-character prefix only.
- After `revoke`, `export` is denied (`revoked`) and the denial is journaled.
- `submit` prints the worker (`host (not sandboxed)`) and `status` carries `"worker":"host"`.

### Firecracker worker (jailed)

The same flow on the jailed Firecracker worker, a real transcript from
`docker compose run --rm test-kvm sh scripts/demo.sh --worker firecracker` on 2026-10-03 (same
conventions and the same two trimmed compose lines). The demo also registers the guest image
built by `scripts/build-guest-image.sh … --verify`, and ends by listing the workspace and the
job directories:

```text
$ agentos profile register /work/fixtures/profiles/parser-checks-v1
{"digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","id":"parser-checks-v1"}

$ agentos image register /work/build/guest-images/python-stdlib-v1
{"digest":"f231e3eae6b8015ee25f4458f5f73e8ebfc1c54d4af46fce110eeec267402f7f","id":"python-stdlib-v1"}

$ agentos submit task.json --yes --fake-agent-patch fix.patch --crash-at during-execute:apply_patch
task 01a10214-93ef-70cf-b357-226029abf249 submitted; approve these permissions before it runs:
  goal:                 fix the parser
  repository:           /work/fixtures/parser-repo at be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8
  capabilities:         snapshot.read, workspace.apply_patch, verification.run, artifact.export
  editable paths:       src/**
  acceptance:           protected verification profile parser-checks-v1 (9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c)
  limits:               model_requests=1 max_output_tokens_per_request=1000 tool_actions=10 deadline_seconds=600 worker_vcpus=1 worker_memory_mib=256
  agent:                fake-agent (patch 5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5)
  worker:               firecracker, guest image python-stdlib-v1@f231e3eae6b8015ee25f4458f5f73e8ebfc1c54d4af46fce110eeec267402f7f, jailed
2026-10-03T14:04:26.310661Z  WARN agentos_engine::crash: injected crash point=DuringExecute kind=Some("apply_patch") occurrence=0
{"crashed":"during-execute","task_id":"01a10214-93ef-70cf-b357-226029abf249"}
(exit code 75)

$ agentos status 01a10214-93ef-70cf-b357-226029abf249
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1791036865,"handle_prefix":"f38430d1","operation":"snapshot.read","revoked":false},{"expires_ts":1791036865,"handle_prefix":"795b67f2","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1791036865,"handle_prefix":"51d25726","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"1f11b996","operation":"artifact.export","revoked":false}],"guest_image":{"digest":"f231e3eae6b8015ee25f4458f5f73e8ebfc1c54d4af46fce110eeec267402f7f","id":"python-stdlib-v1"},"jailed":true,"jobs":[{"alive":true,"attempt_id":"38a42b30-be76-4733-afe8-b1ae1d239384","effect_id":"24d8e857bc4a2134a4acd74f66dfdaf799648dcae41b1870d65fc11f47ba60ad","lease_generation":1,"receipt":false,"state":"Running"}],"outstanding_effects":[{"effect_id":"24d8e857bc4a2134a4acd74f66dfdaf799648dcae41b1870d65fc11f47ba60ad","kind":"apply_patch","lease_generation":1,"state":"Dispatched"}],"state":"RUNNING","step":4,"task_id":"01a10214-93ef-70cf-b357-226029abf249","usage":{"reserved_model_requests":0,"reserved_tool_actions":1,"settled_model_requests":0,"settled_tool_actions":1,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":null,"worker":"firecracker","workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8"}

$ agentos resume 01a10214-93ef-70cf-b357-226029abf249
{"state":"SUCCEEDED","task_id":"01a10214-93ef-70cf-b357-226029abf249"}

$ agentos status 01a10214-93ef-70cf-b357-226029abf249
{"actions_used":2,"cancel_requested":false,"capabilities":[{"expires_ts":1791036865,"handle_prefix":"f38430d1","operation":"snapshot.read","revoked":false},{"expires_ts":1791036865,"handle_prefix":"795b67f2","operation":"workspace.apply_patch","revoked":false},{"expires_ts":1791036865,"handle_prefix":"51d25726","operation":"verification.run","revoked":false},{"expires_ts":null,"handle_prefix":"1f11b996","operation":"artifact.export","revoked":false}],"guest_image":{"digest":"f231e3eae6b8015ee25f4458f5f73e8ebfc1c54d4af46fce110eeec267402f7f","id":"python-stdlib-v1"},"jailed":true,"jobs":[],"outstanding_effects":[],"state":"SUCCEEDED","step":7,"task_id":"01a10214-93ef-70cf-b357-226029abf249","usage":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","worker":"firecracker","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ agentos events 01a10214-93ef-70cf-b357-226029abf249      # one JSON object per line; shown here as seq + type
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

$ agentos export 01a10214-93ef-70cf-b357-226029abf249 bundle
{"base_revision":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","base_workspace_digest":"be77aa19c032f85329a9596adfd692252a0c87fd09d337b1873feb6003bdd3b8","capabilities":[{"handle_prefix":"f38430d1","operation":"snapshot.read","revoked":false},{"handle_prefix":"795b67f2","operation":"workspace.apply_patch","revoked":false},{"handle_prefix":"51d25726","operation":"verification.run","revoked":false},{"handle_prefix":"1f11b996","operation":"artifact.export","revoked":false}],"contract_digest":"a8d51f26113c60185b2921eaa2c02c76cfbfa24b21b7c3872b1515832adcfb90","final_workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13","generated_events":33,"guest_image_digest":"f231e3eae6b8015ee25f4458f5f73e8ebfc1c54d4af46fce110eeec267402f7f","model":"fake-agent","patch_digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","patches":[{"digest":"5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5","effect_id":"24d8e857bc4a2134a4acd74f66dfdaf799648dcae41b1870d65fc11f47ba60ad","file":"patches/0001-5128fe0b9134f20b9e50aa2f17eab86244f8836df332d55b128456a4b47e6ff5.patch"}],"state":"SUCCEEDED","task_id":"01a10214-93ef-70cf-b357-226029abf249","usage_summary":{"reserved_model_requests":0,"reserved_tool_actions":0,"settled_model_requests":0,"settled_tool_actions":2,"uncertain_model_requests":0,"uncertain_tool_actions":0},"verification_profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","verification_results":[{"accepted_for_final_workspace":true,"completed":true,"effect_id":"4ae26bc3fc1718662adf06f4bb62477ec582183beadb6eb8025548eeb9981f75","evidence_digest":"b00dcca26345f586f6981baf74cf6fb8a16fe0aacd790c278421d2d0a42c4303","exit_code":0,"passed":true,"profile_digest":"9ff584f31b7fef8ac5774ced5c8f1620c27f736b4bdc4d9e03e553df5d8ea12c","workspace_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}],"verified_digest":"060915eeb9b0caf26efbfdab529c36a9e25359be64e71ae67e6651138a5fec13"}

$ ls bundle
evidence
manifest.json
patch.diff
patches
$ ls home/jobs      # one directory per effect attempt
24d8e857…-38a42b30…
4ae26bc3…-05176962…
82ec99f5…-c34ee5b6…

$ agentos revoke 01a10214-93ef-70cf-b357-226029abf249 --capability artifact.export
{"cancelled_jobs":0,"revoked":["artifact.export"],"task_id":"01a10214-93ef-70cf-b357-226029abf249"}

$ agentos export 01a10214-93ef-70cf-b357-226029abf249 bundle-after-revoke
agentos: export denied: capability artifact.export is not usable (revoked)
(exit code 1)

$ ls -ln home/work/01a10214-93ef-70cf-b357-226029abf249
total 49872
-rw------- 1 61000 61000 1073741824 Oct  3 14:04 ws.img
-rw-r--r-- 1     0     0          0 Oct  3 14:04 ws.lock

$ ls home/jobs/24d8e857…-38a42b30…
console.log firecracker.log lock outcome.bin outcome.json output.bin receipt.json request.json status.json stderr.log supervisor.log 
$ ls home/jobs/4ae26bc3…-05176962…
console.log firecracker.log lock outcome.bin outcome.json output.bin receipt.json request.json status.json stderr.log supervisor.log 
$ ls home/jobs/82ec99f5…-c34ee5b6…
console.log firecracker.log lock outcome.bin outcome.json output.bin receipt.json request.json status.json stderr.log supervisor.log 
```

What differs from the host run, and what does not:
- `submit` prints `worker: firecracker, guest image python-stdlib-v1@f231e3ea…, jailed`: the jail
  probe passed in `test-kvm`, so `jailed: true` is recorded (`status` shows `"jailed":true`,
  `"worker":"firecracker"` and the pinned `guest_image`).
- Each of the three effects ran in its own jailed microVM. The controller was killed while the
  patch VM ran; its supervisor finished the job, and `resume` published the retained receipt
  (seq 20) exactly as on the host worker.
- The journal has the same 32 event types in the same order; the final and verified digests
  (`060915ee…`), the patch digest and the evidence digest (`b00dcca2…`) are byte-identical to the
  host run's. The manifest adds only `guest_image_digest`.
- `ws.img` (1 GiB sparse, about 49 MiB allocated) belongs to uid/gid 61000 with mode `0600`, and
  `ws.lock` is next to it. No job directory has a `jail/` left: every chroot and cgroup was
  collected once its job settled, and `scratch.img` was removed after `outcome.json`.

## Home layout

```text
<home>/agentos.db          journal (SQLite, WAL; PRAGMA user_version = schema version 2)
<home>/blobs/              content-addressed artifacts (patches, results, evidence)
<home>/jobs/<effect>-<attempt>/   one directory per effect attempt (see below)
<home>/work/<task>/ws      task workspaces (host worker)
<home>/work/<task>/ws.img  task workspace block image (Firecracker; sparse ext4, 1 GiB);
                           owned by the jail uid (61000) from the first jailed ReadSnapshot on
<home>/work/<task>/ws.lock advisory lock: which VM has ws.img attached
<home>/tasks/<task>/       inputs recorded at submission: snapshot/, profile/, agent.patch
<home>/registry/<id>@<digest>/    registered verification profiles (read-only)
<home>/registry/<id>@<digest>.meta.json   registration time, outside the digest
<home>/registry/images/<id>@<digest>/     registered guest images (root 0444): image.json, vmlinux, rootfs.squashfs
<home>/registry/images/<id>@<digest>.meta.json
<home>/inspect/<task>/<uuid>/   inspector VMs: scratch.img, console.log, firecracker.log, stderr.log, jail/
                                (removed on success; a dead inspector's is collected by the next one)
<home>/bin/firecracker     optional: the Firecracker binary (default for --firecracker)
<home>/bin/jailer          optional: the jailer (default: `jailer` next to the Firecracker binary)
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

A Firecracker job directory also holds, all written by the worker or on its behalf:
`vm.json` (Firecracker's configuration; unjailed only, jailed it is inside the chroot),
`v.sock` (the vsock host socket; unjailed only), `scratch.img` (the per-job scratch drive, sparse
512 MiB, removed once `outcome.json` is written), `console.log` (the guest serial console:
kernel and agent log, Firecracker's stdout), `firecracker.log` (Firecracker's own log; jailed, a
hard link of the chroot's), `stderr.log` (the jailer's or Firecracker's stderr) and, while the
VM runs jailed, `jail/` (`jail/cgroup`, a marker naming the VM's cgroup, and the chroot
`jail/firecracker/<attempt>/root/`). `jail/` is collected once the job is settled.

## Architecture

- **`agentos-core`**: the task contract, the pure task state machine (cancel always wins;
  success needs evidence for the final workspace), the effect model (stable effect ids,
  retry policies, receipt verdicts), budgets, the broker's pure rules (opaque handles,
  scopes, `authorize`, lease arithmetic), the one `workspace_digest` implementation the host
  and the guest both link (so digests agree by construction), the patch rules, and the guest
  control protocol (`guest.rs`).
- **`agentos-store`**: the SQLite journal. Every state change and its event commit in one
  transaction; effects have reservations, leases and receipts. Also the persisted capability
  handles (issued at approval, checked and journaled at intent, dispatch and export, revocable)
  and the content-addressed blob store.
- **`agentos-engine`**:
  - the run loop, with agent turns journaled and replayed;
  - the effect steps;
  - crash injection and `recover`, which waits for and fences live jobs;
  - export;
  - the job-directory protocol, the `Worker` trait (a host-process worker, a scripted test
    worker and the Firecracker worker), the per-job supervisor (`agentos-supervisor`) and
    `SupervisedExecutor`;
  - the fixture executor the host worker runs;
  - `firecracker.rs` (`FirecrackerWorker`, `vm.json`, the preflight, the `Inspector`),
    `guestlink.rs` (the host side of the protocol, the `GuestLauncher` seam: real Firecracker or
    the fake guest) and `jail.rs` (planning, argv, staging, probe, decision, collection).
- **`agentos-guest`**: the guest's PID 1 and agent, a static musl binary at
  `/sbin/agentos-guest` in the image. It mounts `/proc`, `/sys`, `/dev`, `/run` and `/tmp`
  (tmpfs, 64 MiB each), formats the scratch drive, mounts the workspace, listens on vsock port
  5200, runs `git apply` as uid 1000 (`builder`) and the check as uid 1001 (`check`, through an
  `exec-check` trampoline that sets `RLIMIT_NPROC=256`, `RLIMIT_NOFILE=1024`,
  `oom_score_adj=1000` and `no_new_privs` before dropping privileges; the agent itself runs at
  `oom_score_adj -1000`), and powers the VM off on connection EOF, on `Shutdown` or when no
  `Hello` arrives within 10 s. The same session code runs as the fake guest of the test tiers.
- **`agentos-cli`**: the `agentos` binary (`submit`, `status`, `events`, `pause`, `resume`,
  `cancel`, `revoke`, `profile`, `image`, `export`). Each command is one short-lived controller
  process. The supervisor and its worker are this same binary, re-executed through hidden
  `supervise run|worker` subcommands.

The **`Executor` trait** (`run`, `retained_outcome`, `reconcile`, `current_workspace`,
`await_job`, `fence_job`) is the backend seam; `SupervisedExecutor` implements it over job
directories. The **`Worker` trait** is the seam below it: the host worker and the Firecracker
worker sit there.

Every effect attempt is a job owned by a per-job supervisor. It is launched detached (its own
session), enforces the lease (`now + min(effect timeout, deadline - now)`) and the task
deadline by killing the worker's process groups, honours the `cancel` marker, writes the
receipt, and exits. A controller that dies leaves its jobs running; recovery waits for a live
job until its lease plus a grace period, and only then fences it. A verification killed at its
lease or deadline yields a failure receipt; a killed patch yields none and is reconciled, never
recorded as failed.

### The Firecracker worker

```text
controller (agentos CLI)                        [holds driver.lock]
  └─ agentos supervise run <job>  (session leader, subreaper, holds <job>/lock)
       └─ agentos supervise worker <job>  (own process group = worker_pgid)
            └─ jailer --id <attempt> … -- --no-api --config-file /vm.json      (same group; root)
               ═exec═▶ /firecracker --id <attempt> … --no-api --config-file /vm.json
                       (same pid; uid 61000; chroot <job>/jail/firecracker/<attempt>/root;
                        cgroup <cgroup root>/agentos/<attempt>; stdout → console.log)
                 └─ guest: agentos-guest (PID 1) ─┬─ git apply       as uid 1000 (builder)
                                                 └─ check command   as uid 1001 (check)
```

The jailer sets up the chroot, cgroup and limits and then `exec`s Firecracker **in the same
process** (pid, pgid, sid and stdio unchanged), so every 3a kill path (lease, deadline,
cancel, the fence after a supervisor SIGKILL) reaches the VM unchanged, and a SIGKILLed
Firecracker destroys its VM. The guest also shuts itself down when its control connection
closes, so a dead worker takes its VM down within milliseconds. Unjailed, the worker spawns
`firecracker --no-api --config-file <job>/vm.json --id <attempt>` directly.

**The VM.** `vcpu_count` and `mem_size_mib` come from the contract (`worker_vcpus` 1..=32,
`worker_memory_mib` ≥ 128, validated at `submit`), `smt: false`, no network interface, no
MMDS. Drives: `vda` the image's squashfs (read-only root), `vdb` the task's `ws.img` (ext4,
`Writeback`; the guest `syncfs`es before any success reply, so a reported success is durable
before the host writes an outcome), `vdc` the job's `scratch.img` (formatted at boot). Kernel
command line: `console=ttyS0 reboot=k panic=1 pci=off nomodule quiet loglevel=4
init=/sbin/agentos-guest`; it carries nothing secret.

**The protocol** (`agentos_core::guest`, vsock port 5200, guest CID 3). One host-initiated
connection per VM through Firecracker's Unix socket (`CONNECT 5200` / `OK`), then frames: a
`u32` big-endian length, a kind byte (JSON or raw bytes), the body. `Hello` (protocol 1, the
per-job `attempt_token`, the ids, `mode` job or inspect) → `Ready`; then one request:
`ReadSnapshot` (files as raw frames) → `SnapshotDone`, `ApplyPatch` → `PatchApplied`,
`RunVerification` (the staged profile) → `Verified`, or the inspection queries `Digest` and
`PatchState`; `Shutdown` → `Bye`. Limits are checked before allocating: JSON frame ≤ 1 MiB,
raw frame ≤ 16 MiB, file ≤ 64 MiB, snapshot ≤ 256 MiB and 65 536 files, profile ≤ 64 MiB,
patch ≤ 4 MiB, captured output 64 KiB + 1. A frame over its limit is a protocol failure, never
a hang. The guest's replies are data: outcome bytes, evidence and receipts are built on the host
and are byte-identical to the host worker's. No capability handle ever enters the guest; the
`attempt_token` is identity, not authority, and travels only in `Hello`.

**Inspection boots.** `reconcile` (did a receipt-less patch apply?) and `current_workspace`
(the digest at resume) are answered by booting the guest in inspect mode from the controller
process (`Inspector`, bounded by 60 s, jailed like a job VM, `ws.img` remounted read-only
after journal replay). `NotApplied` only when the digest equals the base, `Applied` only when
reversing the patch gives the base back, `Unknown` otherwise (and on a busy `ws.lock`, a missing
image, a boot failure or a timeout).

**The jail** (`jail.rs`). For every job VM and inspector VM when jailed: *plan* the chroot
`<dir>/jail/firecracker/<id>/root/` and the cgroup `<cgroup root>/agentos/<id>`; *stage* by hard
links (`vmlinux`, `rootfs.squashfs` from the registry, root `0444`; `ws.img` and `scratch.img`
chowned to the jail uid, `0600`), an empty `firecracker.log` hard-linked back to the job
directory, the chroot-view `vm.json`, and the marker `jail/cgroup`; *spawn* the jailer:

```text
jailer --id <id> --exec-file <firecracker> --uid 61000 --gid 61000 --chroot-base-dir <dir>/jail
       --cgroup-version 2 --parent-cgroup agentos --cgroup "cpu.max=<vcpus×100000> 100000"
       --cgroup memory.max=<(memory_mib+128)×1048576> --cgroup memory.swap.max=0 --cgroup pids.max=64
       --resource-limit fsize=1073741824 -- --no-api --config-file /vm.json
```

(never `--new-pid-ns`, `--daemonize` or `--netns`); and *collect* after the process is gone:
`rmdir` the cgroup named by the marker (only under `<cgroup root>/agentos/`) and remove
`jail/`. The worker collects its own jail; after a supervisor SIGKILL the controller collects
it, but only once the job is settled (after `wait_for_job`, or after the fence on resume).

### Security model (summary)

Untrusted: the repository snapshot, the patch, what the verification check does at run time,
the guest kernel after untrusted code ran, and — the reason for the jail — the Firecracker
process itself after a VM escape.

- **Guest network / host secrets**: no network device (only `lo`); the guest sees only its three
  drives and `/dev/vsock`; Firecracker is spawned with `env_clear()`; the kernel command line is
  constant. (`/dev/vsock` is root-only `0600`, but that mode does not stop `socket(AF_VSOCK)`;
  the real defence is that the host listens on nothing the guest can reach and no token or
  connection ever reaches uid 1001.)
- **CPU / memory / processes / disk**: `vcpu_count`, `mem_size_mib`, guest OOM priorities (the
  check is killed first, the agent survives and reports), `RLIMIT_NPROC`, group kill and VM
  teardown, fixed-size images, a read-only root and 64 MiB tmpfs.
- **Patch escape, tampered profile, forged outcomes, stray connections**: the 3a checks run
  inside the guest; the profile pin is checked on the staged copy in the guest and by the host;
  outcomes are built on the host; the token is bound at `Hello`, one connection per VM.
- **VM escape into the Firecracker process**: the jail. The process runs as uid 61000 in a
  `pivot_root` chroot holding only the staged inodes, in a cgroup with
  `cpu.max`/`memory.max`/`memory.swap.max=0`/`pids.max=64`, with `RLIMIT_FSIZE` 1 GiB and
  Firecracker's own seccomp filter. It can write nothing of the home but its own `ws.img`,
  `scratch.img`, `vm.json` and `firecracker.log`; the registry, journal and `request.json` are
  root-owned or outside the chroot. A second bug is needed to reach the host (in the kernel or KVM, or a bypass of
  Firecracker's seccomp filter); note the jail has no PID or network namespace (3b-2).
  Unjailed, the escape lands as the controller's UID.
- **Trusted computing base**: the host kernel and KVM, Firecracker and the jailer v1.17.0 (the
  jailer runs as root for the milliseconds of setup), the pinned guest kernel and image bytes,
  `agentos-guest`, `git`/`python3` in the guest, the controller, supervisor and worker.

## Known limits (v0.1, Phases 1-3b-1)

Sandboxing and the jail:

- **Sandboxed only with `--worker firecracker`.** With the default `host` worker, repository
  and verification code runs on the host as the same UID. Do not run untrusted repositories or
  patches on the host worker. The supervisor kills process groups it knows about; a process
  that escapes with `setsid` is not tracked. (In the guest, a check that escapes its process
  group with `setsid` is likewise not killed by the group logic; it is bounded only by the VM's
  teardown at the end of the job.)
- **Jailed Firecracker needs root and a writable, delegated cgroup v2 tree.** Without them
  `--worker firecracker` refuses (exit 1, task untouched). `--allow-unjailed` runs it as the
  controller's UID without chroot or cgroup and records `jailed: false`; then the controller's
  UID is the blast radius of a VM escape.
- **The jail edits the host's root cgroup.** `delegate` (the jail probe, on every executor
  build) writes `+cpu +memory +pids` into the host root `cgroup.subtree_control` and, on
  systemd hosts, creates `/sys/fs/cgroup/agentos` directly under the root cgroup.
- **The jailed process has no PID or network namespace** of its own (`--new-pid-ns` and
  `--netns` are 3b-2). It has no network device and its seccomp filter forbids `fork`/`exec`.
- **One jail uid (61000) per home.** Every VM of the home runs as it (at most one job VM and one
  inspector at a time, each in its own chroot); `ws.img` of a jailed task belongs to it, so a
  home used jailed must stay used as root. The jail hard-links the registry image, `ws.img` and
  `scratch.img` into the chroot, so the home must be one filesystem mounted without `nodev` or
  `noexec` (`/tmp` is `nodev` on most hosts: not a valid home for jailed use).
- **Jail memory bound vs host page cache — owner decision pending.** The jail's `memory.max` is
  the guest's memory + 128 MiB (`JAIL_MEMORY_OVERHEAD_MIB`). The host page cache generated by the
  drive files (`ws.img`, `scratch.img`) is charged to the same cgroup, so a disk-heavy guest under
  host I/O contention can get Firecracker OOM-killed by its own cgroup (seen once in the KVM tier:
  `guest exited before reporting: firecracker killed by signal 9`). The check's evidence is then
  lost and the effect fails; it is never a false pass. Options for the owner: raise
  `JAIL_MEMORY_OVERHEAD_MIB`, add a `memory.high` below `memory.max`, or change the drives'
  `cache_type`/I/O path.
- **A VM escape lands in the jail, not nowhere.** Micro-architectural side channels are not
  mitigated beyond `smt: false` per VM (host SMT stays on). The guest agent and the jailer's
  setup (as root) are in the TCB.
- **The host kernel (7.2) is newer than Firecracker's validated hosts.** Accepted by the owner;
  `Submitted` records `host_kernel` and `firecracker_version` so a later failure is attributable.
- **`/dev/vsock` mode is not the boundary** (see the security model): uid 1001 can open an
  `AF_VSOCK` socket; nothing on the host answers it.
- **`RLIMIT_NPROC` edge.** The check's trampoline sets `RLIMIT_NPROC=256` while still root, so if
  uid 1001 already has 256 processes (escapees from an earlier check in the same VM), the
  `exec` fails with `EAGAIN` (exit 127). Each job has its own VM, so this needs a hostile check
  in the same boot.

The Firecracker worker:

- **`reconcile` and `current_workspace` boot a VM from the controller process** (bounded by
  60 s, read-only on the workspace after journal replay). A missing `/dev/kvm` or jailer at resume
  fails the command before the task is touched. An inspector that fails after the preflight
  passed (the device vanished, a boot failure, a jailer failure) fails the task on resume with
  `workspace lost: workspace inspection failed: …`: rarely, a task can fail for an
  infrastructure reason.
- **Orphan inspector VMs.** An inspector VM whose controller died is detected and collected by
  the next inspection through its cgroup. An **unjailed** orphan, or a jailed one that died
  before the jailer created its cgroup, cannot be detected that way, and nothing holds `ws.lock`
  for it (the controller held it and died). Mitigation: the guest powers off on connection EOF
  (the controller's death closes it) and when no `Hello` arrives within 10 s.
- **A task recorded jailed cannot finish a cancel while its jailer is gone.** If the probe (or
  any preflight: `/dev/kvm`, a tampered or unregistered image) fails, `cancel` with outstanding
  effects records nothing more than its intent, stops the running jobs, and exits 1; the cancel
  stays pending until the jailer is back (fail-safe: the in-flight effects must be reconciled by
  inspection first).
- **A globally exported `AGENTOS_WORKER=firecracker`** makes every command on a host task exit 2
  (`task was submitted with worker host`): the environment variable is the flag.
- **A partially submitted task** (no `Submitted` event yet) reports `"worker":"host"` in the JSON of `status`.
- **`submit --yes` runs the preflight and the probe twice** (before anything is written, and
  again when building the executor). A host change in between leaves the task READY and
  unapproved.
- **Workspace and scratch image sizes are constants** (`ws.img` 1 GiB, `scratch.img` 512 MiB),
  not contract limits, and the images are never garbage-collected (the jails are). A killed job
  leaves its `scratch.img`.
- **One VM boot per effect** (about 0.65 s to `Ready` here) and one inspector boot per resume
  and per receipt-less patch; no VM reuse or snapshots.
- **Executable bits are lost** in the guest: the protocol's `File{path, len}` carries no mode, so
  snapshot and profile files are `0644` there. A profile command such as `./check.sh` works on
  the host worker but fails in the guest (`cannot run profile command: Permission denied`); use
  `["sh", "check.sh"]` or `["python3", "check.py"]`.
- **Guest-only reason strings.** The guest can refuse with `profile command too large: N bytes
  of JSON, limit 869712` (a `command` whose JSON would overflow the reply frame) and
  `scratch dir: …`. Only `profile command too large` is guest-only; the host worker also emits
  `scratch dir: …`.
- **Guest memory as reported.** `Ready.memory_mib` is the guest's `MemTotal`: 229 for a 256 MiB
  VM (the guest kernel keeps the rest).
- **The guest image has no `mount` binary** (the root is read-only and `mount(2)` is denied to
  the check either way).
- **`EffectDispatched.worker` says `fixture-executor`** for Firecracker jobs too (a constant from
  3a; the journal schema is frozen). `Submitted.worker` is the authoritative record. A Firecracker
  `Submitted` has no `guest_image` field (it would falsely name `fixture-executor-v0`); it has
  `guest_image_id` and `guest_image_digest`.
- **The guest trusts the host's clock** (`kvm-clock`) and entropy, and the serial console
  (`console.log`) is the only guest log.
- **The image build needs network and root** in the build container (snapshot.debian.org, the
  Firecracker CI bucket); the kernel is a pinned download, not built from source.
- **x86_64 only** (the image recipe pins the x86_64 kernel).
- **The KVM tier's container** needs `/dev/kvm`, `CAP_SYS_ADMIN`, a seccomp profile that allows
  `pivot_root` and the cgroup-delegation entrypoint; the default `test` service needs none of it.

Carried from 3a:

- **A revoke can miss a job that is just starting.** A revoke that lands between a dispatch
  being authorized and the job directory being created finds no job to stop; that job runs to
  completion, and the next request is denied.
- **`Denied` rows are forgeable.** They are audit rows any caller can append; only the
  `Capability*` rows are written by the broker itself.
- **No job-directory garbage collection.** `<home>/jobs` grows with every attempt.
- **Export bypasses the effect model.** It is authorized through the broker
  (`artifact.export`, journaled, revocable) but is not a journaled effect; it leaves an
  `Exported` audit row.
- **Host paths can leak into exports (host worker).** Exported verification evidence
  (stdout/stderr) of the host worker can contain absolute host paths. The guest sees only
  guest paths (`/workspace`, `/scratch/…`).
- **No real model.** The model call is a stand-in: only the fake agent exists, and the real
  model adapter is Phase 4.
- **One driver per home.** Only one process drives a home at a time (`driver.lock`).
  Recovery's blob garbage collection assumes no concurrent writers.
- **Resume needs the same patch.** `resume --fake-agent-patch` must be given the same
  patch again. A different one makes the journal replay diverge, which fails the task.
- **No migration.** A database written by an earlier build (`user_version < 2`) is refused.
- **Reserved tables.** `tasks.checkpoint` is always NULL, because journal replay is the
  checkpoint mechanism. The `observations` table is reserved.

## Phase 3b-2 (not built yet)

- Block-device rate limiting for the drives.
- Contract-driven disk sizes (`worker_disk_mib`, an optional contract field) instead of the
  1 GiB / 512 MiB constants.
- Garbage collection of workspace and scratch images together with the job directories.
- Building the guest kernel from source (the build plan's Phase 6 "reproducible guest image
  build" finishes there).
- Jailer extras that need a different supervision model or more privilege: `--new-pid-ns` (the
  jailer parent exits at once), `--netns`, a per-job jail uid, the `userfaultfd` device.
