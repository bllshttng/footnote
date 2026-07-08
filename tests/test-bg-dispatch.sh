#!/usr/bin/env bash
# test-bg-dispatch.sh - regression tests for Phase 2 (ab-e366539f):
#   - US5 targeted bg-dispatch  (skills/target/scripts/dispatch-node.sh)
#   - US6 ready-gated auto-launch (skills/blueprint/scripts/autolaunch-on-ready.sh)
#
# Hermetic: a mock `fno` on PATH stands in for backlog/claim/agents, so NO real
# bg worker is launched and NO real backlog/claim state is touched. The
# auto-launch gate is controlled via an exported get_config stub (the
# test_dedupe_dead_duplicates pattern), so the dotted config key needs no yq.
#
# Coverage: AC5-HP/ERR/UI/EDGE/FR, AC6-HP/ERR/UI/EDGE/FR, the node:<id> claim
# double-dispatch guard, the ready/blocked/deferred gate, the no-merge default,
# and the planning-session-not-mutated invariant.
#
# Exit codes: 0 pass | 1 assertion failed | 77 skipped (missing deps).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"
AUTOLAUNCH="$REPO_ROOT/skills/blueprint/scripts/autolaunch-on-ready.sh"

PASS=0; FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
skip() { printf 'SKIP: %s\n' "$*" >&2; exit 77; }

command -v jq  >/dev/null 2>&1 || skip "jq required"
[[ -f "$DISPATCH" ]]   || skip "dispatch-node.sh missing"
[[ -f "$AUTOLAUNCH" ]] || skip "autolaunch-on-ready.sh missing"

TMP=$(mktemp -d -t bg-dispatch.XXXXXX)
trap 'rm -rf "$TMP"' EXIT

