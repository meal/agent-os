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
# The check runs with PATH=/usr/bin:/bin (the guest agent's CHECK_PATH), so the interpreter
# is linked there. packages.txt installs no Debian Python: this is the only one in the image.
for debian in "$ROOT"/usr/bin/python3* "$ROOT"/usr/lib/python3*; do
  if [ -e "$debian" ]; then echo "customize.sh: unexpected Debian Python $debian" >&2; exit 1; fi
done
mkdir -p "$ROOT/opt/pyenv/versions"
cp -a "$PYENV_ROOT/versions/$AGENTOS_PYTHON_VERSION" "$ROOT/opt/pyenv/versions/"
ln -s "/opt/pyenv/versions/$AGENTOS_PYTHON_VERSION/bin/python3" "$ROOT/usr/bin/python3"
ln -s "/opt/pyenv/versions/$AGENTOS_PYTHON_VERSION/bin/python" "$ROOT/usr/bin/python"
[ "$(chroot "$ROOT" /usr/bin/env -i PATH=/usr/bin:/bin python3 --version)" = "Python $AGENTOS_PYTHON_VERSION" ]
