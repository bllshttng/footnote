#!/usr/bin/env bash
# test_register_task_provider_attribution.sh
#
# Integration tests for Phase 04 of provider rotation substrate (ab-256f6b6e).
# Covers AC04.4, AC04.5, AC04.3 (mixed-schema read), and the register-task.py
# provider attribution propagation from target-state.md frontmatter.
#
# All tests run in mktemp sandbox dirs; HOME is overridden so no real
# ~/.fno/ledger.json is touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# The cost helpers moved into the fno package; point PYTHONPATH at the package
# source so file-path invocations of _session_cost resolve `from fno.cost...`.
export PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:${PYTHONPATH}}"
REGISTER_PY="$REPO_ROOT/cli/src/fno/cost/_register.py"
SESSION_COST_PY="$REPO_ROOT/cli/src/fno/cost/_session_cost.py"
TMP=$(mktemp -d -t register-task-provider-test.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }

# ---------------------------------------------------------------------------
# Helper: write a minimal target-state.md
# $1 = path, $2 = session_id
# Remaining args are optional extra frontmatter lines (e.g. "provider_id: foo")
# ---------------------------------------------------------------------------
make_state() {
    local state_path="$1"
    local sid="$2"
    shift 2
    mkdir -p "$(dirname "$state_path")"
    cat > "$state_path" <<STATEOF
---
status: COMPLETE
current_phase: ship
iteration: 1
input: "Provider attribution integration test"
session_id: $sid
created_at: 2026-05-04T10:00:00Z
pr_number: 42
quality_check_passed: true
output_validated: true
artifact_shipped: true
ledger_updated: false
STATEOF
    for extra in "$@"; do
        printf '%s\n' "$extra" >> "$state_path"
    done
    printf -- '---\n' >> "$state_path"
}

# ---------------------------------------------------------------------------
# AC04.4: register-task.py propagates provider_id + account_id
# ---------------------------------------------------------------------------
echo ""
echo "=== AC04.4: provider fields propagated from target-state.md ==="

