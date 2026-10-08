# VM disk and I/O resources — design

Status: design for package 9 of the [v0.1 completion plan](../plans/2026-10-04-v01-completion.md).
Parent: [v0.1 completion design, "VM resources"](2026-10-04-v01-completion-design.md#vm-resources).

## Goal

A task's contract decides the size of its two writable VM drives and, optionally, their I/O
rates. The values are validated before anything is journaled, recorded at submission,
applied identically on every later command and on recovery, and exported with the result.
Old contracts and old journals keep exactly the behavior they had. A resource failure is
visible and never yields accepted verification.

## Current behavior

- `ws.img` is created sparse at `WS_IMAGE_BYTES` (1 GiB) on the task's first snapshot and
  reused by every later effect and by the inspector. The guest formats it at snapshot time.
- `scratch.img` is created sparse at `SCRATCH_IMAGE_BYTES` (512 MiB) per job and removed
  when the job ends. The guest formats it at boot.
- Drives have no rate limiter. The workspace drive is `Writeback`, scratch and rootfs `Unsafe`.
- vCPUs and memory come from the contract's `limits` on every command. The contract is
  stored at submission and digest-pinned, so these values cannot change for a task.
- The jail's `memory.max` is `worker_memory_mib + JAIL_MEMORY_OVERHEAD_MIB` (128). The
  overhead was not chosen from measurements.

## Contract fields

Four optional fields in `limits`. Each is `#[serde(default, skip_serializing_if =
"Option::is_none")]`, so a contract without them serializes byte-for-byte as before and keeps
its digest. A golden test pins the digest of an existing fixture contract.

| Field | Range | Absent means | Applies to |
| --- | --- | --- | --- |
| `worker_disk_mib` | 512..=32768 | 1024 | workspace drive size |
| `worker_scratch_mib` | 384..=32768 | 512 | scratch drive size |
| `worker_disk_bandwidth_mib_s` | 1..=4096 | no limit | workspace and scratch, each |
| `worker_disk_iops` | 10..=1000000 | no limit | workspace and scratch, each |

Minimums follow from what the drives must hold. The workspace receives a snapshot of up to
`SNAPSHOT_BYTES_LIMIT` (256 MiB) plus ext4 metadata and patch growth. Scratch holds the staged
profile (`PROFILE_LIMIT`, 64 MiB), the patch workspace, the check's directory and the
reverse-check copy of the workspace content (up to 256 MiB). Maximums are conservative bounds
for one local host, not a capacity promise; the capacity check below decides whether a task
can run here.

Byte sizes are computed as `u64::from(mib).checked_mul(1 << 20)`. Values outside the ranges
are rejected by `Contract::validate` with the field name and range, before submission journals
anything. These fields are ignored by the host worker, which has no drives; `submit` prints
them only for the Firecracker worker.

## Resolution, recording and replay

`VmResources` (in `agentos-core`) is the resolved form: `version`, `disk_mib`, `scratch_mib`,
`bandwidth_mib_s: Option`, `iops: Option`. `VmResources::resolve(&Limits)` applies the
defaults above and is version 1.

`Submitted` for a Firecracker task records `"vm_resources": {...}`. Every later command
resolves the stored contract again and requires the result to equal the record; a mismatch
is an internal error that touches nothing. A journal without `vm_resources` (every task
submitted before this change) resolves to the frozen version-0 values: 1024 MiB workspace,
512 MiB scratch, no rate limits. These are the current constants and are never changed;
a future default change bumps the version instead.

The export manifest gains `vm_resources: Option<VmResources>` with `serde(default,
skip_serializing_if = "Option::is_none")`. The committed 2026-10-08 manifests still
deserialize; the evidence test already does this through the `Manifest` type.

## Drives

`FirecrackerConfig` gains `resources: VmResources`, set by the CLI from the resolution above,
replacing the constants at the three use sites.

- **Workspace image size is fixed once.** `ws.img` is created at the first snapshot with
  `disk_mib`. Every later effect and the inspector check that an existing `ws.img` has exactly
  that length and fail visibly otherwise. Nothing ever recreates or resizes it.
- **Host capacity.** Both images are preallocated with `fallocate` instead of left sparse,
  so a host ENOSPC can only happen before launch, never in the middle of a guest write.
  A failed preallocation removes the partial file and fails the job before the VM starts.
  The effect then fails as an infrastructure failure with the reason
  `host disk: cannot preallocate <n> MiB for <file>: <error>`. `submit` additionally runs a
  `statvfs` preflight on the work root for `disk_mib + scratch_mib` and refuses with exit 1
  when free space is short; this is advisory, since space can disappear afterwards, and the
  preallocation is the real guarantee. A filesystem without `fallocate` support is refused
  by the same preflight.
- **Rate limits.** When set, the workspace and scratch drives get Firecracker's
  `rate_limiter` (v1.17.0 API: `RateLimiter { bandwidth, ops }`, each a
  `TokenBucket { size, refill_time, one_time_burst? }`). Bandwidth uses
  `size = bandwidth_mib_s << 20` bytes and ops `size = iops` with `refill_time = 1000` ms,
  and `one_time_burst` is omitted. The rootfs is read-only and never limited. Without the
  fields the rendered `vm.json` is unchanged; a golden test pins both forms.

## Memory accounting

The workspace drive is `Writeback`, so host page cache for its writes is charged to the
jail's cgroup. Before changing `JAIL_MEMORY_OVERHEAD_MIB`, measure on the KVM tier with an
otherwise idle host:

1. A write-heavy verification that fills most of the workspace drive and one that fills
   scratch, at 256 and 1024 MiB guest memory, with and without a bandwidth limit.
2. During each, sample the jail cgroup's `memory.current`, `memory.peak`, and `memory.stat`
   (`anon`, `file`, `file_dirty`, `file_writeback`) and read `memory.events` (`oom_kill`,
   `max`) at the end.
3. Repeat under host I/O contention from a parallel writer outside the jail.

Record the samples under `docs/evidence/`. Keep 128 MiB if `memory.peak - guest memory` stays
below it with margin and no run records an `oom_kill`; otherwise choose the smallest value
the measurements support and record why. Reclaimable page cache is expected to be reclaimed
under `memory.max` rather than kill Firecracker; the measurements decide.

**Infrastructure OOM never verifies.** With the existing `with_jail_memory_max_mib` seam set
low during a write-heavy verification, the VM is killed by the cgroup. The test asserts no
`VerifyPassed`, an effect failure whose reason names the VM's exit, and an `oom_kill` count
above zero in `memory.events`.

## Executable modes

The guest protocol transfers file contents without modes, so a profile whose command runs a
file from the profile directory directly cannot work in a VM. `profile register` rejects a
profile when `command[0]` is a relative path containing `/` (for example `./check.sh`) or a
bare name that is also a file in the profile directory. The message says to run it through
its interpreter (`["sh", "check.sh"]`, `["python3", "check.py"]`). Absolute paths name
programs in the guest image and are allowed. Transporting modes needs a new guest protocol
version and is outside this package.

## Tests

| Test | Tier |
| --- | --- |
| Old contract digest unchanged; new fields round-trip; each bound and overflow rejected with the field name | default |
| Resolution: absent fields give version 1 defaults; a journal without `vm_resources` gives version 0 | default |
| `Submitted` records `vm_resources`; resume uses it; a record that disagrees with the contract refuses without journaling | default, fake jail |
| Rendered `vm.json` with and without rate limits (golden) | default |
| `ws.img` created at `disk_mib` and preallocated; a wrong-sized existing image fails visibly and is not replaced | default, fake guest |
| Preallocation failure (fault seam) fails the job before launch with the host-disk reason | default |
| Manifest carries `vm_resources`; older manifests still deserialize | default |
| `profile register` rejects `./check.sh` and a bare profile file name, accepts interpreter commands and absolute paths | default |
| Guest sees the contracted drive sizes (`/sys/block/vdb/size`, `vdc`) | real KVM |
| Full workspace and full scratch at the contracted sizes fail visibly, never verify | real KVM |
| Bandwidth limit bites: writing 5× the per-second rate takes at least 3.5 s; no upper bound is asserted | real KVM |
| Crash and resume of a task with non-default resources keeps the recorded sizes | real KVM |
| Infrastructure OOM during write-heavy verification never verifies | real KVM |

## Order of work

Offline first: executable-mode rejection, contract fields and validation, resolution and
recording, manifest, `vm.json` rendering, preallocation and size checks. Then the KVM tests
and the memory measurements on an idle host, then the overhead decision.

## Out of scope

Mode transport, network, per-task host disk quotas beyond preallocation, read rate limits on
the rootfs, and changing the default sizes.
