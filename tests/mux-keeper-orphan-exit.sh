#!/usr/bin/env bash
set -uo pipefail

# The orphan proof: a keeper whose socket DIRECTORY is deleted must end
# itself and its child, and a keeper must die to SIGTERM. Three legs over
# two plain panes in one private root: sigterm (keeper B), survives (the
# positive control - keeper A outlives a server SIGKILL that keeps the dir),
# and orphan (the dir deleted under keeper A). The sigterm and orphan legs
# fail on a build without the self-exit paths and must stay green after.
# Every keeper and child is a named pid read from the mux, never a count.

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

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fko.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
mkdir -p "$MUX_DIR"
# Exported BEFORE the server starts: the server passes its env to every
# keeper it spawns, so the isolation root and the fast poll reach them.
export FNO_MUX_DIR="$MUX_DIR"
export FNO_AGENTS_HOME="$TMP_DIR/agents"
export FNO_AGENTS_WORKER_BIN="$WORKER_BIN"
# Ignored by a build without the orphan poll; the fixed build polls at 500ms.
export FNO_KEEPER_ORPHAN_POLL_MS=500
SESSION="fko-$$"
export SESSION
SERVER_PID=""
PIDS=""
FAILED=0

cleanup() {
    # Keepers first, while a live server can still list them.
    "$MUX_BIN" mux pane keeper list --json 2>/dev/null | python3 -c '
import json, os, subprocess, sys
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for r in rows:
    if r.get("session") != os.environ["SESSION"]:
        continue
    for field in ("keeper_pid", "child_pid"):
        pid = r.get(field)
        if pid:
            subprocess.run(["kill", "-9", str(pid)], capture_output=True)
' || true
    sleep 0.3
    # The argv sweep: this run's keepers carry the temp prefix in --sock.
    ps -axo pid=,command= | python3 -c '
import subprocess, sys
needle = sys.argv[1]
for ln in sys.stdin:
    pid, _, rest = ln.strip().partition(" ")
    if "ps -axo" in ln or "python3 -c" in ln:
        continue
    if needle in rest and "fno-agents-worker" in rest:
        subprocess.run(["kill", "-9", pid], capture_output=True)
' "$TMP_DIR" || true
    for pid in $PIDS; do
        kill -9 "$pid" 2>/dev/null || true
    done
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    wait 2>/dev/null || true
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

# Two plain panes; every pane is keeper-hosted, so each names a keeper.
"$MUX_BIN" mux pane run --session "$SESSION" --json -- sleep 600 >"$TMP_DIR/a.json"
"$MUX_BIN" mux pane run --session "$SESSION" --json -- sleep 600 >"$TMP_DIR/b.json"

mapfile -t CHILD_PIDS < <("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
pids = sorted(r["child_pid"] for r in rows if r.get("child_pid"))
assert len(pids) == 2, f"expected two pane child pids, got {rows}"
print("\n".join(str(p) for p in pids))
')
CHILD_A="${CHILD_PIDS[0]}"
CHILD_B="${CHILD_PIDS[1]}"
PIDS="$CHILD_A $CHILD_B"

keeper_pid_for() {
    CHILD="$1" "$MUX_BIN" mux pane keeper list --json | python3 -c '
import json, os, sys
rows = json.load(sys.stdin)
child = int(os.environ["CHILD"])
rows = [r for r in rows if r.get("session") == os.environ["SESSION"] and r.get("child_pid") == child]
assert len(rows) == 1, f"expected one keeper row for child {child}, got {rows}"
print(rows[0]["keeper_pid"])
'
}
KEEPER_A="$(keeper_pid_for "$CHILD_A")"
KEEPER_B="$(keeper_pid_for "$CHILD_B")"
PIDS="$PIDS $KEEPER_A $KEEPER_B"
echo "[before] keeper A $KEEPER_A child $CHILD_A; keeper B $KEEPER_B child $CHILD_B"

# Leg sigterm: a blocked-inherited signal must still end keeper and child.
RESULT=FAIL
kill -TERM "$KEEPER_B" 2>/dev/null || true
for _ in {1..100}; do
    if ! alive "$KEEPER_B" && ! alive "$CHILD_B"; then
        RESULT=PASS
        break
    fi
    sleep 0.1
done
if [[ "$RESULT" == "FAIL" ]]; then FAILED=1; fi
echo "leg=sigterm result=$RESULT keeper=$KEEPER_B child=$CHILD_B"

# Leg survives: the control. A server SIGKILL that keeps the socket dir
# must leave keeper A alive, reparented to init, still answering.
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
sleep 1
RESULT=FAIL
if alive "$KEEPER_A" && [[ "$(ppid_of "$KEEPER_A")" == "1" ]]; then
    if CHILD_A="$CHILD_A" KEEPER_A="$KEEPER_A" "$MUX_BIN" mux pane keeper list --json 2>/dev/null | python3 -c '
import json, os, sys
rows = json.load(sys.stdin)
keeper = int(os.environ["KEEPER_A"])
child = int(os.environ["CHILD_A"])
sys.exit(0 if any(r.get("keeper_pid") == keeper and r.get("child_pid") == child for r in rows) else 1)
'; then
        RESULT=PASS
    fi
fi
if [[ "$RESULT" == "FAIL" ]]; then FAILED=1; fi
echo "leg=survives result=$RESULT keeper=$KEEPER_A child=$CHILD_A"

# Leg orphan: the whole mux dir gone means no server can ever reach the
# keeper again; it must end itself and its child within two polls.
rm -rf "$FNO_MUX_DIR"
RESULT=FAIL
for _ in {1..150}; do
    if ! alive "$KEEPER_A" && ! alive "$CHILD_A"; then
        RESULT=PASS
        break
    fi
    sleep 0.1
done
if [[ "$RESULT" == "FAIL" ]]; then FAILED=1; fi
echo "leg=orphan result=$RESULT keeper=$KEEPER_A child=$CHILD_A"

if [[ "$FAILED" -ne 0 ]]; then
    echo "FAIL: a keeper lived past SIGTERM or a deleted socket dir" >&2
    exit 1
fi
echo "PASS: keepers end themselves on SIGTERM and when the socket dir is gone; the dir-kept control survived the server kill"
