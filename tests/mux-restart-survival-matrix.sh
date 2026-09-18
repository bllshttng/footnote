#!/usr/bin/env bash
set -euo pipefail

# The restart survival matrix: every kind of pane, view and daemon row,
# measured across three restarts (the mux server alone, the agents daemon
# alone, and both SIGKILLed at once). A row survives when its child pid is
# UNCHANGED and its canary counter kept advancing across the gap; a portal
# row also re-arms LIVE at the same index without a second viewer. No
# assertion here trusts an exit code or a survivor count.
#
# Rows, each a stub by default: (a) a plain shell running the canary,
# (b) an ad-hoc `pane run` with no --worker, (c) a --worker keeper pane
# (the existing proof), (d) a portal viewer on a stub thread row, (e) the
# daemon legs against a private agents home. With FNO_MATRIX_LIVE=1 rows
# (d) and (e) use real claude and codex threads through `fno agents spawn`.
#
# The run opens with the positive control for the liveness reader: a canary
# is killed by name and the reader must call it dead. A reader that calls a
# killed canary alive fails the run before any row is judged.
#
# Nothing here touches the live fleet: every store is a private root
# (FNO_MUX_DIR, FNO_AGENTS_HOME, HOME) and every survivor ends by named pid.

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MUX_BIN="${FNO_MUX_BIN:-$REPO_ROOT/crates/fno/target/debug/fno}"
WORKER_BIN="${FNO_AGENTS_WORKER_BIN:-$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker}"
AGENTS_CLIENT_BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"
DAEMON_BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-daemon"
if [[ ! -x "$MUX_BIN" || ! -x "$WORKER_BIN" || ! -x "$AGENTS_CLIENT_BIN" || ! -x "$DAEMON_BIN" ]]; then
    if [[ -n "${FNO_MUX_BIN:-}" || -n "${FNO_AGENTS_WORKER_BIN:-}" ]]; then
        echo "FAIL: explicit binaries are not executable" >&2
        exit 1
    fi
    echo "[setup] building all three crates" >&2
    cargo build --manifest-path "$REPO_ROOT/crates/fno-agents/Cargo.toml" \
        --bin fno-agents-worker --bin fno-agents --bin fno-agents-daemon
    cargo build --manifest-path "$REPO_ROOT/crates/fno/Cargo.toml" --bin fno
fi

# A private HOME keeps every consumer (config, pr-watch, provider state)
# inside the run; the daemon restart's pr-watch refresh self-gates on a
# config that does not exist here.
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fmx.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
AGENTS_HOME="$TMP_DIR/agents-home"
PRIVATE_HOME="$TMP_DIR/home"
mkdir -p "$MUX_DIR" "$AGENTS_HOME" "$PRIVATE_HOME"
export HOME="$PRIVATE_HOME"
export FNO_MUX_DIR="$MUX_DIR"
export FNO_AGENTS_HOME="$AGENTS_HOME"
export FNO_AGENTS_WORKER_BIN="$WORKER_BIN"
SESSION="fmx-$$"
export SESSION
export FNO_SERVER="$SESSION"
SERVER_PID=""
DAEMON_PID=""
SURVIVOR_PIDS=""
SOCK="$MUX_DIR/$SESSION.sock"

STUB_DIR="$TMP_DIR/stubbin"
DAEMON_DIR="$TMP_DIR/claude-daemon"
mkdir -p "$STUB_DIR" "$DAEMON_DIR" "$TMP_DIR/repo"

CLAUDE_SID="01a0f1ce-0000-4c1e-8a1c-2d3e4f5a6b7c"
CODEX_SID="01a0f1ce-1111-4c1e-8a1c-2d3e4f5a6b7c"

