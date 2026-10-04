#!/bin/sh
# Protected parser checks must agree under Debian's legacy interpreter and pinned pyenv.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
. "$REPO/runtime/python.lock"
[ "$(pyenv --version)" = "pyenv $AGENTOS_PYENV_VERSION" ]
[ "$(pyenv version-name)" = "$AGENTOS_PYTHON_VERSION" ]
[ "$(pyenv exec python --version)" = "Python $AGENTOS_PYTHON_VERSION" ]
[ "$(python3 --version)" = "Python $AGENTOS_PYTHON_VERSION" ]
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
for interpreter in /usr/bin/python3 "$PYENV_ROOT/versions/$AGENTOS_PYTHON_VERSION/bin/python3"; do
    mkdir "$scratch/repo"
    cp -R "$REPO/fixtures/parser-repo/." "$scratch/repo/"
    if "$interpreter" "$REPO/fixtures/profiles/parser-checks-v1/check_parser.py" "$scratch/repo" > "$scratch/result"; then
        echo "broken fixture unexpectedly passed: $interpreter" >&2
        exit 1
    else
        [ "$?" -eq 1 ]
    fi
    git -C "$scratch/repo" apply "$REPO/fixtures/parser-repo.fix.patch"
    "$interpreter" "$REPO/fixtures/profiles/parser-checks-v1/check_parser.py" "$scratch/repo"
    rm -rf "$scratch/repo"
done
echo "both interpreter acceptance checks passed"
