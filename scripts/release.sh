#!/bin/sh
# Builds the release tarball agentos-VERSION-x86_64-linux.tar.gz (and its .sha256) into
# build/release/: a static musl agentos, the pinned Firecracker and jailer, one guest image,
# the parser and duration profiles and the reference analyzer, MANIFEST.json and SHA256SUMS. The tarball is
# reproducible: sorted names, fixed times and owners, gzip without a timestamp.
#
#   docker compose run --rm test-kvm sh scripts/release.sh VERSION [IMAGE]
#
# Runs in test-kvm, which mounts the built guest images (scripts/build-guest-image.sh) and the
# fetched Firecracker (scripts/fetch-firecracker.sh). IMAGE defaults to agent-cli-py314-v1: guest
# protocol 2, a source-built, provenanced kernel and the pinned agent CLI binary. The older
# recipes speak protocol 1 and are refused by this release's controller.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$REPO"
[ "$#" -ge 1 ] && [ "$#" -le 2 ] || { echo 'usage: scripts/release.sh VERSION [IMAGE]' >&2; exit 2; }
version=$1
image=${2:-agent-cli-py314-v1}
case "$version" in *[!0-9A-Za-z.+-]* | '' | -*) echo "invalid version $version" >&2; exit 2;; esac
case "$image" in *[!0-9A-Za-z.-]* | '' | -*) echo "invalid image $image" >&2; exit 2;; esac
fc="$REPO/build/firecracker/v1.17.0"
[ -x "$fc/firecracker" ] && [ -x "$fc/jailer" ] || { echo "fetch Firecracker first: sh scripts/fetch-firecracker.sh" >&2; exit 1; }
[ -f "build/guest-images/$image/image.json" ] || { echo "build the $image guest image first" >&2; exit 1; }
GIT="git -c safe.directory=$REPO -C $REPO"
commit=$($GIT rev-parse HEAD)
# A release is built only from a committed tree: no modified and no untracked file (ignored
# build output is fine), so its commit says exactly what it contains.
if [ -n "$($GIT status --porcelain)" ]; then
  echo 'release.sh: the tree has modified or untracked files; commit or remove them first:' >&2
  $GIT status --porcelain >&2
  exit 1
fi
epoch=$($GIT log -1 --format=%ct)

cargo build --locked --release -q -p agentos-cli --target x86_64-unknown-linux-musl
bin="$REPO/target/x86_64-unknown-linux-musl/release/agentos"
if readelf -l "$bin" | grep -q 'Requesting program interpreter'; then echo 'agentos is not static' >&2; exit 1; fi

name="agentos-$version-x86_64-linux"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
stage="$scratch/$name"
mkdir -p "$stage/bin" "$stage/images" "$stage/profiles" "$stage/components"
install -m 0755 "$bin" "$stage/bin/agentos"
install -m 0755 "$fc/firecracker" "$fc/jailer" "$stage/bin/"
cp -R "build/guest-images/$image" "$stage/images/$image"
cp -R fixtures/profiles/parser-checks-v1 fixtures/profiles/duration-checks-v1 "$stage/profiles/"
cp -R fixtures/components/repo-analyzer-v1 "$stage/components/"
# The registry digests, computed by the release's own agentos.
home="$scratch/home"
digest() { "$stage/bin/agentos" --home "$home" "$1" register "$2" | python3 -c 'import json, sys; print(json.load(sys.stdin)["digest"])'; }
image_digest=$(digest image "$stage/images/$image")
profile_digest=$(digest profile "$stage/profiles/parser-checks-v1")
duration_digest=$(digest profile "$stage/profiles/duration-checks-v1")
component_digest=$(digest component "$stage/components/repo-analyzer-v1")
# The versions the code itself reports (protocols, policies, runtimes), the toolchain, and the
# image's own provenance record.
"$stage/bin/agentos" version > "$scratch/versions.json"
rustc --version > "$scratch/rustc"
python3 - "$stage/MANIFEST.json" "$scratch/versions.json" "$scratch/rustc" "$stage/images/$image/image.json" <<PY
import json, sys
out, versions, rustc, image_json = sys.argv[1:]
image = json.load(open(image_json))
json.dump({
    "version": "$version", "commit": "$commit", "arch": "x86_64",
    "firecracker": "v1.17.0",
    "rust": open(rustc).read().strip(),
    "versions": json.load(open(versions)),
    "image": {"id": "$image", "digest": "$image_digest",
              "kernel_sha256": image.get("kernel_sha256"),
              "kernel_build": image.get("kernel_build"),
              "interpreter": image.get("interpreter")},
    "profile": {"id": "parser-checks-v1", "digest": "$profile_digest"},
    "profiles": [{"id": "parser-checks-v1", "digest": "$profile_digest"},
                 {"id": "duration-checks-v1", "digest": "$duration_digest"}],
    "component": {"id": "repo-analyzer-v1", "digest": "$component_digest"},
}, open(out, "w"), separators=(",", ":"), sort_keys=True)
PY
(cd "$stage" && find . -type f ! -name SHA256SUMS | LC_ALL=C sort | xargs sha256sum > SHA256SUMS)
mkdir -p build/release
out="build/release/$name.tar.gz"
tar --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner --mode='u+rwX,go+rX,go-w' \
  -C "$scratch" -cf - "$name" | gzip -9n > "$out.part"
mv "$out.part" "$out"
sha256sum "$out" | cut -d' ' -f1 > "$out.sha256"
if [ "$(id -u)" -eq 0 ]; then chown -R "$(stat -c %u:%g "$REPO")" build/release; fi
echo "$out $(cat "$out.sha256")"
