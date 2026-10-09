#!/bin/sh
# Installs an agentos release tarball (scripts/release.sh) on this host.
#
#   install.sh TARBALL --sha256 HEX [--prefix DIR] [--home DIR] [--allow-unjailed]
#   install.sh --self-test
#
# In order, each refusal exits 2 with the reason and leaves nothing new behind:
#   1. the architecture must be x86_64;
#   2. the tarball must have the given sha256, hold one release directory without absolute or
#      `..` paths, and every file must match its SHA256SUMS, which must list every file;
#   3. /dev/kvm must be a character device readable and writable by this user, and git must
#      be installed (agentos parses patches with it);
#   4. cgroup v2 with cpu, memory and pids must be available for the jail (unless
#      --allow-unjailed);
#   5. the staged release's own `agentos host-check` must pass: KVM, Firecracker, git and,
#      unless --allow-unjailed, the jail probe (root, delegable cgroups, the home's filesystem);
# then the release is staged under PREFIX/.staging-<pid>, moved into PREFIX/VERSION with one
# rename and PREFIX/current switched with a symlink rename. The same version with the same
# content is a no-op; with other content it is refused. The home is created only when absent;
# an existing one is only given the release's image, profile and component, registered by
# the installed agentos (content-addressed, so registering twice changes nothing).
#
# Defaults: --prefix $HOME/.local/share/agentos, --home $HOME/.agentos.
# Test seams, honoured only with AGENTOS_INSTALL_TEST=1: AGENTOS_INSTALL_ARCH (uname -m),
# AGENTOS_INSTALL_KVM (the KVM device path; a regular file passes), AGENTOS_INSTALL_CGROUP (the
# cgroup v2 root), AGENTOS_INSTALL_GIT (the git command) and AGENTOS_INSTALL_INTERRUPT=1 (exit
# 9 after staging, before the rename).
set -eu
umask 022

refuse() { echo "install.sh: $*" >&2; exit 2; }
seam() {
  if [ "${AGENTOS_INSTALL_TEST:-}" = 1 ]; then eval "printf '%s' \"\${$1:-$2}\""; else printf '%s' "$2"; fi
}

self_test() {
  REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
  t=$(mktemp -d)
  trap 'chmod -R u+w "$t" 2>/dev/null; rm -rf "$t"' EXIT
  export AGENTOS_INSTALL_TEST=1
  # A fake KVM device (a regular file stands in for the character device) and cgroup root.
  : > "$t/kvm"
  mkdir -p "$t/cgroup"; echo 'cpuset cpu io memory pids' > "$t/cgroup/cgroup.controllers"
  export AGENTOS_INSTALL_KVM="$t/kvm" AGENTOS_INSTALL_CGROUP="$t/cgroup" AGENTOS_INSTALL_ARCH=x86_64
  # A release whose agentos records its arguments.
  make_release() { # make_release DIR VERSION CONTENT
    top="$1/agentos-$2-x86_64-linux"
    mkdir -p "$top/bin" "$top/images/img-v1" "$top/profiles/prof-v1" "$top/components/comp-v1"
    cat > "$top/bin/agentos" <<'FAKE'
#!/bin/sh
printf '%s\n' "$*" >> "$AGENTOS_INSTALL_TEST_LOG"
case "$*" in *host-check*)
  case "${AGENTOS_INSTALL_TEST_HOSTCHECK:-ok}" in
    ok) echo '{"jail":"ok"}';;
    *) echo "{\"jail\":\"${AGENTOS_INSTALL_TEST_HOSTCHECK#fail:}\"}"; exit 1;;
  esac;;
