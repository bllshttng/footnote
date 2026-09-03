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

mkdir -p "$TMP/b" "$TMP/home/.fno"
git -C "$TMP/b" init -q
git -C "$TMP/b" config user.email test@example.com
git -C "$TMP/b" config user.name Test
printf 'seed\n' > "$TMP/b/seed"
git -C "$TMP/b" add seed
git -C "$TMP/b" commit -qm seed
git -C "$TMP/b" worktree add -q "$TMP/a" -b session-a-worktree
mkdir -p "$TMP/a/.fno" "$TMP/b/.fno"
EXPECTED_A="$(cd "$TMP/a" && pwd -P)"
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
      elif [[ "$1" == "--cwd" ]]; then
        printf '%s\n' "$2" > "$CWD_RECORD"
        shift 2
      else
        shift
      fi
    done
    if [[ "${LOOP_ALLOW:-0}" == "1" ]]; then
      printf '%s\n' '{"decision":"allow","termination_reason":"DonePRGreen","message":"selected owner","fires":1,"fingerprint":"session"}'
    else
      printf '%s\n' '{"decision":"block","termination_reason":null,"message":"selected owner","fires":1,"fingerprint":"session"}'
    fi
    ;;
  finalize)
    shift
    while [[ $# -gt 0 ]]; do
      if [[ "$1" == "--cwd" ]]; then
        printf '%s\n' "$2" > "$FINALIZE_CWD_RECORD"
        shift 2
      else
        shift
      fi
    done
    exit 0
    ;;
  *) exit 2 ;;
esac
STUB
chmod +x "$STUB"

run_target() {
    local resolver_rc="$1" stderr_file="$TMP/target.stderr"
    : > "$stderr_file"
    rm -f "$TMP/state-record" "$TMP/cwd-record"
    TARGET_RC=0
    (
        cd "$TMP/b" || exit 1
        env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= SELECTED_STATE="$TMP/a/.fno/target-state.md" \
            RESOLVER_RC="$resolver_rc" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
            bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$TMP/session-a.jsonl\"}"
    ) >/dev/null 2>"$stderr_file" || TARGET_RC=$?
    TARGET_STDERR="$(cat "$stderr_file")"
}

run_agy() {
    local resolver_rc="$1" stderr_file="$TMP/agy.stderr" stdout_file="$TMP/agy.stdout"
    : > "$stderr_file"
    : > "$stdout_file"
    rm -f "$TMP/state-record" "$TMP/cwd-record"
    # No rc capture: the agy adapter is JSON-only on stdout and has no exit-2
    # path, so its exit code carries no verdict. Every caller below asserts on
    # AGY_STDOUT. A captured code nothing reads is the shape that let a test
    # certify a resolve while the gate behind it was still off.
    (
        cd "$TMP/b" || exit 1
        env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= SELECTED_STATE="$TMP/a/.fno/target-state.md" \
            RESOLVER_RC="$resolver_rc" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
            bash "$AGY_HOOK" <<< "{\"conversationId\":\"session-a\",\"transcriptPath\":\"$TMP/session-a.jsonl\",\"workspacePaths\":[\"$TMP/b\"],\"fullyIdle\":true}"
    ) >"$stdout_file" 2>"$stderr_file" || true
    AGY_STDOUT="$(cat "$stdout_file")"
    AGY_STDERR="$(cat "$stderr_file")"
}

run_target 0
if [[ "$TARGET_RC" -eq 2 && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" \
    && "$(cat "$TMP/cwd-record" 2>/dev/null)" == "$EXPECTED_A" ]]; then
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

# x-8d1d: a joiner spawned by `fno backlog join` shares the holder's worktree
# but owns no manifest - its session id matches none, the target session's
# manifest stays selected for its own session, and the joiner's stop still
# allows the visitor out with exit 0.
printf '{"message":{"role":"assistant","content":"working"}}\n' > "$TMP/joiner-j-x-8d1d-1.jsonl"
JOINER_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= SELECTED_STATE="$TMP/a/.fno/target-state.md" \
        RESOLVER_RC=1 STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$TMP/joiner-j-x-8d1d-1.jsonl\"}"
) >/dev/null 2>"$TMP/joiner.stderr" || JOINER_RC=$?
JOINER_STDERR="$(cat "$TMP/joiner.stderr")"
if [[ "$JOINER_RC" -eq 0 && "$JOINER_STDERR" == *"no manifest names session joiner-j-x-8d1d-1; visitor allowed"* ]]; then
    pass "joiner session matching no manifest stops as a visitor (x-8d1d)"
else
    fail "joiner visitor outcome missing (rc=$JOINER_RC stderr=$JOINER_STDERR)"
fi

run_target 2
if [[ "$TARGET_RC" -eq 2 && "$TARGET_STDERR" == *"checker unavailable"* ]]; then
    pass "target resolver failure takes the bounded-block path"
else
    fail "target resolver failure was not bounded (rc=$TARGET_RC stderr=$TARGET_STDERR)"
fi

run_agy 0
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" \
    && "$(cat "$TMP/cwd-record" 2>/dev/null)" == "$EXPECTED_A" ]]; then
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

rm -f "$TMP/finalize-cwd"
TARGET_FINALIZE_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" SELECTED_STATE="$TMP/a/.fno/target-state.md" \
        RESOLVER_RC=0 STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        LOOP_ALLOW=1 FINALIZE_CWD_RECORD="$TMP/finalize-cwd" \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$TMP/session-a.jsonl\"}"
) >/dev/null 2>/dev/null || TARGET_FINALIZE_RC=$?
if [[ "$TARGET_FINALIZE_RC" -eq 0 && "$(cat "$TMP/finalize-cwd" 2>/dev/null)" == "$EXPECTED_A" ]]; then
    pass "target finalize uses the selected manifest worktree"
else
    fail "target finalize mixed the resident cwd (rc=$TARGET_FINALIZE_RC cwd=$(cat "$TMP/finalize-cwd" 2>/dev/null))"
fi

rm -f "$TMP/finalize-cwd"
AGY_FINALIZE_OUT=$(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" SELECTED_STATE="$TMP/a/.fno/target-state.md" \
        RESOLVER_RC=0 STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        LOOP_ALLOW=1 FINALIZE_CWD_RECORD="$TMP/finalize-cwd" \
        bash "$AGY_HOOK" <<< "{\"conversationId\":\"session-a\",\"transcriptPath\":\"$TMP/session-a.jsonl\",\"workspacePaths\":[\"$TMP/b\"],\"fullyIdle\":true}"
) 2>/dev/null
if [[ "$AGY_FINALIZE_OUT" == "{}" && "$(cat "$TMP/finalize-cwd" 2>/dev/null)" == "$EXPECTED_A" ]]; then
    pass "agy finalize uses the selected manifest worktree"
else
    fail "agy finalize mixed the resident cwd (stdout=$AGY_FINALIZE_OUT cwd=$(cat "$TMP/finalize-cwd" 2>/dev/null))"
fi

CODEX_TRANSCRIPT="$TMP/rollout-2026-08-25T21-00-00-session-a.jsonl"
printf '{"message":{"role":"assistant","content":"working"}}\n' > "$CODEX_TRANSCRIPT"
rm -f "$TMP/resolver-id" "$TMP/state-record"
CODEX_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" CODEX_THREAD_ID="session-a" FNO_AGENTS_BIN="$STUB" \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" RESOLVER_RC=0 \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$CODEX_TRANSCRIPT\"}"
) >/dev/null 2>/dev/null || CODEX_RC=$?
if [[ "$CODEX_RC" -eq 2 && "$(cat "$TMP/resolver-id" 2>/dev/null)" == "session-a" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "target shim resolves a Codex rollout basename by bare thread id"
else
    fail "target shim sent the prefixed Codex basename (rc=$CODEX_RC resolver=$(cat "$TMP/resolver-id" 2>/dev/null))"
fi

# Codex WITHOUT CODEX_THREAD_ID. The hook runner calls `env_clear()` and replays
# only the session snapshot plus the hook's own declared env, and fno declares
# none - so the var the test above supplies is absent in production. The uuid is
# still in the payload twice: codex sends `session_id` (StopCommandInput) and the
# rollout basename carries it as a suffix. Measured 2026-09-02: without this the
# resolver got `rollout-<utc>-<uuid>`, missed, and the hook took the silent-allow
# path - 1666 claude loop_check events against 2 codex over six days.
rm -f "$TMP/resolver-id" "$TMP/state-record"
CODEX_NOENV_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" RESOLVER_RC=0 \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"session_id\":\"session-a\",\"transcript_path\":\"$CODEX_TRANSCRIPT\"}"
) >/dev/null 2>/dev/null || CODEX_NOENV_RC=$?
if [[ "$CODEX_NOENV_RC" -eq 2 && "$(cat "$TMP/resolver-id" 2>/dev/null)" == "session-a" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "target shim resolves a Codex rollout with no CODEX_THREAD_ID in env"
else
    fail "target shim silently allowed with no CODEX_THREAD_ID (rc=$CODEX_NOENV_RC resolver=$(cat "$TMP/resolver-id" 2>/dev/null))"
fi

# Same session, transcript_path absent. `StopCommandInput.transcript_path` is
# NullableString, so the payload `session_id` has to carry the resolve on its own.
rm -f "$TMP/resolver-id" "$TMP/state-record"
CODEX_NOPATH_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" RESOLVER_RC=0 \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"session_id\":\"session-a\",\"transcript_path\":null}"
) >/dev/null 2>/dev/null || CODEX_NOPATH_RC=$?
# rc 2 is the assertion, not incidental: with the manifest resolved and no
# transcript to hand `loop-check --transcript`, the hook MUST take the bounded
# unavailable-block path rather than allow. Asserting the resolver id alone
# would pass on a run where the session resolved and the gate was still off.
if [[ "$(cat "$TMP/resolver-id" 2>/dev/null)" == "session-a" && "$CODEX_NOPATH_RC" -eq 2 ]]; then
    pass "target shim resolves by payload session_id and blocks with no transcript"
else
    fail "target shim lost the session or allowed with a null transcript_path (rc=$CODEX_NOPATH_RC resolver=$(cat "$TMP/resolver-id" 2>/dev/null))"
fi

# No payload session_id and no CODEX_THREAD_ID: the uuid has to come off the
# rollout basename by shape. The ladder is what keeps the gate armed if the
# payload key is ever absent or renamed.
UUID_TRANSCRIPT="$TMP/rollout-2026-08-25T21-00-00-01a06212-1e0e-79a3-a4c0-5af187a5cbfc.jsonl"
printf '{"message":{"role":"assistant","content":"working"}}\n' > "$UUID_TRANSCRIPT"
rm -f "$TMP/resolver-id" "$TMP/state-record"
CODEX_SHAPE_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$STUB" \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" RESOLVER_RC=0 \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$UUID_TRANSCRIPT\"}"
) >/dev/null 2>/dev/null || CODEX_SHAPE_RC=$?
if [[ "$(cat "$TMP/resolver-id" 2>/dev/null)" == "01a06212-1e0e-79a3-a4c0-5af187a5cbfc" ]]; then
    pass "target shim strips the uuid off a rollout basename with no id anywhere else"
else
    fail "target shim kept the rollout prefix (rc=$CODEX_SHAPE_RC resolver=$(cat "$TMP/resolver-id" 2>/dev/null))"
fi

# The resolver is a LADDER, not one shot. The first candidate misses (rc 1) and
# the next one hits. A one-shot resolve reads that first miss as "no manifest
# names this session" and takes the silent allow this PR exists to remove.
rm -f "$TMP/resolver-id" "$TMP/state-record"
LADDER_STUB="$TMP/fno-agents-ladder"
cat > "$LADDER_STUB" <<'LADDER'
#!/usr/bin/env bash
case "$1" in
  manifest-for-session)
    printf '%s\n' "$3" >> "$RESOLVER_ID_RECORD"
    if [[ "$3" == "session-a" ]]; then printf '%s\n' "$SELECTED_STATE"; exit 0; fi
    exit 1
    ;;
  loop-check) printf '%s\n' '{"decision":"block","message":"keep going"}'; exit 0 ;;
  *) exit 0 ;;
