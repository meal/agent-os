#!/bin/sh
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/../../.." && pwd)
ROOT=${1:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}
GUEST_BIN=${2:?usage: customize.sh ROOT AGENTOS_GUEST_BIN}
. "$REPO/runtime/python.lock"
[ "$(pyenv --version)" = "pyenv $AGENTOS_PYENV_VERSION" ]
[ "$(pyenv exec python --version)" = "Python $AGENTOS_PYTHON_VERSION" ]
# Preserve the legacy scaffold; install the new interpreter only in this new recipe.
sh "$REPO/guest/python-stdlib-v1/hooks/customize.sh" "$ROOT" "$GUEST_BIN"
mkdir -p "$ROOT/opt/pyenv/versions" "$ROOT/usr/local/bin"
cp -a "$PYENV_ROOT/versions/$AGENTOS_PYTHON_VERSION" "$ROOT/opt/pyenv/versions/"
ln -s "/opt/pyenv/versions/$AGENTOS_PYTHON_VERSION/bin/python3" "$ROOT/usr/local/bin/python3"
ln -s "/opt/pyenv/versions/$AGENTOS_PYTHON_VERSION/bin/python" "$ROOT/usr/local/bin/python"
[ "$(chroot "$ROOT" /usr/local/bin/python3 --version)" = "Python $AGENTOS_PYTHON_VERSION" ]
