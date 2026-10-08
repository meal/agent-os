#!/bin/sh
# Builds a guest image directory (image.json, vmlinux, rootfs.squashfs) from a recipe and
# prints {"id","digest"} as `agentos image register` computes it.
#
#   docker compose run --rm test-kvm sh scripts/build-guest-image.sh \
#       guest/python-stdlib-v1 build/guest-images/python-stdlib-v1 [--verify]
#
# Run it in `test-kvm` for the KVM tier: that service mounts the `guest-images` volume on
# build/guest-images (in `test` the same path is the host's build/ directory).
#
# Steps ("Guest image" in docs/superpowers/specs/2026-10-02-phase-3b-firecracker-worker-design.md):
#   1. the kernel from RECIPE/kernel.lock, sha256-verified, cached in build/kernels/;
#   2. agentos-guest, static musl (x86_64-unknown-linux-musl);
#   3. mmdebstrap from the pinned snapshot.debian.org archive (RECIPE/snapshot.lock) with
#      RECIPE/packages.txt and the RECIPE/hooks/customize.sh hook, SOURCE_DATE_EPOCH exported;
#   4. a read-only zstd squashfs with fixed times and owners;
#   5. image.json from RECIPE/image.json.in.
# OUT_DIR is replaced only once the new image is complete, and holds exactly the three files.
#
# --verify builds twice (OUT_DIR and OUT_DIR.verify; the second build compiles agentos-guest
# from scratch in its own target directory) and fails unless the three files are
# byte-identical (cmp). OUT_DIR.verify is removed when they are, kept for diffing otherwise.
#
# OUT_DIR (and OUT_DIR.verify) is replaced with `rm -rf`, so before any work the script
# refuses (exit 2) an OUT_DIR that is `/`, the repository root or one of its ancestors, a
# symlink, a mount point, not a directory, or a directory holding anything but image.json,
# vmlinux and rootfs.squashfs. A path that does not exist is accepted.
#
# `scripts/build-guest-image.sh --self-test` checks that guard and the --verify comparison
# without building anything (no root, no network).
#
# Needs root in the container (compose gives it) and network access to snapshot.debian.org
# and the Firecracker CI bucket.
set -eu

PROG=build-guest-image.sh
usage() { echo "usage: scripts/build-guest-image.sh RECIPE_DIR OUT_DIR [--verify] | --self-test" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/.." && pwd -P)
MUSL_TARGET=x86_64-unknown-linux-musl
IMAGE_FILES="image.json vmlinux rootfs.squashfs"

refuse_out() { echo "$PROG: refusing OUT_DIR $1: $2" >&2; exit 2; }

# check_out PATH: exits 2 unless PATH is safe to delete and replace with an image directory.
check_out() {
  [ -n "$1" ] || refuse_out "''" "empty path"
  lexical=$(realpath -m -s "$1")
  [ -L "$lexical" ] && refuse_out "$lexical" "it is a symlink"
  resolved=$(realpath -m "$1")
  [ "$resolved" = / ] && refuse_out "$resolved" "it is the root directory"
  case "$REPO/" in "$resolved"/*) refuse_out "$resolved" "it is the repository root $REPO or one of its ancestors" ;; esac
  [ -e "$resolved" ] || return 0
  [ -d "$resolved" ] || refuse_out "$resolved" "it exists and is not a directory"
  mountpoint -q "$resolved" && refuse_out "$resolved" "it is a mount point"
  for entry in "$resolved"/* "$resolved"/.[!.]* "$resolved"/..?*; do
    [ -e "$entry" ] || [ -L "$entry" ] || continue
    name=${entry##*/}
    case " $IMAGE_FILES " in *" $name "*) ;; *) refuse_out "$resolved" "it holds $name, which is not an image file" ;; esac
    { [ -f "$entry" ] && [ ! -L "$entry" ]; } || refuse_out "$resolved" "$name is not a regular file"
  done
}

# compare_outputs A B: exit 0 and remove B when the image files of A and B are identical;
# otherwise name the differing files, keep B, and return 1.
compare_outputs() {
  differ=
  for f in $IMAGE_FILES; do
    cmp -s "$1/$f" "$2/$f" || differ="$differ $f"
  done
  if [ -n "$differ" ]; then
    echo "$PROG: --verify: not reproducible:$differ (kept $2)" >&2
    return 1
  fi
  check_out "$2"
  rm -rf --one-file-system "$2"
}

