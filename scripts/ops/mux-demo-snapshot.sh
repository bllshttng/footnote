#!/usr/bin/env bash
# Shoot the mux for a public page without one byte of a real session in it.
#
#   scripts/ops/mux-demo-snapshot.sh --out shot.svg [--theme light] [--format png] [--size 200x60]
#
# Builds a throwaway server in a temp state root: its own config, graph,
# agent registry, view prefs and socket dir. It seeds invented backlog nodes
# and three harness panes, shoots it with `fno mux serve --snapshot --server`,
# then kills the server and deletes the root. The live mux is never touched.
set -euo pipefail

FNO="${FNO_BIN:-fno}"
SERVER=demo
# /tmp, not TMPDIR: a socket path must stay under 104 bytes on macOS.
# pwd -P: macOS resolves /tmp to /private/tmp, and HOME must match the real path.
ROOT="$(cd "$(mktemp -d /tmp/fno-demo.XXXXXX)" && pwd -P)"
PANES=()

cleanup() {
  for p in "${PANES[@]+"${PANES[@]}"}"; do
    "$FNO" mux pane kill "$p" --server "$SERVER" >/dev/null 2>&1 || true
  done
  "$FNO" mux kill-server "$SERVER" --end-unkept >/dev/null 2>&1 || true
  if [ -n "${FNO_DEMO_KEEP:-}" ]; then echo "kept $ROOT" >&2; else rm -rf "$ROOT"; fi
}
trap cleanup EXIT

mkdir -p "$ROOT/mux" "$ROOT/agents" "$ROOT/code/checkout" "$ROOT/text"
# State under $ROOT/.fno: the mux board reads the graph from $HOME/.fno.
printf 'schema_version = 1\nstate_dir = "%s"\n' "$ROOT/.fno" >"$ROOT/config.toml"
export FNO_CONFIG="$ROOT/config.toml" FNO_MUX_DIR="$ROOT/mux" FNO_AGENTS_HOME="$ROOT/agents"
unset FNO_SERVER FNO_SESSION FNO_PANE FNO_OWNER_BIRTH
# Under an owner session the server lives as long as its owner. Left alone,
# the owner is the first short-lived `pane run`, and the server shuts down
# before the shot. This script owns it instead, so it also dies with us.
export FNO_OWNER_PID=$$
# The sideline opens on the backlog, the way the product is pitched.
printf '{"sideline_view":"backlog","experimental_backlog_view":true}\n' >"$ROOT/agents/mux-view.json"

cd "$ROOT/code/checkout"
# Invented work. The board's lanes come from priority, and its scope from
# the server's project, so these stay unscoped.
IDS=()
file() { # file <priority> <title> : record the id, show the demo's home path
  local id
  id="$("$FNO" backlog idea "$2" -p "$1" --difficulty medium | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
  "$FNO" backlog update "$id" --set cwd='~/code/checkout' >/dev/null
  IDS+=("$id")
}
file p1 "Rate limit the checkout api per key"
file p1 "Retry webhooks with backoff and a dead-letter queue"
file p1 "Cache product search for 60 seconds"
file p1 "Show order status on the receipt page"
file p1 "Page the on-call when p95 latency passes 800ms"
file p2 "Add an audit log for refunds"
file p2 "Split the payments module out of the monolith"
file p2 "Remove the legacy coupon service"
file p3 "Move image resizing to a queue worker"
file p3 "Support Apple Pay on the checkout page"

# The three panes work the first three nodes: a claim moves each node to In
# Progress, and a session row names the harness session its pane runs.
SIDS=(7d1e2f3a-4b5c-4d6e-8f70-81a2b3c4d5e6 0199a1b2-c3d4-7e5f-8a6b-7c8d9e0f1a2b ses_3f9a1c2b7e4dA1b2C3d4E5f6g7)
HARNESSES=(claude codex opencode)
for i in 0 1 2; do
  "$FNO" agents claim acquire "node:${IDS[$i]}" --holder "target-session:${SIDS[$i]}" --ttl 2h --pid-unavailable >/dev/null
  "$FNO" backlog session add "${IDS[$i]}" --phase execute --harness "${HARNESSES[$i]}" --session-id "${SIDS[$i]}" >/dev/null
done
# The selected card carries a plan and a rank. --operator: this script is
# the operator's own tool, writing a graph that dies with the shot.
mkdir -p "$ROOT/code/checkout/plans"
printf -- '---\nstatus: ready\n---\n# Rate limit the checkout api per key\n' >"$ROOT/code/checkout/plans/rate-limit.md"
"$FNO" backlog update "${IDS[0]}" --plan-path '~/code/checkout/plans/rate-limit.md' >/dev/null
"$FNO" backlog rank "${IDS[0]}" --top --operator >/dev/null

