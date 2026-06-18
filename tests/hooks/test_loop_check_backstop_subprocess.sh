#!/usr/bin/env bash
# tests/hooks/test_loop_check_backstop_subprocess.sh
#
# Journey test for cross-process fingerprint backstop accumulation (Gap 4).
#
# The fingerprint history is persisted via the cwd-derived default events path
# (.fno/events.jsonl) across completely separate process invocations.
# This proves that three separate subprocess fires accumulate the backstop
# counter through shared on-disk state, not in-memory state.
#
# Setup:
#   - Unattended manifest (attended: false) -> N=3 backstop threshold
#   - no-PR gh stub -> pr_state=none for all fires (identical fingerprint)
#   - fixed-sha git stub -> git_sha component stable
#
# Protocol (all three pass ONLY --state/--transcript/--cwd; events path derives
# from cwd, mirroring the real shim which also passes no --events flag):
#   Fire 1: run binary as subprocess -> assert decision=block, 1 loop_check line
#   Fire 2: run binary as subprocess -> assert decision=block, 2 loop_check lines
#   Fire 3: run binary as subprocess -> assert decision=allow,
#            termination_reason=NoProgress, fires=3 in JSON output
#
# Modelled after tests/hooks/test_loop_check_shim.sh conventions.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# ── counters ─────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[backstop] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[backstop] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[backstop] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[backstop] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ────────────────────────────────────────────────────────────────
command -v jq   >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }
command -v bash >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }

REAL_BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$REAL_BIN" ]]; then
    skip "fno-agents debug binary not found at $REAL_BIN; run: cd crates/fno-agents && cargo build"
    exit 77
fi

# ── helpers ───────────────────────────────────────────────────────────────────
make_stub() {
    local path="$1"; shift
    cat > "$path"
    chmod +x "$path"
}

# Emits gh's real no-PR stderr: step 2 classifies a bare exit-1 as an OUTAGE
# (streak frozen, no NoProgress), while this message marks healthy no-PR
# world-state so the backstop keeps ticking.
make_no_pr_gh() {
    local path="$1"
    make_stub "$path" <<'STUB'
#!/bin/sh
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
echo 'no pull requests found for branch "feat"' >&2
exit 1
STUB
}

make_git_stub() {
    local path="$1" sha="${2:-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1}"
    make_stub "$path" <<STUB
#!/bin/sh
echo "${sha}"
STUB
}

count_lines_matching() {
    local pattern="$1" file="$2"
    grep -c "$pattern" "$file" 2>/dev/null || echo 0
}

# ── test setup ────────────────────────────────────────────────────────────────
TMP_DIR="$(mktemp -d)"
HOME_DIR="${TMP_DIR}/home"
STUB_BIN="${TMP_DIR}/stubs"
EVENTS_FILE="${TMP_DIR}/.fno/events.jsonl"

mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

# Isolated settings so the binary never reads the real $HOME/.fno/settings.yaml
printf '# isolated\n' > "${TMP_DIR}/.fno/settings.yaml"

MANIFEST="${TMP_DIR}/state.md"
TRANSCRIPT="${TMP_DIR}/transcript.jsonl"

# Unattended manifest: attended: false -> N=3 threshold
cat > "$MANIFEST" <<'MANIFEST'
---
session_id: backstop-sess-001
created_at: 2026-06-05T00:00:00Z
attended: false
---
MANIFEST

# Transcript: user message only (no promise)
printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

make_no_pr_gh "${STUB_BIN}/gh"
make_git_stub "${STUB_BIN}/git"

# ── fire helper ───────────────────────────────────────────────────────────────
# Runs the binary as a SEPARATE subprocess passing only --state/--transcript/--cwd.
# The events path is derived from cwd (default: <cwd>/.fno/events.jsonl).
# Returns the JSON output in FIRE_OUTPUT; sets FIRE_RC.
fire_subprocess() {
    FIRE_OUTPUT=""
    FIRE_RC=0
    FIRE_OUTPUT=$(
        HOME="${HOME_DIR}" \
        FNO_LOOPCHECK_GH_BIN="${STUB_BIN}/gh" \
        FNO_LOOPCHECK_GIT_BIN="${STUB_BIN}/git" \
            "$REAL_BIN" loop-check \
            --state "$MANIFEST" \
            --transcript "$TRANSCRIPT" \
            --cwd "$TMP_DIR" \
            --now "2026-06-05T00:30:00Z" \
            2>/dev/null
    ) || FIRE_RC=$?
}

