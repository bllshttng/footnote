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
#   AC26 flush refusal: a foreign live writer rooted in the checkout (named by
#        pid + registry row) refuses the commit advice; the same dirty checkout
#        without one still gets it; an unreadable measurement refuses; the
#        refusal latches separately so it never eats the nudge
#   compact gate: the compact advice matches a MEASURED injection path, all THREE
#        answers (injectable / not-injectable / could-not-measure), plus source
#        sweeps for the dead crown verb and the raw transport name, each with a
#        positive control so a passing absent-assertion proves the file was read
#
# No python that can import fno.cli is a HARD FAIL here, not a skip. This file is
# the only thing pinning the hook's behaviour, so an exit-0 skip makes every claim
# resting on it decorative; a fresh worktree with no cli/.venv once "passed" this
# suite having run none of it.
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
  # NOT a skip, and deliberately not exit 0. This file is the only thing that pins
  # the hook's behaviour, so an exit-0 skip makes every claim resting on it
  # decorative: a fresh worktree with no cli/.venv "passed" this suite while
  # running none of it. Fail loudly and name the fix.
  echo "FAIL: no python can import fno.cli from $FNO_SRC" >&2
  echo "      This suite cannot verify the hook without it, and a silent pass here" >&2
  echo "      is worth less than a red run. Fix: (cd cli && uv sync)" >&2
  exit 1
fi
BINDIR="$(mktemp -d)"
# PYTHONPATH pinned INSIDE the shim, not just for the discovery probe above:
# without it, `python -m fno.cli` resolves the `fno` package however this
# interpreter already has it installed (a shared/editable venv can point at
# the canonical checkout), so the hook would silently exercise a DIFFERENT
# tree's fno.cli than the worktree fno being tested - invisible whenever the
# two trees agree, and a false pass the moment they diverge (x-1b75: it
# masked registry-json's Rust port entirely, taking four AC31 assertions
# down with it before this line existed).
printf '#!/usr/bin/env bash\nexport PYTHONPATH="%s"\nexec "%s" -m fno.cli "$@"\n' "$FNO_SRC" "$FNO_PYTHON" > "$BINDIR/fno"
# fno-py is the console script name; provide it too in case anything resolves it.
cp "$BINDIR/fno" "$BINDIR/fno-py"
chmod +x "$BINDIR/fno" "$BINDIR/fno-py"
export PATH="$BINDIR:$PATH"

# x-1b75: registry-json now dispatches through the Rust client (no Python leg
# left), so `fno agents registry-json` must resolve THIS checkout's binary, not
# a stale one elsewhere on PATH. The "auto" runtime's installed-binary search
# (rust_binary.resolve_installed_binary) deliberately excludes the cargo dev
# target and would otherwise pick up whatever `fno-agents` a developer machine
# already has on PATH - possibly a build that predates this verb's port.
# Prepending the fresh debug build wins the PATH search either way.
AGENTS_BIN_DIR="$REPO_ROOT/crates/fno-agents/target/debug"
if [ ! -x "$AGENTS_BIN_DIR/fno-agents" ]; then
  echo "FAIL: $AGENTS_BIN_DIR/fno-agents not built." >&2
  echo "      registry-json has no Python leg left (x-1b75); this suite needs the real binary." >&2
  echo "      Fix: (cd crates/fno-agents && cargo build --bin fno-agents)" >&2
  exit 1
fi
export PATH="$AGENTS_BIN_DIR:$PATH"

# --- sandbox: isolated state_dir + config + HOME so nothing leaks ----------
SBX="$(mktemp -d)"
trap 'rm -rf "$SBX" "$BINDIR"' EXIT
mkdir -p "$SBX/.fno/agents"
printf 'schema_version: 1\nconfig:\n  state_dir: %s/.fno/\n' "$SBX" > "$SBX/.fno/settings.yaml"
touch "$SBX/.fno/.path-migration-done"   # prevent [setup] state_dir re-migration
LATCHES="$SBX/.fno/latches"               # latches live in a subdir, not the root
mkdir -p "$LATCHES"
printf '[target.handoff]\nking_used_pct_trigger = 40\nused_pct_trigger = 50\n' > "$SBX/.fno/config.toml"
export FNO_CONFIG="$SBX/.fno/settings.yaml"
export HOME="$SBX"
export FNO_REPO_ROOT="$SBX"
# Ambient harness identity is a FIXTURE, not something inherited. The hook asks
# `fno agents mail send --to-self`, which derives its recipient from these markers, so a
# developer machine leaked the REAL session id in and CI (which has none) took a
# different branch than the local run: the local pass was environment-dependent.
# Pin the primary hook identity to the fixture Claude session. Record the real
# parent harness for the compact probe below, where --to-self refuses a
# contradicted marker as inherited identity.
_SESSION_HARNESS="$(PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -c \
  'from fno.claims.session_pid import resolve_session_harness; print(resolve_session_harness() or "")')"
unset CODEX_THREAD_ID CLAUDE_CODE_SESSION_ID CODEX_SESSION_ID GEMINI_SESSION_ID OPENCODE_SESSION_ID CLAUDE_SESSION_ID
export CLAUDE_CODE_SESSION_ID="$KING_SID"
cd "$SBX"   # isolate: hook latches (.fno/), git root (carveouts), and events all land under $SBX

# Clear the carveouts ledger through the accessor that owns it (the repo's
# space), not the retired checkout path - a truncate of .fno/carveouts.jsonl
# is inert now that write and read both resolve project_log().
clear_carveouts() {
  PYTHONPATH="$FNO_SRC" "$FNO_PYTHON" -c \
    'from fno.carveout.core import CARVEOUTS_NAME; from fno.paths import project_log; p = project_log(CARVEOUTS_NAME); p.parent.mkdir(parents=True, exist_ok=True); p.write_text("")'
}

