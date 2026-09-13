#!/usr/bin/env bash
# The workspace-standalone conformance journey (product-boundary proof).
#
# Proves, with the REAL mux binary, that the plain workspace works with every
# optional backend absent or erroring: server start, plain shell panes, split,
# unique markers through each child, the same live child across viewer
# detachment, doctor diagnostics naming missing components as advisory, and a
# full-component control leg. Erroring sentinels for the optional backends
# (python3, uv, fno-agents, fno-agents-worker) fail visibly if anything calls
# them. Never claims reboot or server-kill survival.
#
# Emits WORKSPACE_STANDALONE_PASS only when every leg passes. Never touches a
# real user's mux state: FNO_MUX_DIR, HOME and cwd are all temp-isolated.

set -uo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
FNO="$REPO_ROOT/crates/fno/target/debug/fno"

log() { printf '%s\n' "$*"; }
fail() { log "FAIL: $*"; exit 1; }
step() { log "== $*"; }

command -v jq >/dev/null 2>&1 || fail "jq is required (brew install jq)"
command -v script >/dev/null 2>&1 || fail "script(1) is required for the viewer leg"

step "building the real binaries (cargo build)"
cargo build --manifest-path crates/fno/Cargo.toml --bin fno -q || fail "fno build"
cargo build --manifest-path crates/fno-agents/Cargo.toml -q || fail "runtime binaries build"
[ -x "$FNO" ] || fail "the fno binary did not land at $FNO"

TMP_ROOT=$(mktemp -d /tmp/fno-ws.XXXXXX 2>/dev/null || mktemp -d "${TMPDIR:-/tmp}/fno-ws.XXXXXX")
mkdir -p "$TMP_ROOT/mux" "$TMP_ROOT/home/.fno" "$TMP_ROOT/work"
# Pre-seed the migration sentinel + settings so the pane login-shell wrapper
# does not trigger first-run Python provisioning inside the journey.
: > "$TMP_ROOT/home/.fno/settings.yaml"
touch "$TMP_ROOT/home/.fno/.path-migration-done"
export FNO_MUX_DIR="$TMP_ROOT/mux"
export HOME="$TMP_ROOT/home"
export FNO_SENTINEL_LOG="$TMP_ROOT/sentinels.log"
touch "$FNO_SENTINEL_LOG"
UNIQ="$$"
SENT="$TMP_ROOT/sentinels"
mkdir -p "$SENT"

S1="wsa-$UNIQ"
S2="wsc-$UNIQ"
cleanup() {
  [ -n "${S1:-}" ] && FNO_SERVER="$S1" "$FNO" mux kill-server >/dev/null 2>&1
  [ -n "${S2:-}" ] && FNO_SERVER="$S2" "$FNO" mux kill-server >/dev/null 2>&1
  rm -rf "$TMP_ROOT"
}
trap cleanup EXIT

# Erroring sentinels for every optional backend. One direct invocation is the
# POSITIVE CONTROL for the log itself: if the log never records a real call,
# a later "log is empty" would be vacuous.
for name in python3 uv fno-agents fno-agents-worker fno-agents-daemon; do
  cat > "$SENT/$name" <<'EOF'
#!/bin/sh
printf 'sentinel invoked: %s %s\n' "$(basename "$0")" "$*" >> "$FNO_SENTINEL_LOG"
exit 97
EOF
  chmod +x "$SENT/$name"
done

"$SENT/fno-agents" --control-probe >/dev/null 2>&1
grep -q "sentinel invoked: fno-agents --control-probe" "$FNO_SENTINEL_LOG" \
  || fail "sentinel log control: the log did not record a direct invocation"
: > "$FNO_SENTINEL_LOG"

# The isolated environment every leg runs under. Sentinels sit FIRST on PATH.
export PATH="$SENT:/usr/bin:/bin"
export FNO_SERVER="$S1"

# ── Leg 1: the journey under erroring sentinels ────────────────────────────
step "leg 1: journey under erroring sentinels (components=sentinels)"

