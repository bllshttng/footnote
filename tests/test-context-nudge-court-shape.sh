#!/usr/bin/env bash
# test-context-nudge-court-shape.sh - Stop hook: the orphan nudge can detect the
# court option once the manifest carries a shape.
#
#   AC8  crowned + live spawned workers + shape: court -> no orphan nudge, no
#        king_orphan_block event
#   AC9  shape: pass -> the nudge fires and option 1 names the verb
#   AC10 no manifest (or one without a shape) -> the nudge fires
#
# Drives the REAL hook against the REAL worktree fno (same FNO_PYTHON discovery
# and sandbox shape as test-context-nudge.sh). No python that can import fno.cli
# is a HARD FAIL here, not a skip, for the same reason as its sibling suite.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$REPO_ROOT/hooks/context-nudge.sh"
SCOPE="x-test-epic"
KING_SID="king-court-shape-sid"

pass=0
fail=0
ok()   { echo "PASS: $1"; pass=$((pass+1)); }
bad()  { echo "FAIL: $1"; fail=$((fail+1)); }
assert_contains() { [[ "$2" == *"$3"* ]] && ok "$1" || bad "$1 (needle='$3' not in output)"; }
assert_absent()   { [[ "$2" != *"$3"* ]] && ok "$1" || bad "$1 (unexpected '$3')"; }

export FNO_SRC="$REPO_ROOT/cli/src"
FNO_PYTHON=""
for _cand in \
  "$REPO_ROOT/cli/.venv/bin/python" \
  "$(dirname "$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)")/cli/.venv/bin/python" \
  "$(command -v python3 || true)" \
  "$(command -v python || true)"
do
  [ -n "$_cand" ] && [ -x "$_cand" ] || continue
  if PYTHONPATH="$FNO_SRC" "$_cand" -c 'import fno.cli' >/dev/null 2>&1; then
    FNO_PYTHON="$_cand"; break
  fi
done
if [ -z "$FNO_PYTHON" ]; then
  echo "FAIL: no python can import fno.cli from $FNO_SRC" >&2
  echo "      Fix: (cd cli && uv sync)" >&2
  exit 1
fi
BINDIR="$(mktemp -d)"
# PYTHONPATH pinned INSIDE the shim (see test-context-nudge.sh sibling
# comment): without it `python -m fno.cli` can silently resolve a DIFFERENT
# tree's fno.cli than the worktree being tested.
printf '#!/usr/bin/env bash\nexport PYTHONPATH="%s"\nexec "%s" -m fno.cli "$@"\n' "$FNO_SRC" "$FNO_PYTHON" > "$BINDIR/fno"
cp "$BINDIR/fno" "$BINDIR/fno-py"
chmod +x "$BINDIR/fno" "$BINDIR/fno-py"
export PATH="$BINDIR:$PATH"

# x-1b75: registry-json has no Python leg left; resolve THIS checkout's Rust
# binary, not a stale one elsewhere on PATH (see test-context-nudge.sh sibling
# comment for the full reasoning).
AGENTS_BIN_DIR="$REPO_ROOT/crates/fno-agents/target/debug"
if [ ! -x "$AGENTS_BIN_DIR/fno-agents" ]; then
  echo "FAIL: $AGENTS_BIN_DIR/fno-agents not built." >&2
  echo "      Fix: (cd crates/fno-agents && cargo build --bin fno-agents)" >&2
  exit 1
fi
export PATH="$AGENTS_BIN_DIR:$PATH"

SBX="$(mktemp -d)"
trap 'rm -rf "$SBX" "$BINDIR"' EXIT
mkdir -p "$SBX/.fno/agents" "$SBX/.fno/latches"
printf 'schema_version: 1\nconfig:\n  state_dir: %s/.fno/\n' "$SBX" > "$SBX/.fno/settings.yaml"
touch "$SBX/.fno/.path-migration-done"
printf '[target.handoff]\nking_used_pct_trigger = 40\nused_pct_trigger = 50\n' > "$SBX/.fno/config.toml"
export FNO_CONFIG="$SBX/.fno/settings.yaml"
export HOME="$SBX"
export FNO_REPO_ROOT="$SBX"
unset CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID CLAUDE_SESSION_ID
export CLAUDE_CODE_SESSION_ID="$KING_SID"
cd "$SBX"

# A crowned king with two live spawned workers: the exact shape the orphan
# check exists for. liveness_measured_at is generated HERE, at fixture-write
# time: SERVED_LIVENESS_MAX_AGE_SECS is 120, so a hardcoded stamp would age
# past the window and the suite would turn red on a clock, not on a defect.
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n --arg ts "$FRESH_TS" '{schema_version: 13, agents: ([
  {name:"king-court", harness:"claude", cwd:"/tmp", log_path:"/tmp/k", status:"live",
   short_id:"'"$KING_SID"'", harness_session_id:"'"$KING_SID"'",
   crown_level:1, crown_scope:"'"$SCOPE"'", crown_grantor:"human"},
  {name:"court-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live",
   short_id:"a", spawned_by_session:"'"$KING_SID"'", liveness:"alive", liveness_measured_at:$ts},
  {name:"court-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live",
   short_id:"b", spawned_by_session:"'"$KING_SID"'", liveness:"alive", liveness_measured_at:$ts}
])}' > "$SBX/.fno/agents/registry.json"

