#!/usr/bin/env bash
# Advisory-only worktree peer detection for both SessionStart carriers.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELPER="$REPO_ROOT/hooks/helpers/worktree-live-peers.sh"
CARRIER="$REPO_ROOT/hooks/worktree-peers-session-start.sh"
CODEX_WRAPPER="$REPO_ROOT/hooks/session-start.sh"
HEARTBEAT="$REPO_ROOT/hooks/claim-heartbeat.sh"
PASS=0
FAIL=0

pass() { PASS=$((PASS + 1)); printf '[worktree-peers] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf '[worktree-peers] FAIL: %s\n' "$*" >&2; }

if [[ ! -f "$HELPER" || ! -f "$CARRIER" ]]; then
  fail "helper and SessionStart carrier must exist"
  exit 1
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT
PROJECT="$TMP_DIR/project"
git init -q "$PROJECT"
PROJECT_GIT_DIR="$(git -C "$PROJECT" rev-parse --absolute-git-dir)"
LIVE_DIR="$PROJECT_GIT_DIR/fno/live"
mkdir -p "$LIVE_DIR"

run_helper() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" bash "$HELPER"
}

run_carrier() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" bash "$CARRIER"
}

reset_live() { rm -rf "$LIVE_DIR"; mkdir -p "$LIVE_DIR"; }

# The production defaults are a coupled contract: the writer must refresh
# before the reader can classify an active session as stale.
writer_throttle="$(sed -n 's/^LIVE_THROTTLE=\([0-9][0-9]*\)$/\1/p' "$HEARTBEAT")"
reader_window="$(sed -n 's/^WINDOW=\([0-9][0-9]*\)$/\1/p' "$HELPER")"
if [[ "$writer_throttle" == 30 && "$reader_window" == 120 ]] \
    && (( writer_throttle < reader_window )); then
  pass "production defaults keep the writer throttle inside the reader window"
else
  fail "default coupling: writer=[$writer_throttle] reader=[$reader_window]"
fi

# AC1-HP: another fresh session produces the advisory.
touch "$LIVE_DIR/peer-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* \
      && "$out" == *"fno-overlap-observed"* ]]; then
  pass "fresh peer stamp emits the advisory"
else
  fail "fresh peer stamp: rc=$rc out=[$out]"
fi

# AC2-HP: a session never warns about its own stamp.
reset_live
touch "$LIVE_DIR/self-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "self-only stamp is silent"
else
  fail "self-only stamp: rc=$rc out=[$out]"
fi

# Exercise the production writer-to-reader boundary without fabricating a stamp.
reset_live
printf '{"cwd":"%s","session_id":"peer-session","tool_name":"Edit"}' "$PROJECT" \
  | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"fno-overlap-observed"* ]]; then
  pass "PostToolUse activity is consumed by the SessionStart reader"
else
  fail "writer-to-reader journey: rc=$rc out=[$out]"
fi

# The same journey must use Codex's thread identity as the stamp name.
reset_live
printf '{"cwd":"%s","session_id":"ignored","tool_name":"Edit"}' "$PROJECT" \
  | FNO_PLATFORM=codex CODEX_THREAD_ID=peer-codex bash "$HEARTBEAT" >/dev/null 2>&1
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -f "$LIVE_DIR/peer-codex" \
      && "$out" == *"fno-overlap-observed"* ]]; then
  pass "Codex PostToolUse activity is consumed by the shared reader"
else
  fail "Codex writer-to-reader journey: rc=$rc out=[$out]"
fi

# A whole-directory .fno symlink must not leak activity across worktrees.
REPO="$TMP_DIR/repo"
PEER_WORKTREE="$TMP_DIR/peer-worktree"
SHARED_FNO="$TMP_DIR/shared-fno"
git init -q "$REPO"
git -C "$REPO" config user.email test@example.com
git -C "$REPO" config user.name Test
touch "$REPO/tracked"
git -C "$REPO" add tracked
git -C "$REPO" commit -qm init
git -C "$REPO" worktree add -q "$PEER_WORKTREE"
mkdir -p "$SHARED_FNO"
ln -s "$SHARED_FNO" "$REPO/.fno"
ln -s "$SHARED_FNO" "$PEER_WORKTREE/.fno"
ambient_dir="$(GIT_DIR="$(git -C "$REPO" rev-parse --absolute-git-dir)" \
  GIT_WORK_TREE="$REPO" bash "$HELPER" --live-dir "$PROJECT")"
if [[ "$ambient_dir" == "$LIVE_DIR" ]]; then
  pass "explicit cwd overrides ambient Git repository selection"
else
  fail "ambient Git environment redirected live dir to [$ambient_dir]"
fi
printf '{"cwd":"%s","session_id":"peer-session","tool_name":"Edit"}' "$REPO" \
  | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1
other_out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PEER_WORKTREE" \
  | CODEX_THREAD_ID= bash "$HELPER" 2>/dev/null)"
same_out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$REPO" \
  | CODEX_THREAD_ID= bash "$HELPER" 2>/dev/null)"
