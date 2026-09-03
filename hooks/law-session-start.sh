#!/usr/bin/env bash
# SessionStart hook: name the standing operator law, so no session re-derives a
# ruling that already exists.
#
# The failure this closes, measured 2026-09-02: a session loaded using-fno at
# full context, was asked about the language boundary, searched docs/,
# AGENTS.md and the loaded rule files, concluded the direction was
# unrecorded, and argued against a port - while law d-1d474a79 mandating
# exactly that port had been live for six hours. It never searched the
# decision store, because nothing told it to.
#
# Why the nag channel and not a doc or a skill edit. SessionStart fires on
# startup, resume, clear AND compact. Compaction is the exact moment an agent
# loses the reasoning behind a ruling while keeping a vague sense that one was
# made, so it is the highest-risk moment for re-derivation. A doc helps only an
# agent that goes looking; a skill clause helps only on the one careful read at
# load. This line re-states itself at all four triggers and reads the store
# each time, so it cannot drift from the store.
#
# It names SUBJECTS, not just a count. The specimen above is an agent that did
# not know a `python-to-rust-conversion` ruling existed; a bare count would not
# have told it. The subject is the searchable key.
#
# NEVER gate this on a crown. Law is fleet-wide and applies to every session,
# crowned or not. `hooks/king-postcompact-reinject.sh` is the cautionary case:
# it gates on `crown_level`, a field a DIFFERENT verb is responsible for
# writing, and it has therefore exited 0 silently for every king in the fleet.
# Prefer a gate on state the hook can read for itself. The law list needs no
# precondition, which is what makes it the safer design.
#
# Hook contract: stdout is appended to the session prompt; exit 0 always.
set -uo pipefail

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT_LIB="$HOOK_DIR/../scripts/lib/with-timeout.sh"

command -v fno >/dev/null 2>&1 || exit 0
[[ -f "$WT_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/with-timeout.sh
source "$WT_LIB" 2>/dev/null || exit 0

rc=0
# 10s, against a measured ~1s read. The verb used to take 32s because
# `_derive_coord_expiry_ref` re-read the whole graph once per coord row; that
# N+1 is fixed in this same change. A DEPLOYED fno older than that fix still
# takes 32s, fires this bound, and says so - which is the correct report for a
# real 30-second stall, and it clears on `fno doctor update`.
payload=$(with_timeout 10 fno backlog decisions --lane law --state live --limit 0 -J 2>/dev/null) || rc=$?

# A non-zero exit is not silence: collapsing a failed read into an empty string
# is the absence-as-success trap. But exit 2 is Typer's "no such command",
# which a DEPLOYED fno older than this feature returns on EVERY session until
# someone runs `fno doctor update`; nagging forever is noise.
if [[ $rc -ne 0 ]]; then
    [[ $rc -eq 2 ]] && exit 0
    printf '## Standing law\n\ncould not be read (fno backlog decisions exit %s). Run it directly.\n' "$rc"
    exit 0
fi

printf '%s' "$payload" | python3 -c '
import json
import sys

RENDER_CAP = 8

def _fail(why):
    """An unreadable payload is a REPORT, never silence.

    The shell leg above already refuses to collapse a failed read into an
    empty string. This leg used to do exactly that: any parse error exited
    0, so a session start rendered nothing and looked identical to a store
    holding no law. That is the absence-as-success trap the header refuses,
    reintroduced eight lines later.
    """
    print("## Standing law")
    print()
    print(
        f"could not be read ({why}). The store may hold rulings this "
        "session has not seen. Run `fno backlog decisions --lane law "
        "--state live` directly."
    )
    sys.exit(0)


raw = sys.stdin.read()

# Scan every "{" and take the first object that LOOKS LIKE THIS PAYLOAD, not
# the first that merely parses. The verb shares stdout with whatever preamble
# the deployed fno prints, and such a preamble can carry a brace (a `dedup:`
# line, a config note quoting a dict). Taking the first parseable object read
# a quoted dict in the preamble as the answer and rendered nothing - the same
# absence-as-success trap, re-entered through a narrower door. "decisions"
# present is the marker that distinguishes the real payload from noise.
payload = None
decoder = json.JSONDecoder()
saw_object = False
at = raw.find("{")
while at >= 0:
    try:
        candidate, _ = decoder.raw_decode(raw[at:])
    except ValueError:
        candidate = None
    if isinstance(candidate, dict):
        saw_object = True
        if "decisions" in candidate:
            payload = candidate
            break
    at = raw.find("{", at + 1)

if payload is None:
    _fail(
        "output carried a JSON object with no decisions key"
        if saw_object
        else "no JSON object in the output"
    )

rows = payload.get("decisions")
if not isinstance(rows, list):
    _fail("decisions is not a list")
total = payload.get("total")
if not isinstance(total, int):
    total = len(rows)
damaged = payload.get("damaged") or 0

# No live law is the correct steady state on a fresh install, and silence
# renders it. A damaged store is NOT that state and is reported even at zero:
# unreadable records and no records are different facts.
if not total and not damaged:
    sys.exit(0)

subjects = []
for row in rows:
    if not isinstance(row, dict):
        continue
    subject = str(row.get("subject") or "").strip()
    if subject and subject not in subjects:
        subjects.append(subject)

print("## Standing law")
print()

# Damage with nothing live is a damage report, not a law list. The old order
# printed "0 live ruling(s) the operator already made" and then told the
# reader to go read one of them.
if not total:
    print(
        f"{damaged} record(s) in the store could not be parsed, so whether "
        "any law is live here is unknown. Run `fno backlog decisions --lane "
        "law --state live` directly."
    )
    sys.exit(0)

shown = subjects[:RENDER_CAP]
tail = ""
if len(subjects) > len(shown):
    tail = f", and {len(subjects) - len(shown)} more"
if shown:
    print(
        f"{total} live ruling(s) the operator already made: "
        + ", ".join(shown)
        + tail
        + "."
    )
else:
    print(f"{total} live ruling(s) the operator already made.")
print()
print(
    "Read one before deciding a direction question that touches it: "
    "`fno backlog decisions <subject>`. A bare `fno backlog decisions` "
    "lists recent rulings across every subject. Do not re-derive a "
    "standing ruling."
)
if damaged:
    print()
    print(
        f"{damaged} record(s) in the store could not be parsed, so this "
        "list is incomplete."
    )
'
exit 0