# ---- mock fno -------------------------------------------------------------
MOCKBIN="$TMP/bin"; mkdir -p "$MOCKBIN"
MOCKSTATE="$TMP/mock"; mkdir -p "$MOCKSTATE"
cat > "$MOCKBIN/fno" <<'MOCK'
#!/usr/bin/env bash
set -uo pipefail
S="${MOCK_STATE:?}"
sub="${1:-}"; verb="${2:-}"
case "$sub $verb" in
  "backlog get")
    id="${3:-}"
    [[ -f "$S/get_err" ]] && exit 1            # simulate a transient read failure
    if [[ -f "$S/status_$id" ]]; then
      # Emit pr_number (open-PR guard sim) when pr_<id> is set; otherwise
      # omit the field so the guard's `.pr_number // empty` stays empty.
      pr_fragment=""
      [[ -f "$S/pr_$id" ]] && pr_fragment=",\"pr_number\":\"$(cat "$S/pr_$id")\",\"completed_at\":null"
      # Emit _resolved_cwd when set, otherwise omit the field (stale-abi sim).
      if [[ -f "$S/resolved_cwd_$id" ]]; then
        printf '{"id":"%s","_status":"%s","_resolved_cwd":"%s","cwd":"%s"%s}\n' \
          "$id" "$(cat "$S/status_$id")" \
          "$(cat "$S/resolved_cwd_$id")" \
          "$(cat "$S/cwd_$id" 2>/dev/null || echo "")" "$pr_fragment"
      else
        printf '{"id":"%s","_status":"%s","cwd":"%s"%s}\n' \
          "$id" "$(cat "$S/status_$id")" "$(cat "$S/cwd_$id" 2>/dev/null || echo "")" "$pr_fragment"
      fi
    else
      exit 1   # unknown node -> nonzero, no output (mirrors not-found)
    fi ;;
  "backlog ready")
    [[ -f "$S/ready_err" ]] && exit 1          # simulate an enumeration failure
    cat "$S/ready.json" 2>/dev/null || echo "[]" ;;
  "agents spawn-guard")
    # x-73cc: the shared bg-dispatch guard. dispatch-node.sh now calls this
    # instead of `claim status` + `claim acquire`. Synthesize the verdict from
    # the same mock state (claim_$id, claim_err, reserve_held) so every existing
    # scenario keeps exercising the wrapper's branch mapping. --no-reserve (the
    # dry-run / claimed path) runs Guard 1 only.
    id="${3:-}"; no_reserve=0
    for a in "$@"; do [[ "$a" == "--no-reserve" ]] && no_reserve=1; done
    [[ -f "$S/claim_err" ]] && { printf '{"verdict":"error","detail":"claim probe failed (mock); not dispatching to avoid a double-launch"}\n'; exit 3; }
    st="$(cat "$S/claim_$id" 2>/dev/null || echo free)"
    case "$st" in
      live)      printf '{"verdict":"already-running","reason":"live-claim","holder":"target-session:holder-%s"}\n' "$id"; exit 0 ;;
      corrupted) printf '{"verdict":"corrupted","detail":"node:%s claim is corrupted; force-release or repair before dispatching"}\n' "$id"; exit 0 ;;
    esac
    # free / stale -> dispatchable candidate
    if [[ "$no_reserve" -eq 1 ]]; then printf '{"verdict":"dispatchable"}\n'; exit 0; fi
    [[ -f "$S/reserve_held" ]] && { printf '{"verdict":"already-running","reason":"reservation-held"}\n'; exit 0; }
    printf '{"verdict":"dispatchable","reservation_key":"dispatch:%s","reservation_holder":"dispatch-node:mock"}\n' "$id"; exit 0 ;;
  "claim status")
    key="${3:-}"; id="${key#node:}"
    [[ -f "$S/claim_err" ]] && exit 1          # simulate a probe crash (nonzero, no stdout)
    printf '{"state":"%s","holder":"target-session:holder-%s"}\n' \
      "$(cat "$S/claim_$id" 2>/dev/null || echo free)" "$id" ;;
  "claim acquire")
    # dispatcher reservation (dispatch:<id>). reserve_held models a racing peer.
    [[ -f "$S/reserve_held" ]] && { echo "held by other" >&2; exit 1; }
    echo "acquired ${3:-}"; exit 0 ;;
  "claim release")
    echo "released ${3:-}"; exit 0 ;;
  "agents list")
    [[ -f "$S/agents_list_err" ]] && exit 1   # simulate a crashed probe (daemon down)
    [[ -f "$S/agents_list_garbage" ]] && { echo "<<not json>>"; exit 0; }
    cat "$S/agents_list.json" 2>/dev/null || echo '{"agents":[]}' ;;
  "agents rm")
    printf 'rm %s\n' "${3:-}" >> "$S/rm.log"; echo "removed ${3:-}" ;;
  "agents spawn")
    printf '%s\n' "$*" >> "$S/ask.log"
    [[ -f "$S/ask.fail" ]] && { echo "daemon down: connection refused" >&2; exit 1; }
    # ask_collision models a racing worker: spawn refuses an existing name with
    # exit 2 (Group 1: spawn never resumes; the old ask resume path is gone).
    if [[ -f "$S/ask_collision" ]]; then
      echo "agent 'tgt-aaaa1111' already exists; use 'fno agents rm tgt-aaaa1111' first or pick another name" >&2; exit 2
    fi
    # ask_noid models a broken receipt: exit 0 but no parseable short_id.
    if [[ -f "$S/ask_noid" ]]; then echo "Sure, starting on that now."; else echo '{"name": "tgt-aaaa1111", "short_id": "deadbeef01", "provider": "claude", "status": "live"}'; fi ;;
  *) exit 0 ;;
esac
MOCK
chmod +x "$MOCKBIN/fno"
export MOCK_STATE="$MOCKSTATE"
export PATH="$MOCKBIN:$PATH"

set_status() { echo "$2" > "$MOCKSTATE/status_$1"; }
set_claim()  { echo "$2" > "$MOCKSTATE/claim_$1"; }
set_agent_live() { printf '{"agents":[{"name":"%s","status":"%s"}]}\n' "$1" "$2" > "$MOCKSTATE/agents_list.json"; }
set_cwd() { echo "$2" > "$MOCKSTATE/cwd_$1"; }
set_resolved_cwd() { echo "$2" > "$MOCKSTATE/resolved_cwd_$1"; }
set_pr() { echo "$2" > "$MOCKSTATE/pr_$1"; }   # node carries an open (unmerged) PR
reset_mock() { rm -f "$MOCKSTATE"/status_* "$MOCKSTATE"/claim_* "$MOCKSTATE"/cwd_* "$MOCKSTATE"/resolved_cwd_* "$MOCKSTATE"/pr_* "$MOCKSTATE"/ask.log "$MOCKSTATE"/ask.fail "$MOCKSTATE"/ask_collision "$MOCKSTATE"/ready.json "$MOCKSTATE"/claim_err "$MOCKSTATE"/ready_err "$MOCKSTATE"/get_err "$MOCKSTATE"/ask_noid "$MOCKSTATE"/reserve_held "$MOCKSTATE"/agents_list.json "$MOCKSTATE"/agents_list_err "$MOCKSTATE"/agents_list_garbage "$MOCKSTATE"/rm.log 2>/dev/null || true; }
ask_count()  { [[ -f "$MOCKSTATE/ask.log" ]] && wc -l < "$MOCKSTATE/ask.log" | tr -d ' ' || echo 0; }

