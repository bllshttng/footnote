#!/usr/bin/env bash
# _impeccable_stub.sh - deterministic stand-in for /impeccable in tests.
#
# Reads its behavior from environment variables so tests can drive the
# critique loop without /impeccable being available. Output mimics the
# format frontend-executor's parser regex expects:
#
#   score: <NN>/40
#   next-subcommand: <name>
#
# Inputs:
#   STUB_SCORE_SEQUENCE  - space-separated scores returned in order, one
#                          per critique invocation. After the list is
#                          exhausted, the last value repeats.
#   STUB_NEXT_SUBCOMMAND - default 'craft'. Echoed verbatim.
#   STUB_INVOCATION_LOG  - file path to record each call (for assertions).
#
# Subcommand argument:
#   craft       - prints "/impeccable craft: complete" (no score)
#   critique    - prints score + next-subcommand line
#   anything-else - same as craft
#
# Side effect: increments the iteration counter stored in
# $STUB_INVOCATION_LOG so the next critique call returns the next score.
#
# Usage in tests:
#   STUB_SCORE_SEQUENCE="30 33 38" \
#     STUB_INVOCATION_LOG=/tmp/stub.log \
#     bash tests/operator/_impeccable_stub.sh critique

set -uo pipefail

SUBCOMMAND="${1:-craft}"
STUB_SCORE_SEQUENCE="${STUB_SCORE_SEQUENCE:-30}"
STUB_NEXT_SUBCOMMAND="${STUB_NEXT_SUBCOMMAND:-craft}"
STUB_INVOCATION_LOG="${STUB_INVOCATION_LOG:-/dev/null}"

case "$SUBCOMMAND" in
    critique)
        # Determine which iteration index we're on by counting prior
        # critique entries in the log (zero-based). `grep -c` exits
        # non-zero with no match, so swallow the failure and default to 0
        # without leaking the fallback into the count.
        idx=0
        if [[ -f "$STUB_INVOCATION_LOG" && "$STUB_INVOCATION_LOG" != "/dev/null" ]]; then
            count=$(grep -c '^critique' "$STUB_INVOCATION_LOG" 2>/dev/null) || count=0
            idx="$count"
        fi

        # Pick score by index; clamp to last when out of range.
        # shellcheck disable=SC2206
        scores=( $STUB_SCORE_SEQUENCE )
        last=$(( ${#scores[@]} - 1 ))
        [[ $idx -gt $last ]] && idx=$last
        score="${scores[$idx]}"

        if [[ "$STUB_INVOCATION_LOG" != "/dev/null" ]]; then
            echo "critique" >> "$STUB_INVOCATION_LOG"
        fi

        echo "score: ${score}/40"
        echo "next-subcommand: ${STUB_NEXT_SUBCOMMAND}"
        ;;
    *)
        if [[ "$STUB_INVOCATION_LOG" != "/dev/null" ]]; then
            echo "$SUBCOMMAND" >> "$STUB_INVOCATION_LOG"
        fi
        echo "/impeccable ${SUBCOMMAND}: complete"
        ;;
esac