"$FNO" --server "$FNO_MUX_DIR/$S1.sock" >"$TMP_ROOT/server.log" 2>&1 &
SERVER_PID=$!
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
  [ -S "$FNO_MUX_DIR/$S1.sock" ] && break
  sleep 1
done
[ -S "$FNO_MUX_DIR/$S1.sock" ] || {
  log "--- server.log:"
  cat "$TMP_ROOT/server.log" 2>/dev/null
  fail "the mux server never bound its socket"
}

PANES0=$("$FNO" mux pane ls --json 2>/dev/null | jq 'length')
[ "$PANES0" = "0" ] || fail "a fresh server carries unexpected panes: $PANES0"

PA=$("$FNO" mux pane run --cwd "$TMP_ROOT/work" -- /bin/bash | tr -d '[:space:]')
case "$PA" in ''|*[!0-9]*) fail "first pane id is not numeric: '$PA'";; esac

# The split is the REAL PaneSplit verb on the existing pane, not a second
# run: the journey claims to prove split, so it must drive split. The split
# receipt prints the new pane id; the ls that follows confirms it exists.
PB=$("$FNO" mux pane split "$PA" --direction right | tr -d '[:space:]')
case "$PB" in ''|*[!0-9]*|0) fail "split produced no usable pane id: '$PB'";; esac
sleep 1
SPLIT_LIVE=$("$FNO" mux pane ls --json | jq -r --argjson id "$PB" \
  '[.[].pane_id] | index($id) != null')
[ "$SPLIT_LIVE" = "true" ] || fail "the split pane $PB is not listed by the server"
log "panes: $PA (run) $PB (split)"

send_marker() {
  local pane="$1" tag="$2" file="$3"
  # --raw: plain keystrokes. The default guarded send renders through the
  # Python porcelain, which is a delivery-backed view, not workspace behavior.
  # The marker exchange is proven by the FILE the child writes: a split pane's
  # grid stays empty until a client views it, so pane output is not a witness
  # here, but tee's file is the child's own receipt.
  "$FNO" mux pane send "$pane" --text "echo $tag | tee $file" --submit --raw >/dev/null 2>&1 \
    || fail "pane send failed for pane $pane"
  local tries=0
  while [ "$tries" -lt 10 ]; do
    sleep 1
    [ -f "$file" ] && grep -q "$tag" "$file" && return 0
    tries=$((tries + 1))
  done
  fail "marker $tag never landed in $file from pane $pane"
}

MA1="$TMP_ROOT/mark-a1.txt"
MB1="$TMP_ROOT/mark-b1.txt"
send_marker "$PA" "marker-a1-$UNIQ" "$MA1"
send_marker "$PB" "marker-b1-$UNIQ" "$MB1"
log "markers through both children: ok"

PID_A=$("$FNO" mux pane ls --json | jq -r --argjson id "$PA" '.[] | select(.pane_id == $id) | .child_pid')
PID_B=$("$FNO" mux pane ls --json | jq -r --argjson id "$PB" '.[] | select(.pane_id == $id) | .child_pid')
case "$PID_A" in ''|0) fail "no child_pid for pane $PA";; esac

# Viewer attach under a real PTY, then a hard detach (viewer death). The pane
# keeper owns the pty master, so the same child must keep answering.
case "$(uname -s)" in
  Darwin) script -q /dev/null "$FNO" --session "$S1" >"$TMP_ROOT/viewer.log" 2>&1 & ;;
  *) script -qec "$FNO --session $S1" /dev/null >"$TMP_ROOT/viewer.log" 2>&1 & ;;
esac
VIEWER_PID=$!
sleep 3
pgrep -f "session $S1" >/dev/null 2>&1 || log "note: viewer attach not visible to pgrep (continuing)"
kill -9 "$VIEWER_PID" 2>/dev/null
sleep 2
if pgrep -f "session $S1" >/dev/null 2>&1; then
  pkill -9 -f "session $S1" 2>/dev/null
  log "note: the attach child outlived its pty wrapper and was killed"
fi
sleep 1

