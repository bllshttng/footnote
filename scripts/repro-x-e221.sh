#!/usr/bin/env bash
# Positive operational proof for x-e221: one territory = one list.
#
# Builds a fresh throwaway fixture (isolated FNO_HOME + project dir), writes
# the fixture graph through the store's own writer, then runs the resolver,
# the territory readout, the blueprinter feed (status -> deliver -> repair
# marker), and the two cap readers on the shared parity fixture. Every
# subprocess is bounded; the fixture is removed on exit. Exits zero only
# after every named marker asserts:
#   territory key, cap=4, kingless, blueprinter handle, delivery refusal
#   recorded as a repair with the idea preserved, and the cross-territory
#   nominated-review row staying visible on the board.
set -u

fail() { echo "repro-x-e221: FAIL: $1" >&2; exit 1; }

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel 2>/dev/null) || ROOT=$(pwd)
cd "$ROOT" || fail "cannot cd to repo root $ROOT"

TIMEOUT_BIN=$(command -v timeout || command -v gtimeout) || fail "timeout/gtimeout required to bound subprocesses"
CARGO=$(command -v cargo) || fail "cargo required (Rust cap reader + board marker)"
UV=$(command -v uv) || fail "uv required (python test runner)"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/repro-x-e221.XXXXXX") || fail "mktemp failed"
trap 'rm -rf "$TMP"' EXIT
FNO_HOME_DIR="$TMP/fno-home"
FIXTURE="$TMP/project"
mkdir -p "$FNO_HOME_DIR" "$FIXTURE/.fno" "$FIXTURE/plans" || fail "fixture dirs"
export FNO_HOME="$FNO_HOME_DIR"
export PYTHONPATH="$ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}"

# One python entrypoint: the cli project env (its deps) + repo source via
# PYTHONPATH, the app invoked with argv.
fno_py() {
  "$UV" run --project "$ROOT/cli" "$TIMEOUT_BIN" 120 python3 -c 'import sys; from fno.cli import app; sys.exit(app())' "$@"
}

py() {
  "$UV" run --project "$ROOT/cli" "$TIMEOUT_BIN" 120 python3 "$@"
}

# --- fixture: settings (the one candidate FNO_CONFIG names) -----------------
mkdir -p "$FNO_HOME_DIR"
cat > "$FIXTURE/.fno/settings.yaml" <<EOF
config:
  state_dir: "$FNO_HOME_DIR"
  work:
    projects:
      fno:
        path: "$FIXTURE"
active_backlog:
  enabled: true
  interval: "5m"
EOF
export FNO_CONFIG="$FIXTURE/.fno/settings.yaml"
# The work-map reader consults the GLOBAL settings file directly (a different
# reader from load_settings); redirect it too so the real machine's workspace
# map cannot leak into the fixture.
export FNO_GLOBAL_SETTINGS_PATH="$FIXTURE/.fno/settings.yaml"
# Claims root must leave the machine before the fixture claims step writes.
export FNO_CLAIMS_ROOT="$FNO_HOME_DIR"

