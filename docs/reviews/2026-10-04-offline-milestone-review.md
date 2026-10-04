# v0.1 offline reliability milestone

Base: `8c273b1`; implementation branch: `codex/v01-completion` in the isolated worktree.
The user requested implementation and then explicitly deferred provider/KVM setup.
No paid API calls, real KVM tests, merges or pushes were performed.

## Completed scope

- Task deadlines bound model calls and preserve uncertain billing accounting.
- Successful HTTP bodies are capped at 4 MiB; definite error bodies at 4096 bytes.
- Key files are bounded at 4096 bytes and reject symlinks/nonregular descriptors promptly.
- New submissions record model policy/limits version 1 and the normalized provider endpoint.
  Permanent failures stop; transient failures use durable backoff/Retry-After scheduling.
  Policy 0 preserves legacy observation shape and retry decisions.
- Serialized requests are capped at 8 MiB before journal/blob/reservation creation.
- Scoped Tokio/UUID updates, pinned pyenv/Python development, interpreter compatibility
  tests and a new candidate guest recipe preserve the legacy image default.
- Default rustfmt, strict Clippy, shared Compose checks, PR/push CI and gated manual
  acceptance workflows are implemented. Branch-protection and runner/environment
  configuration remain administrator work.
- Host and fake-jail harnesses repair through a loopback fake API, record every provider
  attempt and replay exact request/call/workspace/patch/protected-profile results.

## Verification

`COMPOSE_PROJECT_NAME=agent-os sh scripts/check.sh` passed all four gates before review:
formatting, strict Clippy, full host tests, full fake-jail tests. The combined log reports
1810 passed and 4 ignored cases, including gated early-return cases; these totals do not
establish real provider/KVM acceptance. Logs: `/tmp/agentos-v01-final-gates.log`.

Check dispatch, acceptance preflight/failure/no-overwrite tests, guest build deletion
self-tests, both-interpreter fixture checks and checksum-verified actionlint 1.7.12 passed.
`acceptance.sh offline` also passed end-to-end and preserved its own local attempt log.

Final required gates after the review fix also passed, exit 0. The combined log reports
1814 passed, 0 failed and 4 ignored cases, including gated early returns.
Log: `/tmp/agentos-v01-reviewed-gates.log`. Final `git diff --check` passed.
The script preflight/failure/no-overwrite and check-dispatch tests passed after the fix.

## Fresh branch review

One independent read-only reviewer inspected `8c273b1..1aebe3a`, the requirements and logs.
No Critical findings. One Important finding was reproduced and fixed: response-only
recordings omitted transient HTTP rejections, so a successful 429→repair run could not
replay its five attempts. RED showed five calls versus four. Version-2 ordered recordings
now preserve typed provider outcomes, exact response bytes/usage, request pins and retry
metadata. Both host and fake-jail 429→repair regressions pass; legacy depth-keyed fixtures
remain supported. Additional tests cover repeated requests, wrong request pins, transport,
malformed response bytes, usage and unknown recording versions.

One Minor remains deferred: add explicit pause/resume coverage while the original retry
not-before time is still in the future. Pause stopping, crash/resume and durable schedule
behavior are covered separately; the reviewer found no implementation defect here.

## Rulings and unresolved gates

- Work in an isolated ignored worktree and retain the ledger because the master plan is
  unfinished. Cost if wrong: work is discoverable through the branch/worktree, not main.
- Use `AfterComplete` with effect kind `model_retry` for the schedule crash hook. Cost if
  wrong: existing generic crash matrices would miss this distinct retry boundary; focused
  retry crash tests cover it.
- Strip policy-1 failure metadata from policy-0 observations. Cost if wrong: legacy request
  decisions or serialized observations diverge; compatibility fixtures cover the boundary.
- Digest the harness's serialized contract exactly as submission does. Cost if wrong:
  acceptance exports fail contract validation.
- The reviewer set aside real provider/KVM acceptance, candidate guest boot and independent
  reproducibility, final host/kernel/Firecracker provenance and secret review, and
  two-repository release evidence. These gates remain open; no simulated or skipped run
  counts. Cost if prematurely closed: unsupported compatibility/isolation/release claims.
- The reviewer set aside GC/outstanding-effect retention, host ENOSPC publication, VM
  disk/I/O/memory measurements, executable-mode restrictions, Wasm capability/ABI/runtime
  behavior, source-built kernel and fresh-host installer safety. Packages 8–12 remain
  planned, with focused designs and preceding acceptance dependencies. Cost if treated
  as shipped: these features and guarantees are absent from this milestone.
- The reviewer set aside administrator branch-protection/environment/runner configuration
  and independent upstream version verification. Workflow enforcement needs administrator
  configuration; the executor checked official upstream releases and recorded pins. Cost
  if omitted: checks may not be mandatory or acceptance runners may be unavailable.
- The reviewer limited transcript API redesign to its concrete new acceptance failure.
  Preserve legacy fixture semantics and version the new ordered recording format. Cost
  if wrong: old fixtures or replay consumers lose compatibility; full suites exercise both.

The master [plan](../superpowers/plans/2026-10-04-v01-completion.md) is not complete.
Real provider/KVM acceptance is the next milestone gate; collection, resource limits,
components and release packaging remain future packages. The candidate recipe explicitly
records that copying a pinned interpreter is not an independent Python source rebuild.
