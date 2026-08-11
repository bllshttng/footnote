#!/usr/bin/env bash
# check-markdown-style.sh - CI gate: ADDED markdown lines must pass the style rules.
#
# Scope: docs/, skills/, agents/. ADDED lines only, via `git diff -U0 <base>...HEAD`,
# never whole files: every one of those trees already breaks the rules today, so
# a whole-file gate is unlandable and would force a full rewrite on a one-line
# typo. This is the same ratchet shape the repo runs for LOC.
#
# The verb owns the added-line extraction, the per-file style-exception scan, and
# the absence guard (a touched gated tree with zero inspected lines fails too:
# "no violations" and "the checker never ran" look identical from an exit code).
# This script is the CI glue that resolves the fno runner and the base ref.
#
# Run: bash scripts/ci/check-markdown-style.sh [base-ref]
# Env: MARKDOWN_STYLE_BASE (default: the arg, else origin/main).
# Exit: 0 pass, 1 a violation or a zero-line inspection on a touched tree, 2 the
# checker could not run.

set -euo pipefail

BASE="${1:-${MARKDOWN_STYLE_BASE:-origin/main}}"
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
cd "$REPO_ROOT/cli"

# Prefer the in-tree build (`uv run`) over an installed snapshot.
if command -v uv >/dev/null 2>&1; then
  FNO=(uv run fno-py)
elif command -v fno >/dev/null 2>&1; then
  FNO=(fno)
else
  echo "check-markdown-style: neither 'uv' nor 'fno' available" >&2
  exit 2
fi

"${FNO[@]}" lint style --surface markdown --diff-base "$BASE"