# registry.json on disk at state_dir/agents/registry.json: {"schema_version":13,"agents":[...]}.
# liveness_measured_at is generated HERE, at call time: SERVED_LIVENESS_MAX_AGE_SECS
# is 120, so a hardcoded stamp would age past the window and the suite would
# turn red on a clock, not on a defect.
write_registry() {
  local king_crown="$1" has_children="$2" has_peer="${3:-no}"
  local ts children='[]' peers='[]'
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ "$has_children" = "yes" ]; then
    children='[{"name":"kfad-a","harness":"claude","cwd":"/tmp","log_path":"/tmp/a","status":"live","short_id":"a","spawned_by_session":"'"$KING_SID"'","liveness":"alive","liveness_measured_at":"'"$ts"'"},
               {"name":"kfad-b","harness":"claude","cwd":"/tmp","log_path":"/tmp/b","status":"live","short_id":"b","spawned_by_session":"'"$KING_SID"'","liveness":"alive","liveness_measured_at":"'"$ts"'"}]'
  fi
  # A peer king: a DIFFERENT crowned session with a disjoint scope, for the
  # king roll-up (peers / king-above) test.
  if [ "$has_peer" = "yes" ]; then
    peers='[{"name":"peer-king","harness":"claude","cwd":"/tmp","log_path":"/tmp/p","status":"live","short_id":"p","harness_session_id":"peer-sid","crown_level":1,"crown_scope":"peer-scope","crown_grantor":"human"}]'
  fi
  local crown_level='null' crown_scope='null' crown_grantor='null'
  if [ "$king_crown" = "yes" ]; then
    crown_level='1'; crown_scope="\"$SCOPE\""; crown_grantor='"human"'
  fi
  jq -n --argjson children "$children" --argjson peers "$peers" \
    --argjson cl "$crown_level" --argjson cs "$crown_scope" --argjson cg "$crown_grantor" '{
    schema_version: 13,
    agents: ( [{
      name:"king-test", harness:"claude", cwd:"/tmp", log_path:"/tmp/k",
      status:"live", short_id:"'"$KING_SID"'",
      harness_session_id:"'"$KING_SID"'",
      crown_level:$cl, crown_scope:$cs, crown_grantor:$cg
    }] + $children + $peers )
  }' > "$SBX/.fno/agents/registry.json"
}

# A registry with no row for THIS session: the hand-started REPL shape. The
# context check does not consult the registry, so the nudge still fires; `--check`
# answers not-injectable at resolve_agent.
write_registry_without_self() {
  jq -n '{schema_version: 13, agents: [{
    name:"someone-else", harness:"claude", cwd:"/tmp", log_path:"/tmp/x",
    status:"live", short_id:"other", harness_session_id:"other-sid",
    crown_level:null, crown_scope:null, crown_grantor:null
  }]}' > "$SBX/.fno/agents/registry.json"
}

write_registry_with_unlinked_child() {
  local ts
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  jq -n --arg ts "$ts" '{schema_version: 13, agents: [
    {
      name:"king-test", harness:"claude", cwd:"/tmp", log_path:"/tmp/k",
      status:"live", short_id:"king-test-session-id",
      harness_session_id:"king-test-session-id",
      crown_level:1, crown_scope:"x-test-epic", crown_grantor:"human"
    },
    {
      name:"unlinked-worker", harness:"claude", cwd:"/tmp", log_path:"/tmp/u",
      status:"live", short_id:"unlinked", spawned_by_session:null,
      crown_level:null, crown_scope:null,
      liveness:"alive", liveness_measured_at:$ts
    },
    {
      name:"operator-peer", harness:"claude", cwd:"/tmp", log_path:"/tmp/o",
      status:"live", short_id:"operator-peer", spawned_by_session:null,
      crown_level:null, crown_scope:null, origin:"operator",
      liveness:"alive", liveness_measured_at:$ts
    }
  ]}' > "$SBX/.fno/agents/registry.json"
}

# A transcript with one assistant usage line: input_tokens sets the pct against
# the 1M window (claude-sonnet-4-6). 500000 -> 50%, 300000 -> 30%.
write_transcript() {  # write_transcript <path> <input_tokens> [model]
  local _m="${3:-claude-sonnet-4-6}"
  jq -nc --argjson t "$2" --arg m "$_m" '{type:"assistant",message:{model:$m,usage:{input_tokens:$t,cache_creation_input_tokens:0,cache_read_input_tokens:0}}}' > "$1"
}

# Build a real-shape Stop payload pointing at a sandbox transcript + the session.
payload() {  # payload <transcript-path>
  jq -nc --arg t "$1" --arg s "$KING_SID" \
    '{session_id:$s, transcript_path:$t, cwd:"/repo", hook_event_name:"Stop", stop_hook_active:false}'
}

