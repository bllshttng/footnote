#!/usr/bin/env bash
set -euo pipefail

# The keeper's real proof: a worker pane's child pid, recorded; the mux
# server, SIGKILLed; that exact pid, still alive with parent 1; a fresh
# server, re-adopting the SAME pid; and the pane, ANSWERING a prompt after
# all of it. No assertion here trusts an exit code or a survivor count.
# A plain (non-worker) pane is carried along as the control: it must die
# with the server, exactly as it always has.

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

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fk.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
mkdir -p "$MUX_DIR"
export FNO_MUX_DIR="$MUX_DIR"
export FNO_AGENTS_WORKER_BIN="$WORKER_BIN"
SESSION="fk-$$"
export SESSION
SERVER_PID=""

cleanup() {
    "$MUX_BIN" mux kill-server "$SESSION" >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

alive() { kill -0 "$1" 2>/dev/null; }
ppid_of() { ps -o ppid= -p "$1" | tr -d '[:space:]'; }

"$MUX_BIN" mux server --session "$SESSION" >"$TMP_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in {1..100}; do
    if "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); sys.exit(0 if any(r.get("session")==os.environ["SESSION"] and r.get("state")=="live" for r in rows) else 1)'; then
        break
    fi
    sleep 0.1
done

# The worker pane: a responder that answers every line it is sent. It is
# the harness under test - after the server's death IT must still answer.
"$MUX_BIN" mux pane run --session "$SESSION" --worker proof-worker --json -- \
    bash -c 'while IFS= read -r l; do echo "GOT:$l"; done' >"$TMP_DIR/worker.json"
# The control: a plain pane, which is CORRECT to die with its server.
"$MUX_BIN" mux pane run --session "$SESSION" --json -- sleep 600 >"$TMP_DIR/plain.json"

PANE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/worker.json")"
PLAIN_PANE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/plain.json")"

CHILD_PID="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
pids=[r.get("child_pid") for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert len(pids)==1, f"expected one worker pane child pid, got {pids}"
print(pids[0])' "$PANE_ID")"
PLAIN_PID="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
pids=[r.get("child_pid") for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert len(pids)==1, f"expected one plain pane child pid, got {pids}"
print(pids[0])' "$PLAIN_PANE_ID")"
KEEPER_ROW="$("$MUX_BIN" mux pane keeper list --json | python3 -c '
import json,sys,os
rows=json.load(sys.stdin)
rows=[r for r in rows if r.get("session")==os.environ["SESSION"] and r.get("child_pid")==int(sys.argv[1])]
assert len(rows)==1, f"expected the keeper row for the worker child, got {rows}"
r=rows[0]
assert r.get("keeper_pid"), "the keeper row names its own pid"
print(r["keeper_pid"])' "$CHILD_PID")"
echo "[before] worker pane $PANE_ID child=$CHILD_PID keeper=$KEEPER_ROW; plain pane $PLAIN_PANE_ID child=$PLAIN_PID"

# The named death: SIGKILL, never SIGTERM - a graceful path could spare the
# child through a route that proves nothing about the hangup.
KILLED_SERVER="$SERVER_PID"
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
sleep 1

if alive "$CHILD_PID"; then
    echo "[after kill] worker child $CHILD_PID is ALIVE"
else
    echo "FAIL: worker child $CHILD_PID died with the server (pid $KILLED_SERVER)" >&2
    exit 1
fi
PARENT="$(ppid_of "$CHILD_PID")"
if [[ "$PARENT" == "$KEEPER_ROW" ]]; then
    echo "[after kill] worker child $CHILD_PID is still parented by its keeper $KEEPER_ROW"
else
    echo "FAIL: worker child $CHILD_PID has ppid=$PARENT, expected its keeper $KEEPER_ROW" >&2
    exit 1
fi
if alive "$KEEPER_ROW"; then
    echo "[after kill] keeper $KEEPER_ROW is ALIVE and holds the master"
else
    echo "FAIL: keeper $KEEPER_ROW died" >&2
    exit 1
fi
KEEPER_PARENT="$(ppid_of "$KEEPER_ROW")"
if [[ "$KEEPER_PARENT" == "1" ]]; then
    echo "[after kill] keeper $KEEPER_ROW reparented to init/launchd (ppid=1)"
else
    echo "FAIL: keeper $KEEPER_ROW has ppid=$KEEPER_PARENT, expected 1 after the server died" >&2
    exit 1
fi
if alive "$PLAIN_PID"; then
    echo "FAIL: plain pane child $PLAIN_PID survived the server kill; plain panes must die with it (AC4)" >&2
    exit 1
else
    echo "[after kill] plain pane child $PLAIN_PID is dead, as it always was"
fi

# The re-adoption: a fresh server on the same session binds the SAME child.
"$MUX_BIN" mux server --session "$SESSION" >>"$TMP_DIR/server.log" 2>&1 &
SERVER_PID=$!
for _ in {1..100}; do
    if "$MUX_BIN" mux ls --json | python3 -c 'import json,os,sys; rows=json.load(sys.stdin); sys.exit(0 if any(r.get("session")==os.environ["SESSION"] and r.get("state")=="live" for r in rows) else 1)'; then
        break
    fi
    sleep 0.1
done
sleep 1

CHILD_PID_AFTER="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
rows=[r for r in rows if r.get("pane_id") and r.get("child_pid")==int(sys.argv[1])]
assert len(rows)==1, f"the fresh server did not re-adopt child pid {sys.argv[1]}: {rows}"
print(rows[0]["pane_id"])' "$CHILD_PID")"
if [[ "$CHILD_PID_AFTER" != "" ]]; then
    echo "[re-adopt] fresh server bound the SAME child pid $CHILD_PID as pane $CHILD_PID_AFTER - a re-adoption, not a respawn"
else
    echo "FAIL: the re-adopted pane vanished" >&2
    exit 1
fi

# The answer: the surviving pane still ANSWERS a prompt. A live pid hosting
# a wedged harness is not a survival. The pane's ID is the fresh server's
# (the dead server's id died with it); the CHILD pid is the survivor.
"$MUX_BIN" mux pane send --session "$SESSION" "$CHILD_PID_AFTER" --text 'proof-line-274a' --raw --submit >/dev/null
# Whole-grid read, never `--lines`: read_tail reads the BOTTOM N display
# rows, and a fresh adopted VT holds this pane's short answer at the TOP
# rows with an empty history - a sparse grid reads as empty through
# --lines even though the answer is on screen.
ANSWER=""
for _ in {1..40}; do
    ANSWER="$("$MUX_BIN" mux pane read --session "$SESSION" "$CHILD_PID_AFTER" 2>/dev/null | grep -F 'GOT:proof-line-274a' || true)"
    if [[ -n "$ANSWER" ]]; then break; fi
    sleep 0.25
done
if [[ -n "$ANSWER" ]]; then
    echo "[answer] the surviving pane answered through the re-adopted server: $ANSWER"
else
    echo "FAIL: the surviving pane never answered after re-adoption" >&2
    exit 1
fi

echo "PASS: worker child $CHILD_PID outlived the killed server $SESSION, was re-adopted by a fresh server, and answered a prompt; the plain pane died with it"
