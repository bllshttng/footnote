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
# sessions sharing .fno should not both nag. Bursts DO happen (four births 3s
# apart, 2026-07-30T02:39:58..02:40:08), so only the newest gets the full offer,
# but the rest ride along as bare ids: the cursor eats them either way, and
# naming them is the difference between deferred and destroyed (x-965f).

set -uo pipefail

# fno shells can wedge on a stalled daemon / graph lock; bound every call with
# the shared wall-clock helper rather than the harness's 30s hook timeout
# (x-989d). Fails closed like the other injection hooks: a missing helper exits 0.
HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib/with-timeout.sh
source "$HOOK_DIR/../scripts/lib/with-timeout.sh" 2>/dev/null || exit 0
# shellcheck source=../scripts/lib/events-lock.sh
source "$HOOK_DIR/../scripts/lib/events-lock.sh" 2>/dev/null || exit 0

REPO_ROOT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "$PWD")
EVENTS="$REPO_ROOT/.fno/events.jsonl"
# No journal means there is no cursor state to serialize. Keep this ahead of
# lock acquisition because a fresh checkout does not have a .fno parent yet.
[[ -f "$EVENTS" ]] || exit 0

CURSOR="$REPO_ROOT/.fno/.think-offer-cursor"
if [[ -L "$CURSOR" ]]; then
    CURSOR=$(_resolve_event_symlink "$CURSOR") || exit 0
fi
CURSOR_LOCK="${CURSOR}.lock.d"
CURSOR_LOCK_TOKEN="$(hostname):$$:$(date -u +%s):$RANDOM"

cursor_lock_attempts=0
while ! mkdir "$CURSOR_LOCK" 2>/dev/null; do
    if _steal_stale_event_dir "$CURSOR_LOCK"; then
        continue
    fi
    (( cursor_lock_attempts >= 20 )) && exit 0
    sleep 0.05
    cursor_lock_attempts=$((cursor_lock_attempts + 1))
done
printf '%s' "$CURSOR_LOCK_TOKEN" > "$CURSOR_LOCK/owner" 2>/dev/null || {
    rmdir "$CURSOR_LOCK" 2>/dev/null || true
    exit 0
}
cleanup_cursor_lock() {
    [[ -r "$CURSOR_LOCK/owner" ]] || return
    [[ "$(< "$CURSOR_LOCK/owner")" == "$CURSOR_LOCK_TOKEN" ]] || return
    command -p rm -f "$CURSOR_LOCK/owner" 2>/dev/null || true
    rmdir "$CURSOR_LOCK" 2>/dev/null || true
}
trap cleanup_cursor_lock EXIT

# Both tools are required before the cursor can move. A GC rewrite publishes a
# recoverable inode-pinned cursor mapping before replacing the journal; finish
# that mapping here if GC died in the narrow replace-to-cursor window.
command -v jq >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1 || exit 0
CURSOR_PENDING="${CURSOR}.gc-pending"
if [[ -f "$CURSOR_PENDING" ]]; then
    python3 - "$EVENTS" "$CURSOR" "$CURSOR_PENDING" <<'PY' 2>/dev/null || exit 0
import json
import os
import sys

events, cursor, pending = sys.argv[1:]
temp = None
try:
    payload = json.loads(open(pending, encoding="ascii").read())
    stat = os.stat(events)
    value = payload["cursor"]
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError("invalid cursor")
    if payload.get("device") != stat.st_dev or payload.get("inode") != stat.st_ino:
        raise SystemExit(0)
    temp = f"{cursor}.recover.{os.getpid()}"
    with open(temp, "x", encoding="ascii") as handle:
        handle.write(str(value))
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, cursor)
    os.unlink(pending)
except SystemExit:
    raise
except Exception:
    if temp is not None:
        try:
            os.unlink(temp)
        except OSError:
            pass
    raise SystemExit(1)
PY
fi

size=$(wc -c < "$EVENTS" 2>/dev/null | tr -d ' ')
[[ "$size" =~ ^[0-9]+$ ]] || exit 0

