#!/usr/bin/env bash
# Stop + SessionStart hook: report the operator-capture queue depth, so the
# king's capture loop is a measured number at the moment, not prose to
# remember.
#
# The failure this closes: a king records from the direction it is pushed.
# Worker mail arrives as a discrete event (id, queue, ack), so it gets
# recorded; operator conversation is a stream with no boundary, so it does
# not - one reign recorded twenty-two rulings from worker mail and zero from
# operator conversation. `fno inbox operator` derives the queue from the
# session transcript; this hook is the push mechanism that surfaces its
# depth at the two turn boundaries a king actually pauses on.
#
# Depth-gated on purpose: it prints nothing when the queue is empty, so a
# quiet session costs one bounded read and no context. A non-zero depth
# prints the count, the oldest turn's age, and its excerpt - the same
# measured line inject-mail-notify.sh delivers for mail, which is the channel
# that gets recorded.
#
# NEVER gate this on a crown. `hooks/king-postcompact-reinject.sh` gated on
# `crown_level`, a field a different verb writes, and exited 0 silently for
# every king in the fleet. The queue depth here is state the verb derives
# for itself; a session with an empty queue is the steady state.
#
# Hook contract: stdout is appended to the session prompt; exit 0 always.
set -uo pipefail

command -v fno >/dev/null 2>&1 || exit 0

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WT_LIB="$HOOK_DIR/../scripts/lib/with-timeout.sh"
[[ -f "$WT_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/with-timeout.sh
source "$WT_LIB" 2>/dev/null || exit 0

rc=0
payload=$(with_timeout 10 fno inbox operator status --json 2>/dev/null) || rc=$?

# A non-zero exit is not silence: collapsing a failed read into an empty
# string is the absence-as-success trap. But exit 2 is Typer's "no such
# command", which a DEPLOYED fno older than this feature returns on every
# turn until someone runs `fno doctor update`; nagging forever is noise.
if [[ $rc -ne 0 ]]; then
    [[ $rc -eq 2 ]] && exit 0
    printf '## Operator capture\n\ncould not be read (fno inbox operator status exit %s). Run it directly.\n' "$rc"
    exit 0
fi

printf '%s' "$payload" | python3 -c '
import json
import sys

raw = sys.stdin.read()

# Take the first object that LOOKS LIKE THIS PAYLOAD, not the first that
# merely parses: the verb shares stdout with whatever preamble the deployed
# fno prints, and a preamble can carry a brace. "depth" present is the
# marker; an unreadable payload is a REPORT, never silence.
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
        if "depth" in candidate:
            payload = candidate
            break
    at = raw.find("{", at + 1)

if payload is None:
    if saw_object:
        print("## Operator capture")
        print()
        print("read returned a JSON object with no depth key; run `fno inbox operator status` directly.")
    sys.exit(0)

depth = payload.get("depth")
if not isinstance(depth, int) or depth <= 0:
    sys.exit(0)

age = payload.get("oldest_age_s")
age_text = f", oldest {age}s old" if isinstance(age, int) else ""
excerpt = payload.get("oldest_excerpt") or ""
print(f"## Operator capture: {depth} undispositioned operator turn(s){age_text}")
print()
print("Disposition each before the tick ends: record with `fno backlog idea "
      "--source-kind operator_request`, `fno backlog capture add`, or `fno "
      "inbox law set`, then `fno inbox operator ack <turn-id> --outcome "
      "law:<id>|capture:<fu-id>|node:<id>|nothing`.")
if excerpt:
    print()
    print(f"Oldest: {excerpt}")
'
exit 0
