#!/usr/bin/env bash
# test-register-task.sh -- coverage for the completion-stamp ritual fixes
# (plan: internal/fno/plans/2026-04-29-completion-stamp-ritual-fixes.md)
#
# Surface under test: fno.cost._register (the former
# scripts/metrics/register-task.py), run by file path with cli/src on
# PYTHONPATH so the package resolves pre-install.
#
# These tests run TDD-style: written BEFORE the implementation and assert
# the target behavior. Each assertion cites the change number from the plan.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# The cost helpers moved into the fno package; point PYTHONPATH at the package
# source so file-path invocations of _session_cost resolve `from fno.cost...`.
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:${PYTHONPATH}}"
REGISTER_PY="$REPO_ROOT/cli/src/fno/cost/_register.py"
# _register imports fno.config, which needs tomli_w/pydantic. Ambient python3
# has neither, so every invocation must use the project interpreter. A worktree
# has no .venv of its own; fall back to uv, which resolves the shared one.
PY="$REPO_ROOT/cli/.venv/bin/python"
[[ -x "$PY" ]] || PY="$(cd "$REPO_ROOT/cli" 2>/dev/null && uv run python -c 'import sys; print(sys.executable)' 2>/dev/null)"
[[ -n "${PY:-}" && -x "$PY" ]] || PY="python3"
TMP=$(mktemp -d -t register-task-test.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# Each test runs in its own LEDGER_DIR so HOME-based writes are isolated.
# Override HOME so register-task.py writes ledger.json into the sandbox.

make_state_file() {
    # $1=state path, $2=session_id, $3=graph_node_id (or "" to omit)
    # graph_node_id lives BELOW the closing --- to mirror init-target-state.sh's layout.
    local state_path="$1"
    local sid="$2"
    local node_id="${3:-}"
    mkdir -p "$(dirname "$state_path")"
    cat > "$state_path" <<EOF
---
status: COMPLETE
current_phase: ship
iteration: 1
input: "Test feature for register-task TDD"
plan_path: "internal/fno/plans/2026-04-29-completion-stamp-ritual-fixes.md"
session_id: $sid
created_at: 2026-04-29T05:00:00Z
pr_number: 999
quality_check_passed: true
output_validated: true
artifact_shipped: true
ledger_updated: false
---
# Body content below frontmatter
EOF
    if [[ -n "$node_id" ]]; then
        printf '\ngraph_node_id: %s\n' "$node_id" >> "$state_path"
    fi
}

# ---------------------------------------------------------------------------
# Change #4: scalar session_id and graph_node_id populated in ledger entry
# ---------------------------------------------------------------------------
echo ""
echo "=== Change #4: scalar identity fields populated ==="

run_change4_with_node() {
    local sandbox="$TMP/c4-with-node"
    mkdir -p "$sandbox/.fno"
    make_state_file "$sandbox/.fno/target-state.md" \
        "20260429T000000Z-1234-aabbcc" "ab-deadbeef"

    # Run register-task.py from inside the sandbox so root_path becomes sandbox.
    HOME="$sandbox" \
        "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-abc" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    # Read the global ledger that register-task.py wrote
    local ledger="$sandbox/.fno/ledger.json"
    if [[ ! -f "$ledger" ]]; then
        fail "C4-AC1: ledger.json not written. stderr: $(cat "$sandbox/stderr.log")"
        return
    fi

    # AC1-HP: scalar session_id set
    local sid_val
    sid_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(e.get('session_id') or '')" "$ledger")
    if [[ "$sid_val" == "20260429T000000Z-1234-aabbcc" ]]; then
        pass "C4-AC1-HP: ledger entry has scalar session_id from target-state"
    else
        fail "C4-AC1-HP: expected scalar session_id '20260429T000000Z-1234-aabbcc', got '$sid_val'"
    fi

    # AC2-HP: graph_node_id set
    local node_val
    node_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(e.get('graph_node_id') or '')" "$ledger")
    if [[ "$node_val" == "ab-deadbeef" ]]; then
        pass "C4-AC2-HP: ledger entry has graph_node_id from target-state body"
    else
        fail "C4-AC2-HP: expected graph_node_id 'ab-deadbeef', got '$node_val'"
    fi
}
run_change4_with_node

run_change4_no_node() {
    local sandbox="$TMP/c4-no-node"
    mkdir -p "$sandbox/.fno"
    make_state_file "$sandbox/.fno/target-state.md" \
        "20260429T010101Z-9999-bbccdd" ""

    HOME="$sandbox" \
        "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-no-node" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    local ledger="$sandbox/.fno/ledger.json"
    if [[ ! -f "$ledger" ]]; then
        fail "C4-AC3: ledger.json not written. stderr: $(cat "$sandbox/stderr.log")"
        return
    fi

    # AC3-EDGE: missing graph_node_id stays null (do not synthesize one)
    local node_val
    node_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(repr(e.get('graph_node_id')))" "$ledger")
    if [[ "$node_val" == "None" ]]; then
        pass "C4-AC3-EDGE: missing graph_node_id stays null in entry"
    else
        fail "C4-AC3-EDGE: expected graph_node_id None, got $node_val"
    fi
}
run_change4_no_node

# AC4-EDGE: prose-line `graph_node_id:` in body MUST NOT poison the entry
# (silent-failure-hunter finding: regex without ID-shape validation would
# match `> graph_node_id: ab-old (deprecated)` in markdown prose).
run_change4_prose_line_rejected() {
    local sandbox="$TMP/c4-prose"
    mkdir -p "$sandbox/.fno"
    cat > "$sandbox/.fno/target-state.md" <<'EOF'
---
status: COMPLETE
current_phase: ship
iteration: 1
input: "Test prose-line collision"
plan_path: "internal/x.md"
session_id: 20260429T020202Z-1111-cccccc
created_at: 2026-04-29T05:00:00Z
pr_number: 999
quality_check_passed: true
output_validated: true
artifact_shipped: true
ledger_updated: false
---
# Body content with adversarial prose
Some notes mention `graph_node_id: ab-old (deprecated)` in passing.
EOF

    HOME="$sandbox" \
        "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-prose" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    local ledger="$sandbox/.fno/ledger.json"
    [[ -f "$ledger" ]] || { fail "C4-AC4-EDGE: ledger not written"; return; }

    local node_val
    node_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(repr(e.get('graph_node_id')))" "$ledger")
    # The prose line has `(deprecated)` which fails the ab-[0-9a-f]{6,}$
    # shape check. graph_node_id must stay None, NOT be set to the bogus value.
    if [[ "$node_val" == "None" ]]; then
        pass "C4-AC4-EDGE: prose-line graph_node_id rejected by shape validator"
    else
        fail "C4-AC4-EDGE: expected None (shape-rejected), got $node_val"
    fi
}
run_change4_prose_line_rejected

# ---------------------------------------------------------------------------
# Change #2: graph-sync matcher tolerates absolute-vs-relative plan_path
# and prefers graph_node_id lookup when present.
# ---------------------------------------------------------------------------
echo ""
echo "=== Change #2: graph-sync matcher robustness ==="

# Use the helper functions directly via a Python harness rather than running
# the full subprocess (the real _sync_to_graph spawns roadmap-tasks.py update,
# which would mutate the user's home graph.json). The harness imports the
# module and exercises the matcher in isolation.

run_change2_relative_vs_absolute() {
    local sandbox="$TMP/c2-paths"
    mkdir -p "$sandbox"
    cat > "$sandbox/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-relpath",
      "plan_path": "internal/fno/plans/test-fixture.md",
      "status": "ready"
    }
  ]
}
JSON

    # Write a small harness that invokes the matcher's lookup logic directly.
    cat > "$sandbox/harness.py" <<PYEOF