offset=0
[[ -f "$CURSOR" ]] && offset=$(tr -d ' \n' < "$CURSOR" 2>/dev/null)
[[ "$offset" =~ ^[0-9]+$ ]] || offset=0
# File shrank/rotated -> reset to start (don't trust a stale offset).
(( offset > size )) && offset=0
# Nothing new appended since last scan.
(( offset >= size )) && exit 0

# Never burn the slice we cannot deliver: the cursor advance below is one-way,
# and both tools are used unconditionally after it. jq is NOT a given here -
# session-start.sh exits silently without it - and losing the slice is worse
# than re-scanning it next turn.
# Scan only the slice [offset, size): bound the read with `head -c` so events
# appended AFTER we captured `size` are NOT consumed here -- the cursor only
# advances to `size`, so a racing append belongs to the next run, never both
# (once-per-offer). Per-line JSON parse so a malformed/truncated line is skipped,
# not fatal. Latest think_offered wins; carry its offer_line, the authoritative
# command the offer path recorded (a reconstructed bare `/think <id>` is a single
# non-mode token the router rejects -- skills/think/SKILL.md). Older offers in
# the slice ride along as bare ids. \x1f-separated, not tab: tab is
# IFS-whitespace, so `read` would collapse an empty offer_line and shift the
# ride-along list into it (same trap the enrichment parse below documents).
parsed=$(tail -c +"$((offset + 1))" "$EVENTS" 2>/dev/null | head -c "$((size - offset))" 2>/dev/null | python3 -c '
import sys, json, re
# Read the slice as BYTES and require exactly the count the caller measured.
# A completion sentinel alone proves the parser ran, not that it was fed: a
# failing `tail`/`head` hands it empty stdin, it finds no offers, and the caller
# advances over a live offer. A short read exits without the sentinel instead,
# so the cursor stays put and the next turn re-scans (verified: a `tail` that
# exits 1 used to consume the slice and emit nothing).
expected = int(sys.argv[1])
raw = sys.stdin.buffer.read()
if len(raw) != expected:
    sys.exit(1)
# Decode with replacement, never raising: .fno/events.jsonl has non-Python
# appenders (the stop hooks printf shell-interpolated lines) and `head -c` cuts
# on a byte boundary, so a truncated multi-byte char is reachable. Decoding at a
# bare `for line in sys.stdin` would raise OUTSIDE the per-line try below and
# take every offer in the slice down with it. A mangled line instead fails
# json.loads inside the try and is skipped, as the file header already claims.
# The reminder wrapper is hook-owned; offer_line is free text (spawn_think
# interpolates a filesystem path into it), so it gets the same defang the
# enrichment parse applies to title/details or it could close the wrapper early.
_TAG = re.compile(r"<\s*(/?)\s*system-reminder\s*>", re.IGNORECASE)
nid = ""
offer = ""
older = []
for line in raw.decode("utf-8", "replace").splitlines():
    line = line.strip()
    if not line:
        continue
    # Every field access stays INSIDE the try, and both carried values are
    # type-checked: a valid-JSON non-object line, a non-dict "data", or a
    # non-string node_id/offer_line must skip that line, never kill the parse.
    # The old code tolerated a bad id by overwriting it; accumulating older ids
    # would instead carry it to the join, and a dead parse here means the cursor
    # advances over the whole slice and destroys every offer in it.
    try:
        ev = json.loads(line)
        if ev.get("type") != "think_offered":
            continue
        data = ev.get("data") or {}
        x = data.get("node_id")
        o = data.get("offer_line")
    except Exception:
        continue
    if isinstance(x, str) and x:
        if nid:
            older.append(nid)
        nid = _TAG.sub(r"[\1system-reminder]", x)
        offer = _TAG.sub(r"[\1system-reminder]", o) if isinstance(o, str) else ""
# Cap the ride-along list. Not defensive padding: on a COLD START there is no
# cursor, so offset is 0 and the entire history is one slice - measured at 418
# ids and 3472 chars against the events.jsonl in this repo. Bursts are small (4 is
# the observed max), which is why an earlier cut of this deleted the cap as
# speculative; that reasoning missed the offset==0 case entirely.
MAX = 5
if len(older) > MAX:
    older = older[-MAX:] + ["+%d more" % (len(older) - MAX)]
# Trailing \x04 is a completion sentinel: `command -v python3` proves presence,
# not success, and a present-but-broken interpreter (dead pyenv shim, broken
# venv) would emit nothing while the cursor advanced anyway. The caller refuses
# to advance without this byte, so a failed parse re-scans instead of eating the
# slice. A clean scan that found no offer still prints it, and still advances.
sys.stdout.write("\x1f".join([nid, offer, ", ".join(older)]) + "\x04")
' "$((size - offset))" 2>/dev/null)

# No completion sentinel -> the parse died rather than finding nothing. Leave the
# cursor where it is so the next turn re-scans; burning the slice on a broken
# interpreter is the loss this hook exists to prevent.
[[ "$parsed" == *$'\x04' ]] || exit 0
parsed="${parsed%$'\x04'}"

# Advance the cursor to the captured EOF regardless of what we found -- consuming
# exactly the [offset, size) slice we scanned is what makes the reminder fire
# once per offer.
printf '%s' "$size" > "$CURSOR" 2>/dev/null || true

IFS=$'\x1f' read -r node_id offer_cmd older_ids <<<"$parsed"
[[ -n "$node_id" ]] || exit 0

# The cursor already consumed these, so this clause is their only surfacing.
also_born_line=""
[[ -n "${older_ids:-}" ]] && also_born_line="
Also born this gap (not offered separately): ${older_ids}."

# Suppressing the NEWEST offer must not destroy the older ids with it: the guard
# below exits before any reminder is built, and the cursor is already past the
# whole slice (codex P2). Naming an id is not an offer, so it is safe to list
# them unresolved - a stale id is noise, a discarded one is data loss.
emit_older_only() {
    [[ -n "${older_ids:-}" ]] || exit 0
    jq -n --arg ctx "<system-reminder>
Nodes born this gap, not offered: ${older_ids}.
</system-reminder>" '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'
    exit 0
}

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
    # `cd` failure exits 1 -- the SAME code as an authoritative not-found -- so a
    # deleted worktree (archive-worktree.sh / `fno worktree cleanup` can remove one
    # under a live session) would read as "node absent" and destroy a live offer.
    # Map it to 99 so it lands in the degrade-to-surfacing branch below.
    node_json=$( cd "$REPO_ROOT" 2>/dev/null || exit 99; with_timeout 3 fno backlog get "$node_id" 2>/dev/null )
    _get_rc=$?
    if [[ "$_get_rc" -eq 0 ]]; then
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
            emit_older_only
        fi
    elif [[ "$_get_rc" -eq 1 ]]; then
        # Authoritative not-found -> phantom, suppress.
        emit_older_only
    fi
    # Any OTHER nonzero degrades to surfacing, because the cursor already
    # advanced and suppressing would discard a possibly-real offer for good.
    # Only rc 1 means "graph read cleanly, node absent": `fno backlog get` exits
    # 3 on an unreadable graph (GRAPH_UNREADABLE_EXIT, cli/src/fno/graph/cli.py),
    # 2 on a click usage error, 124 when with_timeout kills a wedged call, and 99
    # when the `cd` above failed. Every one of those is transient or our own
    # fault, never evidence the node does not exist.
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
offer, not something that already ran - nothing was spawned.${also_born_line}
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
            cand_id=$( cd "$REPO_ROOT" && with_timeout 3 fno backlog ready 2>/dev/null | python3 -c '
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
                cand_id=$( cd "$REPO_ROOT" && with_timeout 3 fno backlog next 2>/dev/null | python3 -c '
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
                cand_title=$( cd "$REPO_ROOT" && with_timeout 3 fno backlog get "$cand_id" 2>/dev/null | python3 -c '
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
Run \`${offer_cmd}\` now, or skip?${also_born_line}${ondeck_line}

This is an offer, not something that already ran - nothing was spawned.
</system-reminder>"
    fi
fi

# jq presence was checked before the cursor advanced, so reaching here means it
# exists. All node text reaches JSON only through --arg, so backticks / quotes /
# $() in a title render literally and never trigger shell expansion (AC2-EDGE).
jq -n --arg ctx "$reminder" \
    '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":$ctx}}'

exit 0
