#!/usr/bin/env bash
# test-bg-dispatch.sh - regression tests for `/target bg` (dispatch-node.sh).
#
# x-3873 change 4: the launch is ONE command per node - the door,
# `fno agents spawn --node <id> --substrate thread` - and only what the human
# typed rides beside --node. The script carries no resolver, no posture
# plumbing, and no worktree ensure (the door owns all three).
#
# Hermetic: a mock `fno` on PATH stands in for backlog/agents, so NO real
# worker is launched and NO real backlog/claim state is touched.
#
# Exit codes: 0 pass | 1 assertion failed | 77 skipped (missing deps).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DISPATCH="$REPO_ROOT/skills/target/scripts/dispatch-node.sh"

PASS=0; FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1"; FAIL=$((FAIL + 1)); }
skip() { printf 'SKIP: %s\n' "$*" >&2; exit 77; }

command -v jq  >/dev/null 2>&1 || skip "jq required"
[[ -f "$DISPATCH" ]]   || skip "dispatch-node.sh missing"

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
      plan_fragment=""
      [[ -f "$S/plan_$id" ]] && plan_fragment=",\"plan_path\":\"$(cat "$S/plan_$id")\""
      [[ -f "$S/slug_$id" ]] && printf '{"id":"%s","status":"%s","slug":"%s","cwd":"/repo"%s%s}\n' \
        "$id" "$(cat "$S/status_$id")" "$(cat "$S/slug_$id")" "$pr_fragment" "$plan_fragment"
      [[ -f "$S/slug_$id" ]] || printf '{"id":"%s","status":"%s","cwd":"/repo"%s%s}\n' \
        "$id" "$(cat "$S/status_$id")" "$pr_fragment" "$plan_fragment"
    else
      exit 1   # unknown node -> nonzero, no output (mirrors not-found)
    fi ;;
  "backlog ready")
    [[ -f "$S/ready_err" ]] && exit 1          # simulate an enumeration failure
    cat "$S/ready.json" 2>/dev/null || echo "[]" ;;
  "agents spawn-guard")
    id="${3:-}"
    [[ -f "$S/claim_err" ]] && { printf '{"verdict":"error","detail":"claim probe failed (mock); not dispatching to avoid a double-launch"}\n'; exit 3; }
    st="$(cat "$S/claim_$id" 2>/dev/null || echo free)"
    case "$st" in
      live)      printf '{"verdict":"already-running","reason":"live-claim","holder":"target-session:holder-%s"}\n' "$id"; exit 0 ;;
      corrupted) printf '{"verdict":"corrupted","detail":"node:%s claim is corrupted; force-release or repair before dispatching"}\n' "$id"; exit 0 ;;
    esac
    printf '{"verdict":"dispatchable"}\n'; exit 0 ;;
  "agents list")
    [[ -f "$S/agents_list_err" ]] && exit 1   # simulate a crashed probe (daemon down)
    [[ -f "$S/agents_list_garbage" ]] && { echo "<<not json>>"; exit 0; }
    cat "$S/agents_list.json" 2>/dev/null || echo '{"agents":[]}' ;;
  "agents rm")
    printf 'rm %s\n' "${3:-}" >> "$S/rm.log"; echo "removed ${3:-}" ;;
  "agents name")
    id="${3:-}"
    # x-57fe: the dispatch carries --verb t (the bridge refuses a verb-less
    # mint), and the minted name is the canonical hex shape.
    printf 't-%s\n' "${id##*-}" ;;
  "agents spawn")
    printf '%s\n' "$*" >> "$S/ask.log"
    if [[ -f "$S/spawn_refuse" ]]; then
      printf 'node dispatch refused: node=%s verdict=already-running reason=live-claim; no worker launched\n' "$(cat "$S/spawn_refuse")" >&2
      exit 2
    fi
    [[ -f "$S/ask.fail" ]] && { echo "daemon down: connection refused" >&2; exit 1; }
    # ask_collision models a racing worker: spawn refuses an existing name with
    # exit 2 (Group 1: spawn never resumes; the old ask resume path is gone).
    if [[ -f "$S/ask_collision" ]]; then
      echo "agent 'target-ab-aaaa1111' already exists; use 'fno agents rm target-ab-aaaa1111' first or pick another name" >&2; exit 2
    fi
    # ask_noid models a thin receipt: exit 0 but no session carrier at all.
    if [[ -f "$S/ask_noid" ]]; then echo '{"name": "target-x", "status": "live"}'
    else echo '{"name": "target-ab-aaaa1111", "short_id": "deadbeef01", "harness": "claude", "status": "live"}'; fi ;;
  *) exit 0 ;;
esac
MOCK
chmod +x "$MOCKBIN/fno"
export MOCK_STATE="$MOCKSTATE"
export PATH="$MOCKBIN:$PATH"

