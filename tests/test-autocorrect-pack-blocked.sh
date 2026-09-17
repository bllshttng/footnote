#!/usr/bin/env bash
# test-autocorrect-pack-blocked.sh
#
# autocorrect-pack.sh emits a BLOCKED-state section read through the store
# api. It once read `.nodes` out of graph.json while the entry list lived
# under `.entries`, so the selector ran against an empty list on every
# invocation and the packet shipped an empty section for its whole life.
# jq's stderr was discarded too, so a read that never matched anything was
# indistinguishable from a read that found nothing.
#
# These are the checks that would have caught it: a fixture store with known
# blocked nodes must show up in the packet, and a store that cannot be read
# must read as NOT READ (failed), never as an empty answer.
#
# Exit codes: 0 pass / 1 assertion failed / 77 skipped (missing deps)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PACK="${REPO_ROOT}/scripts/autocorrect-pack.sh"

pass() { printf '[autocorrect-blocked] PASS: %s\n' "$*"; }
fail() { printf '[autocorrect-blocked] FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf '[autocorrect-blocked] SKIP: %s\n' "$*" >&2; exit 77; }

# The store read needs a keeper to answer; without the worker binary the
# fixture store cannot exist. The smoke gate builds the crate's debug bins
# (structural "Build fno-agents debug binary" step, or the auto-inserted
# build this marker line triggers in changed-smoke: target/debug/fno-agents-worker),
# so the repo checkout's own build output is the third arm here.
worker_resolvable() {
  if [[ -n "${FNO_AGENTS_WORKER:-}" && -f "$FNO_AGENTS_WORKER" ]]; then return 0; fi
  if [[ -x "$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker" ]]; then
    FNO_AGENTS_WORKER="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents-worker"
    export FNO_AGENTS_WORKER
    return 0
  fi
  command -v fno-agents-worker >/dev/null 2>&1
}
worker_resolvable || skip "no fno-agents-worker (FNO_AGENTS_WORKER unset, binary not on PATH)"
[[ -f "$PACK" ]] || fail "not found: $PACK"
command -v python3 >/dev/null 2>&1 || skip "python3 not on PATH"

TMP="$(mktemp -d)" || fail "mktemp failed"
trap 'rm -rf "$TMP"' EXIT

drop_keeper() {
  # Same standalone load as the pack: `import fno.graph.store` runs the
  # package __init__, which needs deps a bare python3 does not carry, and a
  # silent import failure would leak every spawned fixture keeper.
  REPO_ROOT="$REPO_ROOT" PYTHONPATH="$REPO_ROOT/cli/src${PYTHONPATH:+:$PYTHONPATH}" python3 -c '
import importlib.util
import os
import sys
import types
from pathlib import Path

cli_src = Path(os.environ["REPO_ROOT"], "cli", "src").resolve()
fno_pkg = types.ModuleType("fno")
fno_pkg.__path__ = [str(cli_src / "fno")]
graph_pkg = types.ModuleType("fno.graph")
graph_pkg.__path__ = [str(cli_src / "fno" / "graph")]
sys.modules.setdefault("fno", fno_pkg)
sys.modules.setdefault("fno.graph", graph_pkg)
spec = importlib.util.spec_from_file_location(
    "fno.graph.store", cli_src / "fno" / "graph" / "store.py"
)
store = importlib.util.module_from_spec(spec)
sys.modules["fno.graph.store"] = store
spec.loader.exec_module(store)
store.shutdown_keeper(Path(sys.argv[1]))
' "$1" 2>/dev/null || true
}

mkdir -p "$TMP/claude" "$TMP/fno"
# One in-window S1 event so the packet gets past its empty-log guard. The log
# lives under FNO_HOME, not CLAUDE_DIR (footnote state never sits under .claude/).
printf '%s | S1 | test | test.md | fixture event\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$TMP/fno/corrections.log"

# Two blocked nodes, one via `status` and one via blocked_count, plus a node
# that must NOT appear.
cat > "$TMP/graph.json" <<'JSON'
{"entries":[
  {"id":"tst-block01","title":"blocked by status","status":"blocked","blocked_count":0,
   "last_blocked_reason":"waiting on upstream"},
  {"id":"tst-block02","title":"blocked by count","status":"ready","blocked_count":3,
   "last_blocked_reason":null},
  {"id":"tst-clear01","title":"not blocked at all","status":"ready","blocked_count":0}
]}
JSON

OUT="$TMP/packet.yaml"
CLAUDE_DIR_OVERRIDE="$TMP/claude" FNO_HOME="$TMP/fno" FNO_GRAPH_PATH="$TMP/graph.json" \
  bash "$PACK" --dry-run > "$OUT" 2>"$TMP/err.log" \
  || fail "autocorrect-pack exited non-zero (stderr: $(cat "$TMP/err.log"))"
drop_keeper "$TMP/graph.json"

grep -q 'tst-block01' "$OUT" \
  || fail "status==blocked node missing from packet (this is the .nodes/.entries bug)"
pass "a status==blocked node reaches the packet"

grep -q 'tst-block02' "$OUT" \
  || fail "blocked_count>0 node missing from packet"
pass "a blocked_count>0 node reaches the packet"

grep -q 'tst-clear01' "$OUT" \
  && fail "an unblocked node leaked into the packet"
pass "an unblocked node is excluded"

grep -q 'waiting on upstream' "$OUT" \
  || fail "last_blocked_reason did not survive into the packet"
pass "last_blocked_reason survives into the packet"

# A store that cannot be read must say NOT READ (failed), never [].
printf '{broken' > "$TMP/unreadable.json"
CLAUDE_DIR_OVERRIDE="$TMP/claude" FNO_HOME="$TMP/fno" FNO_GRAPH_PATH="$TMP/unreadable.json" \
  bash "$PACK" --dry-run > "$TMP/bad-packet.yaml" 2>"$TMP/bad-err.log" \
  || fail "unreadable-store run exited non-zero (stderr: $(cat "$TMP/bad-err.log"))"
drop_keeper "$TMP/unreadable.json"

grep -q 'NOT READ (failed)' "$TMP/bad-packet.yaml" \
  || fail "an unreadable store must read as NOT READ (failed)"
grep -q 'tst-block01' "$TMP/bad-packet.yaml" \
  && fail "the unreadable-store packet leaked nodes from another graph"
pass "an unreadable store reads as NOT READ (failed), never an empty answer"

printf '[autocorrect-blocked] All blocked-section scenarios passed\n'