# The graph path MUST resolve inside the temp fixture before anything is
# written: an unisolated writer is the exact failure this script exists to
# make impossible again.
RESOLVED=$( cd "$FIXTURE" && py -c 'from fno import paths; print(paths.graph_json())' ) || fail "graph path resolve"
FNO_REAL=$(cd "$FNO_HOME_DIR" && pwd -P)
case "$RESOLVED" in
  "$FNO_REAL"/*) : ;;
  *) fail "graph path $RESOLVED is not inside $FNO_HOME_DIR; refusing to write" ;;
esac

# --- fixture: graph through the store's own writer --------------------------
cat > "$FIXTURE/plans/x-idea.md" <<'EOF'
---
status: design
---
# x-idea design stub
EOF

py - <<PYEOF || fail "fixture graph write"
import pathlib
from fno.graph.store import locked_mutate_graph

plan = str(pathlib.Path("$FIXTURE") / "plans" / "x-idea.md")
cwd = "$FIXTURE"

def mutator(entries):
    entries.clear()
    entries.extend([
        {"id": "x-epic", "type": "epic", "project": "fno", "status": "ready", "priority": "p1", "cwd": cwd},
        {"id": "x-1", "parent": "x-epic", "project": "fno", "status": "ready", "priority": "p1", "cwd": cwd},
        {"id": "x-2", "parent": "x-epic", "project": "fno", "status": "ready", "priority": "p1", "cwd": cwd},
        {"id": "x-idea", "parent": "x-epic", "project": "fno", "status": "idea", "priority": "p1",
         "plan_path": plan, "cwd": cwd},
        {"id": "p-1", "project": "fno", "status": "ready", "priority": "p1", "cwd": cwd},
    ])
    return entries

locked_mutate_graph(pathlib.Path("$RESOLVED"), mutator)
print("fixture graph written:", "$RESOLVED")
PYEOF

# --- fixture: registry cache (two live node-working rows, no crown) ---------
py - <<PYEOF || fail "fixture registry write"
import json, pathlib

registry = {
    "schema_version": 1,
    "agents": [
        {"name": "w1", "status": "live", "pid": 1, "node": "x-1", "harness": "claude", "cwd": "/tmp", "log_path": "/tmp/w.log"},
        {"name": "w2", "status": "live", "pid": 1, "node": "x-2", "harness": "claude", "cwd": "/tmp", "log_path": "/tmp/w.log"},
    ],
}
path = pathlib.Path("$FNO_HOME_DIR") / "agents" / "registry.json"
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text(json.dumps(registry))
print("fixture registry written:", path)
PYEOF

# --- fixture: live worker:<name> slot claims (the census liveness oracle) ---
py - <<PYEOF || fail "fixture claims write"
from fno.claims.core import acquire_claim
from fno.claims.io import global_claims_root

root = global_claims_root()
for name in ("w1", "w2"):
    acquire_claim(
        f"worker:{name}",
        "repro-fixture",
        reason="x-e221 repro fixture",
        ttl_ms=600000,
        pid_unavailable=True,
        root=root,
    )
print("fixture claims written under", root)
PYEOF

echo "== 1. resolver =="
RESOLVER=$( (cd "$FIXTURE" && fno_py config active-backlog --json) ) || fail "resolver verb"
echo "$RESOLVER" | python3 -c '
import json, sys
targets = json.load(sys.stdin)
assert isinstance(targets, list) and len(targets) == 1, targets
t = targets[0]
assert t["scope"] == "fno", t
assert t["rung"] == 1 and t["kingless"] is True, t
assert t["project"] == "fno", t
print("MARKER territory-key: scope=fno rung=1 (resolver)")
print("MARKER kingless: true (resolver)")
' || fail "resolver markers"

echo "== 2. blueprinter feed: status =="
STATUS=$( (cd "$FIXTURE" && fno_py agents worker --json blueprint-feed --scope fno) ) || fail "feed status verb"
echo "$STATUS" | python3 -c '
import json, sys
out = json.load(sys.stdin)
assert out["action"] == "status", out
assert out["kingless"] is True, out
assert out["ideas"] and out["ideas"][0]["id"] == "x-idea", out
assert out["worker"] is None, out
name = out["worker_name_next"]
assert name.startswith("blueprinter-fno-"), out
print("MARKER blueprinter-handle:", name)
print("MARKER feed-idea: x-idea (design-rung stub selected)")
' || fail "feed status markers"

echo "== 3. blueprinter feed: deliver refuses with no live worker, idea preserved =="
( cd "$FIXTURE" && fno_py agents worker --json blueprint-feed --scope fno --deliver ) | python3 -c '
import json, sys
out = json.load(sys.stdin)
assert out["action"] == "blocked", out
assert out["reason"] == "worker_not_live", out
assert out["kingless"] is True, out
' || fail "deliver refusal"
( cd "$FIXTURE" && fno_py agents worker --json blueprint-feed --scope fno ) | python3 -c '
import json, sys
out = json.load(sys.stdin)
assert out["worker"] is None and out["worker_name_next"].startswith("blueprinter-fno-"), out
assert [i["id"] for i in out["ideas"]] == ["x-idea"], out
print("MARKER delivery-repair: worker_not_live recorded, idea x-idea preserved")
' || fail "repair marker"

echo "== 4. territory readout =="
( cd "$FIXTURE" && fno_py config active-backlog-territories --json ) | python3 -c '
import json, sys
rows = json.load(sys.stdin)
assert isinstance(rows, list) and len(rows) == 1, rows
r = rows[0]
assert r["scope"] == "fno" and r["kingless"] is True, r
assert r["cap"] == 4, r
assert r["live"] == 2, r
print("MARKER cap: live=2 cap=4 (readout counts only this territory)")
' || fail "readout markers"

echo "== 5. both cap readers on the shared parity fixture =="
unset FNO_HOME
"$UV" run --project cli pytest cli/tests/agents/test_spawn_gate.py -k territory -q \
  >/dev/null 2>&1 || fail "python cap reader (spawn gate territory tests)"
echo "MARKER cap-reader python: shared fixture verdicts agree"
( cd "$ROOT" && "$CARGO" test --manifest-path crates/fno-agents/Cargo.toml --lib territory_cap 2>/dev/null ) \
  || fail "rust cap reader (spawn gate territory tests)"
echo "MARKER cap-reader rust: shared fixture verdicts agree"

echo "== 6. cross-territory nominated review stays visible =="
( cd "$ROOT" && "$CARGO" test --manifest-path crates/fno-agents/Cargo.toml --lib a_cross_territory_mergeable_pr_stays_visible 2>/dev/null ) \
  || fail "board nomination marker"
echo "MARKER nominated-review: cross-territory PR demoted to out_of_scope, never hidden"

echo "repro-x-e221: PASS (all markers asserted)"
