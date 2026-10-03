#!/bin/sh
# The README demo: docker compose run --rm test sh scripts/demo.sh
# On the Firecracker worker (KVM tier: AGENTOS_FIRECRACKER and AGENTOS_GUEST_IMAGE set):
#   docker compose run --rm test-kvm sh scripts/demo.sh --worker firecracker
set -u
WORKER=host
while [ $# -gt 0 ]; do
  case "$1" in
    --worker) WORKER=${2:?--worker needs host or firecracker}; shift 2 ;;
    --worker=*) WORKER=${1#--worker=}; shift ;;
    *) echo "usage: scripts/demo.sh [--worker host|firecracker]" >&2; exit 2 ;;
  esac
done
case "$WORKER" in
  host) ;;
  firecracker)
    : "${AGENTOS_FIRECRACKER:?set AGENTOS_FIRECRACKER to the Firecracker binary}"
    : "${AGENTOS_GUEST_IMAGE:?set AGENTOS_GUEST_IMAGE to the built guest image directory}"
    # The jailer next to the binary unless AGENTOS_JAILER names another; the demo never
    # passes --allow-unjailed: on the KVM tier the task runs jailed.
    : "${AGENTOS_JAILER:=$(dirname "$AGENTOS_FIRECRACKER")/jailer}" ;;
  *) echo "unknown worker $WORKER (host or firecracker)" >&2; exit 2 ;;
esac
cargo build -q -p agentos-cli
BIN=/work/target/debug/agentos
rm -rf /tmp/demo && mkdir -p /tmp/demo && cd /tmp/demo
# Each path is passed as one argument (no word splitting).
agentos() {
  if [ "$WORKER" = firecracker ]; then
    "$BIN" --home /tmp/demo/home --worker firecracker --firecracker "$AGENTOS_FIRECRACKER" --jailer "$AGENTOS_JAILER" "$@"
  else
    "$BIN" --home /tmp/demo/home "$@"
  fi
}
run() { echo "\$ agentos $*"; agentos "$@"; code=$?; [ $code -eq 0 ] || echo "(exit code $code)"; echo; }
cat > task.json <<'JSON'
{
  "goal": "fix the parser",
  "repository": { "source": "/work/fixtures/parser-repo", "revision": "recorded-at-submission" },
  "profile": "python-stdlib-v1",
  "editable_paths": ["src/**"],
  "verification_profile": "parser-checks-v1",
  "capabilities": ["snapshot.read", "workspace.apply_patch", "verification.run", "artifact.export"],
  "limits": { "model_requests": 1, "max_output_tokens_per_request": 1000, "tool_actions": 10,
              "deadline_seconds": 600, "worker_vcpus": 1, "worker_memory_mib": 256 }
}
JSON
cp /work/fixtures/parser-repo.fix.patch fix.patch
run profile register /work/fixtures/profiles/parser-checks-v1
if [ "$WORKER" = firecracker ]; then
  run image register "$AGENTOS_GUEST_IMAGE"
fi
# The patch job is launched under its own supervisor, then the controller is killed (exit 75).
run submit task.json --yes --fake-agent-patch fix.patch --crash-at during-execute:apply_patch 2>&1
ID=$(ls /tmp/demo/home/tasks | grep -v '^\.' | head -1)
run status "$ID"
run resume "$ID"
run status "$ID"
echo "\$ agentos events $ID      # one JSON object per line; shown here as seq + type"
agentos events "$ID" | sed -E 's/^\{"payload":.*"seq":([0-9]+),"ts":[0-9]+,"type":"([A-Za-z]+)"\}$/\1 \2/'
echo
run export "$ID" bundle
echo "\$ ls bundle"; ls bundle
echo "\$ ls home/jobs      # one directory per effect attempt"; ls home/jobs | sed -E 's/^([0-9a-f]{8})[0-9a-f]{56}-([0-9a-f]{8})[0-9a-f-]{28}$/\1…-\2…/'
echo
run revoke "$ID" --capability artifact.export
run export "$ID" bundle-after-revoke
if [ "$WORKER" = firecracker ]; then
  # Jailed: ws.img belongs to the jail uid (61000) from the first ReadSnapshot on; ws.lock is
  # the attach lock. Every job's jail/ (chroot and cgroup marker) was collected after it settled.
  echo "\$ ls -ln home/work/$ID"; ls -ln "home/work/$ID"
  echo
  for j in home/jobs/*; do
    short=$(basename "$j" | sed -E 's/^([0-9a-f]{8})[0-9a-f]{56}-([0-9a-f]{8})[0-9a-f-]{28}$/\1…-\2…/')
    echo "\$ ls home/jobs/$short"; ls "$j" | tr '\n' ' '; echo
  done
fi
