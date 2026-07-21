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

# Resolve + in-progress guard: suppress an offer that should not reach the
# operator. Two cases, both from one `fno backlog get`:
#   (a) PHANTOM  -- the node no longer resolves (removed / superseded / a
#       never-persisted legacy-prefix id). Keyed off the command's EXIT CODE.
#   (b) UNDERWAY -- the node is already being worked: it has a PR, or a
#       lifecycle state past just-born (claimed / next / done / superseded). A
#       born-with-why /think only makes sense on a just-born, not-yet-started
#       node; once it is claimed or has a PR the "why" conversation already
#       happened, so re-offering just spawns a DUPLICATE /think on a live
#       session (observed: x-ef41 offered in an unrelated session AND in its
#       own, while claimed + PR open). Keyed off the resolved JSON.
# Degrade to surfacing whenever `fno` is unavailable or its output cannot be
# parsed, so a missing/garbled resolver never eats a real fresh offer. Run from
# $REPO_ROOT so resolution is deterministic even if graph_json is project-local.
if command -v fno >/dev/null 2>&1; then
    if node_json=$( cd "$REPO_ROOT" && fno backlog get "$node_id" 2>/dev/null ); then
        # Resolved. Suppress only if the node is already underway; a parse
        # failure or unknown shape exits 1 -> surface (fail safe).
        if printf '%s' "$node_json" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
    # isinstance guard: fno backlog get emits an object, but a null / list body
    # would make d.get raise AttributeError outside the try. Keep the whole
    # access inside the try so any unexpected shape exits 1 -> surface (gemini).
    underway = isinstance(d, dict) and (
        bool(d.get("pr_number")) or d.get("status") in {"in_progress", "claimed", "next", "done", "superseded"}
    )
    sys.exit(0 if underway else 1)
except Exception:
    sys.exit(1)  # unparseable or unexpected shape -> do NOT suppress (surface)
' 2>/dev/null; then
            exit 0
        fi
    else
        # Unresolvable -> phantom, suppress.
        exit 0
    fi
fi

# Fall back to the router-valid dispatch form if the event carried no offer_line.
[[ -n "$offer_cmd" ]] || offer_cmd="/think dispatch ${node_id}"

# The full v1 bare-id reminder. This is the FALLBACK, surfaced verbatim whenever
# enrichment cannot run or parse (fno unavailable, empty/non-dict node body,
# titleless node) -- never blank, never truncated JSON (AC2-ERR). It is the ONLY
# place the v1 "offer is pending" phrasing survives.
v1_reminder="<system-reminder>
A born-with-why offer is pending for ${node_id}. Surface it to the operator as a
yes/no before wrapping up: \"Run \`${offer_cmd}\` now, or skip?\" This is an
offer, not something that already ran - nothing was spawned.
</system-reminder>"

reminder="$v1_reminder"

# Enrichment (offer path only). Reuse $node_json captured by the underway guard
# above -- the offered node is fetched exactly once, ever. One parse emits
# tab-separated title / <=200-char why-excerpt / domain; whitespace is collapsed
# first so neither field can carry a tab or newline. Any failure (fno absent, so
# node_json unset; empty or non-dict body; titleless node) leaves $reminder as
# the v1 fallback.
if [[ -n "${node_json:-}" ]]; then
    enrich=$(printf '%s' "$node_json" | python3 -c '
import sys, json, re
# Node title/details are free text (organic capture from transcripts), embedded
# inside the hook-owned <system-reminder> wrapper. jq --arg keeps the JSON valid
# but does NOT neutralize a literal </system-reminder> in that text, so a node
# could break out of the reminder and inject context into the next prompt. Defang
# the reminder delimiter (open/close, case- and whitespace-insensitive) before it
# is embedded; the real wrapper is added in bash, after this.
_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)
def defang(s):
    return _TAG.sub(r"[\1system-reminder]", s)
try:
    d = json.load(sys.stdin)
    if not isinstance(d, dict):
        sys.exit(1)
    title = defang(" ".join((d.get("title") or "").split()))
    if not title:
        sys.exit(1)
    why = defang(" ".join((d.get("details") or "").split()))
    if len(why) > 200:
        cut = why[:200].rsplit(" ", 1)[0].rstrip()
        why = (cut or why[:200]) + "…"
    domain = (d.get("domain") or "").strip()
    # Unit separator (non-whitespace): tab is IFS-whitespace, so bash read would
    # collapse an empty why field and shift domain into it. \x1f never appears in
    # a node title, so an empty middle field survives intact.
    sys.stdout.write("\x1f".join([title, why, domain]))
except Exception:
    sys.exit(1)
' 2>/dev/null) || enrich=""

    if [[ -n "$enrich" ]]; then
        IFS=$'\x1f' read -r e_title e_why e_domain <<<"$enrich"

        # Empty details -> omit the "Why:" clause entirely, no dangling label (AC1-EDGE).
        why_line=""
        [[ -n "$e_why" ]] && why_line=" Why: ${e_why}."

        # Second candidate (US3): top-ranked ready node sharing the offered node's
        # domain (board/rank order, excluding the offered node), else `fno backlog
        # next`, else none. Every step degrades to empty on failure -> solo offer.
        cand_id=""
        cand_title=""
        if command -v fno >/dev/null 2>&1; then
            cand_id=$( cd "$REPO_ROOT" && fno backlog ready 2>/dev/null | python3 -c '
import sys, json
offered, domain = sys.argv[1], sys.argv[2]
try:
    rows = json.load(sys.stdin)
    if isinstance(rows, list):
        for r in rows:
            if not isinstance(r, dict) or r.get("id") == offered:
                continue
            if domain and r.get("domain") != domain:
                continue
            print(r.get("id") or "")
            break
except Exception:
    pass
' "$node_id" "$e_domain" 2>/dev/null ) || cand_id=""

            if [[ -z "$cand_id" ]]; then
                cand_id=$( cd "$REPO_ROOT" && fno backlog next 2>/dev/null | python3 -c '
import sys, json
offered = sys.argv[1]
try:
    d = json.load(sys.stdin)
    if isinstance(d, dict) and d.get("id") and d.get("id") != offered:
        print(d["id"])
except Exception:
    pass
' "$node_id" 2>/dev/null ) || cand_id=""
            fi

            if [[ -n "$cand_id" ]]; then
                cand_title=$( cd "$REPO_ROOT" && fno backlog get "$cand_id" 2>/dev/null | python3 -c '
import sys, json, re
_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)
try:
    d = json.load(sys.stdin)
    t = " ".join((d.get("title") or "").split()) if isinstance(d, dict) else ""
    print(_TAG.sub(r"[\1system-reminder]", t))
except Exception:
    pass
' 2>/dev/null ) || cand_title=""
                # No title -> drop the candidate (the "Also on deck" shape needs one).
                [[ -n "$cand_title" ]] || cand_id=""
            fi
        fi

        ondeck_line=""
        [[ -n "$cand_id" ]] && ondeck_line="

Also on deck: ${cand_id} - \"${cand_title}\" (\`/think ${cand_id}\`)."

        reminder="<system-reminder>
It's about time you think about ${node_id} - \"${e_title}\".${why_line}
Run \`${offer_cmd}\` now, or skip?${ondeck_line}

This is an offer, not something that already ran - nothing was spawned.
</system-reminder>"
    fi
fi

# jq is a repo invariant for these hooks (session-start.sh uses it unconditionally).
# All node text reaches JSON only through --arg, so backticks / quotes / $() in a
# title render literally and never trigger shell expansion (AC2-EDGE).
jq -n --arg ctx "$reminder" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'

exit 0
