#!/usr/bin/env bash
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
test -f cli/pyproject.toml
test -f cli/README.md
test -f cli/src/fno/__init__.py
test -f cli/src/fno/cli.py
# `gates` removed: the package was deleted by the control-plane collapse
# wedge (ab-d0337fbc); `claims` stands in as the structural probe.
for sub in state graph runtime worker events claims reality_check; do
  test -f "cli/src/fno/${sub}/__init__.py"
done
echo "PASS: skeleton complete"
