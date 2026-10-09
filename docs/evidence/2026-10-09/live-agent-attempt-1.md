# Live agent session, attempt 1: rejected by the API (one request)

Command (run by the owner): `sh scripts/acceptance.sh live-agent <key file>`, commit
`9105e7ee3771112a276672efebf2ae4f99e5d4b0`, image `agent-cli-py314-v1`, model
`claude-haiku-5-5`, cap 12 requests. The harness tests of the same binary passed (3 of 3), the
live test failed, as the harness is meant to when the API refuses a body.

- The real CLI started and made its first call; the controller forwarded it as one journaled
  `ModelCall` (normalized: `stream` false, `max_tokens` clamped, `model` rewritten, the three
  top-level beta fields removed).
- The API answered **400** `invalid_request_error`:
  `messages.1.output_config: Extra inputs are not permitted`. One send, usage 0 tokens, task
  Failed, session ended cleanly. The summary is `live-agent-attempt-1.summary.json` (the key is
  scanned out of it by the test; it holds the request id of the rejection).
- Cause: the request's `messages[1]` is a **`system`-role message with its own `output_config`**,
  the CLI's mid-conversation system message (a beta feature, `mid-conversation-system-2026-04-07`
  and `per-turn-control-2026-07-01` in its `anthropic-beta` list). Stripping only the top-level
  fields was not enough, as the traffic findings had warned.
- Fix (host side, `agentos-core::messages`, image unchanged): each message is reduced to `role`
  and `content`, and a `system` message's blocks are merged into the `user` message before it, or
  the one after it, after that message's own blocks (a `tool_result` is never separated from its
  `tool_use` and stays first). Unit-tested.

This attempt does not show that the live API accepts the rest of the body. See attempt 2.
