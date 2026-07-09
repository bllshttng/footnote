#!/usr/bin/env bash
# SessionStart hook: inject the using-fno SKILL.md as additionalContext.
#
# This is the footnote equivalent of the superpowers session-start hook
# (https://github.com/obra/superpowers/blob/main/hooks/session-start).
# It ensures every Claude session opened in a footnote-enabled project
# starts with the two-surface preamble already loaded, so the agent knows
# both /fno:* slash commands and the `fno <verb>` CLI exist from
# turn one - even on a brand-new project or after a `clear` / `compact`.
#
# Modes:
#   The matcher in hooks.json fires this on startup|clear|compact (or "")
#   so the preamble reloads after every context compaction. Without that
#   the agent would lose discoverability mid-session.
#
# Output:
#   Emits JSON with `hookSpecificOutput.additionalContext` (Claude Code)
#   or `additionalContext` (Copilot / SDK) per platform. The harness
#   injects the value into the system prompt of the next turn.
#
# Failure mode:
#   Hooks must NEVER block session start. If the skill body is missing
#   or unreadable, emit an empty object so the harness sees a valid
#   no-op response. Errors go to stderr where the harness logs them.

set -uo pipefail

PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-}"
if [[ -z "${PLUGIN_ROOT}" ]]; then
    # Fall back to discovering the plugin root relative to this script
    # (one level up from hooks/). Mirrors superpowers's fallback.
    PLUGIN_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fi

SKILL_PATH="${PLUGIN_ROOT}/skills/using-fno/SKILL.md"
if [[ ! -r "${SKILL_PATH}" ]]; then
    echo "session-start-using-fno: SKILL.md missing or unreadable at ${SKILL_PATH}" >&2
    # Silent no-op: emit empty object so the harness has a valid response.
    echo '{}'
    exit 0
fi

# Read the SKILL.md content. Errors here are non-fatal: degrade to silent.
SKILL_BODY="$(cat "${SKILL_PATH}" 2>/dev/null)" || {
    echo "session-start-using-fno: could not read ${SKILL_PATH}" >&2
    echo '{}'
    exit 0
}

# JSON-escape via bash parameter substitution. Each ${s//old/new} is a
# single C-level pass - orders of magnitude faster than a character loop.
# Pattern lifted from superpowers/hooks/session-start.
escape_for_json() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    printf '%s' "$s"
}

SKILL_ESCAPED="$(escape_for_json "${SKILL_BODY}")"

# Wrap the skill body in framing that makes it look load-bearing at the
# top of the conversation. The EXTREMELY_IMPORTANT marker matches the
# superpowers convention and triggers the harness's high-attention path.
CONTEXT="<EXTREMELY_IMPORTANT>\nYou are in a footnote-enabled project.\n\n**Below is the full content of the 'using-fno' skill - your introduction to the two surfaces (slash-command workflows and the fno CLI). Re-read it when in doubt about which surface to use:**\n\n${SKILL_ESCAPED}\n</EXTREMELY_IMPORTANT>"

# Output context injection in the right JSON shape for the host platform.
# Claude Code expects hookSpecificOutput.additionalContext (nested).
# Cursor expects additional_context (snake_case, top-level).
# Copilot CLI / SDK expects additionalContext (camelCase, top-level).
# We use printf instead of heredoc to dodge a bash 5.3+ heredoc bug.
if [[ -n "${CURSOR_PLUGIN_ROOT:-}" ]]; then
    printf '{\n  "additional_context": "%s"\n}\n' "${CONTEXT}"
elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]] && [[ -z "${COPILOT_CLI:-}" ]]; then
    printf '{\n  "hookSpecificOutput": {\n    "hookEventName": "SessionStart",\n    "additionalContext": "%s"\n  }\n}\n' "${CONTEXT}"
else
    printf '{\n  "additionalContext": "%s"\n}\n' "${CONTEXT}"
fi

exit 0
