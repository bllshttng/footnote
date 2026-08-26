#!/usr/bin/env bash
# tests/hooks/test_king_stop_hook.sh
#
# Both reachable stop paths, driven over scope-keyed king state, in one file.
#
# A guard placed on one of N reachable paths is decorative: it reads as
# protection and ships green while the others stay broken. The king loop ships
# claude-only, so the two paths must not disagree about what a king manifest
# means. The claude shim gates it. The agy adapter cannot, and must say so
# rather than allowing the stop as if nothing were running.
#
# Tests:
#   K1  claude shim, no manifest at all      -> resolver miss, allow
#   K2  claude shim, king manifest, block     -> exit 2, --driver king forwarded
#   K3  claude shim, king manifest, NoWork    -> exit 0, finalize NOT invoked
#   K4  claude shim, both manifests present   -> target wins, --driver target
#   K5  agy adapter, king manifest            -> refuses by name, never allows
#   K7  claude shim, manifest names ANOTHER session -> exit 0, never gates
#   K8  agy adapter, repeated king refusals         -> bounded, then allows
#   K6  agy adapter, no manifest              -> allow (the refusal is scoped)
#   K9  conflicting ambient markers omit harness narrowing
#   K10 a claude transcript beats a lone foreign codex marker

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

# Write a king manifest that names `$1` as the session it crowned.
king_manifest_naming() {
    mkdir -p "${TMP_DIR}/.fno/kings"
    cat > "${TMP_DIR}/.fno/kings/x-f3d0.md" <<MANIFEST
---
fno_id: king-test-001
created_at: 2026-08-18T00:00:00Z
scope: x-f3d0
harness: claude
harness_session_id: $1
budget_max_iterations: 40
---
MANIFEST
    printf '%s\n' "$1" > "${TMP_DIR}/.fno/live-crown-session"
}

# A tmp project with a king manifest and a transcript. Sets TMP_DIR, HOME_DIR,
# TRANSCRIPT, ARGS_LOG, BIN.
setup_king() {
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "${TMP_DIR}/bin"
    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"assistant","content":"working"}}\n' > "$TRANSCRIPT"
    # `harness_session_id` is the transcript basename, which is what the hook
    # derives its own id from. A real `fno agents king init` writes it and now refuses
    # without it, so a manifest lacking one is a state the system cannot reach.
    king_manifest_naming "transcript"
    ARGS_LOG="${TMP_DIR}/fno-agents.args"
    BIN="${TMP_DIR}/bin/fno-agents"
    cat > "${TMP_DIR}/bin/fno" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" > .fno/fno.args
if [[ "$1 $2" != "king manifest-path" ]]; then
    exit 1
fi
session=""
while [[ $# -gt 0 ]]; do
    if [[ "$1" == "--harness-session-id" ]]; then
        session="${2:-}"
        break
    fi
    shift
done
expected=$(cat .fno/live-crown-session 2>/dev/null || true)
if [[ -n "$session" && "$session" == "$expected" && -f .fno/kings/x-f3d0.md ]]; then
    printf '%s\n' "$PWD/.fno/kings/x-f3d0.md"
    exit 0
fi
exit 1
STUB
    chmod +x "${TMP_DIR}/bin/fno"
}

# A stub fno-agents that records its argv and prints $1 for loop-check.
stub_binary() {
    local decision="$1"
    cat > "$BIN" <<STUB
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${ARGS_LOG}"
if [[ "\$1" == "manifest-for-session" ]]; then
    if [[ -f .fno/target-state.md ]]; then
        printf '%s\n' "\$PWD/.fno/target-state.md"
        exit 0
    fi
    exit 1
fi
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
        env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" FNO_HARNESS=claude \
            FNO_AGENTS_BIN="$BIN" \
            bash "$CLAUDE_HOOK" <<< "$input" >/dev/null 2>"$CLAUDE_ERR"
    ) || CLAUDE_RC=$?
}

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" 2>/dev/null || true; }

