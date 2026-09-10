#!/usr/bin/env bash
# Repo-wide Python static step. CI, fno doctor test, and the merge-result probe run this one file.
# $1 is the cli directory to check (default: cli next to this repo root).
set -euo pipefail
export NO_COLOR=1
cli_dir="${1:-$(cd "$(dirname "$0")/../.." && pwd)/cli}"
cd "$cli_dir"
python_files="$(find src -type f -name '*.py' -print | wc -l | tr -d '[:space:]')"
test "$python_files" -gt 0
${RUFF:-uv run ruff} check --no-respect-gitignore --color=never --output-format=concise src/
${MYPY:-uv run mypy} --no-color-output src/
echo "python-static: checked ${python_files} Python files with ruff + mypy"
