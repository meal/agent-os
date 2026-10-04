#!/bin/sh
# Compile before the Python test applies its 60-second demo execution limit.
set -eu
REPO=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
cd "$REPO"
cargo build --locked -p agentos-cli
exec pyenv exec python scripts/test-pages.py
