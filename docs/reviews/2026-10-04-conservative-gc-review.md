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
