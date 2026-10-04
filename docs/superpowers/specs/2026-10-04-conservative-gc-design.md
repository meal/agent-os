# Conservative transient collection

This refines the approved v0.1 completion design, package 8. Development and
verification use the existing isolated worktree and Docker Compose; actual live/KVM
acceptance remains deferred.

## Ownership and retention

`agentos gc --dry-run` and `agentos gc` operate under the exclusive home driver lock.
The collector accepts only terminal tasks with no pending cancellation and no
INTENDED, DISPATCHED or UNKNOWN effects. It keeps journals, task inputs, registries
and every blob. It checks every registered blob's integrity before deleting anything.

Owned candidates are `<home>/jobs/<effect>-<attempt>` with matching request and
published outcome, `<home>/model/<effect>-<attempt>` with matching retained response,
and `<home>/work/<task>/{ws,workspace,ws.img}`. Job/model outcomes must match the
settled effect's result digest and a readable registered blob. Missing or malformed
receipts are retained. Unknown directories and names are refused.

Job locks and workspace locks are taken exclusively and held during deletion. A live
job, busy workspace, any inspection entry, or any leftover jail/cgroup marker blocks
collection for its entire task. This first collector does not manage external cgroups
or delete inspection directories; existing worker reconciliation owns those operations.
Nested symlinks, special files, hard-linked regular files and traversal are refused.
Mount roots inside candidate trees are refused using Linux statx; unavailable
mount-root detection also retains data. The recursive remover repeats this check.
The single-link top-level `v.sock` of a recorded Firecracker job is allowed after
the same receipt and lock checks; the worker shuts down its VM before publishing.
All eligible paths are planned before deletion, then ownership, references and locks
are rechecked. Keep workspace parents and `ws.lock` so the held inode cannot be replaced.
Delete job/model copies before terminal workspace contents. Interrupted collection
can be rerun; exports use retained blobs and inputs and remain valid.

## Durable deletion

Pin the home and each candidate parent using directory descriptors opened without
following symlinks. Rename and recursive removal use descriptor-relative operations;
substituting an absolute ancestor cannot redirect deletion. Capture the candidate's
device/inode during revalidation and compare it again after staging. A replacement
is retained. Lock-file descriptors and workspace parents remain held throughout.

Before moving data, publish a version-1 ticket as `gc-trash/<path-digest>.json`, sync
its file and parent, then atomically move the candidate to `gc-trash/<path-digest>/data`
without overwriting an existing destination. The ticket records the original owned
relative path, terminal task, settled effect/result/attempt when applicable, and the
source device/inode. Revalidate that proof against the journal and registered blobs
on restart. A partially removed payload need not retain its original receipt files.
The ticket outlives both payload and staging-directory removal. Interrupted moves,
partial recursive deletion and completed deletion with a remaining ticket can retry.
Unknown, redirected, corrupt or mismatched tickets/data are retained.

## Bounded report and failures

Report relative paths and `candidate`, `deleted` or `refused` status plus a reason.
Reasons retain at most 1024 UTF-8 bytes plus an ellipsis, including malformed JSON errors.
Limit the report to 1000 candidates and each candidate tree to 10000 entries.
Historical workspace parents containing only locks are streamed past and do not
consume the candidate limit. Any validation refusal, including limit exhaustion,
retains the entire pass before deletion. Dry runs can create lock files but never
remove task data. An I/O failure during deletion is reported as a refusal; no task
state changes and no recovery/provider/VM execution occur.

## Publication fault coverage

A per-BlobStore publication hook injects errors before file fsync, rename or directory
fsync; it has no environment/global switch. Default stores have no hook. Tests inject
ENOSPC at each boundary, verify no artifact/effect success reference is committed,
clear the fault and recover an intact retained receipt without reexecuting it.

## Validation

Cover completed collection and repeatability, nonterminal and pending-cancel retention,
terminal outstanding effects, live job/workspace locks, inspection/jail leftovers,
missing/corrupt receipts/blobs, symlink roots and children, hard links, unknown paths,
bounded enumeration, driver-lock exclusion, unchanged exported patch/evidence,
publication recovery, ancestor/candidate substitution after validation, interruption
after receipt removal, and more than 1000 historical empty workspace parents.
Run formatting, strict Clippy, full host and fake-jail suites.
The mount regression is enabled with `AGENTOS_GC_MOUNT_FIXTURE` naming a tree
containing a read-only Docker bind mount at `mounted/`, with `foreign` containing
`foreign data must remain`. It tests the actual mount without KVM or extra capabilities.
No dependency changes: checked current tempfile 3.27.0 and rustix 1.1.5 online at
https://docs.rs/crate/tempfile/latest and https://docs.rs/crate/rustix/latest.
