#!/usr/bin/env bash
# tests/events/test-loop-check-emission-schema.sh
#
# Journey test for loop-check event emission schema (Gap 3).
#
# Runs the REAL fno-agents loop-check binary directly (not via the shim) once
# per decision shape:
#   S1  block-no-PR           (gh present, returns no PR)
#   S2  termination-DonePRGreen (green gh + promise)
#   S3  advisory-mode fire     (gh absent -> FNO_LOOPCHECK_GH_BIN=/nonexistent)
#   S4  legacy-manifest allow  (status: COMPLETE -> loop_check_legacy_manifest)
#
# For EVERY emitted line in the resulting events.jsonl this test sources
# scripts/lib/events-validate.sh and calls validate_event TYPE JSON for each
# line, asserting rc=0.
#
# This catches emitter-vs-schema drift with REAL emitted lines (not fixtures).
#
# Modelled after tests/events/test-bash-validator.sh conventions.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VALIDATOR="${REPO_ROOT}/scripts/lib/events-validate.sh"

# ── counters ─────────────────────────────────────────────────────────────────
PASS=0; FAIL=0; SKIP_COUNT=0

log()  { printf '[schema] %s\n' "$*"; }
pass() { PASS=$((PASS+1)); printf '[schema] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[schema] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[schema] SKIP: %s\n' "$*" >&2; }

# ── pre-flight ────────────────────────────────────────────────────────────────
command -v jq    >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }
command -v bash  >/dev/null 2>&1 || { skip "bash not on PATH"; exit 77; }

[[ -f "$VALIDATOR" ]] || { skip "events-validate.sh not found: $VALIDATOR"; exit 77; }

REAL_BIN="${REPO_ROOT}/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$REAL_BIN" ]]; then
    skip "fno-agents debug binary not found at $REAL_BIN; run: cd crates/fno-agents && cargo build"
    exit 77
fi

# ── source the validator ──────────────────────────────────────────────────────
# shellcheck disable=SC1090
source "$VALIDATOR"

# ── helper: make a stub binary ───────────────────────────────────────────────
make_stub() {
    local path="$1"; shift
    cat > "$path"
    chmod +x "$path"
}

make_green_gh() {
    local path="$1"
    make_stub "$path" <<'STUB'
#!/bin/sh
if echo "$*" | grep -q -- "--version"; then
  echo 'gh version 2.x'; exit 0
fi
if echo "$*" | grep -q "headRefName"; then
  echo '{"state":"OPEN","number":1,"headRefName":"main"}'; exit 0
fi
if echo "$*" | grep -q "checks"; then
  echo '[{"name":"ci","state":"SUCCESS","bucket":"pass"}]'; exit 0
fi
if echo "$*" | grep -q "pulls/"; then
  echo '[]'; exit 0
fi
if echo "$*" | grep -q "reviews"; then
  echo '{"reviews":[{"author":{"login":"chatgpt-codex-connector"},"state":"COMMENTED","submittedAt":"2026-06-05T01:00:00Z"}],"comments":[]}'; exit 0
fi
exit 1
STUB
}

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

make_git_stub() {
    local path="$1" sha="${2:-deadbeefdeadbeefdeadbeefdeadbeef00000001}"
    make_stub "$path" <<STUB
#!/bin/sh
echo "${sha}"
STUB
}

# ── helper: build a minimal manifest ────────────────────────────────────────
# Usage: write_manifest FILEPATH SESSION_ID ATTENDED [extra_lines]
write_manifest() {
    local path="$1" sid="$2" attended="${3:-true}"
    cat > "$path" <<MANIFEST
---
session_id: ${sid}
created_at: 2026-06-05T00:00:00Z
attended: ${attended}
---
MANIFEST
}

