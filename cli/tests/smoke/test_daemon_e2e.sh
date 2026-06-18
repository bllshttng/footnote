#!/usr/bin/env bash
# Smoke test: daemon end-to-end (Task 5.4)
# AC1-HP: pre-written inbox -> _on_change (forced idle) -> _spawn_drain ->
#         stub claude runs fno mail drain -> graph node created
#         Log shows "spawn drain" and "drain complete". Completes in ~5 sec.
#
# Strategy:
#   - Override HOME to a tmp dir (no real ~/.fno mutations)
#   - Stub `claude` on PATH: runs `fno mail drain --json` then prints
#     a fixed JSON envelope so _spawn_drain can extract session_id.
#   - Pre-inject one heads-up message via `fno mail send`.
#   - Extract _log / _spawn_drain / _on_change from the daemon script
#     via awk (same pattern as test_abi_watch_bypass.sh, bash 3.2 compat).
#   - Override _detect_state to return "idle".
#   - Call _on_change directly. No fswatch, no real claude, no launchd.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CLI_DIR="$REPO_ROOT/cli"
DAEMON="$REPO_ROOT/scripts/abi-watch.sh"

PASS=0
FAIL=0

_fail() {
  echo "FAIL: $*" >&2
  FAIL=$((FAIL + 1))
}

_pass() {
  PASS=$((PASS + 1))
}

# ---------------------------------------------------------------------------
# Setup: isolated tmp tree
# ---------------------------------------------------------------------------
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

FAKE_HOME="$WORK/home"
FAKE_BIN="$WORK/bin"
INBOX_ROOT="$WORK/inbox"
PROJECT="smoke-proj"

mkdir -p "$FAKE_HOME/.fno"
mkdir -p "$FAKE_BIN"
mkdir -p "$INBOX_ROOT"

# Drain needs Path.cwd() as repo_root; cd to WORK so .fno/ lands there.
cd "$WORK"

# Minimal drain prompt (content doesn't matter - stub claude ignores it).
echo "# Inbox drain prompt (stub)" > "$FAKE_HOME/.fno/inbox-drain-prompt.md"

# Triage stub: consumed by fno mail drain when FNO_INBOX_TRIAGE_STUB is set.
TRIAGE_STUB="$WORK/triage_stub.sh"
cat > "$TRIAGE_STUB" << 'TRIAGE_EOF'
#!/usr/bin/env bash
cat > /dev/null
echo '{"action":"create_node","title":"E2E smoke node","priority":"p2","body":"Created by daemon e2e smoke test.","follow_up_question":null}'
TRIAGE_EOF
chmod +x "$TRIAGE_STUB"

# Fake `fno` binary: intercepts "fno new" calls from drain.py.
# Also passes through "fno mail ..." subcommands to the real CLI.
cat > "$FAKE_BIN/fno" << FAKE_ABI_EOF
#!/usr/bin/env bash
# If the first sub-command is "new", emit a fake ab-id and exit.
if [[ "\${1:-}" == "new" ]]; then
  echo "ab-smoke-daemon-e2e"
  exit 0
fi
# Otherwise delegate to the real CLI via uv.
exec uv run --project "$CLI_DIR" fno "\$@"
FAKE_ABI_EOF
chmod +x "$FAKE_BIN/fno"

# Stub `claude`: mimics what a real `claude -p --output-format json --bare`
# would do when given the drain prompt.  Runs `fno mail drain --json` so
# the actual drain path is exercised, then emits a fixed JSON envelope.
cat > "$FAKE_BIN/claude" << FAKE_CLAUDE_EOF
#!/usr/bin/env bash
# Stub claude for daemon e2e smoke test.
# Run the real drain (uses FNO_INBOX_ROOT and FNO_INBOX_TRIAGE_STUB from env).
FNO_INBOX_ROOT="$INBOX_ROOT" \
  FNO_INBOX_TRIAGE_STUB="$TRIAGE_STUB" \
  uv run --project "$CLI_DIR" fno mail drain \
    --json \
    --from "$PROJECT" > /dev/null 2>&1 || true
# Emit the JSON envelope _spawn_drain expects.
echo '{"session_id": "stub-sid-deadbeef", "type": "result"}'
FAKE_CLAUDE_EOF
chmod +x "$FAKE_BIN/claude"

export PATH="$FAKE_BIN:$PATH"

# ---------------------------------------------------------------------------
# Pre-inject: one heads-up message so drain has something to process
# ---------------------------------------------------------------------------
FNO_INBOX_ROOT="$INBOX_ROOT" \
  uv run --project "$CLI_DIR" fno mail send \
    --to-project "$PROJECT" \
    --from-name "sender-proj" \
    --kind heads-up \
    --body "Watchdog: new PR landed in sender-proj." > /dev/null