# A Stop that re-fires after a previous block (mid-compaction): stop_hook_active
# is the one continuation signal the payload carries, and the hook must exit on it.
payload_compact() {  # payload_compact <transcript-path>
  jq -nc --arg t "$1" --arg s "$KING_SID" \
    '{session_id:$s, transcript_path:$t, cwd:"/repo", hook_event_name:"Stop", stop_hook_active:true}'
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
assert_contains "AC5: reason names the crowned scope" "$OUT" "$SCOPE"
assert_contains "AC5: canon ask names the scope-keyed rolling doc" "$OUT" "crown-${SCOPE}.md"
events_has king_context_nudge && ok "AC5: king_context_nudge event emitted" || bad "AC5: no king_context_nudge event"

# A crown survives a compact, so this percentage asks a king to COMPACT and keep
# ruling. It is not a handoff threshold, and the branch must not read as one: the
# old wording pointed a king at the more expensive move on a number that measures
# nothing about ruling quality.
assert_contains "crown: compact is the default, not handoff" "$OUT" 'COMPACT AND KEEP RULING'
assert_contains "crown: crown survives a compact" "$OUT" 'maintained across a compact'
assert_absent   "crown: no handoff threshold claim" "$OUT" 'handoff trigger'
assert_contains "crown: handoff is a quality judgement" "$OUT" 'ORCHESTRATION is visibly degrading'
assert_contains "crown: names the successor-handle cost" "$OUT" 'NEW mail handle'
# The stored rung is stale for any king crowned before succession moved into
# spawn, and those rows were never migrated, so the nudge must not print one.
assert_absent   "crown: does not print a stale rung" "$OUT" 'level 1'

# === AC7: negative controls ===================================================
# same band fires once -> second fire is latched, no block
run_hook "$(payload "$SBX/t.jsonl")"
assert_absent "AC7: second fire same band is latched" "$OUT" '"decision":"block"'

# crowned but BELOW trigger -> exit 0, no block, no output
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
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
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry no no
write_transcript "$SBX/t.jsonl" 500000   # 50% >= general trigger 50
run_hook "$(payload "$SBX/t.jsonl")"
assert_eq     "AC17: uncrowned past trigger exits 0" "$RC" "0"
assert_contains "AC17: uncrowned decision block" "$OUT" '"decision":"block"'
assert_contains "AC17: reason carries measured 50%" "$OUT" '50% used'
assert_contains "AC17: reason names the general trigger" "$OUT" 'session compact trigger (50%)'
events_has session_context_nudge && ok "AC17: session_context_nudge event emitted" || bad "AC17: no session_context_nudge event"

# uncrowned BELOW the general trigger -> exit 0, no block
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/low.jsonl" 300000   # 30% < 50
run_hook "$(payload "$SBX/low.jsonl")"
assert_eq     "AC17: uncrowned below trigger exits 0" "$RC" "0"
assert_absent "AC17: uncrowned below trigger no block" "$OUT" '"decision":"block"'

# === AC14: orphan block names both workers + the three resolutions ============
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry yes yes                       # crowned king + 2 live children
write_transcript "$SBX/low.jsonl" 300000     # below trigger -> isolate orphan check
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC14: orphan decision block" "$OUT" '"decision":"block"'
assert_contains "AC14: names worker kfad-a" "$OUT" 'kfad-a'
assert_contains "AC14: names worker kfad-b" "$OUT" 'kfad-b'
assert_contains "AC14: names resolution 1 (court)" "$OUT" 'stay as court'
# Succession moved into spawn: a sitting king spawning its heir over its own scope
# transfers the crown in the write that vacates its own. The verb this line used to
# name, and the flag it used to pass, were both deleted; an assertion pinning them
# passed while documenting a command that no longer exists.
assert_contains "AC14: names resolution 2 (spawn the heir)" "$OUT" 'fno agents spawn -k'
assert_contains "AC14: names resolution 3 (carveout)" "$OUT" 'carveout add'
events_has king_orphan_block && ok "AC14: king_orphan_block event emitted" || bad "AC14: no king_orphan_block event"

# === AC31: unlinked-only workers make ownership unknown, never zero ==========
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_with_unlinked_child
write_transcript "$SBX/low.jsonl" 300000
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC31: unlinked-only population blocks" "$OUT" '"decision":"block"'
assert_contains "AC31: reports zero linked workers" "$OUT" 'Linked count: 0'
assert_contains "AC31: names unlinked worker" "$OUT" 'unlinked-worker'
assert_contains "AC31: reports ownership unknown" "$OUT" 'ownership unknown'
assert_absent "AC31: excludes operator session" "$OUT" 'operator-peer'
write_registry yes yes

# === AC15: a carveout carrying the scope suppresses the orphan block ==========
rm -f "$LATCHES"/.context-nudge-ctx-* 2>/dev/null      # keep ctx latch; clear orphan latch
rm -f "$LATCHES"/.context-nudge-orphan-* 2>/dev/null
fno backlog carveout add -k deferred --scope "$SCOPE" "workers review-orphaned; advisory self-review" >/dev/null 2>&1
run_hook "$(payload "$SBX/low.jsonl")"
assert_absent "AC15: carveout suppresses orphan block" "$OUT" 'cannot be a pure pass'

# a carveout for a DIFFERENT scope does NOT suppress
rm -f "$LATCHES"/.context-nudge-orphan-* 2>/dev/null
clear_carveouts   # drop the matching-scope carveout from above
fno backlog carveout add -k deferred --scope "other-scope" "unrelated" >/dev/null 2>&1
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC15: wrong-scope carveout does not suppress" "$OUT" 'cannot be a pure pass'

# === AC16: both checks fire in one output; orphan resolved does not kill ctx ===
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
# remove the matching carveout so orphans are unresolved again; keep king + children
clear_carveouts
write_transcript "$SBX/t.jsonl" 500000     # past trigger AND has orphans
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC16: ctx reason present" "$OUT" 'COMPACT AND KEEP RULING'
assert_contains "AC16: orphan reason present" "$OUT" 'cannot be a pure pass'

# === AC18: latch holds across CWD (global state dir, not CWD-relative) ========
# The latch is per-session state (transcript + band) and lives in the global
# state dir, so a session whose cwd moves between fires (canonical <-> worktree,
# or a cwd with no .fno) must not re-nudge within the same band. Pre-fix the
# CWD-relative latch made the second fire block (or never latch at all from a
# cwd with no .fno).
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry yes no
write_transcript "$SBX/t.jsonl" 500000   # 50%, band 5
cd "$SBX"                                 # cwd with a .fno
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC18: first fire (cwd with .fno) blocks" "$OUT" '"decision":"block"'
mkdir -p "$SBX/no-fno-cwd"; cd "$SBX/no-fno-cwd"   # cwd with NO .fno of its own
run_hook "$(payload "$SBX/t.jsonl")"
assert_absent "AC18: second fire (different cwd) is latched, no re-block" "$OUT" '"decision":"block"'
cd "$SBX"                                 # restore for any trailing steps

# === AC19: window gate - the quality branch needs a large window ===============
# A 200k window (unlisted model) at 55% used does NOT fire quality (MIN_WINDOW),
# and with 90k remaining it clears the RESERVE floor too, so the hook is silent.
# The same percentage on a 1M window would block (covered by AC5/AC17).
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry no no
write_transcript "$SBX/small.jsonl" 110000 "gpt-5-codex"   # 200k window, 55%, 90k left
run_hook "$(payload "$SBX/small.jsonl")"
assert_eq     "AC19: small window high pct exits 0" "$RC" "0"
assert_absent "AC19: small window no quality block" "$OUT" '"decision":"block"'

# === AC20: capacity branch fires on a small window near the floor ==============
# Same 200k window, but 60k remaining (<= RESERVE): capacity fires even though
# quality can never fire on this window. Latches once per band like the rest.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/small.jsonl" 140000 "gpt-5-codex"   # 200k window, 70%, 60k left
run_hook "$(payload "$SBX/small.jsonl")"
assert_contains "AC20: capacity branch blocks" "$OUT" '"decision":"block"'
assert_contains "AC20: reason carries measured 70%" "$OUT" '70% used'
run_hook "$(payload "$SBX/small.jsonl")"
assert_absent "AC20: capacity latch holds (second fire silent)" "$OUT" '"decision":"block"'

# === AC21: a compaction re-fire (stop_hook_active:true) is silent ==============
# A Stop that re-fires after a previous block is mid-compaction: re-blocking
# loops, nudging is noise. The hook exits before any check, even on a payload
# that would otherwise block.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/t.jsonl" 550000                      # 55% on 1M would block
run_hook "$(payload_compact "$SBX/t.jsonl")"
assert_eq     "AC21: compaction re-fire exits 0" "$RC" "0"
assert_absent "AC21: compaction re-fire no block" "$OUT" '"decision":"block"'

# === AC22: flush-cadence - dirty tree static across turns nudges to commit ====
# Isolated git repo (the shared sandbox is not a repo, so the other checks never
# see a dirty tree). >10 uncommitted files, HEAD unmoved across 3 turn-ends ->
# block whose reason quotes a FILE count, never a commit count; latches per band.
FLUSH_REPO="$SBX/flush-repo"
mkdir -p "$FLUSH_REPO"
git -C "$FLUSH_REPO" init -q
git -C "$FLUSH_REPO" config user.email t@t.t
git -C "$FLUSH_REPO" config user.name t
git -C "$FLUSH_REPO" commit -q --allow-empty -m base      # a HEAD to track
rm -f "$LATCHES"/.context-nudge-flush* 2>/dev/null
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo "x$i" > "$FLUSH_REPO/file$i.txt"; done
write_transcript "$SBX/low.jsonl" 300000                   # 30% -> isolate from ctx
cd "$FLUSH_REPO"
run_hook "$(payload "$SBX/low.jsonl")"                      # stop 1: static=1
assert_absent "AC22: stop 1 no flush block" "$OUT" '"decision":"block"'
run_hook "$(payload "$SBX/low.jsonl")"                      # stop 2: static=2
assert_absent "AC22: stop 2 no flush block" "$OUT" '"decision":"block"'
run_hook "$(payload "$SBX/low.jsonl")"                      # stop 3: static=3 -> fires
assert_contains "AC22: stop 3 flush blocks" "$OUT" '"decision":"block"'
assert_contains "AC22: message quotes a file count" "$OUT" '12 files'
assert_contains "AC22: message names uncommitted work" "$OUT" 'uncommitted'
assert_absent "AC22: never a commit-count phrasing" "$OUT" 'commits this session'
run_hook "$(payload "$SBX/low.jsonl")"                      # stop 4: latched
assert_absent "AC22: stop 4 latched (once per band)" "$OUT" '"decision":"block"'
cd "$SBX"

# === AC23: clean tree -> no flush output =====================================
rm -f "$LATCHES"/.context-nudge-flush* 2>/dev/null
git -C "$FLUSH_REPO" add -A && git -C "$FLUSH_REPO" commit -q -m "land the work"
write_transcript "$SBX/low.jsonl" 300000
cd "$FLUSH_REPO"
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
assert_absent "AC23: clean tree no flush block" "$OUT" '"decision":"block"'
cd "$SBX"

# === AC26: flush refusal - a foreign live writer rooted in the checkout =======
# (x-299b) The flush nudge counts dirty files and never asks who wrote them:
# twice in one night two kings were urged to commit 351 mid-flight lines that
# belonged to a live codex worker rooted in the same canonical checkout. Now
# the fire is gated on foreign_rooted_writers. Every arm asserts a string the
# outcome ALONE produces: the refusal NAMES the pid, because absence of a
# commit suggestion is also what a hook that never ran produces. The control
# arm is the same dirty checkout with no rooted foreign writer, where the
# suggestion must still appear - a guard that refuses everything reads as
# fixed and is not.
FLUSH_REPO="$SBX/flush-repo"
rm -f "$LATCHES"/.context-nudge-flush-* 2>/dev/null   # latch + static-HEAD state, not ctx bands
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do echo "f$i" > "$FLUSH_REPO/foreign-dirty$i.txt"; done
cd "$FLUSH_REPO"
sleep 300 &                                            # foreign writer: cwd = FLUSH_REPO
FPID=$!
# status "busy" is deliberate: a dispatched worker mid-turn projects an active
# status, not always "live", and the guard must see it anyway (review round 1).
jq -n --argjson pid "$FPID" '{schema_version: 13, agents: [{
  name:"t-foreign-probe", harness:"codex", cwd:"/tmp", log_path:"/tmp/fp",
  status:"busy", short_id:"fp", harness_session_id:"foreign-probe-sid",
  pid:$pid, crown_level:null, crown_scope:null, crown_grantor:null }]}' > "$SBX/.fno/agents/registry.json"
