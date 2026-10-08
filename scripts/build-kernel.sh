#!/bin/sh
# Builds the guest kernel from pinned source (guest/kernel/source.lock) with Firecracker's
# config fragments, reproducibly, into OUT_DIR/vmlinux and OUT_DIR/kernel.json (provenance).
# Run in the kernel-builder service: docker compose run --rm kernel-builder sh scripts/build-kernel.sh OUT [--verify]
#   1. the source tarball, cached in build/kernels/ and sha256-checked on every use;
#   2. the fragments, sha256-checked, concatenated in Firecracker's order, `make olddefconfig`;
#      the result must equal the committed guest/kernel/linux-<version>.config;
#   3. `make vmlinux` with fixed build metadata (timestamp from the lock, user, host, version).
# --verify builds twice in independent directories and fails unless both vmlinux are identical.
# --print-config writes the resolved config to OUT_DIR/config (to update the committed one).
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
[ "$#" -ge 1 ] || { echo 'usage: scripts/build-kernel.sh OUT_DIR [--verify|--print-config]' >&2; exit 2; }
out=$1
mode=${2:-build}
case "$mode" in build|--verify|--print-config) ;; *) echo "unknown option: $mode" >&2; exit 2;; esac
lock="$REPO/guest/kernel/source.lock"
field() { python3 -c 'import json, sys; v = json.load(open(sys.argv[1]))[sys.argv[2]]; print("\n".join(v) if isinstance(v, list) else v)' "$lock" "$1"; }
url=$(field url); sha=$(field sha256); version=$(field version); epoch=$(field source_date_epoch)
tarball="$REPO/build/kernels/linux-$version.tar.xz"
mkdir -p "$REPO/build/kernels"
if ! { [ -f "$tarball" ] && echo "$sha  $tarball" | sha256sum -c --status; }; then
  curl -fsSL --proto '=https' --proto-redir '=https' -o "$tarball.part" "$url"
  echo "$sha  $tarball.part" | sha256sum -c --quiet || { rm -f "$tarball.part"; echo 'kernel source sha256 mismatch' >&2; exit 1; }
  mv "$tarball.part" "$tarball"
fi
# The fragments, each checked against the lock, in order.
frags=""
i=1
for f in $(field fragments); do
  want=$(field fragment_sha256 | sed -n "${i}p")
  echo "$want  $REPO/guest/kernel/$f" | sha256sum -c --quiet || { echo "fragment $f does not match the lock" >&2; exit 1; }
  frags="$frags $REPO/guest/kernel/$f"
  i=$((i + 1))
done
committed="$REPO/guest/kernel/linux-$version.config"

# build DIR: one complete build in DIR (created fresh); leaves DIR/linux-$version/vmlinux.
build() {
  dir=$1
  rm -rf "$dir"
  mkdir -p "$dir"
  tar -xJf "$tarball" -C "$dir"
  tree="$dir/linux-$version"
  # shellcheck disable=SC2086 # the fragment list is space separated paths without spaces
  awk 1 $frags > "$tree/.config"
  make -s -C "$tree" olddefconfig
  if [ "$mode" = --print-config ]; then
    mkdir -p "$out"; cp "$tree/.config" "$out/config"; echo "resolved config written to $out/config"; exit 0
  fi
  cmp "$tree/.config" "$committed" || { echo "olddefconfig no longer resolves to $committed" >&2; exit 1; }
  KBUILD_BUILD_TIMESTAMP=$(date -u -d "@$epoch" '+%Y-%m-%d %H:%M:%S UTC') \
  KBUILD_BUILD_USER=agentos KBUILD_BUILD_HOST=agentos KBUILD_BUILD_VERSION=1 \
    make -s -C "$tree" -j"$(nproc)" vmlinux
}

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
build "$scratch/a"
if [ "$mode" = --verify ]; then
  # A second, independent tree at another path: the output must not depend on either.
  build "$scratch/second-build"
  cmp "$scratch/a/linux-$version/vmlinux" "$scratch/second-build/linux-$version/vmlinux" ||
    { echo 'two kernel builds differ' >&2; exit 1; }
  echo 'two kernel builds are byte-identical'
fi
mkdir -p "$out"
cp "$scratch/a/linux-$version/vmlinux" "$out/vmlinux"
python3 - "$out/kernel.json" "$version" "$sha" "$(sha256sum "$committed" | cut -d' ' -f1)" \
  "$(gcc --version | head -n1)" "$(ld --version | head -n1)" "$(sha256sum "$out/vmlinux" | cut -d' ' -f1)" <<'PY'
import json, sys
path, version, source, config, gcc, ld, vmlinux = sys.argv[1:]
json.dump({"version": version, "source_sha256": source, "config_sha256": config,
           "gcc": gcc, "binutils": ld, "vmlinux_sha256": vmlinux}, open(path, "w"), sort_keys=True)
PY
if [ "$(id -u)" -eq 0 ]; then chown -R "$(stat -c %u:%g "$REPO")" "$out" "$REPO/build/kernels"; fi
cat "$out/kernel.json"; echo
