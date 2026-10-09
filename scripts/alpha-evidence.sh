#!/bin/sh
# The alpha release evidence: two distinct snapshots (the parser and duration fixtures), each
# run on the installed release in a fresh container on the jailed worker with the reference
# analyzer, once with the deterministic agent (killed during the patch and resumed) and once
# with the live model under the bounded contract (12 requests of at most 16000 output tokens,
# 12 tool actions, 900 seconds). A live run is recorded whatever its outcome and never
# retried. Outputs go to build/evidence/alpha-<time>-<pid>/; summarize them with
# scripts/alpha-summary.py.
#
#   sh scripts/alpha-evidence.sh TARBALL KEY_FILE
#
# The key file is mounted read-only and named by path only; its content never appears in an
# argument or environment variable. AGENTOS_ALPHA_SKIP_LIVE=1 skips the live runs (a dry run of
# everything else, which costs nothing).
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
[ "$#" -eq 2 ] || { echo 'usage: scripts/alpha-evidence.sh TARBALL KEY_FILE' >&2; exit 2; }
tarball=$(realpath "$1")
key=$2
[ -f "$tarball" ] && [ -f "$tarball.sha256" ] || { echo "$1 and $1.sha256 must exist" >&2; exit 2; }
[ -f "$key" ] && [ ! -L "$key" ] || { echo 'the key must be a regular, non-symlink file' >&2; exit 2; }
key=$(realpath "$key")
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo 'the evidence run needs a usable /dev/kvm' >&2; exit 2; }
image=debian@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
out="$REPO/build/evidence/alpha-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$out"
name=$(basename "$tarball")
cat > "$out/inside.sh" <<'INSIDE'
#!/bin/sh
set -eu
release=$1; sha=$2
log() { echo "alpha: $*"; }
printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260901T000000Z/ bookworm main\n' > /etc/apt/sources.list
rm -f /etc/apt/sources.list.d/*
apt-get -qq -o Acquire::Check-Valid-Until=false update
apt-get -qq install -y --no-install-recommends git > /dev/null
sh /scripts/kvm-entrypoint.sh true
sh /scripts/install.sh "$release" --sha256 "$sha" --prefix /opt/agentos --home /root/.agentos > /out/install.log
bin=/opt/agentos/current/bin
cp /opt/agentos/current/MANIFEST.json /out/release-MANIFEST.json
manifest=/opt/agentos/current/MANIFEST.json
field() { sed -n "s/.*\"$1\":{\"digest\":\"\([0-9a-f]*\)\",\"id\":\"\([^\"]*\)\".*/\\$2/p" "$manifest"; }
image_id=$(field image 2)
component_id=$(field component 2); component_digest=$(field component 1)
agentos() { "$bin/agentos" --home /root/.agentos --worker firecracker --firecracker "$bin/firecracker" --jailer "$bin/jailer" "$@"; }
contract() { # contract SNAPSHOT PROFILE GOAL MODEL? > file
  if [ -n "${4:-}" ]; then
    caps='"snapshot.read","workspace.apply_patch","verification.run","artifact.export","snapshot.analyze","model.request"'
    limits='"model_requests":12,"max_output_tokens_per_request":16000,"tool_actions":12,"deadline_seconds":900'
  else
    caps='"snapshot.read","workspace.apply_patch","verification.run","artifact.export","snapshot.analyze"'
    limits='"model_requests":1,"max_output_tokens_per_request":1000,"tool_actions":10,"deadline_seconds":600'
  fi
  printf '{"goal":"%s","repository":{"source":"/work/%s-repo","revision":"recorded-at-submission"},"profile":"%s","editable_paths":["src/**"],"verification_profile":"%s","capabilities":[%s],"analyzer":{"id":"%s","digest":"%s"},"limits":{%s,"worker_vcpus":1,"worker_memory_mib":256}}\n' \
    "$3" "$1" "$image_id" "$2" "$caps" "$component_id" "$component_digest" "$limits"
}
record() { # record DIR TASK START END
  dir=$1; mkdir -p "$dir"
  agentos status "$2" > "$dir/status.json"
  agentos events "$2" > "$dir/events.jsonl"
  agentos export "$2" "$dir/bundle" > "$dir/manifest.json" 2> "$dir/export.err" || true
  printf '{"task_id":"%s","started":%s,"ended":%s,"seconds":%s}\n' "$2" "$3" "$4" "$(( $4 - $3 ))" > "$dir/run.json"
}
for snap in parser:parser-checks-v1 duration:duration-checks-v1; do
  s=${snap%%:*}; profile=${snap#*:}
  rm -rf "/work/$s-repo"; mkdir -p /work; cp -R "/fixtures/$s-repo" "/work/$s-repo"
  case $s in
    parser) goal='fix the parser: parse_kv must pass the protected parser checks';;
    duration) goal='fix parse_duration so that it passes the protected duration checks';;
  esac
  contract "$s" "$profile" "$goal" > "/work/$s-fake.json"
  contract "$s" "$profile" "$goal" model > "/work/$s-live.json"
  log "$s: deterministic agent, killed during the patch"
  start=$(date +%s)
  set +e
  agentos submit "/work/$s-fake.json" --yes --fake-agent-patch "/fixtures/$s-repo.fix.patch" \
    --crash-at during-execute:apply_patch > "/out/$s-fake-submit.out" 2> "/out/$s-fake-submit.err"
  code=$?
  set -e
  [ "$code" -eq 75 ] || { log "$s: submit exited $code, expected 75"; cat "/out/$s-fake-submit.err"; exit 1; }
  task=$(sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p' "/out/$s-fake-submit.err" | head -n1)
  agentos resume "$task" > "/out/$s-fake-resume.json" || true
  record "/out/$s-fake" "$task" "$start" "$(date +%s)"
  log "$s: deterministic: $(cat "/out/$s-fake-resume.json")"
  if [ "${SKIP_LIVE:-}" = 1 ]; then log "$s: live run skipped (dry run)"; continue; fi
  log "$s: live model (bounded; recorded whatever happens, never retried)"
  start=$(date +%s)
  set +e
  agentos --api-key-file /run/agentos/key submit "/work/$s-live.json" --yes --model anthropic:claude-opus-5-5 \
    > "/out/$s-live-submit.out" 2> "/out/$s-live-submit.err"
  code=$?
  set -e
  end=$(date +%s)
  task=$(sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p' "/out/$s-live-submit.out" "/out/$s-live-submit.err" | head -n1)
  [ -n "$task" ] || { log "$s: the live submit did not create a task (exit $code)"; cat "/out/$s-live-submit.err"; exit 1; }
  record "/out/$s-live" "$task" "$start" "$end"
  log "$s: live: exit $code, $(cat "/out/$s-live-submit.out")"
done
log 'all runs recorded'
INSIDE
docker run --rm --init \
  --device /dev/kvm:/dev/kvm --cap-add SYS_ADMIN \
  --security-opt "seccomp=$REPO/scripts/kvm-seccomp.json" --security-opt apparmor=unconfined \
  -v "$tarball:/release/$name:ro" -v "$REPO/fixtures:/fixtures:ro" -v "$REPO/scripts:/scripts:ro" \
  -v "$key:/run/agentos/key:ro" -v "$out:/out" -e "SKIP_LIVE=${AGENTOS_ALPHA_SKIP_LIVE:-}" "$image" \
  sh /out/inside.sh "/release/$name" "$(cat "$tarball.sha256")" 2>&1 | tee "$out/alpha.log"
case "$(tail -n1 "$out/alpha.log")" in *'all runs recorded') echo "alpha evidence recorded: $out";; *) echo "alpha evidence run failed: $out" >&2; exit 1;; esac
