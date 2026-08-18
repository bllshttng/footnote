#!/usr/bin/env bash
# tests/hooks/test_king_stop_hook.sh
#
# Both reachable stop paths, driven over ONE king manifest, in one file.
#
# A guard placed on one of N reachable paths is decorative: it reads as
# protection and ships green while the others stay broken. The king loop ships
# claude-only, so the two paths must not disagree about what a king manifest
# means. The claude shim gates it. The agy adapter cannot, and must say so
# rather than allowing the stop as if nothing were running.
#
# Tests:
#   K1  claude shim, no manifest at all      -> allow, binary never called
#   K2  claude shim, king manifest, block     -> exit 2, --driver king forwarded
#   K3  claude shim, king manifest, NoWork    -> exit 0, finalize NOT invoked
#   K4  claude shim, both manifests present   -> target wins, --driver target
#   K5  agy adapter, king manifest            -> refuses by name, never allows
#   K6  agy adapter, no manifest              -> allow (the refusal is scoped)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
CLAUDE_HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"
AGY_HOOK="${REPO_ROOT}/hooks/agy-target-stop-hook.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[king-hook] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[king-hook] FAIL: %s\n' "$*" >&2; }

[[ -f "$CLAUDE_HOOK" ]] || { fail "missing $CLAUDE_HOOK"; exit 1; }
[[ -f "$AGY_HOOK" ]]    || { fail "missing $AGY_HOOK"; exit 1; }
command -v jq >/dev/null 2>&1 || { printf '[king-hook] SKIP: jq not on PATH\n' >&2; exit 77; }

# A tmp project with a king manifest and a transcript. Sets TMP_DIR, HOME_DIR,
# TRANSCRIPT, ARGS_LOG, BIN.
setup_king() {
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "${TMP_DIR}/bin"
    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"assistant","content":"working"}}\n' > "$TRANSCRIPT"
    cat > "${TMP_DIR}/.fno/king-state.md" <<'MANIFEST'
---
fno_id: king-test-001
created_at: 2026-08-18T00:00:00Z
scope: board drain
harness: claude
budget_max_iterations: 40
---
MANIFEST
    ARGS_LOG="${TMP_DIR}/fno-agents.args"
    BIN="${TMP_DIR}/bin/fno-agents"
}

# A stub fno-agents that records its argv and prints $1 for loop-check.
stub_binary() {
    local decision="$1"
    cat > "$BIN" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${ARGS_LOG}"
if [[ "\$1" == "loop-check" ]]; then
    cat <<'JSON'
${decision}
JSON
    exit 0
fi
exit 0
STUB
    chmod +x "$BIN"
}

run_claude_hook() {
    local input="$1"
    CLAUDE_RC=0
    CLAUDE_ERR="${TMP_DIR}/claude.err"
    (
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" FNO_AGENTS_BIN="$BIN" \
            bash "$CLAUDE_HOOK" <<< "$input" >/dev/null 2>"$CLAUDE_ERR"
    ) || CLAUDE_RC=$?
}

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" 2>/dev/null || true; }

# ── K1: no manifest at all -> silent allow, binary never called ───────────────
{
    setup_king
    rm -f "${TMP_DIR}/.fno/king-state.md"
    stub_binary '{"decision":"block","message":"should never run"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    if [[ "$CLAUDE_RC" -eq 0 && ! -f "$ARGS_LOG" ]]; then
        pass "K1: no manifest is the only safe silent allow"
    else
        fail "K1: rc=$CLAUDE_RC args=$(cat "$ARGS_LOG" 2>/dev/null)"
    fi
    cleanup
}

# ── K2: king manifest + a block decision -> exit 2, --driver king forwarded ───
{
    setup_king
    stub_binary '{"decision":"block","message":"2 actionable; next: undispatched: x-1234"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    ARGS="$(cat "$ARGS_LOG" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 2 ]] \
        && grep -q -- "--driver king" <<< "$ARGS" \
        && grep -q -- "king-state.md" <<< "$ARGS"; then
        pass "K2: a king manifest blocks and routes to the king driver"
    else
        fail "K2: rc=$CLAUDE_RC (want 2) args=$ARGS"
    fi
    cleanup
}

# ── K3: NoWork terminal -> allow, and finalize is NOT invoked ─────────────────
# finalize stamps a plan and graduates a node. A king has neither, so running it
# over a king manifest would read fields that are not there.
{
    setup_king
    stub_binary '{"decision":"allow","termination_reason":"NoWork","message":"board clean"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    ARGS="$(cat "$ARGS_LOG" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 0 ]] && ! grep -q '^finalize' <<< "$ARGS"; then
        pass "K3: a king NoWork terminal allows without invoking finalize"
    else
        fail "K3: rc=$CLAUDE_RC args=$ARGS"
    fi
    cleanup
}

# ── K4: both manifests -> the target manifest wins ───────────────────────────
# A session holding a target manifest is a worker whatever else is on disk.
{
    setup_king
    cat > "${TMP_DIR}/.fno/target-state.md" <<'MANIFEST'
---
session_id: target-test-001
created_at: 2026-08-18T00:00:00Z
---
MANIFEST
    stub_binary '{"decision":"allow","message":"nothing to do"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    ARGS="$(cat "$ARGS_LOG" 2>/dev/null)"
    if grep -q -- "--driver target" <<< "$ARGS" && ! grep -q "king-state.md" <<< "$ARGS"; then
        pass "K4: a target manifest outranks a king manifest beside it"
    else
        fail "K4: args=$ARGS"
    fi
    cleanup
}

# ── K5: the agy adapter refuses by name rather than allowing ─────────────────
{
    setup_king
    AGY_TRANSCRIPT="${TMP_DIR}/agy.jsonl"
    printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$AGY_TRANSCRIPT"
    INPUT="{\"transcriptPath\":\"${AGY_TRANSCRIPT}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    AGY_ERR="${TMP_DIR}/agy.err"
    AGY_OUT=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" FNO_AGENTS_BIN="$BIN" \
            bash "$AGY_HOOK" <<< "$INPUT" 2>"$AGY_ERR"
    )
    DECISION="$(printf '%s' "$AGY_OUT" | jq -r '.decision // "<none>"' 2>/dev/null)"
    if [[ "$DECISION" == "continue" ]] && grep -q "king-state.md" "$AGY_ERR"; then
        pass "K5: agy names the king manifest and refuses to allow the stop"
    else
        fail "K5: decision=$DECISION stderr=$(cat "$AGY_ERR")"
    fi
    cleanup
}

# ── K6: the agy refusal is scoped to a king manifest ─────────────────────────
{
    setup_king
    rm -f "${TMP_DIR}/.fno/king-state.md"
    AGY_TRANSCRIPT="${TMP_DIR}/agy.jsonl"
    printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$AGY_TRANSCRIPT"
    INPUT="{\"transcriptPath\":\"${AGY_TRANSCRIPT}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    AGY_OUT=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" bash "$AGY_HOOK" <<< "$INPUT" 2>/dev/null
    )
    if [[ "$(printf '%s' "$AGY_OUT" | jq -r '.decision // "<none>"')" == "<none>" ]]; then
        pass "K6: with no king manifest agy allows as before"
    else
        fail "K6: unexpected decision: $AGY_OUT"
    fi
    cleanup
}

printf '[king-hook] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
