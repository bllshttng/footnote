#!/usr/bin/env bash
# x-3227: the target stop hook exports the session cargo build-dir env for its
# loop-check child, so done_probes running cargo reuse the session build base
# instead of cold-compiling into .cargo/config.toml's {cargo-cache-home} tree.
#
#   E1  env unset + fno answers the build-dir value -> child sees it exported
#   E2  env unset + no fno on PATH            -> child env unchanged
#   E3  env already preset by the session     -> the preset survives untouched
#   E4  fno prints something else             -> nothing is exported
#
# Mirrors tests/hooks/test_loop_check_shim.sh: the loop-check binary is a stub
# (FNO_AGENTS_BIN) that dumps its env, and `fno` is a stub on PATH.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"

PASS=0; FAIL=0; SKIP_COUNT=0
pass() { PASS=$((PASS+1)); printf '[probe-env] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[probe-env] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[probe-env] SKIP: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v jq >/dev/null 2>&1 || { skip "jq not on PATH"; exit 77; }

safe_path() { echo "/usr/bin:/bin:/usr/sbin:/sbin"; }

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" "${HOME_DIR:-/nonexistent}" 2>/dev/null || true; }

# A tmp project with a state file, a stub fno-agants -> env dump, and (when
# $1 is a nonempty value) a stub `fno` printing that value as the build dir.
setup_env() {
    local build_dir_answer="${1:-}"
    TMP_DIR="$(mktemp -d)"
    HOME_DIR="${TMP_DIR}/home"
    mkdir -p "${TMP_DIR}/.fno" "${HOME_DIR}/.fno" "${TMP_DIR}/bin"
    printf '{"role":"assistant","content":"hello"}\n' > "${TMP_DIR}/aaaa-0001.jsonl"
    cat > "${TMP_DIR}/.fno/target-state.md" <<STATE
---
session_id: test-session-env
created_at: 2026-09-15T00:00:00Z
claude_transcript_id: aaaa-0001
attended: true
status: IN_PROGRESS
---
STATE

    ENV_DUMP="${TMP_DIR}/loop-check-child-env.txt"
    STUB_AGENTS="${TMP_DIR}/fno-agents-stub"
    cat > "$STUB_AGENTS" <<STUB
#!/usr/bin/env bash
env > "$ENV_DUMP"
printf '{"decision":"allow","termination_reason":"DonePRGreen","message":"ok","fires":1,"fingerprint":"z"}\n'
exit 0
STUB
    chmod +x "$STUB_AGENTS"

    FNO_PATH="${TMP_DIR}/bin:$(safe_path)"
    if [[ -n "$build_dir_answer" ]]; then
        STUB_FNO="${TMP_DIR}/bin/fno"
        cat > "$STUB_FNO" <<STUB
#!/usr/bin/env bash
printf '%s\n' '$build_dir_answer'
exit 0
STUB
        chmod +x "$STUB_FNO"
    fi
}

run_hook() {
    HOOK_RC=0
    # -u scrubs the ambient rc export: a dev shell carries the real
    # CARGO_BUILD_BUILD_DIR, and E1/E2/E4 need the hook to start without it.
    (cd "$TMP_DIR" && env -u CARGO_BUILD_BUILD_DIR CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        HOME="$HOME_DIR" PATH="$FNO_PATH" FNO_AGENTS_BIN="$STUB_AGENTS" \
        bash "$HOOK" <<< '{"transcript_path":"aaaa-0001.jsonl"}' \
        >/dev/null 2>&1) || HOOK_RC=$?
}

child_has_env_line() { grep -q "^CARGO_BUILD_BUILD_DIR=$1\$" "$ENV_DUMP" 2>/dev/null; }

# ── E1 ──────────────────────────────────────────────────────────────────────
log_e1="E1: fno answers the build dir -> loop-check child inherits it"
{
    setup_env "/tmp/fake-cargo-base/{workspace-path-hash}"
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "$log_e1: hook rc $HOOK_RC"; ok=false; }
    child_has_env_line "/tmp/fake-cargo-base/{workspace-path-hash}" \
        || { fail "$log_e1: no CARGO_BUILD_BUILD_DIR in child env"; ok=false; }
    [[ "$ok" == true ]] && pass "$log_e1"
    cleanup
}

# ── E2 ──────────────────────────────────────────────────────────────────────
log_e2="E2: no fno on PATH -> child env unchanged"
{
    setup_env ""
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "$log_e2: hook rc $HOOK_RC"; ok=false; }
    if grep -q "^CARGO_BUILD_BUILD_DIR=" "$ENV_DUMP" 2>/dev/null; then
        fail "$log_e2: child saw a value the hook could not have resolved"
        ok=false
    fi
    [[ "$ok" == true ]] && pass "$log_e2"
    cleanup
}

# ── E3 ──────────────────────────────────────────────────────────────────────
log_e3="E3: session already carries the env -> preset survives untouched"
{
    setup_env "/tmp/fake-cargo-base/{workspace-path-hash}"
    HOOK_RC=0
    (cd "$TMP_DIR" && env CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= HOME="$HOME_DIR" \
        PATH="$FNO_PATH" FNO_AGENTS_BIN="$STUB_AGENTS" \
        CARGO_BUILD_BUILD_DIR="/tmp/session-own-base/{workspace-path-hash}" \
        bash "$HOOK" <<< '{"transcript_path":"aaaa-0001.jsonl"}' \
        >/dev/null 2>&1) || HOOK_RC=$?
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "$log_e3: hook rc $HOOK_RC"; ok=false; }
    child_has_env_line "/tmp/session-own-base/{workspace-path-hash}" \
        || { fail "$log_e3: preset value was not what the child saw"; ok=false; }
    [[ "$ok" == true ]] && pass "$log_e3"
    cleanup
}

# ── E4 ──────────────────────────────────────────────────────────────────────
log_e4="E4: fno prints a non-build-dir answer -> nothing exported"
{
    setup_env "not-a-build-dir"
    run_hook
    ok=true
    [[ "$HOOK_RC" -eq 0 ]] || { fail "$log_e4: hook rc $HOOK_RC"; ok=false; }
    if grep -q "^CARGO_BUILD_BUILD_DIR=" "$ENV_DUMP" 2>/dev/null; then
        fail "$log_e4: garbage answer was exported"
        ok=false
    fi
    [[ "$ok" == true ]] && pass "$log_e4"
    cleanup
}

printf '[probe-env] done: %d pass, %d fail, %d skip\n' "$PASS" "$FAIL" "$SKIP_COUNT"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