# Pane text: an invented transcript per harness, shown by a process that
# never prints a prompt. The cursor is hidden, so no block reads as a glyph.
E=$'\033'
cat >"$ROOT/text/claude.txt" <<EOF
${E}[36m> rate limit the checkout api per key${E}[0m

${E}[2m  Read src/checkout/handler.ts${E}[0m
${E}[2m  Read src/middleware/limits.ts${E}[0m

  The handler has no limit today. I will add a token bucket per api
  key: 60 requests a minute, and a 429 with Retry-After when it empties.

${E}[32m  Edited src/middleware/limits.ts  +42 -3${E}[0m
${E}[32m  Edited src/checkout/handler.ts   +6 -1${E}[0m

  Running npm test -- limits
${E}[32m  14 passed${E}[0m

  Opened PR 118: rate limit the checkout api per key.
EOF
cat >"$ROOT/text/codex.txt" <<EOF
${E}[1m> retry webhooks with backoff${E}[0m

${E}[2m  Ran rg -n "deliver\(" src/webhooks${E}[0m
  Delivery is one attempt today. A failed call is dropped.

  Plan:
  1. Retry 5 times, doubling from 2s.
  2. Move the event to a dead-letter queue after the last try.
  3. Add a replay command for the queue.

${E}[2m  Working (2m 14s)${E}[0m
EOF
cat >"$ROOT/text/opencode.txt" <<EOF
${E}[1m> cache product search for 60 seconds${E}[0m

  Search runs the full query on every keypress.
  I added a 60s cache keyed by the normalized query.

${E}[32m  Edited src/search/index.ts  +18 -2${E}[0m
${E}[33m  Waiting on review before I push.${E}[0m
EOF
cat >"$ROOT/text/pi.txt" <<EOF
${E}[1m> page the on-call when p95 passes 800ms${E}[0m

  Added an alert rule and a runbook link.
${E}[32m  3 files changed${E}[0m
EOF
cat >"$ROOT/text/docs.txt" <<EOF
${E}[1m> document the refund audit log${E}[0m

  Drafting docs/refunds.md from the new schema.
EOF

# Each pane prints its text, hides the cursor, and prints it again on every
# resize, so the sizing attach below lands on a full screen.
show() {
  printf "draw() { printf '\\\\033[?25l\\\\033[2J\\\\033[H'; cat '%s'; }; trap draw WINCH; draw; while :; do sleep 1; done" "$1"
}
run() { # run <args...> : start a pane, record its id
  PANES+=("$("$FNO" mux pane run --server "$SERVER" --json "$@" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pane_id"])')")
}

W=(--workspace checkout)
# The first tab stays the active one, so the three-pane tab comes first.
run "${W[@]}" --cwd "$ROOT/code/checkout" --worker archer -- sh -c "$(show "$ROOT/text/codex.txt")"
run "${W[@]}" --tab 1 --at "${PANES[0]}" --split right --worker scout -- sh -c "$(show "$ROOT/text/claude.txt")"
run "${W[@]}" --tab 1 --at "${PANES[0]}" --split down --worker reviewer -- sh -c "$(show "$ROOT/text/opencode.txt")"
run "${W[@]}" --worker pager -- sh -c "$(show "$ROOT/text/pi.txt")"
run "${W[@]}" --worker scribe -- sh -c "$(show "$ROOT/text/docs.txt")"
"$FNO" mux tab rename --server "$SERVER" "${W[@]}" --tab 1 --name agents >/dev/null
"$FNO" mux tab rename --server "$SERVER" "${W[@]}" --tab 2 --name alerts >/dev/null
"$FNO" mux tab rename --server "$SERVER" "${W[@]}" --tab 3 --name docs >/dev/null

# The registry rows that name each pane's harness, model and state. The
# server reads the registry on an interval, so wait a few seconds for it.
python3 - "$ROOT/agents/registry.json" "$SERVER" "$ROOT/code/checkout" "${SIDS[@]}" "${PANES[@]}" <<'PY'
import json, sys, datetime
path, server, cwd = sys.argv[1:4]
sids = sys.argv[4:7]
ids = [int(p) for p in sys.argv[7:]]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
def row(name, harness, model, pane, state, sid=None):
    return {"harness_session_id": sid} | {
        "name": name, "cwd": cwd, "harness": harness, "model": model,
        "status": "live", "liveness": "alive", "liveness_measured_at": now,
        "created_at": now, "last_message_at": now, "substrate": "pane",
        "mux": {"session": server, "pane_id": pane},
        "inside_leg": {"state": state, "seq": 1, "received_at": now},
    }
json.dump({"schema_version": 1, "agents": [
    row("archer", "codex", "gpt-6-sol", ids[0], "working", sids[1]),
    row("scout", "claude", "opus", ids[1], "done", sids[0]),
    row("reviewer", "opencode", "zen", ids[2], "blocked", sids[2]),
    row("pager", "pi", "glm-5", ids[3], "working"),
    row("scribe", "claude", "sonnet", ids[4], "done"),
]}, open(path, "w"))
PY

sleep 6

# The server reads the registry only while a viewer is attached, and the
# first attach ends before the rows join. A warm-up shot starts that read.
HOME="$ROOT" "$FNO" mux serve --snapshot --server "$SERVER" --size 200x56 --fit --squad checkout --out "$ROOT/warm.svg" >/dev/null
sleep 3

# The flags after ours win. HOME makes the status row read ~/code/checkout.
HOME="$ROOT" "$FNO" mux serve --snapshot --server "$SERVER" --size 200x56 --fit --squad checkout "$@"
