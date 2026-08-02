#!/usr/bin/env bash
# worktree-harness-guard.sh - PreToolUse blocking guard (x-193d Wave 5).
#
# Enforces the epic invariant on the MANUAL-session path x-3e70's dispatch guard
# does not cover: at most one harness owns a worktree at a time. On the first
# write in a worktree this session claims it (via `fno claim worktree-guard`);
# a SECOND, DIFFERENT harness writing into the same worktree is refused, naming
# the owner. Same-harness re-entry (two claude sessions, a subagent) never
# refuses.
#
# GATES ON THE TARGET, NOT ON WHERE THE SHELL IS STANDING. The claim protects
# one thing: two harnesses mutating one branch's working tree. An Edit or Write
# to a path outside the owned worktree cannot do that, so the ownership question
# is asked about the TARGET path. Gating on cwd alone refused every tool call in
# a session whose cwd had drifted into a foreign worktree, including a Write to
# an absolute path in an unrelated tree - the exact destination
# plan-location-guard.sh requires for a plan. It hit the orchestrator profile
# hardest, which is the one profile that poses no write risk: reading a worker's
# diff is what puts your cwd there. Target-gating is also STRICTER, since it
# catches a write INTO an owned worktree from a cwd outside it, which cwd-gating
# missed entirely. The parse mirrors plan-location-guard.sh
# (`.tool_input.file_path` anchored to the payload cwd) for the reason stated
# there: the hook's spawn cwd and the session cwd differ on a worktree or
# multi-repo dispatch.
#
# A REFUSED SESSION ALWAYS HAS A MOVE. A guard with no exit is a trap, and this
# one had none. The refusal named FNO_WORKTREE_GRANT and FNO_WORKTREE_OK, both
# read inside `fno claim`, which this hook runs as its own subprocess with the
# hook's environment - so an inline `VAR=x cmd` prefix is only text in a string
# and never reaches the check, and a session already refused cannot put either
# in its environment. Both escapes are therefore honoured HERE, the only layer
# that sees the command text, alongside a bare `cd` out. The refusal names the
# escapes that actually work and nothing else.
#
# Fail-open by construction: it blocks ONLY on a parsed verdict=foreign. A
# missing/old `fno` (no worktree-guard verb), no jq, a non-git dir, any error,
# or an empty harness identity -> approve.

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

command -v fno >/dev/null 2>&1 || _approve
# No jq was already an approve in practice: without it nothing parses a verdict
# and the foreign test below can never be true. Returning early makes that
# explicit instead of leaving it as a side effect of empty variables.
command -v jq >/dev/null 2>&1 || _approve

_field() { printf '%s' "$PAYLOAD" | jq -r "$1 // \"\"" 2>/dev/null || true; }

TOOL="$(_field '.tool_name')"
FILE_PATH="$(_field '.tool_input.file_path')"
COMMAND="$(_field '.tool_input.command')"
CWD="$(printf '%s' "$PAYLOAD" | jq -er '.cwd | select(type=="string" and length>0)' 2>/dev/null || true)"
[[ -n "$CWD" && -d "$CWD" ]] || CWD="$PWD"

# The deepest EXISTING ancestor of a target. A Write creates its file and
# sometimes its directory, so the target itself is not yet resolvable; the
# ownership question is about the tree it lands in either way.
_anchor_dir() {
    local p="$1"
    [[ "$p" == /* ]] || p="$CWD/$p"
    p="$(dirname "$p")"
    while [[ -n "$p" && "$p" != "/" && ! -d "$p" ]]; do p="$(dirname "$p")"; done
    printf '%s' "$p"
}

# A command that only changes directory writes nothing, and it is the move that
# ends a wedge. Rejects every chaining and substitution character so
# `cd /tmp && rm -rf .` cannot ride in on the exemption.
_is_bare_cd() {
    local c="$1"
    c="${c#"${c%%[![:space:]]*}"}"
    c="${c%"${c##*[![:space:]]}"}"
    [[ "$c" == "cd" || "$c" == "cd "* ]] || return 1
    case "$c" in
        *[\;\&\|\`\$\(\)\<\>]*|*$'\n'*) return 1 ;;
    esac
    return 0
}

ANCHOR="$CWD"
case "$TOOL" in
    Edit|Write) [[ -n "$FILE_PATH" ]] && ANCHOR="$(_anchor_dir "$FILE_PATH")" ;;
esac

# The verb resolves the worktree root from its own cwd and the harness from the
# ambient session markers; run it FROM the anchor. --json so we branch on the
# verdict, not the exit code (exit is nonzero for both foreign AND an old fno
# without the subcommand; only a parsed verdict=foreign should block).
OUT="$(cd "$ANCHOR" 2>/dev/null && fno claim worktree-guard --json 2>/dev/null || true)"
[[ -n "$OUT" ]] || _approve

_json() { printf '%s' "$OUT" | jq -er "$1 | select(. != \"\") // empty" 2>/dev/null || true; }

VERDICT="$(_json '.verdict')"
[[ "$VERDICT" == "foreign" ]] || _approve

OWNER_HARNESS="$(_json '.owner_harness')"
OWNER_HOLDER="$(_json '.owner_holder')"
WORKTREE="$(_json '.worktree')"

# Escapes, evaluated only once the verdict is foreign, and only for Bash: an
# Edit or Write has already been judged by its target above.
if [[ "$TOOL" == "Bash" ]]; then
    _is_bare_cd "$COMMAND" && _approve
    [[ "$COMMAND" == *"FNO_WORKTREE_OK=1"* ]] && _approve
    if [[ -n "$WORKTREE" ]]; then
        for form in "FNO_WORKTREE_GRANT=$WORKTREE" \
                    "FNO_WORKTREE_GRANT=\"$WORKTREE\"" \
                    "FNO_WORKTREE_GRANT='$WORKTREE'"; do
            [[ "$COMMAND" == *"$form"* ]] && _approve
        done
    fi
fi

_block "Worktree ${WORKTREE:-here} is owned by a ${OWNER_HARNESS:-different} session (${OWNER_HOLDER:-unknown}); a second harness must not mutate its working tree concurrently. This is about the TARGET, not where your shell is standing: reads, and writes to any path outside that worktree, are unaffected. To leave, run \`cd <path outside the worktree>\` - a bare cd is always allowed. To proceed anyway (a dispatcher deliberately sent you here, e.g. a cross-model review converging on one branch), prefix the command inline: \`FNO_WORKTREE_GRANT=${WORKTREE:-<worktree>} <your command>\` - it frees THIS worktree only. An inline FNO_WORKTREE_OK=1 frees every worktree; prefer the scoped grant."