# ── K9: conflicting markers keep the session-id lookup harness-neutral ───────
{
    setup_king
    stub_binary '{"decision":"block","message":"scoped board has work"}'
    CLAUDE_RC=0
    (
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" FNO_HARNESS="" \
            CODEX_THREAD_ID="foreign-codex" CLAUDE_CODE_SESSION_ID="transcript" \
            FNO_AGENTS_BIN="$BIN" bash "$CLAUDE_HOOK" \
            <<< "{\"transcript_path\":\"${TRANSCRIPT}\"}" >/dev/null 2>/dev/null
    ) || CLAUDE_RC=$?
    FNO_ARGS="$(cat "${TMP_DIR}/.fno/fno.args" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 2 ]] && ! tr ' ' '\n' <<< "$FNO_ARGS" | grep -qx -- "--harness"; then
        pass "K9: conflicting markers omit harness narrowing"
    else
        fail "K9: rc=$CLAUDE_RC fno_args=$FNO_ARGS"
    fi
    cleanup
}

# ── K10: a claude-located transcript beats a lone foreign codex marker ────────
{
    setup_king
    stub_binary '{"decision":"block","message":"scoped board has work"}'
    mkdir -p "${HOME_DIR}/.claude/projects/proj"
    CLAUDE_TRANSCRIPT="${HOME_DIR}/.claude/projects/proj/transcript.jsonl"
    printf '{"message":{"role":"assistant","content":"working"}}\n' > "$CLAUDE_TRANSCRIPT"
    CLAUDE_RC=0
    (
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" FNO_HARNESS="" \
            CODEX_THREAD_ID="foreign-codex" \
            FNO_AGENTS_BIN="$BIN" bash "$CLAUDE_HOOK" \
            <<< "{\"transcript_path\":\"${CLAUDE_TRANSCRIPT}\"}" >/dev/null 2>/dev/null
    ) || CLAUDE_RC=$?
    FNO_ARGS="$(cat "${TMP_DIR}/.fno/fno.args" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 2 ]] \
        && tr ' ' '\n' <<< "$FNO_ARGS" | grep -qx -- "--harness" \
        && tr ' ' '\n' <<< "$FNO_ARGS" | grep -qx -- "claude"; then
        pass "K10: claude transcript wins over a lone foreign codex marker"
    else
        fail "K10: rc=$CLAUDE_RC fno_args=$FNO_ARGS"
    fi
    cleanup
}

# ── K1: no manifest at all -> resolver miss, then allow ───────────────────────
{
    setup_king
    rm -f "${TMP_DIR}/.fno/kings/x-f3d0.md" "${TMP_DIR}/.fno/live-crown-session"
    stub_binary '{"decision":"block","message":"should never run"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    ARGS="$(cat "$ARGS_LOG" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 0 ]] \
        && grep -q '^manifest-for-session' <<< "$ARGS" \
        && ! grep -q '^loop-check' <<< "$ARGS"; then
        pass "K1: no target manifest resolves as a visitor"
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
        && grep -q -- ".fno/kings/x-f3d0.md" <<< "$ARGS"; then
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
    if grep -q -- "--driver target" <<< "$ARGS" && ! grep -q ".fno/kings/" <<< "$ARGS"; then
        pass "K4: a target manifest outranks a king manifest beside it"
    else
        fail "K4: args=$ARGS"
    fi
    cleanup
}

# ── K5: the agy adapter refuses by name rather than allowing ─────────────────
{
    setup_king
    king_manifest_naming "c1"
    AGY_TRANSCRIPT="${TMP_DIR}/agy.jsonl"
    printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$AGY_TRANSCRIPT"
    INPUT="{\"transcriptPath\":\"${AGY_TRANSCRIPT}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    AGY_ERR="${TMP_DIR}/agy.err"
    AGY_OUT=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" FNO_AGENTS_BIN="$BIN" \
            bash "$AGY_HOOK" <<< "$INPUT" 2>"$AGY_ERR"
    )
    DECISION="$(printf '%s' "$AGY_OUT" | jq -r '.decision // "<none>"' 2>/dev/null)"
    if [[ "$DECISION" == "continue" ]] && grep -q ".fno/kings/x-f3d0.md" "$AGY_ERR"; then
        pass "K5: agy names the king manifest and refuses to allow the stop"
    else
        fail "K5: decision=$DECISION stderr=$(cat "$AGY_ERR")"
    fi
    cleanup
}

# ── K6: the agy refusal is scoped to a king manifest ─────────────────────────
{
    setup_king
    rm -f "${TMP_DIR}/.fno/kings/x-f3d0.md" "${TMP_DIR}/.fno/live-crown-session"
    AGY_TRANSCRIPT="${TMP_DIR}/agy.jsonl"
    printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$AGY_TRANSCRIPT"
    INPUT="{\"transcriptPath\":\"${AGY_TRANSCRIPT}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    AGY_OUT=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" bash "$AGY_HOOK" <<< "$INPUT" 2>/dev/null
    )
    if [[ "$(printf '%s' "$AGY_OUT" | jq -r '.decision // "<none>"')" == "<none>" ]]; then
        pass "K6: with no king manifest agy allows as before"
    else
        fail "K6: unexpected decision: $AGY_OUT"
    fi
    cleanup
}


