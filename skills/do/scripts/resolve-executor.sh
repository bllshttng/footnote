#!/usr/bin/env bash
# resolve-executor.sh - three-tier executor resolution shim.
#
# Reads its inputs from environment variables so the resolver is testable
# independent of the operator skill body. Echoes the resolved executor
# name to stdout. Diagnostics go to stderr.
#
# Inputs (env vars):
#   TASK_EXEC      - explicit executor on the task block (highest priority)
#   PLAN_EXEC      - explicit executor on the plan frontmatter
#   TASK_FILES     - newline-separated file list for surface inference
#   AUTO_ROUTE_FRONTEND - boolean (default true). Falsey values that disable
#                        inference: "false", "False", "FALSE", "0", "no", "No".
#                        YAML readers stringify booleans inconsistently, so
#                        the gate accepts the common spellings.
#
# Resolution chain (highest to lowest priority):
#   1. TASK_EXEC if set and non-empty
#   2. PLAN_EXEC if set and non-empty
#   3. Surface inference via fno.executor._surface (if AUTO_ROUTE_FRONTEND != false)
#   4. 'do' (default)
#
# Failure mode: an unrecognized explicit executor name (anything other than
# do|tdd|impeccable) logs a WARN to stderr and falls through to 'do'. This is
# the fail-closed behavior cited by AC1.5-FR.

set -uo pipefail

TASK_EXEC="${TASK_EXEC:-}"
PLAN_EXEC="${PLAN_EXEC:-}"
TASK_FILES="${TASK_FILES:-}"
AUTO_ROUTE_FRONTEND="${AUTO_ROUTE_FRONTEND:-true}"
# Attribution for the executor_resolved telemetry event (x-64cb); both are
# best-effort - empty when the caller does not set them, and the event still
# emits with empty strings.
TASK_ID="${TASK_ID:-}"
PLAN_PATH="${PLAN_PATH:-}"
# x-dbaf status-breakpoint coordinates for the task_started emit. All best-effort:
# empty TARGET_RUN/NODE_ID fall back to the manifest inside `fno event emit`.
TARGET_RUN="${TARGET_RUN:-}"
NODE_ID="${NODE_ID:-}"
TASK_TITLE="${TASK_TITLE:-}"

# KNOWN_EXECUTORS holds canonical names only. Aliases are normalized before
# the validation check (normalize_alias runs first), so adding an alias
# means only updating normalize_alias - KNOWN_EXECUTORS stays stable.
KNOWN_EXECUTORS="do impeccable"

is_known_executor() {
    local candidate="$1"
    case " $KNOWN_EXECUTORS " in
        *" $candidate "*) return 0 ;;
        *) return 1 ;;
    esac
}

normalize_alias() {
    case "$1" in
        tdd) echo "do" ;;
        *) echo "$1" ;;
    esac
}

is_falsey() {
    case "$1" in
        false|False|FALSE|0|no|No|NO|"") return 0 ;;
        *) return 1 ;;
    esac
}

# Best-effort telemetry: record the resolution decision so `fno backlog triage
# health` can fold routing-tier metrics. NEVER changes routing - stdout carries
# only the resolved value (echoed before this runs), and fno's own output is
# swallowed so it cannot corrupt that contract. On a missing/failing `fno` we
# print one stderr note and move on (AC5-ERR).
emit_resolution() {
    local resolved="$1" tier="$2" warn_bool="$3"
    if ! command -v fno >/dev/null 2>&1; then
        echo "resolve-executor: note: fno unavailable, skipped executor_resolved emit" >&2
        return 0
    fi
    local esc_task esc_plan esc_val
    esc_task="$(json_escape "$TASK_ID")"
    esc_plan="$(json_escape "$PLAN_PATH")"
    esc_val="$(json_escape "$resolved")"
    local data
    data="$(printf '{"task":"%s","plan_path":"%s","resolved":"%s","tier":"%s","warn_fallback":%s}' \
        "$esc_task" "$esc_plan" "$esc_val" "$tier" "$warn_bool")"
    if ! fno event emit -t executor_resolved -d "$data" >/dev/null 2>&1; then
        echo "resolve-executor: note: executor_resolved emit failed (non-fatal)" >&2
    fi
}

