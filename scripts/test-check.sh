#!/bin/sh
# Test check selection and failure propagation with an injected Docker command.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"
cat > "$scratch/bin/docker" <<'MOCK'
#!/bin/sh
printf '%s\n' "$*" >> "$AGENTOS_CHECK_TEST_LOG"
case "$*" in *"$AGENTOS_CHECK_TEST_FAILURE"*) [ -z "$AGENTOS_CHECK_TEST_FAILURE" ] || exit 42;; esac
MOCK
chmod +x "$scratch/bin/docker"
export AGENTOS_CHECK_TEST_LOG="$scratch/commands"
export AGENTOS_CHECK_TEST_FAILURE="cargo clippy"
export PATH="$scratch/bin:$PATH"
if sh "$REPO/scripts/check.sh" > "$scratch/stdout" 2> "$scratch/stderr"; then
    echo "injected Clippy failure did not fail checks" >&2; exit 1
else
    [ "$?" -eq 42 ]
fi
[ "$(wc -l < "$scratch/commands")" -eq 2 ]
! grep -q 'cargo test' "$scratch/commands"
AGENTOS_CHECK_TEST_FAILURE=; export AGENTOS_CHECK_TEST_FAILURE
: > "$scratch/commands"
sh "$REPO/scripts/check.sh"
[ "$(wc -l < "$scratch/commands")" -eq 5 ]
grep -q 'AGENTOS_TEST_JAIL=fake' "$scratch/commands"
grep -q 'run --rm test-mount cargo test -p agentos-engine --test gc --locked mount_gate_' "$scratch/commands"
: > "$scratch/commands"
sh "$REPO/scripts/check.sh" mount
[ "$(wc -l < "$scratch/commands")" -eq 1 ]
: > "$scratch/commands"
sh "$REPO/scripts/check.sh" host
[ "$(wc -l < "$scratch/commands")" -eq 1 ]
grep -q 'cargo test --workspace --locked' "$scratch/commands"
if sh "$REPO/scripts/check.sh" unknown > "$scratch/stdout" 2> "$scratch/stderr"; then exit 1; else [ "$?" -eq 2 ]; fi
echo "check selection and failure propagation passed"