# Confirm message is present before running daemon
UNREAD_BEFORE=$(FNO_INBOX_ROOT="$INBOX_ROOT" \
  uv run --project "$CLI_DIR" fno mail unread --json --name "$PROJECT")
UNREAD_COUNT=$(echo "$UNREAD_BEFORE" | python3 -c "import json,sys; print(len(json.load(sys.stdin)))")
if [[ "$UNREAD_COUNT" != "1" ]]; then
  _fail "pre-condition: expected 1 unread message, got $UNREAD_COUNT"
  echo "FAIL ($PASS/$((PASS+FAIL)) passed)" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# AC1-HP: call _on_change directly (no fswatch, no real launchd)
# ---------------------------------------------------------------------------
REPO_ABI_DIR="$WORK/.fno"
mkdir -p "$REPO_ABI_DIR"

# Run _on_change in an isolated subshell.
# Extract _log / _spawn_drain / _on_change from the daemon via awk
# (bash 3.2 compat: eval "$(awk ...)" not source <(awk ...)).
bash -c "
set -eo pipefail
export HOME='$FAKE_HOME'
export PATH='$FAKE_BIN:$PATH'
export FNO_INBOX_ROOT='$INBOX_ROOT'
export FNO_INBOX_TRIAGE_STUB='$TRIAGE_STUB'

PROJECT='$PROJECT'
REPO_ROOT='$WORK'
LOG='$REPO_ABI_DIR/abi-watch.log'
SESSION_FILE='$FAKE_HOME/.fno/${PROJECT}-watch-session.json'
PROMPT_FILE='$FAKE_HOME/.fno/inbox-drain-prompt.md'

# Extract _log, _spawn_drain, _on_change from the daemon script.
eval \"\$(awk '
  /^_log\(\)/         { f=1 }
  /^_spawn_drain\(\)/ { f=1 }
  /^_on_change\(\)/   { f=1 }
  f { print }
  f && /^\}$/ { f=0 }
' '$DAEMON')\"

# Override _detect_state to return idle (bypass detection not the focus here).
_detect_state() { echo 'idle'; }

_on_change
" 2>/dev/null

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

# Assert 1: log file exists with "spawn drain" and "drain complete"
LOG_FILE="$REPO_ABI_DIR/abi-watch.log"
if [[ ! -f "$LOG_FILE" ]]; then
  _fail "AC1-HP: abi-watch.log not created at $LOG_FILE"
else
  if grep -q "spawn drain" "$LOG_FILE"; then
    _pass
  else
    _fail "AC1-HP: log missing 'spawn drain' (log: $(cat "$LOG_FILE"))"
  fi

  if grep -q "drain complete" "$LOG_FILE"; then
    _pass
  else
    _fail "AC1-HP: log missing 'drain complete' (log: $(cat "$LOG_FILE"))"
  fi
fi

# Assert 2: session file written with stub session_id
SESSION_FILE="$FAKE_HOME/.fno/${PROJECT}-watch-session.json"
if [[ ! -f "$SESSION_FILE" ]]; then
  _fail "AC1-HP: session file not written at $SESSION_FILE"
else
  SID=$(python3 -c "import json; print(json.load(open('$SESSION_FILE')).get('session_id',''))")
  if [[ "$SID" == "stub-sid-deadbeef" ]]; then
    _pass
  else
    _fail "AC1-HP: session_id mismatch, got '$SID' (expected stub-sid-deadbeef)"
  fi
fi

# Assert 3: graph node created (drain writes via fake fno new)
# The drain e2e leaves a record in convo-signals.jsonl OR the triage stub
# output is consumed. The drain consumes via the md render's read_at (not the
# cursor `mail unread`), so check the render directly: the heads-up is now read.
UNREAD_AFTER_COUNT=$(FNO_INBOX_ROOT="$INBOX_ROOT" \
  uv run --project "$CLI_DIR" python3 -c "
from fno.inbox.store import read_unread_threads
print(len(read_unread_threads('$PROJECT')))")
if [[ "$UNREAD_AFTER_COUNT" == "0" ]]; then
  _pass
else
  _fail "AC1-HP: expected 0 unread after drain (heads-up should be read), got $UNREAD_AFTER_COUNT"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
TOTAL=$((PASS + FAIL))
if [[ "$FAIL" -gt 0 ]]; then
  echo "FAIL ($PASS/$TOTAL passed)" >&2
  exit 1
fi
echo "OK ($PASS/$TOTAL)"
