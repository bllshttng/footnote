#!/usr/bin/env bash
# test_spawn_guard.sh - the /agent spawn node-guard path (x-73cc).
#
# spawn.sh uses a read-only `fno agents spawn-guard` probe for early UX, while
# `fno agents spawn --node` owns the race-critical guard and reservation at the
# actual worker-birth choke point. These tests stub `fno` on PATH so each early
# verdict and late refusal receipt is exercised without a real daemon / claim
# store. Self-contained: real jq, stubbed fno. Run:
#
#   bash skills/agent/tests/test_spawn_guard.sh

set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPAWN="$HERE/../scripts/spawn.sh"
TMP="$(mktemp -d -t agents-spawn-guard.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

PASS=0
FAIL=0
ok()  { local l="$1"; if [[ "$2" == "$3" ]]; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); printf 'FAIL: %s (want %q got %q)\n' "$l" "$3" "$2"; fi; }
has() { local l="$1" hay="$2" needle="$3"; if printf '%s' "$hay" | grep -qF "$needle"; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); printf 'FAIL: %s (%q not in %q)\n' "$l" "$needle" "$hay"; fi; }
no()  { local l="$1" hay="$2" needle="$3"; if printf '%s' "$hay" | grep -qF "$needle"; then FAIL=$((FAIL+1)); printf 'FAIL: %s (%q UNEXPECTEDLY in %q)\n' "$l" "$needle" "$hay"; else PASS=$((PASS+1)); fi; }
field() { printf '%s\n' "$1" | sed -n "s/^result=\\([^ ]*\\).*/\\1/p;q"; }

# --- the fno stub: a programmable spawn-guard verdict + a call log -----------
STUBDIR="$TMP/bin"; mkdir -p "$STUBDIR"
cat > "$STUBDIR/fno" <<'STUB'
#!/usr/bin/env bash
echo "CALL: $*" >> "$STUB_LOG"
case "$1 $2" in
  "agents spawn-guard")
    [[ -n "${STUB_VERDICT:-}" ]] && printf '%s\n' "$STUB_VERDICT"
    exit "${STUB_VERDICT_RC:-0}" ;;
  "agents list")
    echo '{"agents":[]}'; exit 0 ;;
  "agents spawn"|"agents host")
    if [[ -n "${STUB_CLI_GUARD_REASON:-}" ]]; then
      node=""
      previous=""
      for argument in "$@"; do
        [[ "$previous" == "--node" ]] && node="$argument"
        previous="$argument"
      done
      echo "node dispatch refused: node=$node verdict=already-running reason=$STUB_CLI_GUARD_REASON; no worker launched" >&2
      exit 2
    fi
    echo "LAUNCH: $*" >> "$STUB_LOG"
    if [[ "${STUB_SPAWN_FAIL:-0}" == "1" ]]; then echo "spawn boom" >&2; exit 1; fi
    # short_id is programmable (default 8-hex) so the receipt-shape tests can feed
    # a daemon name-slug / empty / torn value. `-` (not `:-`) keeps an explicit "".
    # STUB_MUX=1 emits a Python mux-pane receipt: short_id "" + identity fields +
    # mux coords; the name/provider are taken from env so they can MATCH the launch
    # (spawn.sh accepts a pane receipt only when every identity field matches).
    if [[ "${STUB_MUX:-0}" == "1" ]]; then
      echo "{\"name\":\"${STUB_MUX_NAME-x}\",\"short_id\":\"${STUB_MUX_SHORT_ID-}\",\"session_id\":\"${STUB_MUX_SESSION_ID-}\",\"harness\":\"${STUB_MUX_PROVIDER-claude}\",\"status\":\"${STUB_MUX_STATUS-live}\",\"mux_session\":\"${STUB_MUX_SESSION-main}\",\"pane_id\":${STUB_PANE_ID-1}}"; exit 0
    fi
    echo "{\"name\":\"x\",\"short_id\":\"${STUB_SHORT_ID-deadbeef}\",\"harness\":\"claude\",\"status\":\"live\"}"; exit 0 ;;
  "claim release")
    exit 0 ;;
  *) exit 0 ;;
esac
STUB
chmod +x "$STUBDIR/fno"