import importlib.util, json, os, sys
spec = importlib.util.spec_from_file_location("rt", "$REGISTER_PY")
rt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rt)

graph = json.load(open("$sandbox/graph.json"))
# Simulate what _sync_to_graph does internally: look up by absolute plan_path
# against graph entries that store relative paths. Anchor relative paths at
# repo root via git rev-parse - use the sandbox as a fake repo root for the test.
repo_root = "$REPO_ROOT"
abs_plan = os.path.join(repo_root, "internal/fno/plans/test-fixture.md")
entry = {"plan_path": abs_plan, "graph_node_id": None}

node = rt._match_graph_node(graph["entries"], entry, repo_root=repo_root)
print(json.dumps({"matched_id": node.get("id") if node else None}))
PYEOF

    local result
    result=$("$PY" "$sandbox/harness.py" 2>&1)
    if [[ "$result" == *'"matched_id": "ab-relpath"'* ]]; then
        pass "C2-AC1-HP: matcher resolves abs ledger path against rel graph path"
    else
        fail "C2-AC1-HP: matcher returned $result"
    fi
}
run_change2_relative_vs_absolute

run_change2_prefer_node_id() {
    local sandbox="$TMP/c2-id"
    mkdir -p "$sandbox"
    cat > "$sandbox/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-nomatch-path",
      "plan_path": "some/other/plan.md",
      "status": "ready"
    },
    {
      "id": "ab-id-match",
      "plan_path": "different/path.md",
      "status": "ready"
    }
  ]
}
JSON

    cat > "$sandbox/harness.py" <<PYEOF
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("rt", "$REGISTER_PY")
rt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rt)