# ── helper: validate all lines in an events file ─────────────────────────────
# Usage: validate_events_file LABEL EVENTS_FILE
# Returns: 0 if all valid, increments FAIL counter for each invalid line.
validate_events_file() {
    local label="$1" events_file="$2"

    if [[ ! -f "$events_file" ]]; then
        fail "${label}: events file not created at ${events_file}"
        return 1
    fi

    local line_num=0 line_ok=true
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        line_num=$((line_num + 1))

        # Extract the type field from the envelope
        local ev_type
        ev_type=$(jq -r '.type // empty' <<< "$line" 2>/dev/null)
        if [[ -z "$ev_type" ]]; then
            fail "${label}: line ${line_num} has no 'type' field: ${line}"
            line_ok=false
            continue
        fi

        local rc=0
        local diag
        diag=$(validate_event "$ev_type" "$line" 2>&1) || rc=$?
        if [[ "$rc" -eq 2 ]]; then
            # rc=2 = schema substrate unavailable; treat as skip (not hard fail)
            skip "${label}: line ${line_num} schema unavailable (rc=2): ${diag}"
        elif [[ "$rc" -ne 0 ]]; then
            fail "${label}: line ${line_num} type=${ev_type} failed validation (rc=${rc}): ${diag}"
            fail "${label}: offending event: ${line}"
            line_ok=false
        fi
    done < "$events_file"

    if [[ "$line_num" -eq 0 ]]; then
        fail "${label}: events file was empty (expected at least one event)"
        return 1
    fi

    [[ "$line_ok" == "true" ]] && return 0 || return 1
}

# ─────────────────────────────────────────────────────────────────────────────
# S1: block-no-PR -- gh present but returns exit 1 for pr commands
# ─────────────────────────────────────────────────────────────────────────────
log "S1: block-no-PR emission"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"

    MANIFEST="${TMP_DIR}/state.md"
    write_manifest "$MANIFEST" "schema-sess-s1" "true"

    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

    EVENTS_FILE="${TMP_DIR}/.fno/events.jsonl"

    make_no_pr_gh "${STUB_BIN}/gh"
    make_git_stub "${STUB_BIN}/git" "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1"

    HOME="${HOME_DIR}" FNO_LOOPCHECK_GH_BIN="${STUB_BIN}/gh" FNO_LOOPCHECK_GIT_BIN="${STUB_BIN}/git" \
        "$REAL_BIN" loop-check \
        --state "$MANIFEST" \
        --transcript "$TRANSCRIPT" \
        --cwd "$TMP_DIR" \
        --now "2026-06-05T00:30:00Z" \
        --events "$EVENTS_FILE" \
        >/dev/null 2>/dev/null || true

    if validate_events_file "S1" "$EVENTS_FILE"; then
        pass "S1: block-no-PR events all validate against schema"
    fi

    rm -rf "$TMP_DIR"
}

# ─────────────────────────────────────────────────────────────────────────────
# S2: termination-DonePRGreen -- green gh + promise
# ─────────────────────────────────────────────────────────────────────────────
log "S2: termination-DonePRGreen emission"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"

    MANIFEST="${TMP_DIR}/state.md"
    write_manifest "$MANIFEST" "schema-sess-s2" "true"

    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"assistant","content":"Done! <promise>MISSION COMPLETE</promise>"}}\n' > "$TRANSCRIPT"

    EVENTS_FILE="${TMP_DIR}/.fno/events.jsonl"

    make_green_gh "${STUB_BIN}/gh"
    make_git_stub "${STUB_BIN}/git" "deadbeefdeadbeefdeadbeefdeadbeef00000001"

    HOME="${HOME_DIR}" FNO_LOOPCHECK_GH_BIN="${STUB_BIN}/gh" FNO_LOOPCHECK_GIT_BIN="${STUB_BIN}/git" \
        "$REAL_BIN" loop-check \
        --state "$MANIFEST" \
        --transcript "$TRANSCRIPT" \
        --cwd "$TMP_DIR" \
        --now "2026-06-05T00:30:00Z" \
        --events "$EVENTS_FILE" \
        >/dev/null 2>/dev/null || true

    if validate_events_file "S2" "$EVENTS_FILE"; then
        pass "S2: DonePRGreen events all validate against schema"
    fi

    rm -rf "$TMP_DIR"
}