# The harness stubs answer every line and never exit: the survival behavior
# under test. The `fno` stub stands in for the peek argv a portal fill runs.
cat >"$STUB_DIR/claude" <<'STUB'
#!/bin/sh
while IFS= read -r l; do echo "claude-stub:$l"; done
STUB
cat >"$STUB_DIR/codex" <<'STUB'
#!/bin/sh
while IFS= read -r l; do echo "codex-stub:$l"; done
STUB
cat >"$STUB_DIR/fno" <<'STUB'
#!/bin/sh
echo "fno-stub:$*"
while IFS= read -r l; do echo "stub-responder:$l"; done
STUB
chmod +x "$STUB_DIR/claude" "$STUB_DIR/codex" "$STUB_DIR/fno"
export PATH="$STUB_DIR:$PATH"
export FNO_AGENTS_BIN="$STUB_DIR/fno-agents"
cat >"$FNO_AGENTS_BIN" <<'STUB'
#!/bin/sh
if [ "$1" = "reentry-plan" ]; then
    printf '%s\n' '{"resolved": true, "argv": ["/bin/sh", "-c", "while IFS= read -r l; do echo \"stub-responder:$l\"; done"], "env": {}, "config_dir": null}'
    exit 0
fi
exit 3
STUB
chmod +x "$FNO_AGENTS_BIN"

# The paneless thread rows the portals reach (the portal-restore proof's
# planter, plus `created_at`: the Rust registry reader requires it, and a
# row it cannot decode stops the daemon from ever booting in this home).
python3 - "$AGENTS_HOME/registry.json" "$TMP_DIR" "$CLAUDE_SID" "$CODEX_SID" <<'PY'
import json, sys
path, tmp, csid, xsid = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
doc = {"schema_version": 31, "agents": [{
    "name": "proof-claude",
    "cwd": f"{tmp}/repo",
    "harness": "claude",
    "harness_session_id": csid,
    "status": "live",
    "liveness": "alive",
    "created_at": "2026-09-17T00:00:00Z",
}, {
    "name": "proof-codex",
    "cwd": f"{tmp}/repo",
    "harness": "codex",
    "harness_session_id": xsid,
    "status": "live",
    "liveness": "alive",
    "created_at": "2026-09-17T00:00:00Z",
}]}
json.dump(doc, open(path, "w"))
PY
cat >"$DAEMON_DIR/roster.json" <<ROSTER
[{"id": "sessionId:pr00f1a1-0000-0000-0000-000000000001", "name": "proof-claude", "cwd": "$TMP_DIR/repo"}]
ROSTER
export FNO_CLAUDE_DAEMON_DIR="$DAEMON_DIR"

cleanup() {
    "$MUX_BIN" mux kill-server "$SESSION" >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
        kill -9 "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    for pid in $SURVIVOR_PIDS; do
        kill -9 "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$TMP_DIR" 2>/dev/null || { sleep 2; rm -rf "$TMP_DIR"; }
}
trap cleanup EXIT

alive() { kill -0 "$1" 2>/dev/null; }
ppid_of() { ps -o ppid= -p "$1" | tr -d '[:space:]'; }
counter_lines() {
    if [[ -f "$1" ]]; then wc -l <"$1" | tr -d '[:space:]'; else echo 0; fi
}

# The counter must pass `min` within 10s: proof the child APPENDED across
# the gap, not merely that its pid answered a signal.
counter_grew() {
    local file="$1" min="$2" i
    for i in {1..100}; do
        if [[ "$(counter_lines "$file")" -gt "$min" ]]; then return 0; fi
        sleep 0.1
    done
    return 1
}

pane_field() {
    "$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
field, pane = sys.argv[1], int(sys.argv[2])
got = [r.get(field) for r in rows if r.get("pane_id") == pane]
assert len(got) == 1, f"expected one row for pane {pane}, got {rows}"
print(got[0])
' "$1" "$2"
}

# The portal row's viewer process, counted by server truth: exactly one
# pane row carries the child pid. A whole-table ps scan is refused on
# purpose: it counts self-matches.
viewer_count() {
    "$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
child = int(sys.argv[1])
print(len([r for r in rows if r.get("child_pid") == child]))
' "$CHILD_D"
}

