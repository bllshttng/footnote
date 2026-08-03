#!/usr/bin/env bash
# Advisory-only worktree peer detection for both SessionStart carriers.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELPER="$REPO_ROOT/hooks/helpers/worktree-live-peers.sh"
CARRIER="$REPO_ROOT/hooks/worktree-peers-session-start.sh"
CODEX_WRAPPER="$REPO_ROOT/hooks/session-start.sh"
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
mkdir -p "$PROJECT/.fno/live"

run_helper() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" \
      FNO_WORKTREE_LIVE_WINDOW_SECONDS=120 bash "$HELPER"
}

run_carrier() {
  printf '{"cwd":"%s","session_id":"%s"}' "$PROJECT" "$1" \
    | CODEX_THREAD_ID="${TEST_CODEX_THREAD_ID:-}" \
      FNO_WORKTREE_LIVE_WINDOW_SECONDS=120 bash "$CARRIER"
}

reset_live() { rm -rf "$PROJECT/.fno/live"; mkdir -p "$PROJECT/.fno/live"; }

# AC1-HP: another fresh session produces the advisory.
touch "$PROJECT/.fno/live/peer-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* ]]; then
  pass "fresh peer stamp emits the advisory"
else
  fail "fresh peer stamp: rc=$rc out=[$out]"
fi

# AC2-HP: a session never warns about its own stamp.
reset_live
touch "$PROJECT/.fno/live/self-session"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "self-only stamp is silent"
else
  fail "self-only stamp: rc=$rc out=[$out]"
fi

# AC3: a stale predecessor is a handoff, regardless of harness identity.
reset_live
touch -t 202001010000 "$PROJECT/.fno/live/prior-harness-session"
out="$(TEST_CODEX_THREAD_ID=self-session run_helper ignored-input 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "stale cross-harness handoff is silent"
else
  fail "stale handoff: rc=$rc out=[$out]"
fi

# AC4-ERR: absent and malformed live directories fail open and stay silent.
rm -rf "$PROJECT/.fno/live"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "absent live directory is silent"
else
  fail "absent directory: rc=$rc out=[$out]"
fi
touch "$PROJECT/.fno/live"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "malformed live path is silent"
else
  fail "malformed directory: rc=$rc out=[$out]"
fi
rm -f "$PROJECT/.fno/live"
mkdir -p "$PROJECT/.fno/live"
touch "$PROJECT/.fno/live/peer-session"
chmod 000 "$PROJECT/.fno/live"
out="$(run_helper self-session 2>/dev/null)"; rc=$?
chmod 700 "$PROJECT/.fno/live"
if [[ "$rc" -eq 0 && -z "$out" ]]; then
  pass "unreadable live directory is silent"
else
  fail "unreadable directory: rc=$rc out=[$out]"
fi

# Claude's direct carrier adds the same hygiene section around the shared note.
reset_live
touch "$PROJECT/.fno/live/peer-session"
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
      CLAUDE_PROJECT_DIR="$PROJECT" FNO_WORKTREE_LIVE_WINDOW_SECONDS=120 \
      bash "$CODEX_WRAPPER") 2>/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$out" == *"Another session is working in this worktree."* ]]; then
  pass "Codex wrapper renders the shared advisory"
else
  fail "Codex wrapper: rc=$rc out=[$out]"
fi

printf '[worktree-peers] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