write_transcript "$SBX/low.jsonl" 300000
run_hook "$(payload "$SBX/low.jsonl")"                 # stop 1: static=1
assert_absent "AC26: stop 1 no block yet" "$OUT" '"decision":"block"'
run_hook "$(payload "$SBX/low.jsonl")"                 # stop 2: static=2
assert_absent "AC26: stop 2 no block yet" "$OUT" '"decision":"block"'
run_hook "$(payload "$SBX/low.jsonl")"                 # stop 3: static=3 -> fires
assert_contains "AC26: refusal NAMES the foreign pid" "$OUT" "$FPID"
assert_contains "AC26: refusal names the owning row" "$OUT" 't-foreign-probe'
assert_contains "AC26: refusal is a refusal" "$OUT" 'REFUSED'
assert_absent "AC26: refusal never advises a commit" "$OUT" 'commit it now'
events_has context_flush_refused && ok "AC26: context_flush_refused event emitted" || bad "AC26: no context_flush_refused event"

# Latch separation: the refusal touched the FOREIGN latch and left the flush
# latch unconsumed, so the real nudge can still fire once the writer leaves.
flush_latches=$(find "$LATCHES" -name '.context-nudge-flush-latch-*' 2>/dev/null | wc -l | tr -d ' ')
assert_eq "AC26: refusal leaves the flush latch unconsumed" "$flush_latches" "0"
foreign_latches=$(find "$LATCHES" -name '.context-nudge-flush-foreign-*' 2>/dev/null | wc -l | tr -d ' ')
[ "$foreign_latches" -gt 0 ] && ok "AC26: foreign latch written" || bad "AC26: no foreign latch written"

# Control arm: same dirty checkout, foreign writer gone and its registry row
# with it -> the commit suggestion must STILL appear.
kill "$FPID" 2>/dev/null
wait "$FPID" 2>/dev/null || true
write_registry no no                                   # live rows, none pid-bearing
rm -f "$LATCHES"/.context-nudge-flush-* 2>/dev/null
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC26: control arm still advises the commit" "$OUT" 'commit it now'

