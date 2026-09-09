#!/usr/bin/env bash
# tests/hooks/test_stop_hook_pre_manifest_distress.sh
#
# A worker that dies before `target init` writes a manifest has no state
# file, so the shim takes the pre-manifest visitor-allowed exit and never
# calls loop-check. Before this test's change that exit read the <help> tag
# and dropped it on the floor; now it calls the `distress-scan` verb.
#
# Tests (against the REAL shim + REAL fno-agents binary):
#   T1 (AC8-HP)   a real codex help-tag transcript -> exit 0 + one blocked row
#                 carrying the evidence string and harness: codex.
#   T2 (AC9-EDGE) empty transcript_path -> exit 0, verb not called, no row.
#   T3 (AC11-EDGE) same wiring, a help-tag-free transcript -> no NEW row,
#                 proven against T1's already-written row (positive control
#                 in the same run, not an absence asserted alone).
#
# Modelled after tests/hooks/test_loop_check_e2e.sh conventions: tmpdir per
# case, isolated HOME + FNO_SPACES_DIR, the real debug binary, skip (77) when
# it is not built.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"
FIXTURE="${REPO_ROOT}/tests/fixtures/rollout-codex-help.jsonl"

PASS=0; FAIL=0; SKIP_COUNT=0
log()  { printf '[distress] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[distress] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[distress] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[distress] SKIP: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]]    || { fail "hook not found: $HOOK"; exit 1; }
[[ -f "$FIXTURE" ]] || { fail "fixture not found: $FIXTURE"; exit 1; }
command -v jq   >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }
command -v bash >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }
command -v git  >/dev/null 2>&1 || { skip "git not on PATH"; exit 77; }
command -v python3 >/dev/null 2>&1 || { skip "python3 not on PATH"; exit 77; }

REAL_BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$REAL_BIN" ]]; then
    skip "fno-agents debug binary not found at $REAL_BIN; run: cd crates/fno-agents && cargo build"
    exit 77
fi

# ── helper: a fake `fno agents newest-assistant-text --transcript <path>` ──
# The reader that speaks the real codex shape is Python (peek.py); this
# stub prints the same answer for a fixture on disk without needing a full
# `fno` install, the same shortcut distress.rs's own unit tests take.
make_reader_stub() {
    local path="$1"
    cat > "$path" <<'STUB'
#!/bin/sh
[ "$1" = agents ] && [ "$2" = newest-assistant-text ] && [ "$3" = --transcript ] || exit 42
python3 -c '
import json, sys
with open(sys.argv[1]) as fh:
    rec = json.loads(fh.readline())
print(rec["payload"]["content"][0]["text"], end="")
' "$4"
STUB
    chmod +x "$path"
}

init_git_repo() {
    local dir="$1"
    git -C "$dir" init -q
    git -C "$dir" config user.email "test@test.com"
    git -C "$dir" config user.name "Test"
    git -C "$dir" commit -q --allow-empty -m "init" 2>/dev/null || true
}

run_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift
    HOOK_RC=0
    HOOK_STDERR=""
    HOOK_STDERR=$(
        cd "$cwd" || exit 1
        # Scrub the ambient harness markers before adding the test's own: this
        # script itself may run inside a live Claude Code session (CI, or a
        # dev driving it from an agent), and env VAR=val only ADDS to the
        # inherited environment - a leaked CLAUDE_CODE_SESSION_ID silently
        # stamped harness:claude on a codex fixture and masked a real T1 gap.
        env -u CLAUDE_CODE_SESSION_ID -u CODEX_THREAD_ID -u GEMINI_SESSION_ID \
            CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= "$@" bash "$HOOK" <<< "$input_json" 2>&1 >/dev/null
    ) || HOOK_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# T1 (AC8-HP): a real codex help-tag transcript -> exit 0 + one blocked row
# ─────────────────────────────────────────────────────────────────────────────
log "T1: pre-manifest stop with a real help-tag transcript"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    SPACES_DIR="${TMP_DIR}/spaces"
    mkdir -p "$HOME_DIR/.fno" "$SPACES_DIR"
    init_git_repo "$TMP_DIR"
    READER_STUB="${TMP_DIR}/fno-reader-stub"
    make_reader_stub "$READER_STUB"

    INPUT_JSON=$(jq -cn --arg t "$FIXTURE" --arg s "t1-session" \
        '{transcript_path:$t, session_id:$s}')
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_SPACES_DIR=${SPACES_DIR}" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_FNO_BIN=${READER_STUB}" \
        "CODEX_THREAD_ID=t1-codex-thread"

    t1_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T1: expected exit 0, got $HOOK_RC (stderr: $HOOK_STDERR)"
        t1_ok=false
    fi
    # `state path events` re-derives the space root independently of the hook's
    # own resolution (both shell the real canonical_repo_root git probe, but as
    # two separate processes); trust the file the hook actually wrote instead of
    # asserting a second, separate re-derivation lands on the identical path.
    EVENTS_FILE="$(find "$SPACES_DIR" "$HOME_DIR" -type f -name events.jsonl 2>/dev/null | head -1)"
    if [[ -z "$EVENTS_FILE" ]]; then
        fail "T1: no events file under $SPACES_DIR or $HOME_DIR (stderr: $HOOK_STDERR)"
        t1_ok=false
    elif ! grep -q '"type":"blocked"' "$EVENTS_FILE" 2>/dev/null; then
        fail "T1: no blocked row in $EVENTS_FILE"
        t1_ok=false
    else
        ROW=$(grep '"type":"blocked"' "$EVENTS_FILE" | head -1)
        if ! echo "$ROW" | grep -q 'Operation not permitted'; then
            fail "T1: blocked row missing the evidence string: $ROW"
            t1_ok=false
        fi
        if ! echo "$ROW" | jq -e '.harness == "codex"' >/dev/null 2>&1; then
            fail "T1: blocked row harness is not codex: $ROW"
            t1_ok=false
        fi
    fi
    [[ "$t1_ok" == "true" ]] && pass "T1: visitor allowed + blocked row with evidence + harness codex"
    T1_EVENTS_FILE="$EVENTS_FILE"
    T1_TMP_DIR="$TMP_DIR"
    T1_SPACES_DIR="$SPACES_DIR"
    T1_HOME_DIR="$HOME_DIR"
}

