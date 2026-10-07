# Conservative GC Implementation Plan

> **For agentic workers:** Use superpowers:executing-plans inline, with a final independent review. Steps use checkbox syntax.

**Goal:** Collect proven redundant transient paths without changing recovery or exports.
**Architecture:** The CLI holds the driver lock. The engine plans owned candidates, takes
job/workspace locks, validates published blob copies and rechecks before removing them.
The store supplies a scoped publication fault hook for ENOSPC recovery tests. Collection
pins directories and atomically stages deletion with durable ownership tickets, so
path replacement cannot redirect removal and partial deletion can retry.
**Tech Stack:** Existing Rust, SQLite, Docker Compose; no new dependencies.
**Spec:** [focused design](../specs/2026-10-04-conservative-gc-design.md).

## Global constraints

Keep every task/input/journal/registry/blob and job evidence (logs, status, request,
receipt, outcome); collect only terminal, fully settled tasks, all-or-nothing per task.
List at most 1000 report entries (the summary counts all) and walk trees of at most 10000
entries and 64 levels. Retain inspection/jail leftovers. Hold `ws.lock` for the batch and a
job's lock while its files go. Never use real user data in deletion tests.

Revised 2026-10-07 after the adversarial review (see the review record): per-task decisions,
batches with bounded descriptors, kept job evidence, a real mount gate, exit codes.

## Review focus

- Terminal tasks can have outstanding effects: test the state independently.
- Receipts can be absent/corrupt: refuse their task's transient copies.
- Locks outlive controller processes: acquire real file locks during tests.
- Paths can redirect outside the home: refuse roots, children and hard links.
- Publication can fail after rename: recovery must publish the intact receipt once.

## Task 1: Scoped publication failures

Files: store `src/blob.rs`; engine `tests/publication.rs`.
Interface: `PublicationStage` and `BlobStore::with_publication_hook`.

- [x] Add failure tests at file sync, rename and directory sync. Run through Compose;
  expect failed publication rather than a success digest/reference.
- [x] Route `put` through an optional per-store hook at those exact boundaries;
  clean pre-rename temporary files and retain post-rename objects for later recovery.
- [x] Recover retained verification output after ENOSPC; assert settled effect and
  no additional executor run. Run store/publication tests; expect all green.

## Task 2: Collector and command

Files: engine `src/gc.rs`, `src/gc/confined.rs`, `tests/gc.rs`, `src/lib.rs`; CLI `src/commands/gc.rs`,
`src/commands/mod.rs`, `src/args.rs`, `tests/cli.rs`.
Interface: `collect(lock: &HeldDriverLock, db: &Db, blobs: &BlobStore, opts: Options)`;
`HeldDriverLock::verify(root, &driver_lock_file)` proves the driver lock. The JSON report
has a per-status summary and bounded entries with status/reason; the CLI exits 1 when
anything was refused or skipped.

- [x] Write behavioral tests for the complete validation list in the focused spec.
  Add CLI test invoking the absent `gc --dry-run`; expect command rejection.
- [x] Implement planning, task/receipt/reference checks, lock acquisition and
  revalidation, path refusal and bounded reports. Wire CLI after taking the lock.
- [x] Run focused engine and CLI GC tests; expect redundant files removed, exports
  intact, all unsafe/unknown paths retained. Run regression gates; expect green.
- [x] Document usage and conservative limits; record completed offline scope and
  pending real acceptance. Review the focused diff independently before finishing.

Commands:

Review fixes add deterministic regressions for ancestor/candidate replacement after
validation, interrupted deletion after receipt removal, and over 1000 historical empty
workspace parents. Descriptor-relative rename/removal uses durable versioned tickets
and pre-move device/inode pins. Refuse real mount roots in validation and removal;
run the mount test with a read-only disposable Docker bind mount.

```sh
docker compose run --rm test cargo test -p agentos-store --locked
docker compose run --rm test cargo test -p agentos-engine --test publication --test gc --locked
docker compose run --rm test cargo test -p agentos-cli --test cli --locked gc_
sh scripts/check.sh mount   # the test-mount service: real tmpfs mounts inside candidates
sh scripts/check.sh
```
