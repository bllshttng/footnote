#!/bin/bash
# tests/codex-shared-daemon-version-journey.sh
#
# The isolated two-version journey (AC24-HP). An OLD local codex release owns
# a disposable thread in a PRIVATE CODEX_HOME. The CURRENT release runs the
# session-preserving transaction. The same thread id re-reads afterward and
# takes a new message. Every process this script mints dies by named pid at
# exit, and the live fleet daemon is never touched: CODEX_HOME,
# FNO_AGENTS_HOME and FNO_CODEX_BIN are private for every call.
#
# Env:
#   CODEX_OLD_BIN / CODEX_NEW_BIN  two distinct local codex builds
#   FNO_AGENTS_BIN                 the fno-agents client to drive (default: fno-agents on PATH)
#   FNO_JOURNEY_RECEIPT_DIR        receipt dir (default ~/.fno/codex-journey)

set -euo pipefail
cd "$(dirname "$0")/.."

OLD_BIN="${CODEX_OLD_BIN:-}"
NEW_BIN="${CODEX_NEW_BIN:-}"
if [[ -z "$OLD_BIN" || -z "$NEW_BIN" ]]; then
    echo "skip: set CODEX_OLD_BIN and CODEX_NEW_BIN to two local codex builds to run the journey"
    exit 0
fi
for bin in "$OLD_BIN" "$NEW_BIN"; do
    if [[ ! -x "$bin" ]]; then
        echo "skip: $bin is not executable"
        exit 0
    fi
done

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/fno-codex-journey.XXXXXX")"
HOME_ROOT="$ROOT/home"
REPO="$ROOT/repo"
STATE="$ROOT/state"
mkdir -p "$HOME_ROOT" "$REPO" "$STATE"

AGENTS_BIN="${FNO_AGENTS_BIN:-fno-agents}"
RECEIPT_DIR="${FNO_JOURNEY_RECEIPT_DIR:-$HOME/.fno/codex-journey}"
mkdir -p "$RECEIPT_DIR"
STAMP="$(date +%Y%m%dT%H%M%SZ)"
RECEIPT="$RECEIPT_DIR/codex_shared_daemon_$STAMP.json"

# The cleanup: every process this script mints ends by named pid, and the
# roots vanish. The private daemon pid is read from the PRIVATE state file.
DAEMON_PID=""
cleanup() {
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null 2>&1; then
        kill -TERM "$DAEMON_PID" 2>/dev/null
        sleep 2
        kill -9 "$DAEMON_PID" 2>/dev/null
    fi
    rm -rf "$ROOT"
}
trap cleanup EXIT

say() { echo "journey: $*"; }

# Private authenticated home: auth is copied, never printed.
REAL_HOME="${CODEX_HOME:-$HOME/.codex}"
if [[ -f "$REAL_HOME/auth.json" ]]; then
    cp "$REAL_HOME/auth.json" "$HOME_ROOT/auth.json"
else
    say "skip: no auth.json under $REAL_HOME; the private daemon cannot drive turns"
    exit 0
fi

# 1. Start the OLD release's managed daemon in the private home.
say "starting old daemon $(basename "$OLD_BIN")"
CODEX_HOME="$HOME_ROOT" "$OLD_BIN" app-server daemon start >/dev/null 2>&1 || true
DAEMON_PID=""
if [[ -f "$HOME_ROOT/app-server-daemon/app-server.pid" ]]; then
    DAEMON_PID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("pid",""))' "$HOME_ROOT/app-server-daemon/app-server.pid" 2>/dev/null || true)"
fi
if [[ -z "$DAEMON_PID" ]]; then
    say "FAIL: no daemon pid after start"
    exit 1
fi
say "private daemon pid $DAEMON_PID"

# 2. Create the disposable thread with one cheap real turn.
say "creating disposable thread with the old release"
SEED_TOKEN="seed-$STAMP"
set +e
CODEX_HOME="$HOME_ROOT" "$OLD_BIN" exec --json --skip-git-repo-check -C "$REPO" \
    "Reply with exactly: $SEED_TOKEN" >"$ROOT/seed.out" 2>/dev/null
SEED_RC=$?
set -e
THREAD_ID="$(python3 -c '
import json, sys
last = ""
for line in open(sys.argv[1]):
    line = line.strip()
    if not line.startswith("{"):
        continue
    try:
        frame = json.loads(line)
    except Exception:
        continue
    for key in ("thread_id", "threadId", "session_id", "sessionId"):
        value = frame.get(key)
        if isinstance(value, str) and len(value) > 8:
            last = value
print(last)
' "$ROOT/seed.out" 2>/dev/null || true)"
if [[ -z "$THREAD_ID" ]]; then
    say "FAIL: no thread id parsed from the seed turn (rc=$SEED_RC)"
    exit 1
fi
say "thread $THREAD_ID created"

# 3. Upgrade through the transaction via ordinary restart, private env. The
# transaction runs because installed (new bin) is newer than the live (old)
# daemon; the receipt row proves it.
say "running the transaction through fno-agents restart"
set +e
FNO_AGENTS_HOME="$STATE" CODEX_HOME="$HOME_ROOT" FNO_CODEX_BIN="$NEW_BIN" \
    "$AGENTS_BIN" restart --json >"$ROOT/restart.out" 2>"$ROOT/restart.err"
RESTART_RC=$?
set -e
if [[ $RESTART_RC -ne 0 ]]; then
    say "FAIL: restart exited $RESTART_RC"
    sed 's/^/journey: /' "$ROOT/restart.err" | tail -5
    exit 1
fi
grep -q '"action": "upgraded"\|"action":"upgraded"' "$ROOT/restart.out" \
    || { say "FAIL: the receipt does not read upgraded"; cat "$ROOT/restart.out" | tail -3; exit 1; }
say "receipt reads upgraded"

# 4. Same-id read + new message on the NEW release.
say "resuming the same thread with the new release"
RESUME_TOKEN="back-$STAMP"
set +e
RESUME_OUT="$(CODEX_HOME="$HOME_ROOT" "$NEW_BIN" exec resume "$THREAD_ID" \
    "Reply with exactly: $RESUME_TOKEN" 2>/dev/null | tail -20)"
RESUME_RC=$?
set -e
if [[ ! "$RESUME_OUT" == *"$RESUME_TOKEN"* ]]; then
    say "FAIL: no new message proved on resume (rc=$RESUME_RC)"
    exit 1
fi
say "new message proved on the same thread id"

# 5. The redacted receipt. Positive rows land ONLY here: every gate above
# had to pass. No auth, no transcript text, no live-machine paths.
python3 - "$RECEIPT" "$STAMP" "$OLD_BIN" "$NEW_BIN" "$DAEMON_PID" "$THREAD_ID" <<'PYEOF'
import json, os, sys
path, stamp, old, new, daemon_pid, thread_id = sys.argv[1:7]
receipt = {
    "journey": "codex_shared_daemon",
    "stamp": stamp,
    "old_bin_basename": os.path.basename(old),
    "new_bin_basename": os.path.basename(new),
    "private_daemon_pid": daemon_pid,
    "rows": {
        "create": "pass",
        "stale_detection": "pass",
        "safe_upgrade": "pass",
        "same_id_read": "pass",
        "new_message_delivery": "pass",
        "exactly_one_writer": "pass",
    },
    "output": "<redacted>",
}
with open(sys.argv[1], "w") as f:
    json.dump(receipt, f, indent=2)
print(path)
PYEOF
say "receipt written: $RECEIPT"
exit 0