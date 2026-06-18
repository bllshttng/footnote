#!/usr/bin/env bash
# Tests that the stop-hook shim (hooks/target-stop-hook.sh) invokes
# `fno-agents finalize` on a TERMINAL-ALLOW loop-check decision, and only then
# (control-plane step 6, ab-f8e5f214).
#
# Strategy: stub the fno-agents binary via FNO_AGENTS_BIN. The stub branches on
# its first arg: `loop-check` prints a configured decision JSON; `finalize`
# records its invocation (and --reason) to a marker file and exits with a
# configured code. We then assert whether finalize was called and with what
# reason, and that a finalize failure never changes the shim's exit code.
#
# Tests:
#   T1  terminal-allow (DonePRGreen) -> finalize called with --reason DonePRGreen, shim exit 0
#   T2  block decision               -> finalize NOT called, shim exit 2
#   T3  allow + null reason          -> finalize NOT called (not terminal), shim exit 0
#   T4  finalize fails (exit 1)      -> shim still exits 0 (side-effects never block)
#   T5  finalize gets --state/--cwd/--transcript forwarded

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"

PASS=0; FAIL=0
log()  { printf '[finalize-invoke] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[finalize-invoke] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[finalize-invoke] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v jq >/dev/null 2>&1 || { printf '[finalize-invoke] SKIP: jq not on PATH\n' >&2; exit 77; }

# Build a temp project + a stub fno-agents that branches on verb.
# Args: <decision_json> <finalize_rc>
# Sets: TMP_DIR HOME_DIR STATE_FILE TRANSCRIPT_FILE STUB MARKER
setup() {
    local decision="$1" finalize_rc="${2:-0}"
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"
    TRANSCRIPT_FILE="${TMP_DIR}/uuid-1234.jsonl"
    printf '{"role":"assistant","content":"hi"}\n' > "$TRANSCRIPT_FILE"
    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: fin-sess-1
created_at: 2026-06-07T00:00:00Z
claude_transcript_id: uuid-1234
attended: true
---
STATE
    MARKER="${TMP_DIR}/finalize_called"
    STUB="${TMP_DIR}/fno-agents"
    cat > "$STUB" <<STUBEOF
#!/usr/bin/env bash
verb="\$1"; shift
if [[ "\$verb" == "loop-check" ]]; then
    cat <<'DECISION'
${decision}
DECISION
    exit 0
elif [[ "\$verb" == "finalize" ]]; then
    printf 'finalize %s\n' "\$*" >> "${MARKER}"
    exit ${finalize_rc}
fi
exit 0
STUBEOF
    chmod +x "$STUB"
}
cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" 2>/dev/null || true; }

run_hook() {
    HOOK_RC=0
    HOOK_STDERR=$(
        cd "$TMP_DIR" || exit 1
        printf '{"transcript_path":"%s"}' "$TRANSCRIPT_FILE" \
            | env HOME="$HOME_DIR" FNO_AGENTS_BIN="$STUB" bash "$HOOK" 2>&1 >/dev/null
    ) || HOOK_RC=$?
}

# ── T1: terminal-allow -> finalize called with --reason DonePRGreen ──────────
log "T1: terminal-allow (DonePRGreen) -> finalize invoked"
{
    setup '{"decision":"allow","termination_reason":"DonePRGreen","message":"PR green","fires":1,"fingerprint":"a"}'
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "T1: expected exit 0, got $HOOK_RC"; ok=false; }
    [[ -f "$MARKER" ]] || { fail "T1: finalize was NOT invoked"; ok=false; }
    grep -q -- "--reason DonePRGreen" "$MARKER" 2>/dev/null || { fail "T1: --reason DonePRGreen not forwarded: $(cat "$MARKER" 2>/dev/null)"; ok=false; }
    $ok && pass "T1: finalize invoked with --reason DonePRGreen, shim exit 0"
    cleanup
}

# ── T2: block -> finalize NOT called ─────────────────────────────────────────
log "T2: block decision -> finalize NOT invoked"
{
    setup '{"decision":"block","termination_reason":null,"message":"keep going","fires":1,"fingerprint":"b"}'
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 2 ]] || { fail "T2: expected exit 2, got $HOOK_RC"; ok=false; }
    [[ -f "$MARKER" ]] && { fail "T2: finalize was invoked on a block decision"; ok=false; }
    $ok && pass "T2: block -> finalize not invoked, shim exit 2"
    cleanup
}

# ── T3: allow + null reason -> finalize NOT called (not terminal) ─────────────
log "T3: allow + null termination_reason -> finalize NOT invoked"
{
    setup '{"decision":"allow","termination_reason":null,"message":"legacy allow","fires":1,"fingerprint":"c"}'
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "T3: expected exit 0, got $HOOK_RC"; ok=false; }
    [[ -f "$MARKER" ]] && { fail "T3: finalize invoked despite null termination_reason"; ok=false; }
    $ok && pass "T3: allow+null reason -> finalize not invoked, shim exit 0"
    cleanup
}

# ── T4: finalize fails -> shim still exits 0 (non-blocking) ───────────────────
log "T4: finalize failure does not change shim exit"
{
    setup '{"decision":"allow","termination_reason":"Budget","message":"budget hit","fires":3,"fingerprint":"d"}' 1
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "T4: finalize failure changed shim exit to $HOOK_RC (must stay 0)"; ok=false; }
    [[ -f "$MARKER" ]] || { fail "T4: finalize was not invoked"; ok=false; }
    $ok && pass "T4: finalize exit 1 -> shim still exit 0 (side-effects never block)"
    cleanup
}

# ── T5: finalize gets --state/--cwd/--transcript forwarded ───────────────────
log "T5: finalize receives forwarded args"
{
    setup '{"decision":"allow","termination_reason":"DoneAdvisory","message":"adv","fires":1,"fingerprint":"e"}'
    run_hook
    ok=true
    args="$(cat "$MARKER" 2>/dev/null)"
    grep -q -- "--state" "$MARKER" 2>/dev/null || { fail "T5: --state not forwarded: $args"; ok=false; }
    grep -q -- "--cwd" "$MARKER" 2>/dev/null || { fail "T5: --cwd not forwarded: $args"; ok=false; }
    grep -q -- "--transcript" "$MARKER" 2>/dev/null || { fail "T5: --transcript not forwarded: $args"; ok=false; }
    $ok && pass "T5: finalize receives --state/--cwd/--transcript"
    cleanup
}

printf '[finalize-invoke] Results: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -gt 0 ]] && exit 1
exit 0
