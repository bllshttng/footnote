#!/usr/bin/env bash
# codemap-fresh.sh - Check if .fno/codemap.md is within 24 hours
# Contract: stdout one line "codemap-fresh {pass|fail|warn|unknown} {message}"
# Exit: always 0

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
CODEMAP="$REPO_ROOT/.fno/codemap.md"

if [[ ! -f "$CODEMAP" ]]; then
    echo "codemap-fresh warn codemap not found at .fno/codemap.md (run: /target to generate)"
    exit 0
fi

# Get file modification time and compare to 24 hours ago
NOW=$(date +%s)
FILE_MTIME=$(stat -f %m "$CODEMAP" 2>/dev/null || stat -c %Y "$CODEMAP" 2>/dev/null || echo "0")
AGE_SECONDS=$((NOW - FILE_MTIME))
AGE_HOURS=$((AGE_SECONDS / 3600))
MAX_AGE_SECONDS=86400  # 24 hours

if [[ $AGE_SECONDS -le $MAX_AGE_SECONDS ]]; then
    echo "codemap-fresh pass codemap is ${AGE_HOURS}h old (within 24h)"
    exit 0
fi

echo "codemap-fresh warn codemap is ${AGE_HOURS}h old (>24h) - consider refreshing with: fno codemap"
exit 0