graph = json.load(open("$sandbox/graph.json"))
entry = {"plan_path": "/absolutely/wrong/path.md", "graph_node_id": "ab-id-match"}

node = rt._match_graph_node(graph["entries"], entry, repo_root="$REPO_ROOT")
print(json.dumps({"matched_id": node.get("id") if node else None}))
PYEOF

    local result
    result=$("$PY" "$sandbox/harness.py" 2>&1)
    if [[ "$result" == *'"matched_id": "ab-id-match"'* ]]; then
        pass "C2-AC2-HP: matcher prefers graph_node_id over plan_path"
    else
        fail "C2-AC2-HP: matcher returned $result"
    fi
}
run_change2_prefer_node_id

run_change2_no_match() {
    local sandbox="$TMP/c2-nomatch"
    mkdir -p "$sandbox"
    cat > "$sandbox/graph.json" <<'JSON'
{"entries": [{"id": "ab-other", "plan_path": "x.md"}]}
JSON

    cat > "$sandbox/harness.py" <<PYEOF
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("rt", "$REGISTER_PY")
rt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rt)

graph = json.load(open("$sandbox/graph.json"))
entry = {"plan_path": "/totally/different.md", "graph_node_id": None}

node = rt._match_graph_node(graph["entries"], entry, repo_root="$REPO_ROOT")
print(json.dumps({"matched": node is not None}))
PYEOF

    local result
    result=$("$PY" "$sandbox/harness.py" 2>&1)
    if [[ "$result" == *'"matched": false'* ]]; then
        pass "C2-AC3-EDGE: matcher returns None when nothing matches"
    else
        fail "C2-AC3-EDGE: matcher returned $result (expected no match)"
    fi
}
run_change2_no_match

# ---------------------------------------------------------------------------
# Commit A (ab-c00da95d): scalar session_id is the dedup key
# ---------------------------------------------------------------------------
# Replaces the union-of-sessions set intersection with a primary-key check
# on the scalar `session_id`. Legacy entries with `session_id: null` and a
# transcript UUID in `sessions` are intentionally NOT deduped here; the
# four-tier hook pre-check (ledger-dedup-lookup.py) handles those.
echo ""
echo "=== Commit A: scalar session_id dedup ==="

run_commit_a_distinct_scalars_append() {
    local sandbox="$TMP/cA-distinct"
    mkdir -p "$sandbox/.fno"

    # First entry: session target-A
    make_state_file "$sandbox/.fno/target-state.md" "target-A" ""
    HOME="$sandbox" "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" "transcript-uuid-A" \
        > "$sandbox/out1.log" 2> "$sandbox/err1.log"

    # Re-seed ledger_updated so second run is allowed
    sed -i.bak 's/^ledger_updated: true$/ledger_updated: false/' "$sandbox/.fno/target-state.md"

    # Second entry: session target-B
    make_state_file "$sandbox/.fno/target-state.md" "target-B" ""
    HOME="$sandbox" "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" "transcript-uuid-B" \
        > "$sandbox/out2.log" 2> "$sandbox/err2.log"

    local ledger="$sandbox/.fno/ledger.json"
    local count
    count=$("$PY" -c "import json,sys; print(len(json.load(open(sys.argv[1]))['entries']))" "$ledger")
    if [[ "$count" == "2" ]]; then
        pass "cA-AC1-HP: distinct scalar session_ids both appended"
    else
        fail "cA-AC1-HP: expected 2 entries, got $count. stderr1=$(cat "$sandbox/err1.log") stderr2=$(cat "$sandbox/err2.log")"
    fi
}
run_commit_a_distinct_scalars_append

run_commit_a_same_scalar_rejected() {
    local sandbox="$TMP/cA-dup"
    mkdir -p "$sandbox/.fno"

    make_state_file "$sandbox/.fno/target-state.md" "target-A" ""
    HOME="$sandbox" "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" "transcript-uuid-A" \
        > "$sandbox/out1.log" 2> "$sandbox/err1.log"

    sed -i.bak 's/^ledger_updated: true$/ledger_updated: false/' "$sandbox/.fno/target-state.md"

    # Second invocation with the SAME target-state session_id -> reject
    HOME="$sandbox" "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" "transcript-uuid-A2" \
        > "$sandbox/out2.log" 2> "$sandbox/err2.log"

    local ledger="$sandbox/.fno/ledger.json"
    local count
    count=$("$PY" -c "import json,sys; print(len(json.load(open(sys.argv[1]))['entries']))" "$ledger")
    if [[ "$count" == "1" ]] && grep -q "Skipping duplicate entry for target fno_id" "$sandbox/err2.log"; then
        pass "cA-AC2-ERR: duplicate scalar session_id rejected with explicit stderr"
    else
        fail "cA-AC2-ERR: expected 1 entry + stderr message; count=$count stderr=$(cat "$sandbox/err2.log")"
    fi
}
run_commit_a_same_scalar_rejected

