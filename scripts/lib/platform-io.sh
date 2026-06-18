#!/usr/bin/env bash
# platform-io.sh - cross-platform hook input/output for stop hooks.
#
# Lifted from hooks/target-stop-hook.sh (Phase 1 of stop-hook refactor).
# Behavior is identical to the inline definitions.
#
# Supports Claude Code (Stop), Gemini CLI (AfterAgent), and Codex CLI
# (Stop). Each platform has a different JSON schema for block/allow
# decisions and a different way of surfacing the last assistant message.
#
# Requires (set by caller):
#   PLATFORM - one of {gemini, codex, claude}, populated via detect_platform

# detect_platform
#   Echoes the platform name based on env vars set by the host CLI. Falls
#   back to "claude" when no platform-specific marker is present.
detect_platform() {
    if [[ -n "${GEMINI_PROJECT_DIR:-}" ]]; then
        echo "gemini"
    elif [[ -n "${CODEX_PLUGIN_ROOT:-}" ]]; then
        echo "codex"
    elif [[ -n "${CLAUDE_PLUGIN_ROOT:-}" ]]; then
        echo "claude"
    else
        echo "claude"  # Default / Claude Code
    fi
}

# emit_block REASON SYSTEM_MSG
#   Print a platform-appropriate "block exit" JSON document on stdout.
#   REASON is fed back as the next user message (Claude/Codex) or the
#   retry prompt (Gemini).
emit_block() {
    local reason="$1"
    local system_msg="$2"

    case "$PLATFORM" in
        gemini)
            # Gemini AfterAgent uses "deny" and feeds reason as retry prompt
            jq -n --arg reason "$reason" \
                '{"decision":"deny","reason":$reason}'
            ;;
        codex)
            # Codex Stop uses "block" + reason + systemMessage (same as Claude Code)
            jq -n --arg reason "$reason" --arg msg "$system_msg" \
                '{"decision":"block","reason":$reason,"systemMessage":$msg}'
            ;;
        *)
            # Claude Code: block + reason + systemMessage
            jq -n --arg reason "$reason" --arg msg "$system_msg" \
                '{"decision":"block","reason":$reason,"systemMessage":$msg}'
            ;;
    esac
}

# emit_approve [SYSTEM_MSG]
#   Print a platform-appropriate "allow exit" JSON document on stdout.
#   Codex requires zero-byte output to allow; Claude/Gemini emit JSON.
emit_approve() {
    local system_msg="${1:-}"

    case "$PLATFORM" in
        gemini)
            # Gemini: explicit approve
            echo '{"decision":"approve"}'
            ;;
        codex)
            # Codex: no output = allow (must produce zero bytes on stdout)
            :
            ;;
        *)
            # Claude Code: always emit approve JSON
            if [[ -n "$system_msg" ]]; then
                jq -n --arg msg "$system_msg" '{"decision":"approve","systemMessage":$msg}'
            else
                echo '{"decision":"approve"}'
            fi
            ;;
    esac
}

# extract_last_output_from_hook HOOK_INPUT_JSON
#   Pull the last assistant message from the hook input JSON. Gemini and
#   Codex provide it directly under prompt_response / last_assistant_message;
#   Claude Code requires parsing the transcript JSONL (handled by the
#   caller, not this helper). Echoes empty string on miss.
extract_last_output_from_hook() {
    local hook_input="$1"

    case "$PLATFORM" in
        gemini)
            # AfterAgent provides prompt_response directly
            echo "$hook_input" | jq -r '.prompt_response // empty' 2>/dev/null
            ;;
        codex)
            # Stop provides last_assistant_message directly
            echo "$hook_input" | jq -r '.last_assistant_message // empty' 2>/dev/null
            ;;
        *)
            # Claude Code: handled by transcript parsing below
            echo ""
            ;;
    esac
}
