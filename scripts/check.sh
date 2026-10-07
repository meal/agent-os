#!/bin/sh
# Required offline gates. CI selects one gate; local runs default to all five.
# `mount` runs the GC mount-root regressions in the `test-mount` service (CAP_SYS_ADMIN only).
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$REPO"
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-agentos-check}
check=${1:-all}
[ "$#" -le 1 ] || { echo "usage: scripts/check.sh [all|fmt|clippy|host|fake|mount]" >&2; exit 2; }
case "$check" in all|fmt|clippy|host|fake|mount) ;; *) echo "unknown check: $check" >&2; exit 2;; esac
if [ "$check" = all ] || [ "$check" = fmt ]; then docker compose run --rm test cargo fmt --all -- --check; fi
if [ "$check" = all ] || [ "$check" = clippy ]; then docker compose run --rm test cargo clippy --workspace --all-targets --locked -- -D warnings; fi
if [ "$check" = all ] || [ "$check" = host ]; then docker compose run --rm test cargo test --workspace --locked; fi
if [ "$check" = all ] || [ "$check" = fake ]; then docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test cargo test --workspace --locked; fi
if [ "$check" = all ] || [ "$check" = mount ]; then
    # A filter that matches nothing passes, and without the variable the tests skip: pass it
    # explicitly and require exactly the four mount regressions.
    out=$(docker compose run --rm -e AGENTOS_GC_MOUNT_TESTS=1 test-mount cargo test -p agentos-engine --test gc --locked mount_gate_ 2>&1) || { printf '%s\n' "$out"; exit 1; }
    printf '%s\n' "$out"
    printf '%s\n' "$out" | grep -q 'test result: ok\. 4 passed; 0 failed' || { echo "mount gate: expected exactly 4 mount_gate_ tests to pass" >&2; exit 1; }
fi