if [[ -z "$other_out" && "$same_out" == *"Another session is working in this worktree."* ]]; then
  pass "shared .fno state remains isolated by physical worktree"
else
  fail "shared .fno isolation: other=[$other_out] same=[$same_out]"
fi

# AC3: a stale predecessor is a handoff, regardless of harness identity.
reset_live
touch -t 202001010000 "$LIVE_DIR/prior-harness-session"
out="$(TEST_CODEX_THREAD_ID=self-session run_helper ignored-input 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "stale cross-harness handoff is silent"
else
  fail "stale handoff: rc=$rc out=[$out]"
fi

# AC4-ERR: absent and malformed live directories fail open and stay silent.
rm -rf "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "absent live directory is silent"
else
  fail "absent directory: rc=$rc out=[$out]"
fi
touch "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "malformed live path is silent"
else
  fail "malformed directory: rc=$rc out=[$out]"
fi
rm -f "$LIVE_DIR"
mkdir -p "$LIVE_DIR"
touch "$LIVE_DIR/peer-session"
chmod 000 "$LIVE_DIR"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
chmod 700 "$LIVE_DIR"
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "unreadable live directory is silent"
else
  fail "unreadable directory: rc=$rc out=[$out]"
fi

# Claude's direct carrier adds the same hygiene section around the shared note.
reset_live
touch "$LIVE_DIR/peer-session"
out="$(run_carrier self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"## Worktree hygiene"* \
      && "$out" == *"Another session is working in this worktree."* ]]; then
  pass "Claude carrier renders the shared advisory"
else
  fail "Claude carrier: rc=$rc out=[$out]"
fi

# Codex's existing wrapper calls the same helper from its hygiene block.
WRAPPER_HOME="$TMP_DIR/home"
WRAPPER_BIN="$TMP_DIR/bin"
mkdir -p "$WRAPPER_HOME/.fno" "$WRAPPER_BIN"
touch "$WRAPPER_HOME/.fno/config.toml"
printf '#!/usr/bin/env bash\nexit 1\n' >"$WRAPPER_BIN/fno"
printf '#!/usr/bin/env bash\nexit 1\n' >"$WRAPPER_BIN/uv"
chmod +x "$WRAPPER_BIN/fno" "$WRAPPER_BIN/uv"
out="$(printf '{"cwd":"%s","session_id":"self-session"}' "$PROJECT" \
  | (cd "$PROJECT" && HOME="$WRAPPER_HOME" FNO_HOME="$WRAPPER_HOME/.fno" \
      PATH="$WRAPPER_BIN:$PATH" FNO_PLATFORM=codex CODEX_THREAD_ID=self-session \
      CLAUDE_PROJECT_DIR="$PROJECT" bash "$CODEX_WRAPPER") 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* ]]; then
  pass "Codex wrapper renders the shared advisory"
else
  fail "Codex wrapper: rc=$rc out=[$out]"
fi

# AC5-CON: this mechanism is absent from PreToolUse and its PostToolUse writer
# exits 0 after Edit, Write, and Bash payloads in every stamp state.
pretool="$(jq -r '.hooks.PreToolUse[]?.hooks[]?.command // empty' \
  "$REPO_ROOT/hooks/hooks.json" "$REPO_ROOT/hooks/codex-hooks.json")"
if [[ "$pretool" == *"worktree-live-peers"* || "$pretool" == *"worktree-peers-session-start"* ]]; then
  fail "peer advisory is reachable from PreToolUse"
else
  pass "peer advisory is absent from both PreToolUse surfaces"
fi
never_refused=1
for state in absent malformed unreadable empty self fresh-peer stale-peer; do
  for tool in Edit Write Bash; do
    rm -rf "$LIVE_DIR"
    case "$state" in
      absent) ;;
      malformed) mkdir -p "${LIVE_DIR%/*}"; touch "$LIVE_DIR" ;;
      unreadable) mkdir -p "$LIVE_DIR"; chmod 000 "$LIVE_DIR" ;;
      empty) mkdir -p "$LIVE_DIR" ;;
      self) mkdir -p "$LIVE_DIR"; touch "$LIVE_DIR/self-session" ;;
      fresh-peer) mkdir -p "$LIVE_DIR"; touch "$LIVE_DIR/peer-session" ;;
      stale-peer) mkdir -p "$LIVE_DIR"; touch -t 202001010000 "$LIVE_DIR/peer-session" ;;
    esac
    printf '{"cwd":"%s","session_id":"self-session","tool_name":"%s"}' "$PROJECT" "$tool" \
      | CODEX_THREAD_ID= bash "$HEARTBEAT" >/dev/null 2>&1 || never_refused=0
    [[ "$state" == unreadable ]] && chmod 700 "$LIVE_DIR"
  done
done
if [[ "$never_refused" -eq 1 ]]; then
  pass "Edit, Write, and Bash never refuse across every stamp state"
else
  fail "an Edit, Write, or Bash activity payload returned non-zero"
fi

printf '[worktree-peers] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
