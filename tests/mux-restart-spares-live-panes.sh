#!/usr/bin/env bash
set -euo pipefail

# This probe covers the mux-side identity claim. Thread PTY survival is already
# exercised by the daemon client tests in crates/fno-agents/src/client.rs.
# Every mux assertion below names the session, child pid, and child start time.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MUX_BIN="${FNO_MUX_BIN:-$REPO_ROOT/crates/fno/target/debug/fno}"
if [[ ! -x "$MUX_BIN" ]]; then
    if [[ -n "${FNO_MUX_BIN:-}" ]]; then
        echo "FAIL: explicit FNO_MUX_BIN is not executable: $MUX_BIN" >&2
        exit 1
    fi
    echo "[setup] building mux binary: crates/fno" >&2
    cargo build --manifest-path "$REPO_ROOT/crates/fno/Cargo.toml" --bin fno
fi
if [[ ! -x "$MUX_BIN" ]]; then
    echo "FAIL: mux binary is still unavailable after the default build: $MUX_BIN" >&2
    exit 1
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fm.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
CARGO_HOME_TEST="$TMP_DIR/cargo"
mkdir -p "$MUX_DIR" "$CARGO_HOME_TEST/bin"
ln -s "$MUX_BIN" "$CARGO_HOME_TEST/bin/fno"
ln -s /usr/bin/true "$CARGO_HOME_TEST/bin/fno-agents"

export FNO_MUX_DIR="$MUX_DIR"
export FNO_BIN="$MUX_BIN"
export CARGO_HOME="$CARGO_HOME_TEST"
export FNO_AGENTS_BIN="/usr/bin/true"
export PATH="$CARGO_HOME_TEST/bin:$PATH"
SESSION="f2ae-$$"
export SESSION
SERVER_PID=""

cleanup() {
    "$MUX_BIN" mux kill-server "$SESSION" >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

"$MUX_BIN" mux server --session "$SESSION" >"$TMP_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in {1..100}; do if "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); sys.exit(0 if any(r.get("session")==os.environ["SESSION"] and r.get("state")=="live" for r in rows) else 1)'; then break; fi; sleep 0.05; done

if ! "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); sys.exit(0 if any(r.get("session")==os.environ["SESSION"] and r.get("state")=="live" for r in rows) else 1)'; then
    echo "FAIL: scratch mux server never became live" >&2
    exit 1
fi

"$MUX_BIN" mux pane run --session "$SESSION" --json -- sleep 600 >"$TMP_DIR/pane.json"
CHILD_PID="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c 'import json,sys; rows=json.load(sys.stdin); pids=[r.get("child_pid") for r in rows if r.get("child_pid")]; (len(pids)==1) or (_ for _ in ()).throw(SystemExit(f"expected one named pane child pid, got {pids!r}")); print(pids[0])')"
CHILD_START="$(ps -o lstart= -p "$CHILD_PID" | tr -s '[:space:]' ' ' | sed -e 's/^ //' -e 's/ $//')"
if [[ -z "$CHILD_START" ]]; then
    echo "FAIL: pane child pid $CHILD_PID has no positive start-time marker" >&2
    exit 1
fi

SIDECAR="$MUX_DIR/$SESSION.ver"
printf '57\n' >"$SIDECAR"
RESTART_RC=0
RESTART_OUTPUT="$(uv run --project "$REPO_ROOT/cli" fno-py agents restart --no-revive --json 2>&1)" || RESTART_RC=$?
printf '%s\n' "$RESTART_OUTPUT"
# A spared stale-wire server is a reported FAILURE, not success: the command
# must exit 1 so exit-code automation sees the fleet is still skewed (the
# same contract the wedged rows already had), while the pane child survives.
if [[ "$RESTART_RC" -ne 1 ]]; then
    echo "FAIL: restart rc=$RESTART_RC; a spared stale-wire server must exit 1" >&2
    exit 1
fi
if ! grep -F "mux session '$SESSION'" <<<"$RESTART_OUTPUT" | grep -F 'spared' >/dev/null; then
    echo "FAIL: restart output did not name the spared session" >&2
    exit 1
fi

if ! "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); r=next((r for r in rows if r.get("session")==os.environ["SESSION"]),None); sys.exit(0 if r and r.get("state")=="live" and r.get("stale") is True else 1)'; then
    echo "FAIL: named session was not live and below-floor after restart" >&2
    exit 1
fi

CHILD_PID_AFTER="$(ps -o pid= -p "$CHILD_PID" | tr -d '[:space:]')"
CHILD_START_AFTER="$(ps -o lstart= -p "$CHILD_PID" | tr -s '[:space:]' ' ' | sed -e 's/^ //' -e 's/ $//')"
if [[ "$CHILD_PID_AFTER" != "$CHILD_PID" ]]; then
    echo "FAIL: pane child pid changed: before=$CHILD_PID after=$CHILD_PID_AFTER" >&2
    exit 1
fi
if [[ "$CHILD_START_AFTER" != "$CHILD_START" ]]; then
    echo "FAIL: pane child start time changed: before=$CHILD_START after=$CHILD_START_AFTER" >&2
    exit 1
fi

echo "PASS: $SESSION spared; child pid=$CHILD_PID start=$CHILD_START unchanged"
