# Hands-on testing guide

Start with a trusted Python project and the offline provider. You can test repair,
approval, recovery and exported patches with Docker Compose, without an API key or
KVM. Live Anthropic and real Firecracker acceptance remain open gates; running the
offline tier does not close them.

## Get a test checkout ready

Use a Linux x86_64 machine with Git and Docker Engine plus the Compose plugin.
The checked-in image supplies Rust, Git and the pinned pyenv/Python runtime; you do
not need to install Python or Ruby on the host. Ruby is not used by this project.
Run these commands from the repository root. Use `main`, which carries the
published walkthrough and its runnable examples; the repository's default branch
may lag documentation updates.

```sh
git clone --branch main https://github.com/meal/agent-os.git
cd agent-os
docker compose version
docker compose build test
docker compose run --rm test sh scripts/check-pages.sh
```

The last command builds the CLI and runs the documentation examples. Expect three
tests and `OK`: link checks, offline model repair/export, and the approval-to-patch
handoff. A cold build can take several minutes; compilation happens before the
60-second example execution limits. The examples use disposable test directories.

## First hands-on run: a configuration bug

Use [the Pages usage example](https://meal.github.io/agent-os/#usage). It models a
service whose configuration reader mishandles `PORT = 8080` and indented comments.
Save its `model-task.json` contract in the checkout, then open a container:

```sh
docker compose run --rm test sh
```

Run the example's steps in order. It creates a new `build/usage.*` directory holding
the original service copy, task, private home, journal, export and review copy.
The scripted provider is specific to this parser; it cannot solve arbitrary bugs.

| Checkpoint | Expected result |
| --- | --- |
| Protected check on the original source | Nonzero exit, with failing parser cases. This is the bug you want fixed. |
| Submit without `--yes` | `approval.json` reports `READY`; the permission summary is on stderr. No task effects have run. |
| Review the contract | Only `src/**` is editable; `model.request` is explicit; the protected check and budgets are recorded. |
| Resume the approved task | `SUCCEEDED` after the scripted repair loop. |
| Export | A manifest, `patch.diff`, individual patches, model artifacts and verification evidence. |
| Review/apply to the disposable copy | `git apply --check` succeeds and the protected check reports `10/10 checks passed`. |
| Original source | Unchanged. Agent OS worked on its own snapshot/workspace. |

Inspect `bundle/manifest.json`: `state` must be `SUCCEEDED`,
`final_workspace_digest` must equal `verified_digest`, and a passing verification
result must have `accepted_for_final_workspace: true`. Inspect `patch.diff` and the
verification evidence as well. The status is a technical acceptance result; you
still decide whether the change is appropriate for your project.

## Move to your own project

Choose a small, reproducible bug in a Python project with standard-library tests.
The execution environment does not automatically install your dependencies, run
arbitrary shell commands supplied by the model, or provide a network to VM jobs.
Projects needing other runtimes or packages require a matching profile/image.

1. Commit the source revision you intend to test. Export its tracked files into
   a separate directory, keeping credentials, build products and virtualenvs out.
   Agent OS snapshots regular files, including untracked files, rather than
   consulting `.gitignore`; symlinks in the source are refused.
2. Write a regression check that fails on that source. Register it separately as
   a protected verification profile. The check receives the workspace path as
   its last argument and should return zero only when acceptance passes.
3. Write a narrow contract with a concrete goal, source path, editable paths,
   protected profile and budgets. `repository.revision` is
   `recorded-at-submission` or an Agent OS workspace digest, **not a Git SHA**.
4. Submit without `--yes`, inspect the printed permissions, then resume that task
   when you approve them. Resume uses the recorded model and endpoint.
5. Review the export and apply it only to the same clean base revision. Run the
   project's normal checks before committing the change.

For example, prepare a snapshot on the host (replace the path):

```sh
PROJECT_REPO=/absolute/path/to/your-service
PROJECT_SNAPSHOT=$(mktemp -d)
git -C "$PROJECT_REPO" archive HEAD | tar -x -C "$PROJECT_SNAPSHOT"
```

For the parser-shaped service in this example, the protected profile is:

```json
{ "id": "service-checks-v1", "command": ["python3", "check_service.py"], "protected": true }
```

Put `profile.json` and `check_service.py` under `build/service-checks-v1/` in the
Agent OS checkout. Adapt the included
[`check_parser.py`](../fixtures/profiles/parser-checks-v1/check_parser.py) to import
your project from the workspace argument and cover the bug plus existing behavior.
Run it on `PROJECT_SNAPSHOT` first and confirm it fails for the expected reason.
Checks run with a cleared environment, so use explicit paths and arguments.

## Optional: run a live model on that snapshot

This sends the goal and repository content included in model requests to the
selected provider and consumes your Anthropic API account's budget. Keep the key
in a regular, nonsymlink file outside the repository, with owner-only access; only
its absolute path is needed below. Select a model ID available to your account
from [Anthropic's current model list](https://platform.claude.com/docs/en/models/overview).
Compatibility and successful repair with a real provider still need acceptance.

On the host, set paths and mount them into the trusted test container:

```sh
KEY_FILE=/absolute/path/to/anthropic-key
docker compose run --rm \
  -v "$PROJECT_SNAPSHOT:/project:ro" \
  -v "$KEY_FILE:/run/agentos/key:ro" test sh
```

Inside that container, save the Pages model contract as `build/service-task.json`.
Change its goal to your bug, `repository.source` to `/project`,
`verification_profile` to `service-checks-v1`, and `editable_paths` to the exact
source directories you want the agent to modify. Keep the explicit capabilities
and request/action/deadline limits; those counts are limits, not a currency budget.
The example permits eight calls, each requesting up to 16000 output tokens, so
choose limits deliberately for your account. Then:

```sh
cargo build --locked -p agentos-cli
mkdir -p build
task_home=$(mktemp -d /work/build/live-task.XXXXXX)
agentos() {
  target/debug/agentos --home "$task_home" --worker host \
    --api-key-file /run/agentos/key "$@"
}
agentos profile register build/service-checks-v1
MODEL_ID=replace_with_an_available_model_id
agentos submit build/service-task.json --model "anthropic:$MODEL_ID" \
  > "$task_home/submission.json"
task_id=$(pyenv exec python -c \
  'import json,sys; print(json.load(open(sys.argv[1]))["task_id"])' \
  "$task_home/submission.json")
agentos status "$task_id"
```

Stop here to review the permissions, source digest, endpoint and budgets. When
you approve this task, run:

```sh
agentos resume "$task_id"
agentos status "$task_id"
agentos events "$task_id" > "$task_home/events.ndjson"
agentos export "$task_id" "$task_home/bundle"
```

Export can also describe failed tasks. Apply changes only after the success and
verification checks above pass. On the host, inspect the bundle under `build/`,
then use a clean checkout of the same committed revision:

```sh
# Replace BUNDLE with the absolute path to the reviewed build/live-task.*/bundle.
BUNDLE=/absolute/path/to/reviewed/bundle
git -C "$PROJECT_REPO" status --short
git -C "$PROJECT_REPO" apply --check "$BUNDLE/patch.diff"
git -C "$PROJECT_REPO" apply "$BUNDLE/patch.diff"
git -C "$PROJECT_REPO" diff
# Run your project's tests and review the result before committing.
```

## Test recovery and the offline gates

From the host at the repository root:

```sh
docker compose run --rm test sh scripts/demo.sh
COMPOSE_PROJECT_NAME=agentos-manual sh scripts/check.sh
sh scripts/acceptance.sh offline
```

The crash demo intentionally exits the controller at code 75, then resumes the
same task. Its export succeeds; its later revoked export deliberately fails.
The demo prints those outcomes and is a demonstration, not a fail-fast acceptance
test. `scripts/check.sh` runs all five required gates (formatting, Clippy, host tests, fake-jail
tests, and the GC mount gate in the `test-mount` service) and propagates failures.
Offline acceptance tests host and fake-jail repair/replay through a loopback fake
API, preserving a log under a new private `build/evidence/` directory.

For a manual task, `status` shows state and outstanding effects; `events` shows
the journal. Use `pause`, `resume`, or `cancel` with the recorded task ID. Resume
after a controller crash reconciles outstanding work. Avoid changing the source,
profile, model or provider endpoint and expecting the recorded task to adopt it.

## Later: real KVM and combined live acceptance

Use a Linux x86_64 Docker host with usable `/dev/kvm`, cgroup v2, and the jailer
setup in the [README KVM tier](../README.md#the-kvm-tier-firecracker). These commands
download pinned artifacts, build the guest twice to compare it, and retain logs:

```sh
sh scripts/acceptance.sh kvm
sh scripts/acceptance.sh live /absolute/path/to/anthropic-key
```

`live` requires KVM too: it covers both the host and jailed Firecracker worker,
with a bounded fixture repair and offline replay of the real recording. It does
not just run a host smoke test. Missing setup fails explicitly rather than passing
as a skipped gate. The Python 3.14 images are selected the same way; `python-stdlib-py314-v2`,
the release image, first builds its kernel from source twice:

```sh
AGENTOS_ACCEPTANCE_IMAGE=python-stdlib-py314-v2 sh scripts/acceptance.sh kvm
```

Both passed real KVM acceptance; the tests' default image stays `python-stdlib-v1`. See
[evidence requirements](evidence/README.md) before promoting recordings or logs.

## What to preserve if a test fails

Keep the commit, exact command, exit status, worker, task ID, status/events and the
matching `build/evidence/` directory. Keep unsuccessful attempt logs alongside
successful runs. A 401/403 points to credentials or access; 429/5xx trigger recorded
waits; reaching a request/action/deadline limit means the task did not fit its
contract. A protected-check failure means the proposed code did not pass acceptance.
For KVM refusal, read the preflight reason rather than switching to unjailed mode.
Keep API keys private and inspect model/code artifacts before sharing them.
