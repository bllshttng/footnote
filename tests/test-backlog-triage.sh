#!/usr/bin/env bash
# test-backlog-triage.sh - end-to-end exercise of fno backlog triage verbs.
#
# Covers ab-67de1b86 Phase 05 Task 5.3 scenarios:
#   1. triage context on empty project returns candidates: []
#   2. After intake of 2 plans, context returns both candidates
#   3. context --deep includes plan_excerpt for each candidate
#   4. validate accepts a clean proposal; rejects one with a cycle
#   5. apply mutates graph.json + re-renders graph.md exactly once
#   6. propose --dry-run emits a valid proposal template
#   7. projects emits an alphabetical JSON list

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TMP=$(mktemp -d -t backlog-triage.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

# Redirect HOME so Path.home() / .fno resolves into $TMP.
export HOME="$TMP/home"
mkdir -p "$HOME/.fno"
GRAPH_JSON="$HOME/.fno/graph.json"
GRAPH_MD="$HOME/.fno/graph.md"
echo '{"entries": []}' > "$GRAPH_JSON"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

resolve_fno() {
    if command -v fno >/dev/null 2>&1; then
        echo "fno"
        return
    fi
    local venv_py="$REPO_ROOT/cli/.venv/bin/python"
    if [[ -x "$venv_py" ]]; then
        echo "$venv_py -m fno.cli"
        return
    fi
    echo "python3 -m fno.cli"
}

ABI=$(resolve_fno)
run_fno() {
    # shellcheck disable=SC2086
    $ABI "$@"
}

echo "Using fno: $ABI"
echo "Graph: $GRAPH_JSON"
echo

# --- Scenario 1: context on empty project -----------------------------------
# Pipe JSON to Python via stdin so multiline plan excerpts don't break embedded
# triple-quoted strings. stderr is discarded so diagnostics don't pollute the
# JSON stream.
run_fno --json backlog triage context --all 2>/dev/null > "$TMP/ctx.json"
empty_count=$(python3 -c "import json,sys; print(len(json.load(sys.stdin).get('candidates', [])))" < "$TMP/ctx.json")
if [[ "$empty_count" == "0" ]]; then
    pass "context returns empty candidates on empty graph"
else
    fail "context should return 0 candidates, got $empty_count"
fi

# --- Scenario 2: intake two plans, then context returns both ---------------
plan_x="$TMP/plan-x.md"
plan_y="$TMP/plan-y.md"
cat > "$plan_x" <<EOF
---
title: Plan X
---
# Body X
Line 2 of plan X.
EOF
cat > "$plan_y" <<EOF
---
title: Plan Y
---
# Body Y
Line 2 of plan Y.
EOF

run_fno backlog intake "$plan_x" >/dev/null 2>&1
run_fno backlog intake "$plan_y" >/dev/null 2>&1

run_fno --json backlog triage context --all 2>/dev/null > "$TMP/ctx.json"
candidate_count=$(python3 -c "import json,sys; print(len(json.load(sys.stdin).get('candidates', [])))" < "$TMP/ctx.json")
if [[ "$candidate_count" == "2" ]]; then
    pass "context returns 2 candidates after 2 intakes"
else
    fail "context should return 2 candidates, got $candidate_count"
fi

# --- Scenario 3: context --deep includes plan_excerpt ----------------------
run_fno --json backlog triage context --deep --all 2>/dev/null > "$TMP/ctx-deep.json"
excerpt_check=$(python3 -c "
import json, sys
d = json.load(sys.stdin)
has_excerpt = all('plan_excerpt' in c and c['plan_excerpt'] for c in d.get('candidates', []))
print('yes' if has_excerpt else 'no')
" < "$TMP/ctx-deep.json")
if [[ "$excerpt_check" == "yes" ]]; then
    pass "context --deep includes non-empty plan_excerpt"
else
    fail "context --deep missing plan_excerpt on candidates"
fi

# --- Scenario 4: validate accepts clean / rejects cycle --------------------
clean_prop="$TMP/clean.json"
echo '{"dependencies": [], "priority_changes": [], "duplicates": []}' > "$clean_prop"
run_fno --json backlog triage validate "$clean_prop" >/dev/null 2>&1
clean_rc=$?
if [[ $clean_rc -eq 0 ]]; then
    pass "validate accepts empty proposal (exit 0)"
else
    fail "validate on empty proposal should exit 0, got $clean_rc"
fi

# Build a cycle: add blocker edge A -> B, then propose B -> A
node_a=$(python3 -c "
import json
d = json.load(open('$GRAPH_JSON'))
print(d['entries'][0]['id'])
")
node_b=$(python3 -c "
import json
d = json.load(open('$GRAPH_JSON'))
print(d['entries'][1]['id'])
")
# Seed a direct blocked_by on B so any edge from A already traverses back
python3 -c "
import json
p = '$GRAPH_JSON'
d = json.load(open(p))
for e in d['entries']:
    if e['id'] == '$node_b':
        e['blocked_by'] = ['$node_a']
json.dump(d, open(p, 'w'))
"

cycle_prop="$TMP/cycle.json"
cat > "$cycle_prop" <<EOF
{
  "dependencies": [
    {"from": "$node_b", "to": "$node_a", "reason": "test cycle"}
  ],
  "priority_changes": [],
  "duplicates": []
}
EOF
cycle_out=$(run_fno backlog triage validate "$cycle_prop" 2>&1)
cycle_rc=$?
if [[ $cycle_rc -ne 0 ]]; then
    pass "validate exits non-zero on cycle-creating edge"
else
    fail "validate should flag cycle (expected non-zero exit)"
fi
if [[ "$cycle_out" == *"cycle"* ]]; then
    pass "validate logs cycle rejection"
else
    fail "validate output missing 'cycle' rejection message"
fi

# --- Scenario 5: apply mutates graph.json + re-renders graph.md exactly once
# Reset blocked_by so a fresh apply has somewhere to go.
python3 -c "
import json
p = '$GRAPH_JSON'
d = json.load(open(p))
for e in d['entries']:
    e['blocked_by'] = []
json.dump(d, open(p, 'w'))
"

apply_prop="$TMP/apply.json"
cat > "$apply_prop" <<EOF
{
  "dependencies": [
    {"from": "$node_a", "to": "$node_b", "reason": "test edge"}
  ],
  "priority_changes": [
    {"id": "$node_a", "to": "high", "reason": "bump for test"}
  ],
  "duplicates": [],
  "validation_errors": []
}
EOF

# Rendering is idempotent; we only check content changed as expected.
md_before_hash=$(shasum "$GRAPH_MD" 2>/dev/null | awk '{print $1}' || echo "absent")
run_fno backlog triage apply "$apply_prop" >/dev/null 2>&1
applied_rc=$?

if [[ $applied_rc -eq 0 ]]; then
    pass "apply exits 0 on clean proposal"
else
    fail "apply failed with exit $applied_rc"
fi

# Check graph.json now has the blocker and priority
applied_check=$(python3 -c "
import json
d = json.load(open('$GRAPH_JSON'))
by_id = {e['id']: e for e in d['entries']}
a = by_id.get('$node_a', {})
b = by_id.get('$node_b', {})
if a.get('priority') != 'high':
    print('priority-not-applied')
elif '$node_a' not in b.get('blocked_by', []):
    print('edge-not-applied')
else:
    print('ok')
")
if [[ "$applied_check" == "ok" ]]; then
    pass "apply persisted priority_change + dependency edge to graph.json"
else
    fail "apply didn't persist correctly: $applied_check"
fi

md_after_hash=$(shasum "$GRAPH_MD" 2>/dev/null | awk '{print $1}' || echo "absent")
if [[ "$md_before_hash" != "$md_after_hash" ]]; then
    pass "apply re-rendered graph.md"
else
    fail "apply did not re-render graph.md (hash unchanged: $md_before_hash)"
fi

# --- Scenario 6: propose --dry-run emits a valid template ------------------
# Capture only stdout; dry-run writes its human-readable summary to stderr.
run_fno --json backlog triage propose --dry-run --all 2>/dev/null > "$TMP/dry.json"
dry_keys=$(python3 -c "
import json, sys
d = json.load(sys.stdin)
need = ['dependencies', 'priority_changes', 'duplicates']
missing = [k for k in need if k not in d]
print(','.join(missing) if missing else 'ok')
" < "$TMP/dry.json")
if [[ "$dry_keys" == "ok" ]]; then
    pass "propose --dry-run has dependencies/priority_changes/duplicates keys"
else
    fail "propose --dry-run missing keys: $dry_keys"
fi

# --- Scenario 7: projects emits an alphabetical JSON list ------------------
# Seed two projects manually so the list is non-trivial.
python3 -c "
import json
p = '$GRAPH_JSON'
d = json.load(open(p))
d['entries'][0]['project'] = 'zeta'
d['entries'][1]['project'] = 'alpha'
# Both still need completed_at == None to count as pending
json.dump(d, open(p, 'w'))
"
run_fno --json backlog triage projects 2>/dev/null > "$TMP/projects.json"
projects_check=$(python3 -c "
import json, sys
d = json.load(sys.stdin)
if 'projects' not in d:
    print('missing-projects-key')
else:
    names = [e['name'] for e in d['projects']]
    counts = {e['name']: e['pending_count'] for e in d['projects']}
    if names != ['alpha', 'zeta']:
        print(f'wrong-order:{names}')
    elif counts != {'alpha': 1, 'zeta': 1}:
        print(f'wrong-counts:{counts}')
    else:
        print('ok')
" < "$TMP/projects.json")
if [[ "$projects_check" == "ok" ]]; then
    pass "projects returns alphabetical {projects: [{name, pending_count}, ...]}"
else
    fail "projects shape wrong: $projects_check"
fi

echo
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
