#!/bin/sh
# The clean-host smoke run: installs a release (scripts/release.sh) in a fresh Debian container
# on this KVM host and drives the parser fixture end to end on the jailed worker.
#
#   sh scripts/smoke-install.sh build/release/agentos-VERSION-x86_64-linux.tar.gz
#
# The container starts from debian:bookworm-slim pinned by digest with the same KVM settings as
# the test-kvm service (the device, CAP_SYS_ADMIN, the seccomp profile). Like an unprepared
# host it has no git and a read-only cgroup tree, and the installer must refuse both; then
# git comes from the pinned Debian snapshot and the cgroups are delegated (kvm-entrypoint.sh,
# what a host's init does). As root: install, submit the fixture with the release's analyzer
# and a crash during the patch, resume, export, and compare the export with the release's
# manifest and the fixture patch. As an unprivileged user: install with --allow-unjailed and
# run the fixture unjailed. Only one KVM host is available, so a fresh container on it stands in
# for a fresh host. Logs and the bundle go to build/evidence/smoke-<time>-<pid>/.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
[ "$#" -eq 1 ] || { echo 'usage: scripts/smoke-install.sh TARBALL' >&2; exit 2; }
tarball=$(realpath "$1")
[ -f "$tarball" ] && [ -f "$tarball.sha256" ] || { echo "$1 and $1.sha256 must exist" >&2; exit 2; }
[ -c /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo 'the smoke run needs a usable /dev/kvm' >&2; exit 2; }
image=debian@sha256:7c7b2c966bc9ee8cedfeef67e0e279108992c77681fa595db4a9d65c06ccc587
out="$REPO/build/evidence/smoke-$(date -u +%Y%m%dT%H%M%SZ)-$$"
mkdir -p "$out"
name=$(basename "$tarball")
cat > "$out/inside.sh" <<'INSIDE'
#!/bin/sh
set -eu
release=$1; sha=$2
log() { echo "smoke: $*"; }
refused() { # refused MESSAGE: the root install must refuse with MESSAGE and leave no prefix
  if sh /scripts/install.sh "$release" --sha256 "$sha" --prefix /opt/agentos --home /root/.agentos > /out/refused.out 2>&1; then
    log "the installer accepted an unprepared host ($1)"; exit 1
  fi
  grep -q "$1" /out/refused.out || { log "expected a refusal '$1':"; cat /out/refused.out; exit 1; }
  [ ! -e /opt/agentos/current ] || { log 'a refused install was activated'; exit 1; }
  log "refused as expected: $(tail -n1 /out/refused.out)"
}
refused 'git is required'
# git (the host-side patch parser) and the CA certificates (TLS to the model provider),
# from the guest rootfs's Debian snapshot.
printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20260901T000000Z/ bookworm main\n' > /etc/apt/sources.list
rm -f /etc/apt/sources.list.d/*
apt-get -qq -o Acquire::Check-Valid-Until=false update
apt-get -qq install -y --no-install-recommends git ca-certificates > /dev/null
refused 'host check failed'
log 'delegating cgroups as a host init would'
sh /scripts/kvm-entrypoint.sh true
log "installing $release"
sh /scripts/install.sh "$release" --sha256 "$sha" --prefix /opt/agentos --home /root/.agentos
bin=/opt/agentos/current/bin
manifest=/opt/agentos/current/MANIFEST.json
# MANIFEST.json is compact with sorted keys: each entry starts with its digest and id.
field() { sed -n "s/.*\"$1\":{\"digest\":\"\([0-9a-f]*\)\",\"id\":\"\([^\"]*\)\".*/\\$2/p" "$manifest"; }
image_id=$(field image 2); image_digest=$(field image 1)
component_id=$(field component 2); component_digest=$(field component 1)
profile_digest=$(field profile 1)
log "release image $image_id@$image_digest, analyzer $component_id@$component_digest"
mkdir -p /work && cp -R /fixtures/parser-repo /work/repo
cat > /work/task.json <<JSON
{"goal":"fix the parser","repository":{"source":"/work/repo","revision":"recorded-at-submission"},
 "profile":"$image_id","editable_paths":["src/**"],"verification_profile":"parser-checks-v1",
 "capabilities":["snapshot.read","workspace.apply_patch","verification.run","artifact.export","snapshot.analyze"],
 "analyzer":{"id":"$component_id","digest":"$component_digest"},
 "limits":{"model_requests":1,"max_output_tokens_per_request":1000,"tool_actions":10,"deadline_seconds":600,"worker_vcpus":1,"worker_memory_mib":256}}