echo "=============================================="
echo "US5 - targeted bg-dispatch (dispatch-node.sh)"
echo "=============================================="

# ---- AC5-HP: single ready node launches via fno agents spawn, no --bare/-p ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^launched ab-aaaa1111 name=target-ab-aaaa1111 session=deadbeef01" \
  && pass "AC5-HP: ready node launched with stable target-<full-id> name + session" \
  || fail "AC5-HP: expected launched line, got: $out"
grep -q -- "--provider claude" "$MOCKSTATE/ask.log" \
  && pass "AC5-HP: dispatch used fno agents spawn --provider claude" \
  || fail "AC5-HP: ask.log missing --provider claude"
if grep -Eq -- "(^| )(--bare|-p)( |$)" "$MOCKSTATE/ask.log"; then
  fail "AC5-HP: FORBIDDEN --bare/-p reached the dispatch (must be subscription lane)"
else
  pass "AC5-HP: never --bare/-p (subscription lane only)"
fi
grep -q "/target no-merge ab-aaaa1111" "$MOCKSTATE/ask.log" \
  && pass "AC5-HP: no-merge injected by default" \
  || fail "AC5-HP: no-merge not injected (ask.log: $(cat "$MOCKSTATE/ask.log"))"

# ---- AC5-HP batch + --allow-merge suppresses no-merge ----
reset_mock; set_status ab-aaaa1111 ready; set_status ab-bbbb2222 ready
out="$(bash "$DISPATCH" --allow-merge ab-aaaa1111 ab-bbbb2222 2>&1)"
[[ "$(echo "$out" | grep -c '^launched ')" -eq 2 ]] \
  && pass "AC5-HP: batch launches both ready nodes" \
  || fail "AC5-HP: batch expected 2 launched, got: $out"
grep -q "no-merge" "$MOCKSTATE/ask.log" \
  && fail "AC5-HP: --allow-merge should suppress no-merge but ask.log has it" \
  || pass "AC5-HP: --allow-merge suppresses the no-merge default"

# ---- AC5-ERR: dispatch failure surfaces, node stays ready, exit 1, no fallback ----
reset_mock; set_status ab-aaaa1111 ready; : > "$MOCKSTATE/ask.fail"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"; rc=$?
echo "$out" | grep -q "^failed ab-aaaa1111 reason=" \
  && pass "AC5-ERR: dispatch failure surfaced (not silent)" \
  || fail "AC5-ERR: expected failed line, got: $out"
[[ "$rc" -eq 1 ]] \
  && pass "AC5-ERR: exit 1 when nothing launched + a hard failure" \
  || fail "AC5-ERR: expected exit 1, got $rc"
# Node status in the mock is unchanged (the script never mutates the node).
[[ "$(cat "$MOCKSTATE/status_ab-aaaa1111")" == "ready" ]] \
  && pass "AC5-ERR: node left ready/re-dispatchable (never mutated)" \
  || fail "AC5-ERR: node status changed unexpectedly"

# ---- AC5-UI: mixed batch -> a distinct line per node, none silent ----
reset_mock
set_status ab-aaaa1111 ready                       # -> launched
set_status ab-cccc3333 done                        # -> skipped-done
set_status ab-dddd4444 claimed; set_claim ab-dddd4444 live   # -> already-running
# ab-eeee5555 has no status_ file -> failed (no such node)
out="$(bash "$DISPATCH" ab-aaaa1111 ab-cccc3333 ab-dddd4444 ab-eeee5555 2>&1)"
echo "$out" | grep -q "^launched ab-aaaa1111 "        && \
echo "$out" | grep -q "^skipped-done ab-cccc3333 "    && \
echo "$out" | grep -q "^already-running ab-dddd4444 " && \
echo "$out" | grep -q "^failed ab-eeee5555 " \
  && pass "AC5-UI: mixed batch yields a distinct outcome per node" \
  || fail "AC5-UI: missing a per-node outcome, got: $out"