# ─────────────────────────────────────────────────────────────────────────────
# T2 (AC9-EDGE): empty transcript_path -> exit 0, no row written
# ─────────────────────────────────────────────────────────────────────────────
log "T2: pre-manifest stop with an empty transcript_path"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    SPACES_DIR="${TMP_DIR}/spaces"
    mkdir -p "$HOME_DIR/.fno" "$SPACES_DIR"
    init_git_repo "$TMP_DIR"

    INPUT_JSON=$(jq -cn --arg s "t2-session" '{transcript_path:"", session_id:$s}')
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_SPACES_DIR=${SPACES_DIR}" \
        "FNO_AGENTS_BIN=${REAL_BIN}"

    t2_ok=true
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "T2: expected exit 0, got $HOOK_RC (stderr: $HOOK_STDERR)"
        t2_ok=false
    fi
    EVENTS_FILE="$(find "$SPACES_DIR" "$HOME_DIR" -type f -name events.jsonl 2>/dev/null | head -1)"
    if [[ -n "$EVENTS_FILE" ]] && grep -q '"type":"blocked"' "$EVENTS_FILE" 2>/dev/null; then
        fail "T2: a blocked row was written despite an empty transcript_path"
        t2_ok=false
    fi
    [[ "$t2_ok" == "true" ]] && pass "T2: empty transcript_path -> visitor allowed, nothing written"
    rm -rf "$TMP_DIR" 2>/dev/null || true
}

# ─────────────────────────────────────────────────────────────────────────────
# T3 (AC11-EDGE): a help-tag-free transcript writes no NEW row, alongside the
# working positive control from T1 in the same run.
# ─────────────────────────────────────────────────────────────────────────────
log "T3: pre-manifest stop with a help-tag-free transcript (T1 is the control)"
{
    if [[ ! -f "${T1_EVENTS_FILE:-}" ]]; then
        skip "T3: T1 did not produce an events file to diff against"
    else
        BEFORE_COUNT=$(grep -c '"type":"blocked"' "$T1_EVENTS_FILE" 2>/dev/null || echo 0)
        NO_HELP_FIXTURE="${T1_TMP_DIR}/no-help.jsonl"
        python3 -c '
import json
with open("'"$FIXTURE"'") as fh:
    rec = json.loads(fh.readline())
rec["payload"]["content"][0]["text"] = "all clear, nothing stuck here"
print(json.dumps(rec))
' > "$NO_HELP_FIXTURE"
        READER_STUB2="${T1_TMP_DIR}/fno-reader-stub-2"
        make_reader_stub "$READER_STUB2"

        INPUT_JSON=$(jq -cn --arg t "$NO_HELP_FIXTURE" --arg s "t3-session" \
            '{transcript_path:$t, session_id:$s}')
        run_hook "$T1_TMP_DIR" "$INPUT_JSON" \
            "HOME=${T1_HOME_DIR}" \
            "FNO_SPACES_DIR=${T1_SPACES_DIR}" \
            "FNO_AGENTS_BIN=${REAL_BIN}" \
            "FNO_LOOPCHECK_FNO_BIN=${READER_STUB2}" \
            "CODEX_THREAD_ID=t3-codex-thread"

        t3_ok=true
        if [[ "$HOOK_RC" -ne 0 ]]; then
            fail "T3: expected exit 0, got $HOOK_RC (stderr: $HOOK_STDERR)"
            t3_ok=false
        fi
        AFTER_COUNT=$(grep -c '"type":"blocked"' "$T1_EVENTS_FILE" 2>/dev/null || echo 0)
        if [[ "$AFTER_COUNT" -ne "$BEFORE_COUNT" ]]; then
            fail "T3: blocked row count changed ($BEFORE_COUNT -> $AFTER_COUNT) with no help tag present"
            t3_ok=false
        fi
        if [[ "$BEFORE_COUNT" -lt 1 ]]; then
            fail "T3: control invariant broken - T1 left no blocked row to diff against"
            t3_ok=false
        fi
        [[ "$t3_ok" == "true" ]] && pass "T3: no help tag -> no new row, T1's row still the only one"
    fi
    rm -rf "${T1_TMP_DIR:-/nonexistent}" 2>/dev/null || true
}

log "── summary: $PASS passed, $FAIL failed, $SKIP_COUNT skipped ──"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
