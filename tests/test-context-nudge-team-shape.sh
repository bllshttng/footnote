#!/usr/bin/env bash
# test-context-nudge-team-shape.sh - Stop hook: the orphan nudge can detect the
# team option once the manifest carries a shape.
#
#   AC8  promoted + live spawned workers + shape: team -> no orphan nudge, no
#        lead_orphan_block event
#   AC9  shape: pass -> the nudge fires and option 1 names the verb
#   AC10 no manifest (or one without a shape) -> the nudge fires
#   AC11 the manifest sits only in the role ROW's cwd space -> no nudge
#
# Drives the REAL hook against the REAL worktree fno (same FNO_PYTHON discovery
# and sandbox shape as test-context-nudge.sh). No python that can import fno.cli
# is a HARD FAIL here, not a skip, for the same reason as its sibling suite.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$REPO_ROOT/hooks/context-nudge.sh"
SCOPE="x-test-epic"
LEAD_SID="lead-team-shape-sid"

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
# Spell the binary path contiguously: the smoke runner greps this file for
# `target/debug/fno-agents` to decide whether selecting this harness must
# carry the cargo build step, and a split spelling selects the harness
# without its build (the red this comment prevents).
AGENTS_BIN="$REPO_ROOT/crates/fno-agents/target/debug/fno-agents"
if [ ! -x "$AGENTS_BIN" ]; then
  echo "FAIL: $AGENTS_BIN not built." >&2
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
export FNO_AGENTS_HOME="$SBX/.fno/agents"
export FNO_REPO_ROOT="$SBX"
unset CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID CLAUDE_SESSION_ID
export CLAUDE_CODE_SESSION_ID="$LEAD_SID"
cd "$SBX"

# A promoted lead with two live spawned workers: the exact shape the orphan
# check exists for. liveness_measured_at is generated HERE, at fixture-write
# time: SERVED_LIVENESS_MAX_AGE_SECS is 120, so a hardcoded stamp would age
# past the window and the suite would turn red on a clock, not on a defect.
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq -n --arg ts "$FRESH_TS" '{schema_version: 13, agents: ([
  {name:"lead-team", harness:"claude", cwd:"'"$SBX"'", log_path:"/tmp/k", status:"live",
   short_id:"'"$LEAD_SID"'", harness_session_id:"'"$LEAD_SID"'",
   role_level:1, role_scope:"'"$SCOPE"'", role_grantor:"human"},
  {name:"team-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live",
   short_id:"a", spawned_by_session:"'"$LEAD_SID"'", liveness:"alive", liveness_measured_at:$ts},
  {name:"team-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live",
   short_id:"b", spawned_by_session:"'"$LEAD_SID"'", liveness:"alive", liveness_measured_at:$ts}
])}' > "$SBX/.fno/agents/registry.json"

# Above the lead trigger so the general context nudge fires: its presence is
# the positive control that the hook RAN in the silent case, separating "team
# resolved it" from "nothing happened".
jq -nc '{type:"assistant",message:{model:"claude-sonnet-4-6",usage:{input_tokens:600000,cache_creation_input_tokens:0,cache_read_input_tokens:0}}}' > "$SBX/t.jsonl"
payload() {
  jq -nc --arg t "$SBX/t.jsonl" --arg s "$LEAD_SID" \
    '{session_id:$s, transcript_path:$t, cwd:"/repo", hook_event_name:"Stop", stop_hook_active:false}'
}
run_hook() {
  rm -f "$SBX/.fno/latches"/.context-nudge-* 2>/dev/null
  OUT=$(printf '%s' "$1" | bash "$HOOK" 2>/dev/null); RC=$?
}
ROWS_BIN="${FNO_BIN:-}"
if [[ -z "$ROWS_BIN" ]]; then
  for _profile in debug release; do
    if [[ -x "$REPO_ROOT/crates/fno/target/$_profile/fno" ]]; then
      ROWS_BIN="$REPO_ROOT/crates/fno/target/$_profile/fno"
      break
    fi
  done
fi
[[ -n "$ROWS_BIN" ]] || ROWS_BIN=$(command -v fno 2>/dev/null)
events_has() { "$ROWS_BIN" doctor event rows --events "$SBX/.fno/events.jsonl" 2>/dev/null | jq -r '.[]' 2>/dev/null | grep -q "\"type\":\"$1\""; }
reset_events() {
  local store root
  store=$("$ROWS_BIN" doctor event rows --events "$SBX/.fno/events.jsonl" --store-path-only | jq -er '.store') || return 1
  root=$(cd "$SBX" && pwd -P)
  [[ "$store" == "$root/"* ]] || { bad "event reset escaped its sandbox"; return 1; }
  rm -f "$SBX/.fno/events.jsonl" "$store" "$store-wal" "$store-shm"
}

