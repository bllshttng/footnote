#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_HOOK="$ROOT/hooks/target-stop-hook.sh"
AGY_HOOK="$ROOT/hooks/agy-target-stop-hook.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); printf '[session-manifest] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[session-manifest] FAIL: %s\n' "$*" >&2; }

mkdir -p "$TMP/a/.fno" "$TMP/b/.fno" "$TMP/home/.fno"
cat > "$TMP/a/.fno/target-state.md" <<STATE
---
fno_id: run-a
session_id: run-a
harness_session_id: session-a
claude_session_id: session-a
owner_cwd: "$TMP/a"
---
STATE
cat > "$TMP/b/.fno/target-state.md" <<STATE
---
fno_id: run-b
session_id: run-b
harness_session_id: session-b
claude_session_id: session-b
owner_cwd: "$TMP/b"
---
STATE
printf '{"message":{"role":"assistant","content":"working"}}\n' > "$TMP/session-a.jsonl"

STUB="$TMP/fno-agents"
cat > "$STUB" <<'STUB'
#!/usr/bin/env bash
case "$1" in
  manifest-for-session)
    [[ -n "${RESOLVER_ID_RECORD:-}" ]] && printf '%s\n' "$3" > "$RESOLVER_ID_RECORD"
    [[ "${RESOLVER_RC:-0}" == "0" ]] && printf '%s\n' "$SELECTED_STATE"
    exit "${RESOLVER_RC:-0}"
    ;;
  loop-check)
    shift
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--state" ]]; then
        printf '%s\n' "$2" > "$STATE_RECORD"
        shift 2
      else
        shift
      fi
    done
    printf '%s\n' '{"decision":"block","termination_reason":null,"message":"selected owner","fires":1,"fingerprint":"session"}'
    ;;
  finalize) exit 0 ;;
  *) exit 2 ;;
esac
STUB
chmod +x "$STUB"

run_target() {
    local resolver_rc="$1" stderr_file="$TMP/target.stderr"
    : > "$stderr_file"
    rm -f "$TMP/state-record"
    TARGET_RC=0
    (
        cd "$TMP/b" || exit 1
        env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" SELECTED_STATE="$TMP/a/.fno/target-state.md" \
            RESOLVER_RC="$resolver_rc" STATE_RECORD="$TMP/state-record" \
            bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$TMP/session-a.jsonl\"}"
    ) >/dev/null 2>"$stderr_file" || TARGET_RC=$?
    TARGET_STDERR="$(cat "$stderr_file")"
}

run_agy() {
    local resolver_rc="$1" stderr_file="$TMP/agy.stderr" stdout_file="$TMP/agy.stdout"
    : > "$stderr_file"
    : > "$stdout_file"
    rm -f "$TMP/state-record"
    AGY_RC=0
    (
        cd "$TMP/b" || exit 1
        env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" SELECTED_STATE="$TMP/a/.fno/target-state.md" \
            RESOLVER_RC="$resolver_rc" STATE_RECORD="$TMP/state-record" \
            bash "$AGY_HOOK" <<< "{\"conversationId\":\"session-a\",\"transcriptPath\":\"$TMP/session-a.jsonl\",\"workspacePaths\":[\"$TMP/b\"],\"fullyIdle\":true}"
    ) >"$stdout_file" 2>"$stderr_file" || AGY_RC=$?
    AGY_STDOUT="$(cat "$stdout_file")"
    AGY_STDERR="$(cat "$stderr_file")"
}

run_target 0
if [[ "$TARGET_RC" -eq 2 && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "target shim sends session A's manifest to loop-check from worktree B"
else
    fail "target shim did not select A (rc=$TARGET_RC state=$(cat "$TMP/state-record" 2>/dev/null) stderr=$TARGET_STDERR)"
fi

run_target 1
if [[ "$TARGET_RC" -eq 0 && "$TARGET_STDERR" == *"loop-check: no manifest names session session-a; visitor allowed"* ]]; then
    pass "target shim emits the visitor-allow marker"
else
    fail "target visitor outcome missing (rc=$TARGET_RC stderr=$TARGET_STDERR)"
fi

run_target 2
if [[ "$TARGET_RC" -eq 2 && "$TARGET_STDERR" == *"checker unavailable"* ]]; then
    pass "target resolver failure takes the bounded-block path"
else
    fail "target resolver failure was not bounded (rc=$TARGET_RC stderr=$TARGET_STDERR)"
fi

run_agy 0
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "agy shim sends session A's manifest to loop-check from worktree B"
else
    fail "agy shim did not select A (stdout=$AGY_STDOUT state=$(cat "$TMP/state-record" 2>/dev/null) stderr=$AGY_STDERR)"
fi

run_agy 1
if [[ "$AGY_STDOUT" == "{}" && "$AGY_STDERR" == *"loop-check: no manifest names session session-a; visitor allowed"* ]]; then
    pass "agy shim emits the visitor-allow marker"
else
    fail "agy visitor outcome missing (stdout=$AGY_STDOUT stderr=$AGY_STDERR)"
fi

run_agy 2
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$AGY_STDERR" == *"checker unavailable"* ]]; then
    pass "agy resolver failure takes the bounded-continue path"
else
    fail "agy resolver failure was not bounded (stdout=$AGY_STDOUT stderr=$AGY_STDERR)"
fi

CODEX_TRANSCRIPT="$TMP/rollout-2026-08-25T21-00-00-session-a.jsonl"
printf '{"message":{"role":"assistant","content":"working"}}\n' > "$CODEX_TRANSCRIPT"
rm -f "$TMP/resolver-id" "$TMP/state-record"
CODEX_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" CODEX_THREAD_ID="session-a" FNO_AGENTS_BIN="$STUB" \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" RESOLVER_RC=0 \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$CODEX_TRANSCRIPT\"}"
) >/dev/null 2>/dev/null || CODEX_RC=$?
if [[ "$CODEX_RC" -eq 2 && "$(cat "$TMP/resolver-id" 2>/dev/null)" == "session-a" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "target shim resolves a Codex rollout basename by bare thread id"
else
    fail "target shim sent the prefixed Codex basename (rc=$CODEX_RC resolver=$(cat "$TMP/resolver-id" 2>/dev/null))"
fi

mv "$TMP/b/.fno/target-state.md" "$TMP/b/.fno/target-state.md.bak"
run_target 0
if [[ "$TARGET_RC" -eq 2 && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "target shim finds A when worktree B has no cwd manifest"
else
    fail "target shim missed A without a cwd manifest (rc=$TARGET_RC state=$(cat "$TMP/state-record" 2>/dev/null))"
fi

run_agy 0
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "agy shim finds A when worktree B has no cwd manifest"
else
    fail "agy shim missed A without a cwd manifest (stdout=$AGY_STDOUT state=$(cat "$TMP/state-record" 2>/dev/null))"
fi

run_target 1
if [[ "$TARGET_RC" -eq 0 && "$TARGET_STDERR" == *"visitor allowed"* ]]; then
    pass "target shim names a visitor when no cwd manifest exists"
else
    fail "target no-cwd visitor outcome missing (rc=$TARGET_RC stderr=$TARGET_STDERR)"
fi

run_agy 1
if [[ "$AGY_STDOUT" == "{}" && "$AGY_STDERR" == *"visitor allowed"* ]]; then
    pass "agy shim names a visitor when no cwd manifest exists"
else
    fail "agy no-cwd visitor outcome missing (stdout=$AGY_STDOUT stderr=$AGY_STDERR)"
fi
mv "$TMP/b/.fno/target-state.md.bak" "$TMP/b/.fno/target-state.md"

printf '[session-manifest] Results: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
