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
| `worker_disk_bandwidth_mib_s` | 32..=4096 | no limit | workspace and scratch, each |
| `worker_disk_iops` | 5000..=1000000 | no limit | workspace and scratch, each |

Minimums follow from what the drives must hold. The workspace receives a snapshot of up to
`SNAPSHOT_BYTES_LIMIT` (256 MiB) plus ext4 metadata and patch growth. Scratch holds the staged
profile (`PROFILE_LIMIT`, 64 MiB), the patch workspace, the check's directory and the
reverse-check copy of the workspace content (up to 256 MiB). Maximums are conservative bounds
for one local host, not a capacity promise; the capacity check below decides whether a task
can run here.

The rate minimums were raised from the first draft (1 MiB/s, 10 operations/s) after
measuring on the KVM tier, because Firecracker's limiter throttles reads as well as writes,
and the guest formats scratch through it before its 10 s watchdog. Measured on the
reference host (NVMe, xfs), unlimited rows for comparison:

| Snapshot | Limit | Snapshot time (deadline 120 s) | Inspection (deadline 60 s) |
| --- | --- | --- | --- |
| parser fixture | 500 operations/s | boot fails (watchdog) | n/a |
| parser fixture | 1000 operations/s | 16.3 s | 5.5 s |
| parser fixture | 4 MiB/s | 15.8 s | 5.4 s |
| 249 MiB, 30,055 files | 16 MiB/s and 2000 operations/s | 71.1 s | 42.1 s |
| 249 MiB, 30,055 files | 32 MiB/s and 5000 operations/s | 28.4 s | 15.7 s |
| 243 MiB, 65,045 files | none | 7.0 s | 2.4 s |
| 243 MiB, 65,045 files | 32 MiB/s and 5000 operations/s | 45.0 s | 21.9 s |

At the minimums a snapshot near both limits keeps at least 2.6 times its deadlines; a KVM
test pins that case. A slower host disk slows every row regardless of the limit, which only
caps the rate.

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
- **Host capacity.** Both images stay sparse. Preallocating them was considered and
  rejected after measuring the cost: the test container's work root shares a filesystem with
  48 GB free, and 32 parallel test tasks at 1.5 GiB each would reserve about all of it.
  Instead, `submit` and every job launch run an advisory `statvfs` check on the work root:
  free space must cover the unallocated part of `ws.img` plus `scratch_mib`. A shortfall
  fails before launch with `host disk: <n> MiB free under <work root>, the VM may write <m> MiB`
  (exit 1 at `submit`, an infrastructure failure for a job). Space can still run out after
  the check. A host ENOSPC during a guest write reaches the guest as a block I/O error, and
  its effect fails. That is safe for these reasons:
  - The guest digests the mounted workspace at verification time, and `VerifyPassed`
    requires that digest to equal the task's current digest, so a partly written image
    cannot pass.
  - Every `ApplyPatch` first checks that the image's digest is the expected base, so a
    partly written image turns later patches into a visible version conflict instead of
    building on it.
  - The guest kernel logs each failed block request on the serial console as
    `[<time>] I/O error, dev vdX, …`. When it has, the worker reports a refusal, a failed
    check or a failed boot as `host disk: the VM's drives returned I/O errors (see
    console.log): …`, so the agent is not sent to fix code that is not broken. A check that
    passes despite such errors keeps its evidence. The check cannot write the console or
    the kernel log (`EACCES` on `/dev/console`, `/dev/ttyS0`, `/dev/tty0` and `/dev/kmsg`,
    checked on KVM), so it cannot disguise its own failure as the host's.

  Measured on KVM with the work root on a 200 MiB tmpfs: a snapshot of 150 MiB fails with
  `syncfs /workspace: I/O error`; a host that fills while a patch's VM formats scratch fails
  the boot; a check whose scratch writes hit the full host gets `EIO`. Each is reported as
  `host disk: …`, and the image is unchanged after the failed patch.
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

**Measured (2026-10-08, `docs/evidence/2026-10-08/vm-memory/`).** Peak usage is the wrong
measure under heavy writes: page cache fills whatever room exists, so at 256 MiB the peak sat
at the 384 MiB limit in every case. What reclaim cannot drop is anonymous memory plus dirty
and writeback page cache, summed within one sample (every 20 ms). The hostile disk-fill check:

| Case | Unreclaimable peak | Headroom to `memory.max` | OOM kills |
| --- | --- | --- | --- |
| 256 MiB guest, alone, with and without 32 MiB/s and a host writer | 284–312 MiB | 71–99 MiB | 0 |
| 256 MiB guest, four VMs filling at once | 302–322 MiB | 61–81 MiB | 0 |
| 1024 MiB guest, the same four single-VM cases | 682–767 MiB | 384–469 MiB | 0 |

**Decision: keep `JAIL_MEMORY_OVERHEAD_MIB` at 128 MiB.** The tightest case keeps 61 MiB,
about half the overhead, and four concurrent VMs did not reproduce the OOM the disk-fill
test once met. A spike shorter than the 20 ms sampling could go unseen, so the disk-fill
test still runs alone. Step 12 shows that an OOM, if it happens, is a visible failure and
never accepted evidence.

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
| `ws.img` created sparse at `disk_mib`, scratch at `scratch_mib`; a wrong-sized existing image fails visibly in the worker and the inspector and is not replaced | default, fake guest |
| The free-space check refuses at `submit` and fails a job before launch (fault seam for the free-space figure) | default |
| The jail's `RLIMIT_FSIZE` covers the larger image | default, fake jail; real KVM with `worker_disk_mib` above 1024 |
| Manifest carries `vm_resources`; older manifests still deserialize | default |
| `profile register` rejects `./check.sh` and a bare profile file name, accepts interpreter commands and absolute paths | default |
| Guest sees the contracted drive sizes (`/sys/block/vdb/size`, `vdc`) | real KVM |
| Full workspace and full scratch at the contracted sizes fail visibly, never verify | real KVM |
| Bandwidth limit bites: 160 MiB synced at the 32 MiB/s minimum takes at least 3.5 s; no upper bound is asserted | real KVM |
| Crash and resume of a task with non-default resources keeps the recorded sizes | real KVM |
| Infrastructure OOM during write-heavy verification never verifies | real KVM |
| Host ENOSPC (work root on a small tmpfs, images larger than it) during snapshot, patch and a scratch write in verification: visible failure, no `VerifyPassed`, and afterwards the inspected image digest equals the journal's or the task fails | real KVM |
| The minimum rates run the fixture, and a near-limit snapshot (65,045 files, 243 MiB) within the deadlines | real KVM |

## Order of work

Offline first: executable-mode rejection, contract fields and validation, resolution and
recording, manifest, `vm.json` rendering, sizes, the free-space check and the jail's file-size limit. Then the KVM tests
and the memory measurements on an idle host, then the overhead decision.

## Out of scope

Mode transport, network, per-task host disk quotas or preallocation, read rate limits on
the rootfs, and changing the default sizes.