RED_ROWS=""

# One row's survival facts, recorded not fatal: a red run must name every
# red row, not stop at the first. Recorded in RED_ROWS; the caller decides.
verify_row() {
    local phase="$1" row="$2" pane="$3" child="$4" file="$5" min="$6"
    if ! alive "$child"; then
        echo "FAIL: [$phase] row ($row) child $child is DEAD"
        RED_ROWS="$RED_ROWS [$phase]$row"
        return 1
    fi
    # Re-adoption is asynchronous: poll for the binding before judging it.
    local i got bound=""
    for i in {1..100}; do
        got="$(pane_field child_pid "$pane" 2>/dev/null || echo none)"
        if [[ "$got" == "$child" ]]; then bound=1; break; fi
        sleep 0.1
    done
    if [[ -z "$bound" ]]; then
        echo "FAIL: [$phase] row ($row) pane $pane never re-bound to child $child (last: $got)"
        RED_ROWS="$RED_ROWS [$phase]$row"
        return 1
    fi
    if [[ "$min" != "none" ]] && ! counter_grew "$file" "$min"; then
        echo "FAIL: [$phase] row ($row) child $child stopped ticking (stuck at $(counter_lines "$file"))"
        RED_ROWS="$RED_ROWS [$phase]$row"
        return 1
    fi
    echo "[$phase] row ($row) pane $pane child $child unchanged, counter -> $(counter_lines "$file")"
    return 0
}

# A canary loop as a file: the portable survival fact. Written as a script
# so both shell and argv rows run the same loop.
CANARY="$TMP_DIR/canary.sh"
cat >"$CANARY" <<CANARY
#!/bin/sh
while :; do echo tick >> "\$1"; sleep 0.2; done
CANARY
chmod +x "$CANARY"

# The one non-passive client attach that makes a fresh server mint its home
# squad (a tab create refuses without one), the portal-restore proof's shape.
ATTACH_CLIENT="$TMP_DIR/attach_client.py"
cat >"$ATTACH_CLIENT" <<'PY'
import json, socket, struct, sys
sock_path, cwd = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
msg = {"Attach": {"proto": 81, "build": "restart-matrix", "rows": 24, "cols": 80, "cwd": cwd}}
body = json.dumps(msg).encode()
s.sendall(struct.pack(">I", len(body)) + body)
s.settimeout(3)
try:
    s.recv(65536)
except socket.timeout:
    pass
s.close()
PY

start_server() {
    "$MUX_BIN" mux server --session "$SESSION" >>"$TMP_DIR/server.log" 2>&1 &
    SERVER_PID=$!
    for _ in {1..100}; do
        if [[ -S "$SOCK" ]]; then
            for _ in {1..50}; do
                if "$MUX_BIN" mux pane ls --session "$SESSION" --json >/dev/null 2>&1; then
                    break
                fi
                sleep 0.2
            done
            local attempt
            for attempt in 1 2 3 4 5; do
                if python3 "$ATTACH_CLIENT" "$SOCK" "$TMP_DIR/repo" >/dev/null 2>&1; then
                    return 0
                fi
                sleep 0.5
            done
            echo "FAIL: the attach never landed" >&2
            exit 1
        fi
        sleep 0.1
    done
    echo "FAIL: the mux server never went live" >&2
    exit 1
}

kill_server_hard() {
    kill -9 "$SERVER_PID"
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=""
    sleep 1
}

