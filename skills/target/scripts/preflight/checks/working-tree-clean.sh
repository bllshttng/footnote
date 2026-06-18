#!/usr/bin/env bash
# working-tree-clean.sh - Check for uncommitted changes in working tree
# Contract: stdout one line "working-tree-clean {pass|fail|warn|unknown} {message}"
# Exit: always 0 (failure encoded in stdout)
# Supports: .fno/preflight-ignore.txt allowlist

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo "")"

if [[ -z "$REPO_ROOT" ]]; then
    echo "working-tree-clean unknown not in a git repository"
    exit 0
fi

# Get status output
STATUS=$(git -C "$REPO_ROOT" status --porcelain 2>/dev/null || echo "")

if [[ -z "$STATUS" ]]; then
    echo "working-tree-clean pass clean working tree"
    exit 0
fi

# Load allowlist if it exists
ALLOWLIST="$REPO_ROOT/.fno/preflight-ignore.txt"
FILTERED_STATUS="$STATUS"

if [[ -f "$ALLOWLIST" ]]; then
    # Filter out lines whose filename or path appears in the allowlist
    while IFS= read -r pattern; do
        [[ -z "$pattern" || "$pattern" =~ ^# ]] && continue
        FILTERED_STATUS=$(echo "$FILTERED_STATUS" | grep -v "$pattern" || true)
    done < "$ALLOWLIST"
fi

# Always exclude .fno from the dirty check - it's preflight's own config
# dir. Match both the files-inside form (".fno/foo") AND the bare entry
# (".fno" with no trailing slash). In a worktree, .fno is a SYMLINK
# to the shared coordination dir, so `git status --porcelain` prints it as
# "?? .fno" (no trailing slash) and the slash-only pattern missed it,
# falsely failing preflight on every worktree run.
# Anchor `.fno` to a path-segment boundary so this excludes the
# `.fno` dir/symlink and files inside it, but NOT an unrelated path that
# merely ends in `.fno` (e.g. `my-project.fno`) - gh #402 HIGH.
# The boundary is whitespace-or-slash, NOT `(^|/)`: git status --porcelain
# prints `XY <path>` (e.g. `?? .fno`), so the path is preceded by the
# status separator space, and a `(^|/)` anchor would miss the bare symlink and
# re-introduce the false preflight failure this fix exists to prevent.
FILTERED_STATUS=$(printf '%s\n' "$FILTERED_STATUS" | grep -vE '([[:space:]]|/)\.fno(/|$)' || true)

if [[ -z "$FILTERED_STATUS" ]]; then
    echo "working-tree-clean pass clean working tree (allowlisted files excluded)"
    exit 0
fi

# Collect file names for context
FILE_COUNT=$(echo "$FILTERED_STATUS" | grep -c . || true)
# `cut -c4-` preserves paths with spaces; `awk '{print $2}'` would only capture the first word.
FILE_LIST=$(echo "$FILTERED_STATUS" | cut -c4- | head -5 | tr '\n' ' ' | sed 's/ $//')
if [[ $FILE_COUNT -gt 5 ]]; then
    FILE_LIST="$FILE_LIST ... (+$((FILE_COUNT - 5)) more)"
fi

echo "working-tree-clean fail $FILE_COUNT untracked/modified files: $FILE_LIST"
exit 0
