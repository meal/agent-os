# Phase 4: real agent workflow — design

Date: 2026-10-03
Status: draft for owner review. Design approved in conversation (2026-10-03) with the recommended defaults for the open questions; this written spec still needs review before a plan is written.
Parent spec: `Agent_OS_v1_Build_Plan.md` (Phase 4, "Real agent workflow").
Builds on: Phases 1-3b-1 (merged to `main`).

## Purpose and scope

A real language model drives the existing agent seam and fixes the fixture repository through
the broker, inside the contract's limits. Verification evidence is collected by the trusted
profile, never by the model. A crash at any point neither replays a completed model call nor
double-spends budget. The deterministic `FakeAgent` stays as the offline path.

Exit condition (build plan): a real model fixes the fixture through the broker and produces
independently collected test evidence.

Non-goals: Wasm components (Phase 5); a second real provider (the trait exists, only Anthropic
is implemented); streaming responses; parallel tool calls; prompt caching tuning; dollar-cost
accounting beyond recording reported token usage; natural-language contract generation;
network access from the guest (the model call is host-side only).

Assumptions: one local owner, one active controller (`driver.lock`), the fixture Python
profile, x86-64 Linux. Builds and tests run through `docker compose`. Default `cargo test`
makes no network calls.

## Decisions (owner-approved)

1. Provider-agnostic `ModelProvider` trait; Anthropic Messages API first, plus a deterministic fake.
2. Tools: `list_files`, `read_file`, `apply_patch`, `run_verification`, `finish`. Each is a
   broker-authorized, journaled effect. Reads and patches count against `limits.tool_actions`;
   model calls count against `limits.model_requests`; requested output is capped by
   `limits.max_output_tokens_per_request`.
3. Reads see the **current** revision through a host-side shadow workspace, validated by digest.
4. Response content is carried inline in the journaled observation (bounded by the output cap).
5. Model calls require an explicit contract capability `model.request`.
6. HTTP client: `reqwest` (rustls, no default features), pinned to the latest stable at
   implementation time (0.13.5 on 2026-10-03). Owner to confirm the new dependency in review.

## Architecture

### How a model call is journaled and replayed

A model call is an effect, `EffectKind::ModelCall { model }`, using the existing pipeline
`intend -> dispatch -> execute -> store_result -> register_result -> complete` (`steps.rs`).

- The request body (the full Messages API JSON, canonical: sorted keys, no timestamps) is put
  in the blob store and registered as an unlinked artifact *before* the turn is journaled. Its
  digest is the effect's request digest. The turn carries only the digest.
- The raw response bytes are the result artifact (`artifact_type` `model-response`).
- Replay: `ModelAgent` rebuilds the identical request from the observations it has already
  seen, so the digest recomputes and `journal::intended(after, "ModelCall", Some(&digest))`
  finds the completed effect. No second HTTP call is made.
- Budget: `Reservation::for_kind(&kind, 1)` reserves one model request; `tool_actions_for`
  returns 0 for `ModelCall`, 1 for `ListFiles`/`ReadFile`. `check_model_budget` already counts
  Reserved + Settled + Uncertain.
- `EffectId::derive` hashes `model` and `path` as it hashes `expected_base`.

Rejected alternatives: a second transcript journal (duplicates the effects/usage/attempts
lifecycle and recovery); an agent-side response cache (no reservation, no audit, breaks
"replay consumes stored responses").

### Types (agentos-core)

- `effect.rs`: `EffectKind::{ModelCall{model}, ListFiles, ReadFile{path}}`, tags
  `model_call | list_files | read_file`. `capability()`: `ModelCall` -> `Capability::ModelRequest`;
  list/read -> `SnapshotRead`. `retry_policy()`: `ModelCall` -> new `RetryPolicy::ForfeitThenRetry`;
  list/read -> `Retry`.
- `contract.rs`: `Capability::ModelRequest` (contract name `model.request`).
  `broker::scope_for(ModelRequest) = Scope::Task`. `Limits` is unchanged.
- `ReadFile` path validation is a runner pre-check like `patch_denial`: relative, no `..`,
  present in the snapshot manifest. A failure is a journaled `Denied` audit row and a
  `FileReadRejected` observation.

