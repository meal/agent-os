# Claude Code traffic against a stub endpoint — findings

Task 1 of the [guest agent runner plan](../plans/2026-10-09-guest-agent-runner.md). Measured on
2026-10-09 with `scripts/cli-traffic-stub.py` (answers every request `500`, forwards nothing,
needs no key).

- CLI: `@anthropic-ai/claude-code` 2.1.295 (`latest`; `stable` is 2.1.286). It ran under Node
  v26.3.0 here (`npm view node` says 26.11.1; the LTS line is 24.21.0). The guest image pins
  the CLI version and a Node LTS chosen in Task 9, after checking again.
- Invocation: `ANTHROPIC_BASE_URL=http://127.0.0.1:<port> ANTHROPIC_API_KEY=placeholder
  DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_TELEMETRY=1
  claude -p "<goal>" < /dev/null`, with a fresh `HOME`. No login was needed.
- **Only one path was called:** `POST /v1/messages?beta=true`. No `count_tokens`, no telemetry
  or event-logging path appeared with those variables set. The proxy still answers every other
  path locally (Task 4) because a future CLI version may add one.
- The path carries a query string (`?beta=true`); the proxy matches the path part only.
- **`stream: true`** on every request; `max_tokens` was **128000**; the model was
  `claude-opus-5-5`. So the SSE conversion (Task 2) and the `max_tokens` clamp are both needed
  on the first call, not edge cases.
- Body keys: `context_management, max_tokens, messages, metadata, model, output_config,
  safeguards, stream, system, thinking, tools`.
- **Beta headers:** `anthropic-beta: claude-code-20250219, interleaved-thinking-2025-05-14,
  thinking-token-count-2026-05-13, context-management-2025-06-27,
  prompt-caching-scope-2026-01-05, mid-conversation-system-2026-04-07,
  per-turn-control-2026-07-01, mid-conversation-tool-changes-2026-07-01, effort-2025-11-24,
  dangerous-tool-use-2026-09-03, afk-mode-2026-01-31`. The plan forwards only the body, so
  the provider adapter will not send these. **Open risk:** fields such as `context_management`
  and `output_config` may be rejected (400) without their beta header. Task 2's normalizer
  therefore removes `context_management`, `safeguards` and `output_config` from the forwarded
  body (a constant list), and the first billed run (Task 10) decides whether more must go or
  whether the adapter must send a fixed beta set. Not testable without a key.
- On a `500` the CLI retried the same request at least seven times within the 60 s window, with
  `x-stainless-retry-count` rising. Each retry reaches the controller as its own call and
  consumes one `model_requests` unit; the budget, not the CLI, ends such a storm.
- Headers the proxy drops: all of them (`x-api-key` is the placeholder).

## Scripted-model run (plan Task 4b)

`scripts/cli-scripted-stub.py` served two canned answers built by the real
`agentos_core::messages::sse_from_message` (`cargo run -p agentos-core --example dump_sse`):
a `text` + `Write` tool_use, then an `end_turn` text. Run as an unprivileged user with
`claude -p "create hello.txt" --permission-mode bypassPermissions --output-format json
< /dev/null`.

- **The SDK accepted the codec's SSE.** The tool ran (`hello.txt` was created with the scripted
  content), the CLI made exactly **two** `/v1/messages` calls and exited **0** with
  `stop_reason: end_turn`. No side call to a second model, no other path.
- Headless edit flag: `--permission-mode bypassPermissions` (from `claude --help`).
- Body values: `thinking: {"type":"adaptive","display":"omitted"}` (no `budget_tokens`, so the
  `max_tokens` clamp cannot violate a thinking budget; `normalize_request` needs no change),
  `output_config: {"effort":"medium"}`, `context_management: {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`.
  `normalize_request` strips the last two (no beta header is sent); whether the live API then
  accepts the rest remains Task 10's question.
- The CLI offers ~20 tools, including `WebFetch`, `WebSearch`, `Agent`, `Workflow`,
  `ScheduleWakeup`. In the guest the network ones fail (no NIC); the session handler passes
  `--disallowedTools` for them so the model is not offered tools that cannot work.
- Not yet shown: a `thinking` content block in a response (the CLI sent `display: omitted`, so
  the model is not expected to return one); a streamed `thinking` block is left to the live run.
