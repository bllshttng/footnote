#!/usr/bin/env bash
# Re-inject plan goal + current phase after a context compaction.
#
# Carrier (the load-bearing decision, verified against the harness reference):
# on Claude this rides SessionStart with source=="compact", the only
# post-compaction event that injects into MODEL context. PostCompact on Claude
# has no decision control - its output goes to stderr / the user only, never
# the model - so no payload shape can deliver context through it. SessionStart
# injects via hookSpecificOutput.additionalContext, which is why this script is
# registered under SessionStart(matcher="compact") in hooks.json, not PostCompact.
# On Codex the same script still runs as a PostCompact hook, and the systemMessage
# carrier is used there. The carrier is chosen from the harness (FNO_PLATFORM /
# CLAUDE_PLUGIN_ROOT), not from the event payload, so a lost or empty stdin cannot
# silently downgrade Claude to a carrier the model never sees. That carrier choice
# and the payload emission live in scripts/lib/postcompact-carrier.sh, shared with
# hooks/king-postcompact-reinject.sh.
set -uo pipefail

STATE_FILE=".fno/target-state.md"
FNO_DIR=".fno"

# The fallback is BASH_SOURCE-relative, never `git rev-parse`: this hook runs
# with cwd set to the SESSION's repo, which is not the plugin, so a git toplevel
# would resolve GUARD_LIB into an unrelated checkout and source whatever it finds.
SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${FNO_PLATFORM:-}" == "codex" ]]; then
    PLUGIN_ROOT="${CODEX_PLUGIN_ROOT:-${PLUGIN_ROOT:-$SOURCE_ROOT}}"
else
    PLUGIN_ROOT="${CLAUDE_PLUGIN_ROOT:-${CODEX_PLUGIN_ROOT:-$SOURCE_ROOT}}"
fi
GUARD_LIB="$PLUGIN_ROOT/scripts/lib/target-guard.sh"
CARRIER_LIB="$PLUGIN_ROOT/scripts/lib/postcompact-carrier.sh"

