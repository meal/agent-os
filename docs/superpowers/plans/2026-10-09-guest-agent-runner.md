# External agent CLI inside the guest — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (or
> superpowers:executing-plans). Steps use checkbox (`- [ ]`) syntax.

**Goal:** Run an existing coding-agent CLI as the task's agent inside the Firecracker guest with
the v0.1 guarantees intact, as designed in
[External agent CLI inside the guest](../specs/2026-10-09-guest-agent-runner-design.md).

**Constraints:** red/green per step (state it in every subagent brief; show the failing output);
the controller reads each diff against the brief before ticking a task; `sh scripts/check.sh` after each commit; **no Co-Authored-By
trailer on any commit** (user CLAUDE.md overrides the harness attribution; say so in every
subagent prompt); check library and CLI versions online when they are first pinned, never from
memory; run `df -h /` before any compose/cargo work and delete only our `*_target` volumes; do
not edit `crates/` or `guest/` while an `agentos-acceptance` run is in flight; never read or
print `~/.anthropic-key`; do not tag or publish a release.

## What the code already fixes (read before editing)

- **One microVM per effect attempt** (`firecracker.rs` module doc, `run_vm`): booted, one
  request, reply, `Shutdown`. The controller never holds a guest connection; it reaches a worker
  only through the job directory (`job.rs`), and the supervisor kills everything at lease end.
  The workspace persists on `ws.img`. So a session is a **new, long job**, not an executor
  call on a live VM.
- The guest is passive: `Session::serve` (`agentos-guest/src/agent.rs`) answers one host request
  at a time under `REQUEST`. A session request holds that lock for its whole length, which is
  fine because that VM serves nothing else (`ws.lock` keeps a second VM off the image).
- The host runner drives an `Agent` (`agent.rs`): an action becomes a journaled turn, then
  `act()` performs it. `call_model(cx, since, turn, base, request, body)` (`runner.rs`) already
  does capability check, budget, size check, intent, retention and reconcile. **Reuse it for every
  guest model request; do not write a second model path.**
- Replay (`drive`) re-feeds journaled observations into a fresh agent and fails with
  `NondeterministicAgent` on divergence. A CLI process cannot be replayed.

## Decisions (they close the design's open questions)

1. **Control flow.** A new `AgentAction::RunSession` (journaled like any turn) makes the runner
   start one *session job* (`EffectKind::RunAgentSession`, retry policy fail-no-retry,
   capability `agent.session`) without waiting for its receipt, then serve its mailbox:
   `<job>/session/<n>.req` (written by the worker) -> `call_model` -> `<n>.resp`. The session
   ends on the job's receipt. The model call turns are numbered after the `RunSession` turn.
2. **Crash (Q1).** No session resume. On recovery, an open `RunSession` turn reconciles
   outstanding `ModelCall` effects as today (a retained response is published, never resent),
   kills the job, and fails the task `agent session lost`. The only retry is a new task.
3. **Changes leave the guest (Q2).** The CLI runs in a scratch copy of the workspace, never the
   mounted `ws.img`. At exit the guest returns `git diff --binary --no-index`-style output as a
   patch in one raw frame; the runner feeds it to the **existing** `apply_patch` path
   (`patch_denial`: `PathNotEditable`, `DigestExcludedPath`; expected-base check), then the
   existing `verify`. No new workspace effect; protected verification is untouched.
4. **Streaming (Q4).** The provider stays non-streaming. The host rewrites each body
   (`stream:false`, `max_tokens` clamped to `limits.max_output_tokens_per_request`) and journals
   the body it sends. The guest proxy answers a client that asked for `stream:true` with SSE
   built from the whole JSON response (pure code, unit-tested).
5. **Only the body crosses.** Guest headers are dropped; the host adds the key. The placeholder
   key the CLI holds is meaningless. The proxy forwards only `POST /v1/messages`; every other
   path is answered locally (statuses taken from Task 1) and never hangs the CLI.
6. **Budgets and lease (Q5).** One `model_requests` unit per forwarded call. A session job has
   its own lease class: `EffectTimeouts` gains `session` (default = the remaining task deadline,
   never above a new `MAX_SESSION_TIMEOUT_MS`), and `supervised.rs`'s `MAX_EFFECT_TIMEOUT_MS`
   (600 s, which would kill any session over 10 minutes) applies to every other kind only.
   Tests update with it (Task 6). The mailbox loop runs all of `drive()`'s per-turn guards for
   every request (`interrupted`, `deadline_stop`, `model_failure_policy`, `turn_limit`,
   `check_request_size`, `pending_model_retry`), serves `.req` files strictly in `n` order, one
   at a time, and the guest `Bridge` serializes concurrent proxy connections.
