# Live agent session, attempt 3: passed

Run by the owner with `sh scripts/acceptance.sh live-agent <key file>` at commit
`d90b9ba08c9ea6713b87f98b08979fc1766ee616` (the two codec fixes included), image
`agent-cli-py314-v1` (rebuilt reproducibly by the run), model `claude-haiku-5-5`, cap 12 requests.
Files: `live-agent-pass/` (summary, recording, log, setup, image log). The run's exported bundle
is not committed: its request files carry the agent CLI's full system prompt, whose security-policy
sentence contains the word "authorization" and trips the repository's secret scan (a false
positive; the key is absent). The recording and summary keep every request digest and the full
responses.

- The real Claude Code 2.1.295 binary ran in the Firecracker microVM, talked to the loopback
  proxy, and every model request went through the controller as a journaled, capability-checked
  `ModelCall` with the key added on the host. **5 model calls, all COMPLETED**, 20 input and 1640
  output tokens as the usage ledger counted them; no uncertain call; one session job.
- The CLI's edits came back as a patch, went through `ApplyPatch` and the protected
  verification: **task `Succeeded`, `VerifyPassed` once**, no problems reported by the harness
  (events: 3 agent turns, 5 `SessionModelCall`, 9 completed effects, no failure or denial).
- The harness checks passed: no `context_management`/`safeguards`/`output_config` and no
  `system`-role message in any forwarded body, `stream` false, `max_tokens` within the cap, the
  model name rewritten to `claude-haiku-5-5`, and the key absent from the recording, bundle,
  summary, blobs and database.
- Three attempts in total: attempt 1 (HTTP 400, per-message beta field), attempt 2 (HTTP 400,
  thinking block lost its signature), attempt 3 (this). Total spend: 7 sends.

Not shown: other CLIs (Codex); subscription logins (unsupported by design: no network in the
guest); session resume after a controller crash (not supported: the task fails "agent session
lost"); streaming is simulated, the provider call is non-streaming.
