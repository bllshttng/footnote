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

# Scan only the slice [offset, size): bound the read with `head -c` so events
# appended AFTER we captured `size` are NOT consumed here -- the cursor only
# advances to `size`, so a racing append belongs to the next run, never both
# (once-per-offer). Per-line JSON parse so a malformed/truncated line is skipped,
# not fatal. Latest think_offered wins; carry its offer_line, the authoritative
# command the offer path recorded (a reconstructed bare `/think <id>` is a single
# non-mode token the router rejects -- skills/think/SKILL.md).
parsed=$(tail -c +"$((offset + 1))" "$EVENTS" 2>/dev/null | head -c "$((size - offset))" 2>/dev/null | python3 -c '
import sys, json
nid = ""
offer = ""
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        ev = json.loads(line)
    except Exception:
        continue
    if ev.get("type") == "think_offered":
        data = ev.get("data") or {}
        x = data.get("node_id")
        if x:
            nid = x
            offer = data.get("offer_line") or ""
print(nid + "\t" + offer)
' 2>/dev/null)

# Advance the cursor to the captured EOF regardless of what we found -- consuming
# exactly the [offset, size) slice we scanned is what makes the reminder fire
# once per offer.
printf '%s' "$size" > "$CURSOR" 2>/dev/null || true

node_id="${parsed%%$'\t'*}"
offer_cmd="${parsed#*$'\t'}"
[[ -n "$node_id" ]] || exit 0

# Resolve-guard: suppress a phantom offer whose node no longer resolves in the
# backlog -- removed, superseded, or a never-persisted legacy-prefix id. There is
# nothing to /think about, so surfacing it just nags the operator (recurring with
# orphaned `ab-`-prefixed offers). The cursor already advanced above, so this
# never re-fires for that offer. Degrade to surfacing when `fno` is unavailable,
# so a missing resolver never suppresses a real offer. Run from $REPO_ROOT so
# resolution is deterministic even if config.paths.graph_json is project-local.
if command -v fno >/dev/null 2>&1 && ! ( cd "$REPO_ROOT" && fno backlog get "$node_id" ) >/dev/null 2>&1; then
    exit 0
fi

# Fall back to the router-valid dispatch form if the event carried no offer_line.
[[ -n "$offer_cmd" ]] || offer_cmd="/think dispatch ${node_id}"

reminder="<system-reminder>
A born-with-why offer is pending for ${node_id}. Surface it to the operator as a
yes/no before wrapping up: \"Run \`${offer_cmd}\` now, or skip?\" This is an
offer, not something that already ran - nothing was spawned.
</system-reminder>"

# jq is a repo invariant for these hooks (session-start.sh uses it unconditionally).
jq -n --arg ctx "$reminder" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'

exit 0