esac
echo '{}'
FAKE
    chmod 0755 "$top/bin/agentos"
    printf '%s' "$3" > "$top/bin/firecracker"; : > "$top/bin/jailer"
    : > "$top/images/img-v1/image.json"; : > "$top/profiles/prof-v1/profile.json"
    : > "$top/components/comp-v1/component.json"
    printf '{"version":"%s"}\n' "$2" > "$top/MANIFEST.json"
    (cd "$top" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
    (cd "$1" && tar -czf "agentos-$2-x86_64-linux.tar.gz" "agentos-$2-x86_64-linux")
    echo "$1/agentos-$2-x86_64-linux.tar.gz"
  }
  sum() { sha256sum "$1" | cut -d' ' -f1; }
  export AGENTOS_INSTALL_TEST_LOG="$t/calls"
  mkdir -p "$t/r1" "$t/r2" "$t/bad"
  rel=$(make_release "$t/r1" 1.0.0 one)
  expect_refusal() { # expect_refusal MESSAGE PREFIX ARGS...
    msg=$1; prefix=$2; shift 2
    before=$(find "$prefix" 2>/dev/null | sort)
    if sh "$0" "$@" > "$t/out" 2> "$t/err"; then echo "self-test: accepted ($msg)" >&2; exit 1; fi
    grep -q "$msg" "$t/err" || { echo "self-test: expected '$msg', got:" >&2; cat "$t/err" >&2; exit 1; }
    [ "$(find "$prefix" 2>/dev/null | sort)" = "$before" ] || { echo "self-test: refusal ($msg) left files" >&2; exit 1; }
  }
  p="$t/prefix"; h="$t/home"
  expect_refusal 'checksum mismatch' "$p" "$rel" --sha256 "$(printf '0%.0s' $(seq 64))" --prefix "$p" --home "$h"
  AGENTOS_INSTALL_ARCH=aarch64 expect_refusal 'unsupported architecture aarch64' "$p" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h"
  AGENTOS_INSTALL_KVM="$t/no-kvm" expect_refusal 'KVM is not usable' "$p" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h"
  mkdir -p "$t/v1cgroup"
  AGENTOS_INSTALL_CGROUP="$t/v1cgroup" expect_refusal 'cgroup v2 with cpu, memory and pids' "$p" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h"
  AGENTOS_INSTALL_GIT=no-such-git expect_refusal 'git is required' "$p" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h"
  # cgroups present but not delegable: the release's own jail probe refuses.
  AGENTOS_INSTALL_TEST_HOSTCHECK='fail:jailer unavailable: cannot delegate cpu memory pids' \
    expect_refusal 'host check failed.*cannot delegate' "$p" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h"
  # A release with a file SHA256SUMS does not list, and one whose file does not match.
  cp -r "$t/r1/agentos-1.0.0-x86_64-linux" "$t/bad/agentos-1.0.0-x86_64-linux"
  : > "$t/bad/agentos-1.0.0-x86_64-linux/bin/extra"
  (cd "$t/bad" && tar -czf extra.tar.gz agentos-1.0.0-x86_64-linux)
  expect_refusal 'not listed in SHA256SUMS' "$p" "$t/bad/extra.tar.gz" --sha256 "$(sum "$t/bad/extra.tar.gz")" --prefix "$p" --home "$h"
  rm "$t/bad/agentos-1.0.0-x86_64-linux/bin/extra"; echo tampered > "$t/bad/agentos-1.0.0-x86_64-linux/bin/jailer"
  (cd "$t/bad" && tar -czf tampered.tar.gz agentos-1.0.0-x86_64-linux)
  expect_refusal 'does not match SHA256SUMS' "$p" "$t/bad/tampered.tar.gz" --sha256 "$(sum "$t/bad/tampered.tar.gz")" --prefix "$p" --home "$h"
  # A path escaping the staging directory.
  mkdir -p "$t/esc/inner"; : > "$t/esc/evil"
  (cd "$t/esc/inner" && tar -czPf ../escape.tar.gz ../evil)
  expect_refusal 'unsafe path' "$p" "$t/esc/escape.tar.gz" --sha256 "$(sum "$t/esc/escape.tar.gz")" --prefix "$p" --home "$h"
  # An interrupted install leaves only its staging directory, which the next run removes.
  if AGENTOS_INSTALL_INTERRUPT=1 sh "$0" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h" > /dev/null 2>&1; then
    echo 'self-test: the interrupted install succeeded' >&2; exit 1
  fi
  [ ! -e "$p/1.0.0" ] && [ ! -e "$p/current" ] || { echo 'self-test: an interrupted install was activated' >&2; exit 1; }
  ls -d "$p"/.staging-* > /dev/null 2>&1 || { echo 'self-test: no staging left to clean' >&2; exit 1; }
  # A real install, then the same again (a no-op), into an existing home that is kept.
  mkdir -p "$h"; echo keep > "$h/marker"
  sh "$0" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h" > "$t/out"
  ! ls -d "$p"/.staging-* > /dev/null 2>&1 || { echo 'self-test: staging was not cleaned' >&2; exit 1; }
  [ "$(readlink "$p/current")" = 1.0.0 ] && [ -x "$p/1.0.0/bin/agentos" ] || { echo 'self-test: not installed' >&2; exit 1; }
  [ "$(cat "$h/marker")" = keep ] || { echo 'self-test: the existing home was changed' >&2; exit 1; }
  grep -q "^--home $h image register $p/1.0.0/images/img-v1$" "$t/calls" &&
    grep -q "^--home $h profile register $p/1.0.0/profiles/prof-v1$" "$t/calls" &&
    grep -q "^--home $h component register $p/1.0.0/components/comp-v1$" "$t/calls" ||
    { echo 'self-test: the release was not registered:' >&2; cat "$t/calls" >&2; exit 1; }
  sh "$0" "$rel" --sha256 "$(sum "$rel")" --prefix "$p" --home "$h" > "$t/out"
  grep -q 'already installed' "$t/out" || { echo 'self-test: a reinstall was not a no-op' >&2; exit 1; }
  # The same version with other content is refused, and the installed one is untouched.
  other=$(make_release "$t/r2" 1.0.0 two)
  expect_refusal 'already installed with different content' "$p" "$other" --sha256 "$(sum "$other")" --prefix "$p" --home "$h"
  [ "$(cat "$p/1.0.0/bin/firecracker")" = one ] || { echo 'self-test: the installed release changed' >&2; exit 1; }
  # A fresh home is created; --allow-unjailed installs without cgroup v2.
  AGENTOS_INSTALL_CGROUP="$t/v1cgroup" sh "$0" "$rel" --sha256 "$(sum "$rel")" --prefix "$t/p2" --home "$t/new-home" --allow-unjailed > "$t/out" 2> "$t/err"
  [ -d "$t/new-home" ] && grep -q 'unjailed' "$t/err" || { echo 'self-test: --allow-unjailed failed' >&2; exit 1; }
  grep -q -- "--home $t/new-home .*--allow-unjailed host-check" "$t/calls" ||
    { echo 'self-test: the unjailed host check was not asked to allow unjailed use' >&2; exit 1; }
  expect_refusal 'paths with spaces' "$t/sp ace" "$rel" --sha256 "$(sum "$rel")" --prefix "$t/sp ace" --home "$h"
  echo 'install.sh self-test passed'
}