6a. **Journal shape.** In-session model calls are journaled as `SessionModelCall` events, which
   `session_turns` skips (it only reads `AgentTurn`); only `RunSession`, `ApplyPatch`, `Verify`
   are ordinary agent turns. So a crash after the session ended resumes at the journaled
   `ApplyPatch`/`Verify` with no replay divergence and no second send (test in Task 8).
6b. **Model.** The host rewrites `model` in every forwarded body to the model the controller is
   configured with (the CLI asks for its own default, `claude-opus-5-5` at 128k), so the budget
   bounds cost and not only request count. Done in `normalize_request`'s caller (Task 7) and
   tested; the original requested name is recorded in the audit event.
7. **Large bodies.** Bodies travel in raw frames (16 MiB limit), checked with
   `check_request_size` (8 MiB) on the host; the 1 MiB JSON frame limit is never used for them.
8. **Tiers.** Tasks 2-8 are testable on the fake guest (`spawn_fake` plus a scripted fake CLI),
   no KVM. Task 9 is the KVM tier, Task 10 the gated live run.

## Review Focus

Each line has its test in the named task.

- `stream:true` from the CLI gets a valid SSE event sequence back (Task 2, Task 4).
- A body with `max_tokens` above the contract cap is clamped and the clamped body is journaled
  (Task 2, Task 7).
- A request to `/v1/messages/count_tokens` or an unknown path; `POST /v1/messages?beta=true` (the query string) is the forwarded one returns at
  once and is not counted against the budget (Task 4).
- A CLI that never calls the model and never exits, or one that keeps calling past the budget,
  ends at the deadline/budget with the process killed (Task 5, Task 7).
- A diff that touches a non-editable or `.git` path fails the task cleanly with `Denied` audit
  and no workspace change (Task 7).
- A controller crash between a retained response and its delivery does not send twice (Task 8).
- No key, and no network interface, reaches the guest while the CLI runs (Task 9).

---

- [x] **1. Spike the real CLI's traffic (no key, no bill).** Look up the current Claude Code and
  Node versions online; record them. `scripts/cli-traffic-stub.py` (stdlib `http.server`) logs
  method, path, headers and body shape of every request and answers `401`-style errors without
  contacting anyone. Run the CLI with `ANTHROPIC_BASE_URL` pointing at it, a dummy
  `ANTHROPIC_API_KEY`, and the settings that disable auto-update and nonessential traffic.
  Write `docs/superpowers/specs/2026-10-09-guest-agent-runner-traffic.md`: paths called, whether
  `stream` is set, `anthropic-beta` headers, env vars needed, the status the CLI tolerates for
  non-messages paths, and the model names it requests. **Gate:** if the CLI cannot run without a
  login or a second host, stop and report; do not widen the plan. Commit stub and findings.

- [x] **2. Pure message codec (`agentos-core/src/messages.rs`).** `normalize_request(body, cap)
  -> Result<Vec<u8>, String>`: parses JSON, requires an object with `messages`, sets
  `stream:false`, lowers `max_tokens` to `cap` (missing => `cap`), removes `context_management`, `safeguards` and `output_config` (Task 1 findings: beta-only fields, and we send no beta header), keeps sorted-key canonical
  bytes (serde_json maps are sorted). `sse_from_message(response: &Value) -> Vec<u8>`:
  `message_start`, one `content_block_start`/`..._delta`/`..._stop` per block (text ->
  `text_delta`, tool_use -> `input_json_delta` with the input serialized once), `message_delta`
  with `stop_reason` and `usage.output_tokens`, `message_stop`. Red tests first: clamp, missing
  `max_tokens`, `stream:true` forced off, non-object body rejected, byte-stable output, SSE for
  a text block, a tool_use block and an empty `content`, each event `event:`/`data:` framed with
  a blank line. Both host and guest crates use it.

- [x] **3. Protocol v2** (490a8a8). **Follow-up, owned by Task 9:** the three `guest/*/image.json.in` still say `"protocol":1`, so a v2 controller refuses every existing image until they are bumped and the images rebuilt (and re-registered); the v0.1 rc5 images keep working only with an rc5 controller. `GUEST_PROTOCOL = 2`; add `RunAgent { argv, env, base_url, timeout_secs,
  expected_base }`, `ModelRequest { id }` (guest -> host, followed by one raw frame), `ModelReply
  { id, status }` (host -> guest, followed by one raw frame), `AgentDone { exit_code, signal,
  workspace_digest, patch_bytes }` (followed by one raw frame holding the patch, empty if none).
  Update the message round-trip test (count 17 -> 21), the `allowed()` table (Job mode adds
  `RunAgent`; Inspect unchanged), and `tests/guest_protocol.rs`. Bumping the protocol means every
  guest image is rebuilt: record that in the image manifests' `README.md` and keep the old image
  usable only with a v1 controller. Red test: a v1 `Hello` is `Refused` with the existing
  "unsupported protocol" text.

