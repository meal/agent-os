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
