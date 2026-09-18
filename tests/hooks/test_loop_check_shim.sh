#!/usr/bin/env bash
# Test suite for the native Stop hook path (hooks/target-stop-hook.sh after
# the hook is a ~12-line exec wrapper of `fno-agents hook stop`,
# and the translation the old shell shim carried (ownership, foreign-session
# guard, decision translation, harness-shaped block, terminal cleanup) lives
# in crates/fno-agents/src/hook/stop.rs. These tests drive the REAL binary
# through the wrapper with file fixtures: manifest, transcript, space.
#
# The stub-driven cases from the shell era are gone with the shell: T2/T7/T9/
# T10/T11/T15/T18 staged a lying or failing loop-check child, which the native
# handler cannot even express (decide runs in process), and the bounded
# unavailable counters now guard decide-not-answering, which needs fault
# injection to stage. Every case that survives keeps its contract number.
#
# Tests:
#   T1  no state file -> exit 0, empty stdout
#   T2  (rewired to AC15-EDGE) no fno-agents binary on any path -> one stderr
#       line, exit 0, empty stdout - the named behavior change
#   T3  no-intent block (non-claude env) -> exit 2, continue message on stderr
#   T4  advisory-unit promise -> allow terminal, exit 0
#   T5  read-only invariant: state file unchanged across a block fire
#   T6  foreign transcript -> exit 0 without a block
#   T8  claude_transcript_id: null does not disable the hook
#   T13 codex-authored manifest + a CLAUDE stop -> exit 0, no block
#   T14 codex-authored manifest + that codex session's own stop -> judged
#   T16 block decision (claude env) -> stdout {"decision":"block","reason"} + exit 0
#   T17 claude marker + foreign marker -> ambiguous, legacy exit-2 block, no JSON

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"
BIN="${FNO_AGENTS_BIN:-${REPO_ROOT}/crates/fno-agents/target/release/fno-agents}"

# ── counters ────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[shim] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[shim] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[shim] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[shim] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ───────────────────────────────────────────────────────────────
[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
[[ -x "$BIN" ]] || { fail "fno-agents binary not executable at $BIN (build it or set FNO_AGENTS_BIN)"; exit 1; }

# ── fixture builders ─────────────────────────────────────────────────────────
# Globals set: TMP_DIR HOME_DIR SPACE_DIR TRANSCRIPT_FILE STATE_FILE
setup_env() {
    local uuid="${1:-aaaa-0000}"

    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    SPACE_DIR="${TMP_DIR}/space"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}" "${SPACE_DIR}/kings"

    TRANSCRIPT_FILE="${TMP_DIR}/${uuid}.jsonl"
    printf '{"role":"assistant","content":"hello"}\n' > "$TRANSCRIPT_FILE"

    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: test-session-001
created_at: 2026-06-05T00:00:00Z
claude_transcript_id: ${uuid}
attended: true
---
STATE
}

setup_env_codex() {
    local transcript_basename="$1"
    local thread_uuid="$2"

    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    SPACE_DIR="${TMP_DIR}/space"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}" "${SPACE_DIR}/kings"

    TRANSCRIPT_FILE="${TMP_DIR}/${transcript_basename}.jsonl"
    printf '{"role":"assistant","content":"hello"}\n' > "$TRANSCRIPT_FILE"

    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: test-session-codex
created_at: 2026-06-05T00:00:00Z
harness: codex
harness_session_id: ${thread_uuid}
claude_session_id:
codex_thread_id: ${thread_uuid}
attended: false
---
STATE
}

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" "${HOME_DIR:-/nonexistent}" 2>/dev/null || true; }

safe_path() {
    echo "/usr/bin:/bin:/usr/sbin:/sbin"
}

