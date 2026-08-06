#!/usr/bin/env bash
# test-context-nudge.sh - Stop hook: context nudge for EVERY session + crown-only
# orphan check.
#
# Drives the REAL hook against the REAL worktree fno (PATH wrapper, like
# test-handoff's FNO_PYTHON discovery) with isolated state, a REAL fixture
# transcript the probe reads, and a real-shape Claude Stop payload. Covers:
#   AC5  crowned real fire path -> decision:block whose reason carries the pct
#   AC7  negative controls (below trigger, same-band latch, next band)
#   AC9  no kill -0 / owner_pid / target-state read in the hook
#   AC14 orphan block names both workers + the three resolutions
#   AC15 a carveout carrying the scope suppresses the orphan block (field match)
#   AC16 both checks fire in one output; orphan resolution does not suppress ctx
#   AC17 every-session nudge: uncrowned past the general trigger blocks + emits
#        session_context_nudge; below the general trigger does not
#   AC18 latch holds across CWD: the per-session latch lives in the global state
#        dir, so a cwd move between fires does not re-nudge within a band
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HOOK="$REPO_ROOT/hooks/context-nudge.sh"
SCOPE="x-test-epic"
KING_SID="king-test-session-id"

pass=0
fail=0
ok()   { echo "PASS: $1"; pass=$((pass+1)); }
bad()  { echo "FAIL: $1"; fail=$((fail+1)); }
assert_contains() { [[ "$2" == *"$3"* ]] && ok "$1" || bad "$1 (needle='$3' not in output)"; }
assert_absent()   { [[ "$2" != *"$3"* ]] && ok "$1" || bad "$1 (unexpected '$3')"; }
assert_eq()       { [[ "$2" == "$3" ]] && ok "$1" || bad "$1 (expected='$3' actual='$2')"; }

# --- FNO_PYTHON discovery (mirror test-handoff.sh): an interpreter that can ---
# --- import the worktree CLI, plus a PATH wrapper mapping fno -> fno-py.    ---
export FNO_SRC="$REPO_ROOT/cli/src"
FNO_PYTHON=""
for _cand in \
  "$REPO_ROOT/cli/.venv/bin/python" \
  "$(command -v python3 || true)" \
  "$(command -v python || true)"
do
  [ -n "$_cand" ] && [ -x "$_cand" ] || continue
  if PYTHONPATH="$FNO_SRC" "$_cand" -c 'import fno.cli' >/dev/null 2>&1; then
    FNO_PYTHON="$_cand"; break
  fi
done
if [ -z "$FNO_PYTHON" ]; then
  echo "SKIP: no python can import fno.cli from $FNO_SRC (need cli/.venv)"; exit 0
fi
BINDIR="$(mktemp -d)"
printf '#!/usr/bin/env bash\nexec "%s" -m fno.cli "$@"\n' "$FNO_PYTHON" > "$BINDIR/fno"
# fno-py is the console script name; provide it too in case anything resolves it.
cp "$BINDIR/fno" "$BINDIR/fno-py"
chmod +x "$BINDIR/fno" "$BINDIR/fno-py"
export PATH="$BINDIR:$PATH"

# --- sandbox: isolated state_dir + config + HOME so nothing leaks ----------
SBX="$(mktemp -d)"
trap 'rm -rf "$SBX" "$BINDIR"' EXIT
mkdir -p "$SBX/.fno/agents"
printf 'schema_version: 1\nconfig:\n  state_dir: %s/.fno/\n' "$SBX" > "$SBX/.fno/settings.yaml"
touch "$SBX/.fno/.path-migration-done"   # prevent [setup] state_dir re-migration
printf '[target.handoff]\nking_used_pct_trigger = 40\nused_pct_trigger = 50\n' > "$SBX/.fno/config.toml"
export FNO_CONFIG="$SBX/.fno/settings.yaml"
export HOME="$SBX"
export FNO_REPO_ROOT="$SBX"
cd "$SBX"   # isolate: hook latches (.fno/), git root (carveouts), and events all land under $SBX

