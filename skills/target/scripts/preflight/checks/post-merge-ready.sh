#!/usr/bin/env bash
# post-merge-ready.sh - target-preflight check (ab-dba85fcc, US1)
# Contract: stdout first line "post-merge-ready {pass|warn|unknown} {message}"
# Exit: always 0. NEVER `fail` - post-merge config is post-merge-only state and
# must not block a /target build (warn/unknown only).
#
# Consumes the single readiness oracle so the rule lives in exactly one place
# (cli/src/fno/config_cli.py:post_merge_readiness). The warn fires only where
# the gap actually bites (the oracle's `unconfigured` already requires repo
# activity); a fresh repo is `dormant` -> pass/silent.
#
# FNO_BIN overrides the binary (tests / non-PATH installs).

# Deliberately NO `set -e`: a failing/old oracle must degrade to `unknown`,
# never crash the check (the runner would report a check bug).
set -uo pipefail

NAME="post-merge-ready"
FNO="${FNO_BIN:-fno}"

if ! command -v "$FNO" >/dev/null 2>&1; then
    echo "$NAME unknown $FNO not found on PATH (run: fno update)"
    exit 0
fi

OUT="$("$FNO" config doctor --post-merge --json 2>/dev/null)"
RC=$?
if [[ $RC -ne 0 || -z "$OUT" ]]; then
    # AC1-FR: an installed fno too old to expose --post-merge degrades here.
    echo "$NAME unknown post-merge oracle unavailable - installed fno too old? (run: fno update)"
    exit 0
fi

STATUS="$(printf '%s' "$OUT" | python3 -c 'import sys, json
try:
    print(json.load(sys.stdin).get("status") or "")
except Exception:
    print("")' 2>/dev/null)"

case "$STATUS" in
    unconfigured)
        # AC1-HP / AC1-UI: one actionable line naming the key and the consequence.
        echo "$NAME warn config.post_merge.parking_lot_path is unset - /fno:pr merged prose+triage will be skipped after a merge (set it: fno setup post-merge)"
        ;;
    ready | opted_out | dormant)
        echo "$NAME pass post-merge config $STATUS"
        ;;
    error)
        # AC1-ERR: a settings load failure surfaces as unknown carrying the cause.
        CAUSE="$(printf '%s' "$OUT" | python3 -c 'import sys, json
try:
    print(json.load(sys.stdin).get("cause") or "settings load error")
except Exception:
    print("settings load error")' 2>/dev/null)"
        echo "$NAME unknown post-merge config could not be read: $CAUSE"
        ;;
    *)
        echo "$NAME unknown unrecognized post-merge verdict"
        ;;
esac
exit 0