echo "$out" | grep -q "^summary: " \
  && pass "AC5-UI: summary line emitted" \
  || fail "AC5-UI: no summary line"

# ---- AC5-EDGE: already-running skips dispatch (no fno agents spawn) ----
reset_mock; set_status ab-dddd4444 claimed; set_claim ab-dddd4444 live
out="$(bash "$DISPATCH" ab-dddd4444 2>&1)"
echo "$out" | grep -q "^already-running ab-dddd4444 " \
  && pass "AC5-EDGE: live-claimed node reported already-running" \
  || fail "AC5-EDGE: expected already-running, got: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "AC5-EDGE: already-running did NOT dispatch a second worker" \
  || fail "AC5-EDGE: a worker was dispatched for a live-claimed node"

# ---- open-PR guard: a node carrying an open PR is parked, NOT re-dispatched ----
# A no-merge worker links pr_number at PR creation, so even after its PID claim
# dies the explicit-id dispatch path must treat the node as in flight:
# ready status + free claim (dead worker) + open PR.
reset_mock; set_status ab-ffff6666 ready; set_claim ab-ffff6666 free; set_pr ab-ffff6666 16
out="$(bash "$DISPATCH" ab-ffff6666 2>&1)"
echo "$out" | grep -q '^already-running ab-ffff6666 reason="node carries open PR #16' \
  && pass "open-PR guard: open-PR node reported already-running" \
  || fail "open-PR guard: expected already-running open-PR line, got: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "open-PR guard: open-PR node did NOT dispatch a duplicate worker" \
  || fail "open-PR guard: a duplicate worker was dispatched for an open-PR node"

# ---- AC5-EDGE: a READY node with a stale claim is recoverable (re-dispatch) ----
reset_mock; set_status ab-dddd4444 ready; set_claim ab-dddd4444 stale
out="$(bash "$DISPATCH" ab-dddd4444 2>&1)"
echo "$out" | grep -q "^launched ab-dddd4444 " \
  && pass "AC5-EDGE: ready node with a stale claim re-dispatched (recovery)" \
  || fail "AC5-EDGE: stale claim not recovered, got: $out"

# ---- AC5-EDGE: empty set -> nothing-up-next, exit 0 ----
reset_mock
out="$(bash "$DISPATCH" 2>&1)"; rc=$?
echo "$out" | grep -q "nothing-up-next" && [[ "$rc" -eq 0 ]] \
  && pass "AC5-EDGE: empty set reports nothing-up-next, exit 0" \
  || fail "AC5-EDGE: empty set wrong (rc=$rc): $out"

# ---- AC5-EDGE: --all-ready over zero ready nodes -> nothing-up-next ----
reset_mock; echo "[]" > "$MOCKSTATE/ready.json"
out="$(bash "$DISPATCH" --all-ready 2>&1)"
echo "$out" | grep -q "nothing-up-next" \
  && pass "AC5-EDGE: --all-ready with zero ready -> nothing-up-next" \
  || fail "AC5-EDGE: --all-ready empty wrong: $out"

# ---- AC5-EDGE: --all-ready batch + --max soft cap ----
reset_mock
printf '[{"id":"ab-aaaa1111"},{"id":"ab-bbbb2222"},{"id":"ab-cccc3333"}]\n' > "$MOCKSTATE/ready.json"
set_status ab-aaaa1111 ready; set_status ab-bbbb2222 ready; set_status ab-cccc3333 ready
out="$(bash "$DISPATCH" --all-ready --max 2 2>&1)"
[[ "$(echo "$out" | grep -c '^launched ')" -eq 2 && "$(echo "$out" | grep -c '^deferred-cap ')" -eq 1 ]] \
  && pass "AC5-EDGE: --all-ready --max 2 launches 2, defers 1" \
  || fail "AC5-EDGE: --max cap wrong: $out"

# ---- gate: blocked/deferred parked ----
reset_mock; set_status ab-ffff6666 blocked; set_status ab-7777aaaa deferred
out="$(bash "$DISPATCH" ab-ffff6666 ab-7777aaaa 2>&1)"
[[ "$(echo "$out" | grep -c '^parked ')" -eq 2 && "$(ask_count)" -eq 0 ]] \
  && pass "gate: blocked + deferred nodes parked, never dispatched" \
  || fail "gate: blocked/deferred not parked: $out"