# ─────────────────────────────────────────────────────────────────────────────
# S3: advisory-mode fire -- gh binary absent
# ─────────────────────────────────────────────────────────────────────────────
log "S3: advisory-mode (gh absent)"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno"

    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"

    MANIFEST="${TMP_DIR}/state.md"
    write_manifest "$MANIFEST" "schema-sess-s3" "true"

    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

    EVENTS_FILE="${TMP_DIR}/.fno/events.jsonl"

    # Point gh and git at non-existent paths -> advisory mode
    HOME="${HOME_DIR}" \
        "$REAL_BIN" loop-check \
        --state "$MANIFEST" \
        --transcript "$TRANSCRIPT" \
        --cwd "$TMP_DIR" \
        --now "2026-06-05T00:30:00Z" \
        --events "$EVENTS_FILE" \
        --gh-bin="/nonexistent/gh" \
        --git-bin="/nonexistent/git" \
        >/dev/null 2>/dev/null || true

    if validate_events_file "S3" "$EVENTS_FILE"; then
        pass "S3: advisory-mode events all validate against schema"
    fi

    rm -rf "$TMP_DIR"
}

# ─────────────────────────────────────────────────────────────────────────────
# S4: legacy-manifest allow -- status: COMPLETE -> loop_check_legacy_manifest
# ─────────────────────────────────────────────────────────────────────────────
log "S4: legacy-manifest (status: COMPLETE)"
{
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    STUB_BIN="${TMP_DIR}/stubs"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "$STUB_BIN"

    printf '# isolated\n' > "${TMP_DIR}/.fno/config.toml"

    MANIFEST="${TMP_DIR}/state.md"
    # Legacy manifest: has status: COMPLETE instead of attended field
    cat > "$MANIFEST" <<'MANIFEST'
---
session_id: schema-sess-s4
created_at: 2026-06-04T00:00:00Z
status: COMPLETE
---
MANIFEST

    TRANSCRIPT="${TMP_DIR}/transcript.jsonl"
    printf '{"message":{"role":"user","content":"go"}}\n' > "$TRANSCRIPT"

    EVENTS_FILE="${TMP_DIR}/.fno/events.jsonl"

    make_green_gh "${STUB_BIN}/gh"
    make_git_stub "${STUB_BIN}/git" "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"

    HOME="${HOME_DIR}" FNO_LOOPCHECK_GH_BIN="${STUB_BIN}/gh" FNO_LOOPCHECK_GIT_BIN="${STUB_BIN}/git" \
        "$REAL_BIN" loop-check \
        --state "$MANIFEST" \
        --transcript "$TRANSCRIPT" \
        --cwd "$TMP_DIR" \
        --now "2026-06-05T01:00:00Z" \
        --events "$EVENTS_FILE" \
        >/dev/null 2>/dev/null || true

    if validate_events_file "S4" "$EVENTS_FILE"; then
        pass "S4: legacy-manifest events all validate against schema"
    fi

    # Bonus: legacy event name should appear
    if grep -q 'loop_check_legacy_manifest' "$EVENTS_FILE" 2>/dev/null; then
        pass "S4 bonus: loop_check_legacy_manifest event present"
    else
        fail "S4 bonus: loop_check_legacy_manifest event missing; content: $(cat "$EVENTS_FILE" 2>/dev/null)"
    fi

    rm -rf "$TMP_DIR"
}

# ── summary ────────────────────────────────────────────────────────────────────
echo ""
printf '[schema] Results: %d passed, %d failed, %d skipped\n' "$PASS" "$FAIL" "$SKIP_COUNT"
if [[ "$FAIL" -gt 0 ]]; then
    exit 1
fi
exit 0