### Model provider (new `crates/agentos-engine/src/model/`)

- `provider.rs`: `trait ModelProvider { async fn complete(&self, body: &[u8]) -> ProviderResult }`,
  `ProviderResult::{Response(bytes, Usage), Rejected{status, body}, Transport(String)}`.
  `ApiKey(String)` has a redacting `Debug`.
- `anthropic.rs`: `POST {base}/v1/messages` with `x-api-key`, `anthropic-version: 2023-06-01`,
  `content-type: application/json`. **Exactly one send per attempt; no client-level retries**
  (a retry under one reservation would be an unaccounted second billed request). `base_url` is
  overridable for tests. Default model `claude-opus-5-5`.
- `fake.rs`: `FakeProvider` scripted from `fixtures/transcripts/<name>.json`, a list of
  `{expect_request_digest?, response}`. A `Recording` mode writes live responses in the same
  format for later offline replay; keys are never written.
- `executor.rs`: `ModelExecutor` implements `Executor`: `run` -> `ExecOutcome::success` (raw
  response), `Rejected` -> settled failure, `Transport` -> unresolved; it durably retains the
  response under `<home>/model/<effect_id>-<attempt_id>/response.json` (atomic write) before
  returning, so `retained_outcome` works like the supervisor's `receipt.json`;
  `await_job` = `Dead`, `reconcile` = `Unknown`.
- `RoutingExecutor { jobs, model, reads }` dispatches by `EffectKind`; `Cx` keeps one `exec`,
  so `steps.rs` and `recover.rs` stay generic.

### Agent seam (`agent.rs`)

`Agent::next` stays sync and deterministic. Additions:

- `AgentAction::{CallModel{request: Digest, body: Vec<u8>}, ListFiles, ReadFile(String)}`.
  `describe()` prints the digest; replay comparison uses the digest only.
- `Observation::{ModelResponse{content: Value, stop_reason, output_tokens}, ModelCallFailed{reason},
  ModelCallLost, Files{files}, FileRead{path, content, truncated}, FileReadRejected{reason}}`.
- `ModelAgent { contract, model, transcript, pending_tool_use_id }`:
  - `Start` -> system prompt + goal + file list -> `CallModel`.
  - `ModelResponse` -> append the entire `content` array to the history unchanged (thinking
    blocks must be echoed back); map the single `tool_use` to `ListFiles | ReadFile |
    ApplyPatch | Verify | Finish`. Requests use `tool_choice: {type: "auto",
    disable_parallel_tool_use: true}`, `strict: true` tools, and
    `max_tokens = limits.max_output_tokens_per_request`. (Forced `any` is rejected with 400 by
    current models.)
  - Tool observations -> `tool_result` (`is_error` for rejections, `PatchRejected`,
    `VersionConflict`, failed verification) -> `CallModel`.
  - `stop_reason` of `end_turn | max_tokens | refusal` with no tool -> `Finish`.
  - `BudgetExhausted | ModelCallFailed | ModelCallLost` -> request again only if the budget
    check passes, else `Finish`.
- `FakeAgent` is unchanged; every existing test keeps passing.

### Runner (`runner.rs`) and journal

`act()` gains `call_model`, `list_files` and `read_file`. `journal::effect_observation` handles
the three new kinds; `action_kind()` returns the new tags for the crash hook. `turn_limit`
already bounds the loop. The bounded repair loop is the existing rule: failed verification
returns to RUNNING within remaining limits; SUCCEEDED still requires verification evidence on
the final workspace revision.

### Uncertain model requests

Today an unresolved outcome or a receipt-less DISPATCHED effect ends in `mark_unreconcilable`
and a failed task. For `ModelCall`:

- New `Db::forfeit_effect(effect, reason)`: DISPATCHED/UNKNOWN -> FAILED, `usage.status =
  'Uncertain'` (never Released; still counted), journals a reserved `EffectForfeited` row.
- `recover.rs` maps `RetryPolicy::ForfeitThenRetry` to a new `Decision::Forfeit`; `run_attempt`
  routes an unresolved `ModelCall` there too.