self_test() {
  t=$(mktemp -d)
  fails=0
  ok() { echo "ok   $1"; }
  bad() { echo "FAIL $1"; fails=$((fails + 1)); }
  listing() { (cd "$1" && find . -printf '%p %s %m\n' | sort | sha256sum); }
  # The full script must refuse with exit 2 and leave DIR exactly as it was.
  expect_refused() {
    before=$(listing "$2")
    if (cd "$t" && sh "$REPO/scripts/build-guest-image.sh" "$REPO/guest/python-stdlib-v1" "$3" >/dev/null 2>"$t/err"); then
      bad "$1: accepted"
    else
      code=$?
      if [ "$code" -eq 2 ] && [ "$(listing "$2")" = "$before" ] && grep -q "refusing OUT_DIR" "$t/err"; then
        ok "$1: refused, nothing deleted ($(sed 's/^[^:]*: //' "$t/err"))"
      else
        bad "$1: exit $code, $(cat "$t/err")"
      fi
    fi
  }
  # The guard alone, in this process: for paths a broken guard must never get to delete.
  expect_guard() {
    if (cd "${4:-$t}" && check_out "$3") 2>"$t/err"; then r=accepted; else r=refused; fi
    if [ "$r" = "$2" ]; then ok "$1: $r $(cat "$t/err")"; else bad "$1: $r, expected $2 $(cat "$t/err")"; fi
  }
  mkdir -p "$t/unrelated/sub" "$t/extra" "$t/image"
  echo keep > "$t/unrelated/sub/file"
  for f in $IMAGE_FILES; do echo x > "$t/extra/$f"; echo x > "$t/image/$f"; done
  echo keep > "$t/extra/notes.txt"
  ln -s "$t/image" "$t/link"

  expect_guard "'.' in the repository" refused . "$REPO"
  expect_guard "the repository root" refused "$REPO"
  expect_guard "an ancestor of the repository" refused "$(dirname "$REPO")"
  expect_guard "/" refused /
  expect_guard "a non-existent path" accepted "$t/new/python-stdlib-v1"
  expect_guard "an existing image directory" accepted "$t/image"
  if mountpoint -q /dev/shm && [ -z "$(ls -A /dev/shm)" ]; then
    expect_guard "an empty mount point (/dev/shm)" refused /dev/shm
  fi
  expect_refused "a populated unrelated directory" "$t/unrelated" "$t/unrelated"
  expect_refused "an image directory with an extra file" "$t/extra" "$t/extra"
  expect_refused "a symlink to an image directory" "$t/image" "$t/link"
  expect_refused "a file" "$t/unrelated" "$t/unrelated/sub/file"
  # OUT_DIR.verify gets the same guard.
  mkdir -p "$t/v" "$t/v.verify"; echo keep > "$t/v.verify/other"
  before=$(listing "$t/v.verify")
  if (cd "$t" && sh "$REPO/scripts/build-guest-image.sh" "$REPO/guest/python-stdlib-v1" "$t/v" --verify >/dev/null 2>"$t/err"); then
    bad "OUT_DIR.verify with a stray file: accepted"
  elif [ "$(listing "$t/v.verify")" = "$before" ] && grep -q "v.verify" "$t/err"; then
    ok "OUT_DIR.verify with a stray file: refused, nothing deleted"
  else
    bad "OUT_DIR.verify: $(cat "$t/err")"
  fi

  # The --verify comparison: a mismatch names the files and keeps the second output.
  mkdir -p "$t/a" "$t/b"
  for f in $IMAGE_FILES; do echo same > "$t/a/$f"; echo same > "$t/b/$f"; done
  echo other > "$t/b/rootfs.squashfs"
  if (compare_outputs "$t/a" "$t/b") 2>"$t/err"; then
    bad "--verify mismatch: accepted"
  elif grep -q "not reproducible: rootfs.squashfs (kept $t/b)" "$t/err" && [ -f "$t/b/rootfs.squashfs" ]; then
    ok "--verify mismatch: exit 1, $(sed 's/^[^:]*: //' "$t/err")"
  else
    bad "--verify mismatch: $(cat "$t/err")"
  fi
  echo same > "$t/b/rootfs.squashfs"
  if (compare_outputs "$t/a" "$t/b") 2>"$t/err" && [ ! -e "$t/b" ]; then
    ok "--verify match: exit 0, second output removed"
  else
    bad "--verify match: $(cat "$t/err")"
  fi

  # Linked Git worktrees use a .git file instead of a directory.
  [ -f "$REPO/Cargo.toml" ] && [ -e "$REPO/.git" ] || bad "the repository is intact"
  rm -rf "$t"
  [ "$fails" -eq 0 ] || { echo "$fails self-test(s) failed"; exit 1; }
  echo "all self-tests passed"
}