run() {
  # run(): env-vars in, spawn.sh args after. Fresh call log per run.
  STUB_LOG="$TMP/calls.log"; : > "$STUB_LOG"
  export STUB_LOG
  PATH="$STUBDIR:$PATH" bash "$SPAWN" "$@"
}
calllog() { cat "$TMP/calls.log" 2>/dev/null; }

NODE="x-7777"

# --- dispatchable -> proceeds to spawn, honest launched receipt --------------
out="$(STUB_VERDICT='{"verdict":"dispatchable","reservation_key":"dispatch:'"$NODE"'","reservation_holder":"dispatch-skill:1"}' \
  run --name w1 --provider claude --message '/target x' --node "$NODE")"
ok 'dispatchable -> launched' "$(field "$out")" 'launched'
has 'dispatchable short_id' "$out" 'short_id=deadbeef'
has 'dispatchable did spawn' "$(calllog)" 'agents spawn --harness'

# --- live-claim already-running -> NO spawn, holder surfaced -----------------
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:owner"}' \
  run --name w2 --provider claude --message '/target x' --node "$NODE")"
ok 'live-claim -> already-running' "$(field "$out")" 'already-running'
has 'live-claim holder text' "$out" 'live worker holds node:x-7777 (target-session:owner)'
no  'live-claim did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- self-handoff: live claim is the CALLER's own -> guide, do NOT spawn ------
# --self matches .holder: distinct receipt routing to the sanctioned handoff.
# spawn.sh must NOT spawn and must NOT release the claim (authority is locked to
# handoff.sh / `fno backlog unclaim`, ab-588326a7).
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:owner"}' \
  run --name w2h --provider claude --message '/target x' --node "$NODE" --self 'target-session:owner')"
ok  'self-handoff -> self-handoff receipt' "$(field "$out")" 'self-handoff'
has 'self-handoff routes to sanctioned path' "$out" 'fno backlog unclaim'
no  'self-handoff did NOT spawn' "$(calllog)" 'agents spawn --harness'
no  'self-handoff did NOT release the node claim' "$(calllog)" 'claim release'

# --- self-handoff with a DIFFERENT holder -> still refuse (foreign) -----------
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:someone-else"}' \
  run --name w2f --provider claude --message '/target x' --node "$NODE" --self 'target-session:owner')"
ok  'foreign holder + --self -> still already-running' "$(field "$out")" 'already-running'
no  'foreign holder did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- unproven-claim -> already-running, and the receipt says what it MEASURED -
# x-2fe6 AC8-HP. A held claim proves a holder, never a worker. The old receipt
# asserted "live worker holds node" for a launch window that may never boot and
# for an external `backlog next --claim`, which is a claim with nobody behind it.
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"unproven-claim","holder":"spawn-handover:t-x-7777","init_reached":false}' \
  run --name w2u --provider claude --message '/target x' --node "$NODE")"
ok  'unproven-claim -> already-running' "$(field "$out")" 'already-running'
has 'unproven-claim names the untested condition' "$out" 'no worker has reached target init'
has 'unproven-claim names the holder' "$out" 'spawn-handover:t-x-7777'
no  'unproven-claim does NOT assert a live worker' "$out" 'live worker holds node'
no  'unproven-claim did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- an unproven claim that is the CALLER's own still routes to handoff -------
# The self-handoff receipt must stay reachable: reading unproven-claim into the
# fail-closed arm would lose it for a caller holding its own unbooted claim.
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"unproven-claim","holder":"target-session:owner","init_reached":false}' \
  run --name w2uh --provider claude --message '/target x' --node "$NODE" --self 'target-session:owner')"
ok  'unproven + --self -> self-handoff receipt' "$(field "$out")" 'self-handoff'
no  'unproven self-handoff did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- unproven-claim on the POST-SPAWN refusal, the path a real dispatch takes -
# The probe above runs before the spawn. This block fires when the claim is
# taken in the window between them. An arm on only one of the two reads as
# protection and ships green while the live path stays broken: without it the
# reason falls past the esac and a benign skip becomes result=failed, exit 1.
out="$(STUB_VERDICT='{"verdict":"dispatchable"}' STUB_CLI_GUARD_REASON=unproven-claim \
  run --name w2p --provider claude --message '/target x' --node "$NODE")"; rc=$?