# Event read, carrier choice, and payload emission all come from the shared lib
# (keyed on the harness, never the event payload - the reasoning is in the lib).
# An unreadable lib means a broken plugin install; emit nothing rather than guess.
[[ -r "$CARRIER_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/postcompact-carrier.sh
source "$CARRIER_LIB"

# SessionStart carries a "source" field; "compact" is the value the harness
# sets after a compaction. PostCompact (Codex) has no such field, so SOURCE is
# empty there.
EVENT="$(postcompact_read_event)"
SOURCE="$(printf '%s' "$EVENT" | sed -n 1p)"
EVENT_SESSION_ID="$(printf '%s' "$EVENT" | sed -n 2p)"
EVENT_TRANSCRIPT="$(printf '%s' "$EVENT" | sed -n 3p)"
CALLER_SESSION_ID="$(postcompact_resolve_sid "$EVENT_SESSION_ID" "$EVENT_TRANSCRIPT")"

# Defensive gate: on SessionStart, reinject only for compaction. The matcher
# ("compact") already enforces this at registration; this check keeps the script
# correct independent of registration and makes the gate testable directly. An
# empty SOURCE (PostCompact on Codex) passes through unchanged.
if [[ -n "$SOURCE" && "$SOURCE" != "compact" ]]; then
    exit 0
fi

CARRIER="$(postcompact_carrier "$SOURCE")"

if [[ -f "$GUARD_LIB" ]]; then
    # shellcheck source=../scripts/lib/target-guard.sh
    source "$GUARD_LIB"
    # Only reinject the goal when target is actively owned by this session. Stale
    # state from a prior session would otherwise inject a dead goal into an
    # unrelated compaction event.
    if ! target_is_active "$STATE_FILE" "$CALLER_SESSION_ID"; then
        exit 0
    fi
else
    # Fallback: old inline check if the guard lib is somehow unavailable.
    [[ -f "$STATE_FILE" ]] || exit 0
    STATUS=$(grep '^status:' "$STATE_FILE" 2>/dev/null | awk '{print $2}')
    [[ "$STATUS" == "IN_PROGRESS" ]] || exit 0
fi

# Extract key state fields. The manifest is inputs-only post-wedge (ab-d0337fbc):
# no current_phase / iteration / *_passed gate booleans live here anymore
# (ab-88f0854d removed those dead reads). Progress is external now - git HEAD,
# PR/CI, the loop-check verb - surfaced via `fno whoami` / `fno whoami status`.
GOAL=$(grep '^input:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^input: *//' | sed 's/^"//' | sed 's/"$//')
PLAN_PATH=$(grep '^plan_path:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^plan_path: *//' | tr -d '"')
NODE=$(grep '^graph_node_id:' "$STATE_FILE" 2>/dev/null | head -1 | sed 's/^graph_node_id: *//' | tr -d '"' | tr -d "'")

# Build re-injection context
CONTEXT="## Post-Compaction Context Reminder

**Goal:** $GOAL"
[[ -n "$NODE" && "$NODE" != "null" ]] && CONTEXT="${CONTEXT}
**Backlog node:** $NODE"

# If plan path exists, add task count
if [[ -n "$PLAN_PATH" && -d "$PLAN_PATH" ]]; then
    TOTAL_TASKS=$(grep -c '### Task' "$PLAN_PATH"/*.md 2>/dev/null | awk -F: '{s+=$NF}END{print s+0}')
    CONTEXT="${CONTEXT}
**Plan:** $PLAN_PATH ($TOTAL_TASKS tasks)"
elif [[ -n "$PLAN_PATH" && -f "$PLAN_PATH" ]]; then
    # A single-FILE plan used to vanish here: only -d was tested, so compaction
    # lost the plan path exactly when a flat quick plan was in play (x-59b0).
    CONTEXT="${CONTEXT}
**Plan:** $PLAN_PATH (single file)"
fi

# Task-context binding pointer (x-59b0): the current attempt's bound binding
# lives under the existing handoff artifact root, one slot per node
# (task-context-<node>.json); each attempt's binding is immutable through its
# digest, and receipts embed their own copies. The pointer and its declared
# constraints ride once; source CONTENTS never ride. A pointer in context is
# not a read - the stage field stays the only honest observation record.
BINDING_FILE=""
if [[ -n "$NODE" ]]; then
    CANDID=".fno/artifacts/handoff/task-context-${NODE}.json"
    [[ -f "$CANDID" ]] && BINDING_FILE="$CANDID"
fi
if [[ -n "$BINDING_FILE" ]]; then
    # The verifier decides what may be re-emitted: an edited digest, stage, or
    # constraint set is a corrupt declared binding and renders NOTHING (the
    # fields below are the verifier's answer, never the raw file's).
    VERIFIED="$(python3 - "$BINDING_FILE" <<'PY' 2>/dev/null
import json, subprocess, sys
try:
    with open(sys.argv[1]) as fh:
        binding = json.load(fh)
except Exception:
    sys.exit(0)
try:
    out = subprocess.run(
        ["fno-agents", "task-context-show"],
        input=json.dumps({"binding": binding}),
        capture_output=True, text=True, timeout=20,
    )
    answer = json.loads(out.stdout)
except Exception:
    sys.exit(0)
if not answer.get("ok"):
    sys.exit(0)
print(str(binding.get("binding_digest", "unknown")))
print(str(binding.get("stage", "unknown")))
for c in binding.get("required_constraints", [])[:10]:
    if isinstance(c, str) and c.strip():
        print("- " + c)
PY
)"
    if [[ -n "$VERIFIED" ]]; then
        BDIGEST="$(printf '%s\n' "$VERIFIED" | sed -n 1p)"
        BSTAGE="$(printf '%s\n' "$VERIFIED" | sed -n 2p)"
        BCONSTRAINTS="$(printf '%s\n' "$VERIFIED" | sed -n '3,$p')"
        CONTEXT="${CONTEXT}
**Task context:** $BINDING_FILE (binding_digest $BDIGEST, stage $BSTAGE)"
        if [[ -n "$BCONSTRAINTS" ]]; then
            CONTEXT="${CONTEXT}
**Required constraints:**
$BCONSTRAINTS"
        fi
        CONTEXT="${CONTEXT}
The binding pointer is not a read; required sources revalidate through the resume receipt context gate."
    fi
fi

CONTEXT="${CONTEXT}

Progress is not in the manifest. Run \`fno whoami\` then \`fno whoami status\`
for live phase + completion state (git HEAD, PR/CI, review)."

postcompact_emit "$CARRIER" "$CONTEXT"

exit 0