- [x] **4. Guest loopback proxy** (91deba6; `lo` bring-up deferred to Task 5, see there) (`agentos-guest/src/proxy.rs`).** Blocking `std::net`, no new
  dependency, bound to `127.0.0.1:0` inside the VM (`lo` is down today: `init.rs` documents that it configures nothing; see Task 5). `trait Bridge { fn forward(&self, body: Vec<u8>) -> Result<(u16, Vec<u8>),
  String>; }` (the guest session implements it with `ModelRequest`/`ModelReply`; tests use a
  fake). Handler: `POST /v1/messages` -> `Bridge`; if the request body had `stream:true`, wrap
  the reply with `sse_from_message` and `text/event-stream`; local answers for everything else
  per Task 1; request line and header limits, body limit `RAW_FRAME_LIMIT`, per-connection read
  timeout 30 s. Tests with real sockets: plain call, streaming call, count_tokens, unknown
  path, oversized body (413), slow client (timeout), upstream error status passed through.

- [x] **4b. Scripted-model run of the real CLI (blocks Task 5; no key, no KVM).** Extend
  `scripts/cli-traffic-stub.py` (or a sibling) to answer `POST /v1/messages` with canned SSE
  produced by the real `sse_from_message` (fixture dumped by a core unit test): turn 1 a
  `tool_use` that edits/creates a file in the work directory, turn 2 an `end_turn` text. Run
  `claude -p` as a non-root user with the headless-edit flags (take them from `claude --help`,
  not memory). Record in the traffic findings: SSE accepted (also with a `thinking` block); the
  flags that let edits happen without prompts (found: `--permission-mode bypassPermissions`, `--disallowedTools WebFetch WebSearch ...` for tools that cannot work offline; findings in the traffic file); exit code after `end_turn`; whether a side call
  goes to a second model; the actual values of `thinking`, `output_config`,
  `context_management`. If `thinking` is `{type:"enabled", budget_tokens:N}` with N at or above
  the clamped `max_tokens`, `normalize_request` must lower N below it or drop `thinking`; add
  that test to `messages.rs` first.