if [ $# -eq 1 ] && [ "$1" = --self-test ]; then self_test; exit 0; fi
[ $# -ge 2 ] && [ $# -le 3 ] || usage
VERIFY=
if [ $# -eq 3 ]; then
  [ "$3" = --verify ] || usage
  VERIFY=1
fi
# The guard, before any work.
check_out "$2"
OUT=$(realpath -m "$2")
[ -z "$VERIFY" ] || check_out "$OUT.verify"
RECIPE=$(realpath -e "$1")

[ "$(id -u)" -eq 0 ] || { echo "build-guest-image.sh: needs root (run it with docker compose)" >&2; exit 1; }
for f in kernel.lock snapshot.lock packages.txt image.json.in hooks/customize.sh; do
  [ -f "$RECIPE/$f" ] || { echo "build-guest-image.sh: $RECIPE/$f is missing" >&2; exit 1; }
done

json() { python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }
# An optional field: empty when absent.
json_opt() { python3 -c 'import json, sys; print(json.load(open(sys.argv[1])).get(sys.argv[2], ""))' "$1" "$2"; }
# A kernel is downloaded (`url`) or built here by scripts/build-kernel.sh (`built`: its output
# directory, relative to the repository); either way `sha256` pins the vmlinux.
KERNEL_URL=$(json_opt "$RECIPE/kernel.lock" url)
KERNEL_BUILT=$(json_opt "$RECIPE/kernel.lock" built)
KERNEL_SHA256=$(json "$RECIPE/kernel.lock" sha256)
KERNEL_VERSION=$(json "$RECIPE/kernel.lock" version)
SUITE=$(json "$RECIPE/snapshot.lock" suite)
MIRROR=$(json "$RECIPE/snapshot.lock" mirror)
SOURCE_DATE_EPOCH=$(json "$RECIPE/snapshot.lock" source_date_epoch)
export SOURCE_DATE_EPOCH
INCLUDE=$(grep -v '^[[:space:]]*$' "$RECIPE/packages.txt" | tr '\n' ',' | sed 's/,$//')

cd "$REPO"
AGENT_VERSION=$(cargo metadata --no-deps --format-version 1 --locked |
  python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "agentos-guest"))')
# /work belongs to the host user, not to the container's root: tell git it is ours to read.
GIT="git -c safe.directory=$REPO -C $REPO"
GIT_SHA=$($GIT rev-parse HEAD)
# A build from uncommitted changes says so.
if [ -n "$($GIT status --porcelain --untracked-files=no)" ]; then GIT_SHA="$GIT_SHA-dirty"; fi

TMPS=
cleanup() {
  for d in $TMPS; do rm -rf --one-file-system "$d"; done
}
trap cleanup EXIT
trap 'exit 130' INT TERM
# scratch_dir VAR TEMPLATE: VAR=$(mktemp -d TEMPLATE), removed on exit (not in a $(…) subshell,
# which would lose the TMPS update).
scratch_dir() { _d=$(mktemp -d "$2"); TMPS="$TMPS $_d"; eval "$1=\$_d"; }

# What the script creates under $REPO/build belongs to the owner of the checkout (the host
# user), not to the container's root, so `rm -rf build` and `git clean -fdx` work on the
# host. Paths outside $REPO/build are never touched.
OWNER=$(stat -c %u:%g "$REPO")
own_under_build() {
  for p in "$@"; do
    rp=$(realpath -m "$p")
    case "$rp" in "$REPO/build" | "$REPO/build/"*) [ -e "$rp" ] && chown -h "$OWNER" "$rp" ;; esac
  done
  return 0
}

# 1. The kernel, cached in build/kernels/ (or built there) and verified on every use.
KERNEL="$REPO/build/kernels/vmlinux-$KERNEL_VERSION"
[ -z "$KERNEL_BUILT" ] || KERNEL="$REPO/$KERNEL_BUILT/vmlinux"
kernel_ok() { [ -f "$KERNEL" ] && echo "$KERNEL_SHA256  $KERNEL" | sha256sum -c --status; }
if [ -n "$KERNEL_BUILT" ] && ! kernel_ok; then
  echo "build-guest-image.sh: $KERNEL is missing or not the pinned kernel ($KERNEL_SHA256); build it first:" >&2
  echo "  docker compose run --rm kernel-builder sh scripts/build-kernel.sh $KERNEL_BUILT --verify" >&2
  exit 1
fi
if ! kernel_ok; then
  mkdir -p "$REPO/build/kernels"
  echo "build-guest-image.sh: downloading $KERNEL_URL" >&2
  curl -fsSL --proto '=https' --proto-redir '=https' -o "$KERNEL.part" "$KERNEL_URL"
  echo "$KERNEL_SHA256  $KERNEL.part" | sha256sum -c --quiet || { rm -f "$KERNEL.part"; echo "build-guest-image.sh: kernel sha256 mismatch" >&2; exit 1; }
  mv -f "$KERNEL.part" "$KERNEL"
fi
own_under_build "$REPO/build" "$REPO/build/kernels" "$KERNEL"

# 2. The agent, static musl. TARGET_DIR is cargo's target directory for this build.
build_agent() {
  echo "build-guest-image.sh: cargo build agentos-guest ($MUSL_TARGET) in $1" >&2
  CARGO_TARGET_DIR="$1" cargo build -q --locked --release -p agentos-guest --target "$MUSL_TARGET"
  bin="$1/$MUSL_TARGET/release/agentos-guest"
  # Static: no program interpreter, no shared libraries.
  if readelf -l "$bin" | grep -q 'Requesting program interpreter' || readelf -d "$bin" | grep -q '(NEEDED)'; then
    echo "build-guest-image.sh: $bin is not statically linked" >&2
    exit 1
  fi
}

# 3-5. One image into OUT_DIR (replaced at the end), with the agent at GUEST_BIN.
build_image() {
  out=$1 guest_bin=$2
  scratch_dir root /var/tmp/agentos-rootfs.XXXXXX
  echo "build-guest-image.sh: mmdebstrap $SUITE from $MIRROR into $root" >&2
  # Snapshot Release files may carry an expired Valid-Until; the snapshot is pinned by date.
  mmdebstrap --mode=root --variant=apt --include="$INCLUDE" \
    --aptopt='Acquire::Check-Valid-Until "false"' --aptopt='Acquire::Retries "5"' \
    --customize-hook="sh '$RECIPE/hooks/customize.sh' \"\$1\" '$guest_bin'" \
    "$SUITE" "$root" "$MIRROR"
  setuid=$(find "$root" -xdev -type f \( -perm -4000 -o -perm -2000 \) -print)
  if [ -n "$setuid" ]; then
    echo "build-guest-image.sh: set-uid/set-gid files left in the image:" >&2
    echo "$setuid" >&2
    exit 1
  fi

  mkdir -p "$(dirname "$out")"
  scratch_dir stage "$out.tmp.XXXXXX"
  chmod 0755 "$stage"
  install -m 0644 "$KERNEL" "$stage/vmlinux"
  # mksquashfs refuses SOURCE_DATE_EPOCH together with -mkfs-time/-all-time; the options pin
  # the times (to 0) on their own.
  env -u SOURCE_DATE_EPOCH mksquashfs "$root" "$stage/rootfs.squashfs" -comp zstd -all-root -no-xattrs \
    -mkfs-time 0 -all-time 0 -noappend -no-progress -no-recovery >/dev/null
  chmod 0644 "$stage/rootfs.squashfs"
  sed -e "s|@AGENT_VERSION@|$AGENT_VERSION|" -e "s|@KERNEL_SHA256@|$KERNEL_SHA256|" -e "s|@GIT_SHA@|$GIT_SHA|" \
    "$RECIPE/image.json.in" > "$stage/image.json"
  chmod 0644 "$stage/image.json"
  rm -rf --one-file-system "$root"
  check_out "$out"
  rm -rf --one-file-system "$out"
  mv "$stage" "$out"
  own_under_build "$(dirname "$out")" "$out" "$out/image.json" "$out/vmlinux" "$out/rootfs.squashfs"
}

build_agent "$REPO/target"
build_image "$OUT" "$REPO/target/$MUSL_TARGET/release/agentos-guest"

if [ -n "$VERIFY" ]; then
  scratch_dir fresh_target /var/tmp/agentos-guest-target.XXXXXX
  build_agent "$fresh_target"
  if ! cmp "$REPO/target/$MUSL_TARGET/release/agentos-guest" "$fresh_target/$MUSL_TARGET/release/agentos-guest"; then
    echo "build-guest-image.sh: --verify: agentos-guest differs between two clean builds" >&2
    exit 1
  fi
  build_image "$OUT.verify" "$fresh_target/$MUSL_TARGET/release/agentos-guest"
  compare_outputs "$OUT" "$OUT.verify" || exit 1
  echo "build-guest-image.sh: --verify: two builds are byte-identical" >&2
fi

# The digest, exactly as the registry computes it: register into a scratch home.
cargo build -q --locked -p agentos-cli
scratch_dir home /var/tmp/agentos-image-home.XXXXXX
"$REPO/target/debug/agentos" --home "$home" image register "$OUT"
