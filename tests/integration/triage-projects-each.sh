#!/usr/bin/env bash
# Tests for the `projects` subcommand on scripts/triage.py and the `each`
# iteration mode wired in skills/triage/SKILL.md.
#
# Covers the acceptance criteria from the plan:
# - projects subcommand emits alphabetically-sorted JSON listing distinct
#   projects with pending_count based on _is_pending
# - empty graph returns {"projects": []} and exits 0
# - legacy entries with no project field are skipped silently
# - SKILL.md docs the `each` modifier in frontmatter + tables + new section

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TEST_HOME="$(mktemp -d)"
trap "rm -rf '$TEST_HOME'" EXIT

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

# --------------------------------------------------------------------------
# AC1: empty-graph edge case - projects returns {"projects": []} and exits 0
# --------------------------------------------------------------------------
HOME="$TEST_HOME" python3 scripts/triage.py projects > "$TEST_HOME/empty.json" \
    || fail "projects subcommand exited non-zero on empty graph"

python3 -c "
import json
o = json.load(open('$TEST_HOME/empty.json'))
assert 'projects' in o, 'missing projects key'
assert o['projects'] == [], f'expected empty list, got {o[\"projects\"]}'
" || fail "empty graph did not return {projects: []}"

# --------------------------------------------------------------------------
# AC2: multi-project graph - counts match _is_pending, alphabetically sorted
# We build the graph.json directly so we can inject distinct project
# values (since adopt() infers project from the current repo's basename).
# --------------------------------------------------------------------------
mkdir -p "$TEST_HOME/.fno"
cat > "$TEST_HOME/.fno/graph.json" <<'EOF'
{
    "entries": [
        {
            "id": "ab-zzzzzzzz",
            "title": "Zed task (pending)",
            "type": "feature",
            "project": "zed",
            "priority": "medium",
            "blocked_by": [],
            "completed_at": null,
            "status": "ready"
        },
        {
            "id": "ab-aaaaaaaa",
            "title": "Alpha task 1 (pending)",
            "type": "feature",
            "project": "alpha",
            "priority": "high",
            "blocked_by": [],
            "completed_at": null,
            "status": "ready"
        },
        {
            "id": "ab-aaaaaaab",
            "title": "Alpha task 2 (pending blocked)",
            "type": "feature",
            "project": "alpha",
            "priority": "medium",
            "blocked_by": ["ab-aaaaaaaa"],
            "completed_at": null,
            "status": "blocked"
        },
        {
            "id": "ab-cccccccc",
            "title": "Completed task (excluded)",
            "type": "feature",
            "project": "charlie",
            "priority": "medium",
            "blocked_by": [],
            "completed_at": "2026-01-01T00:00:00Z",
            "status": "ready"
        },
        {
            "id": "ab-ddddddd1",
            "title": "Legacy task no project (excluded)",
            "type": "feature",
            "priority": "medium",
            "blocked_by": [],
            "completed_at": null,
            "status": "ready"
        },
        {
            "id": "ab-eeeeeeee",
            "title": "Bravo task (pending)",
            "type": "feature",
            "project": "bravo",
            "priority": "low",
            "blocked_by": [],
            "completed_at": null,
            "status": "ready"
        }
    ]
}
EOF

HOME="$TEST_HOME" python3 scripts/triage.py projects > "$TEST_HOME/out.json" \
    || fail "projects exited non-zero on populated graph"

python3 -c "
import json
o = json.load(open('$TEST_HOME/out.json'))
p = o['projects']
names = [e['name'] for e in p]
assert names == ['alpha', 'bravo', 'zed'], f'projects not alphabetical or wrong set: {names}'
counts = {e['name']: e['pending_count'] for e in p}
assert counts == {'alpha': 2, 'bravo': 1, 'zed': 1}, f'bad counts: {counts}'
# charlie excluded (completed), legacy ab-ddddddd1 excluded (no project)
assert 'charlie' not in counts, 'completed task project must be excluded'
" || fail "projects output missed acceptance criteria"

# --------------------------------------------------------------------------
# AC3: SKILL.md documents the `each` modifier
# --------------------------------------------------------------------------
SKILL=skills/triage/SKILL.md
grep -q "\[each\]" "$SKILL" \
    || fail "SKILL.md argument-hint missing [each]"

grep -qE "^\| \`/triage each\`" "$SKILL" \
    || fail "Invocation table missing /triage each row"

grep -qE "^\| \`each\`" "$SKILL" \
    || fail "Translating-modifiers table missing each row"

grep -q "^## Iteration Mode" "$SKILL" \
    || fail "SKILL.md missing Iteration Mode section header"

grep -q "each and --project are mutually exclusive" "$SKILL" \
    || fail "SKILL.md missing mutually-exclusive error message"

echo "PASS: triage projects subcommand + SKILL.md each modifier docs"