set_status() { echo "$2" > "$MOCKSTATE/status_$1"; }
set_plan()   { echo "$2" > "$MOCKSTATE/plan_$1"; }
set_slug()   { echo "$2" > "$MOCKSTATE/slug_$1"; }
set_claim()  { echo "$2" > "$MOCKSTATE/claim_$1"; }
set_pr() { echo "$2" > "$MOCKSTATE/pr_$1"; }   # node carries an open (unmerged) PR
reset_mock() { rm -f "$MOCKSTATE"/status_* "$MOCKSTATE"/claim_* "$MOCKSTATE"/cwd_* "$MOCKSTATE"/pr_* "$MOCKSTATE"/ask.log "$MOCKSTATE"/ask.fail "$MOCKSTATE"/ask_collision "$MOCKSTATE"/ask_noid "$MOCKSTATE"/spawn_refuse "$MOCKSTATE"/ready.json "$MOCKSTATE"/claim_err "$MOCKSTATE"/ready_err "$MOCKSTATE"/get_err "$MOCKSTATE"/agents_list.json "$MOCKSTATE"/agents_list_err "$MOCKSTATE"/agents_list_garbage "$MOCKSTATE"/rm.log "$MOCKSTATE"/slug_* "$MOCKSTATE"/plan_* 2>/dev/null || true; }
spawn_argv() { cat "$MOCKSTATE/ask.log" 2>/dev/null | tail -n "${1:-1}"; }

set_status ab-aaaa1111 ready
set_status ab-bbbb2222 ready
set_slug ab-aaaa1111 the-slug

echo "=============================================="
echo "US5 - /target bg dispatch (dispatch-node.sh)"
echo "=============================================="

# ---- AC5-HP: the launch is the door, node + substrate + name only ----------
reset_mock
set_status ab-aaaa1111 ready
set_slug ab-aaaa1111 the-slug
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
rc=$?
[[ "$rc" -eq 0 ]] && pass "single ready dispatch exits 0" || fail "single ready dispatch exits 0 (rc=$rc): $out"
argv="$(spawn_argv 1)"
[[ "$argv" == "agents spawn --node ab-aaaa1111 --substrate thread --name t-aaaa1111" ]] \
  && pass "spawn argv is exactly node+substrate+name" || fail "spawn argv is exactly node+substrate+name (got: $argv)"
grep -q -- "--harness" <<<"$argv" && fail "no --harness rides the spawn" || pass "no --harness rides the spawn"
grep -q -- "--model" <<<"$argv" && fail "no --model rides the spawn" || pass "no --model rides the spawn"
grep -q -- "--route" <<<"$argv" && fail "no --route rides an untyped dispatch" || pass "no --route rides an untyped dispatch"
grep -q "launched ab-aaaa1111 name=t-aaaa1111" <<<"$out" \
  && pass "launched line names the node and agent" || fail "launched line names the node and agent: $out"
grep -q "summary: launched=1 " <<<"$out" \
  && pass "summary counts one launch" || fail "summary counts one launch: $out"
# The worker name must not claim a session id it cannot know (the door
# ensures the worktree, so the receipt carries no landing directory either).
grep -q "cwd=" <<<"$out" && fail "receipt claims no landing cwd" || pass "receipt claims no landing cwd"

# ---- AC5-HP: the typed flags ride, and only those --------------------------
reset_mock
set_status ab-aaaa1111 ready
set_slug ab-aaaa1111 the-slug
out="$(bash "$DISPATCH" --here --route zai/glm-5.3 --permission-mode acceptEdits ab-aaaa1111 2>&1)"
argv="$(spawn_argv 1)"
[[ "$argv" == "agents spawn --node ab-aaaa1111 --substrate thread --name t-aaaa1111 --here --route zai/glm-5.3 --permission-mode acceptEdits" ]] \
  && pass "typed --here/--route/--permission-mode ride the spawn" || fail "typed flags ride the spawn (got: $argv)"

# ---- AC5-EDGE: the retired per-run flags are unknown flags -----------------
reset_mock
set_status ab-aaaa1111 ready
for flag in "--flags L" "--allow-merge" "--no-merge" "--source ab"; do
  out="$(bash "$DISPATCH" $flag ab-aaaa1111 2>&1)"; rc=$?
  flagname="${flag%% *}"
  if [[ "$rc" -eq 2 ]] && grep -q "failed: $flagname reason=\"unknown flag\"" <<<"$out"; then
    pass "retired flag $flagname exits 2 as unknown"
  else
    fail "retired flag $flagname exits 2 as unknown (rc=$rc): $out"
  fi
  [[ "$(spawn_argv 1 2>/dev/null | wc -l | tr -d ' ')" == "0" ]] \
    && pass "retired flag $flagname launches nothing" || fail "retired flag $flagname launches nothing"
done
# The census guards the inverse: the script must not teach the retired reads.
grep -q "dispatch resolve" "$DISPATCH" && fail "no dispatch resolve string in the script" || pass "no dispatch resolve string in the script"
grep -q "dispatch family" "$DISPATCH" && fail "no dispatch family string in the script" || pass "no dispatch family string in the script"

# ---- AC5-ERR: the door's family-2 refusal maps to already-running ----------
reset_mock
set_status ab-aaaa1111 ready
echo "ab-aaaa1111" > "$MOCKSTATE/spawn_refuse"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"; rc=$?
grep -q "already-running ab-aaaa1111" <<<"$out" \
  && pass "door refusal maps to already-running" || fail "door refusal maps to already-running: $out"
