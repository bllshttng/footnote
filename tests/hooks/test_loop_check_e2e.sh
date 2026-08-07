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
#   Cases D/E: real approval store -> production fno mux -> evaluator -> real
#           loop-check + each stop hook -> real finalize + durable receipt.
#   Case F: malformed active delivery stays blocked across the production mux.
#
# Modelled after tests/hooks/test_loop_check_shim.sh conventions:
#   - tmpdir per case, HOME isolated, jq + bash required
#   - pass/fail counters, exit 77 on missing deps, exit 1 on failure

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"
AGY_HOOK="${REPO_ROOT}/hooks/agy-target-stop-hook.sh"
INIT_SCRIPT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

# ── counters ─────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[e2e] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[e2e] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[e2e] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[e2e] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ────────────────────────────────────────────────────────────────
[[ -f "$HOOK" ]]        || { fail "hook not found: $HOOK"; exit 1; }
[[ -f "$AGY_HOOK" ]]    || { fail "hook not found: $AGY_HOOK"; exit 1; }
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

MUX_BIN="${REPO_ROOT}/crates/fno/target/debug/fno"
if [[ ! -x "$MUX_BIN" ]]; then
    cargo build --quiet --manifest-path "${REPO_ROOT}/crates/fno/Cargo.toml" --bin fno \
        || { fail "could not build the production fno mux"; exit 1; }
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

