#!/usr/bin/env bash
# Tests for emit-gate-transition.sh shell-side sigma-review fixes (ab-978e93ed).
#
# Covers:
#   S1: EMIT_EVENT_TYPE != phase_transition must propagate rc on failure
#   S2: unrecognized EMIT_EVENT_TYPE must warn + exit 1
#
# Run: bash tests/test_emit_gate_transition.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
EMITTER="$REPO_ROOT/scripts/lib/emit-gate-transition.sh"

PASS=0
FAIL=0

fail() { echo "FAIL: $*" >&2; FAIL=$((FAIL+1)); }
pass() { echo "PASS: $*"; PASS=$((PASS+1)); }

if [[ ! -f "$EMITTER" ]]; then
    echo "FAIL: emitter not found at $EMITTER" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

make_target_state() {
    local dir="$1" session_id="$2" nonce="$3"
    mkdir -p "$dir/.fno"
    cat > "$dir/.fno/target-state.md" <<EOF
---
status: IN_PROGRESS
session_id: ${session_id}
provenance_nonce: ${nonce}
---
EOF
}

# Runs emit-gate-transition.sh in a synthetic repo root directory.
# Injects a fake EVENTS_SH that delegates to a stub events.sh.
run_emitter() {
    local dir="$1" event_type="$2" gate="$3" phase="$4"
    shift 4
    # Run the script with a fake plugin root so it finds our stub events.sh,
    # and with GIT_DIR faked so git rev-parse returns the tmp dir.
    (
        cd "$dir"
        git init -q .
        EMIT_EVENT_TYPE="$event_type" \
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            bash "$EMITTER" "$gate" "$phase" "$@"
    )
}

# ---------------------------------------------------------------------------
# Test S1-HP: phase_transition (legacy) always exits 0 even if emit fails
#
# The || true path is kept for phase_transition. We simulate failure by
# making events.jsonl a directory (jq append will fail), then assert rc=0.
# ---------------------------------------------------------------------------
test_s1_legacy_phase_transition_exits_0_on_failure() {
    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-s1-hp" nonce="nonce-s1hp0001"
    make_target_state "$dir" "$sid" "$nonce"

    # Make events.jsonl a directory to force jq append failure.
    mkdir -p "$dir/.fno/events.jsonl"

    local rc=0
    (
        cd "$dir"
        git init -q .
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            bash "$EMITTER" quality_check_passed review
    )
    rc=$?

    if [[ "$rc" -eq 0 ]]; then
        pass "S1-HP: phase_transition (legacy) exits 0 even when emit fails (back-compat preserved)"
    else
        fail "S1-HP: phase_transition (legacy) must exit 0 on failure for back-compat, got rc=$rc"
    fi
}

# ---------------------------------------------------------------------------
# Test S1-ERR: subagent_spawn exits non-zero when emit fails
#
# Simulate failure by pointing CLAUDE_PLUGIN_ROOT at a stub events.sh whose
# emit_event_raw always exits 1, then assert rc!=0.
# ---------------------------------------------------------------------------
test_s1_subagent_spawn_propagates_rc_on_failure() {
    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-s1-err" nonce="nonce-s1err0001"
    make_target_state "$dir" "$sid" "$nonce"

    # Create a stub plugin root with a failing events.sh.
    local stub_plugin="$dir/stub-plugin"
    mkdir -p "$stub_plugin/scripts/lib"
    cat > "$stub_plugin/scripts/lib/events.sh" <<'STUB'
#!/usr/bin/env bash
emit_event_raw() {
    echo "stub: emit_event_raw failing on purpose" >&2
    return 1
}
STUB

    local rc=0
    (
        cd "$dir"
        git init -q .
        EMIT_EVENT_TYPE="subagent_spawn" \
        CLAUDE_PLUGIN_ROOT="$stub_plugin" \
            bash "$EMITTER" subagent subagent_dispatch agent_name=code-reviewer
    )
    rc=$?

    if [[ "$rc" -ne 0 ]]; then
        pass "S1-ERR: subagent_spawn propagates rc!=0 on emit failure (panel HIGH conf 88)"
    else
        fail "S1-ERR: subagent_spawn should propagate rc!=0 on failure, but got rc=0"
    fi
}

# ---------------------------------------------------------------------------
# Test S1-ERR2: subagent_complete exits non-zero when emit fails
# ---------------------------------------------------------------------------
test_s1_subagent_complete_propagates_rc_on_failure() {
    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-s1-err2" nonce="nonce-s1err0002"
    make_target_state "$dir" "$sid" "$nonce"

    local stub_plugin="$dir/stub-plugin"
    mkdir -p "$stub_plugin/scripts/lib"
    cat > "$stub_plugin/scripts/lib/events.sh" <<'STUB'
#!/usr/bin/env bash
emit_event_raw() {
    return 1
}
STUB

    local rc=0
    (
        cd "$dir"
        git init -q .
        EMIT_EVENT_TYPE="subagent_complete" \
        CLAUDE_PLUGIN_ROOT="$stub_plugin" \
            bash "$EMITTER" subagent subagent_dispatch agent_name=code-reviewer exit_code=0
    )
    rc=$?

    if [[ "$rc" -ne 0 ]]; then
        pass "S1-ERR2: subagent_complete propagates rc!=0 on emit failure"
    else
        fail "S1-ERR2: subagent_complete should propagate rc!=0 on failure, but got rc=0"
    fi
}

