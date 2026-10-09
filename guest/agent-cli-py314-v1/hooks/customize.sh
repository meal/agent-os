#!/bin/sh
# mmdebstrap customize hook for the agent-cli-py314-v1 guest image: the python-stdlib-py314-v2
# image, plus the Claude Code native binary pinned in agent-cli.lock.
# Called by scripts/build-guest-image.sh as: sh hooks/customize.sh ROOT AGENTOS_GUEST_BIN
#   ROOT               the chroot mmdebstrap built ("$1" of the hook)
#   AGENTOS_GUEST_BIN  the static musl agentos-guest to install as PID 1
# Runs as root in the build container with SOURCE_DATE_EPOCH exported. Fails closed: any
# mismatch (lock shape, tarball sha256, binary sha256, a library the binary needs) stops the build.
set -eu
ROOT=${1:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}
GUEST_BIN=${2:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}
RECIPE=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$RECIPE/../.." && pwd)
die() { echo "customize.sh: $*" >&2; exit 1; }

# The Python image first: the same scaffold, interpreter and checks as python-stdlib-py314-v2.
sh "$REPO/guest/python-stdlib-py314-v2/hooks/customize.sh" "$ROOT" "$GUEST_BIN"

. "$RECIPE/agent-cli.lock"
# A lock the hook does not understand is refused before anything is downloaded.
is_sha256() { [ "${#1}" -eq 64 ] && [ -z "$(printf '%s' "$1" | tr -d '0-9a-f')" ]; }
[ "$AGENTOS_CLAUDE_CODE_PACKAGE" = "@anthropic-ai/claude-code-linux-x64" ] || die "agent-cli.lock: unexpected package"
case "$AGENTOS_CLAUDE_CODE_VERSION" in ''|*[!0-9.]*) die "agent-cli.lock: bad version" ;; esac
[ "$AGENTOS_CLAUDE_CODE_URL" = "https://registry.npmjs.org/$AGENTOS_CLAUDE_CODE_PACKAGE/-/claude-code-linux-x64-$AGENTOS_CLAUDE_CODE_VERSION.tgz" ] ||
  die "agent-cli.lock: the URL is not the npm tarball of $AGENTOS_CLAUDE_CODE_VERSION"
is_sha256 "$AGENTOS_CLAUDE_CODE_TARBALL_SHA256" || die "agent-cli.lock: bad tarball sha256"
is_sha256 "$AGENTOS_CLAUDE_CODE_BINARY_SHA256" || die "agent-cli.lock: bad binary sha256"
[ "${#AGENTOS_CLAUDE_CODE_INTEGRITY}" -eq 95 ] && case "$AGENTOS_CLAUDE_CODE_INTEGRITY" in sha512-*) ;; *) false;; esac || die "agent-cli.lock: bad integrity (npm sha512, 95 characters)"

# Downloads and the extracted member live outside ROOT (the image) and outside the checkout
# (files written there as root would belong to root on the host).
WORK=$(mktemp -d /var/tmp/agentos-agent-cli.XXXXXX)
trap 'rm -rf "$WORK"' EXIT
curl -fsSL --proto '=https' --proto-redir '=https' -o "$WORK/pkg.tgz" "$AGENTOS_CLAUDE_CODE_URL"
echo "$AGENTOS_CLAUDE_CODE_TARBALL_SHA256  $WORK/pkg.tgz" | sha256sum -c --quiet ||
  die "tarball sha256 does not match agent-cli.lock"
# Only the binary is taken from the package; the tarball is checked above, before tar reads it.
tar -xzf "$WORK/pkg.tgz" -C "$WORK" --no-same-owner package/claude
echo "$AGENTOS_CLAUDE_CODE_BINARY_SHA256  $WORK/package/claude" | sha256sum -c --quiet ||
  die "binary sha256 does not match agent-cli.lock"

install -d -m 0755 -o root -g root "$ROOT/opt/agent-cli"
install -m 0755 -o root -g root "$WORK/package/claude" "$ROOT/opt/agent-cli/claude"

# The binary is dynamically linked against glibc (the image's libc6). The loader lists what it
# needs without running the program; any library it cannot find fails the build.
libs=$(chroot "$ROOT" /lib64/ld-linux-x86-64.so.2 --list /opt/agent-cli/claude 2>&1) ||
  die "the loader cannot resolve /opt/agent-cli/claude: $libs"
case "$libs" in *"not found"*) die "/opt/agent-cli/claude needs a missing library: $libs" ;; esac