- [x] **5. Guest session handler** (acab37c; shared exclusion list is `agentos-core::workspace::is_excluded` plus a session-only `.pytest_cache`/`.claude` list; git dir outside the tree; `AgentDone.workspace_digest` is the scratch tree's after purge, the real workspace is re-digested and must equal `expected_base`). **Carried to Task 9 (not testable on the host):** the `lo` up/down ioctl, the VM chown handover, the trampoline exec'ing the CLI as uid 1001, git under split uids, and whether `CHECK_NPROC=256` is enough for Node. The `ModelReply` read has no timeout of its own: the host lease is the backstop.  First: `lo` stays down at boot (verification relies on every connect failing, and `ip` is not in the image). Bring it up only when `RunAgent` is handled, never at boot: enable rustix `net` + `ioctl` features (check the latest rustix online), SIOCSIFFLAGS with IFF_UP on `lo`; it needs CAP_NET_ADMIN so it is verified on the KVM tier (Task 9) with `net-probe` still failing for every non-loopback address and for loopback outside a session. Then: `handlers::run_agent`: copy the workspace to
  `<scratch>/agent/work` (fresh each time, `fresh_scratch`), `git init` + baseline commit there
  as the diff base (set `PYTHONDONTWRITEBYTECODE=1`, `HOME` and caches under scratch), start the proxy, run `argv` in a new process group as the unprivileged
  uid/gid the verification already uses (`run_in_group`), `HOME` and temp under scratch, env
  from the message (allow-list: `ANTHROPIC_BASE_URL` forced to the proxy, `ANTHROPIC_API_KEY`
  forced to the literal `placeholder`, anything else the message names), no inherited env.
  On exit or timeout kill the group, then `git add -A` and `git diff --cached HEAD` (no
  `--binary`: `patchrules` refuses binary patches, so a binary change makes the reply `Refused`
  with that reason) is the patch, so new files are included. Paths the host would reject are
  excluded before staging (`__pycache__`, `*.pyc`, `.pytest_cache`, `.claude`, `.git`: share the
  one component list from `agentos-core::patchrules`, the same one `has_excluded_component`
  uses). Bounded by `PATCH_LIMIT`; over it => `Refused`. Digest the scratch tree. The guest
  `Bridge` serializes concurrent proxy connections (one `ModelRequest` in flight). `Session::serve` dispatches
  `RunAgent` and relays `ModelRequest`s while the child runs. Fake-guest test
  (`tests/fake_session.rs`): a scripted shell "CLI" that curls the proxy twice and edits a file
  yields two `ModelRequest`s and a patch naming that file; a CLI that edits one file, creates a new one and writes a `.pyc`
  yields a patch with both source changes, no `.pyc`, and it applies through the existing
  `apply_patch`; a CLI that sleeps forever is killed
  at `timeout_secs` and the reply says so; `/proc` shows no leftover process.

- [ ] **6. Host link and worker.** Session lease class first (Decision 6): `EffectTimeouts.session`,
  `MAX_SESSION_TIMEOUT_MS`, and the supervised lease/fence tests for a session longer than
  600 s. Then: `GuestLink` gains `recv_request(until)` handling the
  guest-initiated frames. `FirecrackerWorker::run_vm` branches on `EffectKind::RunAgentSession`:
  `RunAgent`, then loop: `ModelRequest` -> write `session/<n>.req` atomically (temp + rename) ->
  poll for `<n>.resp` (50 ms, bounded by lease and cancel marker) -> `ModelReply`; `AgentDone`
  -> outcome (patch stored as the job output). The fake worker path implements the same loop so
  Tasks 7-8 run without KVM. `EffectKind::RunAgentSession` + `Capability::AgentSession`
  (`agent.session`, required by the contract's capability list) in `agentos-core`, with the
  contract-digest-unchanged test the analyzer capability had.

- [ ] **7. Runner session driver.** Journal inner calls as `SessionModelCall` (Decision 6a);
  rewrite `model` per Decision 6b. `AgentAction::RunSession` + `Observation::SessionEnded {
  exit_code, patch: Option<String> }`; `act()` handles it: broker check for `agent.session`,
  start the job (`SupervisedExecutor::start`, new non-waiting entry beside `run`), then loop
  `serve_mailbox`: for each `.req` -> `normalize_request` with the contract cap -> journal a
  turn (`CallModel`) -> `call_model` -> write `.resp` (200 + the retained response bytes, or the
  error status the failure class maps to); stop on receipt, deadline, budget or denial and
  cancel the job. `SessionAgent` (the `Agent` impl): `Start` -> `RunSession`; `SessionEnded`
  with a patch -> `ApplyPatch(patch)`; `PatchApplied` -> `Verify`; `Verification` -> `Finish`;
  anything else -> `Finish`. Tests with the fake CLI: success path ends `SUCCEEDED` with
  protected verification; over-budget stops at `model_requests`; denied `model.request`
  fails the task and sends nothing; clamp is visible in the journaled request; a patch on a
  non-editable and on a `.git` path is `Denied`, workspace unchanged; deadline kills the CLI.

- [ ] **8. Recovery and crash matrix.** Red test first: the session completes, then the
  controller crashes at AfterIntent of the `ApplyPatch`; resume applies the journaled patch and
  verifies with no divergence and no resend. In `recover.rs`/`drive`: an open `RunSession` turn does
  not replay; reconcile outstanding `ModelCall`s, kill the job, fail `agent session lost`
  (distinct from `agent replay diverged`). Extend `crash_matrix.rs` with the session kinds at
  every crash point, with the execution counter proving a retained response is never sent
  twice. GC rule: `session/` mailbox files are collected with the job directory.

- [ ] **9. Real CLI image and KVM tier.** New guest image `guest/claude-code-<ver>/` (hook,
  `packages.txt`, pinned Node and CLI from Task 1, locks), `scripts/build-guest-image.sh` entry,
  `kvm_tier.rs` test: the fake model provider drives the real CLI through the proxy to a
  verified fix; the existing `secret-probe` and `net-probe` profiles run in the same VM and
  prove no key and no NIC; `scripts/acceptance.sh` gains the case. Check disk first; image
  builds use the acceptance compose project.

- [ ] **10. Live run, docs, evidence.** A `live_agent_session.rs` test gated exactly like
  `live_model.rs` (key from the environment, billed once, `-- --ignored`); recorded transcript
  under `docs/evidence/`; README/`docs/testing.md` sections; roadmap status updated; the two
  open caveats written down (no session resume; subscription logins unsupported). Leave
  branch, tag and release to the user.