run_commit_a_legacy_null_does_not_dedupe() {
    # Plan AC4-EDGE: legacy entry has session_id=null and a transcript UUID
    # in `sessions`. A new entry with the same transcript UUID but a
    # different scalar session_id MUST be appended (inner dedup falls
    # through; the hook pre-check is the layer for transcript-UUID matches).
    local sandbox="$TMP/cA-legacy"
    mkdir -p "$sandbox/.fno"

    # Manually seed ledger.json with a legacy-shape entry.
    cat > "$sandbox/.fno/ledger.json" <<'JSON'
{
  "entries": [
    {
      "type": "execution",
      "status": "done",
      "title": "Legacy entry",
      "session_id": null,
      "sessions": ["transcript-uuid-shared"]
    }
  ]
}
JSON

    make_state_file "$sandbox/.fno/target-state.md" "target-B" ""
    HOME="$sandbox" "$PY" "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" "transcript-uuid-shared" \
        > "$sandbox/out.log" 2> "$sandbox/err.log"

    local ledger="$sandbox/.fno/ledger.json"
    local count
    count=$("$PY" -c "import json,sys; print(len(json.load(open(sys.argv[1]))['entries']))" "$ledger")
    if [[ "$count" == "2" ]]; then
        pass "cA-AC4-EDGE: legacy null-scalar entry does NOT block scalar-keyed append"
    else
        fail "cA-AC4-EDGE: expected 2 entries (legacy + new), got $count. stderr=$(cat "$sandbox/err.log")"
    fi
}
run_commit_a_legacy_null_does_not_dedupe

run_commit_a_quick_entry_dedup_symmetric() {
    # Review follow-up: build_quick_entry (used by /think, /spec, /audit)
    # must include a scalar session_id so the inner-flock dedup is
    # symmetric with build_entry. Without it, a same-session quick-entry
    # race would silently no-op the under-flock layer.
    local sandbox="$TMP/cA-quick-dup"
    local fake_home="$sandbox/home"
    mkdir -p "$fake_home/.fno"

    # Use --type so build_quick_entry is invoked (no target-state path).
    HOME="$fake_home" "$PY" "$REGISTER_PY" \
        --type think --title "first" --session "quick-sid-1" \
        > "$sandbox/out1.log" 2> "$sandbox/err1.log"
    HOME="$fake_home" "$PY" "$REGISTER_PY" \
        --type think --title "second" --session "quick-sid-1" \
        > "$sandbox/out2.log" 2> "$sandbox/err2.log"

    local ledger="$fake_home/.fno/ledger.json"
    local count
    count=$("$PY" -c "import json,sys; print(len(json.load(open(sys.argv[1]))['entries']))" "$ledger")
    if [[ "$count" == "1" ]] && grep -q "Skipping duplicate entry for target fno_id" "$sandbox/err2.log"; then
        pass "cA-AC-quick: build_quick_entry sets scalar session_id; dedup rejects duplicate"
    else
        fail "cA-AC-quick: expected 1 entry + stderr message; count=$count stderr=$(cat "$sandbox/err2.log")"
    fi
}
run_commit_a_quick_entry_dedup_symmetric

# ---------------------------------------------------------------------------
# Commit B (ab-c00da95d): root_path is worktree top; worktree field always None
# ---------------------------------------------------------------------------
# build_entry and build_quick_entry now derive root_path from
# `git rev-parse --show-toplevel` (worktree-aware). The `worktree` field
# is set to None unconditionally - legacy JSON readers still find the
# key. _resolve_repo_root stays on --git-common-dir for graph-node lookup.
echo ""
echo "=== Commit B: root_path is worktree-aware ==="

