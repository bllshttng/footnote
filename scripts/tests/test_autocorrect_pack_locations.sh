#!/usr/bin/env bash
# Tests for relative-LOCATION resolution in scripts/autocorrect-pack.sh:
# a git-rule-edit row resolves against CLAUDE_DIR, a skill-commit row
# against the repo root, an insights-tag row is evidence (its `skill=`
# pair pulls the named skill in), and any other relative source adds
# nothing without being counted as a fixture row. Absolute locations keep
# today's behavior. Isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE +
# FNO_GRAPH_PATH so the real roots are never touched.

set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$SCRIPT_DIR/../autocorrect-pack.sh"
PASS=0
FAIL=0

pass() {
    echo "  PASS: $1"
    PASS=$((PASS + 1))
}
fail() {
    echo "  FAIL: $1"
    FAIL=$((FAIL + 1))
}
summary() {
    echo ""
    echo "PASS=$PASS FAIL=$FAIL"
    [[ $FAIL -eq 0 ]]
}

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/fno/postmortems" "$D/claude/rules"

TS="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
LOG="$D/fno/corrections.log"
real_pm="$D/fno/postmortems/pm-a.md"
printf '# body\n' > "$real_pm"
printf 'the rule text\n' > "$D/claude/rules/a.md"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# One row per resolution branch, all in the window.
{
    printf '%s | S1 | git-rule-edit | rules/a.md | enforce the ban\n' "$TS"
    printf '%s | S1 | skill-commit | skills/target/SKILL.md | sha=abc123def456 ref=r1#1\n' "$TS"
    printf '%s | S2 | insights-tag | 2026-09-21.md:12 | quote (x2, s1, signal=workflow_friction skill=target) #agent-correction\n' "$TS"
    printf '%s | S1 | target-postmortem | %s | wall: a\n' "$TS" "$real_pm"
    printf '%s | S1 | other-source | notes/b.md | evidence only\n' "$TS"
} > "$LOG"

run_pack() {
    CLAUDE_DIR_OVERRIDE="$D/claude" FNO_HOME="$D/fno" FNO_GRAPH_PATH="$D/absent-graph.json" \
        bash "$PACK" --dry-run --window 30d
}

echo "T01: relative locations resolve by source"
PACKET="$(run_pack)"
if [[ $? -ne 0 ]]; then
    fail "pack exited nonzero"
    summary
    exit 1
fi
if printf '%s\n' "$PACKET" | grep -q -- "- file: $D/claude/rules/a.md"; then
    pass "git-rule-edit row resolved against CLAUDE_DIR"
else
    fail "git-rule-edit row not resolved"
fi
if printf '%s\n' "$PACKET" | grep -qF 'the rule text'; then
    pass "resolved rule carries full text"
else
    fail "resolved rule missing full text"
fi
SKILL_COUNT=$(printf '%s\n' "$PACKET" | grep -c -- '- file: .*skills/target/SKILL.md')
if [[ "$SKILL_COUNT" -eq 1 ]]; then
    pass "skill-commit and skill=target resolve to one skills/target/SKILL.md entry"
else
    fail "expected 1 skills/target/SKILL.md entry, got $SKILL_COUNT"
fi

echo "T02: the insights-tag report file is evidence, not a pointer"
if printf '%s\n' "$PACKET" | grep -q -- '- file: .*2026-09-21\.md'; then
    fail "report file rendered as an implicated pointer"
else
    pass "no entry for the report file"
fi
if printf '%s\n' "$PACKET" | grep -q -- '- file: rules/a.md'; then
    fail "an unresolved relative path leaked into implicated_rules"
else
    pass "no unresolved relative entry"
fi
if printf '%s\n' "$PACKET" | grep -q 'not found at packet build time'; then
    fail "a dead entry rendered"
else
    pass "no dead entries"
fi

echo "T03: counters stay honest"
FIXTURES=$(printf '%s\n' "$PACKET" | sed -n 's/^skipped_fixture_rows: //p')
DEADS=$(printf '%s\n' "$PACKET" | sed -n 's/^skipped_dead_rows: //p')
if [[ "$FIXTURES" == "0" && "$DEADS" == "0" ]]; then
    pass "no fixture or dead rows counted"
else
    fail "counters moved: fixture=$FIXTURES dead=$DEADS"
fi
if printf '%s\n' "$PACKET" | grep -q -- '- file: .*notes/b\.md'; then
    fail "other-source relative row leaked an entry"
else
    pass "other-source relative row skipped silently"
fi

summary
exit $?
