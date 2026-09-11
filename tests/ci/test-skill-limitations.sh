#!/usr/bin/env bash
# Positive-control wrapper for check-skill-limitations.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GATE="${REPO_ROOT}/scripts/ci/check-skill-limitations.sh"
FIXTURES="${REPO_ROOT}/tests/fixtures/skill-limitations"

fail() {
    printf 'test-skill-limitations: FAIL: %s\n' "$*" >&2
    exit 1
}

[[ -f "$GATE" ]] || fail "gate missing at ${GATE}"
bash -n "$GATE" || fail "gate failed bash -n"
bash "$GATE" --selftest "$FIXTURES" || fail "gate selftest failed"

output="$(bash "$GATE")" || fail "shipped skills tree did not pass"
# Count the same way the gate does: one entry per skills/*/SKILL.md. A
# hardcoded count here broke every skill addition until someone noticed CI.
skill_count="$(find "${REPO_ROOT}/skills" -mindepth 2 -maxdepth 2 -name SKILL.md | wc -l | tr -d ' ')"
grep -Fq "${skill_count} skill file(s) passed" <<<"$output" \
    || fail "positive control did not report all ${skill_count} skill files"

printf 'test-skill-limitations: PASS\n'
