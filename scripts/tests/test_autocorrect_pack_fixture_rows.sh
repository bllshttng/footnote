#!/usr/bin/env bash
# Tests for the fixture-row filter in scripts/autocorrect-pack.sh:
# a vanished LOCATION outside the resolved postmortems root is counted into
# skipped_fixture_rows and never rendered into implicated_rules, while a
# vanished path under the root still renders as deleted.
# Isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE + FNO_GRAPH_PATH so the real
# ~/.fno and ~/.claude are never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$SCRIPT_DIR/../autocorrect-pack.sh"
PASS=0
FAIL=0

pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
summary() {
    echo ""
    echo "PASS=$PASS FAIL=$FAIL"
    [[ $FAIL -eq 0 ]]
}

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/fno/postmortems" "$D/claude"

TS="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
LOG="$D/fno/corrections.log"

real_pm="$D/fno/postmortems/real-pm.md"
printf '# postmortem body\nsome finding\n' > "$real_pm"
vanished="$D/tmp-sibling/pm.md"

{
    printf '%s | S1 | target-postmortem | %s | wall: hit\n' "$TS" "$real_pm"
    printf '%s | S1 | target-postmortem | %s | fixture: leak\n' "$TS" "$vanished"
} > "$LOG"

run_pack() {
    CLAUDE_DIR_OVERRIDE="$D/claude" FNO_HOME="$D/fno" FNO_GRAPH_PATH="$D/absent-graph.json" \
        bash "$PACK" --dry-run --window 30d
}

# ---- T01: implicated_rules renders the resolvable row only ----
echo "T01: resolvable row renders, fixture row does not"
PACKET="$(run_pack)" || { fail "pack exited nonzero"; summary; exit 1; }
if printf '%s' "$PACKET" | grep -q "postmortems/real-pm.md"; then
    pass "real postmortem rendered"
else
    fail "real postmortem missing from implicated_rules"
fi
RULES_SECTION=$(printf '%s\n' "$PACKET" | sed -n '/^implicated_rules:/,$p')
if printf '%s\n' "$RULES_SECTION" | grep -q "tmp-sibling/pm.md"; then
    fail "fixture path rendered into implicated_rules"
else
    pass "fixture path absent from implicated_rules"
fi
if printf '%s' "$PACKET" | grep -q '<file deleted or not found at packet build time>'; then
    fail "fixture row rendered the deleted marker"
else
    pass "fixture row not rendered as deleted either"
fi

# ---- T02: the skip is counted out loud ----
echo "T02: skipped_fixture_rows counted"
if printf '%s' "$PACKET" | grep -q '^skipped_fixture_rows: 1$'; then
    pass "skipped_fixture_rows: 1"
else
    fail "skipped_fixture_rows missing or wrong"
fi

# ---- T03: event_count still counts every window row ----
echo "T03: event_count unchanged by the filter"
if printf '%s' "$PACKET" | grep -q '^event_count: 2$'; then
    pass "event_count: 2 (filter does not hide input size)"
else
    fail "event_count wrong"
fi

# ---- T03b: dead non-postmortem rows count apart from fixtures ----
echo "T03b: dead rule rows land in skipped_dead_rows, not skipped_fixture_rows"
printf '%s | S1 | git-rule-edit | %s/gone-rule.md | wall: aged\n' "$TS" "$D" >> "$LOG"
PACKET3="$(run_pack)"
if printf '%s\n' "$PACKET3" | grep -q '^skipped_dead_rows: 1$'; then
    pass "skipped_dead_rows: 1"
else
    fail "skipped_dead_rows missing or wrong"
fi
if printf '%s\n' "$PACKET3" | grep -q '^skipped_fixture_rows: 1$'; then
    pass "dead rule row not counted as fixture"
else
    fail "dead rule row miscounted as fixture"
fi

# ---- T04: a vanished path UNDER the root still renders as deleted ----
echo "T04: vanished real-corpus row still renders deleted"
aged_pm="$D/fno/postmortems/aged-pm.md"
printf '%s | S1 | target-postmortem | %s | wall: aged out\n' "$TS" "$aged_pm" >> "$LOG"
PACKET2="$(run_pack)"
if printf '%s' "$PACKET2" | grep -q '<file deleted or not found at packet build time>'; then
    pass "aged real row renders the deleted marker"
else
    fail "aged real row lost"
fi
if printf '%s' "$PACKET2" | grep -q '^skipped_fixture_rows: 1$'; then
    pass "aged real row not counted as fixture"
else
    fail "aged real row miscounted as fixture"
fi

# ---- T05: an existing file outside the root still renders full text ----
echo "T05: existing files render regardless of root"
if printf '%s' "$PACKET2" | grep -q "postmortems/real-pm.md"; then
    pass "real row still renders"
else
    fail "real row lost after T04 append"
fi

summary
exit $?
