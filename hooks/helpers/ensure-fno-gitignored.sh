#!/usr/bin/env bash
# Ensure the project's .gitignore ignores .fno/ once a local .fno/ exists.
#
# Project-local .fno/ holds machine-specific session state (target-state.md,
# settings.yaml, ledger.json, claims/). It must never be committed: both to keep
# personal state out of an open-source repo and so a worktree's target-state.md
# cannot leak into project history. This is the universal housekeeping step -
# footnote's SessionStart hook calls it in every project the plugin runs in,
# regardless of which command first created .fno/.
#
# Idempotent + best-effort: no-op when .fno/ is already ignored, when there is
# no .fno/ yet, or outside a git work tree. Never fails the caller.
#
# Usage: ensure-fno-gitignored.sh [repo_root]   (defaults to the git toplevel)
set -uo pipefail

root="${1:-}"
if [[ -z "$root" ]]; then
    root=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
fi
[[ -n "$root" && -d "$root/.fno" ]] || exit 0

# Already covered by an ignore rule? Nothing to do. (A tracked .fno path would
# report "not ignored"; appending the rule is still correct - it stops NEW files
# leaking even if a prior leak needs `git rm --cached` to finish cleaning up.)
if git -C "$root" check-ignore -q .fno 2>/dev/null; then
    exit 0
fi

printf '\n# footnote local session state (machine-specific; never commit)\n.fno/\n' \
    >> "$root/.gitignore" 2>/dev/null || true
exit 0