# ---- gate: EXPLICITLY-NAMED idea/triage nodes dispatch (naming is the vet) ----
reset_mock; set_status ab-8888dddd idea; set_status ab-9999eeee triage
out="$(bash "$DISPATCH" --dry-run ab-8888dddd ab-9999eeee 2>&1)"
[[ "$(echo "$out" | grep -c '^launched ')" -eq 2 ]] \
  && pass "gate: explicit idea + triage nodes dispatch (think->blueprint->do)" \
  || fail "gate: explicit idea/triage not dispatched: $out"

# ---- gate: --all-ready parks an idea node even if the enumeration leaked it ----
reset_mock
printf '[{"id":"ab-aaaa1111"},{"id":"ab-8888dddd"}]\n' > "$MOCKSTATE/ready.json"
set_status ab-aaaa1111 ready; set_status ab-8888dddd idea
out="$(bash "$DISPATCH" --all-ready --dry-run 2>&1)"
echo "$out" | grep -q '^launched ab-aaaa1111 ' && echo "$out" | grep -q '^parked ab-8888dddd ' \
  && pass "gate: --all-ready stays ready-only (leaked idea node parked)" \
  || fail "gate: --all-ready idea guard wrong: $out"

# ---- AC5-FR: dispatch never mutates the caller's target-state.md ----
reset_mock; set_status ab-aaaa1111 ready
STATE="$TMP/.fno"; mkdir -p "$STATE"
printf -- '---\nstatus: IN_PROGRESS\ncurrent_phase: plan\n---\nplanning\n' > "$STATE/target-state.md"
before="$(md5sum "$STATE/target-state.md" 2>/dev/null || md5 -q "$STATE/target-state.md")"
( cd "$TMP" && bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1 )
after="$(md5sum "$STATE/target-state.md" 2>/dev/null || md5 -q "$STATE/target-state.md")"
[[ "$before" == "$after" ]] \
  && pass "AC5-FR: dispatch did NOT touch the planning session's target-state.md" \
  || fail "AC5-FR: target-state.md was mutated by a dispatch"

echo ""
echo "--- review-hardening (sigma-review findings) ---"

# ---- guard fail-closed on a claim-probe error (HIGH: errored probe must NOT
#      collapse to "free" and let a second worker launch over a live claim) ----
reset_mock; set_status ab-aaaa1111 ready; : > "$MOCKSTATE/claim_err"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"claim probe failed" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "guard: claim-probe error fails closed (no dispatch over a possibly-live claim)" \
  || fail "guard: claim-probe error not fail-closed: $out (asks=$(ask_count))"

# ---- guard part 2: a LIVE same-name agent (booting, claim not yet live) is
#      already-running, never re-dispatched (HIGH: the boot-window injection) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; set_agent_live target-ab-aaaa1111 live
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^already-running ab-aaaa1111 reason=\"a live agent target-ab-aaaa1111" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "guard: live same-name agent -> already-running, never re-dispatched" \
  || fail "guard: live same-name agent not caught: $out (asks=$(ask_count))"

# ---- Guard 3 fail-closed: a crashed/garbage `fno agents list` must NOT fall
#      through to a double-launch in the boot window. Parity with spawn.sh
#      (cv-dddd8ae5); sigma silent-failure-hunter on x-73cc. ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; : > "$MOCKSTATE/agents_list_err"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"agents-list probe failed" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "guard: agents-list probe error (rc!=0) fails closed (no boot-window double-launch)" \
  || fail "guard: agents-list probe error not fail-closed: $out (asks=$(ask_count))"
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; : > "$MOCKSTATE/agents_list_garbage"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"agents-list probe failed" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "guard: agents-list garbage (rc=0, not JSON) fails closed" \
  || fail "guard: agents-list garbage not fail-closed: $out (asks=$(ask_count))"

# ---- a DEAD same-name row is removed, then dispatch creates fresh ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; set_agent_live target-ab-aaaa1111 dead
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^launched ab-aaaa1111 " && grep -q "rm target-ab-aaaa1111" "$MOCKSTATE/rm.log" 2>/dev/null \
  && pass "guard: dead same-name row removed, then fresh launch" \
  || fail "guard: dead-row cleanup failed: $out"

