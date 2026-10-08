# Minimal component analyzer — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Package 10 of the [v0.1 completion plan](2026-10-04-v01-completion.md), as designed in
[Minimal component analyzer](../specs/2026-10-08-component-analyzer-design.md).

**Constraints:** red/green per step; `sh scripts/check.sh` after each commit; no co-author
trailer; versions checked online on 2026-10-08/09: Wasmtime 49.0.2, wit-bindgen 0.62.0,
wasm-tools 1.261.0 (release binary, sha256
`ad62b2176037e93e1348cb65d6212d128ca9f097b63d155569f25215818ff7b1`).

- [x] **1. Contract and effect kind.** `snapshot.analyze` capability and the optional
  `analyzer` pin, each requiring the other; old contract digest unchanged.
  `EffectKind::AnalyzeSnapshot` (tag, capability, retry policy); `follow_up_event` is `None`.
- [x] **2. Runtime crate.** `agentos-component` with Wasmtime (default features off), the WIT
  world, the `tree` host resource behind an authority trait, read validation shared with
  `ShadowReader`, budgets, fuel, memory limits, epoch backstop and report checks. Red cases
  as WAT components: granted read, ungranted, revoked between reads, another task's tree,
  infinite loop, memory growth, oversize, malformed and `Err` reports, budgets, path
  validation, an extra import.
- [x] **3. Tooling and reference analyzer.** The image gains `wasm32-unknown-unknown` and the
  pinned wasm-tools. `components/repo-analyzer` builds with wit-bindgen; a script builds the
  component; the bytes are committed with a rebuild-and-compare test and an imports test.
- [ ] **4. Registry.** `agentos component register|list`, refusing non-components, extra
  imports and a missing `analyze` export.
- [ ] **5. Engine.** `ComponentExecutor` routed in the controller, retention under
  `analysis/`, the runner's one analysis after the snapshot, crash matrix for the new kind
  with an execution counter, GC rule.
- [ ] **6. CLI and export.** Approval shows the capability and the pin; `submit` copies the
  component into the task and records it; the manifest's optional `analysis` entry and
  `analysis/report.json`.
- [ ] **7. End to end.** The fixture task with the reference analyzer on the fake and the real
  Firecracker worker; a report claiming `"passed": true` changes no verdict.

**Gate:** granted-only access proven by red tests; analysis survives every crash point
without a second run after retention; its report is exported and never verifies.
