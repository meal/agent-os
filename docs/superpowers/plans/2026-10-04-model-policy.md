# Versioned model reliability implementation plan

Execute with superpowers:executing-plans and test-driven-development.

Goal: new tasks stop on permanent failures, wait durably between transient calls,
bound requests before publication, and resume at their recorded endpoint. Old tasks
retain policy 0. No change to billing uncertainty or same-effect retry rules.

## Serialization and replay

Policy 0 Submitted fixture: `{"model":"anthropic:x"}`; missing policy/limit versions
mean 0 and missing endpoint means `https://api.anthropic.com`. The old observation
`{"ModelCallFailed":{"reason":"http 401: denied"}}` deserializes with failure metadata
absent. Turn 1 calls; turn 2 repeats the same request, as before.

Policy 1 fixture: `{"model":"anthropic:x","model_policy_version":1,
"model_limits_version":1,"model_endpoint":"http://127.0.0.1:1234"}`.
A typed 401 result stops after turn 1 with its authentication reason. A typed 429
result records ModelRetryScheduled before turn 2; after its not-before timestamp
turn 2 calls the same bytes under a new effect/reservation. A later success resets
the consecutive-failure backoff. Unknown policy versions are refused before driving.

Typed failure metadata is optional on ModelCallFailed, omitted when absent. It carries
Permanent/Transient and optional absolute provider retry-not-before timestamp. Retained
failure outputs include this metadata; legacy outputs remain readable. Transport loss
continues to produce ModelCallLost and uncertain usage.

## Steps

- [x] Add failing status-classification, request-boundary and Retry-After tests.
- [x] Implement policy helpers; keep legacy provider constructors usable and add a typed
  rejected variant for Retry-After without altering existing fake transcript format.
- [x] Add old/new observation serialization fixtures and typed executor-result tests.
- [x] Add store-owned retry event API; generic audits refuse ModelRetryScheduled. Validate
  task/effect ownership, failed ModelCall, policy 1, positive retry turn; idempotently
  return the existing schedule by failed effect. Commit schedule in one transaction.
- [x] Add policy-1 flow tests: permanent one-send; transient fresh effect; transport
  uncertain; deadline refusal; pause/cancel/revoke interrupt within 100ms polling.
- [x] Record schedule before retry AgentTurn. Reuse it after crashes; wait again before
  dispatch of an already-journaled retry. Never hold a write transaction while waiting.
  A pending schedule also survives a pause/new session until a subsequent model intent.
- [x] Add crashes after schedule and retry turn and before dispatch; assert original
  not-before timestamp and one fresh reservation. Preserve policy-0 flow/replay.
- [x] Add endpoint submit/status/export/resume tests, including environment mismatch and
  mismatch before key read. New submissions record validated endpoint and versions.
  Missing legacy endpoint permits only official endpoint; custom legacy tasks re-submit.
- [x] Enforce 8MiB actual serialized-request bytes before blob registration, AgentTurn,
  or reservation. Oversize fails with context-size reason; policy 0 retains old behavior.
- [x] Run focused provider/agent/flow/crash/CLI tests and host/fake-jail workspace gates.
  Update current limitations and commit focused changes without a co-author trailer.

Retry schedule: durable 2,4,8,16,32,60-second backoff with no jitter; provider integer
seconds or HTTP-date can only extend it. Ignore malformed headers. Saturate overflow.
Fail rather than wait/send if the schedule reaches the task deadline. Wait checks task
state, deadline and model.request authorization every 100ms with no journal spam.

Validation uses local fake HTTP/provider only. KVM and paid live acceptance stay open.

Execution status: implemented and verified offline in `codex/v01-completion`; see the
[reviewed milestone report](../../reviews/2026-10-04-offline-milestone-review.md).
The additional explicit pause/resume-before-retry-timestamp regression remains a
deferred review minor; pause stopping and crash/resume scheduling are tested separately.