ok  'post-spawn unproven-claim -> already-running' "$(field "$out")" 'already-running'
ok  'post-spawn unproven-claim exits 0' "$rc" '0'
has 'post-spawn unproven-claim names the untested condition' "$out" 'no worker has reached target init'
no  'post-spawn unproven-claim does NOT assert a live worker' "$out" 'live worker holds node'

# --- every guard reason has an arm in BOTH consumers -------------------------
# One Python producer, two shell consumers, and neither case statement carries a
# default: an unhandled reason falls through to a failure handler and turns a
# benign skip into a hard failure. That is how unproven-claim shipped covering
# only the probe path.
#
# CEILING, stated rather than implied: this list is written here, not derived
# from the producer, because the reasons are built in a ternary and no grep
# reads them reliably. So it catches a reason DROPPED from Python and a reason
# missing an arm. It does NOT catch a brand-new reason nobody added here.
_GUARD_REASONS='live-claim unproven-claim suspect-claim reservation-held duplicate-claim auto-deferred defer-failed'
_SPAWN_SH_LOCAL="$(dirname "${BASH_SOURCE[0]}")/../scripts/spawn.sh"
_CLI_PY="$(dirname "${BASH_SOURCE[0]}")/../../../cli/src/fno/agents/cli.py"
_DISPATCH_SH="$(dirname "${BASH_SOURCE[0]}")/../../target/scripts/dispatch-node.sh"
for reason in $_GUARD_REASONS; do
  if grep -qF "\"$reason\"" "$_CLI_PY"; then
    ok "producer still emits: $reason" "present" "present"
  else
    ok "producer still emits: $reason" "GONE from cli.py (stale list)" "present"
  fi
  for consumer in "$_SPAWN_SH_LOCAL" "$_DISPATCH_SH"; do
    label="$(basename "$consumer")"
    if grep -qE "^[[:space:]]*[a-z|-]*${reason}[a-z|-]*\)" "$consumer"; then
      ok "$label has an arm for: $reason" "handled" "handled"
    else
      ok "$label has an arm for: $reason" "UNHANDLED (falls to the failure handler)" "handled"
    fi
  done
done

# --- reservation acquired by a peer between probe and launch -> NO worker ----
out="$(STUB_VERDICT='{"verdict":"dispatchable"}' STUB_CLI_GUARD_REASON=reservation-held \
  run --name w3 --provider claude --message '/target x' --node "$NODE")"
ok 'reservation-held -> already-running' "$(field "$out")" 'already-running'
has 'reservation-held -> duplicate-claim receipt' "$out" 'skipped: duplicate-claim (peer dispatcher holds dispatch:x-7777)'
has 'reservation-held action' "$out" 'action=duplicate-claim'
no  'reservation-held did NOT launch a worker' "$(calllog)" 'LAUNCH:'

# --- corrupted -> failed, NO spawn -------------------------------------------
out="$(STUB_VERDICT='{"verdict":"corrupted","detail":"node:'"$NODE"' claim is corrupted; force-release or repair before dispatching"}' \
  run --name w4 --provider claude --message '/target x' --node "$NODE")"
ok 'corrupted -> failed' "$(field "$out")" 'failed'
has 'corrupted reason' "$out" 'claim is corrupted; force-release or repair'
no  'corrupted did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- stale fno WITHOUT the verb -> fail CLOSED, NO spawn ----------------------
# empty stdout + non-zero rc (Typer "No such command" goes to stderr, suppressed).
out="$(STUB_VERDICT='' STUB_VERDICT_RC=2 \
  run --name w5 --provider claude --message '/target x' --node "$NODE")"
ok 'verb-absent -> failed (fail-closed)' "$(field "$out")" 'failed'
has 'verb-absent reason' "$out" 'spawn-guard unavailable'
no  'verb-absent did NOT spawn' "$(calllog)" 'agents spawn --harness'

# --- spawn-guard returns dispatchable but the launch FAILS -> failed -----------
out="$(STUB_VERDICT='{"verdict":"dispatchable","reservation_key":"dispatch:'"$NODE"'","reservation_holder":"dispatch-skill:1"}' \
  STUB_SPAWN_FAIL=1 \
  run --name w6 --provider claude --message '/target x' --node "$NODE")"
