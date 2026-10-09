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
# Match the portable host tools available on a clean runner. Do not inherit optional
# developer tools such as ripgrep from the caller's PATH.
for utility in sh dirname mktemp mkdir rm chmod ln mkfifo stat realpath git grep head; do
  ln -s "$(command -v "$utility")" "$t/bin/$utility"
done
export PATH="$t/bin" MOCK_LOG="$t/docker.log" AGENTOS_ACCEPTANCE_OUTPUT="$t/evidence"
# Missing/invalid key and unknown tier must fail before invoking Docker.
for mode in unknown live live-agent; do
  if sh "$REPO/scripts/acceptance.sh" "$mode" >/dev/null 2>&1; then echo "accepted invalid setup" >&2; exit 1; else test "$?" -eq 2; fi
done
test ! -e "$MOCK_LOG"
printf 'fake-key\n' > "$t/key"
ln -s "$t/key" "$t/link"
mkfifo "$t/fifo"
: > "$t/empty"
head -c 4097 /dev/zero > "$t/oversize"
for mode in live live-agent; do
  for key in "$t/link" "$t/fifo" "$t" "$t/empty" "$t/oversize"; do
    if sh "$REPO/scripts/acceptance.sh" "$mode" "$key" >/dev/null 2>&1; then exit 1; else test "$?" -eq 2; fi
  done
done
test ! -e "$MOCK_LOG"
# live-agent without a usable /dev/kvm is refused before any docker call; with one, it runs the
# agent session test once on the KVM service, with the key as a read-only mount and not as a value.
if [ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
  AGENTOS_ACCEPTANCE_OUTPUT="$t/agent-evidence" sh "$REPO/scripts/acceptance.sh" live-agent "$t/key" >/dev/null
  grep -Fq 'test-kvm cargo test -p agentos-engine --locked --test live_agent_session' "$MOCK_LOG"
  grep -Fq 'AGENTOS_LIVE_MODEL_TESTS=1' "$MOCK_LOG"
  grep -Fq 'AGENTOS_LIVE_RECORDING_DIR=/evidence' "$MOCK_LOG"
  grep -Fq 'guest/agent-cli-py314-v1' "$MOCK_LOG"
  grep -Fq 'AGENTOS_GUEST_IMAGE=/work/build/guest-images/agent-cli-py314-v1' "$MOCK_LOG"
  grep -Fq ':/run/agentos/key:ro' "$MOCK_LOG"
  if grep -Fq 'fake-key' "$MOCK_LOG"; then echo 'the key value reached docker' >&2; exit 1; fi
  test -f "$t/agent-evidence/live-agent.log"
  test -d "$t/agent-evidence/agent"
else
  if sh "$REPO/scripts/acceptance.sh" live-agent "$t/key" >/dev/null 2>&1; then exit 1; else test "$?" -eq 2; fi
  test ! -e "$MOCK_LOG"
fi
sh "$REPO/scripts/acceptance.sh" offline >/dev/null
grep -Fq 'test cargo test -p agentos-engine --locked --test live_model' "$MOCK_LOG"
test -f "$t/evidence/offline.log"
# A failed requested command must fail the script and leave its log for diagnosis.
if AGENTOS_ACCEPTANCE_OUTPUT="$t/failed-evidence" MOCK_EXIT=37 sh "$REPO/scripts/acceptance.sh" offline >/dev/null 2>&1; then exit 1; else test "$?" -eq 37; fi
test -f "$t/failed-evidence/offline.log"
# Evidence cannot be silently overwritten by a repeated explicit destination.
if sh "$REPO/scripts/acceptance.sh" offline >/dev/null 2>&1; then echo 'overwrote evidence' >&2; exit 1; fi
echo 'acceptance dispatch, preflight refusal, and failure propagation passed'
