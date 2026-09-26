#!/usr/bin/env bash
# x-3227: the session cargo build-dir env reaches the loop-check stage, so
# done_probes running cargo reuse the session build base instead of
# cold-compiling into .cargo/config.toml's {cargo-cache-home} tree.
#
# The claude stop hook is native now (crates/fno-agents/src/hook/stop.rs): it
# resolves the value from config and sets CARGO_BUILD_BUILD_DIR in its own
# process, so the old bash cases (E1 unset+answered, E2 no fno, E3 preset,
# E4 garbage answer) moved to the unit suite
# (`export_session_build_dir_sets_unset_and_honors_preset` beside the
# implementation). The agy adapter is still a bash hook and still exports for
# its child - that contract is what this file pins:
#
#   E5  agy adapter exports the build-dir env for its loop-check child

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

PASS=0; FAIL=0; SKIP_COUNT=0
pass() { PASS=$((PASS+1)); printf '[probe-env] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[probe-env] FAIL: %s\n' "$*" >&2; }
skip() { SKIP_COUNT=$((SKIP_COUNT+1)); printf '[probe-env] SKIP: %s\n' "$*" >&2; }

safe_path() { echo "/usr/bin:/bin:/usr/sbin:/sbin"; }

cleanup() { rm -rf "${TMP_DIR:-/nonexistent}" "${HOME_DIR:-/nonexistent}" 2>/dev/null || true; }

setup_env() {
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

    FNO_PATH="${TMP_DIR}/bin:$(safe_path)"
    STUB_FNO="${TMP_DIR}/bin/fno"
    cat > "$STUB_FNO" <<STUB
#!/usr/bin/env bash
printf '%s\n' '/tmp/fake-cargo-base/{workspace-path-hash}'
exit 0
STUB
    chmod +x "$STUB_FNO"
}

# ── E5: the agy adapter exports the same env ────────────────────────────────
AGY_HOOK="${REPO_ROOT}/hooks/agy-target-stop-hook.sh"
log_e5="E5: agy adapter exports the build-dir env for its loop-check child"
if [[ -f "$AGY_HOOK" ]]; then
    {
        setup_env
        AGY_DUMP="${TMP_DIR}/agy-child-env.txt"
        STUB_AGY_AGENTS="${TMP_DIR}/fno-agents-agy-stub"
        cat > "$STUB_AGY_AGENTS" <<STUB
#!/usr/bin/env bash
env > "$AGY_DUMP"
printf '{"decision":"allow","termination_reason":"DonePRGreen","message":"ok"}\n'
exit 0
STUB
        chmod +x "$STUB_AGY_AGENTS"
        AGY_INPUT="{\"transcriptPath\":\"${TMP_DIR}/aaaa-0001.jsonl\",\"fullyIdle\":true,\"conversationId\":\"c-3227\"}"
        HOOK_RC=0
        (cd "$TMP_DIR" && env -u CARGO_BUILD_BUILD_DIR CLAUDECODE=0 HOME="$HOME_DIR" \
            PATH="$FNO_PATH" FNO_AGENTS_BIN="$STUB_AGY_AGENTS" \
            bash "$AGY_HOOK" <<< "$AGY_INPUT" >/dev/null 2>&1) || HOOK_RC=$?
        ok=true
        [[ "$HOOK_RC" -eq 0 ]] || { fail "$log_e5: hook rc $HOOK_RC"; ok=false; }
        grep -q "^CARGO_BUILD_BUILD_DIR=/tmp/fake-cargo-base/{workspace-path-hash}\$" \
            "$AGY_DUMP" 2>/dev/null \
            || { fail "$log_e5: no CARGO_BUILD_BUILD_DIR in agy child env"; ok=false; }
        [[ "$ok" == true ]] && pass "$log_e5"
        cleanup
    }
else
    skip "$log_e5: agy hook not present"
fi

printf '[probe-env] done: %d pass, %d fail, %d skip\n' "$PASS" "$FAIL" "$SKIP_COUNT"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
