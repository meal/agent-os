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
# Needs root in the container (compose gives it) and network access to snapshot.debian.org
# and the Firecracker CI bucket.
set -eu

usage() { echo "usage: scripts/build-guest-image.sh RECIPE_DIR OUT_DIR [--verify]" >&2; exit 2; }
[ $# -ge 2 ] && [ $# -le 3 ] || usage
VERIFY=
if [ $# -eq 3 ]; then
  [ "$3" = --verify ] || usage
  VERIFY=1
fi
REPO=$(cd "$(dirname "$0")/.." && pwd)
RECIPE=$(realpath -e "$1")
OUT=$(realpath -m "$2")
case "$OUT" in /) usage ;; esac
MUSL_TARGET=x86_64-unknown-linux-musl

[ "$(id -u)" -eq 0 ] || { echo "build-guest-image.sh: needs root (run it with docker compose)" >&2; exit 1; }
for f in kernel.lock snapshot.lock packages.txt image.json.in hooks/customize.sh; do
  [ -f "$RECIPE/$f" ] || { echo "build-guest-image.sh: $RECIPE/$f is missing" >&2; exit 1; }
done

json() { python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$1" "$2"; }
KERNEL_URL=$(json "$RECIPE/kernel.lock" url)
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

# 1. The kernel, cached in build/kernels/ and verified on every use.
KERNEL="$REPO/build/kernels/vmlinux-$KERNEL_VERSION"
kernel_ok() { [ -f "$KERNEL" ] && echo "$KERNEL_SHA256  $KERNEL" | sha256sum -c --status; }
if ! kernel_ok; then
  mkdir -p "$REPO/build/kernels"
  echo "build-guest-image.sh: downloading $KERNEL_URL" >&2
  curl -fsSL -o "$KERNEL.part" "$KERNEL_URL"
  echo "$KERNEL_SHA256  $KERNEL.part" | sha256sum -c --quiet || { rm -f "$KERNEL.part"; echo "build-guest-image.sh: kernel sha256 mismatch" >&2; exit 1; }
  mv -f "$KERNEL.part" "$KERNEL"
fi

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
  rm -rf --one-file-system "$out"
  mv "$stage" "$out"
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
  differ=
  for f in image.json vmlinux rootfs.squashfs; do
    cmp "$OUT/$f" "$OUT.verify/$f" || differ="$differ $f"
  done
  if [ -n "$differ" ]; then
    echo "build-guest-image.sh: --verify: not reproducible:$differ (kept $OUT.verify)" >&2
    exit 1
  fi
  rm -rf --one-file-system "$OUT.verify"
  echo "build-guest-image.sh: --verify: two builds are byte-identical" >&2
fi

# The digest, exactly as the registry computes it: register into a scratch home.
cargo build -q --locked -p agentos-cli
scratch_dir home /var/tmp/agentos-image-home.XXXXXX
"$REPO/target/debug/agentos" --home "$home" image register "$OUT"