run_agy_hook() {
    local cwd="$1"; shift
    local input_json="$1"; shift
    HOOK_RC=0
    HOOK_STDOUT=""
    HOOK_STDERR_FILE="${cwd}/agy-hook.stderr"
    HOOK_STDOUT=$(
        cd "$cwd" || exit 1
        env "$@" bash "$AGY_HOOK" <<< "$input_json" 2>"$HOOK_STDERR_FILE"
    ) || HOOK_RC=$?
    HOOK_STDERR=$(cat "$HOOK_STDERR_FILE" 2>/dev/null || true)
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

setup_delivery_project() {
    local dir="$1" session_id="$2" harness_id="$3" transcript_kind="$4"
    mkdir -p "$dir/.fno" "$dir/home/.fno" "$dir/handoffs" "$dir/stubs" "$dir/hook-bin" "$dir/uv-tools"
    ln -s "${REPO_ROOT}/cli/.venv" "$dir/uv-tools/fno"
    init_git_repo "$dir"
    PROJECT_DIR="$dir" SESSION_ID="$session_id" HARNESS_ID="$harness_id" \
        TRANSCRIPT_KIND="$transcript_kind" uv run --project "${REPO_ROOT}/cli" python - <<'PY'
import datetime as dt
import os
from pathlib import Path

import yaml

from fno.approvals import (
    AdapterCapability,
    ApprovalRequest,
    DecisionKind,
    EffectState,
    EffectStore,
    action_digest,
)


class AllowAuthority:
    source = "e2e-policy"

    def may_approve(self, *, principal_id: str, effect_class: str, destination: str) -> bool:
        return principal_id == "principal:e2e"


root = Path(os.environ["PROJECT_DIR"])
session_id = os.environ["SESSION_ID"]
harness_id = os.environ["HARNESS_ID"]
now = dt.datetime.now(dt.timezone.utc)
request = ApprovalRequest(
    request_id="request-e2e",
    principal_id="principal:e2e",
    work_order_id="x-delivery-e2e",
    attempt_id="attempt-e2e",
    effect_id="effect-e2e",
    effect_class="communication.external",
    destination="email:customer@example.com",
    action_digest=action_digest({"subject": "hello", "body": "e2e"}),
    created_at=now,
    expires_at=now + dt.timedelta(hours=1),
)
adapter = AdapterCapability(adapter_id="smtp-e2e", adapter_version="1")
with EffectStore(
    root / ".fno/approvals.db",
    authority=AllowAuthority(),
    events_path=root / ".fno/events.jsonl",
    now=lambda: now,
) as store:
    store.submit(request)
    store.decide(
        request_digest=request.request_digest,
        deciding_principal_id="principal:e2e",
        decision=DecisionKind.APPROVED,
    )
    token = store.prepare(
        request_digest=request.request_digest,
        idempotency_key="effect-key-e2e",
        adapter=adapter,
    ).dispatch_token
    assert token is not None
    store.settle(
        dispatch_token=token,
        idempotency_key="effect-key-e2e",
        state=EffectState.EXECUTING,
    )
    store.settle(
        dispatch_token=token,
        idempotency_key="effect-key-e2e",
        state=EffectState.ACKNOWLEDGED,
        external_ref="message-e2e",
    )

frontmatter = {
    "node": "x-delivery-e2e",
    "status": "ready",
    "created": now.date().isoformat(),
    "completion": "delivery",
    "company_work": {
        "work_order": {"node_id": "x-delivery-e2e", "attempt_id": "attempt-e2e"},
        "deliverables": [{
            "id": "external-send",
            "kind": "arbitrary-output",
            "work_order_id": "x-delivery-e2e",
            "attempt_id": "attempt-e2e",
            "effect_id": "effect-e2e",
            "required_evidence_ids": ["approval-ready", "effect-ready", "ack-ready"],
        }],
        "effects": [{
            "id": "effect-e2e",
            "work_order_id": "x-delivery-e2e",
            "attempt_id": "attempt-e2e",
            "deliverable_id": "external-send",
            "effect_class": "communication.external",
            "destination": "email:customer@example.com",
            "idempotency_key": "effect-key-e2e",
            "approval_id": request.request_digest,
        }],
        "evidence": [
            {"id": "approval-ready", "work_order_id": "x-delivery-e2e", "attempt_id": "attempt-e2e", "subject_kind": "approval", "subject_id": request.request_digest, "result": "unknown"},
            {"id": "effect-ready", "work_order_id": "x-delivery-e2e", "attempt_id": "attempt-e2e", "subject_kind": "effect", "subject_id": "effect-e2e", "result": "unknown"},
            {"id": "ack-ready", "work_order_id": "x-delivery-e2e", "attempt_id": "attempt-e2e", "subject_kind": "acknowledgment", "subject_id": "effect-e2e", "result": "unknown"},
        ],
    },
}
(root / "plan.md").write_text(
    "---\n" + yaml.safe_dump(frontmatter, sort_keys=False) + "---\n# Delivery E2E\n"
)
(root / ".fno" / ("target" + "-state.md")).write_text(
    "---\n"
    f"session_id: {session_id}\n"
    "created_at: 2026-08-02T12:00:00Z\n"
    "input: generic delivery e2e\n"
    "plan_path: plan.md\n"
    "provider: codex\n"
    f"harness_session_id: {harness_id}\n"
    f"claude_session_id: {harness_id}\n"
    "attended: true\n"
    "no_external: false\n"
    "no_ship: false\n"
    "advisory: false\n"
    "cross_project: false\n"
    "---\n"
    "# Target Session State\n"
    "graph_node_id: x-delivery-e2e\n"
)
transcript = root / f"{harness_id}.jsonl"
if os.environ["TRANSCRIPT_KIND"] == "agy":
    transcript.write_text('{"role":"model","parts":[{"text":"Done <promise>MISSION COMPLETE</promise>"}]}\n')
else:
    transcript.write_text('{"message":{"role":"assistant","content":"Done <promise>MISSION COMPLETE</promise>"}}\n')
PY
    make_no_pr_gh "$dir/stubs/gh"
    make_git_stub "$dir/stubs/git" "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    ln -s "$dir/stubs/gh" "$dir/hook-bin/gh"
}

assert_delivery_artifacts() {
    local dir="$1" session_id="$2"
    grep -q '^status: done$' "$dir/plan.md" || return 1
    grep -q '"type":"delivery_verdict_evaluated"' "$dir/.fno/events.jsonl" || return 1
    grep -q 'DoneDelivery' "$dir/.fno/events.jsonl" || return 1
    grep -q '"type":"session_finalized"' "$dir/.fno/events.jsonl" || return 1
    grep -R -q "session: \`${session_id}\`" "$dir/handoffs" || return 1
    grep -R -q 'fno-delivery://x-delivery-e2e/attempt-e2e/' "$dir/handoffs" || return 1
    PROJECT_DIR="$dir" uv run --project "${REPO_ROOT}/cli" python - <<'PY'
import os
import sqlite3
from pathlib import Path

root = Path(os.environ["PROJECT_DIR"])
with sqlite3.connect(root / ".fno/approvals.db") as conn:
    states = [row[0] for row in conn.execute("SELECT state FROM attempts")]
assert states == ["acknowledged"], states
PY
}

assert_real_pending_path() {
    local dir="$1" top pending
    top=$(git -C "$dir" rev-parse --show-toplevel) || return 1
    pending=$(git -C "$dir" rev-parse --git-path fno-delivery-finalize-pending-probe) || return 1
    [[ "$pending" == /* ]] || pending="$top/$pending"
    [[ "$pending" == "$top/.git/fno-delivery-finalize-pending-probe" ]]
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

    # x-0eaf: seed a local review attestation so the coverage gate sees review.
    # This Case A config has no review lane, so the green gh stub's bot review is
    # not fetched; a head-pinned code-review attestation provides coverage, which
    # is the operator's actual coverage source, so the green path reaches
    # DonePRGreen (not DoneUnreviewed).
    printf '%s\n' '{"type":"review_attestation","data":{"reviewer":"code-review","head_sha":"deadbeefdeadbeefdeadbeefdeadbeef00000001","verdict":"pass"}}' > "${TMP_DIR}/.fno/events.jsonl"

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

# ─────────────────────────────────────────────────────────────────────────────
# Case D: production mux + real approval store + Claude hook + real finalize.
# ─────────────────────────────────────────────────────────────────────────────
log "Case D: generic delivery through production mux and Claude hook"
{
    TMP_DIR="$(mktemp -d)"
    SESSION_ID="delivery-claude-e2e"
    HARNESS_ID="dddd-delivery-claude"
    setup_delivery_project "$TMP_DIR" "$SESSION_ID" "$HARNESS_ID" "claude"

    COMMON_PATH="$(dirname "$MUX_BIN"):${TMP_DIR}/hook-bin:${PATH}"
    EVAL_JSON=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$TMP_DIR/home" UV_TOOL_DIR="$TMP_DIR/uv-tools" \
            FNO_BOOTSTRAP_WHEEL="$REPO_ROOT/cli" PATH="$COMMON_PATH" \
            "$MUX_BIN" delivery evaluate --json --plan-path plan.md --events .fno/events.jsonl
    )
    cd_ok=true
    assert_real_pending_path "$TMP_DIR" \
        || { fail "Case D: pending-finalization path is not in the real test git dir"; cd_ok=false; }
    if ! printf '%s' "$EVAL_JSON" | jq -e '.status == "evaluated" and .verdict.aggregate == "passed"' >/dev/null; then
        fail "Case D: production fno mux did not return a passed canonical verdict: $EVAL_JSON"
        cd_ok=false
    fi

    INPUT_JSON="{\"transcript_path\":\"${TMP_DIR}/${HARNESS_ID}.jsonl\"}"
    run_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${TMP_DIR}/home" \
        "HANDOFFS_DIR=${TMP_DIR}/handoffs" \
        "UV_TOOL_DIR=${TMP_DIR}/uv-tools" \
        "FNO_BOOTSTRAP_WHEEL=${REPO_ROOT}/cli" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_FNO_BIN=${MUX_BIN}" \
        "FNO_LOOPCHECK_GH_BIN=${TMP_DIR}/stubs/gh" \
        "FNO_LOOPCHECK_GIT_BIN=${TMP_DIR}/stubs/git" \
        "PATH=${COMMON_PATH}"

    [[ "$HOOK_RC" -eq 0 ]] || { fail "Case D: Claude hook rc=$HOOK_RC: $HOOK_STDERR"; cd_ok=false; }
    assert_delivery_artifacts "$TMP_DIR" "$SESSION_ID" \
        || { fail "Case D: missing stamped plan, bound receipt, durable terminal, or acknowledged store state"; cd_ok=false; }
    [[ "$HOOK_STDERR" != *"merge"* && "$HOOK_STDERR" != *"research"* ]] \
        || { fail "Case D: generic finalize entered a PR/research side-effect path: $HOOK_STDERR"; cd_ok=false; }
    [[ "$cd_ok" == "true" ]] && pass "Case D: store -> mux -> loop-check -> Claude hook -> finalize"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# Case E: same real generic journey through the agy hook and transcript adapter.
# ─────────────────────────────────────────────────────────────────────────────
log "Case E: generic delivery through production mux and agy hook"
{
    TMP_DIR="$(mktemp -d)"
    SESSION_ID="delivery-agy-e2e"
    HARNESS_ID="eeee-delivery-agy"
    setup_delivery_project "$TMP_DIR" "$SESSION_ID" "$HARNESS_ID" "agy"
    COMMON_PATH="$(dirname "$MUX_BIN"):${TMP_DIR}/hook-bin:${PATH}"
    INPUT_JSON="{\"conversationId\":\"${HARNESS_ID}\",\"fullyIdle\":true,\"workspacePaths\":[\"${TMP_DIR}\"],\"transcriptPath\":\"${TMP_DIR}/${HARNESS_ID}.jsonl\"}"
    run_agy_hook "$TMP_DIR" "$INPUT_JSON" \
        "HOME=${TMP_DIR}/home" \
        "HANDOFFS_DIR=${TMP_DIR}/handoffs" \
        "UV_TOOL_DIR=${TMP_DIR}/uv-tools" \
        "FNO_BOOTSTRAP_WHEEL=${REPO_ROOT}/cli" \
        "FNO_AGENTS_BIN=${REAL_BIN}" \
        "FNO_LOOPCHECK_FNO_BIN=${MUX_BIN}" \
        "FNO_LOOPCHECK_GH_BIN=${TMP_DIR}/stubs/gh" \
        "FNO_LOOPCHECK_GIT_BIN=${TMP_DIR}/stubs/git" \
        "PATH=${COMMON_PATH}"

    ce_ok=true
    assert_real_pending_path "$TMP_DIR" \
        || { fail "Case E: pending-finalization path is not in the real test git dir"; ce_ok=false; }
    [[ "$HOOK_RC" -eq 0 ]] || { fail "Case E: agy hook rc=$HOOK_RC: $HOOK_STDERR"; ce_ok=false; }
    printf '%s' "$HOOK_STDOUT" | jq -e 'type == "object" and length == 0' >/dev/null \
        || { fail "Case E: agy hook did not emit the terminal allow object: $HOOK_STDOUT"; ce_ok=false; }
    assert_delivery_artifacts "$TMP_DIR" "$SESSION_ID" \
        || { fail "Case E: missing stamped plan, bound receipt, durable terminal, or acknowledged store state"; ce_ok=false; }
    [[ "$ce_ok" == "true" ]] && pass "Case E: store -> mux -> loop-check -> agy hook -> finalize"
    cleanup
}

# ─────────────────────────────────────────────────────────────────────────────
# Case F: malformed active declaration stays blocked across the production mux.
# ─────────────────────────────────────────────────────────────────────────────
log "Case F: malformed generic plan fails closed through production mux"
{
    TMP_DIR="$(mktemp -d)"
    SESSION_ID="delivery-malformed-e2e"
    HARNESS_ID="ffff-delivery-malformed"
    setup_delivery_project "$TMP_DIR" "$SESSION_ID" "$HARNESS_ID" "claude"
    PROJECT_DIR="$TMP_DIR" uv run --project "${REPO_ROOT}/cli" python - <<'PY'
import os
from pathlib import Path

path = Path(os.environ["PROJECT_DIR"]) / "plan.md"
text = path.read_text()
text = text.replace(
    "work_order:\n    node_id: x-delivery-e2e\n    attempt_id: attempt-e2e\n",
    "work_order:\n    node_id: x-delivery-e2e\n",
    1,
)
path.write_text(text)
PY
    COMMON_PATH="$(dirname "$MUX_BIN"):${PATH}"
    DECISION=$(
        cd "$TMP_DIR" || exit 1
        env HOME="$TMP_DIR/home" UV_TOOL_DIR="$TMP_DIR/uv-tools" \
            FNO_BOOTSTRAP_WHEEL="$REPO_ROOT/cli" FNO_LOOPCHECK_FNO_BIN="$MUX_BIN" \
            PATH="$COMMON_PATH" "$REAL_BIN" loop-check \
            --state ".fno/target-state.md" \
            --transcript "${HARNESS_ID}.jsonl" \
            --cwd "$TMP_DIR" \
            --events ".fno/events.jsonl" \
            --global-events "$TMP_DIR/home/.fno/events.jsonl" \
            --settings "$TMP_DIR/.fno/missing-settings.toml" \
            --ledger "$TMP_DIR/.fno/missing-ledger.json" \
            --gh-bin "$TMP_DIR/stubs/gh" \
            --git-bin "$TMP_DIR/stubs/git"
    )
    cf_ok=true
    printf '%s' "$DECISION" | jq -e '.decision == "block" and .termination_reason == null' >/dev/null \
        || { fail "Case F: malformed declaration did not block: $DECISION"; cf_ok=false; }
    printf '%s' "$DECISION" | jq -e '.message | contains("attempt_id")' >/dev/null \
        || { fail "Case F: block omitted the rejected binding: $DECISION"; cf_ok=false; }
    [[ "$cf_ok" == "true" ]] && pass "Case F: production mux/parser malformed path remains fail closed"
    cleanup
}

# ── summary ────────────────────────────────────────────────────────────────────
echo ""
printf '[e2e] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
