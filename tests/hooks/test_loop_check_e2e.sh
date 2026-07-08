#!/usr/bin/env bash
# tests/hooks/test_loop_check_e2e.sh
#
# Journey tests for the loop-check integration (Gap 1 + Gap 2).
#
# Gap 1: the REAL binary wired through the REAL shim produces the right exit
#         codes and emits real events into the project events.jsonl.
# Gap 2: the spellings that init-target-state.sh writes for budget fields are
#         the spellings that fno-agents loop-check reads (init->verb coherence).
#
# Tests:
#   Case A (green path): real shim + real binary + green gh stub -> exit 0 +
#           termination(DonePRGreen) in events.jsonl.
#   Case B (block path): real shim + real binary + no-PR gh stub + no promise ->
#           exit 2 + stderr mentions continue message.
#   Case C (budget coherence): init writes unattended settings.yaml format that
#           verb reads; session cost > cap -> Budget termination.
#
# Modelled after tests/hooks/test_loop_check_shim.sh conventions:
#   - tmpdir per case, HOME isolated, jq + bash required
#   - pass/fail counters, exit 77 on missing deps, exit 1 on failure

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"
INIT_SCRIPT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

# ── counters ─────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[e2e] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[e2e] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[e2e] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[e2e] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ────────────────────────────────────────────────────────────────
[[ -f "$HOOK" ]]        || { fail "hook not found: $HOOK"; exit 1; }
[[ -f "$INIT_SCRIPT" ]] || { fail "init script not found: $INIT_SCRIPT"; exit 1; }
command -v jq   >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }
command -v bash >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }
command -v git  >/dev/null 2>&1 || { skip "git not on PATH"; exit 77; }

# Find the real binary (debug build; build was requested in the task pre-step).
REAL_BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$REAL_BIN" ]]; then
    skip "fno-agents debug binary not found at $REAL_BIN; run: cd crates/fno-agents && cargo build"
    exit 77
fi

# ── helper: make a stub binary ───────────────────────────────────────────────
make_stub() {
    local path="$1"; shift
    cat > "$path"
    chmod +x "$path"
}

# ── helper: green gh stub (from MockBins::green() in loop_check.rs) ──────────
# $2 = headRefOid the stub reports; must equal the test repo's real HEAD or
# the verb's unpushed-head guard (codex P1 on #447) blocks DonePRGreen.
make_green_gh() {
    local path="$1"
    local head_oid="${2:-}"
    make_stub "$path" <<STUB
#!/bin/sh
if echo "\$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'
  exit 0
fi
if echo "\$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main","headRefOid":"$head_oid"}'
  exit 0
fi
if echo "\$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'
  exit 0
fi
if echo "\$*" | grep -q "pulls/"; then
  echo '[]'
  exit 0
fi
if echo "\$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'
  exit 0
fi
exit 1
STUB
}

# ── helper: no-PR gh stub (from MockBins::no_pr() in loop_check.rs) ─────────
# Emits gh's real no-PR stderr: step 2 classifies a bare exit-1 as an OUTAGE
# (streak frozen), while this message marks healthy no-PR world-state.
make_no_pr_gh() {
    local path="$1"
    make_stub "$path" <<'STUB'
#!/bin/sh
if echo "$*" | grep -q -- "--version"; then echo 'gh version 2.x'; exit 0; fi
echo 'no pull requests found for branch "feat"' >&2
exit 1
STUB
}

# ── helper: fixed-sha git stub ───────────────────────────────────────────────
make_git_stub() {
    local path="$1" sha="${2:-deadbeefdeadbeefdeadbeefdeadbeef00000001}"
    make_stub "$path" <<STUB
#!/bin/sh
echo "$sha"
STUB
}

# ── helper: run hook from a cwd ──────────────────────────────────────────────
run_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift
    HOOK_RC=0
    HOOK_STDERR=""
    HOOK_STDERR=$(
        cd "$cwd" || exit 1
        env "$@" bash "$HOOK" <<< "$input_json" 2>&1 >/dev/null
    ) || HOOK_RC=$?
}

# ── helper: init a real git repo with a feature branch ───────────────────────
init_git_repo() {
    local dir="$1"
    git -C "$dir" init -q
    git -C "$dir" checkout -q -b feature/test-session 2>/dev/null || true
    git -C "$dir" config user.email "test@test.com"
    git -C "$dir" config user.name "Test"
    # Ensure there is at least one commit so we are not on an unborn branch
    git -C "$dir" commit -q --allow-empty -m "init" 2>/dev/null || true
}

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" 2>/dev/null || true; }

