# Agent CLI sessions from the `agentos` CLI — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (or
> superpowers:executing-plans). Steps use checkbox (`- [ ]`) syntax.

**Goal:** `agentos submit --agent-cli claude-code --model anthropic:<model> task.json --yes` runs
the task's agent as a Claude Code session inside the microVM, as designed in
[External agent CLI inside the guest](../specs/2026-10-09-guest-agent-runner-design.md) and built
by the [guest agent runner plan](2026-10-09-guest-agent-runner.md) (released as v0.2.0-rc2 without
a CLI entry point). Today only tests start a session.

**Constraints:** red/green per step, with the failing output shown; `sh scripts/check.sh` per
commit; **no Co-Authored-By trailer**; subagents stop or kill only containers named
`agent-os-test-run-*`; never read or print `~/.anthropic-key`; no billed run without the owner;
never ship the Claude Code binary in a release (Anthropic's, all rights reserved: the user builds
`guest/agent-cli-py314-v1`); long gate chains run as the foreground command of a
`run_in_background` call, with logs under the ignored `build/` directory.

## Decisions

1. **Flag.** `submit --agent-cli NAME`, `NAME` = `claude-code` (the only preset). It needs
   `--model anthropic:<model>` or `--model fake:<transcript>` (the model the controller rewrites
   every request to, and the provider); it refuses `--fake-agent-patch`.
2. **Record.** `Submitted` gains `"agent_cli": "claude-code"` (absent for other tasks).
   `drive::agent_for` rebuilds the agent on `submit --yes`, `resume` and recovery from that record
   (re-validated like the model name is), so a resumed task gets a fresh `SessionAgent`; the
   engine already fails an open session as "agent session lost".
3. **Preset argv** lives in `agentos-engine::agent` (`claude_code_argv(goal)`):
   `/opt/agent-cli/claude -p <goal> --permission-mode bypassPermissions --disallowedTools
   WebFetch WebSearch`, env empty (the guest sets `PATH`, `HOME`, the proxy URL, the placeholder
   key). A goal that starts with `-` or is over 100,000 bytes (one argv entry is limited to
   128 KiB) is a usage error, never passed on.
4. **Refusals, before anything is written:** host worker (`--worker firecracker` required: a
   session runs only in a microVM); contract without `agent.session` or without `model.request`
   (the message says what to add to `capabilities`); `--agent-cli` with `--fake-agent-patch`;
   unknown agent name. The owner's approval screen shows `agent: claude-code session in the
   microVM, model <m>` and a line that the guest image must carry `/opt/agent-cli/claude`
   (build `guest/agent-cli-py314-v1`); a task that finds no CLI fails with the guest's reason.
5. **Test hook.** `AGENTOS_TEST_AGENT_ARGV` (a JSON array) replaces the preset argv, honoured only
   with `AGENTOS_TEST_WORKERS=1` like the other test switches, so the fake-guest tier can run a
   scripted shell "CLI".
6. **Tiers.** All tasks are testable on the fake guest (`AGENTOS_TEST_WORKER=firecracker-fake`);
   the real CLI on KVM and a live run stay with the owner (billed).

## Review Focus

- A goal starting with `-`, an oversized goal, or one with shell metacharacters reaches argv as
  one inert entry (Task 1).
- `--agent-cli` on the host worker, without `agent.session`, with `--fake-agent-patch` or with
  an unknown name fails with exit 2 and leaves no task behind (Task 2).
- A task submitted `--agent-cli` and killed mid-session fails "agent session lost" on `resume`,
  with the job reaped and no second model call sent (Task 2).
- `status` and `events` show which agent ran; the secret scan of an exported bundle still passes
  (Task 2).
- The test hook does nothing without `AGENTOS_TEST_WORKERS=1` (Task 1).

---

- [x] **1. Preset, parsing and recording** (79305d2). `claude_code_argv` with the goal rules; `AgentCli`
  enum (`FromStr`, `recorded()`); `--agent-cli` on `submit`; `Driver::Session`; `agent_for`
  builds it from the recorded `agent_cli` plus the recorded model; the test hook; unit tests for
  each rule above. No submit-flow changes yet.
- [x] **2. Submit flow, approval screen, end-to-end tests** (b3f08ad, 2bc5f5c). The refusals of Decision 4 in
  `validate`; `Submitted.agent_cli`; the approval line; `resume` and recovery through `agent_for`.
  CLI tests on the fake guest with a scripted shell CLI and the fake model provider: submit
  `--yes` ends SUCCEEDED with protected verification; a crash mid-session then `resume` fails
  "agent session lost"; each refusal; `status`/`events` show the agent.
- [x] **3. Documentation.** README section (build and register `agent-cli-py314-v1`, contract
  capabilities, the submit command, limits: no session resume, Claude Code only, the binary is
  yours to build), `docs/testing.md`, the `--help` text, and the design doc's status line.
