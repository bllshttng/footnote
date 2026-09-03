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

raw = sys.stdin.read()
start = raw.find("{")
if start < 0:
    sys.exit(0)
try:
    payload = json.loads(raw[start:])
except Exception:
    sys.exit(0)

rows = payload.get("decisions") or []
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
    subject = str(row.get("subject") or "").strip()
    if subject and subject not in subjects:
        subjects.append(subject)

print("## Standing law")
print()
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
