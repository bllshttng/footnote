#!/usr/bin/env bash
# resolve-plan-executor.sh - resolve the executor for a FLAT plan (the
# inline /do path), mirroring /do waves's per-task resolution at the
# whole-plan granularity that /do works at.
#
# /do waves consults skills/do/scripts/resolve-executor.sh per task
# block, so frontend tasks route to the `impeccable` executor
# (frontend-executor subagent). The inline /do path had no equivalent, so
# frontend-only plans silently ran as plain `do`. This script closes that
# gap: it extracts the plan's declared file list plus any plan-level
# `executor:` and runs the SAME locked surface inference, so a frontend
# plan resolves to `impeccable` on the inline path too.
#
# Usage:
#   resolve-plan-executor.sh path/to/plan.md   # -> do | impeccable
#   cat plan.md | resolve-plan-executor.sh     # via stdin

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESOLVER="$SCRIPT_DIR/../../skills/do/scripts/resolve-executor.sh"

# Surface inference lives in the in-package module fno.executor._surface
# (the SINGLE source of truth, ported from the retired infer-task-executor.sh).
# In a checkout, point PYTHONPATH at cli/src so `python3 -m fno.executor._surface`
# runs pre-install; otherwise rely on the installed `fno` package.
_FNO_PKG_SRC="$(cd "$SCRIPT_DIR/../.." 2>/dev/null && pwd)/cli/src"
if [[ -f "${_FNO_PKG_SRC}/fno/executor/_surface.py" ]]; then
    export PYTHONPATH="${_FNO_PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

plan_content=""
if [[ $# -ge 1 ]]; then
    # A path was given: it MUST exist. Falling through to stdin here would
    # hang on a TTY waiting for input that never comes (Gemini MEDIUM on
    # PR #385). Fail loud instead.
    if [[ -f "$1" ]]; then
        plan_content="$(cat "$1")"
    else
        echo "resolve-plan-executor: plan file not found: $1" >&2
        exit 2
    fi
elif [[ ! -t 0 ]]; then
    plan_content="$(cat)"
else
    echo "resolve-plan-executor: no plan-file argument and stdin is a TTY (nothing to read)" >&2
    exit 2
fi

# Plan-level explicit executor (frontmatter or a top-level `executor:` line).
# A plan that declares its own executor wins over surface inference, exactly
# like the operator path's plan tier.
# Case-SENSITIVE: the operator convention and resolve-executor.sh contract
# spell it lowercase `executor:`. Matching case-insensitively risked a prose
# line like `Executor: ...` being read as a directive (Gemini MEDIUM on
# PR #385). The `Files:` grep below stays case-insensitive because plans
# legitimately write `**Files:**` / `File:`.
plan_exec="$(printf '%s\n' "$plan_content" \
    | grep -E '^[[:space:]]*executor:[[:space:]]*' \
    | head -1 \
    | sed -E 's/^[[:space:]]*executor:[[:space:]]*//' \
    | tr -d '"' | tr -d "'" | tr -d ' ')"

# Declared file list. Matches the operator task convention: `Files:` /
# `File:`, with optional `**` markdown bold and bracket/quote/backtick
# noise. Comma- or newline-separated.
plan_files="$(printf '%s\n' "$plan_content" \
    | grep -iE '^[[:space:]]*\*{0,2}files?:?\*{0,2}[[:space:]]' \
    | sed -E 's/^[[:space:]]*\*{0,2}[Ff]ile[s]?:?\*{0,2}[[:space:]]*//' \
    | tr ',' '\n' \
    | tr -d '`"][' \
    | sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//' \
    | sed -E 's/[[:space:]]*\([^)]*\)[[:space:]]*$//' \
    | grep -v '^$' || true)"
# The trailing-parenthetical strip handles the documented focused-plan form
# `app/page.tsx (lines 1-5)`: without it the path keeps the ` (lines 1-5)`
# suffix, the locked matcher's `*.tsx` arm never fires, and an App Router /
# pages-router file resolves to `do` instead of impeccable (Codex P2 on
# PR #385). It only removes a parenthetical at end-of-entry, so real paths
# are untouched.

# Delegate to the canonical three-tier resolver. The task tier is unused at
# plan granularity; the plan tier + surface inference cover the inline path.
#
# Fallback when the operator resolver is absent (Codex P2 / Gemini MEDIUM on
# PR #385): this lib lives in scripts/lib, but RESOLVER points into
# skills/do/scripts, which may not co-exist in a lightweight or
# partially-bundled install. Rather than fail silently to an empty string,
# fall back to the in-package locked matcher (fno.executor._surface, always
# importable) honoring the plan-level executor first.
if [[ -f "$RESOLVER" ]]; then
    TASK_EXEC="" PLAN_EXEC="$plan_exec" TASK_FILES="$plan_files" \
        bash "$RESOLVER" 2>/dev/null
elif [[ -n "$plan_exec" ]]; then
    # Normalize the same way resolve-executor.sh would (tdd -> do; unknown
    # falls closed to do).
    case "$plan_exec" in
        impeccable) echo "impeccable" ;;
        do|tdd)     echo "do" ;;
        *)          echo "do" ;;
    esac
else
    printf '%s\n' "$plan_files" | python3 -m fno.executor._surface 2>/dev/null || echo "do"
fi
