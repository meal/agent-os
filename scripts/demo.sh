#!/bin/sh
# The README demo: docker compose run --rm test sh scripts/demo.sh
set -u
cargo build -q -p agentos-cli
BIN=/work/target/debug/agentos
rm -rf /tmp/demo && mkdir -p /tmp/demo && cd /tmp/demo
agentos() { "$BIN" --home /tmp/demo/home --profiles /work/fixtures/profiles "$@"; }
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
run submit task.json --yes --fake-agent-patch fix.patch --crash-at after-dispatch:apply_patch 2>&1
ID=$(ls /tmp/demo/home/tasks | grep -v '^\.' | head -1)
run status "$ID"
run resume "$ID"
run status "$ID"
echo "\$ agentos events $ID      # one JSON object per line; shown here as seq + type"
agentos events "$ID" | sed -E 's/^\{"payload":.*"seq":([0-9]+),"ts":[0-9]+,"type":"([A-Za-z]+)"\}$/\1 \2/'
echo
run export "$ID" bundle
echo "\$ ls bundle"; ls bundle
