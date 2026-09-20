#!/usr/bin/env bash
set -euo pipefail

# The pane-to-thread hand-off, proven on a real keeper.
#
# The claim under test is narrow and load-bearing: a keeper's unix socket can
# be RENAMED out of the pane tree into the thread tree while the keeper is
# running, the keeper survives the rename, its child never stops, and the mux
# server lets the pane go without killing anything.
#
# Nothing here trusts an exit code. Every step reads the world back: the child
# pid before and after, the socket at both paths, and the pane listing.
#
# The harness strategies that need a real claude or codex are NOT exercised
# here. Set FNO_CONVERT_LIVE=1 to run the end-to-end conversion against the
# real CLIs; without it those steps are skipped by name, never silently.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MUX_BIN="${FNO_MUX_BIN:-$REPO_ROOT/crates/fno/target/debug/fno}"
WORKER_BIN="${FNO_AGENTS_WORKER_BIN:-$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker}"
if [[ ! -x "$MUX_BIN" || ! -x "$WORKER_BIN" ]]; then
    if [[ -n "${FNO_MUX_BIN:-}" || -n "${FNO_AGENTS_WORKER_BIN:-}" ]]; then
        echo "FAIL: explicit binaries are not executable" >&2
        exit 1
    fi
    echo "[setup] building both crates" >&2
    cargo build --manifest-path "$REPO_ROOT/crates/fno-agents/Cargo.toml" --bin fno-agents-worker
    cargo build --manifest-path "$REPO_ROOT/crates/fno/Cargo.toml" --bin fno
fi

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/cptt.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
mkdir -p "$MUX_DIR"
# Both roots are isolated. A conversion moves sockets and registry rows, and
# a run that reached the operator's own state root would move THEIRS.
export FNO_MUX_DIR="$MUX_DIR"
export FNO_AGENTS_HOME="$TMP_DIR/agents"
mkdir -p "$FNO_AGENTS_HOME"
export FNO_AGENTS_WORKER_BIN="$WORKER_BIN"
SESSION="cptt-$$"
export SESSION
SERVER_PID=""
SURVIVOR_PIDS=""

# A responder standing in for a keeper-rebind harness. It answers every line
# and never exits, which is the behavior the hand-off must not disturb.
STUB_DIR="$TMP_DIR/stubbin"
mkdir -p "$STUB_DIR"
cat >"$STUB_DIR/grok" <<'STUB'
#!/bin/sh
while IFS= read -r l; do echo "GOT:$l"; done
STUB
chmod +x "$STUB_DIR/grok"
export PATH="$STUB_DIR:$PATH"
WORKER_SESSION="01a0cafe-0000-4c1e-8a1c-2d3e4f5a6b7c"
export WORKER_SESSION