[ "${1:-}" = --self-test ] && { self_test; exit 0; }
[ "$#" -ge 1 ] || refuse 'usage: install.sh TARBALL --sha256 HEX [--prefix DIR] [--home DIR] [--allow-unjailed]'
tarball=$1; shift
want=''; prefix="$HOME/.local/share/agentos"; home="$HOME/.agentos"; unjailed=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --sha256) want=${2:?}; shift 2;;
    --prefix) prefix=${2:?}; shift 2;;
    --home) home=${2:?}; shift 2;;
    --allow-unjailed) unjailed=1; shift;;
    *) refuse "unknown argument $1";;
  esac
done
[ -n "$want" ] || refuse 'the release checksum is required: --sha256 HEX'
case "$prefix$home" in *' '*) refuse 'paths with spaces are not supported for --prefix and --home';; esac

# 1. Architecture.
arch=$(seam AGENTOS_INSTALL_ARCH "$(uname -m)")
[ "$arch" = x86_64 ] || refuse "unsupported architecture $arch; releases are built for x86_64"
# 2. The tarball itself.
[ -f "$tarball" ] || refuse "$tarball is not a file"
got=$(sha256sum "$tarball" | cut -d' ' -f1)
[ "$got" = "$want" ] || refuse "checksum mismatch: $tarball is $got, expected $want"
if tar -tzf "$tarball" | grep -Eq '^/|(^|/)\.\.(/|$)'; then refuse "unsafe path in $tarball"; fi
# 3. KVM (a character device; the test seam's stand-in is a regular file) and git.
kvm=$(seam AGENTOS_INSTALL_KVM /dev/kvm)
kind=-c; [ "${AGENTOS_INSTALL_TEST:-}" = 1 ] && kind=-e
{ [ "$kind" "$kvm" ] && [ -r "$kvm" ] && [ -w "$kvm" ]; } ||
  refuse "KVM is not usable: $kvm must be a device readable and writable by $(id -un) (enable virtualization; add the user to the kvm group)"
