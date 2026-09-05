#!/bin/bash
# repro-x-1b88: end-to-end arms-readout proof in a sandbox, no machine state touched.
#
# Emits five real control_plane_tick rows through the Python emitter into a
# pinned sandbox journal, then reads them back through the Rust reader
# (`fno-agents status --json`) with FNO_AGENTS_HOME pointed at the same
# sandbox, and asserts the readout renders every arm green. Proves the whole
# chain this repo ships (emitter -> journal -> reader -> staleness verdict)
# without deploying anything onto the live daemon or launchd agents.
#
# The live post-deploy check is the readout against the REAL journals:
#   fno agents status          (arms table, red when an arm stopped ticking)
#   fno doctor event find control_plane_tick --since 1h
# No pipefail on purpose: the status pipeline's VERDICT is the python assert's
# exit, not fno-agents' own (a degraded daemon-under-sandbox exits 13 while the
# arms readout - the thing under test - is green).
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Short dir name: the daemon socket under FNO_AGENTS_HOME must stay under
# SUN_LEN (~104 bytes) and ROOT is already long.
T="$ROOT/.fno/r188"
rm -rf "$T"
mkdir -p "$T/ah"

FNO_EVENTS_PATH="$T/ah/events.jsonl" uv run --project "$ROOT/cli" python -c '
from fno.control_plane import emit_tick
for arm in ("king_wake", "watchdog", "pr_watch_merge", "active_backlog", "auto_continue"):
    assert emit_tick(arm, scheduler="probe", interval_s=600, acted=1), arm
print("emitted 5 tick rows")
'

BIN="$ROOT/crates/fno-agents/target/debug/fno-agents"
if [[ ! -x "$BIN" ]]; then
  cargo build --manifest-path "$ROOT/crates/fno-agents/Cargo.toml" --bins >&2
fi

FNO_AGENTS_HOME="$T/ah" \
FNO_AGENTS_RUNTIME=rust \
FNO_AGENTS_BIN="$BIN" \
fno agents status --json | python3 -c '
import json, sys
d = json.load(sys.stdin)
arms = d["arms"]
stale = [a["arm"] for a in arms if a.get("stale")]
assert len(arms) >= 5 and not stale, (len(arms), stale)
print("arms green", len(arms))
'
rm -rf "$T"
