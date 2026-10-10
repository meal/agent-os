# External agent CLI inside the guest — design

Status: implemented on branch `acceptance-evidence` (plan
[2026-10-09-guest-agent-runner](../plans/2026-10-09-guest-agent-runner.md), evidence in
`docs/evidence/2026-10-09/`). The open questions below were closed by the plan's Decisions: a
crash mid-session fails the task ("agent session lost", no resume); changes leave the guest as a
diff through the existing patch path; the provider stays non-streaming and the proxy replays SSE;
the host rewrites the model and strips beta-only fields. The live run needed two codec fixes
(per-message beta fields, thinking signatures) that the original text did not foresee. The
`agentos submit --agent-cli claude-code` entry point is described in the README and was added by
[the CLI plan](../plans/2026-10-10-cli-agent-session.md).

## Goal

Run an existing coding-agent CLI, such as Claude Code or Codex, as the task's agent inside the
Firecracker guest, instead of the controller's own five-tool agent. The guarantees v0.1 proves
must hold unchanged:

- the guest has no network interface;
- no host secret enters the guest, the provider key included;
- every model call is a journaled `ModelCall` effect authorized by `model.request` and
  counted in the usage ledger, settled or uncertain;
- only a protected verification of exactly the final workspace yields `SUCCEEDED`.

## Approach: provider traffic over vsock through the broker

The CLI talks HTTP to a model endpoint. In the guest, that endpoint is a small loopback proxy
in the guest agent (`127.0.0.1:<port>`), which forwards each request over the existing vsock
link to the controller. The controller turns each request into a `ModelCall` effect: it checks
the capability and the budget, adds the key on the host side, sends it once through the
provider adapter, retains the response, and returns it to the guest. The CLI is configured with
that base URL and a placeholder key that grants nothing.

Consequences:

- No NIC is added, and the key never leaves the controller.
- Recovery reuses model-call retention: a crash after a response was retained publishes it
  without a second send. An agent run is replayable offline from its recording.
- Tool use (file edits, shell) happens inside the guest under the existing uid separation,
  limits and jail. The workspace changes are captured as a patch, through the existing
  `ApplyPatch` path or a new "workspace from guest" effect, and still need protected
  verification.

Rejected: giving the guest a network. It removes the no-NIC guarantee and needs the key in
the guest, a security-model change only the owner can decide.

## Open questions for the focused plan

1. **One long session or one effect per call.** A CLI holds conversation state in memory. A
   controller crash mid-session loses that process. Options: replay the recorded calls into a
   new session (deterministic only if the CLI is), or end the attempt and let the agent restart
   from the workspace.
2. **How workspace changes leave the guest.** Either the CLI writes into a scratch copy and the
   guest returns a diff for `ApplyPatch`, or a new effect commits the guest workspace with the
   same expected-version and digest checks.
3. **Which CLIs and how they are pinned.** Each needs a guest image with the CLI and its
   runtime at a pinned version, and its base URL and key settable without a network login.
   Subscription logins (no API key) cannot work without a network and are out of scope.
4. **Streaming.** CLIs usually stream responses; v0.1 reads whole responses. The proxy can
   buffer a streamed exchange into one effect, at the cost of latency.
5. **Budgets.** CLIs issue many small calls; the contract's `model_requests` and token caps
   apply per call, and the deadline bounds the session.

## Tests to plan

A fake CLI in the guest (a script calling the loopback endpoint) proves the path offline: each
call becomes one journaled `ModelCall`, a denied capability stops the session, a crash after
retention does not send again, and no key or network reaches the guest (the existing
`secret-probe` and `net-probe` profiles run beside the CLI). Then one real CLI on the KVM tier,
and a live run gated like the current live test.