run_ac04_4() {
    local sandbox="$TMP/ac04-4"
    mkdir -p "$sandbox/.fno"
    make_state \
        "$sandbox/.fno/target-state.md" \
        "20260504T100000Z-test-ac044" \
        "provider_id: claude-max-secondary" \
        "account_id: account-secondary"

    HOME="$sandbox" \
        python3 "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-ac044" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    local ledger="$sandbox/.fno/ledger.json"
    if [[ ! -f "$ledger" ]]; then
        fail "AC04.4: ledger.json not written. stderr: $(cat "$sandbox/stderr.log")"
        return
    fi

    local provider_val
    provider_val=$(python3 -c "
import json, sys
e = json.load(open(sys.argv[1]))['entries'][-1]
print(e.get('provider_id', '__MISSING__'))
" "$ledger")

    local account_val
    account_val=$(python3 -c "
import json, sys
e = json.load(open(sys.argv[1]))['entries'][-1]
print(e.get('account_id', '__MISSING__'))
" "$ledger")

    if [[ "$provider_val" == "claude-max-secondary" ]]; then
        pass "AC04.4-HP: ledger entry contains provider_id from target-state.md"
    else
        fail "AC04.4-HP: expected provider_id='claude-max-secondary', got '$provider_val'"
    fi

    if [[ "$account_val" == "account-secondary" ]]; then
        pass "AC04.4-HP: ledger entry contains account_id from target-state.md"
    else
        fail "AC04.4-HP: expected account_id='account-secondary', got '$account_val'"
    fi
}
run_ac04_4

# ---------------------------------------------------------------------------
# AC04.5: register-task.py omits fields silently when absent
# ---------------------------------------------------------------------------
echo ""
echo "=== AC04.5: provider fields absent in target-state.md -> omitted from entry ==="

run_ac04_5() {
    local sandbox="$TMP/ac04-5"
    mkdir -p "$sandbox/.fno"
    # State file has NO provider_id / account_id fields
    make_state \
        "$sandbox/.fno/target-state.md" \
        "20260504T110000Z-test-ac045"

    HOME="$sandbox" \
        python3 "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-ac045" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    local ledger="$sandbox/.fno/ledger.json"
    if [[ ! -f "$ledger" ]]; then
        fail "AC04.5: ledger.json not written. stderr: $(cat "$sandbox/stderr.log")"
        return
    fi

    # Verify provider_id key is entirely absent (not null, not empty)
    local has_provider
    has_provider=$(python3 -c "
import json, sys
e = json.load(open(sys.argv[1]))['entries'][-1]
print('yes' if 'provider_id' in e else 'no')
" "$ledger")

    local has_account
    has_account=$(python3 -c "
import json, sys
e = json.load(open(sys.argv[1]))['entries'][-1]
print('yes' if 'account_id' in e else 'no')
" "$ledger")

    if [[ "$has_provider" == "no" ]]; then
        pass "AC04.5-FR: provider_id key absent from entry when not in target-state.md"
    else
        local raw_val
        raw_val=$(python3 -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(repr(e.get('provider_id')))" "$ledger")
        fail "AC04.5-FR: provider_id present in entry when it should be absent (value=$raw_val)"
    fi

    if [[ "$has_account" == "no" ]]; then
        pass "AC04.5-FR: account_id key absent from entry when not in target-state.md"
    else
        local raw_val
        raw_val=$(python3 -c "import json,sys; e=json.load(open(sys.argv[1]))['entries'][-1]; print(repr(e.get('account_id')))" "$ledger")
        fail "AC04.5-FR: account_id present in entry when it should be absent (value=$raw_val)"
    fi

    # Also verify no error was raised (rc=0 already checked by set -e being off;
    # verify explicitly via stderr for clean runs)
    if ! grep -qi "traceback\|error\|exception" "$sandbox/stderr.log" 2>/dev/null; then
        pass "AC04.5-FR: no Python traceback/error in stderr for legacy-state run"
    else
        fail "AC04.5-FR: unexpected error in stderr: $(cat "$sandbox/stderr.log")"
    fi
}
run_ac04_5

# ---------------------------------------------------------------------------
# AC04.3: Mixed-schema ledger read cleanly (old + new entries)
# register-task.py appends to a ledger that already has both old and new entries
# ---------------------------------------------------------------------------
echo ""
echo "=== AC04.3: mixed-schema ledger (old + new entries) parsed without error ==="

run_ac04_3_mixed_schema() {
    local sandbox="$TMP/ac04-3"
    mkdir -p "$sandbox/.fno"

    # Pre-populate ledger.json with 1 old-format and 1 new-format entry
    cat > "$sandbox/.fno/ledger.json" <<'JSONEOF'
{
  "entries": [
    {
      "type": "execution",
      "status": "done",
      "session_id": "old-session-no-provider",
      "cost_usd": 0.05,
      "sessions": ["old-session-no-provider"]
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "new-session-with-provider",
      "provider_id": "claude-max-primary",
      "account_id": "account-primary",
      "cost_usd": 0.10,
      "sessions": ["new-session-with-provider"]
    }
  ]
}
JSONEOF

    # Append a fresh entry via register-task.py
    make_state \
        "$sandbox/.fno/target-state.md" \
        "20260504T120000Z-mixed-schema" \
        "provider_id: claude-max-secondary" \
        "account_id: account-secondary"

    HOME="$sandbox" \
        python3 "$REGISTER_PY" \
        "$sandbox/.fno/target-state.md" \
        "transcript-uuid-mixed" \
        > "$sandbox/output.json" 2> "$sandbox/stderr.log"

    local ledger="$sandbox/.fno/ledger.json"
    if [[ ! -f "$ledger" ]]; then
        fail "AC04.3: ledger.json not written after append. stderr: $(cat "$sandbox/stderr.log")"
        return
    fi

    local entry_count
    entry_count=$(python3 -c "
import json, sys
d = json.load(open(sys.argv[1]))
print(len(d.get('entries', [])))
" "$ledger")

    if [[ "$entry_count" -ge 3 ]]; then
        pass "AC04.3-FR: all entries in mixed-schema ledger parsed without error (count=$entry_count)"
    else
        fail "AC04.3-FR: expected >=3 entries after append to mixed-schema ledger, got $entry_count"
    fi

    # Verify old entry still lacks provider_id
    local old_has_provider
    old_has_provider=$(python3 -c "
import json, sys
entries = json.load(open(sys.argv[1]))['entries']
# find the old entry by session_id
for e in entries:
    if e.get('session_id') == 'old-session-no-provider' or (e.get('sessions') and 'old-session-no-provider' in e.get('sessions', [])):
        print('yes' if 'provider_id' in e else 'no')
        break
else:
    print('not_found')
" "$ledger")

    if [[ "$old_has_provider" == "no" ]]; then
        pass "AC04.3-FR: pre-substrate entry retained without provider_id key"
    elif [[ "$old_has_provider" == "not_found" ]]; then
        fail "AC04.3-FR: old-session entry not found in ledger after append"
    else
        fail "AC04.3-FR: old entry now has provider_id (should be absent)"
    fi
}
run_ac04_3_mixed_schema

# ---------------------------------------------------------------------------
# session-cost.py --by-provider aggregation
# ---------------------------------------------------------------------------
echo ""
echo "=== session-cost.py --by-provider aggregation ==="

run_session_cost_by_provider() {
    local sandbox="$TMP/sc-by-provider"
    mkdir -p "$sandbox/.fno"

    # Build a synthetic ledger with old and new entries
    cat > "$sandbox/.fno/ledger.json" <<'JSONEOF'
{
  "entries": [
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-old-1",
      "cost_usd": 1.00,
      "sessions": ["sess-old-1"]
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-old-2",
      "cost_usd": 2.00,
      "sessions": ["sess-old-2"]
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-prov-a-1",
      "provider_id": "provider-a",
      "account_id": "account-a",
      "cost_usd": 0.50,
      "sessions": ["sess-prov-a-1"]
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-prov-a-2",
      "provider_id": "provider-a",
      "account_id": "account-a",
      "cost_usd": 0.75,
      "sessions": ["sess-prov-a-2"]
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-prov-b-1",
      "provider_id": "provider-b",
      "account_id": "account-b",
      "cost_usd": 1.25,
      "sessions": ["sess-prov-b-1"]
    }
  ]
}
JSONEOF

    # Run with --by-provider against the synthetic ledger
    local result
    if ! result=$(HOME="$sandbox" python3 "$SESSION_COST_PY" --by-provider 2>&1); then
        fail "SC-by-provider: --by-provider flag returned non-zero exit code"
        echo "    Output: $result"
        return
    fi

    # provider-a and provider-b should appear in output
    if echo "$result" | grep -q "provider-a"; then
        pass "SC-by-provider-HP: 'provider-a' appears in --by-provider output"
    else
        fail "SC-by-provider-HP: 'provider-a' not found in --by-provider output"
        echo "    Output: $result"
    fi

    if echo "$result" | grep -q "provider-b"; then
        pass "SC-by-provider-HP: 'provider-b' appears in --by-provider output"
    else
        fail "SC-by-provider-HP: 'provider-b' not found in --by-provider output"
        echo "    Output: $result"
    fi

    # 'unattributed' bucket should appear for old entries
    if echo "$result" | grep -q "unattributed"; then
        pass "SC-by-provider-FR: 'unattributed' bucket appears for pre-substrate entries"
    else
        fail "SC-by-provider-FR: 'unattributed' bucket not in output for old entries"
        echo "    Output: $result"
    fi
}
run_session_cost_by_provider

# ---------------------------------------------------------------------------
# session-cost.py --by-provider: verify existing callers not broken
# (no-flag invocation should exit cleanly even with mixed ledger)
# ---------------------------------------------------------------------------
echo ""
echo "=== session-cost.py --render still works with mixed-schema ledger ==="

run_session_cost_render_compat() {
    local sandbox="$TMP/sc-render-compat"
    mkdir -p "$sandbox/.fno"

    cat > "$sandbox/.fno/ledger.json" <<'JSONEOF'
{
  "entries": [
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-compat-old",
      "cost_usd": 0.50,
      "sessions": ["sess-compat-old"],
      "title": "Old entry without provider",
      "phases_completed": []
    },
    {
      "type": "execution",
      "status": "done",
      "session_id": "sess-compat-new",
      "provider_id": "my-provider",
      "account_id": "my-account",
      "cost_usd": 0.75,
      "sessions": ["sess-compat-new"],
      "title": "New entry with provider",
      "phases_completed": []
    }
  ]
}
JSONEOF

    local output
    if output=$(HOME="$sandbox" python3 "$SESSION_COST_PY" --render 2>&1); then
        pass "SC-render-compat-HP: --render exits 0 with mixed-schema ledger"
    else
        fail "SC-render-compat-HP: --render failed with mixed-schema ledger"
        echo "    Output: $output"
    fi
}
run_session_cost_render_compat

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
