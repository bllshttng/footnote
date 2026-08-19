#!/usr/bin/env bash
# hooks/agy-crown-inject.sh -- agy (Antigravity CLI) PreInvocation crown adapter.
#
# agy has NO session-start event (five events only: PreToolUse, PostToolUse,
# PreInvocation, PostInvocation, Stop). PreInvocation is the surface instead,
# and its stdin carries invocationNum, documented as 0-indexed with the first
# invocation at 0 - so invocationNum == 0 IS session start. The gate is
# load-bearing: PreInvocation fires before EVERY model call, and without it the
# crown line re-lands on every turn of the session.
#
# Contract (agy PreInvocation):
#   stdin  (camelCase): conversationId, invocationNum, ...
#   stdout: {"injectSteps":[{"ephemeralMessage":"<line>"}]}  -> inject the line
#           anything else (incl. {})                        -> inject nothing
#
# ephemeralMessage over userMessage on purpose: userMessage is user-shaped, and
# this repo's pitfalls corpus records that user-shaped injection is
# indistinguishable from an operator typing (the mail-probe entry).
#
# NEVER blocks. Always exits 0 and degrades to silence when anything it reads
# is missing: no jq, no fno, no registry row, no crown. An uncrowned session
# injects nothing at all.
set -uo pipefail

HOOK_INPUT=$(cat)
command -v jq >/dev/null 2>&1 || { echo '{}'; exit 0; }

# invocationNum == 0 is session start; any other value means the model has
# already been called this session and the line has landed.
[[ "$(printf '%s' "$HOOK_INPUT" | jq -r '.invocationNum // empty' 2>/dev/null)" == "0" ]] || { echo '{}'; exit 0; }

CONVERSATION_ID="$(printf '%s' "$HOOK_INPUT" | jq -r '.conversationId // empty' 2>/dev/null)"
[[ -n "$CONVERSATION_ID" ]] || { echo '{}'; exit 0; }

# Crown read from the registry row, never from a name (the same choice as
# king-postcompact-reinject.sh): `fno agents registry-json` is a daemon-free
# file read; this session's row matches session_id OR harness_session_id.
command -v fno >/dev/null 2>&1 || { echo '{}'; exit 0; }
AGENTS_JSON="$(fno agents registry-json 2>/dev/null || true)"
MY_ROW="$(printf '%s' "$AGENTS_JSON" | jq -c --arg sid "$CONVERSATION_ID" \
    '.agents[] | select(.session_id == $sid or .harness_session_id == $sid)' 2>/dev/null | head -1)"
[[ -n "$MY_ROW" ]] || { echo '{}'; exit 0; }
CROWN_LEVEL="$(printf '%s' "$MY_ROW" | jq -r '.crown_level // empty' 2>/dev/null)"
CROWN_SCOPE="$(printf '%s' "$MY_ROW" | jq -r '.crown_scope // empty' 2>/dev/null)"
[[ -n "$CROWN_LEVEL" || -n "$CROWN_SCOPE" ]] || { echo '{}'; exit 0; }

jq -nc --arg m "You are the king: crown level ${CROWN_LEVEL:-?} over ${CROWN_SCOPE:-?}. Confirm with \`fno whoami\`. Before any CLI verb, load the king reference at skills/king-for-a-day/references/cli-commands.md." \
    '{injectSteps: [{ephemeralMessage: $m}]}'
exit 0