JSON
agentos() { "$bin/agentos" --home /root/.agentos --worker firecracker --firecracker "$bin/firecracker" --jailer "$bin/jailer" "$@"; }
log "submitting with a crash during the patch"
set +e
agentos submit /work/task.json --yes --fake-agent-patch /fixtures/parser-repo.fix.patch \
  --crash-at during-execute:apply_patch > /out/submit.out 2> /out/submit.err
code=$?
set -e
[ "$code" -eq 75 ] || { log "submit exited $code, expected the injected crash (75)"; cat /out/submit.err; exit 1; }
task=$(sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p' /out/submit.err | head -n1)
log "task $task crashed as asked; resuming"
agentos resume "$task" | tee /out/resume.json
grep -q '"state":"SUCCEEDED"' /out/resume.json || { log 'resume did not succeed'; exit 1; }
agentos status "$task" > /out/status.json
grep -q '"jailed":true' /out/status.json || { log 'the task did not run jailed'; exit 1; }
agentos export "$task" /out/bundle > /out/manifest.json
check() { grep -q "$1" /out/manifest.json || { log "export: $2"; exit 1; }; }
check "\"guest_image_digest\":\"$image_digest\"" 'not the release image'
check "\"verification_profile_digest\":\"$profile_digest\"" 'not the release profile'
check '"state":"SUCCEEDED"' 'not succeeded'
grep -q '"analyzer":"repo-analyzer-v1"' /out/bundle/analysis/report.json || { log 'no analysis report'; exit 1; }
cmp /out/bundle/patch.diff /fixtures/parser-repo.fix.patch || { log 'the exported patch is not the fixture patch'; exit 1; }
log 'root: install, jailed run, kill and resume, export passed'
# An unprivileged user installs for unjailed use and runs the fixture.
useradd -m -u 1000 agent
chmod 0755 /work && cp /work/task.json /home/agent/task.json && cp -R /work/repo /home/agent/repo
sed -i 's|"/work/repo"|"/home/agent/repo"|' /home/agent/task.json
chown -R agent:agent /home/agent
runuser -u agent -- env HOME=/home/agent sh /scripts/install.sh "$release" --sha256 "$sha" --allow-unjailed > /out/user-install.out 2>&1 ||
  { log 'the unprivileged install failed:'; cat /out/user-install.out; exit 1; }
ubin=/home/agent/.local/share/agentos/current/bin
runuser -u agent -- env HOME=/home/agent "$ubin/agentos" --home /home/agent/.agentos --worker firecracker \
  --firecracker "$ubin/firecracker" --jailer "$ubin/jailer" --allow-unjailed \
  submit /home/agent/task.json --yes --fake-agent-patch /fixtures/parser-repo.fix.patch > /out/user-run.json 2> /out/user-run.err ||
  { log 'the unjailed run failed:'; cat /out/user-run.err; exit 1; }
grep -q '"state":"SUCCEEDED"' /out/user-run.json || { log 'the unjailed run did not succeed'; exit 1; }
user_task=$(sed -n 's/.*"task_id":"\([^"]*\)".*/\1/p' /out/user-run.json)
runuser -u agent -- env HOME=/home/agent "$ubin/agentos" --home /home/agent/.agentos status "$user_task" | grep -q '"jailed":false' ||
  { log 'the unprivileged run claims a jail'; exit 1; }
log 'unprivileged: install with --allow-unjailed, unjailed run passed'
log 'install, jailed run, kill and resume, export, unprivileged unjailed run: all checks passed'
INSIDE
docker run --rm --init \
  --device /dev/kvm:/dev/kvm --cap-add SYS_ADMIN \
  --security-opt "seccomp=$REPO/scripts/kvm-seccomp.json" --security-opt apparmor=unconfined \
  -v "$tarball:/release/$name:ro" -v "$REPO/fixtures:/fixtures:ro" -v "$REPO/scripts:/scripts:ro" \
  -v "$out:/out" "$image" \
  sh /out/inside.sh "/release/$name" "$(cat "$tarball.sha256")" 2>&1 | tee "$out/smoke.log"
status=$(tail -n1 "$out/smoke.log")
case "$status" in *'all checks passed') echo "smoke run passed: $out";; *) echo "smoke run failed: $out" >&2; exit 1;; esac