esac
LADDER
chmod +x "$LADDER_STUB"
LADDER_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" FNO_AGENTS_BIN="$LADDER_STUB" \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= CODEX_THREAD_ID="session-a" \
        SELECTED_STATE="$TMP/a/.fno/target-state.md" \
        RESOLVER_ID_RECORD="$TMP/resolver-id" STATE_RECORD="$TMP/state-record" CWD_RECORD="$TMP/cwd-record" \
        bash "$TARGET_HOOK" <<< "{\"session_id\":\"not-the-stamped-id\",\"transcript_path\":\"$CODEX_TRANSCRIPT\"}"
) >/dev/null 2>/dev/null || LADDER_RC=$?
if grep -qx "not-the-stamped-id" "$TMP/resolver-id" 2>/dev/null \
    && grep -qx "session-a" "$TMP/resolver-id" 2>/dev/null; then
    pass "target shim falls past a missed payload id to the next candidate"
else
    fail "target shim stopped at the first miss (rc=$LADDER_RC tried=$(tr '\n' ',' < "$TMP/resolver-id" 2>/dev/null))"
fi

cp "$TMP/b/.fno/target-state.md" "$TMP/b/.fno/target-state.md.full"
cat > "$TMP/b/.fno/target-state.md" <<STATE
---
fno_id: run-b
session_id: run-b
claude_session_id: session-b
owner_cwd: "$TMP/b"
---
STATE
run_agy 0
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" ]]; then
    pass "agy legacy Claude manifest still resolves by session"
