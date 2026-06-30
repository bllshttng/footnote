#!/usr/bin/env bash
# hooks/born-with-why-offer-inject.sh -- surface a pending born-with-why offer (x-af8d).
#
# UserPromptSubmit hook. The attended born-with-why path (spawn_think.py) emits a
# `think_offered` event to .fno/events.jsonl, but its only surfacing to the agent
# is a stderr line that can be misread or dropped. This hook re-surfaces an
# unconsumed offer ONCE as a <system-reminder> the harness owns, so the operator
# actually gets the yes/no choice the offer path exists to present.
#
# Read-only over .fno/events.jsonl + a byte-offset cursor (.fno/.think-offer-cursor).
# Never blocks, never mutates graph/state, always exits 0. Fires exactly once per
# offer: the cursor advances to EOF after each scan, so a consumed event never
# re-surfaces (AC2-ERR). A malformed/truncated events line is skipped (AC2-EDGE).
#
# ponytail: single project-local cursor (not session-keyed) -- surfacing an
# attended offer once TOTAL across sessions is the intent; two concurrent
# sessions sharing .fno should not both nag. If multiple offers land between two
# turns, only the newest surfaces (offers are attended/human-paced, so 0-1 per
# gap is the norm); the stop-hook escalation that catches the rest is x-965f.

set -uo pipefail

REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
EVENTS="$REPO_ROOT/.fno/events.jsonl"
CURSOR="$REPO_ROOT/.fno/.think-offer-cursor"

# No events file yet -> nothing to surface.
[[ -f "$EVENTS" ]] || exit 0

size=$(wc -c < "$EVENTS" 2>/dev/null | tr -d ' ')
[[ "$size" =~ ^[0-9]+$ ]] || exit 0

offset=0
[[ -f "$CURSOR" ]] && offset=$(tr -d ' \n' < "$CURSOR" 2>/dev/null)
[[ "$offset" =~ ^[0-9]+$ ]] || offset=0
# File shrank/rotated -> reset to start (don't trust a stale offset).
(( offset > size )) && offset=0
# Nothing new appended since last scan.
(( offset >= size )) && exit 0

# Scan only the newly-appended tail. Per-line JSON parse so a malformed or
# truncated line is skipped, not fatal. Latest think_offered node_id wins.
node_id=$(tail -c +"$((offset + 1))" "$EVENTS" 2>/dev/null | python3 -c '
import sys, json
nid = ""
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    if ev.get("type") == "think_offered":
        x = (ev.get("data") or {}).get("node_id")
        if x:
            nid = x
print(nid)
' 2>/dev/null)

# Advance the cursor to EOF regardless of what we found -- consuming the tail we
# just scanned is what makes the reminder fire once per offer.
printf '%s' "$size" > "$CURSOR" 2>/dev/null || true

[[ -n "$node_id" ]] || exit 0

reminder="<system-reminder>
A born-with-why offer is pending for ${node_id}. Surface it to the operator as a
yes/no before wrapping up: \"Run /think ${node_id} now, or skip?\" This is an
offer, not something that already ran - nothing was spawned.
</system-reminder>"

# jq is a repo invariant for these hooks (session-start.sh uses it unconditionally).
jq -n --arg ctx "$reminder" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'

exit 0