# ── helper: run the hook from a given cwd ───────────────────────────────────
# Usage: run_hook <cwd> <stdin_json> [env vars as NAME=VALUE ...]
# Returns rc via $HOOK_RC, stdout via $HOOK_STDOUT, stderr via $HOOK_STDERR.
run_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift

    HOOK_RC=0
    HOOK_STDOUT=""
    HOOK_STDERR=""
    local out
    out=$(
        cd "$cwd" || exit 1
        env CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= CODEX_THREAD_ID= \
            FNO_AGENTS_BIN="$BIN" FNO_EVENTS_PATH="${SPACE_DIR}/events.jsonl" \
            HOME="${HOME_DIR}" PATH="$(safe_path)" \
            "$@" bash "$HOOK" <<< "$input_json" 2>"${TMP_DIR}/stderr.txt"
    )
    HOOK_RC=$?
    HOOK_STDOUT="$out"
    HOOK_STDERR="$(cat "${TMP_DIR}/stderr.txt" 2>/dev/null || true)"
}

# The payload the hook reads: transcript + cwd, no completion intent.
payload() {
    printf '{"transcript_path":"%s","session_id":"%s","cwd":"%s","last_assistant_message":"Work continues."}' \
        "$TRANSCRIPT_FILE" "${1:-sess-t}" "$TMP_DIR"
}
# A promise payload for the advisory-unit terminal case.
promise_payload() {
    printf '{"transcript_path":"%s","session_id":"%s","cwd":"%s","last_assistant_message":"<promise>MISSION COMPLETE: t4</promise>"}' \
        "$TRANSCRIPT_FILE" "${1:-sess-t}" "$TMP_DIR"
}

# ─────────────────────────────────────────────────────────────────────────────
# T1: no state file -> exit 0
# ─────────────────────────────────────────────────────────────────────────────
log "T1: no state file -> exit 0"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    SPACE_DIR="${TMP_DIR}/space"
    mkdir -p "${TMP_DIR}" "${HOME_DIR}" "${SPACE_DIR}"
    TRANSCRIPT_FILE="${TMP_DIR}/aaaa-0001.jsonl"
    printf '{}' > "$TRANSCRIPT_FILE"

    run_hook "$TMP_DIR" "$(payload sess-t1)"

    t1_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T1: expected exit 0, got $HOOK_RC"
        t1_ok=false
    fi
    if [[ -n "$HOOK_STDOUT" ]]; then
        fail "T1: expected empty stdout, got: $HOOK_STDOUT"
        t1_ok=false
    fi
    if ls "${TMP_DIR}/.fno/.loop-check-unavail-"* >/dev/null 2>&1; then
        fail "T1: an unavailable counter was written despite no state file"
        t1_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -q "visitor allowed"; then
        fail "T1: visitor diagnostic absent; got: $HOOK_STDERR"
        t1_ok=false
    fi
    rm -rf "$TMP_DIR" 2>/dev/null || true
    [[ "$t1_ok" == "true" ]] && pass "T1: no state file -> exit 0, visitor diagnostic, no counter"
}

# ─────────────────────────────────────────────────────────────────────────────
# T2 (rewired to AC15-EDGE): no binary on any path -> allow with one stderr line
# ─────────────────────────────────────────────────────────────────────────────
log "T2: no fno-agents binary -> one stderr line, exit 0 (AC15-EDGE)"
{
    setup_env "bbbb-0002"
    run_hook "$TMP_DIR" "$(payload sess-t2)" \
        "PATH=$(safe_path)" \
        "FNO_AGENTS_BIN=/nonexistent/fno-agents"

    t2_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T2: expected exit 0, got $HOOK_RC"
        t2_ok=false
    fi
    if [[ -n "$HOOK_STDOUT" ]]; then
        fail "T2: expected empty stdout, got: $HOOK_STDOUT"
        t2_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -q "no fno-agents could answer the hook verb"; then
        fail "T2: expected the one stderr line; got: $HOOK_STDERR"
        t2_ok=false
    fi
    cleanup
    [[ "$t2_ok" == "true" ]] && pass "T2: missing binary -> allow with one stderr line"
}

