#!/usr/bin/env bash
# Validate a roadmap's task backlog for structural integrity.
#
# Usage:
#   validate-roadmap.sh [--roadmap-id ID]
#
# Checks: circular deps, dangling refs, scope issues, deferred cascades,
# orphaned tasks, duplicate titles. Wraps roadmap-tasks.py validate.
#
# Exit codes:
#   0 = PASS (0 errors, maybe warnings)
#   1 = FAIL (errors found — blocks execution)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROADMAP_TASKS="$SCRIPT_DIR/roadmap-tasks.py"

# Parse args
ROADMAP_ID=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --roadmap-id)
            ROADMAP_ID="$2"
            shift 2
            ;;
        *)
            echo "Usage: validate-roadmap.sh [--roadmap-id ID]" >&2
            exit 1
            ;;
    esac
done

echo "Validating roadmap${ROADMAP_ID:+ (id: $ROADMAP_ID)}..."
echo ""

# Build command
CMD=(python3 "$ROADMAP_TASKS" validate)
if [[ -n "$ROADMAP_ID" ]]; then
    CMD+=(--roadmap-id "$ROADMAP_ID")
fi

# Run validation, capture output and exit code
OUTPUT=$("${CMD[@]}" 2>&1) || EXIT_CODE=$?
EXIT_CODE=${EXIT_CODE:-0}

# Display output
echo "$OUTPUT"

# Count errors and warnings
ERRORS=$(echo "$OUTPUT" | grep -c "^ERROR:" 2>/dev/null || true)
WARNINGS=$(echo "$OUTPUT" | grep -c "^WARN:" 2>/dev/null || true)

echo ""
if [[ "$ERRORS" -gt 0 ]] || [[ "$EXIT_CODE" -ne 0 ]]; then
    echo "=== FAIL: validation failed (errors=$ERRORS, exit_code=$EXIT_CODE) ==="
    exit 1
else
    if [[ "$WARNINGS" -gt 0 ]]; then
        echo "=== PASS with $WARNINGS warning(s) ==="
    else
        echo "=== PASS ==="
    fi
    exit 0
fi