json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

# x-dbaf task_started boundary: this script IS the dispatch chokepoint (it runs
# once per task, right before the executor is invoked), so it is the natural
# emit site. Same non-corrupting contract as emit_resolution: runs after the
# stdout executor value is printed, swallows fno's output, never fails dispatch.
emit_task_started() {
    local resolved="$1"
    command -v fno >/dev/null 2>&1 || return 0
    local data
    data="$(printf '{"title":"%s","executor":"%s"}' \
        "$(json_escape "$TASK_TITLE")" "$(json_escape "$resolved")")"
    if ! fno event emit -t task_started -d "$data" \
        --run "$TARGET_RUN" --node "$NODE_ID" --task "$TASK_ID" >/dev/null 2>&1; then
        echo "resolve-executor: note: task_started emit failed (non-fatal)" >&2
    fi
}

tier_for() {
    case "$1" in
        task) echo "task-block" ;;
        plan) echo "plan-frontmatter" ;;
        inference) echo "surface-inference" ;;
        *) echo "default" ;;
    esac
}

resolve() {
    local source=""
    local value=""
    local warn_fired=0

    if [[ -n "$TASK_EXEC" ]]; then
        source="task"
        value="$TASK_EXEC"
    elif [[ -n "$PLAN_EXEC" ]]; then
        source="plan"
        value="$PLAN_EXEC"
    elif [[ -n "$TASK_FILES" ]] && ! is_falsey "$AUTO_ROUTE_FRONTEND"; then
        source="inference"
        # Surface inference is the in-package module fno.executor._surface (the
        # SINGLE source of truth, ported from the retired infer-task-executor.sh).
        # In a checkout, point PYTHONPATH at cli/src so the module imports
        # pre-install; otherwise rely on the installed `fno` package.
        local pkg_src
        pkg_src="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." 2>/dev/null && pwd)/cli/src"
        if [[ -f "${pkg_src}/fno/executor/_surface.py" ]]; then
            export PYTHONPATH="${pkg_src}${PYTHONPATH:+:${PYTHONPATH}}"
        fi
        # Trailing newline is required: the module reads stdin and drops empty
        # lines, so printf '%s\n' is safe even when TASK_FILES already ends in a
        # newline (the module just sees an extra empty line, which is filtered).
        # If the module is unavailable, value is empty and the is_known_executor
        # check below falls closed to 'do' (matching the old helper-missing path).
        value="$(printf '%s\n' "$TASK_FILES" | python3 -m fno.executor._surface 2>/dev/null)"
    else
        source="default"
        value="do"
    fi

    # Normalize before validating: aliases like 'tdd' map to canonical 'do'
    # before the KNOWN_EXECUTORS check, so KNOWN_EXECUTORS only needs to
    # carry canonical names. Inverting this order would force adding every
    # alias to KNOWN_EXECUTORS as well, creating hidden coupling.
    value="$(normalize_alias "$value")"

    if ! is_known_executor "$value"; then
        echo "resolve-executor: WARN: unknown executor '$value' from $source - falling closed to 'do'" >&2
        value="do"
        warn_fired=1
    fi

    echo "$value"
    echo "resolve-executor: resolved=$value source=$source" >&2

    # Emit AFTER the stdout/stderr contract is satisfied so telemetry can never
    # alter the resolved value or the resolver's diagnostics.
    local warn_bool="false"
    [[ "$warn_fired" -eq 1 ]] && warn_bool="true"
    emit_resolution "$value" "$(tier_for "$source")" "$warn_bool"
    emit_task_started "$value"
}

resolve
