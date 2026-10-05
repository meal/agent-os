#!/bin/sh
set -eu
case "${1:-all}" in all) pattern='test_*.py';; test_review|test_workflow|test_recovery) pattern="$1.py";; *) echo 'unknown browser suite' >&2; exit 2;; esac
[ "$#" -le 1 ] || exit 2
cargo build -p agentos-cli --locked
mkdir -p build/ui-evidence
/opt/agentos-ui-venv/bin/python -m unittest discover -s tests/ui -p "$pattern" -v