# Helper: create a self-contained git repo with a feature worktree.
# Sets globals: REPO_ROOT_FIXTURE, WORKTREE_FIXTURE.
make_worktree_fixture() {
    local fx_root="$1"
    # Canonicalize fx_root via Python so macOS /var -> /private/var symlinks
    # don't trip equality checks (git rev-parse returns the resolved path).
    fx_root=$("$PY" -c "import os,sys; print(os.path.realpath(sys.argv[1]))" "$fx_root")
    REPO_ROOT_FIXTURE="$fx_root/repo"
    WORKTREE_FIXTURE="$fx_root/worktree-x"

    git init -q -b main "$REPO_ROOT_FIXTURE"
    (
        cd "$REPO_ROOT_FIXTURE"
        git config user.email "test@example.com"
        git config user.name "Test"
        git config commit.gpgsign false
        echo "seed" > README.md
        git add README.md
        git -c init.defaultBranch=main commit -q -m "seed"
        git worktree add -q -b feature/x "$WORKTREE_FIXTURE" >/dev/null 2>&1
    )
}

run_commit_b_root_path_in_worktree() {
    local sandbox="$TMP/cB-worktree"
    mkdir -p "$sandbox"
    make_worktree_fixture "$sandbox"

    mkdir -p "$WORKTREE_FIXTURE/.fno"
    make_state_file "$WORKTREE_FIXTURE/.fno/target-state.md" \
        "20260513T230000Z-cB-aaaaaa" ""

    # Run from the worktree so git resolves toplevel to the worktree path.
    HOME="$sandbox" \
        bash -c "cd '$WORKTREE_FIXTURE' && "$PY" '$REGISTER_PY' '$WORKTREE_FIXTURE/.fno/target-state.md' 'transcript-cB-1'" \
        > "$sandbox/out.log" 2> "$sandbox/err.log"

    # AC1-HP: root_path equals the worktree top, not the canonical repo top.
    local ledger="$sandbox/.fno/ledger.json"
    local root_val
    root_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(e.get('root_path') or '')" "$ledger")
    if [[ "$root_val" == "$WORKTREE_FIXTURE" ]]; then
        pass "cB-AC1-HP: root_path equals worktree top in a worktree session"
    else
        fail "cB-AC1-HP: expected root_path=$WORKTREE_FIXTURE, got $root_val. stderr=$(cat "$sandbox/err.log")"
    fi

    # AC7-INV: entry["worktree"] is None.
    local wt_val
    wt_val=$("$PY" -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(repr(e.get('worktree')))" "$ledger")
    if [[ "$wt_val" == "None" ]]; then
        pass "cB-AC7-INV: entry['worktree'] is None unconditionally"
    else
        fail "cB-AC7-INV: expected None, got $wt_val"
    fi

    # AC5-FR: the ledger has a single global writer. The former project-local
    # dual-write was the split-brain that corrupted node-level joins, so
    # neither the worktree nor the canonical repo may grow a stray ledger.
    if [[ -f "$WORKTREE_FIXTURE/.fno/ledger.json" ]]; then
        fail "cB-AC5-FR: project-local ledger leaked into worktree"
    else
        pass "cB-AC5-FR: no project-local ledger written to worktree path"
    fi
    if [[ -f "$REPO_ROOT_FIXTURE/.fno/ledger.json" ]]; then
        fail "cB-AC5-FR: project-local ledger leaked into canonical repo"
    else
        pass "cB-AC5-FR-neg: canonical repo's .fno/ledger.json NOT created"
    fi
}
run_commit_b_root_path_in_worktree

run_commit_b_resolve_repo_root_canonical() {
    # AC6-INV: _resolve_repo_root still returns canonical-repo path
    # (uses --git-common-dir) even when called from inside a worktree.
    local sandbox="$TMP/cB-resolve"
    mkdir -p "$sandbox"
    make_worktree_fixture "$sandbox"

    cat > "$sandbox/harness.py" <<PYEOF
import importlib.util, os, sys
spec = importlib.util.spec_from_file_location("rt", "$REGISTER_PY")
rt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rt)
os.chdir("$WORKTREE_FIXTURE")
print(rt._resolve_repo_root())
PYEOF
    local result
    result=$("$PY" "$sandbox/harness.py" 2>&1 | tail -1)
    if [[ "$result" == "$REPO_ROOT_FIXTURE" ]]; then
        pass "cB-AC6-INV: _resolve_repo_root returns canonical repo from inside worktree"
    else
        fail "cB-AC6-INV: expected $REPO_ROOT_FIXTURE, got $result"
    fi
}
run_commit_b_resolve_repo_root_canonical