# ---- spawn returned 0 but NO short_id receipt => not a provable launch.
#      Must report failed (honest receipt), never "launched session=launched" ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; : > "$MOCKSTATE/ask_noid"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"spawn exit 0 but no short_id receipt" \
  && ! echo "$out" | grep -q "session=launched" \
  && pass "guard: no-short_id receipt reported failed, never launched" \
  || fail "guard: no-short_id mis-reported: $out"

# ---- spawn collision (racing worker took the name) => already-running ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; : > "$MOCKSTATE/ask_collision"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^already-running ab-aaaa1111 reason=\"an agent named target-ab-aaaa1111 already exists (spawn collision)\"" \
  && pass "guard: spawn name collision reported already-running" \
  || fail "guard: spawn collision mis-reported: $out"

# ---- --all-ready enum failure surfaces, never masquerades as nothing-up-next ----
reset_mock; : > "$MOCKSTATE/ready_err"
out="$(bash "$DISPATCH" --all-ready 2>&1)"; rc=$?
echo "$out" | grep -q "^failed --all-ready " && ! echo "$out" | grep -q "nothing-up-next" && [[ "$rc" -eq 1 ]] \
  && pass "guard: --all-ready enum failure surfaced (not 'nothing-up-next')" \
  || fail "guard: --all-ready enum failure mishandled (rc=$rc): $out"

# ---- --dry-run does not dispatch (documented safe-preview path) ----
reset_mock; set_status ab-aaaa1111 ready
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q "session=DRY-RUN" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "coverage: --dry-run previews without dispatching" \
  || fail "coverage: --dry-run wrong: $out (asks=$(ask_count))"

# ---- --flags carrying no-merge does NOT double-inject it ----
reset_mock; set_status ab-aaaa1111 ready
bash "$DISPATCH" --flags "M no-merge" ab-aaaa1111 >/dev/null 2>&1
nm="$(grep -o "no-merge" "$MOCKSTATE/ask.log" 2>/dev/null | wc -l | tr -d ' ')"
[[ "$nm" -eq 1 ]] \
  && pass "coverage: --flags '...no-merge' not double-injected (exactly one)" \
  || fail "coverage: no-merge injected $nm times (expected 1)"

# ---- --allow-merge yields a command with NO no-merge (positive assertion) ----
reset_mock; set_status ab-aaaa1111 ready
bash "$DISPATCH" --allow-merge ab-aaaa1111 >/dev/null 2>&1
if grep -q "/target ab-aaaa1111" "$MOCKSTATE/ask.log" && ! grep -q "no-merge" "$MOCKSTATE/ask.log"; then
  pass "coverage: --allow-merge dispatches '/target <id>' with no no-merge"
else
  fail "coverage: --allow-merge command wrong: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
fi

echo ""
echo "--- external-review hardening (PR #418 gemini + codex findings) ---"

# ---- corrupted node:<id> claim -> fail closed (worker cannot reclaim it) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 corrupted
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"node:ab-aaaa1111 claim is corrupted" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "codex-P2: corrupted claim fails closed (no dispatch)" \
  || fail "codex-P2: corrupted claim not fail-closed: $out"

# ---- claimed-status node whose node:<id> claim is NOT live -> parked, not
#      auto-recovered (legacy graph claim may be stuck) ----
reset_mock; set_status ab-dddd4444 claimed; set_claim ab-dddd4444 stale
out="$(bash "$DISPATCH" ab-dddd4444 2>&1)"
echo "$out" | grep -q "^parked ab-dddd4444 reason=\"claimed but node:ab-dddd4444 claim not live" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "codex-P2: claimed-status + non-live claim parked for manual recovery" \
  || fail "codex-P2: claimed/non-live not parked: $out"

# ---- a node with a recorded (cross-project) cwd dispatches with --cwd ----
reset_mock; set_status ab-aaaa1111 ready; set_cwd ab-aaaa1111 /tmp/example-pipeline
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
grep -q -- "--cwd /tmp/example-pipeline" "$MOCKSTATE/ask.log" \
  && pass "codex-P2: dispatch passes the node's recorded cwd to fno agents spawn" \
  || fail "codex-P2: --cwd not passed: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"

echo ""
echo "--- ab-77b691dc: canonical-default dispatch (--fresh / --here) ---"

# ---- AC1: a node with NO recorded cwd defaults to --fresh (canonical main),
#      so a dispatch from a linked worktree never inherits that worktree ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
# no set_cwd / set_resolved_cwd -> empty node cwd
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
if grep -q -- "--fresh" "$MOCKSTATE/ask.log" && ! grep -q -- "--cwd" "$MOCKSTATE/ask.log"; then
  pass "AC1: no node cwd -> dispatch defaults to --fresh (no --cwd)"
