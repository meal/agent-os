#!/bin/sh
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
mkdir "$t/bin"
cat > "$t/bin/docker" <<'MOCK'
#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_LOG"
exit "${MOCK_EXIT:-0}"
MOCK
chmod +x "$t/bin/docker"
export PATH="$t/bin:$PATH" MOCK_LOG="$t/docker.log" AGENTOS_ACCEPTANCE_OUTPUT="$t/evidence"
# Missing/invalid key and unknown tier must fail before invoking Docker.
for mode in unknown live; do
  if sh "$REPO/scripts/acceptance.sh" "$mode" >/dev/null 2>&1; then echo "accepted invalid setup" >&2; exit 1; else test "$?" -eq 2; fi
done
test ! -e "$MOCK_LOG"
printf 'fake-key\n' > "$t/key"
ln -s "$t/key" "$t/link"
mkfifo "$t/fifo"
for key in "$t/link" "$t/fifo" "$t"; do
  if sh "$REPO/scripts/acceptance.sh" live "$key" >/dev/null 2>&1; then exit 1; else test "$?" -eq 2; fi
done
test ! -e "$MOCK_LOG"
sh "$REPO/scripts/acceptance.sh" offline >/dev/null
rg -q 'test cargo test -p agentos-engine --locked --test live_model' "$MOCK_LOG"
test -f "$t/evidence/offline.log"
# A failed requested command must fail the script and leave its log for diagnosis.
if AGENTOS_ACCEPTANCE_OUTPUT="$t/failed-evidence" MOCK_EXIT=37 sh "$REPO/scripts/acceptance.sh" offline >/dev/null 2>&1; then exit 1; else test "$?" -eq 37; fi
test -f "$t/failed-evidence/offline.log"
# Evidence cannot be silently overwritten by a repeated explicit destination.
if sh "$REPO/scripts/acceptance.sh" offline >/dev/null 2>&1; then echo 'overwrote evidence' >&2; exit 1; fi
echo 'acceptance dispatch, preflight refusal, and failure propagation passed'
