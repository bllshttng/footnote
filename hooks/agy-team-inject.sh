#!/usr/bin/env bash
# fno hook: PreInvocation - agy team inject
# hooks/agy-team-inject.sh -- agy (Antigravity CLI) PreInvocation team +
# announcements adapter.
#
# agy has NO session-start event (five events only: PreToolUse, PostToolUse,
# PreInvocation, PostInvocation, Stop). PreInvocation is the surface instead,
# and its stdin carries invocationNum, documented as 0-indexed with the first
# invocation at 0 - so invocationNum == 0 IS session start. The team line is
# gated on invocationNum == 0: PreInvocation fires before EVERY model call,
# and without the gate the team line re-lands on every turn of the session.
# Announcements are NOT gated: `fno-agents announce read` dedups through its
# own per-session cursor (the same reader claude, codex, opencode and pi call
# each turn), so it is safe on every invocation.
#
# Contract (agy PreInvocation):
#   stdin  (camelCase): conversationId, invocationNum, ...
#   stdout: {"injectSteps":[{"ephemeralMessage":"<line>"},...]}  -> inject lines
#           anything else (incl. {})                             -> inject nothing
#
# ephemeralMessage over userMessage on purpose: userMessage is user-shaped, and
# this repo's pitfalls corpus records that user-shaped injection is
# indistinguishable from a superuser typing (the mail-probe entry).
#
# NEVER blocks. Always exits 0 and degrades to silence when anything it reads
# is missing: no jq, no fno-agents, no registry row, no team. A session with
# no team still gets announcements; a session with neither injects nothing.
set -uo pipefail

HOOK_INPUT=$(cat)
command -v jq >/dev/null 2>&1 || { echo '{}'; exit 0; }

emit() {
    if [[ ${#STEPS[@]} -gt 0 ]]; then
        jq -nc '{injectSteps: ($ARGS.positional | map({ephemeralMessage: .}))}' --args "${STEPS[@]}"
    else
        echo '{}'
    fi
}

CONVERSATION_ID="$(printf '%s' "$HOOK_INPUT" | jq -r '.conversationId // empty' 2>/dev/null)"
[[ -n "$CONVERSATION_ID" ]] || { echo '{}'; exit 0; }

STEPS=()

# Announcements on every model call, ahead of any team line. Same load-aware
# bound and fail-open posture as inject-announce.sh.
command -v fno-agents >/dev/null 2>&1 && {
    HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    # shellcheck source=scripts/lib/hook-budget.sh
    if source "$HOOK_DIR/../scripts/lib/hook-budget.sh" 2>/dev/null; then
        ANNOUNCE_OUT="$(hook_run_optional fno-agents announce read \
            --session-id "$CONVERSATION_ID" \
            --harness agy \
            --boundary prompt 2>/dev/null || true)"
        [[ -n "$ANNOUNCE_OUT" ]] && STEPS+=("$ANNOUNCE_OUT")
    fi
}

# invocationNum == 0 is session start; any other value means the model has
# already been called this session and the team line has landed.
if [[ "$(printf '%s' "$HOOK_INPUT" | jq -r '.invocationNum // empty' 2>/dev/null)" == "0" ]]; then

    # Team read from the registry row, never from a name (the same choice as
    # lead-postcompact-reinject.sh): `fno agents registry-json` is a daemon-free
    # file read; this session's row matches session_id OR harness_session_id.
    command -v fno >/dev/null 2>&1 || { emit; exit 0; }
    # A hook is never a delegated one-verb child, so a FNO_AGENTS_RUNTIME pin here
    # has leaked off a spawned worker: strip it for this read and keep the exit
    # code, so a broken read never reads silently as "no row".
    AGENTS_JSON="$(env -u FNO_AGENTS_RUNTIME fno agents registry-json 2>/dev/null)"
    REG_RC=$?
    [[ "$REG_RC" -ne 0 ]] \
      && echo "agy-team-inject.sh: fno agents registry-json exited $REG_RC; team treated as unknown (the FNO_AGENTS_RUNTIME pin was stripped before the read)" >&2
    MY_ROW="$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$CONVERSATION_ID" \
        '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)"
    [[ -n "$MY_ROW" ]] || { emit; exit 0; }
    CROWN_LEVEL="$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)"
    CROWN_SCOPE="$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)"
    [[ -n "$CROWN_LEVEL" || -n "$CROWN_SCOPE" ]] \
      && STEPS+=("You are the lead: team level ${CROWN_LEVEL:-?} over ${CROWN_SCOPE:-?}. Confirm with \`fno whoami\`. Before any CLI verb, load the lead reference at skills/lead/references/cli-commands.md.")
fi

emit
exit 0
