#!/usr/bin/env bash
set -u
cd /Users/bb16/code/footnote/footnote/.claude/worktrees/x-b1ee
while true; do
  R=$(gh api repos/bllshttng/footnote/commits/4d1751366/check-runs --jq '[.check_runs[] | select(.name | test("cargo")) | .status] | unique' 2>/dev/null)
  if [ "$R" = '["completed"]' ]; then
    C=$(gh api repos/bllshttng/footnote/commits/4d1751366/check-runs --jq '[.check_runs[] | select(.name | test("cargo")) | .conclusion] | unique' 2>/dev/null)
    echo "cargo leg completed: $C"
    exit 0
  fi
  sleep 120
done
