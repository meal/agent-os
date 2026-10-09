#!/bin/sh
# Requested acceptance never treats missing infrastructure as a passing skip.
set -eu
umask 077
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$REPO"
usage() { echo 'usage: scripts/acceptance.sh offline|kvm|live|live-agent [KEY_FILE]' >&2; exit 2; }
[ "$#" -ge 1 ] || usage
mode=$1
case "$mode" in
  offline|kvm) [ "$#" -eq 1 ] || usage;;
  live|live-agent)
    [ "$#" -eq 2 ] || usage
    key=$2
    # Refuse FIFOs/symlinks before any read, network, image build or model invocation.
    [ -f "$key" ] && [ ! -L "$key" ] || { echo 'live key must be a regular, nonsymlink file' >&2; exit 2; }
    bytes=$(stat -c %s -- "$key")
    [ "$bytes" -gt 0 ] && [ "$bytes" -le 4096 ] || { echo 'live key must contain 1..4096 bytes' >&2; exit 2; }
    key=$(realpath -- "$key");;
  *) usage;;
esac
if [ "$mode" != offline ]; then
  [ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo 'requested acceptance requires usable /dev/kvm' >&2; exit 2; }
fi
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-agentos-acceptance}
# The agent session runs the Claude Code image; every other mode keeps the parser image.
default_image=python-stdlib-v1
[ "$mode" != live-agent ] || default_image=agent-cli-py314-v1
image=${AGENTOS_ACCEPTANCE_IMAGE:-$default_image}
case "$image" in python-stdlib-v1|python-stdlib-py314-v1|python-stdlib-py314-v2|agent-cli-py314-v1) ;; *) echo 'unsupported acceptance image' >&2; exit 2;; esac
out=${AGENTOS_ACCEPTANCE_OUTPUT:-build/evidence/$(date -u +%Y%m%dT%H%M%SZ)-$$}
mkdir -p "$(dirname "$out")"
mkdir "$out" # Refuse existing destinations; preserve every attempt.
out=$(realpath "$out")
commit=$(git rev-parse HEAD)
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then commit=$commit-dirty; fi
printf '{"schema_version":1,"tier":"%s","commit":"%s","image":"%s"}\n' "$mode" "$commit" "$image" > "$out/setup.json"
# Keep each failed attempt log. Invoke commands directly: no pipeline masking exit status.
run() {
  log=$1
  shift
  if "$@" > "$out/$log.log" 2>&1; then
    echo "passed: $log ($out/$log.log)"
  else
    code=$?
    echo "failed: $log ($out/$log.log)" >&2
    exit "$code"
  fi
}
if [ "$mode" = offline ]; then
  run offline docker compose run --rm test cargo test -p agentos-engine --locked --test live_model -- --nocapture --test-threads 1
  exit 0
fi
run build docker compose build test test-kvm
run firecracker docker compose run --rm test-kvm sh scripts/fetch-firecracker.sh
if [ "$image" = python-stdlib-py314-v2 ] || [ "$image" = agent-cli-py314-v1 ]; then
  # Its kernel is built here from pinned source, twice, before the image is built (the same
  # kernel for agent-cli-py314-v1, which is py314-v2 plus the agent CLI).
  run kernel-builder docker compose build kernel-builder
  run kernel docker compose run --rm kernel-builder sh scripts/build-kernel.sh build/kernels/out --verify
fi
run image docker compose run --rm test-kvm sh scripts/build-guest-image.sh "guest/$image" "build/guest-images/$image" --verify
if [ "$mode" = live-agent ]; then
  # One billed run of the real CLI on the microVM. Its recording, bundle and summary land in
  # $out/agent; the key is a read-only mount, never a value.
  mkdir "$out/agent"
  run live-agent docker compose run --rm \
    -v "$key:/run/agentos/key:ro" -v "$out/agent:/evidence" \
    -e AGENTOS_API_KEY_FILE=/run/agentos/key -e AGENTOS_LIVE_MODEL_TESTS=1 \
    -e AGENTOS_LIVE_RECORDING_DIR=/evidence \
    -e "AGENTOS_EVIDENCE_COMMIT=$commit" -e "AGENTOS_GUEST_IMAGE=/work/build/guest-images/$image" \
    test-kvm cargo test -p agentos-engine --locked --test live_agent_session -- --nocapture --test-threads 1
  exit 0
fi
if [ "$mode" = kvm ]; then
  run kvm docker compose run --rm -e "AGENTOS_GUEST_IMAGE=/work/build/guest-images/$image" test-kvm cargo test --workspace --locked
  run real-worker docker compose run --rm -e "AGENTOS_GUEST_IMAGE=/work/build/guest-images/$image" -e AGENTOS_TEST_WORKER=firecracker test-kvm cargo test --workspace --locked
else
  # Only the key path is passed. Its content is mounted read-only, never an env/argv value.
  for worker in host firecracker; do
    mkdir "$out/$worker"
    service=test
    [ "$worker" = host ] || service=test-kvm
    run "live-$worker" docker compose run --rm \
      -v "$key:/run/agentos/key:ro" -v "$out/$worker:/evidence" \
      -e AGENTOS_API_KEY_FILE=/run/agentos/key -e AGENTOS_LIVE_MODEL_TESTS=1 \
      -e "AGENTOS_LIVE_WORKER=$worker" -e AGENTOS_LIVE_RECORDING_DIR=/evidence \
      -e "AGENTOS_EVIDENCE_COMMIT=$commit" -e "AGENTOS_GUEST_IMAGE=/work/build/guest-images/$image" \
      "$service" cargo test -p agentos-engine --locked --test live_model -- --nocapture --test-threads 1
  done
fi