# registry.json on disk at state_dir/agents/registry.json: {"schema_version":13,"agents":[...]}.
write_registry() {
  local king_crown="$1" has_children="$2"
  local children='[]'
  if [ "$has_children" = "yes" ]; then
    children='[{"name":"kfad-a","harness":"claude","cwd":"/tmp","log_path":"/tmp/a","status":"live","short_id":"a","spawned_by_session":"'"$KING_SID"'"},
               {"name":"kfad-b","harness":"claude","cwd":"/tmp","log_path":"/tmp/b","status":"live","short_id":"b","spawned_by_session":"'"$KING_SID"'"}]'
  fi
  local crown_level='null' crown_scope='null' crown_grantor='null'
  if [ "$king_crown" = "yes" ]; then
    crown_level='1'; crown_scope="\"$SCOPE\""; crown_grantor='"human"'
  fi
  jq -n --argjson children "$children" --argjson cl "$crown_level" --argjson cs "$crown_scope" --argjson cg "$crown_grantor" '{
    schema_version: 13,
    agents: ( [{
      name:"king-test", harness:"claude", cwd:"/tmp", log_path:"/tmp/k",
      status:"live", short_id:"'"$KING_SID"'",
      harness_session_id:"'"$KING_SID"'",
      crown_level:$cl, crown_scope:$cs, crown_grantor:$cg
    }] + $children )
  }' > "$SBX/.fno/agents/registry.json"
}

# A transcript with one assistant usage line: input_tokens sets the pct against
# the 1M window (claude-sonnet-4-6). 500000 -> 50%, 300000 -> 30%.
write_transcript() {  # write_transcript <path> <input_tokens>
  jq -nc --argjson t "$2" '{type:"assistant",message:{model:"claude-sonnet-4-6",usage:{input_tokens:$t,cache_creation_input_tokens:0,cache_read_input_tokens:0}}}' > "$1"
}

# Build a real-shape Stop payload pointing at a sandbox transcript + the session.
payload() {  # payload <transcript-path>
  jq -nc --arg t "$1" --arg s "$KING_SID" \
    '{session_id:$s, transcript_path:$t, cwd:"/repo", hook_event_name:"Stop", stop_hook_active:false}'
}

run_hook() {  # run_hook <payload> ; sets OUT, RC
  OUT=$(printf '%s' "$1" | bash "$HOOK" 2>/dev/null); RC=$?
}

events_has() { grep -q "\"type\":\"$1\"" "$SBX/.fno/events.jsonl" 2>/dev/null; }

# === AC9: the hook gates on nothing it isn't handed ============================
assert_absent "AC9: no kill -0"        "$(cat "$HOOK")" "kill -0"
assert_absent "AC9: no owner_pid"      "$(cat "$HOOK")" "owner_pid"
assert_absent "AC9: no target-state"   "$(cat "$HOOK")" "target-state"

# === AC5: crowned king past trigger -> block with the measured pct ============
write_registry yes no
write_transcript "$SBX/t.jsonl" 500000
run_hook "$(payload "$SBX/t.jsonl")"
assert_eq     "AC5: exits 0 (block decision in JSON, not exit 2)" "$RC" "0"
assert_contains "AC5: decision block" "$OUT" '"decision":"block"'
assert_contains "AC5: reason carries measured 50%" "$OUT" '50% used'
assert_contains "AC5: reason names the king trigger" "$OUT" 'king handoff trigger (40%)'
events_has king_context_nudge && ok "AC5: king_context_nudge event emitted" || bad "AC5: no king_context_nudge event"

# === AC7: negative controls ===================================================
# same band fires once -> second fire is latched, no block
run_hook "$(payload "$SBX/t.jsonl")"
assert_absent "AC7: second fire same band is latched" "$OUT" '"decision":"block"'

# crowned but BELOW trigger -> exit 0, no block, no output
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/low.jsonl" 300000   # 30% < 40
run_hook "$(payload "$SBX/low.jsonl")"
assert_eq     "AC7: below trigger exits 0" "$RC" "0"
assert_absent "AC7: below trigger no block" "$OUT" '"decision":"block"'

# next band up (60%) blocks again after the 50% latch
write_transcript "$SBX/t.jsonl" 600000
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC7: next band blocks again" "$OUT" '60% used'

# === AC17: every-session context nudge (uncrowned) ============================
# The probe ran for every session at the old crown gate and was discarded; now
# an uncrowned session past the GENERAL trigger (50) blocks with its own message
# + session_context_nudge event. Clear latches + registry first.
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_registry no no
write_transcript "$SBX/t.jsonl" 500000   # 50% >= general trigger 50
run_hook "$(payload "$SBX/t.jsonl")"
assert_eq     "AC17: uncrowned past trigger exits 0" "$RC" "0"
assert_contains "AC17: uncrowned decision block" "$OUT" '"decision":"block"'
assert_contains "AC17: reason carries measured 50%" "$OUT" '50% used'
assert_contains "AC17: reason names the general trigger" "$OUT" 'session handoff trigger (50%)'
events_has session_context_nudge && ok "AC17: session_context_nudge event emitted" || bad "AC17: no session_context_nudge event"

