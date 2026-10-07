# Conservative GC continuation

Continues package 8 of the v0.1 completion plan at base `142f58c` in the existing
`codex/v01-completion` worktree. Real provider/KVM setup remains deferred.

Implemented `agentos gc --dry-run` and `agentos gc`: terminal/settled-only collection,
real driver/job/workspace locks, journal/receipt/blob ownership checks, bounded reports,
path/link refusal and preserved exports. Every input, journal, registry and blob stays.
Validation refusals retain the entire pass; inspection/jail cleanup remains external to GC.

An independent read-only review found a path-substitution defect, interrupted-deletion
recovery gap, accumulated historical-parent limit and a socket test fixture error.
Each was fixed in one pass. New regressions reproduced the first three against the
original collector; the revised collector pins directory handles and uses descriptor
relative removal plus durable tickets and atomic staging. Candidate identity is captured
before the deletion boundary and checked again after staging. More than 1000 previously
collected workspace parents no longer consume the report limit. The socket fixture uses
the existing descriptor path helper to stay under Unix socket path limits.

The scoped BlobStore fault hook injects ENOSPC before file fsync, rename and directory
fsync. Tests verify publication leaves no successful effect/artifact reference, removes
pre-rename temporary files, and recovers an intact retained receipt without reexecution.

Focused evidence: `/tmp/agentos-publication-red.log`, `/tmp/agentos-gc-red.log`,
`/tmp/agentos-gc-cli-red.log`, `/tmp/agentos-gc-ownership-red.log`,
`/tmp/agentos-gc-review-red.log`, `/tmp/agentos-gc-leaf-red.log`,
`/tmp/agentos-gc-review-green.log`, and host/fake CLI logs `/tmp/agentos-gc-cli*.log`.
The first broad gate failed on the socket fixture; its failure is preserved in
`/tmp/agentos-gc-final-gates.log`. All four gates after review fixes passed exit 0 in
`/tmp/agentos-gc-reviewed-final-gates.log`.

A final filesystem regression used a real read-only Docker bind mount with disposable
data. RED `/tmp/agentos-gc-mount-red.log` accepted mounted data; GREEN
`/tmp/agentos-gc-mount-green.log` proves statx mount-root refusal. Validation and recursive
removal both repeat the mount check and fail closed when it is unavailable. No KVM or
additional container capabilities were used. The four gates after this final change
passed exit 0 in `/tmp/agentos-gc-final-with-mount-gates.log`.

The final report-size test exposed an oversized serde error from a malformed receipt.
Reasons now retain a 1024-byte UTF-8 prefix and an ellipsis. RED is preserved at
`/tmp/agentos-gc-report-red.log`; all 16 focused GC cases and the ENOSPC publication
test pass in `/tmp/agentos-gc-all-focused-final.log`. The final four gates are recorded
in `/tmp/agentos-gc-complete-gates.log` and passed exit 0: formatting, strict Clippy,
the full host suite and the full fake-jail suite. Gated early-return cases do not
establish real provider/KVM acceptance. Final `git diff --check` passed.

Review exclusions remain open: actual live/KVM acceptance, external cgroup/inspection
collection, active-task collection, and inherited general home-opening/driver-lock
path handling. This patch adds no dependencies. Packages 9–12 still require focused
resource/component/packaging designs and their real acceptance environments.

## Adversarial review follow-up (2026-10-07)

A second, adversarial review of commit `65e0478` found no critical defect (confinement,
ticket validation and model-retention eligibility held) but four important findings, all
addressed on `gc-collect`:

1. **Unpublished evidence was deleted.** Whole job directories went, logs included. Owner
   decision: keep the logs. Only `output.bin` (a copy of the published blob), `scratch.img`
   and a Firecracker job's `v.sock` are removed now; logs, status, request, receipt and
   outcome stay, and a later pass reports the remnant `collected`.
2. **One problem anywhere stopped all collection, permanently, with exit 0.** Decisions are
   now per task (`retained`); only integrity problems stop the pass. The 1000-candidate cap
   became batches of whole tasks; descriptors are held only for the batch in flight (the
   reviewer's 600-job home failed with `EMFILE`; a 1100-job task now collects under
   `ulimit -n 256`). The home path is canonicalized once; oversized workspaces and outputs,
   receipt-less and superseded attempts no longer block other work. `gc` exits 1 on
   refusals or skips.
3. **The mount regression never ran.** It now runs in the `test-mount` Compose service
   (`sh scripts/check.sh mount`, a CI matrix entry) with a writable tmpfs inside real
   candidates, including one that only the recursive remover can catch.
4. **Hostile cases were untested.** Forged tickets, symlinked candidates and task dirs,
   unsettled model calls, superseded attempts and three crash points per entry kind are
   covered, and the publication test now drives ENOSPC through `run_task`.

The cheap minor findings were fixed too: exit codes, `skipped`/`retained` statuses after a
stop, identity captured during validation, ticket repair (unexecuted tickets dropped,
mismatched in-pass moves undone, own temp files cleaned) instead of wedges, a
`HeldDriverLock` proof for `collect`, no home creation by `gc`, and reasons without
descriptor paths or quoted content. The current design is in the
[focused spec](../superpowers/specs/2026-10-04-conservative-gc-design.md). Still not verified
offline: real foreign bind mounts, jailed/KVM job directories, power-loss durability and
kernels older than 5.8.

### Second review (2026-10-07)

A second adversarial review ("merge after fixes", no critical defect) found that the held
`ws.lock`s were bounded by `--batch-size` entries, not tasks: 300 tasks at batch size 4096
under `ulimit -n 256` exhausted descriptors, and a failed move back was reported as an
integrity stop that stranded staged data. Batches are now capped at 32 tasks (lower when
the descriptor limit is low), the remover holds one descriptor per level, and resource
exhaustion is a retryable refusal that never strands staged data. The docs now state that
only problems found while classifying stop a pass before any deletion, and that a task
interrupted mid-way is finished by the next pass. Also fixed: failed tasks with a stale
pending cancel were never collected; staged data was matched by a device number that need
not survive a reboot; the lock proof and the collected directory could differ; `gc` opened
the home before locking; the mount gate passed with zero tests; dry runs created lock files.
Three mutants that survived the suite (no `settled()` check for model copies, no job-lock
re-take before deletion, no staged-identity check while classifying) are now killed by tests.
