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
- [ ] **4. Record and replay.** Red: `Submitted` carries `vm_resources` for Firecracker tasks
  only; `resume` with a record that disagrees with the contract exits 1 and journals nothing.
  Green: CLI submit/resume plumbing into `FirecrackerConfig.resources`.
- [ ] **5. Manifest.** Red: export includes `vm_resources`; a 2026-10-08 manifest still
  deserializes (already covered by the evidence test). Green: optional manifest field.
- [ ] **6. Rendering.** Red: golden `vm.json` with and without rate limiters; the rootfs never
  has one. Green: `Drive.rate_limiter: Option<RateLimiter>`.
- [ ] **7. Sizes and preallocation.** Red: `ws.img` and `scratch.img` have the contracted
  lengths and allocated blocks (`st_blocks * 512 >= len`); an existing wrong-length `ws.img`
  fails the effect and is left untouched; a preallocation fault seam fails the job before
  launch with the host-disk reason. Green: `fallocate` replacing `sparse`, the length check,
  the `submit` capacity preflight.

## Real KVM

- [ ] **8. Guest view and full drives.** The guest sees the contracted sizes; filling the
  workspace or scratch fails visibly and never verifies.
- [ ] **9. Rate enforcement.** With 8 MiB/s, a 40 MiB write takes at least 3.5 s.
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
