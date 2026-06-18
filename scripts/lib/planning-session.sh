#!/usr/bin/env bash
# planning-session.sh - detect and cost-register a planning session
# (think/plan/audit skill invocation) that ran without a target state file.
#
# Lifted from hooks/target-stop-hook.sh (Phase 1 of stop-hook refactor).
# Behavior is identical to the inline block.
#
# Two detection methods (fallback chain):
#   1. session-state.md file (created by PreToolUse hook if supported)
#   2. Transcript scan for Skill tool calls to fno:think/plan/audit
# Non-blocking - failures logged, don't prevent exit.
#
# Requires (set by caller):
#   STATE_FILE              - path to target-state.md (this branch only fires when ABSENT)
#   TRANSCRIPT_PATH         - path to the active transcript .jsonl
#   SESSION_STATE_FILE      - path to .fno/session-state.md
#   SESSION_SENTINEL        - path to .fno/.session-registered
#   SCRIPT_DIR              - path to the abilities plugin root (parent of scripts/)
#   LOG_FILE                - path to the stop-hook log file
#   log()                   - logging function from the hook

# _detect_planning_session
#   Echoes the session type ("think" / "plan" / "audit") and rc=0 when a
#   planning session is detected, otherwise rc=1.
_detect_planning_session() {
    # Method 1: session-state.md exists (PreToolUse hook created it)
    if [[ -f "$SESSION_STATE_FILE" ]]; then
        sed -n 's/^type:[[:space:]]*//p' "$SESSION_STATE_FILE" | head -1 | tr -d '"'
        return 0
    fi

    # Method 2: Scan transcript for Skill tool calls to planning skills
    # Looks for: "skill": "fno:think" or "fno:plan" or "fno:audit"
    # Only matches the Skill tool invocation pattern, not mentions in text
    if [[ -f "$TRANSCRIPT_PATH" ]]; then
        local detected
        detected=$(grep -o '"skill"[[:space:]]*:[[:space:]]*"abilities:\(think\|plan\|audit\)"' "$TRANSCRIPT_PATH" 2>/dev/null \
            | head -1 \
            | sed 's/.*"abilities:\([^"]*\)".*/\1/')
        if [[ -n "$detected" ]]; then
            log "Planning session detected via transcript scan: $detected"
            echo "$detected"
            return 0
        fi
    fi

    return 1
}

# handle_planning_session_if_applicable
#   If this isn't a target session (no STATE_FILE) but transcript or
#   session-state.md indicates a planning session, register cost and
#   return rc=0 (caller should exit 0). Returns rc=1 otherwise.
handle_planning_session_if_applicable() {
    local SESSION_TYPE
    SESSION_TYPE=$(_detect_planning_session)
    [[ -z "$SESSION_TYPE" ]] && return 1

    local session_id
    session_id=$(basename "$TRANSCRIPT_PATH" .jsonl)

    # Idempotency: skip if already registered for THIS session
    if [[ -f "$SESSION_SENTINEL" ]] && [[ "$(cat "$SESSION_SENTINEL" 2>/dev/null)" == "$session_id" ]]; then
        log "Planning session already registered for $session_id - skipping"
        echo "target: ${SESSION_TYPE:-planning} session already registered" >&2
        return 0
    fi

    log "Planning session detected (type=$SESSION_TYPE) - registering cost"

    # Calculate cost. The cost helpers moved into the fno package; run them as
    # `python3 -m fno.cost.<mod>`, pointing PYTHONPATH at the package source in
    # a checkout so it works pre-install (else rely on the installed `fno`).
    local cost_pkg_src branch cost_json title skill_args slug
    cost_pkg_src="${SCRIPT_DIR}/cli/src"
    [[ -f "${cost_pkg_src}/fno/cost/_session_cost.py" ]] && \
        export PYTHONPATH="${cost_pkg_src}${PYTHONPATH:+:${PYTHONPATH}}"
    branch=$(git branch --show-current 2>/dev/null || echo "")

    cost_json=""
    if [[ -n "$session_id" ]]; then
        local cost_args
        cost_args=(--json)
        [[ -n "$branch" ]] && cost_args+=(--branch "$branch")
        cost_json=$(python3 -m fno.cost._session_cost "${cost_args[@]}" "$session_id" 2>>"$LOG_FILE" || echo "")
    fi

    # Build title from transcript (extract the skill args for context)
    title="${SESSION_TYPE} session"
    if [[ -f "$TRANSCRIPT_PATH" ]]; then
        # Extract the args passed to the planning skill for a better title
        skill_args=""
        skill_args=$(python3 -c "
import json, re
found = False
for line in open('$TRANSCRIPT_PATH'):
    if found:
        break
    try:
        d = json.loads(line.strip())
        for c in (d.get('message',{}).get('content',[]) or []):
            if isinstance(c,dict) and c.get('name')=='Skill':
                inp = c.get('input',{})
                sk = inp.get('skill','')
                if sk == 'abilities:$SESSION_TYPE':
                    args = inp.get('args','')
                    args = re.sub(r'--\S+\s*', '', args).strip()
                    if args:
                        print(args[:100])
                        found = True
                        break
    except Exception:
        pass
" 2>/dev/null)
        if [[ -n "$skill_args" ]]; then
            title="${SESSION_TYPE}: ${skill_args}"
        elif [[ -n "$branch" ]]; then
            slug="${branch#feature/}"
            title="${SESSION_TYPE}: ${slug//-/ }"
        fi
    fi

    # Register via the in-package fno.cost._register module.
    local register_args
    register_args=(--type "$SESSION_TYPE" --title "$title" --session "$session_id")
    [[ -n "$cost_json" ]] && register_args+=(--cost-json "$cost_json")
    python3 -m fno.cost._register "${register_args[@]}" 2>>"$LOG_FILE" 1>/dev/null && \
        log "Planning session registered: $SESSION_TYPE | $session_id" || \
        log "WARNING: fno.cost._register failed for planning session"

    # Write sentinel + clean up state file
    echo "$session_id" > "$SESSION_SENTINEL" 2>/dev/null || true
    rm -f "$SESSION_STATE_FILE" 2>/dev/null || true

    # Allow exit - planning sessions don't loop
    echo "target: ${SESSION_TYPE} session registered" >&2
    return 0
}
