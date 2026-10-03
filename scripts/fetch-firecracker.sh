#!/bin/sh
# Downloads the pinned Firecracker release and installs `firecracker` and `jailer` into DEST
# (default build/firecracker/v1.17.0/), every byte sha256-verified; prints DEST/firecracker.
#
#   docker compose run --rm test sh scripts/fetch-firecracker.sh [DEST]
#
# The file names matter: the jailer names the chroot subdirectory after the Firecracker file
# name, and the CLI finds the jailer as `jailer` next to `firecracker`. A run with both files
# present and matching is a no-op; a file that does not match is replaced. The request is a
# plain https-only `curl` (no credentials, no custom headers).
set -eu

VERSION=v1.17.0
ARCH=x86_64
URL="https://github.com/firecracker-microvm/firecracker/releases/download/$VERSION/firecracker-$VERSION-$ARCH.tgz"
TGZ_SHA256=06094a1108ae9e82aa4c23a775aa92758f53f1175d422270d9d6162cb9ade558
FIRECRACKER_SHA256=99ad0f5cd0514a88aad0e9ae8cfdb3cc3b4ab9d190e1194602406c786b5de7a5
JAILER_SHA256=65ef226e96f0ceda55ba643f445801ef2cc0ea667ef67cad8ac4f406c9c8434f

[ $# -le 1 ] || { echo "usage: scripts/fetch-firecracker.sh [DEST]" >&2; exit 2; }
REPO=$(cd "$(dirname "$0")/.." && pwd -P)
DEST=${1:-$REPO/build/firecracker/$VERSION}

matches() { [ -f "$1" ] && echo "$2  $1" | sha256sum -c --status; }

# What this script creates under $REPO/build belongs to the owner of the checkout (the host
# user), not to the container's root, so `rm -rf build` and `git clean -fdx` work on the
# host. Paths outside $REPO/build (a DEST elsewhere) are never touched.
own_under_build() {
  owner=$(stat -c %u:%g "$REPO")
  for p in "$REPO/build" "$REPO/build/firecracker" "$DEST" "$DEST/firecracker" "$DEST/jailer"; do
    rp=$(realpath -m "$p")
    case "$rp" in "$REPO/build" | "$REPO/build/"*) [ -e "$rp" ] && chown -h "$owner" "$rp" ;; esac
  done
  return 0
}

if matches "$DEST/firecracker" "$FIRECRACKER_SHA256" && matches "$DEST/jailer" "$JAILER_SHA256" &&
  [ -x "$DEST/firecracker" ] && [ -x "$DEST/jailer" ]; then
  own_under_build
  echo "$DEST/firecracker"
  exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
trap 'exit 130' INT TERM
echo "fetch-firecracker.sh: downloading $URL" >&2
curl -fsSL --proto '=https' --proto-redir '=https' -o "$TMP/release.tgz" "$URL"
echo "$TGZ_SHA256  $TMP/release.tgz" | sha256sum -c --quiet
DIR="release-$VERSION-$ARCH"
tar -xzf "$TMP/release.tgz" -C "$TMP" "$DIR/firecracker-$VERSION-$ARCH" "$DIR/jailer-$VERSION-$ARCH"
echo "$FIRECRACKER_SHA256  $TMP/$DIR/firecracker-$VERSION-$ARCH" | sha256sum -c --quiet
echo "$JAILER_SHA256  $TMP/$DIR/jailer-$VERSION-$ARCH" | sha256sum -c --quiet

mkdir -p "$DEST"
# Installed under a temporary name, then renamed: DEST never holds a partial binary.
install -m 0755 "$TMP/$DIR/firecracker-$VERSION-$ARCH" "$DEST/.firecracker.part"
install -m 0755 "$TMP/$DIR/jailer-$VERSION-$ARCH" "$DEST/.jailer.part"
mv -f "$DEST/.firecracker.part" "$DEST/firecracker"
mv -f "$DEST/.jailer.part" "$DEST/jailer"
own_under_build
echo "$DEST/firecracker"