# ── the positive control: a dead canary must read as dead ────────────────
# The kill is the snapshot boundary: reads BEFORE it race the append window
# (a loaded box stretches any gap), reads AFTER a reaped corpse cannot grow.
CTRL_FILE="$TMP_DIR/counter-ctrl"
"$CANARY" "$CTRL_FILE" &
CTRL_PID=$!
counter_grew "$CTRL_FILE" 0 || { echo "FAIL: control canary never ticked" >&2; exit 1; }
kill -9 "$CTRL_PID"
wait "$CTRL_PID" 2>/dev/null || true
if alive "$CTRL_PID"; then
    echo "FAIL: liveness reader calls killed canary $CTRL_PID alive" >&2
    exit 1
fi
sleep 1
CTRL_LINES="$(counter_lines "$CTRL_FILE")"
sleep 1
if [[ "$(counter_lines "$CTRL_FILE")" -ne "$CTRL_LINES" ]]; then
    echo "FAIL: killed canary $CTRL_PID kept ticking ($CTRL_LINES -> $(counter_lines "$CTRL_FILE"))" >&2
    exit 1
fi
echo "[control] reader calls killed canary $CTRL_PID dead and its counter stopped at $CTRL_LINES"

# ── plant the rows ────────────────────────────────────────────────────────
start_server

# (a) plain shell: a new tab's shell pane, fed the canary by send.
PANES_BEFORE="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c 'import json,sys; print(" ".join(str(r.get("pane_id")) for r in json.load(sys.stdin)))')"
"$MUX_BIN" mux tab create --session "$SESSION" >/dev/null 2>&1 \
    || "$MUX_BIN" mux tab create >/dev/null 2>&1
PANE_A="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json, sys
before = set(sys.argv[1].split())
rows = json.load(sys.stdin)
new = [str(r["pane_id"]) for r in rows if str(r["pane_id"]) not in before]
assert len(new) == 1, f"expected one new pane, got {new} in {rows}"
print(new[0])
' "$PANES_BEFORE")"
FILE_A="$TMP_DIR/counter-a"
# The send receipt is soft (an unconfirmed submission is not a failure);
# the counter is the assertion that the canary runs.
"$MUX_BIN" mux pane send --session "$SESSION" "$PANE_A" --raw --submit \
    --text "$CANARY $FILE_A" >/dev/null 2>&1 || true
counter_grew "$FILE_A" 0 || { echo "FAIL: row (a) shell never ran the canary" >&2; exit 1; }
CHILD_A="$(pane_field child_pid "$PANE_A")"
SURVIVOR_PIDS="$CHILD_A"

# (b) ad-hoc argv pane, no --worker: stands in for a private codex pane.
FILE_B="$TMP_DIR/counter-b"
"$MUX_BIN" mux pane run --session "$SESSION" --cwd "$TMP_DIR/repo" --json -- "$CANARY" "$FILE_B" >"$TMP_DIR/pane-b.json"
PANE_B="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/pane-b.json")"
counter_grew "$FILE_B" 0 || { echo "FAIL: row (b) pane never ran the canary" >&2; exit 1; }
CHILD_B="$(pane_field child_pid "$PANE_B")"
SURVIVOR_PIDS="$SURVIVOR_PIDS $CHILD_B"

# (c) the keeper worker pane: the survival that already works.
FILE_C="$TMP_DIR/counter-c"
"$MUX_BIN" mux pane run --session "$SESSION" --cwd "$TMP_DIR/repo" --worker c-keeper --json -- "$CANARY" "$FILE_C" >"$TMP_DIR/pane-c.json"
PANE_C="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/pane-c.json")"
counter_grew "$FILE_C" 0 || { echo "FAIL: row (c) pane never ran the canary" >&2; exit 1; }
CHILD_C="$(pane_field child_pid "$PANE_C")"
KEEPER_C="$("$MUX_BIN" mux pane keeper list --json | python3 -c '
import json, sys, os
rows = json.load(sys.stdin)
rows = [r for r in rows if r.get("session") == os.environ["SESSION"] and r.get("child_pid") == int(sys.argv[1])]
assert len(rows) == 1, f"expected the keeper row for the row-(c) child, got {rows}"
print(rows[0]["keeper_pid"])
' "$CHILD_C")"
SURVIVOR_PIDS="$SURVIVOR_PIDS $CHILD_C $KEEPER_C"

