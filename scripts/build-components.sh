#!/bin/sh
# Builds the analyzer components (components/) into fixtures/components/<name>.wasm:
# a release build for wasm32-unknown-unknown, then `wasm-tools component new`. Paths are
# remapped so the bytes do not depend on where the checkout lives. With --check, builds into
# a scratch directory and fails unless the result equals the committed fixtures.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
mode=${1:-build}
case "$mode" in build|--check) ;; *) echo 'usage: scripts/build-components.sh [--check]' >&2; exit 2;; esac
out="$REPO/fixtures/components"
target=$(mktemp -d)
trap 'rm -rf "$target"' EXIT
export RUSTFLAGS="--remap-path-prefix=$REPO=/agentos --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo -C debuginfo=0"
cargo build --manifest-path "$REPO/components/Cargo.toml" --locked --release \
  --target wasm32-unknown-unknown --target-dir "$target"
dest=$out
if [ "$mode" = --check ]; then dest="$target/check"; fi
mkdir -p "$dest"
for name in repo-analyzer hostile-analyzer; do
  core="$target/wasm32-unknown-unknown/release/$(echo "$name" | tr - _).wasm"
  wasm-tools component new "$core" -o "$dest/$name.wasm"
  wasm-tools validate --features component-model "$dest/$name.wasm"
done
if [ "$mode" = build ] && [ "$(id -u)" -eq 0 ]; then
  # Built by the container's root: give the fixtures back to the owner of the checkout.
  chown -R "$(stat -c %u:%g "$REPO")" "$out"
fi
if [ "$mode" = --check ]; then
  for name in repo-analyzer hostile-analyzer; do
    cmp "$dest/$name.wasm" "$out/$name.wasm" || { echo "$name.wasm differs from the committed fixture" >&2; exit 1; }
  done
  echo "components match the committed fixtures"
fi