# uncrowned BELOW the general trigger -> exit 0, no block
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/low.jsonl" 300000   # 30% < 50
run_hook "$(payload "$SBX/low.jsonl")"
assert_eq     "AC17: uncrowned below trigger exits 0" "$RC" "0"
assert_absent "AC17: uncrowned below trigger no block" "$OUT" '"decision":"block"'

# === AC14: orphan block names both workers + the three resolutions ============
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_registry yes yes                       # crowned king + 2 live children
write_transcript "$SBX/low.jsonl" 300000     # below trigger -> isolate orphan check
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC14: orphan decision block" "$OUT" '"decision":"block"'
assert_contains "AC14: names worker kfad-a" "$OUT" 'kfad-a'
assert_contains "AC14: names worker kfad-b" "$OUT" 'kfad-b'
assert_contains "AC14: names resolution 1 (court)" "$OUT" 'stay as court'
assert_contains "AC14: names resolution 2 (--succeed)" "$OUT" '--succeed'
assert_contains "AC14: names resolution 3 (carveout)" "$OUT" 'carveout add'
events_has king_orphan_block && ok "AC14: king_orphan_block event emitted" || bad "AC14: no king_orphan_block event"

# === AC15: a carveout carrying the scope suppresses the orphan block ==========
rm -f "$SBX/.fno"/.context-nudge-ctx-* 2>/dev/null      # keep ctx latch; clear orphan latch
rm -f "$SBX/.fno"/.context-nudge-orphan-* 2>/dev/null
fno carveout add -k deferred --scope "$SCOPE" "workers review-orphaned; advisory self-review" >/dev/null 2>&1
run_hook "$(payload "$SBX/low.jsonl")"
assert_absent "AC15: carveout suppresses orphan block" "$OUT" 'cannot be a pure pass'

# a carveout for a DIFFERENT scope does NOT suppress
rm -f "$SBX/.fno"/.context-nudge-orphan-* 2>/dev/null
printf '' > "$SBX/.fno/carveouts.jsonl" 2>/dev/null || true   # drop the matching-scope carveout from above
fno carveout add -k deferred --scope "other-scope" "unrelated" >/dev/null 2>&1
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC15: wrong-scope carveout does not suppress" "$OUT" 'cannot be a pure pass'

# === AC16: both checks fire in one output; orphan resolved does not kill ctx ===
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
# remove the matching carveout so orphans are unresolved again; keep king + children
fno carveout list --json >/dev/null 2>&1   # (carveouts persist; clear by truncating)
printf '' > "$SBX/.fno/carveouts.jsonl" 2>/dev/null || true
write_transcript "$SBX/t.jsonl" 500000     # past trigger AND has orphans
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC16: ctx reason present" "$OUT" 'king handoff trigger'
assert_contains "AC16: orphan reason present" "$OUT" 'cannot be a pure pass'

# === AC18: latch holds across CWD (global state dir, not CWD-relative) ========
# The latch is per-session state (transcript + band) and lives in the global
# state dir, so a session whose cwd moves between fires (canonical <-> worktree,
# or a cwd with no .fno) must not re-nudge within the same band. Pre-fix the
# CWD-relative latch made the second fire block (or never latch at all from a
# cwd with no .fno).
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_registry yes no
write_transcript "$SBX/t.jsonl" 500000   # 50%, band 5
cd "$SBX"                                 # cwd with a .fno
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC18: first fire (cwd with .fno) blocks" "$OUT" '"decision":"block"'
mkdir -p "$SBX/no-fno-cwd"; cd "$SBX/no-fno-cwd"   # cwd with NO .fno of its own
run_hook "$(payload "$SBX/t.jsonl")"
assert_absent "AC18: second fire (different cwd) is latched, no re-block" "$OUT" '"decision":"block"'
cd "$SBX"                                 # restore for any trailing steps

echo ""
echo "================================"
echo "Results: $pass passed, $fail failed"
echo "================================"
[ "$fail" -eq 0 ]
