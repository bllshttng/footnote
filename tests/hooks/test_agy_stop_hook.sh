#!/usr/bin/env bash
# Test suite for the agy Stop-hook adapter (hooks/agy-target-stop-hook.sh).
#
# The adapter is a thin translator over `fno-agents loop-check` for agy's
# Gemini-family wire format: camelCase stdin, decision:"continue" to KEEP WORKING,
# JSON-only stdout (no exit-2 path). These tests exercise the translation logic
# with a stubbed fno-agents binary.
#
# Tests:
#   T1  no state file              -> stdout {} (allow stop)
#   T2  fullyIdle == false         -> stdout continue (bg tasks live), binary not called
#   T3  binary missing             -> stdout {} (allow) + loop_check_binary_missing event
#   T4  loop-check block            -> stdout continue, message carried in reason
#   T5  loop-check terminal allow   -> stdout {} + finalize invoked
#   T6  loop-check garbage (present) -> stdout continue (transient retry) + event
#   T7  Gemini-shaped transcript    -> synthesized claude line reaches loop-check
#   T8  silence rule                -> stdout is exactly ONE JSON object, nothing else

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/agy-target-stop-hook.sh"

PASS=0; FAIL=0; SKIP_COUNT=0
log()  { printf '[agy] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[agy] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[agy] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[agy] SKIP: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v jq   >/dev/null 2>&1 || { skip "jq not on PATH";  exit 77; }
command -v bash >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }

# ── helpers ───────────────────────────────────────────────────────────────────
# setup_env: build a tmp project with a state file + an agy-shaped transcript.
# Sets: TMP_DIR HOME_DIR TRANSCRIPT_FILE STATE_FILE
setup_env() {
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"
    TRANSCRIPT_FILE="${TMP_DIR}/transcript.jsonl"
    printf '{"role":"model","parts":[{"text":"hello"}]}\n' > "$TRANSCRIPT_FILE"
    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: agy-test-001
created_at: 2026-06-27T00:00:00Z
attended: false
---
STATE
}
cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" 2>/dev/null || true; }

make_stub() { cat > "$1"; chmod +x "$1"; }
safe_path() { echo "/usr/bin:/bin:/usr/sbin:/sbin"; }

# run_hook <cwd> <stdin_json> [env KEY=VALUE ...] -> sets HOOK_STDOUT, HOOK_RC, HOOK_STDERR
run_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift
    local err_file; err_file="$(mktemp)"
    HOOK_RC=0
    HOOK_STDOUT=$(
        cd "$cwd" || exit 1
        env "$@" bash "$HOOK" <<< "$input_json" 2>"$err_file"
    ) || HOOK_RC=$?
    HOOK_STDERR="$(cat "$err_file")"; rm -f "$err_file"
}

# Assert HOOK_STDOUT is a single JSON object whose .decision matches (or absent).
stdout_decision() { printf '%s' "$HOOK_STDOUT" | jq -r '.decision // "<none>"' 2>/dev/null; }

# ── T1: no state file -> {} ───────────────────────────────────────────────────
log "T1: no state file -> allow {}"
{
    TMP_DIR="$(mktemp -d)"; HOME_DIR="${TMP_DIR}/home"; mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"
    TR="${TMP_DIR}/t.jsonl"; printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$TR"
    INPUT="{\"transcriptPath\":\"${TR}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}"
    if [[ "$HOOK_RC" -eq 0 && "$(stdout_decision)" == "<none>" ]] && printf '%s' "$HOOK_STDOUT" | jq -e . >/dev/null 2>&1; then
        pass "T1: emitted {} (allow), no footnote session"
    else
        fail "T1: expected {} allow; rc=$HOOK_RC stdout=$HOOK_STDOUT"
    fi
    rm -rf "$TMP_DIR" 2>/dev/null || true
}

# ── T2: fullyIdle false -> continue, binary NOT called ────────────────────────
log "T2: fullyIdle false -> continue (bg tasks live)"
{
    setup_env
    MARKER="${TMP_DIR}/called"
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<STUB
#!/usr/bin/env bash
touch "${MARKER}"
echo '{"decision":"allow","termination_reason":"DonePRGreen","message":"x"}'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":false,\"conversationId\":\"c2\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t2=true
    [[ "$(stdout_decision)" == "continue" ]] || { fail "T2: expected continue, got $HOOK_STDOUT"; t2=false; }
    [[ -f "$MARKER" ]] && { fail "T2: binary invoked despite fullyIdle false"; t2=false; }
    [[ "$t2" == true ]] && pass "T2: fullyIdle false -> continue, binary not called"
    cleanup
}

# ── T3: binary missing -> {} (allow) + event ──────────────────────────────────
log "T3: binary missing -> allow {} + binary_missing event"
{
    setup_env
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c3\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "PATH=$(safe_path)" "FNO_AGENTS_BIN=/nonexistent"
    t3=true
    [[ "$(stdout_decision)" == "<none>" ]] && printf '%s' "$HOOK_STDOUT" | jq -e . >/dev/null 2>&1 || { fail "T3: expected {} allow, got $HOOK_STDOUT"; t3=false; }
    grep -q 'loop_check_binary_missing' "${TMP_DIR}/.fno/events.jsonl" 2>/dev/null || { fail "T3: binary_missing event not emitted"; t3=false; }
    [[ "$t3" == true ]] && pass "T3: binary missing -> {} + event"
    cleanup
}