# ─────────────────────────────────────────────────────────────────────────────
# Case A: green path -- real binary, real shim, green gh, promise in transcript
# Expected: exit 0 + termination(DonePRGreen) in events.jsonl
# ─────────────────────────────────────────────────────────────────────────────
log "Case A: green path (real binary + shim)"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    # Write an isolated settings.yaml so the binary doesn't read $HOME
    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"

    # Init a real git repo on a feature branch so the shim's git call works
    init_git_repo "$TMP_DIR"

    # Transcript: last assistant message contains the promise tag
    UUID="aaaa-e2e-case-a"
    TRANSCRIPT="${TMP_DIR}/${UUID}.jsonl"
    printf '{"message":{"role":"assistant","content":"Done! <promise>MISSION COMPLETE</promise>"}}\n' > "$TRANSCRIPT"

    # Manifest: use the real init script (fastest path is hand-writing since
    # init has complex git-branch guards; write a compliant manifest directly)
    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: e2e-sess-green-001
created_at: 2026-06-05T00:00:00Z
input: "test"
plan_path: ""
cross_project: false
provider: claude
provider_mode: standard
provider_upgrade_reason: ""
owner_pid: 1
owner_started_at: 2026-06-05T00:00:00Z
owner_cwd: "${TMP_DIR}"
claude_transcript_id: ${UUID}
scratchpad_path: ${TMP_DIR}/.fno/scratchpad
target_size: M
no_external: false
no_docs: false
no_ship: false
no_verify: true
no_goals: false
no_browser: false
no_clean: true
no_how_to: false
no_memory: false
no_deferrals_capture: false
has_ui: false
attended: true
advisory: false
auto_merge_enabled: false
auto_merge_approved: false
mission_id: null
mission_wave: null
mission_slug: null
mission_from_msg_id: null
---
# Target Session State
STATE

    make_green_gh "${STUB_BIN}/gh" "deadbeefdeadbeefdeadbeefdeadbeef00000001"
    make_git_stub "${STUB_BIN}/git"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_GH_BIN=${STUB_BIN}/gh" \
        "FNO_LOOPCHECK_GIT_BIN=${STUB_BIN}/git" \
        "PATH=${STUB_BIN}:/usr/bin:/bin"

    ca_ok=true

    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "Case A: expected exit 0 from shim, got $HOOK_RC; stderr: $HOOK_STDERR"
        ca_ok=false
    fi

    PROJ_EVENTS="${TMP_DIR}/.fno/events.jsonl"
    if [[ ! -f "$PROJ_EVENTS" ]]; then
        fail "Case A: project events.jsonl not created at $PROJ_EVENTS"
        ca_ok=false
    elif ! grep -q '"termination"' "$PROJ_EVENTS" 2>/dev/null; then
        fail "Case A: termination event missing in events.jsonl; content: $(cat "$PROJ_EVENTS")"
        ca_ok=false
    elif ! grep -q 'DonePRGreen' "$PROJ_EVENTS" 2>/dev/null; then
        fail "Case A: DonePRGreen missing in events.jsonl; content: $(cat "$PROJ_EVENTS")"
        ca_ok=false
    fi

    [[ "$ca_ok" == "true" ]] && pass "Case A: green path -> exit 0 + termination(DonePRGreen)"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# Case B: block path -- no-PR gh, no promise -> exit 2, continue message
# ─────────────────────────────────────────────────────────────────────────────
log "Case B: block path (no PR, no promise)"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"
    init_git_repo "$TMP_DIR"

    UUID="bbbb-e2e-case-b"
    TRANSCRIPT="${TMP_DIR}/${UUID}.jsonl"
    # No promise tag; only a user message
    printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: e2e-sess-block-002
created_at: 2026-06-05T00:00:00Z
input: "test"
plan_path: ""
cross_project: false
provider: claude
provider_mode: standard
provider_upgrade_reason: ""
owner_pid: 1
owner_started_at: 2026-06-05T00:00:00Z
owner_cwd: "${TMP_DIR}"
claude_transcript_id: ${UUID}
scratchpad_path: ${TMP_DIR}/.fno/scratchpad
target_size: M
no_external: false
no_docs: false
no_ship: false
no_verify: true
no_goals: false
no_browser: false
no_clean: true
no_how_to: false
no_memory: false
no_deferrals_capture: false
has_ui: false
attended: true
advisory: false
auto_merge_enabled: false
auto_merge_approved: false
mission_id: null
mission_wave: null
mission_slug: null
mission_from_msg_id: null
---
# Target Session State
STATE

    make_no_pr_gh "${STUB_BIN}/gh"
    make_git_stub "${STUB_BIN}/git" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_GH_BIN=${STUB_BIN}/gh" \
        "FNO_LOOPCHECK_GIT_BIN=${STUB_BIN}/git" \
        "PATH=${STUB_BIN}:/usr/bin:/bin"

    cb_ok=true

    if [[ "$HOOK_RC" -ne 2 ]]; then
        fail "Case B: expected exit 2 from shim, got $HOOK_RC; stderr: $HOOK_STDERR"
        cb_ok=false
    fi

    # Shim echoes the block message from the verb to stderr
    if [[ -z "$HOOK_STDERR" ]]; then
        fail "Case B: expected a message on stderr from the shim, got nothing"
        cb_ok=false
    fi

    [[ "$cb_ok" == "true" ]] && pass "Case B: block path -> exit 2 + stderr message"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# Case C: init->verb budget coherence (Gap 2)