else
  fail "AC1: expected --fresh, no --cwd: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
fi

# ---- AC2: --here opts out -> neither --fresh nor --cwd (inherit caller cwd) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
bash "$DISPATCH" --here ab-aaaa1111 >/dev/null 2>&1
if ! grep -q -- "--fresh" "$MOCKSTATE/ask.log" && ! grep -q -- "--cwd" "$MOCKSTATE/ask.log"; then
  pass "AC2: --here keeps the worker in caller cwd (no --fresh, no --cwd)"
else
  fail "AC2: --here still added a cwd flag: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
fi

# ---- --cwd (node-recorded) wins over the --fresh default (never both) ----
reset_mock; set_status ab-aaaa1111 ready; set_cwd ab-aaaa1111 /tmp/example-pipeline
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
if grep -q -- "--cwd /tmp/example-pipeline" "$MOCKSTATE/ask.log" && ! grep -q -- "--fresh" "$MOCKSTATE/ask.log"; then
  pass "AC6: a recorded node cwd uses --cwd and never adds --fresh"
else
  fail "AC6: node-cwd path added --fresh or dropped --cwd: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
fi

# ---- --dry-run reflects the worktree-ensure default in its preview line ----
# A cwd-less dispatch now ensures a conductor worktree and passes --cwd it
# (falling back to --fresh only when ensure fails), so the preview shows the
# ensure intent rather than a bare --fresh (x-73ca).
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "worktree ensure" \
  && pass "AC1-UI: --dry-run preview shows the worktree-ensure default for a cwd-less node" \
  || fail "AC1-UI: dry-run missing worktree-ensure intent: $out"

# ---- a peer dispatcher holding dispatch:<id> -> already-running, NO ask
#      (boot-window race closed BEFORE the stray-message injection) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free; : > "$MOCKSTATE/reserve_held"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^already-running ab-aaaa1111 reason=\"a peer dispatcher holds dispatch:ab-aaaa1111" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "codex-P1: peer-held reservation -> already-running, no ask (race closed pre-injection)" \
  || fail "codex-P1: reservation race not closed: $out (asks=$(ask_count))"

echo ""
echo "=============================================="
echo "US6 - ready-gated auto-launch (autolaunch-on-ready.sh)"
echo "=============================================="

# get_config stub controls the gate via $GATE; exported so the subprocess sees
# it and skips sourcing config.sh (no yq needed). Mirrors test_dedupe pattern.
get_config() { printf '%s\n' "${GATE:-false}"; }
export -f get_config

mkplan() {  # mkplan <file> <claims-or-empty>
  local f="$1" claims="$2"
  if [[ -n "$claims" ]]; then
    printf -- '---\ntitle: t\nclaims: %s\n---\n# t\n' "$claims" > "$f"
  else
    printf -- '---\ntitle: t\n---\n# t\n' > "$f"
  fi
}

# ---- AC6-EDGE: gate OFF (default) -> silent, no dispatch (Phase-1 unchanged) ----
reset_mock; set_status ab-aaaa1111 ready
mkplan "$TMP/plan-ready.md" ab-aaaa1111
out="$(GATE=false bash "$AUTOLAUNCH" "$TMP/plan-ready.md" 2>&1)"
[[ -z "$out" && "$(ask_count)" -eq 0 ]] \
  && pass "AC6-EDGE: gate OFF is silent and dispatches nothing (default-off)" \
  || fail "AC6-EDGE: gate OFF not silent: [$out] asks=$(ask_count)"

# ---- AC6-HP: gate ON + ready claimed node -> auto-launched ----
reset_mock; set_status ab-aaaa1111 ready
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-ready.md" 2>&1)"
echo "$out" | grep -q "^auto-launched ab-aaaa1111 " && [[ "$(ask_count)" -ge 1 ]] \
  && pass "AC6-HP: gate ON + ready node -> auto-launched + dispatched" \
  || fail "AC6-HP: expected auto-launched, got: $out (asks=$(ask_count))"

