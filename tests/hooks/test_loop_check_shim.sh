#!/usr/bin/env bash
# Test suite for the loop-check shim (hooks/target-stop-hook.sh after Task 2.1).
#
# Task 2.1 / ab-d0337fbc (control-plane collapse wedge): the stop hook is now a
# read-only shim that delegates all stop/allow decisions to `fno-agents loop-check`.
# These tests exercise the shim's orchestration logic: binary resolution, foreign-
# session guard, decision translation, and the read-only invariant.
#
# Tests:
#   T1  no state file -> exit 0, no unavailable counter written
#   T2  binary missing (active session) -> exit 2 bounded-block + event + counter=1
#   T3  block decision -> exit 2, message on stderr
#   T4  allow decision with TerminationReason -> exit 0
#   T5  read-only invariant: state file unchanged across a block fire
#   T6  foreign transcript -> exit 0 without invoking the binary
#   T7  verb returns garbage output (active session) -> exit 2 bounded-block + warning
#   T8  claude_transcript_id: null still invokes the binary
#
# x-81d9 active-session-aware error handling (AC2):
#   T9   verb non-zero (active session) -> exit 2 bounded-block + warning
#   T10  counter at MAX -> loud give-up: exit 0 + loop_check_unavailable_giveup (both logs)
#   T11  counter is per-session_id (a sibling session's budget is untouched)
#   T12  clean decision self-heals the counter (removed before honoring)
#
# Each test feeds the shim stdin JSON: {"transcript_path":"<tmp>/<uuid>.jsonl"}
# and runs the shim from a tmp cwd containing .fno/target-state.md.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"

# ── counters ────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[shim] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[shim] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[shim] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[shim] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ───────────────────────────────────────────────────────────────
[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v jq    >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }
command -v bash  >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }
command -v shasum >/dev/null 2>&1 || command -v sha256sum >/dev/null 2>&1 || { skip "no shasum/sha256sum"; exit 77; }

# ── helper: checksum a file portably ────────────────────────────────────────
file_sum() {
    if command -v shasum >/dev/null 2>&1; then
        shasum "$1" | awk '{print $1}'
    else
        sha256sum "$1" | awk '{print $1}'
    fi
}

# ── helper: build a tmp project dir with a state file ───────────────────────
# Usage: setup_env <transcript_uuid> [claude_transcript_id_override]
# Sets globals: TMP_DIR HOME_DIR TRANSCRIPT_FILE STATE_FILE
setup_env() {
    local uuid="${1:-aaaa-0000}"
    local ctid="${2:-$uuid}"        # claude_transcript_id in state frontmatter

    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"

    TRANSCRIPT_FILE="${TMP_DIR}/${uuid}.jsonl"
    printf '{"role":"assistant","content":"hello"}\n' > "$TRANSCRIPT_FILE"

    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: test-session-001
created_at: 2026-06-05T00:00:00Z
claude_transcript_id: ${ctid}
attended: true
status: IN_PROGRESS
---
STATE
}

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" "${HOME_DIR:-/nonexistent}" 2>/dev/null || true; }

# ── helper: strip a PATH of fno-agents executables ──────────────────────────
safe_path() {
    echo "/usr/bin:/bin:/usr/sbin:/sbin"
}

# ── helper: make a stub binary ───────────────────────────────────────────────
# Reads the script body from stdin
make_stub() {
    local path="$1"
    cat > "$path"
    chmod +x "$path"
}

# ── helper: run the hook from a given cwd ───────────────────────────────────
# Usage: run_hook <cwd> <stdin_json> [env vars as NAME=VALUE ...]
# Returns rc via $HOOK_RC, stderr via $HOOK_STDERR
run_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift
    # remaining args: env assignments (KEY=VALUE)

    HOOK_RC=0
    HOOK_STDERR=""
    HOOK_STDERR=$(
        cd "$cwd" || exit 1
        env "$@" bash "$HOOK" <<< "$input_json" 2>&1 >/dev/null
    ) || HOOK_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# T1: no state file -> exit 0