# Above the king trigger so the general context nudge fires: its presence is
# the positive control that the hook RAN in the silent case, separating "court
# resolved it" from "nothing happened".
jq -nc '{type:"assistant",message:{model:"claude-sonnet-4-6",usage:{input_tokens:500000,cache_creation_input_tokens:0,cache_read_input_tokens:0}}}' > "$SBX/t.jsonl"
payload() {
  jq -nc --arg t "$SBX/t.jsonl" --arg s "$KING_SID" \
    '{session_id:$s, transcript_path:$t, cwd:"/repo", hook_event_name:"Stop", stop_hook_active:false}'
}
run_hook() {
  rm -f "$SBX/.fno/latches"/.context-nudge-* 2>/dev/null
  OUT=$(printf '%s' "$1" | bash "$HOOK" 2>/dev/null); RC=$?
}
events_has() { grep -q "\"type\":\"$1\"" "$SBX/.fno/events.jsonl" 2>/dev/null; }
reset_events() { rm -f "$SBX/.fno/events.jsonl"; }

# The hook reads the manifest from the resolver's DEFAULT root (the space dir
# keyed on this cwd), no longer a repo-local .fno: compute it with the same
# shim + env the hook sees rather than re-deriving the slug here.
KINGS_DIR="$(cd "$SBX" && PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -c 'from fno.paths import space_dir; print(space_dir() / "kings")')"
mkdir -p "$KINGS_DIR"
write_shape() {  # write_shape <shape|none|garbage>
  local _path="$KINGS_DIR/$SCOPE.md"
  rm -f "$_path"
  case "$1" in
    none) ;;
    garbage) printf 'not frontmatter at all\n' > "$_path" ;;
    *) printf -- '---\nscope: %s\nshape: %s\nharness_session_id: %s\n---\n' "$SCOPE" "$1" "$KING_SID" > "$_path" ;;
  esac
}

# === AC9: shape pass -> the nudge fires, option 1 names the verb ==============
reset_events
write_shape pass
run_hook "$(payload)"
assert_contains "AC9: orphan nudge fires on shape: pass" "$OUT" "2 worker(s) you spawned are still alive"
assert_contains "AC9: option 1 names the shape verb" "$OUT" "fno agents king shape court"
events_has king_orphan_block && ok "AC9: king_orphan_block event written" || bad "AC9: no king_orphan_block event"

# === AC10: no manifest -> the nudge fires (read failure never clears) =========
reset_events
write_shape none
run_hook "$(payload)"
assert_contains "AC10: no manifest -> nudge fires" "$OUT" "still alive"
events_has king_orphan_block && ok "AC10: king_orphan_block event written" || bad "AC10: no king_orphan_block event"

# === AC10b: a manifest with no shape line is not a court ======================
reset_events
printf -- '---\nscope: %s\nharness_session_id: %s\n---\n' "$SCOPE" "$KING_SID" > "$SBX/.fno/kings/$SCOPE.md"
run_hook "$(payload)"
assert_contains "AC10b: shapeless manifest -> nudge fires" "$OUT" "still alive"

# === AC8: shape court -> silent, and the hook demonstrably ran ================
reset_events
write_shape court
run_hook "$(payload)"
assert_absent "AC8: no orphan reason on shape: court" "$OUT" "you spawned are still alive"
events_has king_orphan_block && bad "AC8: king_orphan_block event written anyway" || ok "AC8: no king_orphan_block event"
assert_contains "AC8 positive control: the hook ran (context nudge fired)" "$OUT" '"decision":"block"'

# AC8 negative control for the control: with no ALIVE workers at all (both rows
# read a confidently-dead served liveness, fresh basis - not merely unresolved,
# which would fire the unknown-count branch instead) the same crowned session
# emits no orphan block even at shape pass - proving the court branch is what
# silenced it above, not some earlier gate.
reset_events
write_shape pass
DEAD_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq --arg ts "$DEAD_TS" \
  '.agents |= map(if .name == "court-a" or .name == "court-b" then .liveness = "dead" | .liveness_measured_at = $ts else . end)' \
  "$SBX/.fno/agents/registry.json" > "$SBX/.fno/agents/registry.json.tmp" && mv "$SBX/.fno/agents/registry.json.tmp" "$SBX/.fno/agents/registry.json"
run_hook "$(payload)"
assert_absent "AC8 control: dead workers never orphan-block" "$OUT" "you spawned are still alive"

echo
if [ "$fail" -eq 0 ]; then
  echo "court-shape: ALL PASS ($pass)"
  exit 0
fi
echo "court-shape: $fail FAIL ($pass passed)"
exit 1
