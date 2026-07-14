#!/usr/bin/env bash
# worktree-harness-guard.sh - PreToolUse blocking guard (x-193d Wave 5).
#
# Enforces the epic invariant on the MANUAL-session path x-3e70's dispatch guard
# does not cover: at most one harness owns a worktree at a time. On the first
# write in a worktree this session claims it (via `fno claim worktree-guard`);
# a SECOND, DIFFERENT harness entering the same worktree is refused, naming the
# owner. Same-harness re-entry (two claude sessions, a subagent) never refuses.
#
# Fail-open by construction: it blocks ONLY on a parsed verdict=foreign. A
# missing/old `fno` (no worktree-guard verb), a non-git dir, any error, or an
# empty harness identity -> approve. FNO_WORKTREE_OK=1 downgrades foreign to
# override inside the verb, so an operator escape hatch needs no hook change.

set -uo pipefail

_approve() { printf '%s\n' '{}'; exit 0; }

_block() {
    local reason="$1"
    if command -v jq >/dev/null 2>&1; then
        jq -nc --arg reason "$reason" '{
            decision: "block",
            reason: $reason,
            hookSpecificOutput: {
                hookEventName: "PreToolUse",
                permissionDecision: "deny",
                permissionDecisionReason: $reason
            }
        }'
    else
        printf '{"decision":"block","reason":%s}\n' "$(printf '%s' "$reason" | sed 's/"/\\"/g;s/.*/"&"/')"
    fi
    exit 0
}

PAYLOAD="$(cat 2>/dev/null || true)"
CWD=""
if command -v jq >/dev/null 2>&1; then
    CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type=="string" and length>0)' 2>/dev/null || true)"
fi
[[ -n "$CWD" && -d "$CWD" ]] || CWD="$PWD"

command -v fno >/dev/null 2>&1 || _approve

# The verb resolves the worktree root from its own cwd and the harness from the
# ambient session markers; run it FROM the tool's cwd. --json so we branch on
# the verdict, not the exit code (exit is nonzero for both foreign AND an old
# fno without the subcommand; only a parsed verdict=foreign should block).
OUT="$(cd "$CWD" 2>/dev/null && fno claim worktree-guard --json 2>/dev/null || true)"
[[ -n "$OUT" ]] || _approve

VERDICT=""
OWNER_HARNESS=""
OWNER_HOLDER=""
WORKTREE=""
if command -v jq >/dev/null 2>&1; then
    VERDICT="$(printf '%s' "$OUT" | jq -er '.verdict // empty' 2>/dev/null || true)"
    OWNER_HARNESS="$(printf '%s' "$OUT" | jq -er '.owner_harness // empty' 2>/dev/null || true)"
    OWNER_HOLDER="$(printf '%s' "$OUT" | jq -er '.owner_holder // empty' 2>/dev/null || true)"
    WORKTREE="$(printf '%s' "$OUT" | jq -er '.worktree // empty' 2>/dev/null || true)"
fi

[[ "$VERDICT" == "foreign" ]] || _approve

_block "Worktree ${WORKTREE:-here} is owned by a ${OWNER_HARNESS:-different} session (${OWNER_HOLDER:-unknown}); a second harness must not work it concurrently (x-193d). Use that session, a different worktree, or set FNO_WORKTREE_OK=1 to override."
