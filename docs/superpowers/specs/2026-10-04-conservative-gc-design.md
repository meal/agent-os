# Conservative transient collection

This refines the approved v0.1 completion design, package 8. Development and
verification use the existing isolated worktree and Docker Compose; actual live/KVM
acceptance remains deferred. Revised 2026-10-07 after an adversarial review (see
[the review record](../../reviews/2026-10-04-conservative-gc-review.md)): job evidence is
kept, decisions are per task, work is batched, and the mount regressions run in a gate.

## Ownership and retention

`agentos gc --dry-run` and `agentos gc` operate under the exclusive home driver lock;
`collect` demands a `HeldDriverLock`, verified against `<home>/driver.lock` (same inode,
lock held), so it cannot be called without it. `gc` requires an existing home and never
creates one. The home path is canonicalized once; everything inside it is opened
descriptor-relative without following symlinks.

The collector accepts only terminal tasks with no pending cancellation and no INTENDED,
DISPATCHED or UNKNOWN effects. It keeps journals, task inputs, registries and every blob.
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

An integrity problem stops the whole pass, and before any deletion when found while
classifying: an unknown name under `jobs`, `model`, `work` or `gc-trash`; a symlink or
non-directory where an owned directory is expected; a job or model directory naming an
effect the journal does not know; a symlinked workspace; a mount root inside a candidate
(Linux statx `STATX_ATTR_MOUNT_ROOT`; unavailable detection also refuses); an invalid,
foreign or older-version deletion ticket; staged data that is not what its ticket staged;
a corrupt blob. Integrity problems and failures are `refused`; entries not processed after
a stop are `skipped`. Unknown entries are never removed automatically.

## Passes, batches and descriptors

A pass first scans names (one directory descriptor at a time) and groups entries by task;
a job or model directory is attributed through the effect its name carries. It then
classifies every task without deleting anything: locks are probed (taken and released),
trees are walked by path below a pinned directory descriptor, and the device/inode of each
entry and of its parent is captured during this validation.

Eligible tasks are then processed in batches of whole tasks (`--batch-size`, default 64
entries; a task larger than the batch is a batch of its own). For each batch the tasks are
revalidated while their `ws.lock` is taken and held until the batch ends; the
`Validated` boundary follows; then each entry is removed. A job lock is taken again and
held while that job's files are removed. Releasing job locks between validation and
deletion is sound because the driver lock is held: no new supervisor can start, and a
lock seen free cannot be retaken by a dead job. A busy lock is retried for up to 100 ms
before it counts as held (a process forked by another thread briefly shares a just-released
lock). Within a task, job and model copies go before the workspace; a failure stops that
task (its later entries are `skipped`) and the pass continues with the next task.

Descriptors are bounded: one held `ws.lock` per task in the batch, one per directory level of
the tree being removed (at most 64), and a small constant. Nothing is held between batches.

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

On a later pass each ticket's state decides: staged data matching its ticket and proof is
finished (even after the original receipt files are gone); staged data that is the ticket's
inode but fails its checks is moved back (`refused`); staged data that is not the ticket's
inode is an integrity problem and is neither removed nor moved; a ticket whose entry never
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
0 otherwise; usage errors (including a missing home) exit 2. Dry runs can create workspace
lock files but never remove task data. No task state changes and no recovery, provider or VM
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
bounded, content-free reasons; exit codes; never creating a home.

The mount gate (`sh scripts/check.sh mount`, Compose service `test-mount` = the `test`
image plus `CAP_SYS_ADMIN` and AppArmor unconfined, `AGENTOS_GC_MOUNT_TESTS=1`) mounts a
writable tmpfs inside a real workspace candidate (validation refuses it), inside staged
data after validation (only the remover's own check can refuse it), and on `gc-trash`
(the staging rename fails with `EXDEV` and later passes are not wedged). In the gate a
failed mount fails the test; elsewhere the tests print a skip line and return. Without the
statx check the first two tests fail (the remover empties the tmpfs).
No dependency changes: checked current tempfile 3.27.0 and rustix 1.1.5 online at
https://docs.rs/crate/tempfile/latest and https://docs.rs/crate/rustix/latest.

Not verified offline: real bind-mount roots of foreign filesystems, Firecracker/KVM and
jailed job directories (`v.sock`, hard-link counts while jailed, `ws.img` owned by uid 61000),
power-loss durability, and kernels older than 5.8 (where every pass refuses).