# ─────────────────────────────────────────────────────────────────────────────
# Fire 1: expect block, 1 loop_check event line
# ─────────────────────────────────────────────────────────────────────────────
log "Fire 1 (subprocess process 1)"
fire_subprocess

f1_ok=true

DECISION_1=$(jq -r '.decision // "missing"' <<< "$FIRE_OUTPUT" 2>/dev/null)
if [[ "$DECISION_1" != "block" ]]; then
    fail "Fire 1: expected decision=block, got '$DECISION_1'; output: $FIRE_OUTPUT"
    f1_ok=false
fi

# Events file must now exist with at least 1 loop_check line
if [[ ! -f "$EVENTS_FILE" ]]; then
    fail "Fire 1: events.jsonl not created at $EVENTS_FILE"
    f1_ok=false
else
    LINES_AFTER_1=$(count_lines_matching 'loop_check' "$EVENTS_FILE")
    if [[ "$LINES_AFTER_1" -lt 1 ]]; then
        fail "Fire 1: expected at least 1 loop_check line, got $LINES_AFTER_1"
        f1_ok=false
    fi
fi

[[ "$f1_ok" == "true" ]] && pass "Fire 1: decision=block + 1 loop_check event line"

# ─────────────────────────────────────────────────────────────────────────────
# Fire 2: expect block, 2 loop_check event lines (cross-process accumulation)
# ─────────────────────────────────────────────────────────────────────────────
log "Fire 2 (subprocess process 2)"
fire_subprocess

f2_ok=true

DECISION_2=$(jq -r '.decision // "missing"' <<< "$FIRE_OUTPUT" 2>/dev/null)
if [[ "$DECISION_2" != "block" ]]; then
    fail "Fire 2: expected decision=block, got '$DECISION_2'; output: $FIRE_OUTPUT"
    f2_ok=false
fi

LINES_AFTER_2=$(count_lines_matching 'loop_check' "$EVENTS_FILE")
if [[ "$LINES_AFTER_2" -lt 2 ]]; then
    fail "Fire 2: expected at least 2 loop_check lines after fire 2, got $LINES_AFTER_2"
    f2_ok=false
fi

[[ "$f2_ok" == "true" ]] && pass "Fire 2: decision=block + 2 loop_check event lines (cross-process accumulation)"

# ─────────────────────────────────────────────────────────────────────────────
# Fire 3: expect allow + NoProgress + fires=3
# ─────────────────────────────────────────────────────────────────────────────
log "Fire 3 (subprocess process 3 - backstop trip)"
fire_subprocess

f3_ok=true

DECISION_3=$(jq -r '.decision // "missing"' <<< "$FIRE_OUTPUT" 2>/dev/null)
if [[ "$DECISION_3" != "allow" ]]; then
    fail "Fire 3: expected decision=allow (backstop trip), got '$DECISION_3'; output: $FIRE_OUTPUT"
    f3_ok=false
fi

TERM_3=$(jq -r '.termination_reason // "null"' <<< "$FIRE_OUTPUT" 2>/dev/null)
if [[ "$TERM_3" != "NoProgress" ]]; then
    fail "Fire 3: expected termination_reason=NoProgress, got '$TERM_3'; output: $FIRE_OUTPUT"
    f3_ok=false
fi

FIRES_3=$(jq -r '.fires // 0' <<< "$FIRE_OUTPUT" 2>/dev/null)
if [[ "$FIRES_3" -ne 3 ]]; then
    fail "Fire 3: expected fires=3, got '$FIRES_3'; output: $FIRE_OUTPUT"
    f3_ok=false
fi

[[ "$f3_ok" == "true" ]] && pass "Fire 3: allow + NoProgress + fires=3 (backstop tripped across 3 separate processes)"

# ─────────────────────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────────────────────
rm -rf "$TMP_DIR" 2>/dev/null || true

# ── summary ────────────────────────────────────────────────────────────────────
echo ""
printf '[backstop] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