# The hook reads the manifest from the role row's cwd space: the resolver
# keys the row, so the fixture computes the same root the resolver makes,
# with the same shim + env rather than re-deriving the slug here.
space_leads() {  # space_leads <dir> - the leads dir of <dir>'s space
  (cd "$SBX" && SBX="$1" PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -c 'import os; from pathlib import Path; from fno.lead.state import lead_state_root; print(lead_state_root(Path(os.environ["SBX"])) / "leads")')
}
LEADS_DIR="$(space_leads "$SBX")"
mkdir -p "$LEADS_DIR"
write_shape() {  # write_shape <shape|none|garbage>
  local _path="$LEADS_DIR/$SCOPE.md"
  rm -f "$_path"
  case "$1" in
    none) ;;
    garbage) printf 'not frontmatter at all\n' > "$_path" ;;
    *) printf -- '---\nscope: %s\nshape: %s\nharness_session_id: %s\n---\n' "$SCOPE" "$1" "$LEAD_SID" > "$_path" ;;
  esac
}

# === AC9: shape pass -> the nudge fires, option 1 names the verb ==============
reset_events
write_shape pass
run_hook "$(payload)"
assert_contains "AC9: orphan nudge fires on shape: pass" "$OUT" "2 worker(s) you spawned are still alive"
assert_contains "AC9: option 1 names the shape verb" "$OUT" "fno agents org shape team"
events_has lead_orphan_block && ok "AC9: lead_orphan_block event written" || bad "AC9: no lead_orphan_block event"

# === AC10: no manifest -> the nudge fires (read failure never clears) =========
reset_events
write_shape none
run_hook "$(payload)"
assert_contains "AC10: no manifest -> nudge fires" "$OUT" "still alive"
events_has lead_orphan_block && ok "AC10: lead_orphan_block event written" || bad "AC10: no lead_orphan_block event"

# === AC10b: a manifest with no shape line is not a team ======================
reset_events
printf -- '---\nscope: %s\nharness_session_id: %s\n---\n' "$SCOPE" "$LEAD_SID" > "$LEADS_DIR/$SCOPE.md"
run_hook "$(payload)"
assert_contains "AC10b: shapeless manifest -> nudge fires" "$OUT" "still alive"

# === AC8: shape team -> silent, and the hook demonstrably ran ================
reset_events
write_shape team
run_hook "$(payload)"
assert_absent "AC8: no orphan reason on shape: team" "$OUT" "you spawned are still alive"
events_has lead_orphan_block && bad "AC8: lead_orphan_block event written anyway" || ok "AC8: no lead_orphan_block event"
assert_contains "AC8 positive control: the hook ran (context nudge fired)" "$OUT" '"decision":"block"'

# AC8 negative control for the control: with no ALIVE workers at all (both rows
# read a confidently-dead served liveness, fresh basis - not merely unresolved,
# which would fire the unknown-count branch instead) the same promoted session
# emits no orphan block even at shape pass - proving the team branch is what
# silenced it above, not some earlier gate.
reset_events
write_shape pass
DEAD_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq --arg ts "$DEAD_TS" \
  '.agents |= map(if .name == "team-a" or .name == "team-b" then .liveness = "dead" | .liveness_measured_at = $ts else . end)' \
  "$SBX/.fno/agents/registry.json" > "$SBX/.fno/agents/registry.json.tmp" && mv "$SBX/.fno/agents/registry.json.tmp" "$SBX/.fno/agents/registry.json"
run_hook "$(payload)"
assert_absent "AC8 control: dead workers never orphan-block" "$OUT" "you spawned are still alive"

# === AC11: the manifest only in the role ROW's cwd space -> no nudge ========
# The lead's row cwd is a repo dir its shell no longer stands in: the resolver
# must key the row, not the hook's own cwd. The repo space holds the team
# manifest; the shell's space holds none.
reset_events
KINGREPO="$SBX/kingrepo"
mkdir -p "$KINGREPO"
KINGREPO_LEADS="$(space_leads "$KINGREPO")"
mkdir -p "$KINGREPO_LEADS"
FRESH_TS2="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
jq --arg ts "$FRESH_TS2" --arg cwd "$KINGREPO" \
  '.agents |= map(if .name == "lead-team" then .cwd = $cwd | .liveness_measured_at = $ts
                  elif .name == "team-a" or .name == "team-b" then .liveness = "alive" | .liveness_measured_at = $ts
                  else . end)' \
  "$SBX/.fno/agents/registry.json" > "$SBX/.fno/agents/registry.json.tmp" && mv "$SBX/.fno/agents/registry.json.tmp" "$SBX/.fno/agents/registry.json"
rm -f "$LEADS_DIR/$SCOPE.md"
printf -- '---\nscope: %s\nshape: team\nharness_session_id: %s\n---\n' "$SCOPE" "$LEAD_SID" > "$KINGREPO_LEADS/$SCOPE.md"
run_hook "$(payload)"
assert_absent "AC11: no orphan reason with the manifest only in the row-cwd space" "$OUT" "you spawned are still alive"
events_has lead_orphan_block && bad "AC11: lead_orphan_block event written anyway" || ok "AC11: no lead_orphan_block event"
assert_contains "AC11 positive control: the hook ran (context nudge fired)" "$OUT" '"decision":"block"'

echo
if [ "$fail" -eq 0 ]; then
  echo "team-shape: ALL PASS ($pass)"
  exit 0
fi
echo "team-shape: $fail FAIL ($pass passed)"
exit 1
