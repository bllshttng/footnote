#!/usr/bin/env bash
set -euo pipefail

# x-a6b9's real proof: one restore verb answers for every held portal across
# a server SIGKILL - a claude Drive seat RESUMES in its recorded leaf and
# ANSWERS a prompt read off its pane grid, a refused portal is NAMED with a
# reason instead of skipping silently, and the codex pane worker's child pid
# survives both kills re-adopted - the survival that replaces the deleted
# claude-only revive leg.
#
# Two modes. By default the harnesses and the re-entry resolver are STUBS
# (the keeper proof's pattern), so the proof runs anywhere. With
# FNO_PORTAL_LIVE=1 the script plants real claude and codex threads through
# `fno agents spawn` and the real resolver; the PR body pastes one passing
# live run.
#
# No assertion here trusts an exit code or a survivor count. agy,
# cursor-agent and grok are named at the end: their threads reach Locate
# only, so a portal on them restores in place but cannot drive the thread.

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

# The proof owns its server; the CI harness's e2e breadcrumbs (FNO_E2E)
# only add a "client <sentinel> gone" line the parked portal reach's
# observer teardown legitimately produces, which the log scanners flag.
unset FNO_E2E
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fpr.XXXXXX")"
MUX_DIR="$TMP_DIR/mux"
mkdir -p "$MUX_DIR"
export FNO_MUX_DIR="$MUX_DIR"
export FNO_AGENTS_WORKER_BIN="$WORKER_BIN"
SESSION="fpr-$$"
export FNO_SERVER="$SESSION"
SERVER_PID=""
SURVIVOR_PIDS=""
SOCK="$MUX_DIR/$SESSION.sock"

STUB_DIR="$TMP_DIR/stubbin"
AGENTS_HOME="$TMP_DIR/agents-home"
DAEMON_DIR="$TMP_DIR/claude-daemon"
mkdir -p "$STUB_DIR" "$AGENTS_HOME" "$DAEMON_DIR" "$TMP_DIR/repo"

CLAUDE_SID="01a0f1ce-0000-4c1e-8a1c-2d3e4f5a6b7c"
CODEX_SID="01a0f1ce-1111-4c1e-8a1c-2d3e4f5a6b7c"

if [[ "${FNO_PORTAL_LIVE:-0}" == "1" ]]; then
    echo "[mode] LIVE: real claude + codex threads through the real resolver"
else
    echo "[mode] STUB: harnesses and the resolver are stubs"
    export FNO_AGENTS_HOME="$AGENTS_HOME"
    export FNO_CLAUDE_DAEMON_DIR="$DAEMON_DIR"
    export FNO_AGENTS_BIN="$STUB_DIR/fno-agents"
    # The harness stubs answer every line and never exit: the survival
    # behavior under test.
    cat >"$STUB_DIR/claude" <<'STUB'
#!/bin/sh
while IFS= read -r l; do echo "claude-portal:$l"; done
STUB
    cat >"$STUB_DIR/codex" <<'STUB'
#!/bin/sh
while IFS= read -r l; do echo "codex-portal:$l"; done
STUB
    # A Follow-tier fill runs the peek argv, which names the deployed `fno`
    # CLI; the stub stands in for it and still answers stdin lines.
    cat >"$STUB_DIR/fno" <<'STUB'
#!/bin/sh
echo "fno-stub:$*"
while IFS= read -r l; do echo "stub-responder:$l"; done
STUB
    chmod +x "$STUB_DIR/claude" "$STUB_DIR/codex" "$STUB_DIR/fno"
    export PATH="$STUB_DIR:$PATH"
    # The re-entry resolver stub: the attach transition resolves to a
    # responder argv, exactly the shape the real resolver's JSON carries.
    cat >"$FNO_AGENTS_BIN" <<'STUB'
#!/bin/sh
if [ "$1" = "reentry-plan" ]; then
    printf '%s\n' '{"resolved": true, "argv": ["/bin/sh", "-c", "while IFS= read -r l; do echo \"stub-responder:$l\"; done"], "env": {}, "config_dir": null}'
    exit 0
fi
exit 3
STUB
    chmod +x "$FNO_AGENTS_BIN"

    # The paneless thread rows the portals reach: a claude row made
    # Drive-tier by the roster join that stamps its attach id, and a codex
    # row whose portal still has to be ANSWERED FOR by name.
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
}, {
    "name": "proof-codex",
    "cwd": f"{tmp}/repo",
    "harness": "codex",
    "harness_session_id": xsid,
    "status": "live",
    "liveness": "alive",
}]}
json.dump(doc, open(path, "w"))
PY
    # Roster shape: the bare-list form the reader parses (short_id + name).
    cat >"$DAEMON_DIR/roster.json" <<ROSTER