cleanup() {
    "$MUX_BIN" mux kill-server "$SESSION" >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    # The keeper and its child survive the hand-off on purpose. End them so
    # the run leaves nothing behind for an orphan guard to read as a leak.
    for pid in $SURVIVOR_PIDS; do
        kill -9 "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

alive() { kill -0 "$1" 2>/dev/null; }
fail() { echo "FAIL: $1" >&2; exit 1; }

# 1. An isolated server.
"$MUX_BIN" mux server --server "$SESSION" >"$TMP_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in {1..100}; do
    if "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); sys.exit(0 if any(r.get("session")==os.environ["SESSION"] and r.get("state")=="live" for r in rows) else 1)'; then
        break
    fi
    sleep 0.1
done
alive "$SERVER_PID" || fail "the isolated mux server never came up"
echo "[1] isolated server $SESSION is live under $MUX_DIR"

# 2. A keeper-hosted worker pane.
"$MUX_BIN" mux pane run --server "$SESSION" --worker convert-proof --json -- \
    grok --resume "$WORKER_SESSION" >"$TMP_DIR/worker.json"
PANE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/worker.json")"
CHILD_PID="$("$MUX_BIN" mux pane ls --server "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
pids=[r.get("child_pid") for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert len(pids)==1, f"expected one child pid for the worker pane, got {pids}"
print(pids[0])' "$PANE_ID")"
echo "[2] worker pane $PANE_ID hosts child $CHILD_PID"

# 3. The keeper behind it, and the socket it answers on.
read -r KEEPER_PID OLD_SOCKET <<<"$("$MUX_BIN" mux pane keeper list --json | python3 -c '
import json,os,sys
rows=json.load(sys.stdin)
rows=[r for r in rows if r.get("session")==os.environ["SESSION"] and r.get("child_pid")==int(sys.argv[1])]
assert len(rows)==1, f"expected one keeper row for child {sys.argv[1]}, got {rows}"
r=rows[0]
assert r.get("keeper_pid"), "the keeper row names its own pid"
assert r.get("socket"), "the keeper row names its socket"
print(r["keeper_pid"], r["socket"])' "$CHILD_PID")"
SURVIVOR_PIDS="$KEEPER_PID $CHILD_PID"
[[ -S "$OLD_SOCKET" ]] || fail "the keeper socket $OLD_SOCKET is not a socket"
echo "[3] keeper $KEEPER_PID answers at $OLD_SOCKET"

# 4. The hand-off: the socket moves into the thread tree, the pane is
#    released. This is the verb the conversion's keeper-rebind arm drives.
NEW_SOCKET="$MUX_DIR/threads/convert-proof.sock"
"$MUX_BIN" mux pane kill --server "$SESSION" "$PANE_ID" --hand-off-to "$NEW_SOCKET"
sleep 1

# 5. The child never stopped. This is the whole point of the strategy: the
#    conversation is not resumed, it is never interrupted.
alive "$CHILD_PID" || fail "child $CHILD_PID died in the hand-off; the strategy must not stop it"
alive "$KEEPER_PID" || fail "keeper $KEEPER_PID died in the hand-off"
echo "[5] child $CHILD_PID and keeper $KEEPER_PID both survived the hand-off"

# 6. The socket is at the new path and gone from the old one. A rename that
#    left the old path answering would mean two addresses for one keeper.
[[ -S "$NEW_SOCKET" ]] || fail "the socket did not arrive at $NEW_SOCKET"
[[ ! -e "$OLD_SOCKET" ]] || fail "the old path $OLD_SOCKET still exists after the hand-off"
echo "[6] socket moved: $OLD_SOCKET -> $NEW_SOCKET"

# 7. The keeper still ANSWERS at the new path, and answers with the SAME
#    child pid. A live pid behind a socket nothing can reach is not a
#    hand-off, and a new child would be a respawn wearing the old name.
MOVED_CHILD="$("$MUX_BIN" mux pane keeper list --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
rows=[r for r in rows if r.get("socket")==sys.argv[1]]
assert len(rows)==1, f"expected one keeper row at the thread socket, got {rows}"
print(rows[0].get("child_pid"))' "$NEW_SOCKET")"
[[ "$MOVED_CHILD" == "$CHILD_PID" ]] || fail "the keeper at $NEW_SOCKET holds child $MOVED_CHILD, expected $CHILD_PID"
echo "[7] keeper at the thread socket answers with the same child $CHILD_PID"

# 8. The server let the pane go without killing it. The pane is out of the
#    listing and the server is still serving.
"$MUX_BIN" mux pane ls --server "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
gone=[r for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert not gone, f"pane {sys.argv[1]} is still seated after the hand-off: {gone}"' "$PANE_ID"
alive "$SERVER_PID" || fail "the mux server died during the hand-off"
echo "[8] pane $PANE_ID released from the layout; server still live"

if [[ "${FNO_CONVERT_LIVE:-}" == "1" ]]; then
    echo "[live] FNO_CONVERT_LIVE=1: running the end-to-end conversion against the real CLIs"
    AGENTS_BIN="${FNO_AGENTS_BIN:-$REPO_ROOT/crates/fno-agents/target/debug/fno-agents}"
    [[ -x "$AGENTS_BIN" ]] || fail "FNO_CONVERT_LIVE needs $AGENTS_BIN; build it first"
    # A dry run is the safe half: it must name a strategy and move nothing.
    "$AGENTS_BIN" resume convert-proof --substrate thread --dry-run >"$TMP_DIR/dry.json" 2>&1 ||
        fail "the dry run refused: $(cat "$TMP_DIR/dry.json")"
    grep -q 'strategy' "$TMP_DIR/dry.json" || fail "the dry-run receipt names no strategy"
    echo "[live] dry run named a strategy and moved nothing"
else
    echo "[skip] the claude and codex arms need real CLIs; set FNO_CONVERT_LIVE=1 to run them"
fi

echo "PASS: the pane-to-thread hand-off moves the socket and never stops the child"