# Self arm: a live pid-bearing row whose session id IS the hook's own (the pid
# is this test shell, rooted in FLUSH_REPO by the cd above) -> self-excluded,
# proved by the nudge appearing, not by a refusal failing to appear.
jq -n --argjson pid "$$" '{schema_version: 13, agents: [{
  name:"t-self-probe", harness:"claude", cwd:"/tmp", log_path:"/tmp/sp",
  status:"live", short_id:"sp", harness_session_id:"'"$KING_SID"'",
  pid:$pid, crown_level:null, crown_scope:null, crown_grantor:null }]}' > "$SBX/.fno/agents/registry.json"
rm -f "$LATCHES"/.context-nudge-flush-* 2>/dev/null
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "AC26: own-session row does not refuse" "$OUT" 'commit it now'
assert_absent "AC26: own-session row does not refuse (marker)" "$OUT" 'REFUSED'

# Unreadable arm: lsof answers nothing -> the measurement could not be made ->
# fail closed with a refusal that says WHY, never read as nobody-is-here.
UNREADDIR="$(mktemp -d)"
printf '#!/usr/bin/env bash\nexit 1\n' > "$UNREADDIR/lsof"
chmod +x "$UNREADDIR/lsof"
_OLD_PATH="$PATH"; PATH="$UNREADDIR:$PATH"
rm -f "$LATCHES"/.context-nudge-flush-* 2>/dev/null
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
run_hook "$(payload "$SBX/low.jsonl")"
PATH="$_OLD_PATH"; rm -rf "$UNREADDIR"
assert_contains "AC26: unreadable measurement refuses" "$OUT" 'REFUSED'
assert_contains "AC26: refusal names the unreadable measurement" "$OUT" 'could not be measured'
cd "$SBX"

# === AC24: delta-by-shape - plan_path sets the wording ========================
# A /target session (plan_path bound) gets the flush wording; a bare session
# (no plan, no crown) gets the write-a-canon-doc wording. Same pressure, only
# the ask changes. plan_path comes from the target manifest, read best-effort.
rm -f "$LATCHES"/.context-nudge-* "$SBX/.fno/target-state.md" 2>/dev/null
write_registry no no
printf -- '---\nplan_path: /plans/test.md\n---\n' > "$SBX/.fno/target-state.md"
write_transcript "$SBX/t.jsonl" 500000
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC24: plan_path set -> plan-bound wording" "$OUT" 'plan bound'
assert_contains "AC24: plan-bound wording names SUMMARY.md" "$OUT" 'SUMMARY.md'
# no manifest -> neither shape -> canon-doc wording
rm -f "$LATCHES"/.context-nudge-* "$SBX/.fno/target-state.md" 2>/dev/null
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC24: no plan -> full-doc (canon doc) wording" "$OUT" 'canon doc'

# === AC25: king roll-up names peers (computed, not asked for) =================
# The king message states the neighbourhood roll-up from the same registry read.
# A peer king (disjoint scope) surfaces in the message; an isolated king gets no
# roll-up clutter (existing AC5/AC14 cover the zero-peer case unchanged).
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry yes no yes                              # crowned king + 1 peer king
write_transcript "$SBX/t.jsonl" 500000
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "AC25: king roll-up names the peer king" "$OUT" 'peer king'

# === compact gate: the advice matches a MEASURED injection path ================
# The hook used to prescribe a self-inject to every session unconditionally. A
# session with no injection path ran it, got a resolution miss, misread the miss as
# "busy with my turn", and stopped trying to compact. So the sentence is chosen by
# asking the one resolver, and BOTH answers are pinned here: a gate tested on one
# side only is the shape that shipped the bug.
# The compact probe uses owned ambient identity. Switch only this part of the
# harness to the actual parent harness so a Codex-run test does not present a
# contradicted Claude marker to `--to-self`.
if [ "$_SESSION_HARNESS" = "codex" ]; then
  unset CLAUDE_CODE_SESSION_ID
  export CODEX_THREAD_ID="$KING_SID"
fi
#
# not-injectable: a session with NO registry row of its own, which is the
# hand-started REPL that hit this bug. `--check` refuses at resolve_agent, before
# any transport, so this case is decided in Python and does not depend on whether
# the DEPLOYED fno-agents binary carries --probe. Keying it on the roster instead
# would make the assertion flip the moment the binary is updated.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_without_self
write_transcript "$SBX/t.jsonl" 500000
run_hook "$(payload "$SBX/t.jsonl")"
assert_contains "gate: no path -> says so"            "$OUT" 'has NO injection path'
assert_contains "gate: no path -> names the operator" "$OUT" 'ask your operator to type /compact'
# The needle is the PRESCRIPTION, not the substring: this branch legitimately
# quotes the `--to-self --raw --check` command it ran, so a bare '--to-self --raw'
# needle fails on the diagnostic rather than on advice to self-inject.
assert_absent   "gate: no path -> prescribes no self-inject" "$OUT" "fno agents mail send '/compact <brief-path>' --to-self --raw"
# The reason a session must not read as a liveness verdict, stated where it is read.
assert_contains "gate: miss is not a death claim" "$OUT" 'NOT a claim that you are dead'

# injectable: driven by a shim, because no fixture can honestly produce this answer
# for a SELF address in a sandbox. The mux lane is guarded and refuses a mid-turn
# recipient, and a session asking about itself is always mid-turn, so only the
# control.sock lane can answer yes to self - and that needs a real daemon roster
# this sandbox has no business faking. What is under test here is the hook's
# BRANCHING on the verdict; which verdict each lane deserves is pinned in
# cli/tests/test_mail_send_check.py, next to the code that decides it.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
check_shim() {  # check_shim <line-to-print> <exit-code>
  SHIMDIR="$(mktemp -d)"
  { printf '#!/usr/bin/env bash\n'
    printf 'if [ "$1" = "agents" ] && [ "$2" = "mail" ] && [ "$3" = "send" ]; then\n'
    printf '  for a in "$@"; do [ "$a" = "--check" ] && { printf "%%s\\n" %q; exit %s; }; done\n' "$1" "$2"
    printf 'fi\n'
    printf 'exec %q "$@"\n' "$BINDIR/fno"
  } > "$SHIMDIR/fno"
  chmod +x "$SHIMDIR/fno"
  OLD_PATH="$PATH"; PATH="$SHIMDIR:$PATH"
}
unshim() { PATH="$OLD_PATH"; rm -rf "$SHIMDIR"; }