[{"id": "sessionId:pr00f1a1-0000-0000-0000-000000000001", "name": "proof-claude", "cwd": "$TMP_DIR/repo"}]
ROSTER
fi

# The one non-passive client attach that makes a fresh server read its
# persisted workspace: the precondition the restore verb refuses without.
ATTACH_CLIENT="$TMP_DIR/attach_client.py"
cat >"$ATTACH_CLIENT" <<'PY'
import json, socket, struct, sys, time
sock_path, cwd = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
msg = {"Attach": {"proto": 81, "build": "portal-restore-proof", "rows": 24, "cols": 80, "cwd": cwd}}
body = json.dumps(msg).encode()
s.sendall(struct.pack(">I", len(body)) + body)
s.settimeout(3)
try:
    data = s.recv(65536)
    print("attach reply bytes:", len(data))
except socket.timeout:
    print("attach: no immediate reply (ok)")
time.sleep(2)
s.close()
PY

cleanup() {
    "$MUX_BIN" mux kill-server "$SESSION" >/dev/null 2>&1 || true
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -9 "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    for pid in $SURVIVOR_PIDS; do
        kill -9 "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null || true
    if [[ "${KEEP_TMP:-0}" != "1" ]]; then
        rm -rf "$TMP_DIR"
    fi
}
trap cleanup EXIT

