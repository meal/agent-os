# Live agent session, attempt 2: first call answered, second rejected (two requests)

Run by the owner with `sh scripts/acceptance.sh live-agent <key file>` at commit `0ebb90f`
(attempt 1's fix included), model `claude-haiku-5-5`, cap 12.

- **Call 1 succeeded**: 200, usage 4 input / 165 output tokens. The response held a `thinking`
  block (`thinking: ""`, a 1000-character `signature`, the CLI asked for `display: omitted`) and a
  `Bash` `tool_use`. The live API therefore accepts the normalized first request: the
  top-level and per-message beta fields were the only problems of attempt 1.
- **Call 2 rejected**: 400 `messages.1.content.0.thinking: each thinking block must contain
  thinking`. The request the CLI sent back held the thinking block with an **empty signature**.
- Cause: `sse_from_message` replayed a thinking block as a bare `content_block_start`. A real
  stream starts the block empty and delivers the text and signature as `thinking_delta` and
  `signature_delta`; the client rebuilds the block from them. Without the signature delta the
  CLI lost the signature and the API refused the block.
- Fix (`agentos-core::messages`, guest side: the image is rebuilt by the next run): a thinking
  block is replayed as start (empty) + `thinking_delta` (if any text) + `signature_delta` (if
  any signature); unit-tested.
- Checked offline before spending again: the real recorded response of call 1 was replayed
  through the fixed codec to the real CLI against `scripts/cli-scripted-stub.py`; the CLI's next
  request carried the assistant thinking block with `signature_len` 1000 (it was 0 before).

Cost: two sends, 169 tokens in total.
