#!/usr/bin/env bash
# Verify that every checked-in law command form reaches the skill text that
# teaches it. The live mode also checks the machine-local law store.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$REPO_ROOT"

REGISTRY="scripts/ci/law-command-forms.txt"
CANARY="scripts/ci/fixtures/law-command-form-canary.md"

fail() { echo "check-law-command-forms: $*" >&2; exit 1; }
live=0
if [[ "${1:-}" == "--live" ]]; then
    live=1
    shift
fi
[[ $# -eq 0 ]] || fail "unknown argument: $1"
[[ -r "$REGISTRY" ]] || fail "registry not found at $REGISTRY"
[[ -r "$CANARY" ]] || fail "canary not found at $CANARY"

declare -a FORMS=() LAWS=() TARGETS=()
rows=0
non_exempt=0
line_no=0
while IFS= read -r line || [[ -n "$line" ]]; do
    line_no=$((line_no + 1))
    case "$line" in ''|'#'*) continue ;; esac
    form=""; laws=""; targets=""; extra=""
    IFS='|' read -r form laws targets extra <<<"$line"
    [[ -n "$form" && -n "$laws" && -n "$targets" && -z "$extra" ]] ||
        fail "$REGISTRY:$line_no: malformed row, want <form>|<law ids>|<paths or exempt reason>"
    for law in $laws; do
        [[ "$law" =~ ^d-[0-9a-f]{8}$ ]] ||
            fail "$REGISTRY:$line_no: malformed law id '$law'"
    done
    if [[ "$targets" == exempt:* ]]; then
        [[ -n "${targets#exempt:}" ]] || fail "$REGISTRY:$line_no: exempt row needs a reason"
    else
        non_exempt=$((non_exempt + 1))
        IFS=',' read -r -a path_list <<<"$targets"
        for path in "${path_list[@]}"; do
            [[ -n "$path" && -f "$path" ]] ||
                fail "$REGISTRY:$line_no: named path does not exist: $path"
        done
    fi
    FORMS+=("$form"); LAWS+=("$laws"); TARGETS+=("$targets")
    rows=$((rows + 1))
done <"$REGISTRY"

[[ "$rows" -gt 0 ]] || fail "$REGISTRY holds no rows"
[[ "$non_exempt" -gt 0 ]] || fail "registry has no non-exempt rows"

grep -F -- "${FORMS[0]}" "$CANARY" >/dev/null 2>&1 ||
    fail "CONTROL FAILED: canary lacks '${FORMS[0]}'"

path_count=0
path_seen=""
for i in "${!FORMS[@]}"; do
    form="${FORMS[$i]}"; laws="${LAWS[$i]}"; targets="${TARGETS[$i]}"
    [[ "$targets" == exempt:* ]] && continue
    IFS=',' read -r -a path_list <<<"$targets"
    for path in "${path_list[@]}"; do
        case $'\n'"$path_seen"$'\n' in *$'\n'"$path"$'\n'*) ;; *)
            path_seen="${path_seen}${path_seen:+$'\n'}$path"
            path_count=$((path_count + 1))
            ;;
        esac
        if ! grep -F -- "$form" "$path" >/dev/null 2>&1; then
            echo "LAW FORM MISSING: '$form' (law $laws) not in $path" >&2
            exit 1
        fi
    done
done

if [[ "$live" -eq 1 ]]; then
    command -v fno >/dev/null 2>&1 || {
        echo "check-law-command-forms: could not read live law" >&2
        exit 2
    }
    live_json=""
    if ! live_json="$(fno backlog decisions --lane law --state live --limit 0 -J 2>/dev/null)" ||
       [[ -z "$live_json" ]]; then
        echo "check-law-command-forms: could not read live law" >&2
        exit 2
    fi
    registered_ids=""
    for laws in "${LAWS[@]}"; do
        for law in $laws; do
            registered_ids="${registered_ids}${registered_ids:+$'\n'}$law"
        done
    done
    export REGISTERED_LAW_IDS="$registered_ids"
    printf '%s' "$live_json" | python3 -c '
import json
import os
import re
import sys

raw = sys.stdin.read()
decoder = json.JSONDecoder()
payload = None
for offset, char in enumerate(raw):
    if char != "{":
        continue
    try:
        candidate, _ = decoder.raw_decode(raw[offset:])
    except json.JSONDecodeError:
        continue
    if isinstance(candidate, dict) and "decisions" in candidate:
        payload = candidate
        break
if payload is None or not isinstance(payload.get("decisions"), list):
    print("check-law-command-forms: could not read live law", file=sys.stderr)
    raise SystemExit(2)
registered = set(os.environ.get("REGISTERED_LAW_IDS", "").splitlines())
bad = False
for row in payload["decisions"]:
    if not isinstance(row, dict):
        continue
    decision = str(row.get("decision") or "")
    if not re.search(r"(?:^|\s)(?:fno [a-z]|--[a-z])", decision):
        continue
    law_id = str(row.get("decision_id") or row.get("id") or "unknown")
    if law_id in registered:
        continue
    subject = str(row.get("subject") or "unknown")
    print(f"UNREGISTERED LAW FORM: {law_id} {subject}", file=sys.stderr)
    bad = True
raise SystemExit(1 if bad else 0)
'
    rc=$?
    [[ "$rc" -eq 0 ]] && : || exit "$rc"
fi

echo "law command-form check: checked ${non_exempt} form(s) across ${path_count} path(s); controls fired"