# ─────────────────────────────────────────────────────────────────────────────
# T3: no-intent fire with no gh on PATH -> the advisory gate runs before the
# no-intent block: exit 2, the advisory line on stderr.
# ─────────────────────────────────────────────────────────────────────────────
log "T3: no-intent fire, gh absent -> exit 2 + advisory line on stderr"
{
    setup_env "cccc-0003"
    run_hook "$TMP_DIR" "$(payload sess-t3)"

    t3_ok=true
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T3: expected exit 2, got $HOOK_RC (stderr: $HOOK_STDERR)"
        t3_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -q "advisory mode"; then
        fail "T3: advisory line not in stderr; got: $HOOK_STDERR"
        t3_ok=false
    fi
    [[ "$t3_ok" == "true" ]] && pass "T3: block -> exit 2 + advisory line"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T4: advisory-unit promise -> allow terminal, exit 0
# ─────────────────────────────────────────────────────────────────────────────
log "T4: no_ship manifest + promise -> DoneAdvisory allow, exit 0"
{
    setup_env "dddd-0004"
    printf '%s\n' "no_ship: true" >> "$STATE_FILE"
    run_hook "$TMP_DIR" "$(promise_payload sess-t4)"

    t4_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T4: expected exit 0, got $HOOK_RC (stderr: $HOOK_STDERR)"
        t4_ok=false
    fi
    if ! echo "$HOOK_STDERR" | grep -q "DoneAdvisory\|advisory"; then
        fail "T4: expected the advisory terminal on stderr; got: $HOOK_STDERR"
        t4_ok=false
    fi
    [[ "$t4_ok" == "true" ]] && pass "T4: allow -> exit 0 with advisory terminal"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T5: read-only invariant: state file unchanged after a block fire
# ─────────────────────────────────────────────────────────────────────────────
log "T5: read-only invariant"
{
    setup_env "eeee-0005"
    file_sum() {
        if command -v shasum >/dev/null 2>&1; then shasum "$1" | awk '{print $1}';
        else sha256sum "$1" | awk '{print $1}'; fi
    }
    local_before="$(file_sum "$STATE_FILE")"
    run_hook "$TMP_DIR" "$(payload sess-t5)"
    local_after="$(file_sum "$STATE_FILE")"
    if [[ "$local_before" == "$local_after" ]]; then
        pass "T5: state file unchanged (checksums match)"
    else
        fail "T5: state file was modified (before=$local_before after=$local_after)"
    fi
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T6: foreign transcript -> exit 0 without a block
# ─────────────────────────────────────────────────────────────────────────────
log "T6: foreign transcript -> exit 0, not judged"
{
    setup_env "ffff-0006"
    # The manifest names a DIFFERENT claude transcript id.
    sed -i '' 's/claude_transcript_id: ffff-0006/claude_transcript_id: other-id/' "$STATE_FILE" 2>/dev/null \
        || sed -i 's/claude_transcript_id: ffff-0006/claude_transcript_id: other-id/' "$STATE_FILE"
    run_hook "$TMP_DIR" "$(payload sess-t6)"

    t6_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T6: expected exit 0, got $HOOK_RC"
        t6_ok=false
    fi
    if echo "$HOOK_STDERR" | grep -q "continue working"; then
        fail "T6: the foreign stop was judged (block text present)"
        t6_ok=false
    fi
    [[ "$t6_ok" == "true" ]] && pass "T6: foreign transcript -> exit 0, not judged"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T8: claude_transcript_id: null does not disable the hook
# ─────────────────────────────────────────────────────────────────────────────
log "T8: null transcript-id does not disable the hook"
{
    setup_env "hhhh-0008"
    sed -i '' 's/claude_transcript_id: hhhh-0008/claude_transcript_id: null/' "$STATE_FILE" 2>/dev/null \
        || sed -i 's/claude_transcript_id: hhhh-0008/claude_transcript_id: null/' "$STATE_FILE"
    run_hook "$TMP_DIR" "$(payload sess-t8)"

    t8_ok=true
    # The hook must still JUDGE (the ownership evaluation ran and the
    # manifest names nobody), which on the native path is the visitor
    # diagnostic naming every id it tried.
    if ! echo "$HOOK_STDERR" | grep -q "visitor allowed (tried:"; then
        fail "T8: null transcript-id disabled the hook (no visitor diagnostic); got: $HOOK_STDERR"
        t8_ok=false
    fi
    [[ "$t8_ok" == "true" ]] && pass "T8: null transcript-id does not disable the hook"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T13/T14: codex-authored manifest, foreign vs own stop
# ─────────────────────────────────────────────────────────────────────────────
log "T13: codex-authored manifest + a CLAUDE stop -> exit 0, no block"
{
    setup_env_codex "a-claude-uuid-1300" "th-uuid-13"
    # A CLAUDE session's stop: the transcript names no codex thread.
    run_hook "$TMP_DIR" "$(payload sess-t13)"

    t13_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T13: expected exit 0, got $HOOK_RC"
        t13_ok=false
    fi
    if echo "$HOOK_STDERR" | grep -q "continue working"; then
        fail "T13: binary judged another harness's manifest"
        t13_ok=false
    fi
    [[ "$t13_ok" == "true" ]] && pass "T13: foreign harness manifest -> exit 0, not judged"
    cleanup
}

log "T14: codex-authored manifest + that codex session's own stop -> judged"
{
    setup_env_codex "rollout-20260905T000000-th-uuid-14" "th-uuid-14"
    run_hook "$TMP_DIR" "$(payload sess-t14)"

    t14_ok=true
    # The ownership check needs a gh read first; with gh absent the stop is
    # still ANSWERED - the advisory gate's line - rather than ignored.
    if ! echo "$HOOK_STDERR" | grep -q "advisory mode"; then
        fail "T14: the owner's own stop was not judged; got: $HOOK_STDERR"
        t14_ok=false
    fi
    [[ "$t14_ok" == "true" ]] && pass "T14: owner's own stop -> judged (advisory line)"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T16: block decision (claude env) -> stdout {"decision":"block","reason"} + exit 0
# ─────────────────────────────────────────────────────────────────────────────
log "T16: block under claude markers -> stdout JSON + exit 0"
{
    setup_env "pppp-0016"
    run_hook "$TMP_DIR" "$(payload sess-t16)" "CLAUDECODE=1"

    t16_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T16: expected exit 0, got $HOOK_RC"
        t16_ok=false
    fi
    if ! echo "$HOOK_STDOUT" | jq -e '.decision == "block"' >/dev/null 2>&1; then
        fail "T16: stdout is not a block decision JSON; got: $HOOK_STDOUT"
        t16_ok=false
    fi
    [[ "$t16_ok" == "true" ]] && pass "T16: block under claude markers -> structured stdout block"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# T17: claude marker + foreign marker -> ambiguous, legacy exit-2 block, no JSON
# ─────────────────────────────────────────────────────────────────────────────
log "T17: ambiguous markers -> exit-2 block, no stdout JSON"
{
    setup_env "qqqq-0017"
    run_hook "$TMP_DIR" "$(payload sess-t17)" "CLAUDECODE=1" "CODEX_THREAD_ID=foreign-thread"

    t17_ok=true
    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "T17: ambiguous markers must take the exit-2 block, got $HOOK_RC"
        t17_ok=false
    fi
    if [[ -n "$HOOK_STDOUT" ]]; then
        fail "T17: ambiguous markers must not emit stdout JSON: $HOOK_STDOUT"
        t17_ok=false
    fi
    [[ "$t17_ok" == "true" ]] && pass "T17: ambiguous markers -> exit-2 block, no JSON"
    cleanup
}

echo ""
echo "[shim] Results: $PASS passed, $FAIL failed, $SKIP_COUNT skipped"
[[ $FAIL -eq 0 ]]