grep -q "summary: launched=0 parked=0 already=1 skipped=0 done=0 failed=0" <<<"$out" \
  && pass "refused dispatch counts already=1 failed=0" || fail "refused dispatch counts already=1 failed=0: $out"

# ---- Status gating ----------------------------------------------------------
reset_mock
set_status ab-aaaa1111 done
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q "skipped-done ab-aaaa1111 reason=\"node already done\"" <<<"$out" \
  && pass "done node is skipped-done" || fail "done node is skipped-done: $out"
grep -q "summary: launched=0 parked=0 already=0 skipped=0 done=1 failed=0" <<<"$out" \
  && pass "done node summary counts done=1" || fail "done node summary counts done=1: $out"
reset_mock
set_status ab-aaaa1111 blocked
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q "parked ab-aaaa1111 reason=\"blocked (not up-next)\"" <<<"$out" \
  && pass "blocked node is parked" || fail "blocked node is parked: $out"

# --all-ready: a linked idea stub parks; a plan-less idea cold-dispatches.
reset_mock
echo '[{"id":"ab-aaaa1111"},{"id":"ab-cccc3333"}]' > "$MOCKSTATE/ready.json"
set_status ab-aaaa1111 idea
set_status ab-cccc3333 idea
set_plan ab-aaaa1111 /plans/stub.md
out="$(bash "$DISPATCH" --all-ready 2>&1)"
grep -q "parked ab-aaaa1111 reason=\"idea (not up-next)\"" <<<"$out" \
  && pass "linked idea stub parks under --all-ready" || fail "linked idea stub parks under --all-ready: $out"
[[ "$(spawn_argv | grep -c -- '--node ab-cccc3333')" == "1" ]] \
  && pass "plan-less idea cold-dispatches under --all-ready" || fail "plan-less idea cold-dispatches under --all-ready: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
reset_mock

# ---- The dry run previews the real argv and launches nothing ----------------
reset_mock
set_status ab-aaaa1111 ready
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
grep -q "would run: fno agents spawn --node ab-aaaa1111 --substrate thread --name t-aaaa1111" <<<"$out" \
  && pass "dry run previews the door argv" || fail "dry run previews the door argv: $out"
[[ -f "$MOCKSTATE/ask.log" ]] && fail "dry run launches nothing" || pass "dry run launches nothing"

# ---- Open-PR guard ----------------------------------------------------------
reset_mock
set_status ab-aaaa1111 ready
set_pr ab-aaaa1111 1906
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q "already-running ab-aaaa1111 reason=\"node carries open PR #1906; not re-dispatching\"" <<<"$out" \
  && pass "open-PR node reports already-running" || fail "open-PR node reports already-running: $out"

# ---- Unknown node / enumeration failure surface loudly ----------------------
reset_mock
out="$(bash "$DISPATCH" ab-nope9999 2>&1)"; rc=$?
[[ "$rc" -eq 1 ]] && grep -q "failed ab-nope9999 reason=\"no such node (or backlog read failed)\"" <<<"$out" \
  && pass "unknown node fails loudly" || fail "unknown node fails loudly (rc=$rc): $out"
reset_mock
set_status ab-aaaa1111 ready
touch "$MOCKSTATE/ready_err"
out="$(bash "$DISPATCH" --all-ready 2>&1)"; rc=$?
[[ "$rc" -eq 1 ]] && grep -q "not treating as an empty backlog" <<<"$out" \
  && pass "ready enumeration failure is not an empty backlog" || fail "ready enumeration failure is not an empty backlog (rc=$rc): $out"
reset_mock

# ---- --max soft cap ---------------------------------------------------------
reset_mock
set_status ab-aaaa1111 ready
set_status ab-bbbb2222 ready
out="$(bash "$DISPATCH" --max 1 ab-aaaa1111 ab-bbbb2222 2>&1)"
grep -q "deferred-cap ab-bbbb2222 reason=\"--max 1 reached\"" <<<"$out" \
  && pass "--max caps the remainder" || fail "--max caps the remainder: $out"
grep -q "summary: launched=1 " <<<"$out" \
  && pass "--max still launches the first" || fail "--max still launches the first: $out"

# ---- No empty board report without the enumeration --------------------------
reset_mock
out="$(bash "$DISPATCH" --all-ready 2>&1)"; rc=$?
[[ "$rc" -eq 0 ]] && grep -q "nothing-up-next" <<<"$out" \
  && pass "empty board reports nothing-up-next" || fail "empty board reports nothing-up-next (rc=$rc): $out"

# ---- Boot-window name collision ---------------------------------------------
reset_mock
set_status ab-aaaa1111 ready
touch "$MOCKSTATE/ask_collision"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q "already-running ab-aaaa1111 reason=\"an agent named" <<<"$out" \
  && pass "spawn name collision reads already-running" || fail "spawn name collision reads already-running: $out"

echo "=============================================="
echo "Results: PASS=$PASS FAIL=$FAIL"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
