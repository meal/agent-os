# Workflows as chained contracts — design

Status: draft for review, 2026-10-12. Next roadmap item after the CLI agent session (tip af036f7).
Later items, out of scope here: a read-only cockpit, then fan-out.

## Purpose

A pipeline on one workspace. Step B starts from the verified output of step A (for example
implement, then review and fix, then verify). Each step is an ordinary task with its own contract,
capabilities and limits. The whole chain is approved once, up front, and then runs unattended.

## Decisions (agreed 2026-10-12)

1. **Shape:** a pipeline on one workspace, strictly sequential.
2. **Approval:** the whole chain once, up front. One screen lists every step and the summed budget.
3. **Handoff:** step N+1's input is a reference to step N's stored result, not a mutable host
   directory.
4. **Architecture:** a chain layer above tasks. Core, the effect journal and the task state machine
   are unchanged.

Rejected: one task with several phases (touches the core invariants and mixes budgets), and a
CLI-only script chaining `submit` calls (no durability, no single approval, no chain evidence).

## Non-goals (v1)

Retries, branching, DAGs, fan-out, manual gates, per-step agents, a chain-wide deadline, and live
runs of `--agent-cli` chains.

## Handoff

`submit` copies a source into `<home>/tasks/<id>/snapshot` and records its digest. A task's result
is its patch blobs plus `final_workspace_digest`. `gc` may delete a terminal task's workspace image
but keeps task inputs and linked blobs, so the handoff must be reconstructible from those.

When step N succeeds, the controller:

1. takes step N's `snapshot` and applies its accepted patch blobs;
2. checks that the result digests to N's verified `final_workspace_digest`;
3. stages it as step N+1's `snapshot`;
4. creates N+1 as a normal task whose `repository.revision` is that digest.

A digest mismatch fails the chain. No directory on disk is trusted. The workspace image that `gc`
may remove is never needed.

## Data model

- `workflows(id, owner, spec_json, spec_digest, state, current_step, created_ts)`
- `workflow_steps(workflow_id, idx, name, task_id NULL, state, PRIMARY KEY(workflow_id, idx))`

Tasks gain no column; the chain-to-task link lives in `workflow_steps`. The chain's state is derived
from its steps' task states: Running, Succeeded, Failed or Cancelled. A step that never ran after a
failure or cancel is Skipped.

## Approval and capabilities

`agentos workflow submit chain.json` shows one screen: each step's goal, agent, capabilities and
limits, plus the summed budget. Approval covers the chain's `spec_digest`. Tasks for later steps do
not exist yet, so the controller issues each step's capabilities when it creates that step's task,
only from the approved spec. The grants are the same as today and are bound to that one task, so a
step cannot use another step's handles.

## Chain file

```json
{ "name": "fix-and-review",
  "repository": { "source": "/work/repo", "revision": "recorded-at-submission" },
  "steps": [
    { "name": "implement", "contract": { "goal": "...", "profile": "...", "capabilities": [], "limits": {} } },
    { "name": "review",    "contract": { "goal": "..." } }
  ] }
```

- The top-level `repository` feeds step 1. Steps 2..n omit `repository`; setting it there is a
  validation error.
- Each step contract goes through `Contract::validate`.
- `--model`, `--agent-cli` and `--worker` apply chain-wide.
- 2 to 8 steps; step names are unique.

## Execution

The driver loop looks at each Running workflow's current step task:

- Succeeded: build the handoff and create the next task, or mark the chain Succeeded after the last
  step.
- Failed or Cancelled: mark the chain Failed or Cancelled, naming the step.
- Otherwise: keep driving that task.

Steps run one at a time under the single owner and the serialized workspace lock. Creating a step's
task and writing `workflow_steps.task_id` is one SQLite transaction.

## Resume

`agentos resume` recovers the current step's task as it does today, then continues the chain from
durable state. If the controller died after step N succeeded but before N+1 existed, resume creates
N+1. If N+1 exists, the `(workflow_id, idx)` key stops a second one. No step is run twice or skipped.

## Cancel and failure

`agentos cancel <workflow>` cancels the running step's task and marks the rest Skipped; cancel always
wins, as for tasks. A deadline or budget failure in a step fails the chain with no retry. Each
step's own `deadline_seconds` is the only time bound.

## Output

`agentos export <workflow>` writes the last step's bundle plus `chain.json`, which lists every
step's task id, contract digest, patch digest and verified digest in order, so base to final can be
checked. Each step remains exportable on its own.

## Testing

All through `docker compose run --rm test ...`.

- Unit: chain validation, state derivation.
- Fake-guest end to end: 2-step success; step-1 failure stops the chain; handoff digest mismatch
  fails the chain; cancel mid-chain.
- Crash: kill the controller mid step 1, between steps, and mid step 2; each resumes with exactly one
  task per step.
- Capabilities: a step's handles cannot be used by another step's task.
- `gc` between steps: the handoff still works after the workspace image is collected.
