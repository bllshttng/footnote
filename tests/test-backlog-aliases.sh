#!/usr/bin/env bash
# test-backlog-aliases.sh - verify backlog/graph alias stability.
#
# Covers ab-67de1b86 Phase 05 Task 5.2 scenarios:
#   1. fno backlog --help succeeds and lists intake/done/next/ready/triage
#   2. fno graph --help is identical (hidden deprecated alias)
#   3. fno --help lists backlog but not graph
#   4. fno backlog intake <plan> creates a node
#   5. fno backlog adopt <plan> creates a node + warns on stderr
#   6. fno backlog done <id> marks the node complete
#   7. fno backlog done <id> second time is a safe no-op

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TMP=$(mktemp -d -t backlog-aliases.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

# Pin the graph store into the temp dir by redirecting HOME so
# Path.home() / .fno resolves under $TMP. The real user graph
# at ~/.fno/graph.json is never touched.
export HOME="$TMP/home"
mkdir -p "$HOME/.fno"
GRAPH_JSON="$HOME/.fno/graph.json"
echo '{"entries": []}' > "$GRAPH_JSON"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# Resolve the `fno` command: prefer an installed binary, fall back to
# `python -m fno.cli` against the in-repo venv. Both spell the same
# surface; the alias behavior is independent of invocation shape.
resolve_abi() {
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

ABI=$(resolve_abi)

run_abi() {
    # shellcheck disable=SC2086
    $ABI "$@"
}

echo "Using fno: $ABI"
echo "Graph: $GRAPH_JSON"
echo

# Typer renders command rows as `│ <verb>  <help text>  │` inside a box.
# Match a verb row by requiring the verb is at the start of the text cell
# (after any whitespace / box drawing), followed by either whitespace or
# end-of-cell.
verb_in_help() {
    local verb="$1" help="$2"
    grep -E "^[^A-Za-z]*${verb}([[:space:]]|$)" <<<"$help" >/dev/null
}

# --- Scenario 1: fno backlog --help lists advertised verbs ------------------
# x-71b6 In-N-Out tiering: intake/ready are hidden now (still invocable); probe
# the advertised menu instead.
out=$(run_abi backlog --help 2>&1)
for verb in add done next find triage; do
    if verb_in_help "$verb" "$out"; then
        pass "backlog --help lists '$verb'"
    else
        fail "backlog --help missing verb '$verb'"
    fi
done

# --- Scenario 2: fno graph --help identical verb surface --------------------
graph_out=$(run_abi graph --help 2>&1)
if verb_in_help "find" "$graph_out"; then
    pass "graph --help lists 'find' (alias shares app)"
else
    fail "graph --help missing 'find' - alias not sharing the same Typer app?"
fi

# --- Scenario 3: fno --help hides graph, shows backlog ----------------------
top_out=$(run_abi --help 2>&1)
if verb_in_help "backlog" "$top_out"; then
    pass "top-level help shows 'backlog'"
else
    fail "top-level help missing 'backlog'"
fi
if verb_in_help "graph" "$top_out"; then
    fail "top-level help leaks deprecated 'graph' (should be hidden)"
else
    pass "top-level help hides 'graph'"
fi

# --- Scenario 4: backlog intake adopts a plan -------------------------------
plan_a="$TMP/plan-a.md"
cat > "$plan_a" <<EOF
---
title: Intake Test Plan
---
# Body
EOF

intake_out=$(run_abi backlog intake "$plan_a" 2>&1)
if [[ "$intake_out" == *"adopted ab-"* ]]; then
    pass "intake creates a node"
else
    fail "intake did not report adoption: $intake_out"
fi

# --- Scenario 5: backlog adopt is gone (alias removed) ----------------------
plan_b="$TMP/plan-b.md"
cat > "$plan_b" <<EOF
---
title: Adopt Alias Test Plan
---
# Body
EOF

adopt_rc=0
adopt_combined=$(run_abi backlog adopt "$plan_b" 2>&1) || adopt_rc=$?

if [[ "$adopt_rc" -ne 0 ]]; then
    pass "adopt alias is gone (non-zero exit)"
else
    fail "adopt alias still accepted (rc=0): $adopt_combined"
fi

if [[ "$adopt_combined" != *"deprecated"* ]]; then
    pass "no stale deprecation warning on adopt"
else
    fail "adopt is still forwarding through the deprecated alias: $adopt_combined"
fi

# --- Scenario 6: backlog done marks node complete ---------------------------
# Extract the last adopted ID from graph.json (one of the two we just added)
node_id=$(python3 -c "
import json, sys
data = json.load(open('$GRAPH_JSON'))
entries = data.get('entries', [])
print(entries[-1]['id'] if entries else '')
")

if [[ -z "$node_id" ]]; then
    fail "no node ID available for done test"
else
    done_out=$(run_abi backlog done "$node_id" 2>&1)
    if [[ "$done_out" == *"Marked $node_id done"* ]]; then
        pass "done marks node complete"
    else
        fail "done did not report completion: $done_out"
    fi

    # Verify completed_at is set in the json
    has_completed=$(python3 -c "
import json
data = json.load(open('$GRAPH_JSON'))
for e in data.get('entries', []):
    if e.get('id') == '$node_id':
        print('yes' if e.get('completed_at') else 'no')
        break
")
    if [[ "$has_completed" == "yes" ]]; then
        pass "done sets completed_at timestamp"
    else
        fail "done did not set completed_at on the node"
    fi
fi

# --- Scenario 7: done is idempotent on re-run -------------------------------
if [[ -n "$node_id" ]]; then
    done_again=$(run_abi backlog done "$node_id" 2>&1)
    rc=$?
    if [[ $rc -eq 0 ]]; then
        pass "done rerun exits 0 (idempotent)"
    else
        fail "done rerun exit code $rc (expected 0)"
    fi
    if [[ "$done_again" == *"already done"* ]]; then
        pass "done rerun notes already-done"
    else
        fail "done rerun missing already-done message: $done_again"
    fi
fi

echo
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] || exit 1