# ── K7: a manifest naming ANOTHER session must not gate this one ─────────────
{
    setup_king
    # A king crowned in some other session, whose manifest nobody deleted when
    # it died. Kings run in the canonical checkout, which is where every
    # ordinary session runs too, so this file sits beside unrelated work
    # forever. Gating on its PRESENCE held those sessions open until the board
    # went clean, for people who never crowned anything.
    king_manifest_naming "some-other-session"
    stub_binary '{"decision":"block","message":"should never run"}'
    run_claude_hook "{\"transcript_path\":\"${TRANSCRIPT}\"}"
    ARGS="$(cat "$ARGS_LOG" 2>/dev/null)"
    if [[ "$CLAUDE_RC" -eq 0 ]] \
        && grep -q '^manifest-for-session' <<< "$ARGS" \
        && ! grep -q '^loop-check' <<< "$ARGS"; then
        pass "K7: a stale king manifest naming another session never gates this one"
    else
        fail "K7: rc=$CLAUDE_RC args=$(cat "$ARGS_LOG" 2>/dev/null)"
    fi
    cleanup
}


# ── K8: the agy refusal gives up rather than holding a session forever ───────
{
    setup_king
    king_manifest_naming "c1"
    AGY_TRANSCRIPT="${TMP_DIR}/agy.jsonl"
    printf '{"role":"model","parts":[{"text":"x"}]}\n' > "$AGY_TRANSCRIPT"
    INPUT="{\"transcriptPath\":\"${AGY_TRANSCRIPT}\",\"fullyIdle\":true,\"conversationId\":\"c1\"}"
    # Cleanup is best-effort, so a live crown's refusal is still bounded. Fire
    # past the ceiling and require a real allow, mirroring how the
    # checker-unavailable path already gives up.
    LAST_DECISION=""
    for _ in 1 2 3 4 5; do
        LAST_DECISION=$(
            cd "$TMP_DIR" || exit 1
            env HOME="$HOME_DIR" PATH="${TMP_DIR}/bin:${PATH}" FNO_AGENTS_BIN="$BIN" \
                bash "$AGY_HOOK" <<< "$INPUT" 2>/dev/null \
                | jq -r '.decision // "<none>"' 2>/dev/null
        )
    done
    if [[ "$LAST_DECISION" != "continue" ]]; then
        pass "K8: the agy king refusal is bounded and eventually allows the stop"
    else
        fail "K8: still continuing past the ceiling (decision=$LAST_DECISION)"
    fi
    cleanup
}

printf '[king-hook] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