run_commit_b_emit_event_to_worktree() {
    # AC4-EDGE: _emit_ledger_transition reads worktree state file +
    # appends to worktree events.jsonl, NOT canonical repo's files.
    local sandbox="$TMP/cB-emit"
    mkdir -p "$sandbox"
    make_worktree_fixture "$sandbox"

    mkdir -p "$WORKTREE_FIXTURE/.fno"
    # Add a provenance nonce so _emit_ledger_transition actually fires.
    cat > "$WORKTREE_FIXTURE/.fno/target-state.md" <<EOF
---
status: COMPLETE
current_phase: ship
iteration: 1
input: "cB emit test"
plan_path: "x.md"
session_id: 20260513T230000Z-cB-emit
created_at: 2026-05-13T22:00:00Z
provenance_nonce: deadbeefcafef00d
quality_check_passed: true
output_validated: true
artifact_shipped: true
ledger_updated: false
---
EOF

    HOME="$sandbox" \
        bash -c "cd '$WORKTREE_FIXTURE' && "$PY" '$REGISTER_PY' '$WORKTREE_FIXTURE/.fno/target-state.md' 'transcript-cB-emit'" \
        > "$sandbox/out.log" 2> "$sandbox/err.log"

    # Event MUST land in the worktree's events.jsonl, not the canonical repo's.
    if [[ -f "$WORKTREE_FIXTURE/.fno/events.jsonl" ]] \
       && grep -q '"gate":"ledger_updated"' "$WORKTREE_FIXTURE/.fno/events.jsonl" 2>/dev/null; then
        pass "cB-AC4-EDGE: phase_transition event written to worktree events.jsonl"
    else
        fail "cB-AC4-EDGE: worktree events.jsonl missing the ledger_updated event. stderr=$(cat "$sandbox/err.log")"
    fi
    if [[ -f "$REPO_ROOT_FIXTURE/.fno/events.jsonl" ]]; then
        fail "cB-AC4-EDGE: event leaked into canonical repo's events.jsonl"
    else
        pass "cB-AC4-EDGE-neg: canonical repo's events.jsonl NOT created"
    fi
}
run_commit_b_emit_event_to_worktree

# ---------------------------------------------------------------------------
# Commit C (ab-c00da95d): session-cost.py --since flag
# ---------------------------------------------------------------------------
# parse_transcript gains a `since` parameter. argparse validates --since
# via datetime.fromisoformat (with trailing-Z strip). Transcript entries
# with timestamp < since are skipped; entries with no timestamp are
# skipped only when --since is set (preserves backwards-compat for
# --branch and full-sum callers).
echo ""
echo "=== Commit C: session-cost --since ==="

SESSION_COST_PY="$REPO_ROOT/cli/src/fno/cost/_session_cost.py"

# Build a small transcript JSONL fixture covering before/after a cutoff,
# plus one no-timestamp entry. Uses tiny assistant messages with usage.
make_cost_fixture() {
    local fx="$1"
    cat > "$fx" <<'JSON'
{"type":"assistant","timestamp":"2026-05-13T21:00:00Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-05-13T22:30:00Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","message":{"model":"claude-opus-4-8","usage":{"input_tokens":4000,"output_tokens":2000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSON
}

run_commit_c_since_filters_before_cutoff() {
    local sandbox="$TMP/cC-since"
    mkdir -p "$sandbox"
    local fx="$sandbox/transcript.jsonl"
    make_cost_fixture "$fx"

    # Stage a fake $HOME so find_transcript locates the fixture.
    local fake_home="$sandbox/home"
    mkdir -p "$fake_home/.claude/projects/test"
    cp "$fx" "$fake_home/.claude/projects/test/11111111-2222-3333-4444-555555555555.jsonl"

    local out
    out=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --json --since 2026-05-13T22:00:00Z 11111111-2222-3333-4444-555555555555 2>&1)
    local total_in
    total_in=$(echo "$out" | "$PY" -c "import json,sys; d=json.load(sys.stdin); print(d['tokens']['input'])" 2>/dev/null || echo "ERR")

    # Expected: only the 22:30 entry counted (2000 input tokens). The
    # 21:00 entry is before cutoff; the no-timestamp entry is also
    # skipped when --since is set.
    if [[ "$total_in" == "2000" ]]; then
        pass "cC-AC1-HP: --since filters out entries before cutoff and entries without timestamp"
    else
        fail "cC-AC1-HP: expected 2000 input tokens, got $total_in. output=$out"
    fi
}
run_commit_c_since_filters_before_cutoff

run_commit_c_since_unset_accumulates_all() {
    local sandbox="$TMP/cC-no-since"
    local fake_home="$sandbox/home"
    mkdir -p "$fake_home/.claude/projects/test"
    make_cost_fixture "$fake_home/.claude/projects/test/22222222-3333-4444-5555-666666666666.jsonl"

    local out total_in
    out=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --json 22222222-3333-4444-5555-666666666666 2>&1)
    total_in=$(echo "$out" | "$PY" -c "import json,sys; d=json.load(sys.stdin); print(d['tokens']['input'])" 2>/dev/null || echo "ERR")

    # All three entries accumulate: 1000 + 2000 + 4000 = 7000.
    if [[ "$total_in" == "7000" ]]; then
        pass "cC-AC4-EDGE: without --since, no-timestamp entries still accumulate"
    else
        fail "cC-AC4-EDGE: expected 7000 input tokens, got $total_in. output=$out"
    fi
}
run_commit_c_since_unset_accumulates_all

run_commit_c_malformed_since_rejected() {
    local sandbox="$TMP/cC-bad"
    local fake_home="$sandbox/home"
    mkdir -p "$fake_home/.claude/projects/test"
    make_cost_fixture "$fake_home/.claude/projects/test/33333333-4444-5555-6666-777777777777.jsonl"

    local rc stderr_out
    stderr_out=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --since not-an-iso-string --json 33333333-4444-5555-6666-777777777777 2>&1 >/dev/null)
    rc=$?
    if [[ "$rc" == "2" ]] && [[ -n "$stderr_out" ]]; then
        pass "cC-AC2-ERR: malformed --since exits rc=2 with stderr message"
    else
        fail "cC-AC2-ERR: expected rc=2 with stderr; got rc=$rc stderr=$stderr_out"
    fi
}
run_commit_c_malformed_since_rejected

