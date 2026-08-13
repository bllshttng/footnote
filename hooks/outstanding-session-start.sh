#!/usr/bin/env bash
# SessionStart hook: render what is waiting on a human - unharvested carve-outs
# and open operator questions.
#
# This file exists as its OWN hook because hooks/session-start.sh is the
# codex/gemini wrapper and is not registered in hooks/hooks.json; Claude
# registers each *-session-start.sh individually. A block that lived only in
# the wrapper reached every harness except the operator's own, which is the
# "guard placed on one of N reachable paths is decorative" pitfall. The wrapper
# now calls THIS script, so both harnesses run one implementation.
#
# Hook contract: stdout is appended to the session prompt; exit 0 always.
set -uo pipefail

HOOK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v fno >/dev/null 2>&1 || exit 0

WT_LIB="$HOOK_DIR/../scripts/lib/with-timeout.sh"
[[ -f "$WT_LIB" ]] || exit 0
# shellcheck source=../scripts/lib/with-timeout.sh
source "$WT_LIB" 2>/dev/null || exit 0

rc=0
body=$(with_timeout 3 fno outstanding 2>/dev/null) || rc=$?

if [[ $rc -eq 0 ]]; then
    # Empty is the correct steady state, and silence is how it renders.
    [[ -n "$body" ]] && printf '%s' "$body"
    exit 0
fi

# A non-zero exit is NOT silence - collapsing a failed read into an empty
# string is the absence-as-success trap this verb exists to close. But exit 2
# is Typer's "no such command", which is what a DEPLOYED fno older than this
# feature returns on every single session. Nagging about that on every start,
# forever, until someone runs `fno update`, is noise rather than signal, so it
# gets one quiet line and the loud path is reserved for a real failure: a fired
# timeout (124) or an unreadable store (1).
if [[ $rc -eq 2 ]]; then
    exit 0
fi

printf '## Outstanding for you\n\ncould not be read (fno outstanding exit %s). Run it directly.\n' "$rc"
exit 0