PID_A2=$("$FNO" mux pane ls --json | jq -r --argjson id "$PA" '.[] | select(.pane_id == $id) | .child_pid')
[ "$PID_A" = "$PID_A2" ] || fail "child pid changed across viewer detach: $PID_A -> $PID_A2"
kill -0 "$PID_A2" 2>/dev/null || fail "the pane child is not live after viewer detach"

MA2="$TMP_ROOT/mark-a2.txt"
send_marker "$PA" "marker-a2-$UNIQ" "$MA2"
log "same live child across viewer detach: ok"

[ ! -s "$FNO_SENTINEL_LOG" ] || {
  log "sentinel log contents:"
  cat "$FNO_SENTINEL_LOG"
  fail "an optional backend was invoked during the plain journey"
}
log "sentinels never invoked: ok"

FNO_SERVER="$S1" "$FNO" mux kill-server >/dev/null 2>&1

# ── Leg 2: missing backends, diagnostic is advisory ─────────────────────────
step "leg 2: doctor names missing backends, exit stays 0 (components=none)"
(
  # A dev checkout's paired-binary climb finds built siblings, and a smoke
  # harness may pin FNO_AGENTS_FRONT (the resolver's second seam) at the real
  # runtime dir, so absence is forced through BOTH override seams plus a
  # strict PATH.
  export PATH="/usr/bin:/bin"
  export FNO_AGENTS_BIN="/nonexistent/fno-agents"
  export FNO_AGENTS_WORKER="/nonexistent/fno-agents-worker"
  export FNO_AGENTS_FRONT="/nonexistent/fno-agents"
  cd "$TMP_ROOT/work"
  OUT=$("$FNO" mux doctor 2>/dev/null)
  RC=$?
  echo "$OUT" > "$TMP_ROOT/doctor-missing.txt"
  [ "$RC" = "0" ] || fail "doctor exited $RC with missing optional backends"
  echo "$OUT" | grep -q "optional component fno-agents: warn - fno-agents not found" \
    || fail "doctor does not name the missing runtime"
  echo "$OUT" | grep -q "optional component fno-agents-worker: warn - fno-agents-worker not found" \
    || fail "doctor does not name the missing worker"
) || fail "leg 2 failed"

# ── Leg 3: full-component control ───────────────────────────────────────────
step "leg 3: full-component control (components=full)"
export PATH="/usr/bin:/bin"
export FNO_AGENTS_BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"
export FNO_AGENTS_WORKER="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker"
(
  cd "$TMP_ROOT/work"
  OUT=$("$FNO" mux doctor --json 2>/dev/null)
  echo "$OUT" > "$TMP_ROOT/doctor-full.json"
  echo "$OUT" | jq -e '.dependencies | length >= 3' >/dev/null \
    || fail "doctor --json carries no dependencies array"
  echo "$OUT" | jq -e '.dependencies[] | select(.component == "fno-agents" and .availability == "available")' >/dev/null \
    || fail "the control fixture does not see the real runtime as available"
  echo "$OUT" | jq -e '.dependencies[] | select(.component == "fno-agents-worker" and .availability == "available")' >/dev/null \
    || fail "the control fixture does not see the real worker as available"
) || fail "leg 3 doctor failed"

# The control journey: the same pane mechanics with real components present.
export FNO_SERVER="$S2"
"$FNO" --server "$FNO_MUX_DIR/$S2.sock" >"$TMP_ROOT/server2.log" 2>&1 &
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
  [ -S "$FNO_MUX_DIR/$S2.sock" ] && break
  sleep 1
done
[ -S "$FNO_MUX_DIR/$S2.sock" ] || {
  log "--- server2.log:"
  cat "$TMP_ROOT/server2.log" 2>/dev/null
  fail "the control server never bound its socket"
}
PC=$("$FNO" mux pane run --cwd "$TMP_ROOT/work" -- /bin/bash | tr -d '[:space:]')
MC1="$TMP_ROOT/mark-c1.txt"
send_marker "$PC" "marker-c1-$UNIQ" "$MC1"
log "control journey marker: ok"
FNO_SERVER="$S2" "$FNO" mux kill-server >/dev/null 2>&1

log "every leg passed; the plain workspace needed no optional backend"
echo "WORKSPACE_STANDALONE_PASS"