git=$(seam AGENTOS_INSTALL_GIT git)
command -v "$git" > /dev/null 2>&1 || refuse 'git is required (agentos parses patches with it); install git'
# 4. cgroups for the jail.
cgroup=$(seam AGENTOS_INSTALL_CGROUP /sys/fs/cgroup)
if [ -z "$unjailed" ]; then
  controllers=$(cat "$cgroup/cgroup.controllers" 2>/dev/null || true)
  for c in cpu memory pids; do
    case " $controllers " in *" $c "*) ;; *)
      refuse "cgroup v2 with cpu, memory and pids is required for the jail ($cgroup/cgroup.controllers has: ${controllers:-nothing}); pass --allow-unjailed to install for unjailed use";;
    esac
  done
else
  echo 'install.sh: --allow-unjailed: Firecracker will run unjailed unless the host is prepared for the jail later' >&2
fi

# 5. Stage, verify, activate.
created_prefix=''
[ -d "$prefix" ] || created_prefix=1
mkdir -p "$prefix"
for old in "$prefix"/.staging-*; do [ -e "$old" ] && rm -rf "$old"; done
staging="$prefix/.staging-$$"
mkdir "$staging"
# A refusal from here on removes what this run made: its staging, and the prefix if it made it.
fail() {
  rm -rf "$staging"
  if [ -n "$created_prefix" ]; then rmdir "$prefix" 2>/dev/null || true; fi
  refuse "$@"
}
tar -xzf "$tarball" -C "$staging" --no-same-owner || fail "cannot extract $tarball"
set -- "$staging"/*
[ "$#" -eq 1 ] && [ -d "$1" ] || fail "$tarball must hold exactly one release directory"
top=$1; name=$(basename "$top")
version=${name#agentos-}; version=${version%-x86_64-linux}
[ "$name" = "agentos-$version-x86_64-linux" ] && [ -n "$version" ] || fail "unexpected release directory $name"
grep -q "\"version\":\"$version\"" "$top/MANIFEST.json" 2>/dev/null || fail "MANIFEST.json does not name version $version"
[ -f "$top/SHA256SUMS" ] || fail 'SHA256SUMS is missing'
(cd "$top" && sha256sum -c --quiet --strict SHA256SUMS > /dev/null 2>&1) || fail 'a file does not match SHA256SUMS'
listed=$(cd "$top" && cut -d' ' -f3- SHA256SUMS | sed 's|^\*||' | sort)
present=$(cd "$top" && find . -type f ! -name SHA256SUMS | sort)
[ "$listed" = "$present" ] || fail 'a file is not listed in SHA256SUMS'
# The release's own host check, before anything is activated: the CLI's exact rules.
check_args="--home $home --firecracker $top/bin/firecracker --jailer $top/bin/jailer"
[ -z "$unjailed" ] || check_args="$check_args --allow-unjailed"
# shellcheck disable=SC2086 # the arguments are paths without spaces
if ! report=$("$top/bin/agentos" $check_args host-check 2>&1); then
  fail "host check failed: $report"
fi
if [ "${AGENTOS_INSTALL_TEST:-}" = 1 ] && [ "${AGENTOS_INSTALL_INTERRUPT:-}" = 1 ]; then exit 9; fi
dest="$prefix/$version"
if [ -e "$dest" ]; then
  if cmp -s "$dest/SHA256SUMS" "$top/SHA256SUMS" && (cd "$dest" && sha256sum -c --quiet --strict SHA256SUMS > /dev/null 2>&1); then
    rm -rf "$staging"
    echo "agentos $version is already installed in $dest"
  else
    fail "version $version is already installed with different content in $dest; remove it first"
  fi
else
  mv "$top" "$dest"
  rmdir "$staging"
  echo "installed agentos $version in $dest"
fi
ln -sfn "$version" "$prefix/.current.tmp"
mv -T "$prefix/.current.tmp" "$prefix/current"

# 6. The home: created only when absent; given the release's image, profile and component.
if [ -e "$home" ]; then echo "keeping the existing home $home"; else mkdir -p "$home"; fi
agentos="$prefix/$version/bin/agentos"
for kind in image profile component; do
  case $kind in image) dir=images;; profile) dir=profiles;; component) dir=components;; esac
  for entry in "$prefix/$version/$dir"/*; do
    [ -d "$entry" ] || continue
    "$agentos" --home "$home" "$kind" register "$entry" > /dev/null || refuse "cannot register $entry"
  done
done
echo "agentos $version is ready: $agentos --home $home"
