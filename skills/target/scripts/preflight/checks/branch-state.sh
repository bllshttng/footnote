#!/usr/bin/env bash
# branch-state.sh - Check current git branch for dangerous states
# Contract: stdout one line "branch-state {pass|fail|warn|unknown} {message}"
# Exit: always 0

set -euo pipefail

BRANCH=$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")

if [[ -z "$BRANCH" ]]; then
    echo "branch-state unknown not in a git repository"
    exit 0
fi

if [[ "$BRANCH" == "HEAD" ]]; then
    echo "branch-state fail detached HEAD state - not on a named branch"
    exit 0
fi

# Dangerous branch names - working on these directly is risky
DANGEROUS_BRANCHES="main master prod production release stable"
for dangerous in $DANGEROUS_BRANCHES; do
    if [[ "$BRANCH" == "$dangerous" ]]; then
        echo "branch-state fail on protected branch '$BRANCH' - create a feature branch before making changes"
        exit 0
    fi
done

# Warn on branches that look like release branches (v1.2, release/*, hotfix/*)
if echo "$BRANCH" | grep -qE "^(release|hotfix|v[0-9])"; then
    echo "branch-state warn on release/hotfix branch '$BRANCH' - ensure this is intentional"
    exit 0
fi

echo "branch-state pass on branch '$BRANCH'"
exit 0