# ---- AC6-ERR: gate ON + blocked/deferred node -> parked, NOT launched ----
reset_mock; set_status ab-ffff6666 blocked
mkplan "$TMP/plan-blocked.md" ab-ffff6666
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-blocked.md" 2>&1)"
echo "$out" | grep -q "^parked ab-ffff6666 " && [[ "$(ask_count)" -eq 0 ]] \
  && pass "AC6-ERR: gate ON + blocked node parked, never launched" \
  || fail "AC6-ERR: blocked node not parked: $out (asks=$(ask_count))"

# ---- AC6-FR: gate ON + dispatch fails -> surfaced, plan intact, node stays ready ----
reset_mock; set_status ab-aaaa1111 ready; : > "$MOCKSTATE/ask.fail"
planbefore="$(md5sum "$TMP/plan-ready.md" 2>/dev/null || md5 -q "$TMP/plan-ready.md")"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-ready.md" 2>&1)"
planafter="$(md5sum "$TMP/plan-ready.md" 2>/dev/null || md5 -q "$TMP/plan-ready.md")"
echo "$out" | grep -q "^autolaunch-failed ab-aaaa1111 " \
  && pass "AC6-FR: auto-launch dispatch failure surfaced" \
  || fail "AC6-FR: failure not surfaced: $out"
[[ "$planbefore" == "$planafter" && "$(cat "$MOCKSTATE/status_ab-aaaa1111")" == "ready" ]] \
  && pass "AC6-FR: plan intact + node stays ready after a failed auto-launch" \
  || fail "AC6-FR: plan or node mutated on failure"

# ---- AC6-UI: gate ON + plan with no claims node -> no decision line, graceful ----
reset_mock
mkplan "$TMP/plan-noclaim.md" ""
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-noclaim.md" 2>/dev/null)"  # stderr note only
[[ -z "$out" && "$(ask_count)" -eq 0 ]] \
  && pass "AC6-UI: gate ON + no claims node -> no dispatch, no stdout decision" \
  || fail "AC6-UI: no-claims path wrong: [$out] asks=$(ask_count)"

# ---- gate ON + transient backlog read failure -> parked "status read failed",
#      NOT silently mislabeled as a not-ready status, and never launched (MED) ----
reset_mock; : > "$MOCKSTATE/get_err"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-ready.md" 2>&1)"
echo "$out" | grep -q "^parked ab-aaaa1111 reason=\"backlog status read failed" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "AC6: transient backlog read failure parked honestly (not mislabeled), no launch" \
  || fail "AC6: backlog read failure mishandled: $out"

echo ""
echo "--- US1 - _resolved_cwd authority (node-cwd-authority, ab-c0f92987) ---"

# ---- AC1-HP: _resolved_cwd present -> dispatch uses it over raw cwd ----
reset_mock; set_status ab-aaaa1111 ready
set_resolved_cwd ab-aaaa1111 /resolved/root
set_cwd ab-aaaa1111 /recorded/other
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--cwd /resolved/root" \
  && pass "AC1-HP: dry-run command uses _resolved_cwd, not raw cwd" \
  || fail "AC1-HP: _resolved_cwd not used in dry-run command: $out"
echo "$out" | grep -q "cwd=/resolved/root" \
  && pass "AC1-HP: dry-run line contains cwd= token with resolved value" \
  || fail "AC1-HP: dry-run line missing cwd= token: $out"

# ---- AC1-EDGE: no _resolved_cwd (stale fno) -> falls back to raw cwd ----
reset_mock; set_status ab-aaaa1111 ready
set_cwd ab-aaaa1111 /recorded/other
# no set_resolved_cwd: mock emits only cwd field
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--cwd /recorded/other" \
  && pass "AC1-EDGE: stale-abi fallback (no _resolved_cwd) uses raw cwd" \
  || fail "AC1-EDGE: stale-abi fallback wrong: $out"

# ---- AC1-UI: launched and dry-run lines contain a cwd= token ----
reset_mock; set_status ab-aaaa1111 ready
set_resolved_cwd ab-aaaa1111 /resolved/root
set_cwd ab-aaaa1111 /recorded/other
out_dry="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out_dry" | grep -qE "cwd=" \
  && pass "AC1-UI: dry-run line contains cwd= token" \
  || fail "AC1-UI: dry-run line missing cwd= token: $out_dry"
# Real launch path
out_real="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out_real" | grep -q "^launched ab-aaaa1111 " && echo "$out_real" | grep -qE "cwd=" \
  && pass "AC1-UI: launched line contains cwd= token" \
  || fail "AC1-UI: launched line missing cwd= token: $out_real"

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
