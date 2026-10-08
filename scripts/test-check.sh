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
case "$*" in *mount_gate_*) printf 'test result: ok. %s passed; 0 failed\n' "${AGENTOS_CHECK_TEST_MOUNT_PASSED:-4}";; esac
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
[ "$(wc -l < "$scratch/commands")" -eq 6 ]
grep -q 'test sh scripts/build-components.sh --check' "$scratch/commands"
grep -q 'AGENTOS_TEST_JAIL=fake' "$scratch/commands"
grep -q 'run --rm -e AGENTOS_GC_MOUNT_TESTS=1 test-mount cargo test -p agentos-engine --test gc --locked mount_gate_' "$scratch/commands"
: > "$scratch/commands"
sh "$REPO/scripts/check.sh" mount
[ "$(wc -l < "$scratch/commands")" -eq 1 ]
# A mount gate whose filter matched no test (or not all four) must fail.
for passed in 0 3 14; do
    if AGENTOS_CHECK_TEST_MOUNT_PASSED=$passed sh "$REPO/scripts/check.sh" mount > "$scratch/stdout" 2> "$scratch/stderr"; then
        echo "a mount gate with $passed tests passed" >&2; exit 1
    fi
    grep -q 'expected exactly 4' "$scratch/stderr"
done
: > "$scratch/commands"
sh "$REPO/scripts/check.sh" host
[ "$(wc -l < "$scratch/commands")" -eq 1 ]
grep -q 'cargo test --workspace --locked' "$scratch/commands"
if sh "$REPO/scripts/check.sh" unknown > "$scratch/stdout" 2> "$scratch/stderr"; then exit 1; else [ "$?" -eq 2 ]; fi
echo "check selection and failure propagation passed"
