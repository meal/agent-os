# Conservative transient collection

This refines the approved v0.1 completion design, package 8. Development and
verification use the existing isolated worktree and Docker Compose; actual live/KVM
acceptance remains deferred. Revised 2026-10-07 after an adversarial review (see
[the review record](../../reviews/2026-10-04-conservative-gc-review.md)): job evidence is
kept, decisions are per task, work is batched, and the mount regressions run in a gate.
Revised again the same day after a second review: held locks are capped per batch, resource
exhaustion is retryable, and staged data is matched by inode across passes.

## Ownership and retention

`agentos gc --dry-run` and `agentos gc` operate under the exclusive home driver lock, taken
before the store is opened; `collect` demands a `HeldDriverLock`. `HeldDriverLock::verify`
canonicalizes the home path once, opens the home directory, checks `driver.lock` through
that descriptor (same inode, lock held) and keeps the descriptor: `collect` anchors every
operation on it, so the proven lock and the collected home are the same directory. `collect`
also refuses a database or blob store that is not that home's `agentos.db` and
`blobs/objects` (device and inode). `gc` requires an existing home and never creates one.

The collector accepts only terminal tasks with no INTENDED, DISPATCHED or UNKNOWN effects.
A terminal task accepts no further event, so a cancellation still marked pending on a task
that failed meanwhile is irrelevant and does not retain it. It keeps journals, task inputs, registries and every blob.
It checks every registered blob's integrity before deleting anything; a corrupt blob stops
the pass (blobs are shared and not attributable to one task's exports, so this stays global).

What is removed:
- `work/<task>/{ws,workspace,ws.img}` of an eligible task (the parent and `ws.lock` stay);
- `model/<effect>-<attempt>` whose `response.json` is the settled effect's published result
  (state COMPLETED/FAILED, digest, lease, attempt and kind match, result registered);
- from `jobs/<effect>-<attempt>`, when its `receipt.json` is the settled published result
  and its `request.json` names the same effect, task, kind, attempt and lease: `output.bin`
  (only if its bytes hash to the published result), `scratch.img`, and the single-link
  top-level `v.sock` of a Firecracker job.

What is kept: every other file of a job directory (`console.log`, `stderr.log`,
`firecracker.log`, `supervisor.log`, `status.json`, `request.json`, `receipt.json`,
`outcome.json`/`outcome.bin`, `groups`, `vm.json`, `cancel`, `lock`). These are evidence that
is neither exported nor journaled. `receipt.json` is small and makes a reduced directory
self-describing: a later pass reports it `collected`. `outcome.bin` can duplicate the output
bytes but is the worker's own record; it is kept because its redundancy is not proven.
Attempts without a matching receipt (killed, superseded, unresolved, unreadable) are kept
whole and do not block their task; a killed job's `scratch.img` therefore stays.

## Decisions and integrity

Collection is all-or-nothing within a task. Anything that may still be live, or that the
collector will not remove blindly, retains that task only and is reported `retained`:
an unfinished task or pending cancellation, unsettled effects, a held `ws.lock` or job lock,
inspection entries, a job `jail/`, symlinks, special files or hard links inside a candidate
tree, a tree of more than 10000 entries or 64 levels. Other tasks are still collected.

An integrity problem stops the pass: an unknown name under `jobs`, `model`, `work` or `gc-trash`; a symlink or
non-directory where an owned directory is expected; a job or model directory naming an
effect the journal does not know; a symlinked workspace; a mount root inside a candidate
(Linux statx `STATX_ATTR_MOUNT_ROOT`; unavailable detection also refuses); an invalid,
foreign or older-version deletion ticket; staged data that is not what its ticket staged;
a corrupt blob. One found while classifying (the first phase reads everything) stops the
pass before any deletion; one found later (during a batch's revalidation or execution, e.g. a
mount appearing after staging, staged data that changed, data that cannot be moved back)
stops the remaining batches after earlier batches, and earlier tasks of the same batch, were
collected. The all-or-nothing rule is about the decision: execution is entry by entry, so a
failure or interruption in the middle of a task leaves it partly collected and the next pass
finishes it. Integrity problems and failures are `refused`; entries not processed after a
stop are `skipped`. Unknown entries are never removed automatically. Descriptor or memory
exhaustion (`EMFILE`, `ENFILE`, `ENOMEM`, `ENOBUFS`) is never an integrity problem: it is a
refusal whose reason says to retry, and it leaves staged data under a valid ticket.

## Passes, batches and descriptors

A pass first scans names (one directory descriptor at a time) and groups entries by task;
a job or model directory is attributed through the effect its name carries. It then
classifies every task without deleting anything: locks are probed (taken and released),
trees are walked by path below a pinned directory descriptor, and the device/inode of each
entry and of its parent is captured during this validation.

Eligible tasks are then processed in batches of whole tasks. A batch closes at
`--batch-size` entries (default 64, at most 4096; a task larger than that is a batch of its
own) or at `MAX_TASKS_PER_BATCH` = 32 tasks, whichever comes first; the task cap is lowered to
`soft RLIMIT_NOFILE - 64 - 32` (at least 1) when the descriptor limit is low. For each batch the tasks are
revalidated while their `ws.lock` is taken and held until the batch ends; the
`Validated` boundary follows; then each entry is removed. A job lock is taken again and
held while that job's files are removed. Releasing job locks between validation and
deletion is sound because the driver lock is held: no new supervisor can start, and a
lock seen free cannot be retaken by a dead job. A busy lock is retried for up to 100 ms
before it counts as held (a process forked by another thread briefly shares a just-released
lock). Within a task, job and model copies go before the workspace; a failure stops that
task (its later entries are `skipped`) and the pass continues with the next task.

Descriptors are bounded: one held `ws.lock` per task in the batch (at most 32), one per
directory level of the tree being removed (at most 64: the remover lists a directory's names
and closes the listing before descending), and a reserve of about 32 for the home, database,
staging, ticket, parent and job-lock descriptors and transient listings. A soft limit of 128
covers the worst case; below it a deep tree can still exhaust descriptors, which is a
retryable refusal. Nothing is held between batches. The checks run under `ulimit -n 256`
with 300 tasks at `--batch-size 4096` and with a 1100-job task, and under a soft limit of 40
with a 60-level tree (refused with a retry reason, finished once the limit is raised).

## Durable deletion

Each entry is removed through a version-2 ticket `gc-trash/<path-digest>.json` (written to
`.tmp-ticket-XXXXXX`, synced, published without overwriting, directory synced). It records the
owned relative path, its kind (`JobFile`, `Model`, `Workspace`), the terminal task, the
settled effect/result/attempt/lease when applicable and the device/inode captured during
validation. The parent is reopened from the pinned home descriptor without following
symlinks and must have the device/inode captured during validation. The entry is then moved
atomically to `gc-trash/<path-digest>/data` without replacing anything (`RENAME_NOREPLACE`),
its device/inode compared with the ticket, its tree checked again, and only then removed
recursively by descriptor; the remover repeats the mount-root check at every level. The
staging directory is removed, then the ticket.

Within the moving pass the staged entry must have the validated device and inode. On a later
pass it must have the ticket's inode and kind of file and lie on the home's current device;
the recorded device number is not compared, because it need not survive a reboot or remount
(btrfs, overlayfs). Each ticket's state then decides: staged data matching its ticket and
proof is finished (even after the original receipt files are gone); staged data that is the
ticket's inode but fails its checks is moved back (`refused`); staged data that is not the
ticket's inode is an integrity problem and is neither removed nor moved; a move back that
fails for lack of resources leaves the data staged under a ticket naming it (rewritten with
`restore: true` when the data changed after validation, so it is only ever moved back); a ticket whose entry never
moved is dropped and the entry classified afresh; a ticket whose data is gone is completed.
A staging rename that fails (e.g. `EXDEV`) drops its unexecuted ticket at once, and an entry
swapped between validation and the move is moved back, so neither wedges later passes.
Unpublished `.tmp-ticket-XXXXXX` files are removed; any other unknown `gc-trash` entry stops
the pass and is left for inspection. Interrupted collection can be rerun; exports use retained
blobs and inputs and remain valid.

## Report and exit codes

The report has `dry_run`, `batch_size`, `batches`, a `summary` counting every entry by status
(`candidate`, `deleted`, `collected`, `retained`, `refused`, `skipped`), `truncated`, and at
most 1000 `entries` (refused and skipped first) with a relative path, status and reason.
Reasons hold at most 1024 UTF-8 bytes plus an ellipsis, name paths relative to the candidate
(never `/proc/self/fd/...`), and summarize malformed JSON by error class and position without
quoting content. `gc` prints the report, then exits 1 if anything was refused or skipped,
0 otherwise; usage errors (including a missing home) exit 2. Dry runs create nothing (a
missing `ws.lock` is one nobody holds) and remove nothing. No task state changes and no recovery, provider or VM
execution occur.

## Publication fault coverage

A per-BlobStore publication hook injects errors before file fsync, rename or directory
fsync; it has no environment/global switch. Default stores have no hook. The test drives a
task with `run_task` over a store whose hook injects ENOSPC at each boundary once the first
effect has really executed: the runner stops with the effect DISPATCHED, no artifact
reference and no temporary file; recovery on a healthy store publishes the retained outcome
without re-executing it, and the task then succeeds.

## Validation

Covered offline (`tests/gc.rs`, `tests/publication.rs`, CLI `gc_*`): completed collection
with logs, status and receipt kept and a `collected` rerun; non-terminal, pending-cancel and
outstanding-effect retention, including dispatched/unknown model calls with a retained
response; a non-terminal task beside a terminal one; live job and workspace locks;
inspection/jail leftovers; missing, superseded and mismatched receipts kept without blocking;
corrupt blobs and responses; symlink roots, children and hard links; symlinked workspaces,
model and task directories; unknown names; forged tickets (traversal, absolute, dot-dot,
kind and key mismatch, symlinked data, unowned directories, version 1, garbage, foreign temp
files); an oversized workspace retaining only its task; an output over 32 MiB; a symlinked
home path; more than 1000 job directories under a 256-descriptor limit (re-executed child
with `ulimit -n 256`); batches of one; crash points after the ticket, after the move and
after removal; ancestor and candidate substitution; an entry swapped before staging;
bounded, content-free reasons; exit codes; never creating a home; (second review) a failed
task with a stale pending cancel collected; 300 tasks at `--batch-size 4096` under
`ulimit -n 256`; descriptor exhaustion mid-removal retried; a staged entry finished although
its recorded device changed and refused while classifying when its inode differs; a dry run
creating no lock file; a superseded model response kept; a job lock taken after validation
keeping its job; the lock taken before the home is opened; the mount gate failing unless all
three of its tests ran.

The mount gate (`sh scripts/check.sh mount`, Compose service `test-mount` = the `test`
image plus `CAP_SYS_ADMIN` and AppArmor unconfined, `AGENTOS_GC_MOUNT_TESTS=1`) mounts a
writable tmpfs inside a real workspace candidate (validation refuses it), inside staged
data after validation (only the remover's own check can refuse it), and on `gc-trash`
(the staging rename fails with `EXDEV` and later passes are not wedged). In the gate a
failed mount fails the test; elsewhere the tests print a skip line and return. Without the
statx check the first two tests fail (the remover empties the tmpfs).
No dependency changes: checked current tempfile 3.27.0 and rustix 1.1.5 online at
https://docs.rs/crate/tempfile/latest and https://docs.rs/crate/rustix/latest.

Known limits (not addressed): restoring onto a reoccupied path is refused and needs manual
cleanup (never data loss); the tree check walks paths (read-only); every pass re-reads all
job-directory remnants and every blob fully into memory (O(history) time, O(largest blob)
memory); host-worker temporary directories under `work/<task>/` and atomic-write leftovers
are never reclaimed; `test-mount` builds a second image.

Not verified offline: real bind-mount roots of foreign filesystems, Firecracker/KVM and
jailed job directories (`v.sock`, hard-link counts while jailed, `ws.img` owned by uid 61000),
power-loss durability, and kernels older than 5.8 (where every pass refuses).
