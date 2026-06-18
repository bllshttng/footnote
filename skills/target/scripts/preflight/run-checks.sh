#!/usr/bin/env bash
# run-checks.sh - target-preflight orchestrator
# Runs all check scripts in checks/ directory, aggregates results, emits report.
#
# Output:
#   stdout: one line per check with glyph, plus JSON summary on last line
#   exit 0: all checks pass or warn (or unknown)
#   exit 1: any check fails
#
# Environment:
#   PREFLIGHT_CHECKS_DIR - override checks directory (for testing)
#   PREFLIGHT_SESSION_ID - session id for artifact naming

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKS_DIR="${PREFLIGHT_CHECKS_DIR:-$SCRIPT_DIR/checks}"
SESSION_ID="${PREFLIGHT_SESSION_ID:-$(date +%Y%m%dT%H%M%SZ)-$$}"

PASSED=0
FAILED=0
WARNED=0
UNKNOWN=0
FAILED_CHECKS=()

# Emit a preflight check result line with glyph
emit_line() {
    local status="$1"
    local name="$2"
    local message="$3"
    case "$status" in
        pass)    echo "  ✓ $name: $message" ;;
        fail)    echo "  ✗ $name: $message" ;;
        warn)    echo "  ⚠ $name: $message" ;;
        unknown) echo "  ? $name: $message" ;;
        *)       echo "  ? $name: $message (unknown status: $status)" ;;
    esac
}

# Run a single check script and parse its output
run_check() {
    local check_script="$1"
    local check_name
    check_name="$(basename "$check_script" .sh)"

    # Check is executable
    if [[ ! -x "$check_script" ]]; then
        emit_line "unknown" "$check_name" "check script not executable"
        ((UNKNOWN++)) || true
        return 0
    fi

    # Run the check (it must always exit 0)
    # Capture stderr separately so bug diagnostics are surfaced in the report.
    # Use a function-scoped trap so a SIGINT mid-check does not leak the tmp file.
    local output
    local stderr_buf
    stderr_buf="$(mktemp)"
    trap 'rm -f "$stderr_buf"' RETURN INT TERM
    local check_rc=0
    output=$(bash "$check_script" 2>"$stderr_buf") || check_rc=$?
    if [[ $check_rc -ne 0 ]]; then
        local err_first=""
        if [[ -s "$stderr_buf" ]]; then
            err_first=" (stderr: $(head -1 "$stderr_buf"))"
        fi
        rm -f "$stderr_buf"
        trap - RETURN INT TERM
        emit_line "unknown" "$check_name" "check script exited non-zero (bug in check)${err_first}"
        ((UNKNOWN++)) || true
        return 0
    fi
    rm -f "$stderr_buf"
    trap - RETURN INT TERM

    # Parse the first line: "check-name status message..."
    local line
    line=$(echo "$output" | head -1)
    local reported_status
    reported_status=$(echo "$line" | awk '{print $2}')
    local reported_message
    reported_message=$(echo "$line" | cut -d' ' -f3-)

    case "$reported_status" in
        pass)
            emit_line "pass" "$check_name" "$reported_message"
            ((PASSED++)) || true
            ;;
        fail)
            emit_line "fail" "$check_name" "$reported_message"
            ((FAILED++)) || true
            FAILED_CHECKS+=("$check_name")
            ;;
        warn)
            emit_line "warn" "$check_name" "$reported_message"
            ((WARNED++)) || true
            ;;
        unknown)
            emit_line "unknown" "$check_name" "$reported_message"
            ((UNKNOWN++)) || true
            ;;
        *)
            # Empty/whitespace or unrecognized first line: treat as fail (fail-loud contract).
            local first_snippet="${line:0:80}"
            emit_line "fail" "$check_name" "check produced no parseable output (first line: '${first_snippet}')"
            ((FAILED++)) || true
            FAILED_CHECKS+=("$check_name")
            ;;
    esac
    return 0
}

echo "target-preflight: running environment checks..."
echo ""

# Run all checks in the checks directory
if [[ -d "$CHECKS_DIR" ]]; then
    for check_script in "$CHECKS_DIR"/*.sh; do
        [[ -e "$check_script" ]] || continue
        run_check "$check_script"
    done
else
    echo "  ? no checks directory found at $CHECKS_DIR"
    ((UNKNOWN++)) || true
fi

echo ""

# Summary line
TOTAL=$((PASSED + FAILED + WARNED + UNKNOWN))
if [[ $FAILED -gt 0 ]]; then
    SUMMARY_MSG="FAILED: $FAILED check(s) failed"
    echo "preflight: $SUMMARY_MSG"
else
    SUMMARY_MSG="OK: all checks passed or warned"
    echo "preflight: $SUMMARY_MSG"
fi

# JSON summary on the last line (machine-readable)
FAILED_LIST=$(printf '"%s",' "${FAILED_CHECKS[@]+"${FAILED_CHECKS[@]}"}")
FAILED_LIST="[${FAILED_LIST%,}]"
echo "{\"passed\":$PASSED,\"failed\":$FAILED,\"warned\":$WARNED,\"unknown\":$UNKNOWN,\"total\":$TOTAL,\"failed_checks\":$FAILED_LIST}"

# Exit non-zero if any check failed
if [[ $FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
