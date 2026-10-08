# Kernel provenance and fresh-host installation — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package 11 of the [v0.1 completion plan](2026-10-04-v01-completion.md), as designed in
[Kernel provenance and fresh-host installation](../specs/2026-10-09-kernel-and-installer-design.md).

**Constraints:** red/green; `sh scripts/check.sh` after each commit; `df -h /` before long
builds; no co-author trailer.

- [x] **1. Kernel builder.** A `kernel-builder` Compose service pinned by digest with build
  dependencies from the pinned Debian snapshot; `scripts/build-kernel.sh` fetches and checks
  the source, applies the committed Firecracker fragments, checks the resolved config
  against the committed one, builds `vmlinux` reproducibly, and `--verify` builds twice in
  fresh volumes and compares.
- [x] **2. Source-kernel recipe.** `guest/python-stdlib-py314-v2` takes its kernel from the
  builder; `image.json` records the kernel provenance; `build-guest-image.sh --verify` stays
  byte-identical.
- [ ] **3. Kernel on KVM.** The booted kernel's version and embedded config match the record;
  the full KVM suite passes on the new image.
- [ ] **4. Release.** `scripts/release.sh VERSION` builds a static musl `agentos` and the
  release tree with `MANIFEST.json` and `SHA256SUMS`.
- [ ] **5. Installer.** `scripts/install.sh` with every refusal and `--self-test` covering
  each, plus idempotence and the same-version-different-content refusal.
- [ ] **6. Smoke run.** `scripts/smoke-install.sh` in a fresh container on the KVM host:
  install, register, jailed fixture with the analyzer, kill and resume, export, compare.

**Gate:** reproducible, provenanced kernel and image; the release installs and works in a
clean environment without manual repairs; existing homes are preserved.