# ── T4: block -> continue, message in reason ──────────────────────────────────
log "T4: loop-check block -> continue"
{
    setup_env
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
echo '{"decision":"block","termination_reason":null,"message":"PR not green yet","fires":1,"fingerprint":"x"}'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c4\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t4=true
    [[ "$(stdout_decision)" == "continue" ]] || { fail "T4: expected continue, got $HOOK_STDOUT"; t4=false; }
    [[ "$(printf '%s' "$HOOK_STDOUT" | jq -r '.reason')" == "PR not green yet" ]] || { fail "T4: message not carried into reason: $HOOK_STDOUT"; t4=false; }
    [[ "$t4" == true ]] && pass "T4: block -> continue + reason"
    cleanup
}

# ── T5: terminal allow -> {} + finalize invoked ───────────────────────────────
log "T5: terminal allow -> {} + finalize"
{
    setup_env
    FMARK="${TMP_DIR}/finalize_called"
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<STUB
#!/usr/bin/env bash
if [[ "\$1" == "finalize" ]]; then touch "${FMARK}"; exit 0; fi
echo '{"decision":"allow","termination_reason":"DonePRGreen","message":"shipped","fires":1,"fingerprint":"y"}'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c5\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t5=true
    [[ "$(stdout_decision)" == "<none>" ]] && printf '%s' "$HOOK_STDOUT" | jq -e . >/dev/null 2>&1 || { fail "T5: expected {} allow, got $HOOK_STDOUT"; t5=false; }
    [[ -f "$FMARK" ]] || { fail "T5: finalize not invoked on terminal allow"; t5=false; }
    [[ "$t5" == true ]] && pass "T5: terminal allow -> {} + finalize"
    cleanup
}

# ── T6: garbage from a present binary -> continue (transient retry) + event ────
log "T6: loop-check garbage -> continue + event"
{
    setup_env
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
echo 'not json at all'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c6\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t6=true
    [[ "$(stdout_decision)" == "continue" ]] || { fail "T6: expected continue, got $HOOK_STDOUT"; t6=false; }
    grep -q 'loop_check_gh_error' "${TMP_DIR}/.fno/events.jsonl" 2>/dev/null || { fail "T6: gh_error event not emitted"; t6=false; }
    [[ "$t6" == true ]] && pass "T6: garbage -> continue (retry) + event"
    cleanup
}

# ── T7: Gemini-shaped transcript -> synthesized claude line reaches loop-check ─
log "T7: transcript synth feeds loop-check a claude-shaped line"
{
    setup_env
    printf '{"role":"model","parts":[{"text":"<promise>MISSION COMPLETE: x</promise>"}]}\n' > "$TRANSCRIPT_FILE"
    CAP="${TMP_DIR}/captured.jsonl"
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<STUB
#!/usr/bin/env bash
# Capture the --transcript arg's content so the test can assert on the synth.
while [[ \$# -gt 0 ]]; do
  if [[ "\$1" == "--transcript" ]]; then cp "\$2" "${CAP}"; fi
  shift
done
echo '{"decision":"allow","termination_reason":"DonePRGreen","message":"ok"}'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c7\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t7=true
    [[ -f "$CAP" ]] || { fail "T7: loop-check transcript not captured"; t7=false; }
    if [[ -f "$CAP" ]]; then
        # The synth line must be claude-shaped and carry the promise text.
        line_role="$(jq -r '.message.role' "$CAP" 2>/dev/null | head -1)"
        line_has_promise="$(grep -c 'MISSION COMPLETE' "$CAP" 2>/dev/null || echo 0)"
        [[ "$line_role" == "assistant" ]] || { fail "T7: synth role not 'assistant': $(cat "$CAP")"; t7=false; }
        [[ "$line_has_promise" -ge 1 ]] || { fail "T7: synth lost the promise text: $(cat "$CAP")"; t7=false; }
    fi
    [[ "$t7" == true ]] && pass "T7: Gemini transcript synthesized to claude shape"
    cleanup
}

# ── T8: silence rule -> stdout is exactly one JSON object, nothing else ────────
log "T8: silence rule (stdout = one JSON line only)"
{
    setup_env
    STUB="${TMP_DIR}/fno-agents"
    make_stub "$STUB" <<'STUB'
#!/usr/bin/env bash
echo '{"decision":"block","termination_reason":null,"message":"keep going","fires":1,"fingerprint":"x"}'
STUB
    INPUT="{\"transcriptPath\":\"${TRANSCRIPT_FILE}\",\"fullyIdle\":true,\"conversationId\":\"c8\"}"
    run_hook "$TMP_DIR" "$INPUT" "HOME=${HOME_DIR}" "FNO_AGENTS_BIN=${STUB}"
    t8=true
    nlines="$(printf '%s' "$HOOK_STDOUT" | grep -c . )"
    [[ "$nlines" -eq 1 ]] || { fail "T8: stdout has $nlines lines, expected 1: $HOOK_STDOUT"; t8=false; }
    printf '%s' "$HOOK_STDOUT" | jq -e . >/dev/null 2>&1 || { fail "T8: stdout not valid JSON: $HOOK_STDOUT"; t8=false; }
    [[ "$t8" == true ]] && pass "T8: stdout is exactly one JSON object"
    cleanup
}

echo ""
printf '[agy] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
[[ "$FAIL" -gt 0 ]] && exit 1
exit 0
