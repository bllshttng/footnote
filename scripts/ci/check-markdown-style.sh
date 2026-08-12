#!/usr/bin/env bash
# check-markdown-style.sh - CI gate: ADDED markdown lines must pass the style rules.
#
# Scope: docs/, skills/, agents/. ADDED lines only, via `git diff -U0 <base>...HEAD`,
# never whole files: every one of those trees already breaks the rules today, so
# a whole-file gate is unlandable and would force a full rewrite on a one-line
# typo. This is the same ratchet shape the repo runs for LOC.
#
# The verb owns the added-line extraction, the per-file style-exception scan, and
# the absence guard. That guard used to fire on a bare zero, which swept in every
# legitimate zero: a pure rename, a deletion-only trim, a mode change. It now
# compares against git's OWN per-path added-line count and fires only where the
# two disagree, which is a parser failure in the gate rather than anything the
# author can annotate. This script is the CI glue that resolves the runner and
# the base ref.
#
# Run: bash scripts/ci/check-markdown-style.sh [base-ref]
# Env: MARKDOWN_STYLE_BASE (default: the arg, else origin/main).
# Exit: 0 pass, 1 a style violation, 2 the checker could not run OR could not
# read lines git says exist. Both 2s mean the instrument failed, never the prose.

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