ok 'spawn-fail -> failed' "$(field "$out")" 'failed'
no 'wrapper does not duplicate CLI reservation cleanup' "$(calllog)" 'claim release dispatch:x-7777'

# --- no NODE (free-text) -> guard SKIPPED, spawn-guard never called ----------
out="$(STUB_VERDICT='{"verdict":"SHOULD_NOT_BE_READ"}' \
  run --name w7 --provider claude --message 'just a free-text task')"
ok 'no-node -> launched' "$(field "$out")" 'launched'
no  'no-node skipped guard' "$(calllog)" 'agents spawn-guard'

# --- receipt SHAPE by substrate (x-61b7) -------------------------------------
# The default/pane substrate is the owned-PTY daemon worker; derive_short_id()
# (daemon.rs) hands it a NAME-SLUG short_id, not 8-hex. The guard must accept it.
DISP='{"verdict":"dispatchable","reservation_key":"dispatch:'"$NODE"'","reservation_holder":"dispatch-skill:1"}'

# AC1-HP: a pane name-slug receipt -> launched (was a false `failed`).
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='spawngoa' \
  run --name spawn-goal --provider claude --message '/target x' --node "$NODE")"
ok  'pane slug -> launched'        "$(field "$out")" 'launched'
has 'pane slug short_id surfaced' "$out" 'short_id=spawngoa'

# AC1-HP2: a name-slug with a numeric collision suffix (base{n}) -> launched.
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='spawnthi1' \
  run --name spawn-think --provider claude --message '/target x' --node "$NODE")"
ok  'pane slug+suffix -> launched' "$(field "$out")" 'launched'

# AC1-ERR: an empty .short_id still FAILS on the pane lane (cardinal guard).
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='' \
  run --name spawn-empty --provider claude --message '/target x' --node "$NODE")"
ok 'pane empty short_id -> failed' "$(field "$out")" 'failed'

# AC1-ERR2: a multi-line .short_id (banner leak) still FAILS (whole-string match).
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='junk\ndeadbeef' \
  run --name spawn-torn --provider claude --message '/target x' --node "$NODE")"
ok 'pane torn short_id -> failed'  "$(field "$out")" 'failed'

# AC1-EDGE: the bg lane keeps the strict 8-hex rule. A real 8-hex validates...
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='b92eec14' \
  run --name spawn-bg --provider claude --message '/target x' --node "$NODE" --substrate bg)"
ok 'bg 8-hex -> launched'          "$(field "$out")" 'launched'
# ...but a name-slug on the bg lane is still rejected (that lane really returns hex).
out="$(STUB_VERDICT="$DISP" STUB_SHORT_ID='spawngoa' \
  run --name spawn-bg2 --provider claude --message '/target x' --node "$NODE" --substrate bg)"
ok 'bg slug -> failed'             "$(field "$out")" 'failed'

# --- pane worker observability hint (PR #341 delta) --------------------------
# A matched mux-pane receipt launches (main's verified-identity path), but a pane
# row has no log_path -- the report must point at `fno mux attach <session>`, not
# `fno agents logs <name>` (which returns "no logs"). short_id (=name) stays.

# AC-HP: pane launch -> launched, pane coords + mux-attach hint, NOT `agents logs`.
out="$(STUB_MUX=1 STUB_MUX_NAME=paneW STUB_MUX_PROVIDER=codex STUB_MUX_SESSION=main \
  STUB_MUX_SHORT_ID=019fb024 STUB_MUX_SESSION_ID=019fb024-2327-75f3-8b80-06e9d5ade05f \
  run --name paneW --provider codex --message 'Implement x')"
ok  'pane launch -> launched'        "$(field "$out")" 'launched'
has 'pane coords surfaced'           "$out" 'pane="main:1"'
has 'pane hint is mux attach'        "$out" 'fno mux attach main'
no  'pane hint is NOT agents logs'   "$out" 'fno agents logs'

# AC2-CON: a created Codex pane without a thread ID is pending, not launched.
out="$(STUB_MUX=1 STUB_MUX_NAME=paneP STUB_MUX_PROVIDER=codex STUB_MUX_STATUS=spawning \
  STUB_MUX_SESSION=main run --name paneP --provider codex --message 'Implement x')"
