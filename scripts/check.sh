#!/bin/sh
# Required offline gates. CI selects one gate; local runs default to all four.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$REPO"
export COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-agentos-check}
check=${1:-all}
[ "$#" -le 1 ] || { echo "usage: scripts/check.sh [all|fmt|clippy|host|fake]" >&2; exit 2; }
case "$check" in all|fmt|clippy|host|fake) ;; *) echo "unknown check: $check" >&2; exit 2;; esac
if [ "$check" = all ] || [ "$check" = fmt ]; then docker compose run --rm test cargo fmt --all -- --check; fi
if [ "$check" = all ] || [ "$check" = clippy ]; then docker compose run --rm test cargo clippy --workspace --all-targets --locked -- -D warnings; fi
if [ "$check" = all ] || [ "$check" = host ]; then docker compose run --rm test cargo test --workspace --locked; fi
if [ "$check" = all ] || [ "$check" = fake ]; then docker compose run --rm -e AGENTOS_TEST_WORKER=firecracker-fake -e AGENTOS_TEST_JAIL=fake test cargo test --workspace --locked; fi