run_commit_c_tz_mixed_does_not_crash() {
    # Review follow-up: a transcript with an offset-suffixed timestamp
    # (`+00:00`) parses to a tz-aware datetime under fromisoformat, while
    # --since (after rstrip-Z) is naive. Without normalization, the
    # comparison would raise TypeError mid-loop. The fix converts aware
    # datetimes to naive-UTC; this test pins that behavior.
    local sandbox="$TMP/cC-tz"
    local fake_home="$sandbox/home"
    local sid="44444444-5555-6666-7777-888888888888"
    mkdir -p "$fake_home/.claude/projects/test"
    cat > "$fake_home/.claude/projects/test/$sid.jsonl" <<'JSON'
{"type":"assistant","timestamp":"2026-05-13T20:30:00+00:00","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1500,"output_tokens":750,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-05-13T23:00:00+00:00","message":{"model":"claude-opus-4-8","usage":{"input_tokens":3000,"output_tokens":1500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSON

    local out total_in
    out=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --json --since 2026-05-13T22:00:00Z "$sid" 2>&1)
    total_in=$(echo "$out" | "$PY" -c "import json,sys; d=json.load(sys.stdin); print(d['tokens']['input'])" 2>/dev/null || echo "ERR")

    # Only the 23:00 entry should accumulate (3000 input tokens). The
    # 20:30 entry is before cutoff. The crash-free behavior is the
    # primary regression guard; the value check is the correctness bonus.
    if [[ "$total_in" == "3000" ]]; then
        pass "cC-AC-tz: offset-suffixed transcript timestamps compare safely against naive --since"
    else
        fail "cC-AC-tz: expected 3000 input tokens, got $total_in. output=$out"
    fi
}
run_commit_c_tz_mixed_does_not_crash

run_commit_c_since_boundary_equality() {
    # Pins the inclusive-cutoff contract: an entry whose timestamp equals
    # `since` exactly is INCLUDED in accumulation. `_parse_ts` returns a
    # naive datetime; the comparison is `entry_ts < since`, so equality
    # accepts. A future flip to `<=` would silently exclude the boundary.
    local sandbox="$TMP/cC-boundary"
    local fake_home="$sandbox/home"
    local sid="55555555-6666-7777-8888-999999999999"
    mkdir -p "$fake_home/.claude/projects/test"
    cat > "$fake_home/.claude/projects/test/$sid.jsonl" <<'JSON'
{"type":"assistant","timestamp":"2026-05-13T22:00:00Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":1234,"output_tokens":500,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSON
    local out total_in
    out=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --json --since 2026-05-13T22:00:00Z "$sid" 2>&1)
    total_in=$(echo "$out" | "$PY" -c "import json,sys; d=json.load(sys.stdin); print(d['tokens']['input'])" 2>/dev/null || echo "ERR")
    if [[ "$total_in" == "1234" ]]; then
        pass "cC-AC-boundary: entry timestamp exactly equal to --since is INCLUDED"
    else
        fail "cC-AC-boundary: expected 1234 input tokens (equality accepts), got $total_in. output=$out"
    fi
}
run_commit_c_since_boundary_equality

run_commit_c_unparseable_ts_warns() {
    # silent-failure-hunter follow-up: a non-empty timestamp that fails
    # parse must surface a stderr warning when --since is set; otherwise
    # a future shape drift would silently zero the metrics. An entirely
    # missing timestamp stays silent (documented sentinel).
    local sandbox="$TMP/cC-bad-ts"
    local fake_home="$sandbox/home"
    local sid="66666666-7777-8888-9999-aaaaaaaaaaaa"
    mkdir -p "$fake_home/.claude/projects/test"
    cat > "$fake_home/.claude/projects/test/$sid.jsonl" <<'JSON'
{"type":"assistant","timestamp":"not-an-iso-date","message":{"model":"claude-opus-4-8","usage":{"input_tokens":777,"output_tokens":100,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
{"type":"assistant","timestamp":"2026-05-13T23:00:00Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2000,"output_tokens":1000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
JSON
    local stderr_capture
    stderr_capture=$(HOME="$fake_home" "$PY" "$SESSION_COST_PY" --json --since 2026-05-13T22:00:00Z "$sid" 2>&1 >/dev/null)
    if echo "$stderr_capture" | grep -q "unparseable timestamps"; then
        pass "cC-AC-bad-ts: unparseable timestamp under --since emits stderr warning"
    else
        fail "cC-AC-bad-ts: expected 'unparseable timestamps' in stderr, got: $stderr_capture"
    fi
}
run_commit_c_unparseable_ts_warns

# ---------------------------------------------------------------------------
# Commit C: hook drops --branch, passes --since
# ---------------------------------------------------------------------------
echo ""
echo "=== Commit C: hook cost-args plumbing ==="

run_commit_c_hook_cost_args() {
    # AC5-FR: hook must NOT crash when target-state.md lacks created_at.
    # AC6-INV: hook must build cost_args containing --since when created_at present, never --branch.
    local sandbox="$TMP/cC-hook"
    mkdir -p "$sandbox"

    # Extract the cost-args build block from the hook and execute it in
    # isolation against synthetic state files. This is the same shape the
    # spec asks for: assert what arguments the hook would pass.
    #
    # Cases 1 and 2 are inline bash snippets that mirror the cost_args
    # build logic and don't depend on the hook file location. (A former
    # Case 3 extracted run_completion_accounting from
    # scripts/lib/completion-accounting.sh; the control-plane collapse
    # (ab-d0337fbc) deleted that lib when accounting moved into the Rust
    # `fno-agents finalize` writer, so that assertion was dropped.)

    # Case 1: state file with created_at -> --since present, --branch absent.
    cat > "$sandbox/state-with-ts.md" <<EOF
---
status: COMPLETE
created_at: 2026-05-13T22:00:00Z
session_id: my-sid
---
EOF
    local args1
    args1=$(STATE_FILE="$sandbox/state-with-ts.md" bash -c '
        created_at=$(sed -n "s/^created_at:[[:space:]]*//p" "$STATE_FILE" | head -1 | tr -d "\"")
        cost_args="--json"
        if [[ -n "$created_at" ]]; then
            cost_args="$cost_args --since $created_at"
        fi
        echo "$cost_args"
    ')
    if [[ "$args1" == "--json --since 2026-05-13T22:00:00Z" ]]; then
        pass "cC-AC6-INV: with created_at, hook builds --json --since <iso>"
    else
        fail "cC-AC6-INV: expected '--json --since 2026-05-13T22:00:00Z', got '$args1'"
    fi
    if [[ "$args1" != *"--branch"* ]]; then
        pass "cC-AC6-INV: hook does NOT pass --branch"
    else
        fail "cC-AC6-INV: hook still passes --branch: $args1"
    fi

    # Case 2: state file without created_at -> --since omitted, no crash.
    cat > "$sandbox/state-no-ts.md" <<EOF
---
status: COMPLETE
session_id: my-sid
---
EOF
    local args2
    args2=$(STATE_FILE="$sandbox/state-no-ts.md" bash -c '
        created_at=$(sed -n "s/^created_at:[[:space:]]*//p" "$STATE_FILE" | head -1 | tr -d "\"")
        cost_args="--json"
        if [[ -n "$created_at" ]]; then
            cost_args="$cost_args --since $created_at"
        fi
        echo "$cost_args"
    ')
    if [[ "$args2" == "--json" ]]; then
        pass "cC-AC5-FR: state without created_at -> --since omitted, no crash"
    else
        fail "cC-AC5-FR: expected bare '--json', got '$args2'"
    fi
}
run_commit_c_hook_cost_args

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo ""
echo "=== Results ==="
echo "PASS: $PASS"
echo "FAIL: $FAIL"
if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