else
    fail "agy borrowed a legacy Claude resident manifest (stdout=$AGY_STDOUT state=$(cat "$TMP/state-record" 2>/dev/null))"
fi
mv "$TMP/b/.fno/target-state.md.full" "$TMP/b/.fno/target-state.md"

mv "$TMP/b/.fno/target-state.md" "$TMP/b/.fno/target-state.md.bak"
run_target 0
if [[ "$TARGET_RC" -eq 2 && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" \
    && "$(cat "$TMP/cwd-record" 2>/dev/null)" == "$EXPECTED_A" ]]; then
    pass "target shim finds A when worktree B has no cwd manifest"
else
    fail "target shim missed A without a cwd manifest (rc=$TARGET_RC state=$(cat "$TMP/state-record" 2>/dev/null))"
fi

run_agy 0
if [[ "$(printf '%s' "$AGY_STDOUT" | jq -r '.decision // empty')" == "continue" \
    && "$(cat "$TMP/state-record" 2>/dev/null)" == "$TMP/a/.fno/target-state.md" \
    && "$(cat "$TMP/cwd-record" 2>/dev/null)" == "$EXPECTED_A" ]]; then
    pass "agy shim finds A when worktree B has no cwd manifest"
else
    fail "agy shim missed A without a cwd manifest (stdout=$AGY_STDOUT state=$(cat "$TMP/state-record" 2>/dev/null))"
fi

NO_BIN_PATH="/usr/bin:/bin:/usr/sbin:/sbin"
MISSING_TARGET_RC=0
MISSING_TARGET_STDERR=$(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" PATH="$NO_BIN_PATH" FNO_AGENTS_BIN=/nonexistent \
        CLAUDECODE=0 CLAUDE_PLUGIN_ROOT= \
        bash "$TARGET_HOOK" <<< "{\"transcript_path\":\"$TMP/session-a.jsonl\"}" 2>&1 >/dev/null
) || MISSING_TARGET_RC=$?
if [[ "$MISSING_TARGET_RC" -eq 2 && "$MISSING_TARGET_STDERR" == *"checker unavailable"* ]]; then
    pass "target no-cwd resolver absence takes the bounded-block path"
else
    fail "target no-cwd resolver absence failed open (rc=$MISSING_TARGET_RC stderr=$MISSING_TARGET_STDERR)"
fi

MISSING_AGY_OUT="$TMP/missing-agy.out"
MISSING_AGY_ERR="$TMP/missing-agy.err"
MISSING_AGY_RC=0
(
    cd "$TMP/b" || exit 1
    env HOME="$TMP/home" PATH="$NO_BIN_PATH" FNO_AGENTS_BIN=/nonexistent \
        bash "$AGY_HOOK" <<< "{\"conversationId\":\"session-a\",\"transcriptPath\":\"$TMP/session-a.jsonl\",\"workspacePaths\":[\"$TMP/b\"],\"fullyIdle\":true}"
) >"$MISSING_AGY_OUT" 2>"$MISSING_AGY_ERR" || MISSING_AGY_RC=$?
if [[ "$(jq -r '.decision // empty' "$MISSING_AGY_OUT" 2>/dev/null)" == "continue" \
    && "$(cat "$MISSING_AGY_ERR")" == *"checker unavailable"* ]]; then
    pass "agy no-cwd resolver absence takes the bounded-continue path"
else
    fail "agy no-cwd resolver absence failed open (rc=$MISSING_AGY_RC stdout=$(cat "$MISSING_AGY_OUT") stderr=$(cat "$MISSING_AGY_ERR"))"
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
