# Minimal component analyzer — design

Status: design for package 10 of the [v0.1 completion plan](../plans/2026-10-04-v01-completion.md).
Parent: [v0.1 completion design, "Minimal component ABI"](2026-10-04-v01-completion-design.md#minimal-component-abi).

## Goal

One WebAssembly component, the reference repository analyzer, reads a task's recorded
snapshot through granted handles only and returns a bounded report that is exported with the
task. It has no filesystem, network, clock or environment of its own. Running it is a
journaled effect with the same recovery guarantees as every other effect. Its report can
never contribute to `VerifyPassed`.

## Where it runs in a task

A contract opts in with an `analyzer` field and the `snapshot.analyze` capability. Each
requires the other; a contract with neither behaves exactly as today.

```json
"analyzer": { "id": "repo-analyzer-v1", "digest": "<64 hex>" },
"capabilities": [..., "snapshot.analyze"]
```

The runner issues one `AnalyzeSnapshot` effect after the first successful `ReadSnapshot` and
before the first agent turn. Its outcome is journaled; a failed analysis does not fail the
task, since the report is advisory. The agent does not see the report in v0.1: adding a model
tool would change request digests and break replay of recorded transcripts.

Alternatives considered:

- **A post-task `agentos analyze` command, like `export`.** Rejected: export is an
  authorization plus a read, not a journaled effect, and the effect machinery runs only while
  the task runs. Every capability but `artifact.export` also expires at the task deadline.
- **Running in the supervised job process.** Rejected: the host import must consult the
  broker on every read, which needs the database; the job process has none.

## Capability

`snapshot.analyze` (`Capability::SnapshotAnalyze`) has `Scope::Task` and expires at the task
deadline like the others. It is issued at approval and shown in the approval summary. Every
host import call authorizes against it with `Db::check` (a pure check), so a revocation takes
effect at the next read. The effect's intent and dispatch are authorized as usual.

## Registry and pinning

`agentos component register DIR` copies `component.json` (`{"id", "world":
"agentos:analyzer/analyzer@1.0.0"}`) and `component.wasm` into the read-only,
content-addressed `<home>/registry/components/<id>@<digest>/`, reusing the profile and image
registry code. Registration compiles the component and refuses one that imports anything
outside the `agentos:analyzer` interface or does not export `analyze`. The contract always
pins the digest. `submit` copies the entry into the task directory and records
`analyzer_id` and `analyzer_digest` in `Submitted`; the bytes are re-digested before every run.

## WIT world (`wit/agentos-analyzer-v1.wit`)

```wit
package agentos:analyzer@1.0.0;

interface snapshot {
  record entry { path: string, size: u64 }
  variant read-error {
    denied(string),
    not-found,
    invalid-path,
    too-large,
    budget-exhausted,
  }
  /// The task's recorded snapshot, bound to one task and one capability.
  resource tree {
    /// Every regular file, sorted by path.
    list: func() -> result<list<entry>, read-error>;
    /// At most `len` bytes of `path` from `offset`; `len` is at most 1 MiB.
    read: func(path: string, offset: u64, len: u32) -> result<list<u8>, read-error>;
  }
}

world analyzer {
  import snapshot;
  use snapshot.{tree};
  /// A JSON object of at most 64 KiB describing the snapshot.
  export analyze: func(tree: borrow<tree>) -> result<string, string>;
}
```

No WASI interface is linked. The linker defines exactly the `snapshot` interface; a component
importing anything else fails to instantiate, and registration refuses it earlier.

## Host side

A new crate, `agentos-component`, owns the runtime. It depends on Wasmtime 49.0.2 with default
features off and only `component-model`, `cranelift` and `runtime` enabled (`wat` for tests).

- **Binding.** The host value behind a `tree` records the task id and the capability. Every
  call checks that the store's task is the resource's task and that `snapshot.analyze` is
  still usable for it; otherwise `denied(reason)`.
- **Reads.** Paths go through the same validation as `ShadowReader` (`ReadFile`): relative,
  no `..`, no excluded component, no symlink on the path, a regular file inside the snapshot.
  That code is shared, not copied.
- **Budgets the fuel does not see.** Host calls are cheap for the guest, so they have their
  own bounds: at most 1 MiB per read, 16384 calls and 512 MiB read in total. Past a bound the
  call returns `budget-exhausted`.
- **Guest bounds.** Fuel (deterministic) bounds execution; exhaustion or any trap is a
  definite failure. A `StoreLimits` limiter, set before instantiation, bounds linear memory
  to 64 MiB, tables to 10000 elements and instances to 1. An epoch deadline of 60 s is a
  backstop only; its trip is reported as an infrastructure failure because it is not
  deterministic.
- **Report.** The returned string must be at most 64 KiB and parse as a JSON object;
  otherwise the effect fails with `analyzer report rejected: …`. An `Err(text)` from the
  component fails the effect with that text, escaped and bounded.

## Effect and recovery

`EffectKind::AnalyzeSnapshot` (tag `analyze_snapshot`, capability `SnapshotAnalyze`, retry
policy `Retry`). Its payload is the request identity, so replay is exact:

```json
{ "component_digest": "...", "snapshot_digest": "...", "runtime": "wasmtime 49.0.2",
  "fuel": 2000000000, "memory_bytes": 67108864, "report_limit": 65536,
  "read_calls": 16384, "read_bytes": 536870912 }
```

The routing executor sends it to a `ComponentExecutor` in the controller, which follows the
model executor's retention pattern: the outcome is written durably to
`<home>/analysis/<effect>-<attempt>/` before it is returned, and `retained_outcome` finds it
after a crash, so recovery publishes it without running the component again. GC treats
`analysis/` like `model/`: an entry is collected only when its effect is settled and is an
`AnalyzeSnapshot`. `follow_up_event` returns no task event for this kind, so a report can
never produce `VerifyPassed` or change a workspace.

The export manifest gains an optional `analysis` field (`{component_digest, report_digest,
file}` with the report at `analysis/report.json`), omitted when there is none, so earlier
manifests still parse.

## Reference analyzer

`components/repo-analyzer/` is a Rust crate built for `wasm32-unknown-unknown` with
wit-bindgen 0.62.0 and turned into a component with wasm-tools 1.261.0 (versions checked on
2026-10-08). It reports file and byte counts, counts by extension, and line counts of UTF-8
text files. The development image gains the wasm target and a pinned wasm-tools. The built
component is committed under `fixtures/components/repo-analyzer-v1/`, and a test rebuilds it
and compares bytes. A test lists its imports with wasm-tools and asserts they are exactly the
`snapshot` interface.

## Tests (red first)

| Test | Tier |
| --- | --- |
| Contract: `analyzer` without the capability, or the capability without `analyzer`, is rejected; an old contract keeps its digest | default |
| Registration refuses a non-component, a component importing a WASI interface, and one without `analyze`; accepts the reference | default |
| A granted read succeeds and the report is exported | default |
| No capability, a revoked capability (revoked between two reads), and a tree bound to another task: `denied` | default |
| An infinite loop runs out of fuel; memory growth past 64 MiB fails; both are definite failures | default |
| Oversize and malformed reports, and an `Err` from the component, fail the effect with their reasons | default |
| Read budgets: more than 1 MiB per call, too many calls, too many bytes give `budget-exhausted` | default |
| Path validation is the shared one: traversal, symlink, excluded component | default |
| Crash matrix: every crash point on `AnalyzeSnapshot`; a crash after retention publishes without re-running (an execution counter) | default |
| GC keeps an unsettled `analysis/` entry and collects a settled one | default |
| A report claiming `"passed": true` changes no verdict; `follow_up_event` is `None` | default |
| The reference component's imports are exactly the interface; rebuilding it gives the committed bytes | default |
| End to end through the CLI on the fake and the real Firecracker worker: approval shows the capability, the report is in the export | default, real KVM |

## Out of scope

Showing the report to the agent, more than one analyzer per task, components other than the
analyzer world, and WASI.