check_shim 'injectable: control.sock (a paste can still refuse a busy prompt)' 0
run_hook "$(payload "$SBX/t.jsonl")"
unshim
assert_contains "gate: path -> prescribes the front door" "$OUT" "fno agents mail send '/compact <brief-path>' --to-self --raw"
assert_absent   "gate: path -> no operator fallback"      "$OUT" 'ask your operator to type'
# A path is not a landing, and the text must not promise one.
assert_contains "gate: path is not a landing" "$OUT" 'A path is not a landing'

# unmeasurable: the check could not resolve at all, so the hook must claim NEITHER
# verdict. This is the live case until the next `fno doctor update`: the probe shells the
# DEPLOYED fno-agents binary, and one too old to carry --probe answers
# 'unmeasurable: probe-unavailable'.
#
# Shimmed rather than PATH-emptied. Emptying PATH takes jq with it, and the hook
# needs jq for its own registry read and its output, so that variant tested a
# broken hook rather than an unmeasurable check. The shim intercepts only
# `mail send ... --check` and delegates everything else to the real fno, so the
# rest of the nudge (plan_path, handoff path, carveouts) behaves normally.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
check_shim 'unmeasurable: probe-unavailable (the fno-agents binary is absent, too old to carry --probe, or did not answer)' 3
run_hook "$(payload "$SBX/t.jsonl")"
unshim
assert_contains "gate: unmeasured -> says it did not measure" "$OUT" 'could not be measured here'
assert_absent   "gate: unmeasured -> claims no path"          "$OUT" 'has NO injection path'
assert_absent   "gate: unmeasured -> claims a path"           "$OUT" 'HAS an injection path'

# Neither branch may name the plumbing under the front door, nor a deleted verb.
HOOK_SRC="$(cat "$HOOK")"
assert_absent "hook: no raw mail-inject prescription" "$HOOK_SRC" 'mail-inject'
assert_absent "hook: no deleted crown verb"           "$HOOK_SRC" 'agents crown'
assert_absent "hook: no deleted succession flag"      "$HOOK_SRC" '--succeed'

# === Latch location + lifetime =================================================
# The hook wrote four dotfiles per session per band into the state-dir top level
# and deleted none of them: 395 of 527 root entries six days after it landed. A
# file nobody deletes is a file nobody owns, so these assert the location AND
# the deleter, not just the location.

