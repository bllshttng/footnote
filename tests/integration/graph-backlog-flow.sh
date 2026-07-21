#!/usr/bin/env bash
# End-to-end test of the graph backlog lifecycle.
# Adopt three mock plans -> render graph.md -> dry-run triage -> apply
# mutation -> re-render. Each step asserts a specific invariant so
# regression points are obvious.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TEST_HOME="$(mktemp -d)"
trap "rm -rf '$TEST_HOME'" EXIT

mkdir -p "$TEST_HOME/plans"
printf '# Auth feature\n' > "$TEST_HOME/plans/auth.md"
printf '# Billing feature (needs auth)\n' > "$TEST_HOME/plans/billing.md"
printf '# Dashboard feature (independent)\n' > "$TEST_HOME/plans/dashboard.md"

# Enable Obsidian so graph.md is rendered with the Kanban-plugin frontmatter
# (ab-917f813e gates that scaffolding on an Obsidian vault). Without this a
# clean checkout - where .fno/settings.yaml is gitignored - would default
# obsidian.enabled=false and the kanban-plugin assertion below would fail.
mkdir -p "$TEST_HOME/.fno"
printf 'config:\n  obsidian:\n    enabled: true\n    vault: testvault\n' \
    > "$TEST_HOME/.fno/settings.yaml"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# Step 1: Intake three mock plans to the backlog (no roadmap_id).
HOME="$TEST_HOME" python3 scripts/roadmap-tasks.py intake "$TEST_HOME/plans/auth.md" \
    --title "Auth" --priority p1 > /dev/null \
    || fail "intake auth"
HOME="$TEST_HOME" python3 scripts/roadmap-tasks.py intake "$TEST_HOME/plans/billing.md" \
    --title "Billing" --priority p2 > /dev/null \
    || fail "intake billing"
HOME="$TEST_HOME" python3 scripts/roadmap-tasks.py intake "$TEST_HOME/plans/dashboard.md" \
    --title "Dashboard" --priority p3 > /dev/null \
    || fail "intake dashboard"

# Step 2: graph.md renders with all three cards in the Next column.
GRAPH_MD="$TEST_HOME/.fno/graph.md"
[[ -f "$GRAPH_MD" ]] || fail "graph.md missing after intake"
grep -q "^kanban-plugin: board" "$GRAPH_MD" || fail "kanban-plugin frontmatter missing"
grep -q "^## Now" "$GRAPH_MD" || fail "Now column missing"
grep -q "^## Next" "$GRAPH_MD" || fail "Next column missing"
grep -q "^## Later" "$GRAPH_MD" || fail "Later column missing"
grep -q "^## Done" "$GRAPH_MD" || fail "Done column missing"

for title in Auth Billing Dashboard; do
    grep -q "$title" "$GRAPH_MD" || fail "$title card missing from graph.md"
done

# Step 3: Dry-run triage emits a proposal JSON with the three schema keys.
HOME="$TEST_HOME" python3 scripts/triage.py propose --dry-run > "$TEST_HOME/proposal.json" 2> "$TEST_HOME/proposal.err"
grep -q "^Proposed" "$TEST_HOME/proposal.err" || fail "dry-run did not print 'Proposed' header"
python3 -c "
import json, sys
p = json.load(open('$TEST_HOME/proposal.json'))
for key in ('dependencies', 'priority_changes', 'duplicates'):
    assert key in p, f'proposal missing key: {key}'
assert 'candidates' in p and len(p['candidates']) == 3, 'expected 3 candidates'
" || fail "dry-run proposal schema check"

# Step 4: Apply a synthetic proposal (billing blocked_by auth).
AUTH_ID=$(python3 -c "
import json
g = json.load(open('$TEST_HOME/.fno/graph.json'))
print(next(e['id'] for e in g['entries'] if e['title'] == 'Auth'))
")
BILLING_ID=$(python3 -c "
import json
g = json.load(open('$TEST_HOME/.fno/graph.json'))
print(next(e['id'] for e in g['entries'] if e['title'] == 'Billing'))
")

cat > "$TEST_HOME/apply.json" <<EOF
{
    "dependencies": [{"from": "$AUTH_ID", "to": "$BILLING_ID", "reason": "billing requires auth"}],
    "priority_changes": [],
    "duplicates": []
}
EOF

HOME="$TEST_HOME" python3 scripts/triage.py apply "$TEST_HOME/apply.json" > /dev/null \
    || fail "triage apply"

# Step 5: graph.json reflects the new blocked_by edge, graph.md re-rendered.
python3 -c "
import json
g = json.load(open('$TEST_HOME/.fno/graph.json'))
billing = next(e for e in g['entries'] if e['title'] == 'Billing')
assert '$AUTH_ID' in billing['blocked_by'], 'billing not blocked by auth'
assert billing['status'] == 'blocked', f'billing status wrong: {billing[\"status\"]}'
" || fail "apply did not update graph.json"

grep -q "blocked by:" "$GRAPH_MD" || fail "graph.md did not re-render with blocked-by hint"

# AC7: cycle detection drops the offending edge.
cat > "$TEST_HOME/cycle.json" <<EOF
{
    "dependencies": [{"from": "$BILLING_ID", "to": "$AUTH_ID", "reason": "cycle"}],
    "priority_changes": [],
    "duplicates": []
}
EOF
# validate exits non-zero when it drops edges - that's the signal we want.
# Ignore the expected non-zero exit; check stderr for the cycle warning and
# stdout for the dropped edge.
set +e
HOME="$TEST_HOME" python3 scripts/triage.py validate "$TEST_HOME/cycle.json" > "$TEST_HOME/validated.json" 2> "$TEST_HOME/validate.err"
validate_rc=$?
set -e
[[ "$validate_rc" -ne 0 ]] || fail "validate should exit non-zero when edges are dropped"
grep -q "cycle" "$TEST_HOME/validate.err" || fail "cycle detection did not warn"
python3 -c "
import json
v = json.load(open('$TEST_HOME/validated.json'))
assert v['dependencies'] == [], 'cycle edge should have been dropped'
assert v.get('validation_errors'), 'validation_errors should be populated'
" || fail "cycle edge not dropped"

# AC6: empty backlog handled cleanly.
EMPTY_HOME="$(mktemp -d)"
trap "rm -rf '$TEST_HOME' '$EMPTY_HOME'" EXIT
HOME="$EMPTY_HOME" python3 scripts/triage.py propose 2>&1 | grep -q "no pending nodes" \
    || fail "empty backlog did not report 'no pending nodes'"

# Phase 04 AC1-AC2: docs cross-reference the new surface. Cheap grep checks
# so future doc drift is caught here instead of during human review.
grep -q "Backlog Lifecycle" docs/architecture/megawalk-pipeline.md \
    || fail "docs/architecture/megawalk-pipeline.md missing 'Backlog Lifecycle' section"

echo "PASS: all assertions (intake, render, columns, cards, schema, apply, cycle, empty, docs)"
