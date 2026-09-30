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
ROOT="$(mktemp -d /tmp/fno-demo.XXXXXX)"
PANES=()

cleanup() {
  for p in "${PANES[@]+"${PANES[@]}"}"; do
    "$FNO" mux pane kill "$p" --server "$SERVER" >/dev/null 2>&1 || true
  done
  "$FNO" mux kill-server "$SERVER" --end-unkept >/dev/null 2>&1 || true
  if [ -n "${FNO_DEMO_KEEP:-}" ]; then echo "kept $ROOT" >&2; else rm -rf "$ROOT"; fi
}
trap cleanup EXIT

mkdir -p "$ROOT/state" "$ROOT/mux" "$ROOT/agents" "$ROOT/code/checkout" "$ROOT/text"
printf 'schema_version = 1\nstate_dir = "%s"\n' "$ROOT/state" >"$ROOT/config.toml"
export FNO_CONFIG="$ROOT/config.toml" FNO_MUX_DIR="$ROOT/mux" FNO_AGENTS_HOME="$ROOT/agents"
unset FNO_SERVER FNO_SESSION FNO_PANE
# The sideline opens on the backlog, the way the product is pitched.
printf '{"sideline_view":"backlog"}\n' >"$ROOT/agents/mux-view.json"

cd "$ROOT/code/checkout"
for title in \
  "Rate limit the checkout api per key" \
  "Retry webhooks with backoff and a dead-letter queue" \
  "Show order status on the receipt page" \
  "Cache product search for 60 seconds" \
  "Split the payments module out of the monolith" \
  "Add an audit log for refunds" \
  "Page the on-call when p95 latency passes 800ms" \
  "Remove the legacy coupon service"; do
  "$FNO" backlog idea "$title" --project checkout --difficulty medium >/dev/null
done

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

show() { printf "printf '\\\\033[?25l\\\\033[2J\\\\033[H'; cat '%s'; exec sleep 3600" "$1"; }
run() { # run <args...> : start a pane, print its id
  "$FNO" mux pane run --server "$SERVER" --json "$@" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pane_id"])'
}

MAIN="$(run --workspace checkout --cwd "$ROOT/code/checkout" --worker archer -- sh -c "$(show "$ROOT/text/codex.txt")")"
PANES+=("$MAIN")
RIGHT="$(run --workspace checkout --at "$MAIN" --split right --worker scout -- sh -c "$(show "$ROOT/text/claude.txt")")"
PANES+=("$RIGHT")
LOW="$(run --workspace checkout --at "$MAIN" --split down --worker reviewer -- sh -c "$(show "$ROOT/text/opencode.txt")")"
PANES+=("$LOW")

# The registry rows that name each pane's harness, model and state.
python3 - "$ROOT/agents/registry.json" "$SERVER" "$MAIN" "$RIGHT" "$LOW" "$ROOT/code/checkout" <<'PY'
import json, sys, datetime
path, server, main, right, low, cwd = sys.argv[1:]
now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
def row(name, harness, model, pane, state):
    return {
        "name": name, "cwd": cwd, "harness": harness, "model": model,
        "status": "live", "liveness": "alive", "liveness_measured_at": now,
        "created_at": now, "last_message_at": now, "substrate": "pane",
        "mux": {"session": server, "pane_id": int(pane)},
        "inside_leg": {"state": state, "seq": 1, "received_at": now},
    }
json.dump({"schema_version": 1, "agents": [
    row("archer", "codex", "gpt-6-sol", main, "working"),
    row("scout", "claude", "opus", right, "done"),
    row("reviewer", "opencode", "zen", low, "blocked"),
]}, open(path, "w"))
PY
sleep 2

"$FNO" mux serve --snapshot --server "$SERVER" "$@"
