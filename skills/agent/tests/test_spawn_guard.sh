#!/usr/bin/env bash
# test_spawn_guard.sh - the /agent spawn node-guard path (x-73cc).
#
# spawn.sh delegates the race-critical Guard 1 (node:<id> claim probe) + Guard 2
# (dispatch:<id> reservation) to `fno agents spawn-guard`. These tests stub `fno`
# on PATH so each verdict branch is exercised without a real daemon / claim
# store, and assert the honest `result=` receipts + that no spawn/launch happens
# on a non-dispatchable verdict (fail-closed). Self-contained: real jq, stubbed
# fno. Run:
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
field() { printf '%s\n' "$1" | sed -n "s/^result=//p" | head -1 | awk '{print $1}'; }

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
    if [[ "${STUB_SPAWN_FAIL:-0}" == "1" ]]; then echo "spawn boom" >&2; exit 1; fi
    # short_id is programmable (default 8-hex) so the receipt-shape tests can feed
    # a daemon name-slug / empty / torn value. `-` (not `:-`) keeps an explicit "".
    echo "{\"name\":\"x\",\"short_id\":\"${STUB_SHORT_ID-deadbeef}\",\"provider\":\"claude\",\"status\":\"live\"}"; exit 0 ;;
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
has 'dispatchable did spawn' "$(calllog)" 'agents spawn --provider'

# --- live-claim already-running -> NO spawn, holder surfaced -----------------
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:owner"}' \
  run --name w2 --provider claude --message '/target x' --node "$NODE")"
ok 'live-claim -> already-running' "$(field "$out")" 'already-running'
has 'live-claim holder text' "$out" 'live worker holds node:x-7777 (target-session:owner)'
no  'live-claim did NOT spawn' "$(calllog)" 'agents spawn --provider'

# --- self-handoff: live claim is the CALLER's own -> guide, do NOT spawn ------
# --self matches .holder: distinct receipt routing to the sanctioned handoff.
# spawn.sh must NOT spawn and must NOT release the claim (authority is locked to
# handoff.sh / `fno backlog unclaim`, ab-588326a7).
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:owner"}' \
  run --name w2h --provider claude --message '/target x' --node "$NODE" --self 'target-session:owner')"
ok  'self-handoff -> self-handoff receipt' "$(field "$out")" 'self-handoff'
has 'self-handoff routes to sanctioned path' "$out" 'fno backlog unclaim'
no  'self-handoff did NOT spawn' "$(calllog)" 'agents spawn --provider'
no  'self-handoff did NOT release the node claim' "$(calllog)" 'claim release'

# --- self-handoff with a DIFFERENT holder -> still refuse (foreign) -----------
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"live-claim","holder":"target-session:someone-else"}' \
  run --name w2f --provider claude --message '/target x' --node "$NODE" --self 'target-session:owner')"
ok  'foreign holder + --self -> still already-running' "$(field "$out")" 'already-running'
no  'foreign holder did NOT spawn' "$(calllog)" 'agents spawn --provider'

# --- reservation-held already-running -> NO spawn ----------------------------
out="$(STUB_VERDICT='{"verdict":"already-running","reason":"reservation-held"}' \
  run --name w3 --provider claude --message '/target x' --node "$NODE")"
ok 'reservation-held -> already-running' "$(field "$out")" 'already-running'
has 'reservation-held text' "$out" 'a peer dispatcher holds dispatch:x-7777 (racing launch)'
no  'reservation-held did NOT spawn' "$(calllog)" 'agents spawn --provider'

# --- corrupted -> failed, NO spawn -------------------------------------------
out="$(STUB_VERDICT='{"verdict":"corrupted","detail":"node:'"$NODE"' claim is corrupted; force-release or repair before dispatching"}' \
  run --name w4 --provider claude --message '/target x' --node "$NODE")"
ok 'corrupted -> failed' "$(field "$out")" 'failed'
has 'corrupted reason' "$out" 'claim is corrupted; force-release or repair'
no  'corrupted did NOT spawn' "$(calllog)" 'agents spawn --provider'

# --- stale fno WITHOUT the verb -> fail CLOSED, NO spawn ----------------------
# empty stdout + non-zero rc (Typer "No such command" goes to stderr, suppressed).
out="$(STUB_VERDICT='' STUB_VERDICT_RC=2 \
  run --name w5 --provider claude --message '/target x' --node "$NODE")"
ok 'verb-absent -> failed (fail-closed)' "$(field "$out")" 'failed'
has 'verb-absent reason' "$out" 'spawn-guard unavailable'
no  'verb-absent did NOT spawn' "$(calllog)" 'agents spawn --provider'

# --- spawn-guard returns dispatchable but the launch FAILS -> release + failed
out="$(STUB_VERDICT='{"verdict":"dispatchable","reservation_key":"dispatch:'"$NODE"'","reservation_holder":"dispatch-skill:1"}' \
  STUB_SPAWN_FAIL=1 \
  run --name w6 --provider claude --message '/target x' --node "$NODE")"
ok 'spawn-fail -> failed' "$(field "$out")" 'failed'
has 'spawn-fail released reservation' "$(calllog)" 'claim release dispatch:x-7777'

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

# --- summary -----------------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
