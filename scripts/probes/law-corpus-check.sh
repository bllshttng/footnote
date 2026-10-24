#!/usr/bin/env bash
# Rerunnable probe. Runs the `rules-check` mode of `fno inbox law match` on a
# rule file against the live law store, and requires that every live law is
# carried by a rule or disposed of, that no row is invalid, and that the
# hold, freeze and naming families each hold at least one tier-0 rule. A
# pass prints the counts; exit 0. Any gap prints the answer; exit 1.
#
# Read-only: the mode reads the store and the file and writes nothing.
#
# Usage: bash scripts/probes/law-corpus-check.sh <rule-file.toml> [path-to-fno]
# The binary defaults to `fno` on PATH; pass a worktree build to probe a
# change that is not deployed yet.

set -u

FILE="${1:-}"
BIN="${2:-${FNO_BIN:-fno}}"
if [ -z "$FILE" ] || [ ! -f "$FILE" ]; then
    echo "probe: name a rule file that exists (got '${FILE}')" >&2
    exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "probe: jq is required" >&2
    exit 2
fi

request="$(jq -cn --arg file "$FILE" '{mode: "rules-check", file: $file}')"
answer="$(printf '%s' "$request" | "$BIN" inbox law match)"
if [ -z "$answer" ]; then
    echo "probe: no answer from '$BIN inbox law match'" >&2
    exit 2
fi

if printf '%s' "$answer" | jq -e '
    .ok == true
    and (.laws_missing | length) == 0
    and (.invalid | length) == 0
    and ((.tier0_by_family.hold // 0) > 0)
    and ((.tier0_by_family.freeze // 0) > 0)
    and ((.tier0_by_family.naming // 0) > 0)
' >/dev/null; then
    printf '%s' "$answer" | jq -c '{laws_live, laws_annotated, rules, by_tier, by_action, tier0_by_family, dispositions}'
    exit 0
fi
printf '%s\n' "$answer" | jq .
exit 1
