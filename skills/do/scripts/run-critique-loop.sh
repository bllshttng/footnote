#!/usr/bin/env bash
# run-critique-loop.sh - testable shell port of frontend-executor's inner loop.
#
# This is the MECHANICAL portion of the critique loop, extracted from
# agents/frontend-executor.md so it can be tested without spawning a real
# subagent. The actual frontend-executor agent should produce output
# semantically identical to this script (same termination conditions, same
# parse rules, same return-shape fields). Drift between the two is a bug.
#
# The loop:
#   1. Run the impeccable command (via $IMPECCABLE_CMD - in tests, the stub).
#   2. Run impeccable critique.
#   3. Parse score and next-subcommand.
#   4. If score >= threshold, exit SUCCESS.
#   5. If iteration >= max, exit FAILED with reason=max_iterations_reached.
#   6. Else loop with parsed next-subcommand.
#
# Inputs (env):
#   IMPECCABLE_CMD       - command to invoke (default: '/impeccable').
#                          For tests, set to the stub path. The first arg
#                          passed is the subcommand ('craft', 'critique', ...).
#   CRITIQUE_THRESHOLD   - score gate (default 35).
#   MAX_ITER             - per-task ceiling (default 8).
#   STUB_SCORE_SEQUENCE  - passed through to the stub command.
#   STUB_NEXT_SUBCOMMAND - passed through to the stub command.
#   STUB_INVOCATION_LOG  - passed through to the stub command.
#
# Output: stdout in frontend-executor's RESULT contract shape, one field
# per line.

set -uo pipefail

IMPECCABLE_CMD="${IMPECCABLE_CMD:-/impeccable}"
CRITIQUE_THRESHOLD="${CRITIQUE_THRESHOLD:-35}"
MAX_ITER="${MAX_ITER:-8}"
TASK_ID="${TASK_ID:-test-task}"

iteration=0
last_score=0
subcommand="craft"
subcommands_run=""
final_result=""

while :; do
    iteration=$(( iteration + 1 ))

    # 1. Run the current subcommand (craft or whatever critique recommended).
    subcommands_run="${subcommands_run}${subcommands_run:+,}${subcommand}"
    # Split IMPECCABLE_CMD into argv so callers can pass multi-word commands
    # like "bash tests/operator/_impeccable_stub.sh" without quoting issues.
    # Note: read -ra is whitespace-split only and does NOT honor shell quoting,
    # so paths with spaces in IMPECCABLE_CMD will mis-parse. Use a wrapper
    # script if your /impeccable lives in a path with spaces.
    read -ra _IMP_ARGV <<< "$IMPECCABLE_CMD"

    # Helper: invoke a subcommand once, capturing stdout and stderr into
    # two files so a single invocation surfaces both streams. Calling
    # /impeccable twice (once for stdout, once for stderr) would double
    # cost in production AND advance any stateful counter in test stubs.
    sub_out_tmp="$(mktemp)"
    sub_err_tmp="$(mktemp)"
    "${_IMP_ARGV[@]}" "$subcommand" >"$sub_out_tmp" 2>"$sub_err_tmp"
    sub_rc=$?
    sub_stderr="$(cat "$sub_err_tmp")"
    rm -f "$sub_out_tmp" "$sub_err_tmp"
    if [[ $sub_rc -ne 0 ]]; then
        final_result="FAILED"
        sub_stderr_one_line="$(printf '%s' "$sub_stderr" | tr '\n' ' ' | head -c 240)"
        echo "RESULT: FAILED"
        echo "TASK: ${TASK_ID}"
        echo "ITERATIONS: ${iteration}"
        echo "FINAL_SCORE: ${last_score}/40"
        echo "SUBCOMMANDS_RUN: [${subcommands_run}]"
        echo "ERROR: impeccable subcommand '${subcommand}' exited rc=${sub_rc}: ${sub_stderr_one_line}"
        printf '%s\n' "$sub_stderr" >&2
        exit 0
    fi

    # 2. Run critique once, capture stdout + stderr separately, parse stdout.
    crit_out_tmp="$(mktemp)"
    crit_err_tmp="$(mktemp)"
    "${_IMP_ARGV[@]}" critique >"$crit_out_tmp" 2>"$crit_err_tmp"
    critique_rc=$?
    critique_out="$(cat "$crit_out_tmp")"
    critique_stderr="$(cat "$crit_err_tmp")"
    rm -f "$crit_out_tmp" "$crit_err_tmp"
    if [[ $critique_rc -ne 0 ]]; then
        final_result="FAILED"
        critique_stderr_one_line="$(printf '%s' "$critique_stderr" | tr '\n' ' ' | head -c 240)"
        echo "RESULT: FAILED"
        echo "TASK: ${TASK_ID}"
        echo "ITERATIONS: ${iteration}"
        echo "FINAL_SCORE: ${last_score}/40"
        echo "SUBCOMMANDS_RUN: [${subcommands_run}]"
        echo "ERROR: impeccable critique exited rc=${critique_rc}: ${critique_stderr_one_line}"
        printf '%s\n' "$critique_stderr" >&2
        exit 0
    fi

    # Parse score (regex: score:\s*NN/40, case-insensitive)
    score=$(printf '%s\n' "$critique_out" \
        | grep -iE 'score:[[:space:]]*[0-9]+/40' \
        | head -1 \
        | sed -E 's/.*[Ss]core:[[:space:]]*([0-9]+)\/40.*/\1/' \
        || echo "")
    if [[ -z "$score" || ! "$score" =~ ^[0-9]+$ ]]; then
        echo "run-critique-loop: WARN: critique score unparseable, treating as 0" >&2
        score=0
    fi
    last_score="$score"

    # Parse next-subcommand
    next=$(printf '%s\n' "$critique_out" \
        | grep -iE 'next.{0,5}subcommand:[[:space:]]*[a-z_-]+' \
        | head -1 \
        | sed -E 's/.*[Nn]ext.{0,5}subcommand:[[:space:]]*([a-z_-]+).*/\1/' \
        || echo "")
    if [[ -z "$next" ]]; then
        echo "run-critique-loop: WARN: next subcommand unparseable, defaulting to craft" >&2
        next="craft"
    fi

    # 3. Convergence check.
    if [[ "$score" -ge "$CRITIQUE_THRESHOLD" ]]; then
        final_result="SUCCESS"
        break
    fi

    # 4. Ceiling check.
    if [[ "$iteration" -ge "$MAX_ITER" ]]; then
        final_result="FAILED"
        echo "RESULT: FAILED"
        echo "TASK: ${TASK_ID}"
        echo "ITERATIONS: ${iteration}"
        echo "FINAL_SCORE: ${last_score}/40"
        echo "SUBCOMMANDS_RUN: [${subcommands_run}]"
        echo "ERROR: max_iterations_reached"
        exit 0
    fi

    subcommand="$next"
done

echo "RESULT: SUCCESS"
echo "TASK: ${TASK_ID}"
echo "ITERATIONS: ${iteration}"
echo "FINAL_SCORE: ${last_score}/40"
echo "SUBCOMMANDS_RUN: [${subcommands_run}]"