# A fresh fire lands in latches/ and leaves the state-dir top level alone.
rm -rf "$LATCHES"; rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
write_registry yes no
write_transcript "$SBX/lt.jsonl" 500000
run_hook "$(payload "$SBX/lt.jsonl")"
new_latches=$(find "$LATCHES" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ')
root_latches=$(find "$SBX/.fno" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ')
[ "$new_latches" -gt 0 ] && ok "latch: fires into latches/" || bad "latch: nothing written to latches/ (found $new_latches)"
assert_eq "latch: nothing lands at the state-dir top level" "$root_latches" "0"
# Positive control: the counter can see a root file when one is really there.
touch "$SBX/.fno/.context-nudge-probe-control"
control=$(find "$SBX/.fno" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ')
assert_eq "latch: root counter is not blind" "$control" "1"

# A legacy top-level latch is swept on the next fire, whatever its age. That
# probe file from the positive control above is one, so the sweep must take it.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
touch "$SBX/.fno/.context-nudge-flush-legacy-session"
run_hook "$(payload "$SBX/lt.jsonl")"
swept=$(find "$SBX/.fno" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ')
assert_eq "latch: legacy top-level latches are swept" "$swept" "0"

# A latch older than two days is pruned; one inside the window survives.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
touch -t "$(date -u -r $(( $(date +%s) - 5*86400 )) +%Y%m%d%H%M 2>/dev/null || date -u -d '5 days ago' +%Y%m%d%H%M)" \
      "$LATCHES/.context-nudge-flush-stale-session"
touch "$LATCHES/.context-nudge-flush-fresh-session"
run_hook "$(payload "$SBX/lt.jsonl")"
[ -e "$LATCHES/.context-nudge-flush-stale-session" ] \
  && bad "latch: 5-day-old latch survived the prune" \
  || ok "latch: 5-day-old latch pruned"
[ -e "$LATCHES/.context-nudge-flush-fresh-session" ] \
  && ok "latch: fresh latch survives the prune" \
  || bad "latch: fresh latch was pruned"

# The location follows config.state_dir, not $HOME. Point state_dir at a dir
# that is NOT $HOME/.fno; a latch landing under $HOME/.fno means the hook read
# the home dir rather than the resolver, which is the bug one directory down.
ALT_STATE="$SBX/alt-state"
mkdir -p "$ALT_STATE"
printf 'schema_version: 1\nconfig:\n  state_dir: %s/\n' "$ALT_STATE" > "$SBX/alt-settings.yaml"
touch "$ALT_STATE/.path-migration-done"
cp "$SBX/.fno/config.toml" "$ALT_STATE/config.toml" 2>/dev/null || true
cp -R "$SBX/.fno/agents" "$ALT_STATE/agents" 2>/dev/null || true
rm -rf "$LATCHES"
( export FNO_CONFIG="$SBX/alt-settings.yaml"; run_hook "$(payload "$SBX/lt.jsonl")" \
  ; find "$ALT_STATE/latches" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ' > "$SBX/alt-count" )
alt_count="$(cat "$SBX/alt-count" 2>/dev/null || echo 0)"
home_count=$(find "$SBX/.fno/latches" -maxdepth 1 -type f -name '.context-nudge-*' 2>/dev/null | wc -l | tr -d ' ')
[ "$alt_count" -gt 0 ] && ok "latch: follows an overridden config.state_dir" \
  || bad "latch: nothing under the overridden state_dir (found $alt_count)"
assert_eq "latch: overridden state_dir writes nothing under \$HOME/.fno" "$home_count" "0"

assert_contains "latch: resolves through the shell stub" "$HOOK_SRC" 'fno config paths shell-stub'
assert_contains "latch: has a deleter" "$HOOK_SRC" '-mtime +2 -delete'

# latches/ absent at fire time must not error.
rm -rf "$LATCHES"
rm -f "$SBX/.fno"/.context-nudge-* 2>/dev/null
run_hook "$(payload "$SBX/lt.jsonl")"
assert_eq "latch: missing latches/ still exits 0" "$RC" "0"
[ -d "$LATCHES" ] && ok "latch: missing latches/ is created" || bad "latch: latches/ not created"
# Positive control for the three sweeps above: a needle that IS present, so a
# passing absent-assertion proves the haystack was read rather than empty.
assert_contains "hook: sweep positive control" "$HOOK_SRC" 'fno agents spawn -k'

# === AC2: one boot per hook fire, and the fold preserves the values (x-3c74) ===
# A counting fno shim: every invocation appends its argv to a counter file, so
# the assertion is the INVOCATION COUNT, not just the trigger values reaching
# the hook - a test that only checks values passes just as well on two boots as
# on one. Reused for both trigger discrimination and the boot count.
COUNT_BINDIR="$(mktemp -d)"
COUNTER="$SBX/fno-invocations.txt"
printf '#!/usr/bin/env bash\nexport PYTHONPATH="%s"\nprintf "%%s\\n" "$*" >> "%s"\nexec "%s" -m fno.cli "$@"\n' \
  "$FNO_SRC" "$COUNTER" "$FNO_PYTHON" > "$COUNT_BINDIR/fno"
cp "$COUNT_BINDIR/fno" "$COUNT_BINDIR/fno-py"
chmod +x "$COUNT_BINDIR/fno" "$COUNT_BINDIR/fno-py"

# Configure BOTH triggers away from their defaults (50/40) so a stale-value bug
# (e.g. a fold that reads the block but keeps hardcoded defaults) cannot pass.
printf '[target.handoff]\nking_used_pct_trigger = 41\nused_pct_trigger = 55\n' > "$SBX/.fno/config.toml"

# uncrowned, 54% (below the configured 55): the OLD default (50) would have
# blocked here, so a no-block proves the new value reached the hook, not just
# that some value did.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry no no
write_transcript "$SBX/ac2-below.jsonl" 540000
: > "$COUNTER"
OUT=$(printf '%s' "$(payload "$SBX/ac2-below.jsonl")" | PATH="$COUNT_BINDIR:$PATH" bash "$HOOK" 2>/dev/null); RC=$?
assert_absent "AC1-HP: 54% stays below the configured 55% trigger" "$OUT" '"decision":"block"'

# uncrowned, 55%: blocks, and the reason names the configured value.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/ac2-gen.jsonl" 550000
: > "$COUNTER"
OUT=$(printf '%s' "$(payload "$SBX/ac2-gen.jsonl")" | PATH="$COUNT_BINDIR:$PATH" bash "$HOOK" 2>/dev/null); RC=$?
assert_eq     "AC1-HP: uncrowned at 55% exits 0" "$RC" "0"
assert_contains "AC1-HP: reason names the configured general trigger (55%)" "$OUT" 'session compact trigger (55%)'
cfg_calls=$(grep -c 'config get' "$COUNTER"); cfg_calls="${cfg_calls:-0}"
assert_eq     "AC2-HP: exactly one config get invocation (uncrowned fire)" "$cfg_calls" "1"
assert_contains "AC2-HP: the one call names the block" "$(cat "$COUNTER")" 'config get target.handoff'
assert_absent "AC2-EDGE: no separate used_pct_trigger scalar read" "$(cat "$COUNTER")" 'target.handoff.used_pct_trigger'
assert_absent "AC2-EDGE: no separate king_used_pct_trigger scalar read" "$(cat "$COUNTER")" 'target.handoff.king_used_pct_trigger'

# crowned, 40% (below the configured king trigger of 41): the OLD default (40)
# would have blocked here too, so no-block proves the new king value landed.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry yes no
write_transcript "$SBX/ac2-king-below.jsonl" 400000
: > "$COUNTER"
OUT=$(printf '%s' "$(payload "$SBX/ac2-king-below.jsonl")" | PATH="$COUNT_BINDIR:$PATH" bash "$HOOK" 2>/dev/null); RC=$?
assert_absent "AC1-HP: crowned 40% stays below the configured 41% king trigger" "$OUT" '"decision":"block"'

# crowned, 41%: blocks, one boot.
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_transcript "$SBX/ac2-king.jsonl" 410000
: > "$COUNTER"
OUT=$(printf '%s' "$(payload "$SBX/ac2-king.jsonl")" | PATH="$COUNT_BINDIR:$PATH" bash "$HOOK" 2>/dev/null); RC=$?
assert_contains "AC1-HP: crowned at 41% blocks" "$OUT" '"decision":"block"'
cfg_calls=$(grep -c 'config get' "$COUNTER"); cfg_calls="${cfg_calls:-0}"
assert_eq     "AC2-HP: exactly one config get invocation (crowned fire)" "$cfg_calls" "1"

# AC2-EDGE (the regression the count exists to catch): a re-added second scalar
# read must fail the count assertion and the failure message names both lines.
FAKE_COUNTER="$SBX/fake-two-boots.txt"
printf 'config get target.handoff.used_pct_trigger\nconfig get target.handoff.king_used_pct_trigger\n' > "$FAKE_COUNTER"
fake_calls=$(grep -c 'config get' "$FAKE_COUNTER")
if [ "$fake_calls" != "1" ]; then
  ok "AC2-EDGE: a reintroduced second scalar read fails the count (got $fake_calls: $(cat "$FAKE_COUNTER" | tr '\n' ';'))"
else
  bad "AC2-EDGE: two-boot fixture should not count as one"
fi
rm -f "$FAKE_COUNTER"
rm -rf "$COUNT_BINDIR"

# === The liveness-primary partition, six cases each asserting the EXACT ====
# === printed alive number - "fewer than the total" is not enough. ==========
# A synthetic registry: the king row is fixed, the children carry whatever
# liveness shape the case needs. No case names bp-a238/bp-1939 (reaped from
# the real registry days ago) or any other live specimen - every row here is
# invented. Stamps are generated at call time: SERVED_LIVENESS_MAX_AGE_SECS is
# 120, so a hardcoded stamp ages past the window and the suite turns red on a
# clock, not on a defect.
# events.jsonl accumulates for the whole file (no other case here truncates
# it) - clear it once so the all-dead case below can trust a fresh read.
rm -f "$SBX/.fno/events.jsonl" 2>/dev/null

write_registry_liveness() {  # write_registry_liveness '<jq children array>'
  jq -n --argjson children "$1" '{
    schema_version: 13,
    agents: ( [{
      name:"king-test", harness:"claude", cwd:"/tmp", log_path:"/tmp/k",
      status:"live", short_id:"'"$KING_SID"'",
      harness_session_id:"'"$KING_SID"'",
      crown_level:1, crown_scope:"'"$SCOPE"'", crown_grantor:"human"
    }] + $children )
  }' > "$SBX/.fno/agents/registry.json"
}

# --- Mixed: 2 alive + 2 dead, both fresh. Prints 2; neither dead name shows. -
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CHILDREN=$(jq -nc --arg sid "$KING_SID" --arg ts "$FRESH_TS" '[
  {name:"live-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts},
  {name:"live-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live", short_id:"b", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts},
  {name:"dead-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/c", status:"live", short_id:"c", spawned_by_session:$sid, liveness:"dead", liveness_measured_at:$ts},
  {name:"dead-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/d", status:"live", short_id:"d", spawned_by_session:$sid, liveness:"dead", liveness_measured_at:$ts}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_liveness "$CHILDREN"
write_transcript "$SBX/low.jsonl" 300000
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "x-1b75 mixed: prints the alive count (2), not the row count (4)" "$OUT" 'Linked count: 2'
assert_absent   "x-1b75 mixed: dead-a never named" "$OUT" 'dead-a'
assert_absent   "x-1b75 mixed: dead-b never named" "$OUT" 'dead-b'

# --- All alive control: 3 alive, fresh. Prints 3 - forbids a blanket ---------
# --- subtraction, a hardcoded cap, or an off-by-one. -------------------------
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CHILDREN=$(jq -nc --arg sid "$KING_SID" --arg ts "$FRESH_TS" '[
  {name:"alive-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts},
  {name:"alive-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live", short_id:"b", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts},
  {name:"alive-c", harness:"claude", cwd:"/tmp", log_path:"/tmp/c", status:"live", short_id:"c", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_liveness "$CHILDREN"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "x-1b75 all-alive control: prints 3, not a subtraction or a cap" "$OUT" 'Linked count: 3'

# --- All dead: 3 dead, fresh. A confident negative is not an obligation, so --
# --- no orphan block and no event - a real, decidable outcome, not a gap. ---
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CHILDREN=$(jq -nc --arg sid "$KING_SID" --arg ts "$FRESH_TS" '[
  {name:"dead-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid, liveness:"dead", liveness_measured_at:$ts},
  {name:"dead-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live", short_id:"b", spawned_by_session:$sid, liveness:"dead", liveness_measured_at:$ts},
  {name:"dead-c", harness:"claude", cwd:"/tmp", log_path:"/tmp/c", status:"live", short_id:"c", spawned_by_session:$sid, liveness:"dead", liveness_measured_at:$ts}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
rm -f "$SBX/.fno/events.jsonl" 2>/dev/null
write_registry_liveness "$CHILDREN"
run_hook "$(payload "$SBX/low.jsonl")"
assert_absent "x-1b75 all-dead: no orphan reason when every spawned row is confidently dead" "$OUT" "cannot be a pure pass"
events_has king_orphan_block && bad "x-1b75 all-dead: king_orphan_block fired anyway" || ok "x-1b75 all-dead: no king_orphan_block event"

# --- Unresolved: no liveness field at all. Reported as unknown, excluded ----
# --- from the alive count, and the nudge still fires (a broken reader must --
# --- never silently clear this guard). ---------------------------------------
CHILDREN=$(jq -nc --arg sid "$KING_SID" '[
  {name:"unmeasured-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid},
  {name:"unmeasured-b", harness:"claude", cwd:"/tmp", log_path:"/tmp/b", status:"live", short_id:"b", spawned_by_session:$sid}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_liveness "$CHILDREN"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "x-1b75 unresolved: linked (alive) count is 0" "$OUT" 'Linked count: 0'
assert_contains "x-1b75 unresolved: names the unknown count and both rows" "$OUT" '2 spawned worker row(s) have unresolved liveness (unmeasured-a, unmeasured-b)'
assert_contains "x-1b75 unresolved: a broken reader never clears the guard" "$OUT" '"decision":"block"'

# --- Literal "unmeasured" word, fresh: liveness_sweep.rs writes this word ---
# --- verbatim on probe errors or an Orphaned status, so a fresh stamp serves --
# --- it through unchanged (not null). Must land in unresolved like the ------
# --- null case above, never silently pass a `== null` check that only -------
# --- catches the no-measurement case. ----------------------------------------
FRESH_TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
CHILDREN=$(jq -nc --arg sid "$KING_SID" --arg ts "$FRESH_TS" '[
  {name:"unmeasured-word-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid, liveness:"unmeasured", liveness_measured_at:$ts}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_liveness "$CHILDREN"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "x-1b75 unmeasured word: a fresh literal 'unmeasured' word is never alive" "$OUT" 'Linked count: 0'
assert_contains "x-1b75 unmeasured word: lands in unresolved, not silently dropped" "$OUT" '1 spawned worker row(s) have unresolved liveness (unmeasured-word-a)'

# --- Stale stamp: liveness alive, but the measurement is older than the -----
# --- 120s window - reads unknown, never alive. The republished-word trap. ---
STALE_TS=$(date -u -v-200S '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
  || date -u -d '200 seconds ago' '+%Y-%m-%dT%H:%M:%SZ')
CHILDREN=$(jq -nc --arg sid "$KING_SID" --arg ts "$STALE_TS" '[
  {name:"stale-a", harness:"claude", cwd:"/tmp", log_path:"/tmp/a", status:"live", short_id:"a", spawned_by_session:$sid, liveness:"alive", liveness_measured_at:$ts}
]')
rm -f "$LATCHES"/.context-nudge-* 2>/dev/null
write_registry_liveness "$CHILDREN"
run_hook "$(payload "$SBX/low.jsonl")"
assert_contains "x-1b75 stale: a stale alive word never counts as alive" "$OUT" 'Linked count: 0'
assert_contains "x-1b75 stale: the stale row lands in unresolved, not alive" "$OUT" '1 spawned worker row(s) have unresolved liveness (stale-a)'

echo ""
echo "================================"
echo "Results: $pass passed, $fail failed"
echo "================================"
[ "$fail" -eq 0 ]