# ─────────────────────────────────────────────────────────────────────────────
log "T1: no state file -> exit 0"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"
    TRANSCRIPT_FILE="${TMP_DIR}/aaaa-0001.jsonl"
    printf '{}' > "$TRANSCRIPT_FILE"
    # No .fno/target-state.md created

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" "HOME=${HOME_DIR}"

    t1_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T1: expected exit 0, got $HOOK_RC"
        t1_ok=false
    fi
    # AC2-HP: no state file -> instant allow, no counter written.
    if ls "${TMP_DIR}/.fno/.loop-check-unavail-"* >/dev/null 2>&1; then
        fail "T1: an unavailable counter was written despite no state file"
        t1_ok=false
    fi
    rm -rf "$TMP_DIR" 2>/dev/null || true
    [[ "$t1_ok" == "true" ]] && pass "T1: no state file -> exit 0, no counter"
}

# ─────────────────────────────────────────────────────────────────────────────
# T2: binary missing -> exit 0 + loop_check_binary_missing event emitted
# ─────────────────────────────────────────────────────────────────────────────
log "T2: binary missing (active session) -> exit 2 bounded-block + event + counter=1"
{
    setup_env "bbbb-0002"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    # FNO_AGENTS_BIN points at nonexistent; PATH has no fno-agents;
    # REPO_ROOT set to a dir with no crates/fno-agents/target/
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "PATH=$(safe_path)" \
        "FNO_AGENTS_BIN=/nonexistent"

    proj_events="${TMP_DIR}/.fno/events.jsonl"
    counter="${TMP_DIR}/.fno/.loop-check-unavail-test-session-001"

    t2_ok=true

    # AC2-ERR: a missing binary for an ACTIVE session bounded-blocks (was the
    # old silent exit 0 that disabled the ship gate).
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T2: expected exit 2 (bounded block), got $HOOK_RC"
        t2_ok=false
    fi

    # The diagnostic event still fires (before the block).
    if [[ -f "$proj_events" ]] && grep -q 'loop_check_binary_missing' "$proj_events" 2>/dev/null; then
        : # good
    else
        fail "T2: loop_check_binary_missing not found in project events.jsonl (file: $proj_events)"
        t2_ok=false
    fi

    if [[ "$(tr -dc '0-9' < "$counter" 2>/dev/null)" != "1" ]]; then
        fail "T2: expected counter=1 at $counter; got: $(cat "$counter" 2>/dev/null)"
        t2_ok=false
    fi

    if ! echo "$HOOK_STDERR" | grep -qi 'missing\|not found\|binary\|fno-agents'; then
        fail "T2: stderr does not mention missing binary; got: $HOOK_STDERR"
        t2_ok=false
    fi

    [[ "$t2_ok" == "true" ]] && pass "T2: binary missing -> exit 2 + event + counter=1"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T3: block decision -> exit 2, message on stderr
# ─────────────────────────────────────────────────────────────────────────────
log "T3: block decision -> exit 2 + message on stderr"
{
    setup_env "cccc-0003"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
printf '{"decision":"block","termination_reason":null,"message":"keep going","fires":1,"fingerprint":"x"}\n'
exit 0
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${STUB}"

    t3_ok=true
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T3: expected exit 2, got $HOOK_RC"
        t3_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -q 'keep going'; then
        fail "T3: 'keep going' not in stderr; got: $HOOK_STDERR"
        t3_ok=false
    fi
    [[ "$t3_ok" == "true" ]] && pass "T3: block -> exit 2 + correct message"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T4: allow decision with termination_reason -> exit 0
# ─────────────────────────────────────────────────────────────────────────────
log "T4: allow decision -> exit 0"
{
    setup_env "dddd-0004"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
printf '{"decision":"allow","termination_reason":"DonePRGreen","message":"PR merged","fires":1,"fingerprint":"y"}\n'
exit 0
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${STUB}"

    if [[ "$HOOK_RC" -eq 0 ]]; then
        pass "T4: allow -> exit 0"
    else
        fail "T4: expected exit 0, got $HOOK_RC"
    fi
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T5: read-only invariant: state file unchanged after a block fire
# ─────────────────────────────────────────────────────────────────────────────
log "T5: read-only invariant"
{
    setup_env "eeee-0005"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
printf '{"decision":"block","termination_reason":null,"message":"stop now","fires":2,"fingerprint":"z"}\n'
exit 0
STUB

    before_sum=$(file_sum "$STATE_FILE")

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${STUB}"

    after_sum=$(file_sum "$STATE_FILE")

    if [[ "$before_sum" == "$after_sum" ]]; then
        pass "T5: state file unchanged (checksums match)"
    else
        fail "T5: state file was modified (before=$before_sum after=$after_sum)"
    fi
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T6: foreign transcript -> exit 0 without invoking the binary
# ─────────────────────────────────────────────────────────────────────────────
log "T6: foreign transcript -> exit 0, binary not called"
{
    # manifest claude_transcript_id=aaaa-1111; transcript file is bbbb-2222.jsonl
    setup_env "bbbb-2222" "aaaa-1111"

    MARKER="${TMP_DIR}/stub_was_called"
    STUB="${TMP_DIR}/fno-agents-stub"
    # The marker path must be embedded literally into the stub
    cat > "$STUB" <<STUB_EOF
#!/usr/bin/env bash
touch "${MARKER}"
printf '{"decision":"block","termination_reason":null,"message":"should not see this","fires":1,"fingerprint":"f"}\n'
exit 0
STUB_EOF
    chmod +x "$STUB"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${STUB}"

    t6_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T6: expected exit 0, got $HOOK_RC"
        t6_ok=false
    fi
    if [[ -f "$MARKER" ]]; then
        fail "T6: stub was invoked for foreign transcript"
        t6_ok=false
    fi
    [[ "$t6_ok" == "true" ]] && pass "T6: foreign transcript -> exit 0, stub not called"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T7: verb returns garbage output -> exit 0 with warning
# ─────────────────────────────────────────────────────────────────────────────
log "T7: garbage output from verb (active session) -> exit 2 bounded-block + warning"
{
    setup_env "ffff-0007"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
printf 'not json\n'
exit 0
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${STUB}"

    t7_ok=true
    # Non-JSON output for an active session is checker-unavailable -> block.
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T7: expected exit 2 (bounded block), got $HOOK_RC"
        t7_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -qi 'warning\|invalid\|json\|parse\|unexpected\|unavailable'; then
        fail "T7: expected a warning on stderr; got: $HOOK_STDERR"
        t7_ok=false
    fi
    [[ "$t7_ok" == "true" ]] && pass "T7: garbage output -> exit 2 + warning"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T9: verb non-zero (active session) -> exit 2 bounded-block + warning (AC2-ERR)
# ─────────────────────────────────────────────────────────────────────────────
log "T9: verb non-zero -> exit 2 bounded-block + warning"
{
    setup_env "9999-0009"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
echo "boom" >&2
exit 3
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"

    counter="${TMP_DIR}/.fno/.loop-check-unavail-test-session-001"
    t9_ok=true
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T9: expected exit 2, got $HOOK_RC"; t9_ok=false
    fi
    if [[ "$(tr -dc '0-9' < "$counter" 2>/dev/null)" != "1" ]]; then
        fail "T9: expected counter=1; got: $(cat "$counter" 2>/dev/null)"; t9_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -qi 'unavailable\|exited\|warning'; then
        fail "T9: expected a warning on stderr; got: $HOOK_STDERR"; t9_ok=false
    fi
    [[ "$t9_ok" == "true" ]] && pass "T9: verb non-zero -> exit 2 + counter=1 + warning"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T10: counter at MAX -> loud give-up (exit 0 + event to both logs) (AC2-UI)
# ─────────────────────────────────────────────────────────────────────────────
log "T10: counter at MAX -> give-up exit 0 + loop_check_unavailable_giveup"
{
    setup_env "aaaa-0010"

    # Pre-seed the counter at the ceiling (3): the next unavailable fire gives up.
    printf '3' > "${TMP_DIR}/.fno/.loop-check-unavail-test-session-001"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" "PATH=$(safe_path)" "FNO_AGENTS_BIN=/nonexistent"

    proj_events="${TMP_DIR}/.fno/events.jsonl"
    global_events="${HOME_DIR}/.fno/events.jsonl"
    t10_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T10: expected exit 0 (give-up), got $HOOK_RC"; t10_ok=false
    fi
    if ! grep -q 'loop_check_unavailable_giveup' "$proj_events" 2>/dev/null; then
        fail "T10: give-up event missing from project events"; t10_ok=false
    fi
    if ! grep -q 'loop_check_unavailable_giveup' "$global_events" 2>/dev/null; then
        fail "T10: give-up event missing from global events"; t10_ok=false
    fi
    [[ "$t10_ok" == "true" ]] && pass "T10: give-up -> exit 0 + event in both logs"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T11: counter is per-session_id -> a sibling's budget is untouched (AC2-EDGE)
# ─────────────────────────────────────────────────────────────────────────────
log "T11: per-session counter isolation"
{
    setup_env "bbbb-0011"

    # A sibling session B already has a counter at 2 in the shared .fno.
    sibling="${TMP_DIR}/.fno/.loop-check-unavail-sibling-session-B"
    printf '2' > "$sibling"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
exit 3
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"

    mine="${TMP_DIR}/.fno/.loop-check-unavail-test-session-001"
    t11_ok=true
    if [[ "$(tr -dc '0-9' < "$mine" 2>/dev/null)" != "1" ]]; then
        fail "T11: my counter should be 1; got: $(cat "$mine" 2>/dev/null)"; t11_ok=false
    fi
    if [[ "$(tr -dc '0-9' < "$sibling" 2>/dev/null)" != "2" ]]; then
        fail "T11: sibling counter was mutated; got: $(cat "$sibling" 2>/dev/null)"; t11_ok=false
    fi
    [[ "$t11_ok" == "true" ]] && pass "T11: counters isolated per session_id"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T12: a clean decision self-heals (removes) the counter (AC2-FR)
# ─────────────────────────────────────────────────────────────────────────────
log "T12: clean decision self-heals the counter"
{
    setup_env "cccc-0012"

    # Counter is at 2 from prior broken fires.
    counter="${TMP_DIR}/.fno/.loop-check-unavail-test-session-001"
    printf '2' > "$counter"

    STUB="${TMP_DIR}/fno-agents-stub"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
printf '{"decision":"block","termination_reason":null,"message":"keep going","fires":1,"fingerprint":"x"}\n'
exit 0
STUB

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT_FILE}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"

    t12_ok=true
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T12: expected exit 2 (block decision honored), got $HOOK_RC"; t12_ok=false
    fi
    if [[ -f "$counter" ]]; then
        fail "T12: counter should have been removed on a clean decision"; t12_ok=false
    fi
    [[ "$t12_ok" == "true" ]] && pass "T12: clean decision removed the counter"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# Summary
# ─────────────────────────────────────────────────────────────────────────────
echo ""

# T8: claude_transcript_id: null must NOT disable the hook (codex P2 #447) -
# the binary must still be invoked (stub writes its marker).
t8() {
    local T; T=$(mktemp -d)
    local MARKER="$T/invoked"
    local STUB="$T/fno-agents"
    cat > "$STUB" <<STUBEOF
#!/bin/sh
touch "$MARKER"
echo '{"decision":"allow","termination_reason":null,"message":"ok","fires":1,"fingerprint":null}'
STUBEOF
    chmod +x "$STUB"
    mkdir -p "$T/proj/.fno"
    cat > "$T/proj/.fno/target-state.md" <<MANEOF
---
session_id: s8
created_at: 2026-06-05T00:00:00Z
claude_transcript_id: null
---
MANEOF
    local TR="$T/some-real-uuid.jsonl"
    echo '{"message":{"role":"assistant","content":"hi"}}' > "$TR"
    ( cd "$T/proj" && printf '{"transcript_path":"%s"}' "$TR" | HOME="$T" FNO_AGENTS_BIN="$STUB" bash "$HOOK" >/dev/null 2>&1 )
    local rc=$?
    if [[ -f "$MARKER" && $rc -eq 0 ]]; then
        pass "T8: null transcript-id does not disable the hook (binary invoked)"
    else
        fail "T8: binary not invoked despite null transcript-id (rc=$rc)"
    fi
}
t8

printf '[shim] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