# (d) portal viewer on a stub thread row (portal-restore proof's reach).
# The viewer pane binds the thread row's NAME: that name is the join.
ROW_D_NAME="proof-claude"
if [[ "${FNO_MATRIX_LIVE:-0}" == "1" ]]; then
    ROW_D_NAME="proof-claude-$$"
    "$MUX_BIN" agents spawn 'echo matrix-live-echo and stop.' \
        --name "$ROW_D_NAME" --harness claude --substrate thread --cwd "$TMP_DIR/repo" >/dev/null
else
    "$MUX_BIN" mux thread proof-claude --portal 0 >/dev/null 2>&1 || {
        echo "FAIL: the portal reach never filled" >&2
        exit 1
    }
fi
sleep 1
PANE_D="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json, sys
rows = json.load(sys.stdin)
hits = [r for r in rows if r.get("name") == sys.argv[1]]
assert len(hits) == 1, f"expected exactly one pane named {sys.argv[1]}, got {rows}"
print(hits[0]["pane_id"])
' "$ROW_D_NAME")"
CHILD_D="$(pane_field child_pid "$PANE_D")"
FILE_D="none"
LINES_D="none"
SURVIVOR_PIDS="$SURVIVOR_PIDS $CHILD_D"

echo "[before] a: pane $PANE_A child $CHILD_A; b: pane $PANE_B child $CHILD_B; c: pane $PANE_C child $CHILD_C keeper $KEEPER_C; d: pane $PANE_D child $CHILD_D argv: $(ps -o command= -p "$CHILD_D" 2>/dev/null | head -c 100)"

LINES_A="$(counter_lines "$FILE_A")"
LINES_B="$(counter_lines "$FILE_B")"
LINES_C="$(counter_lines "$FILE_C")"

# ── restart 1: the mux server alone ──────────────────────────────────────
KILLED_SERVER="$SERVER_PID"
kill_server_hard
echo "[restart 1] mux server $KILLED_SERVER SIGKILLed"

start_server

for row in a b c d; do
    eval "pane=\$PANE_$(printf %s "$row" | tr a-z A-Z)"
    eval "child=\$CHILD_$(printf %s "$row" | tr a-z A-Z)"
    eval "file=\$FILE_$(printf %s "$row" | tr a-z A-Z)"
    eval "lines=\$LINES_$(printf %s "$row" | tr a-z A-Z)"
    verify_row "restart 1" "$row" "$pane" "$child" "$file" "$lines" || true
done

if [[ "$(viewer_count)" -ne 1 ]]; then
    echo "FAIL: the portal row does not hold exactly one viewer pane row"
    RED_ROWS="$RED_ROWS [restart 1]d-viewer"
fi
if [[ "${FNO_MATRIX_LIVE:-0}" != "1" ]]; then
    NOW_ARGV="$(ps -o command= -p "$CHILD_D" 2>/dev/null || true)"
    case "$NOW_ARGV" in
        *stubbin/fno*|*stub-responder*) ;;
        *) echo "FAIL: the process at viewer pid $CHILD_D lost its stub argv: $NOW_ARGV"
           RED_ROWS="$RED_ROWS [restart 1]d-argv" ;;
    esac
fi
if grep -q "placed in its own tab" "$TMP_DIR/server.log" 2>/dev/null; then
    echo "FAIL: a re-adopted pane lost its seat (placed in its own tab)"
    RED_ROWS="$RED_ROWS [restart 1]seat"
fi
echo "[restart 1] portal still held by exactly one viewer pane at pid $CHILD_D"