# ---------------------------------------------------------------------------
# Test S2-ERR: unrecognized EMIT_EVENT_TYPE exits 1 with warning on stderr
# ---------------------------------------------------------------------------
test_s2_unrecognized_event_type_exits_1_with_warning() {
    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-s2-err" nonce="nonce-s2err0001"
    make_target_state "$dir" "$sid" "$nonce"

    local rc=0 stderr_out=""
    stderr_out=$(
        cd "$dir"
        git init -q . 2>/dev/null
        EMIT_EVENT_TYPE="subagent_spawned" \
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            bash "$EMITTER" subagent subagent_dispatch 2>&1 >/dev/null
    )
    rc=$?

    if [[ "$rc" -ne 0 ]]; then
        pass "S2-ERR: unrecognized EMIT_EVENT_TYPE exits 1 (not silent no-op)"
    else
        fail "S2-ERR: unrecognized EMIT_EVENT_TYPE must exit 1, got rc=0"
    fi

    if echo "$stderr_out" | grep -qi "unrecognized\|WARNING"; then
        pass "S2-ERR: unrecognized EMIT_EVENT_TYPE emits warning to stderr"
    else
        fail "S2-ERR: unrecognized EMIT_EVENT_TYPE must warn on stderr, got: '$stderr_out'"
    fi
}

# ---------------------------------------------------------------------------
# Test S2-HP: recognized types still work (regression guard)
# ---------------------------------------------------------------------------
test_s2_recognized_types_still_succeed() {
    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-s2-hp" nonce="nonce-s2hp0001"
    make_target_state "$dir" "$sid" "$nonce"

    local rc=0
    (
        cd "$dir"
        git init -q .
        EMIT_EVENT_TYPE="subagent_spawn" \
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            bash "$EMITTER" subagent subagent_dispatch agent_name=code-reviewer
    )
    rc=$?

    if [[ "$rc" -eq 0 ]]; then
        pass "S2-HP: recognized EMIT_EVENT_TYPE=subagent_spawn still exits 0 on success"
    else
        fail "S2-HP: recognized type must exit 0 on success, got rc=$rc"
    fi
}

# ---------------------------------------------------------------------------
# Test TYPED-KV: bare false/null/number/array extra-kv values merge as their
# JSON type, not as strings (ab-8cc43d46).
#
# The old `jq -e .` probe exited 1 on a bare `false`/`null` (the -e flag
# reflects output truthiness, not parse success), so those values silently
# landed as the strings "false"/"null". The try-argjson-then-arg pattern
# (mirrored from set-gate.sh) types them correctly while still falling back to
# `--arg` for genuine non-JSON strings.
# ---------------------------------------------------------------------------
test_typed_extra_kv_preserves_json_types() {
    if ! command -v jq >/dev/null 2>&1; then
        pass "TYPED-KV: skipped (jq unavailable)"
        return
    fi

    local dir
    dir="$(mktemp -d)"
    trap 'rm -rf "$dir"' RETURN

    local sid="sess-typed" nonce="nonce-typed00001"
    make_target_state "$dir" "$sid" "$nonce"

    (
        cd "$dir"
        git init -q .
        EMIT_EVENT_TYPE="phase_transition" \
        CLAUDE_PLUGIN_ROOT="$REPO_ROOT" \
            bash "$EMITTER" quality_check_passed review \
                bf=false bt=true nv=null num=3 arr='[1,2]' str=hello
    )

    local types
    types=$(tail -1 "$dir/.fno/events.jsonl" 2>/dev/null \
        | jq -c '.data | [(.bf|type),(.bt|type),(.nv|type),(.num|type),(.arr|type),(.str|type)]' 2>/dev/null)

    if [[ "$types" == '["boolean","boolean","null","number","array","string"]' ]]; then
        pass "TYPED-KV: bare false/null/number/array merge as JSON types (not strings)"
    else
        fail "TYPED-KV: expected [boolean,boolean,null,number,array,string], got: ${types:-<none>}"
    fi
}

# ---------------------------------------------------------------------------
# Run all tests
# ---------------------------------------------------------------------------

echo "Running emit-gate-transition.sh sigma-review fix tests (ab-978e93ed)..."
echo ""
test_s1_legacy_phase_transition_exits_0_on_failure
test_s1_subagent_spawn_propagates_rc_on_failure
test_s1_subagent_complete_propagates_rc_on_failure
test_s2_unrecognized_event_type_exits_1_with_warning
test_s2_recognized_types_still_succeed
test_typed_extra_kv_preserves_json_types
echo ""
echo "Total: $((PASS+FAIL)) | Pass: $PASS | Fail: $FAIL"
[[ "$FAIL" -eq 0 ]] || exit 1