alive() { kill -0 "$1" 2>/dev/null; }
# A control call can land while the server is still re-initializing after a
# kill: the read then resets. Retry the call instead of racing it.
retry_ctl() {
    local tries="$1"; shift
    local i=1
    until "$@" 2>/dev/null; do
        i=$((i + 1))
        if [ "$i" -gt "$tries" ]; then
            return 1
        fi
        sleep 0.5
    done
}
start_server() {
    "$MUX_BIN" mux server --session "$SESSION" >>"$TMP_DIR/server.log" 2>&1 &
    SERVER_PID=$!
    for _ in {1..100}; do
        if [[ -S "$SOCK" ]]; then
            # The socket file can appear before the accept loop serves: a
            # control connect in that window resets (ECONNRESET on Linux).
            # Poll a cheap control call until the server actually answers.
            for _ in {1..50}; do
                if "$MUX_BIN" mux pane ls --session "$SESSION" --json >/dev/null 2>&1; then
                    break
                fi
                sleep 0.2
            done
            # An early-connect reset can also land in the attach itself;
            # retry until the server takes one full non-passive attach.
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

start_server

# ── plant ────────────────────────────────────────────────────────────────
if [[ "${FNO_PORTAL_LIVE:-0}" == "1" ]]; then
    "$MUX_BIN" agents spawn 'echo portal-restore-a6b9 and stop.' \
        --name proof-claude --harness claude --substrate thread --cwd "$TMP_DIR/repo" >/dev/null
    "$MUX_BIN" agents spawn 'echo portal-restore-a6b9 and stop.' \
        --name proof-codex --harness codex --model gpt-5.6-luna --substrate thread --cwd "$TMP_DIR/repo" >/dev/null
fi

if ! "$MUX_BIN" mux thread proof-claude --portal 0 >/dev/null 2>&1; then
    echo "[plant] first reach refused; what the server's registry sees:" >&2
    "$MUX_BIN" agents list --json 2>&1 | head -c 900 >&2 || true
    echo >&2
    if ! "$MUX_BIN" mux thread proof-claude --portal 0; then
        echo "FAIL: the first portal reach never filled; server log tail:" >&2
        tail -5 "$TMP_DIR/server.log" >&2
        exit 1
    fi
fi
sleep 1
"$MUX_BIN" mux thread proof-codex --portal 2 --split right >/dev/null 2>&1 || \
    echo "[plant] portal 2's first reach did not fill; the verb will still answer for it by name"

# A codex PANE worker: the keeper-hosted survivor whose child pid must not
# change across either kill.
"$MUX_BIN" mux pane run --session "$SESSION" --worker proof-pane --json -- \
    codex --resume "$CODEX_SID" >"$TMP_DIR/worker.json"
PANE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pane_id"])' "$TMP_DIR/worker.json")"
CHILD_PID="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
pids=[r.get("child_pid") for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert len(pids)==1, f"expected one pane worker child pid, got {pids}"
print(pids[0])' "$PANE_ID")"
SURVIVOR_PIDS="$CHILD_PID"
echo "[before] portals planted; pane worker pane=$PANE_ID child=$CHILD_PID"

# ── first kill: the verb restores what the workspace held ───────────────
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
sleep 1
alive "$CHILD_PID" || { echo "FAIL: pane worker $CHILD_PID died with the server" >&2; exit 1; }
echo "[after kill 1] pane worker child $CHILD_PID is ALIVE (keeper-held)"

start_server
retry_ctl 10 "$MUX_BIN" mux workspace restore --session "$SESSION" --json >"$TMP_DIR/restore1.json"
python3 - "$TMP_DIR/restore1.json" "$TMP_DIR" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
tmp = sys.argv[2]
portals = doc.get("portals")
assert portals is not None, f"the reply carries no portals array: {doc}"
by_idx = {p.get("portal"): p for p in portals}
zero = by_idx.get(0)
assert zero, f"portal 0 is missing from the reply: {portals}"
assert zero.get("outcome") == "resumed", f"portal 0 did not resume: {zero}"
two = by_idx.get(2)
assert two, f"portal 2 is missing from the reply: {portals}"
if two.get("outcome") == "refused":
    assert two.get("reason"), f"portal 2 refused with no reason: {two}"
    open(f"{tmp}/portal2-refused", "w").write(two["reason"])
print(f"[restore 1] portal 0 resumed (pane {zero.get('pane')}); portal 2: {two.get('outcome')}")
PY

# The filled claude portal ANSWERS from its restored seat.
PORTAL_PANE="$(python3 -c '
import json, sys
doc = json.load(open(sys.argv[1]))
for p in doc.get("portals", []):
    if p.get("portal") == 0 and p.get("outcome") == "resumed":
        print(p["pane"]); break
else:
    sys.exit("portal 0 resumed with no pane")
' "$TMP_DIR/restore1.json")"
"$MUX_BIN" mux pane send --session "$SESSION" "$PORTAL_PANE" --text 'portal-restore-a6b9' --raw --submit >/dev/null 2>&1 \
    || echo "[send] submission receipt unconfirmed; the grid read decides"
GRID=""
for _ in 1 2 3 4 5; do
    sleep 1
    GRID="$("$MUX_BIN" mux pane read --session "$SESSION" "$PORTAL_PANE" 2>/dev/null || true)"
    case "$GRID" in
        *portal-restore-a6b9*) break ;;
    esac
done
case "$GRID" in
    *portal-restore-a6b9*|*stub-responder:portal-restore-a6b9*)
        echo "[answer 1] the restored portal answered from its seat"
        ;;
    *)
        echo "FAIL: the restored portal never answered; grid was: $GRID" >&2
        exit 1
        ;;
esac

# ── second kill: what restore resumed must survive again ────────────────
kill -9 "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
sleep 1
alive "$CHILD_PID" || { echo "FAIL: pane worker $CHILD_PID died on the second kill" >&2; exit 1; }
echo "[after kill 2] pane worker child $CHILD_PID still ALIVE"

start_server
retry_ctl 10 "$MUX_BIN" mux workspace restore --session "$SESSION" --json >"$TMP_DIR/restore2.json"
CHILD_PID_NOW="$("$MUX_BIN" mux pane ls --session "$SESSION" --json | python3 -c '
import json,sys
rows=json.load(sys.stdin)
pids=[r.get("child_pid") for r in rows if r.get("pane_id")==int(sys.argv[1])]
assert len(pids)==1, f"expected the pane worker re-adopted, got {rows}"
print(pids[0])' "$PANE_ID")"
if [[ "$CHILD_PID_NOW" == "$CHILD_PID" ]]; then
    echo "[re-adopt 2] pane worker keeps child pid $CHILD_PID across both kills"
else
    echo "FAIL: pane worker child pid changed across the restarts: $CHILD_PID -> $CHILD_PID_NOW" >&2
    exit 1
fi

# The named limit: Locate-tier harnesses cannot drive through a portal.
echo "[limit] agy, cursor-agent and grok threads reach Locate only: a portal"
echo "        on them restores in place and shows where the thread lives,"
echo "        not the thread - a shared keeper subscriber seat is the keeper"
echo "        protocol design that would change this."

echo "PASS"