# Writes config.toml in the flat budget.unattended.cost_cap_usd format that
# init-target-state.sh resolves from config.sh and that loopcheck.rs
# parse_settings() reads as a TOML table.
# Session cost 0.05 > cap 0.01 -> Budget termination.
# ─────────────────────────────────────────────────────────────────────────────
log "Case C: budget coherence (init format -> verb reads -> Budget trip)"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    # Write config.toml in the flat format that init reads via config.sh and that
    # loopcheck.rs parse_settings() consumes (budget.unattended.cost_cap_usd, no
    # `config:` wrapper). This is the coherence assertion.
    cat > "${TMP_DIR}/.fno/config.toml" <<'TOML'
[budget.attended]
cost_cap_usd = 10.0

[budget.unattended]
cost_cap_usd = 0.01
TOML

    init_git_repo "$TMP_DIR"

    SESSION_ID="e2e-sess-budget-003"
    UUID="cccc-e2e-case-c"
    TRANSCRIPT="${TMP_DIR}/${UUID}.jsonl"
    printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

    # Write ledger with cost > cap for this session
    cat > "${TMP_DIR}/.fno/ledger.json" <<LEDGER
[{"session_id":"${SESSION_ID}","cost_usd":0.05,"tokens":1000}]
LEDGER

    # Write a manifest with attended: false (so verb picks unattended cap 0.01)
    STATE_FILE="${TMP_DIR}/.fno/target-state.md"
    cat > "$STATE_FILE" <<STATE
---
session_id: ${SESSION_ID}
created_at: 2026-06-05T00:00:00Z
input: "test"
plan_path: ""
cross_project: false
provider: claude
provider_mode: standard
provider_upgrade_reason: ""
owner_pid: 1
owner_started_at: 2026-06-05T00:00:00Z
owner_cwd: "${TMP_DIR}"
claude_transcript_id: ${UUID}
scratchpad_path: ${TMP_DIR}/.fno/scratchpad
target_size: M
no_external: false
no_docs: false
no_ship: false
no_verify: true
no_goals: false
no_browser: false
no_clean: true
no_how_to: false
no_memory: false
no_deferrals_capture: false
has_ui: false
attended: false
advisory: false
auto_merge_enabled: false
auto_merge_approved: false
mission_id: null
mission_wave: null
mission_slug: null
mission_from_msg_id: null
---
# Target Session State
STATE

    make_no_pr_gh "${STUB_BIN}/gh"
    make_git_stub "${STUB_BIN}/git"

    INPUT_JSON="{\"transcript_path\":\"${TRANSCRIPT}\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${HOME_DIR}" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_GH_BIN=${STUB_BIN}/gh" \
        "FNO_LOOPCHECK_GIT_BIN=${STUB_BIN}/git" \
        "PATH=${STUB_BIN}:/usr/bin:/bin"

    cc_ok=true

    # Budget trip -> allow -> exit 0 from shim
    if [[ "$HOOK_RC" -ne 0 ]]; then
        fail "Case C: expected exit 0 (Budget allow), got $HOOK_RC; stderr: $HOOK_STDERR"
        cc_ok=false
    fi

    PROJ_EVENTS="${TMP_DIR}/.fno/events.jsonl"
    if [[ ! -f "$PROJ_EVENTS" ]]; then
        fail "Case C: project events.jsonl not created"
        cc_ok=false
    elif ! grep -q '"termination"' "$PROJ_EVENTS" 2>/dev/null; then
        fail "Case C: termination event missing in events.jsonl; content: $(cat "$PROJ_EVENTS")"
        cc_ok=false
    elif ! grep -q 'Budget' "$PROJ_EVENTS" 2>/dev/null; then
        fail "Case C: Budget missing in termination event; content: $(cat "$PROJ_EVENTS")"
        cc_ok=false
    elif ! grep -q 'cost' "$PROJ_EVENTS" 2>/dev/null; then
        fail "Case C: axis=cost missing in termination event; content: $(cat "$PROJ_EVENTS")"
        cc_ok=false
    fi

    [[ "$cc_ok" == "true" ]] && pass "Case C: budget coherence -> exit 0 + termination(Budget,axis=cost)"
    cleanup
}

# ── summary ────────────────────────────────────────────────────────────────────
echo ""
printf '[e2e] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
