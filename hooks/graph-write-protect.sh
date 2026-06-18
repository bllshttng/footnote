#!/usr/bin/env bash
# graph-write-protect.sh - PreToolUse hook: block direct edits to graph.json
#
# Reads tool call payload from stdin (JSON). If the tool is Edit or Write and
# the target file_path resolves to ~/.fno/graph.json (or any path ending
# in /graph.json that is NOT under a test/fixtures directory), the edit is
# blocked with a redirect message.
#
# Operator-authority matrix (Phase 6, design LD3/LD29). While an operator holds
# an interactive/step/paranoid drive window, the bytes flowing into the agent's
# PTY are operator-authored, not LLM authorship. Two additional cells fire only
# when `drive_authority_active`:
#   - Edit/Write a gate boolean in target-state.md  -> REFUSED  (cv-8231f8cb)
#   - Edit/Write a .fno/artifacts/*.md file    -> ALLOWED + audit-tagged
#                                                        (cv-9def52a7)
# Detection is delegated to scripts/lib/drive-authority.sh, which fails open
# (no window) when `fno` is absent, so an ordinary session is never gated.
#
# Exit 0 always (hook result is communicated via stdout JSON).
set -uo pipefail

_HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_REPO_ROOT="$(cd "${_HOOK_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib/drive-authority.sh
source "${_REPO_ROOT}/scripts/lib/drive-authority.sh" 2>/dev/null || true
# shellcheck source=../scripts/lib/events.sh
source "${_REPO_ROOT}/scripts/lib/events.sh" 2>/dev/null || true

# Fail-open shim: if drive-authority.sh did not load, report "no window" so a
# missing lib never blocks an ordinary edit.
if ! declare -F drive_authority_active >/dev/null 2>&1; then
    drive_authority_active() { return 1; }
fi

PAYLOAD=$(cat)

# jq is available in abilities's runtime
FILE_PATH=$(printf '%s' "$PAYLOAD" | jq -r '.tool_input.file_path // ""')
TOOL=$(printf '%s' "$PAYLOAD" | jq -r '.tool_name // ""')

# Only act on Edit and Write tools
if [[ "$TOOL" != "Edit" && "$TOOL" != "Write" ]]; then
    jq -n '{"decision": "approve"}'
    exit 0
fi

# Allow test fixture paths (contains /test or /fixtures/ segment)
if [[ "$FILE_PATH" == *"/test/"* || "$FILE_PATH" == *"/tests/"* || "$FILE_PATH" == *"/fixtures/"* ]]; then
    jq -n '{"decision": "approve"}'
    exit 0
fi

# Block if path targets ~/.fno/graph.json or any /.fno/graph.json
if [[ "$FILE_PATH" == *"/.fno/graph.json" ]] || [[ "$FILE_PATH" == *"/.fno/graph.json"* && "$FILE_PATH" != *"/.fno/graph.json."* ]]; then
    jq -n '{
        "decision": "block",
        "reason": "graph.json must be mutated via `fno backlog` commands; direct Edit/Write blocked. See `fno backlog --help` for available commands (add, idea, intake, update, done, defer, reconcile)."
    }'
    exit 0
fi

# ── Operator-authority matrix cells (fire only during a drive window) ──────
# Resolve the project events.jsonl from the edited file's .fno dir so the
# audit event lands beside the session it annotates even if cwd differs.

# cv-8231f8cb: gate-boolean edit in target-state.md during a drive -> REFUSED.
# Gate flips normally go through `fno gate set` (a Bash tool, not Edit/Write),
# so the legitimate path never reaches here. The HARD-GATE already forbids
# hand-editing these booleans in general; this cell adds the drive-window
# refusal + audit so an operator's keystrokes during a takeover cannot forge a
# gate. Only consult the (subprocess) detector once the cheap path test passes.
if [[ "$FILE_PATH" == *"/.fno/target-state.md" ]]; then
    # Decide by DIFF, not by key-name regex on the payload: an Edit can flip a
    # gate with a minimal "false"->"true" replacement that carries no gate key
    # text, and a non-gate Write still contains the unchanged gate lines. Both
    # mislead a payload-text scan. Instead, realize the post-edit content and
    # compare the SET of gate-boolean assignments before vs after - it shifts
    # only when a gate boolean is actually added/removed/flipped.
    _GATE_RE='^[[:space:]]*[a-z_]*(_passed|_validated|_updated|_shipped|_generated|_captured)[[:space:]]*:'
    _CUR=""
    [[ -f "$FILE_PATH" ]] && _CUR=$(cat "$FILE_PATH")
    if [[ "$TOOL" == "Write" ]]; then
        _AFTER=$(printf '%s' "$PAYLOAD" | jq -r '.tool_input.content // ""')
    else
        _OLD=$(printf '%s' "$PAYLOAD" | jq -r '.tool_input.old_string // ""')
        _NEW=$(printf '%s' "$PAYLOAD" | jq -r '.tool_input.new_string // ""')
        # Replace the first occurrence (Edit requires old_string to be unique,
        # so this matches the realized edit). Quoting the pattern keeps any glob
        # metacharacters literal on bash that supports it; gate-flip edits carry
        # none regardless.
        _AFTER="${_CUR/"$_OLD"/"$_NEW"}"
    fi
    _BEFORE_SET=$(printf '%s\n' "$_CUR"   | grep -E "$_GATE_RE" | tr -d '[:blank:]' | sort)
    _AFTER_SET=$(printf  '%s\n' "$_AFTER" | grep -E "$_GATE_RE" | tr -d '[:blank:]' | sort)
    if [[ "$_BEFORE_SET" != "$_AFTER_SET" ]] && drive_authority_active; then
        _ABIL_DIR="${FILE_PATH%/target-state.md}"
        EVENTS_FILE="${_ABIL_DIR}/events.jsonl"
        if declare -F emit_event >/dev/null 2>&1; then
            emit_event "hook" "gate_edit_forged_during_drive" \
                "$(jq -nc --arg fp "$FILE_PATH" '{file_path:$fp, reason:"drive_authority_active"}' 2>/dev/null || echo '{}')" \
                2>/dev/null || true
        fi
        jq -n '{
            "decision": "block",
            "reason": "gate-boolean edits to target-state.md during an operator drive window are refused (LD3): gate signals authored while an operator holds the PTY are operator-initiated, not LLM authorship. Detach the drive (Ctrl-\\ d), or flip the gate through `fno gate set` so the transition is attributed correctly."
        }'
        exit 0
    fi
fi

# cv-9def52a7: artifact edit during a drive -> ALLOWED, audit-tagged. Artifacts
# are informational state, not an authority decision, so the operator may edit
# them; we only record that the operator (not the LLM) touched it, with a
# last_operator_edit watermark on the event for the audit trail.
if [[ "$FILE_PATH" == *"/.fno/artifacts/"*.md ]] && drive_authority_active; then
    _ABIL_DIR="${FILE_PATH%/artifacts/*}"
    EVENTS_FILE="${_ABIL_DIR}/events.jsonl"
    if declare -F emit_event >/dev/null 2>&1; then
        emit_event "hook" "artifact_edited_operator_initiated" \
            "$(jq -nc --arg fp "$FILE_PATH" --arg ts "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
                '{file_path:$fp, last_operator_edit:$ts, reason:"drive_authority_active"}' 2>/dev/null || echo '{}')" \
            2>/dev/null || true
    fi
    jq -n '{"decision": "approve"}'
    exit 0
fi

# Approve all other paths
jq -n '{"decision": "approve"}'
