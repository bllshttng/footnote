#!/usr/bin/env bash
# Closure probe for the agent field population contract: reruns the live
# field-coverage measurement and requires the five contract fields to be
# classified, never dead. Dead fields outside the contract are the
# evaluator's own finding and do not block this marker. Read-only. Fail
# closed: UNMEASURED or missing JSON propagates the evaluator's exit code,
# and the positive marker prints only on a complete measured report.
set -u
repo="$(git rev-parse --show-toplevel 2>/dev/null)" || repo="$PWD"
cd "$repo" || exit 1
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
fno-py doctor lint field-coverage --live --json >"$tmp"
status=$?
if [ "$status" -ne 0 ]; then
  # exit 1 with a measurable report still lets the contract probe run;
  # UNMEASURED (2) or an unparseable report fails closed.
  if [ "$status" -ne 1 ] || ! python3 -c "import json,sys; p=json.load(open(sys.argv[1])); raise SystemExit(0 if p.get('status') != 'unmeasured' else 1)" "$tmp" 2>/dev/null; then
    cat "$tmp" >&2
    echo "agent-field-population-contract: evaluator exited $status" >&2
    exit "$status"
  fi
fi
python3 scripts/ci/check-agent-field-population-contract.py <"$tmp"