- The agent then sees `ModelCallLost`. Its next `CallModel` is a **new effect with a new
  reservation**: the only honest accounting for "may have been billed".
- A definite HTTP error (4xx/429/5xx received) is a settled failure and counts.
- Lease generation for `ModelCall` never exceeds 1.

### Shadow workspace for reads (`ShadowReader`)

`list_files` and `read_file` are served host-side from `<home>/tasks/<task>/shadow`: the
snapshot plus every COMPLETED `ApplyPatch` in journal order (`journal::journaled_patch`),
accepted only if `workspace_digest(shadow) == task.workspace_digest`; a mismatch yields
`FileReadRejected`. No worker job and no VM boot. Reads are capped at 64 KiB (`truncated: true`).

### Secrets

The key is read once by the CLI (`--api-key-file`, else `ANTHROPIC_API_KEY`) and removed with
`std::env::remove_var` before any spawn. `SupervisedExecutor::launch` currently inherits the
controller environment; it gains `env_clear()`, an explicit `PATH` and `extra_env`, matching
`fixture.rs` and `guestlink.rs`. The key appears in no event, artifact, `Submitted` row,
export or log. A test has a `ScriptedWorker` dump `/proc/$PPID/environ` and `/proc/self/environ`
and asserts the key is absent.

### Contract and CLI

The contract only gains the capability name `model.request`; existing fixture contracts gain
one line. `submit --model anthropic:claude-opus-5-5 | fake:<transcript>` is recorded in
`Submitted.model` (today the constant `"fake-agent"`), and `drive()` builds `ModelAgent` or
`FakeAgent` from that record. `--fake-agent-patch` stays for the fixture agent.

## Testing

All commands run through `docker compose`.

- **Unit:** `ModelAgent` request determinism (same observations -> same bytes and digest); the
  tool-mapping table; `forfeit_effect` arithmetic (settled 1 + uncertain 1 = 2 committed; limit 2
  refuses a third); path validation; `ApiKey` redaction.
- **Scripted transcript (offline, default):** `fixtures/transcripts/parser-fix.json` drives
  list_files -> read_file -> apply_patch -> run_verification (fail) -> apply_patch ->
  run_verification (pass) -> finish. Asserts SUCCEEDED, `verified_digest == workspace_digest`,
  the exported bundle lists model request/response artifacts, settled model requests = 4.
- **Crash matrix:** add `model_call`, `read_file`, `list_files` to `KINDS`. AfterIntent ->
  `Dispatch` (provider calls = 1); AfterDispatch/DuringExecute without a retained response ->
  `Forfeit` then one more call (calls = 2, uncertain = 1); AfterExecuteBeforePublish ->
  `PublishRetained` (calls = 1); AfterBlobPut/AfterRegister/AfterComplete take the existing
  paths. Every row asserts no `NondeterministicAgent`.
- **Provider errors:** 400 -> settled failure + `ModelCallFailed`; transport timeout -> forfeit;
  `stop_reason: refusal` -> Finish -> FAILED "agent finished without verified success".
- **Secrets:** the environment-dump test above, on the host worker and (gated) the Firecracker worker.
- **Live (gated):** `AGENTOS_LIVE_MODEL_TESTS=1` plus a key, else it prints `SKIPPED:` like the
  KVM tier. One end-to-end fixture repair on the host worker.

## Risks

- Request bytes must be fully deterministic across replays; any nondeterminism surfaces as
  `NondeterministicAgent`. Mitigation: canonical JSON, a unit test, and the crash matrix.
- Journal growth: inline responses add about one response per turn (bounded). Request blobs
  are content-addressed, so history re-sent each turn dedupes only partially (O(turns^2)
  bytes on disk). Acceptable for `model_requests` around 12; revisit if limits grow.
- New dependency `reqwest` enlarges the build; kept to rustls with default features off.
- Live-model behaviour (refusals, malformed tool input) cannot be fully scripted; the gated
  live test and the recording mode are the check.

## Documentation

README: replace the "No real model" known limit; document `--model`, `--api-key-file`, the
`model.request` capability, uncertain-request accounting and the secrets handling. Add a demo
transcript from a recorded live run.
