#!/bin/sh
# A failed build must stop the demo; a successful build must precede it.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/bin"
cat > "$scratch/bin/cargo" <<'MOCK'
#!/bin/sh
printf 'build\n' >> "$AGENTOS_PAGES_TEST_LOG"
exit "$AGENTOS_PAGES_BUILD_RESULT"
MOCK
cat > "$scratch/bin/pyenv" <<'MOCK'
#!/bin/sh
printf 'demo\n' >> "$AGENTOS_PAGES_TEST_LOG"
exit "$AGENTOS_PAGES_DEMO_RESULT"
MOCK
chmod +x "$scratch/bin/cargo" "$scratch/bin/pyenv"
export PATH="$scratch/bin:$PATH"
export AGENTOS_PAGES_TEST_LOG="$scratch/commands"
export AGENTOS_PAGES_BUILD_RESULT=42 AGENTOS_PAGES_DEMO_RESULT=0
if sh "$REPO/scripts/check-pages.sh"; then exit 1; else [ "$?" -eq 42 ]; fi
[ "$(cat "$scratch/commands")" = build ]
: > "$scratch/commands"
AGENTOS_PAGES_BUILD_RESULT=0
AGENTOS_PAGES_DEMO_RESULT=43
export AGENTOS_PAGES_BUILD_RESULT AGENTOS_PAGES_DEMO_RESULT
if sh "$REPO/scripts/check-pages.sh"; then exit 1; else [ "$?" -eq 43 ]; fi
[ "$(cat "$scratch/commands")" = "$(printf 'build\ndemo')" ]
AGENTOS_PAGES_DEMO_RESULT=0; export AGENTOS_PAGES_DEMO_RESULT
sh "$REPO/scripts/check-pages.sh"
echo "Pages build order and failure propagation passed"