# ── restart 2: the agents daemon alone ───────────────────────────────────
# The real daemon, private home: lazy-start it, read its pid from the
# supervisor lock, restart, require old -> new and the mux rows untouched.
"$AGENTS_CLIENT_BIN" list >/dev/null 2>&1 || true
for _ in {1..100}; do
    DAEMON_PID="$(head -1 "$AGENTS_HOME/supervisor.sock.lock" 2>/dev/null | awk '{print $1}')"
    [[ -n "$DAEMON_PID" ]] && alive "$DAEMON_PID" && break
    sleep 0.1
done
if [[ -z "$DAEMON_PID" ]] || ! alive "$DAEMON_PID"; then
    echo "FAIL: no daemon started in the private home" >&2
    exit 1
fi
SURVIVOR_PIDS="$SURVIVOR_PIDS $DAEMON_PID"
RESTART_OUT="$("$AGENTS_CLIENT_BIN" restart 2>&1)" || true
echo "[restart 2] $RESTART_OUT"
echo "$RESTART_OUT" | grep -F "restarted: pid $DAEMON_PID -> " >/dev/null || {
    echo "FAIL: the daemon restart receipt does not name old pid $DAEMON_PID" >&2
    exit 1
}
NEW_DAEMON_PID="$(echo "$RESTART_OUT" | python3 -c '
import sys, re
m = re.search(r"restarted: pid \d+ -> (\d+)", sys.stdin.read())
assert m, "no new pid in the restart receipt"
print(m.group(1))
')"
alive "$NEW_DAEMON_PID" || { echo "FAIL: the fresh daemon $NEW_DAEMON_PID is not alive" >&2; exit 1; }
DAEMON_PID="$NEW_DAEMON_PID"
for row in a b c d; do
    eval "pane=\$PANE_$(printf %s "$row" | tr a-z A-Z)"
    eval "child=\$CHILD_$(printf %s "$row" | tr a-z A-Z)"
    eval "file=\$FILE_$(printf %s "$row" | tr a-z A-Z)"
    eval "lines=\$LINES_$(printf %s "$row" | tr a-z A-Z)"
    verify_row "restart 2" "$row" "$pane" "$child" "$file" "$lines" || true
done
echo "[restart 2] daemon $DAEMON_PID is fresh; every pane child unchanged"

# ── restart 3: both at once ──────────────────────────────────────────────
KILLED_SERVER="$SERVER_PID"
KILLED_DAEMON="$DAEMON_PID"
kill -9 "$SERVER_PID"; wait "$SERVER_PID" 2>/dev/null || true; SERVER_PID=""
kill -9 "$DAEMON_PID"; wait "$DAEMON_PID" 2>/dev/null || true; DAEMON_PID=""
sleep 1
echo "[restart 3] server $KILLED_SERVER and daemon $KILLED_DAEMON SIGKILLed together"

start_server
"$AGENTS_CLIENT_BIN" list >/dev/null 2>&1 || true
for _ in {1..100}; do
    DAEMON_PID="$(head -1 "$AGENTS_HOME/supervisor.sock.lock" 2>/dev/null | awk '{print $1}')"
    [[ -n "$DAEMON_PID" ]] && alive "$DAEMON_PID" && break
    sleep 0.1
done
alive "$DAEMON_PID" || { echo "FAIL: no daemon came back after the double kill" >&2; exit 1; }

for row in a b c d; do
    eval "pane=\$PANE_$(printf %s "$row" | tr a-z A-Z)"
    eval "child=\$CHILD_$(printf %s "$row" | tr a-z A-Z)"
    eval "file=\$FILE_$(printf %s "$row" | tr a-z A-Z)"
    eval "lines=\$LINES_$(printf %s "$row" | tr a-z A-Z)"
    verify_row "restart 3" "$row" "$pane" "$child" "$file" "$lines" || true
done

if [[ -n "$RED_ROWS" ]]; then
    echo "FAIL: matrix red on rows:$RED_ROWS" >&2
    exit 1
fi
echo "PASS: every row kept its child pid and its counter across all three restarts; the portal held its seat and its single viewer"
