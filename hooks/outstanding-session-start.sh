#!/usr/bin/env bash
# SessionStart hook: render what is waiting on a human - unharvested carve-outs
# and open operator questions.
#
# Its OWN hook, not a block inside session-start.sh: that file is the
# codex/gemini wrapper and is absent from hooks/hooks.json, because Claude
# registers each *-session-start.sh individually. A block living only there
# reaches every harness except the operator's own. The wrapper calls this
# script, so both harnesses run one implementation.
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
body=$(with_timeout 3 fno inbox outstanding 2>/dev/null) || rc=$?

# Empty output on success is the correct steady state, and silence renders it.
[[ $rc -eq 0 ]] && { [[ -n "$body" ]] && printf '%s' "$body"; exit 0; }

# A non-zero exit is NOT silence: collapsing a failed read into an empty string
# is the absence-as-success trap this verb exists to close. But exit 2 is
# Typer's "no such command", which a DEPLOYED fno older than this feature
# returns on EVERY session until someone runs `fno doctor update`. Nagging forever is
# noise, so the loud path is reserved for a real failure: a fired bound (124)
# or an unreadable store (1).
[[ $rc -eq 2 ]] && exit 0

printf '## Outstanding for you\n\ncould not be read (fno inbox outstanding exit %s). Run it directly.\n' "$rc"
exit 0