ok  'pane pending -> pending'         "$(field "$out")" 'pending'
has 'pane pending coords surfaced'    "$out" 'pane="main:1"'
no  'pane pending is not launched'    "$out" 'result=launched'

out="$(STUB_MUX=1 STUB_MUX_NAME=paneI STUB_MUX_PROVIDER=codex STUB_MUX_STATUS=live \
  STUB_MUX_SESSION=main run --name paneI --provider codex --message 'Implement x')"
ok  'pane idless live -> failed'       "$(field "$out")" 'failed'

out="$(STUB_MUX=1 STUB_MUX_NAME=paneB STUB_MUX_PROVIDER=codex STUB_MUX_STATUS=live \
  STUB_MUX_SESSION=main STUB_MUX_SHORT_ID=deadbeef \
  run --name paneB --provider codex --message 'Implement x')"
ok  'pane live partial identity -> failed' "$(field "$out")" 'failed'

out="$(STUB_MUX=1 STUB_MUX_NAME=paneT STUB_MUX_PROVIDER=codex STUB_MUX_STATUS=spawning \
  STUB_MUX_SESSION=main STUB_MUX_SHORT_ID=deadbeef \
  run --name paneT --provider codex --message 'Implement x')"
ok  'pane pending torn identity -> failed' "$(field "$out")" 'failed'

# AC-EDGE: a session name with a space -> ref quoted so it can't split the line.
out="$(STUB_MUX=1 STUB_MUX_NAME=paneS STUB_MUX_PROVIDER=codex STUB_MUX_SESSION='work 1' \
  STUB_MUX_SHORT_ID=019fb024 STUB_MUX_SESSION_ID=019fb024-2327-75f3-8b80-06e9d5ade05f \
  run --name paneS --provider codex --message 'Implement x')"
ok  'pane spaced session -> launched' "$(field "$out")" 'launched'
has 'pane spaced ref quoted'          "$out" 'pane="work 1:1"'

# AC-ERR: identity mismatch (receipt name != launch name) -> failed, no fake handle.
out="$(STUB_MUX=1 STUB_MUX_NAME=someone-else STUB_MUX_PROVIDER=codex \
  run --name paneM --provider codex --message 'Implement x')"
ok 'pane identity mismatch -> failed' "$(field "$out")" 'failed'

# Regression: a long requested name must not replace the Codex thread handle.
LONGNAME='spawn-x-7624-dedup-check-before-target-disp'
out="$(STUB_MUX=1 STUB_MUX_NAME="$LONGNAME" STUB_MUX_PROVIDER=codex STUB_MUX_SESSION=main \
  STUB_MUX_SHORT_ID=019fb024 STUB_MUX_SESSION_ID=019fb024-2327-75f3-8b80-06e9d5ade05f \
  run --name "$LONGNAME" --provider codex --message 'Implement x')"
ok  'pane long name -> launched'      "$(field "$out")" 'launched'
has 'pane long name short_id'         "$out" 'short_id=019fb024'

# --- outcome vocabulary is a contract, not a convention ----------------------
# Every `result=` spawn.sh can print must be documented in SKILL.md, because the
# /fno:agent model relays that line and has no defined behaviour for an outcome
# the contract never mentions - it may report a normal result as a failure. The
# script is the source of truth here; the doc follows it. Two outcomes had
# already drifted out of the contract when this check was added.
_SPAWN_SH="$(dirname "${BASH_SOURCE[0]}")/../scripts/spawn.sh"
_SKILL_MD="$(dirname "${BASH_SOURCE[0]}")/../SKILL.md"
while IFS= read -r outcome; do
  [[ -z "$outcome" ]] && continue
  if grep -q "result=$outcome" "$_SKILL_MD"; then
    ok "outcome documented: $outcome" "documented" "documented"
  else
    ok "outcome documented: $outcome" "undocumented in SKILL.md" "documented"
  fi
done < <(
  {
    grep -oE "result=[a-z][a-z-]*" "$_SPAWN_SH"
    # `printf 'result=%s'` takes its value from a variable; capture those too.
    grep -oE 'pane_result="[a-z][a-z-]*"' "$_SPAWN_SH" | sed 's/pane_result="/result=/; s/"$//'
  } | sed 's/^result=//' | sort -u
)

# --- summary -----------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
