# VM disk and I/O resources — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package 9 of the [v0.1 completion plan](2026-10-04-v01-completion.md), as designed in
[VM disk and I/O resources](../specs/2026-10-08-vm-resources-design.md).

**Constraints:** every step is red/green; the offline gate (`sh scripts/check.sh`) passes after
each commit; KVM steps run on an otherwise idle host; commits carry no co-author trailer.

## Offline

- [x] **1. Executable-mode rejection.** Red: `profile register` accepts `["./check.sh"]` and
  `["check.sh"]` when `check.sh` is in the profile. Green: reject both with the interpreter
  hint; accept `["sh", "check.sh"]`, `["python3", "check.py"]` and `["/usr/bin/true"]`.
- [x] **2. Contract fields.** Red: pinned digest of the fixture contract; round-trip of the four
  fields; each lower bound, upper bound and `u32::MAX` rejected with the field name. Green:
  optional `Limits` fields with `skip_serializing_if`, validation in `Contract::validate`.
- [x] **3. Resolution.** Red: `VmResources::resolve` defaults (version 1); version-0 values for
  a journal without the record. Green: `agentos-core::resources`.
- [x] **4. Record and replay.** Red: `Submitted` carries `vm_resources` for Firecracker tasks
  only; `resume` with a record that disagrees with the contract exits 1 and journals nothing.
  Green: CLI submit/resume plumbing into `FirecrackerConfig.resources`.
- [x] **5. Manifest.** Red: export includes `vm_resources`; a 2026-10-08 manifest still
  deserializes (already covered by the evidence test). Green: optional manifest field.
- [x] **6. Rendering.** Red: golden `vm.json` with and without rate limiters; the rootfs never
  has one. Green: `Drive.rate_limiter: Option<RateLimiter>`.
- [x] **7. Sizes, free space and file-size limit.** Red: `ws.img` and `scratch.img` have the
  contracted lengths; an existing wrong-length `ws.img` fails the effect and the inspection
  and is left untouched; the free-space check refuses at `submit` and fails a job before
  launch; the jail's `RLIMIT_FSIZE` is the larger image. Green: sizes from `resources`, the
  length check, the `statvfs` check with a test seam, the jail limit.

## Real KVM

- [ ] **8. Guest view and full drives.** The guest sees the contracted sizes; filling the
  workspace or scratch fails visibly and never verifies.
- [ ] **9. Rate enforcement.** With 8 MiB/s, a 40 MiB write with `fsync` inside the timed
  region takes at least 3.5 s. The minimum rates still boot, format scratch and snapshot
  within the timeouts, or the bounds rise.
- [ ] **9b. Host ENOSPC.** Work root on a small tmpfs: ENOSPC during snapshot, patch and a
  verification scratch write fails visibly, never verifies, and leaves the journal's digest
  equal to the inspected image or fails the task. Decide whether it can be reported as an
  infrastructure failure. A jailed task with `worker_disk_mib` above 1024 runs (file-size limit).
- [ ] **10. Recovery.** Kill after launch and resume a task with non-default resources; the
  recorded values are used and the bundle equals an uncrashed run's.
- [ ] **11. Measurements and overhead.** Run the measurement matrix from the design, record it
  under `docs/evidence/`, and set `JAIL_MEMORY_OVERHEAD_MIB` from it.
- [ ] **12. Infrastructure OOM.** Low `memory.max` during a write-heavy verification: no
  `VerifyPassed`, a visible failure reason, `oom_kill > 0`.

```sh
sh scripts/check.sh
docker compose run --rm test-kvm cargo test --workspace --locked
docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker test-kvm cargo test -p agentos-engine --locked --test kvm_tier --test worker_conformance --test crash_matrix
```

**Gate:** real limits match the recorded contract; old tasks replay with version-0 values;
resource failure is visible and never accepted as verification success.
