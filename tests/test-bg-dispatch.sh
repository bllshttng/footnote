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
if [[ "$sub $verb" == "agents dispatch" || "$sub $verb" == "agents claim" ]]; then
  shift
  sub="${1:-}"; verb="${2:-}"
fi
case "$sub $verb" in
  "backlog get")
    id="${3:-}"
    [[ -f "$S/get_err" ]] && exit 1            # simulate a transient read failure
    if [[ -f "$S/status_$id" ]]; then
      # Emit pr_number (open-PR guard sim) when pr_<id> is set; otherwise
      # omit the field so the guard's `.pr_number // empty` stays empty.
      pr_fragment=""
      [[ -f "$S/pr_$id" ]] && pr_fragment=",\"pr_number\":\"$(cat "$S/pr_$id")\",\"completed_at\":null"
      # x-a5e4: a node with dispatch_verb takes the verb/brief resolver path.
      verb_fragment=""
      [[ -f "$S/verb_$id" ]] && verb_fragment=",\"dispatch_verb\":\"$(cat "$S/verb_$id")\""
      # x-3218: a title-derived slug feeds the agent-name budget. Omitted unless
      # set, so every pre-existing scenario keeps its slugless <verb>-<id> name.
      [[ -f "$S/slug_$id" ]] && verb_fragment="$verb_fragment,\"slug\":\"$(cat "$S/slug_$id")\""
      # x-e24a: plan_path distinguishes a plan-less idea (Rung.NONE) from a
      # linked idea stub (Rung.IDEA) for the --all-ready cold-dispatch gate.
      plan_fragment=""
      [[ -f "$S/plan_$id" ]] && plan_fragment=",\"plan_path\":\"$(cat "$S/plan_$id")\""
      # Emit _resolved_cwd when set, otherwise omit the field (stale-fno sim).
      if [[ -f "$S/resolved_cwd_$id" ]]; then
        printf '{"id":"%s","status":"%s","_resolved_cwd":"%s","cwd":"%s"%s%s%s}\n' \
          "$id" "$(cat "$S/status_$id")" \
          "$(cat "$S/resolved_cwd_$id")" \
          "$(cat "$S/cwd_$id" 2>/dev/null || echo "")" "$pr_fragment" "$verb_fragment" "$plan_fragment"
      else
        printf '{"id":"%s","status":"%s","cwd":"%s"%s%s%s}\n' \
          "$id" "$(cat "$S/status_$id")" "$(cat "$S/cwd_$id" 2>/dev/null || echo "")" "$pr_fragment" "$verb_fragment" "$plan_fragment"
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
    [[ -f "$S/claim_garbage" ]] && { echo "note: claim service moved; see docs"; exit 0; }  # banner text, rc 0
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
  "agents workspace"|"workspace worktree")
    # worktree ensure under test: the isolation the launcher routes BOTH
    # node-cwd and no-cwd dispatches through. repo_ensure.log records the
    # --repo root so a test asserts WHICH root fed the isolation, not just
    # that some worktree came back. ensure_fail -> refusal (nonzero, no
    # output); ensure_policy_never -> policy=never, returns the repo root.
    repo=""; name=""; _prev=""
    for a in "$@"; do
      [[ "$_prev" == "--repo" ]] && repo="$a"
      [[ "$_prev" == "--name" ]] && name="$a"
      _prev="$a"
    done
    printf '%s\n' "$repo" >> "$S/repo_ensure.log"
    [[ -f "$S/ensure_fail" ]] && exit 1
    [[ -f "$S/ensure_policy_never" ]] && { printf '%s\n' "$repo"; exit 0; }
    printf '%s/worktrees/%s\n' "$repo" "$name" ;;
  "agents spawn")
    spawn_node=""; _prev=""
    for a in "$@"; do
      [[ "$_prev" == "--node" ]] && spawn_node="$a"
      _prev="$a"
    done
    spawn_claim="$(cat "$S/claim_$spawn_node" 2>/dev/null || echo free)"
    if [[ -n "$spawn_node" && "$spawn_claim" == "corrupted" ]]; then
      echo "node dispatch refused: node=$spawn_node verdict=corrupted reason=claim-corrupted; no worker launched" >&2
      exit 2
    fi
    if [[ -n "$spawn_node" && -f "$S/claim_err" ]]; then
      echo "node dispatch refused: node=$spawn_node verdict=error reason=claim-probe-error; no worker launched" >&2
      exit 3
    fi
    if [[ -n "$spawn_node" && -f "$S/reserve_held" ]]; then
      echo "node dispatch refused: node=$spawn_node verdict=already-running reason=reservation-held; no worker launched" >&2
      exit 2
    fi
    printf '%s\n' "$*" >> "$S/ask.log"
    [[ -f "$S/ask.fail" ]] && { echo "daemon down: connection refused" >&2; exit 1; }
    # ask_collision models a racing worker: spawn refuses an existing name with
    # exit 2 (Group 1: spawn never resumes; the old ask resume path is gone).
    if [[ -f "$S/ask_collision" ]]; then
      echo "agent 'tgt-aaaa1111' already exists; use 'fno agents rm tgt-aaaa1111' first or pick another name" >&2; exit 2
    fi
    # ask_noid models a broken receipt: exit 0 but no parseable short_id.
    if [[ -f "$S/ask_noid" ]]; then echo "Sure, starting on that now."; else echo '{"name": "tgt-aaaa1111", "short_id": "deadbeef01", "harness": "claude", "status": "live"}'; fi ;;
  "dispatch resolve")
    # x-567d: provider/substrate resolver. Default resolves claude/bg (every
    # existing scenario expects the claude bg lane). A resolve_* state file
    # overrides: resolve_fail -> exit 2 (no autonomous substrate); resolve_pair
    # holds a "harness/substrate" pair (headless fallback scenarios).
    printf '%s\n' "$*" >> "$S/resolve.log"
    if [[ -f "$S/resolve_fail" ]]; then echo "dispatch resolve: unknown harness (mock)" >&2; exit 2; fi
    pair="claude/bg"
    [[ -f "$S/resolve_pair" ]] && pair="$(cat "$S/resolve_pair")"
    h="${pair%%/*}"
    # Parse the verb/brief/posture resolver path (--node <id> [--verb <v>]
    # [--merge-posture <p>]).
    r_node=""; r_verb=""; r_posture=""; _prev=""
    for a in "$@"; do
      case "$_prev" in --node|--id) r_node="$a" ;; --verb) r_verb="$a" ;; --merge-posture) r_posture="$a" ;; esac
      _prev="$a"
    done
    # x-4391/x-8151: the RESOLVER owns the merge posture now; the launcher only
    # threads it. from-config folds the same grant source the config-get case
    # models, and every error shape degrades to no-merge.
    posture="$r_posture"
    if [[ "$posture" == "from-config" || -z "$posture" ]]; then
      grant="none"
      [[ -f "$PWD/.fno/auto_merge" ]] && grant="$(cat "$PWD/.fno/auto_merge")"
      [[ "$grant" == "none" ]] && grant="$(cat "$S/cfg_auto_merge" 2>/dev/null || echo none)"
      [[ -f "$S/cfg_auto_merge_err" ]] && grant="unreadable"
      [[ "$grant" == "dispatch" ]] && posture="allow" || posture="no-merge"
    fi
    if [[ -n "$r_verb" ]]; then
      # x-a5e4 verb path: the resolver NORMALIZES the verb per-harness. The
      # posture (not the launcher) adds the carrier below.
      bare="${r_verb#/}"
      case "$h" in
        claude|agy) cmd="/$bare {id}" ;;
        codex)      cmd="\$fno:$bare {id}" ;;
        opencode)   cmd="/fno:$bare {id}" ;;
        *)          cmd='REFUSED: harness deprecated (successor: agy), no dispatch lane' ;;
      esac
    else
      # builtin (no verb): the plain template; the posture below decides the
      # carrier, mirroring resolve_dispatch's allow-override contract.
      case "$h" in
        claude|agy) cmd='/target {id}' ;;
        codex)      cmd='$fno:target {id}' ;;
        opencode)   cmd='/fno:target {id}' ;;
        *)          cmd='REFUSED: harness deprecated (successor: agy), no dispatch lane' ;;
      esac
    fi
    # x-8151: no-merge injects on every rung, once, into family commands only.
    # allow never edits a command (a refusal it carries wins).
    if [[ "$posture" == "no-merge" && "$cmd" != *REFUSED* ]]; then
      case "${cmd%%[[:space:]]*}" in
        /target|/fno:target|\$fno:target)
          if [[ "$cmd" != *"--no-merge"* ]]; then
            if [[ "$cmd" == *" "* ]]; then cmd="${cmd/ / --no-merge }"; else cmd="$cmd --no-merge"; fi
          fi ;;
      esac
    fi
    # The real resolver substitutes {id} when --node is given (per-node path).
    [[ -n "$r_node" ]] && cmd="${cmd//\{id\}/$r_node}"
    printf '{"harness":"%s","substrate":"%s","command":"%s"}\n' "$h" "${pair##*/}" "$cmd" ;;
  "config get")
    # x-4391/x-4be1: only auto_merge.grant is modeled; every other key (e.g.
    # agents.spawn_permission_mode) falls through to empty, matching prod's
    # "unset => empty" so the permission-mode read stays a no-op under the mock.
    key="${3:-}"
    if [[ "$key" == "auto_merge.grant" ]]; then
      [[ -f "$S/cfg_auto_merge_err" ]] && { echo "unknown config key 'auto_merge.grant'" >&2; exit 1; }
      # x-4391 per-node (codex P2): `fno config get` reads the CURRENT cwd. The
      # launcher cd's into each node's project cwd before the read, so a node
      # carrying its own .fno/auto_merge (written by the test) simulates a
      # cross-project posture; the global cfg_auto_merge state is the fallback.
      [[ -f "$PWD/.fno/auto_merge" ]] && { cat "$PWD/.fno/auto_merge"; exit 0; }
      cat "$S/cfg_auto_merge" 2>/dev/null || echo none   # prints the grant literal
      exit 0
    fi
    : ;;
  "agents name")
    # x-3218: the canonical agent-name bridge. Delegates to the REAL primitive
    # (via NAME_BRIDGE) so the dispatcher's naming is proved against the shipped
    # policy, not a mock's guess. Unset -> exit 0 with no output, which is the
    # unreachable-fno signal the launcher degrades on.
    [[ -n "${NAME_BRIDGE:-}" && -x "${NAME_BRIDGE:-}" ]] || exit 0
    shift 2; exec "$NAME_BRIDGE" "$@" ;;
  "event emit") : ;;  # x-567d: fallback/fail telemetry; noop under the mock
  *) exit 0 ;;
esac
MOCK
chmod +x "$MOCKBIN/fno"
export MOCK_STATE="$MOCKSTATE"
export PATH="$MOCKBIN:$PATH"

# x-3218: point the mock's `agents name` case at the REAL canonical owner for the
# WHOLE suite. Production always has a working bridge, so leaving it unset would
# run every scenario through the degraded fallback and certify a branch that only
# fires on a broken or stale fno. Cases wanting the degraded, stale, or noisy
# behaviour override NAME_BRIDGE per call.
VENV_PY="$REPO_ROOT/cli/.venv/bin/python"
if [[ ! -x "$VENV_PY" ]]; then
  CANON="$(cd "$(git -C "$REPO_ROOT" rev-parse --git-common-dir 2>/dev/null)/.." 2>/dev/null && pwd)"
  [[ -n "$CANON" ]] && VENV_PY="$CANON/cli/.venv/bin/python"
fi
NAME_BRIDGE="$TMP/name-bridge"
{
  echo '#!/usr/bin/env bash'
  echo "exec \"$VENV_PY\" -c 'import sys; sys.path.insert(0, \"$REPO_ROOT/cli/src\"); from fno.cli import app; app()' agents name \"\$@\""
} > "$NAME_BRIDGE"
chmod +x "$NAME_BRIDGE"
"$NAME_BRIDGE" target x-1 >/dev/null 2>&1 \
  || { echo "FATAL: canonical naming bridge unreachable at $VENV_PY" >&2; exit 1; }
export NAME_BRIDGE

set_status() { echo "$2" > "$MOCKSTATE/status_$1"; }
set_plan()   { echo "$2" > "$MOCKSTATE/plan_$1"; }   # x-e24a: plan_path -> linked stub (Rung.IDEA)
set_claim()  { echo "$2" > "$MOCKSTATE/claim_$1"; }
set_agent_live() { printf '{"agents":[{"name":"%s","status":"%s"}]}\n' "$1" "$2" > "$MOCKSTATE/agents_list.json"; }
set_cwd() { echo "$2" > "$MOCKSTATE/cwd_$1"; }
set_resolved_cwd() { echo "$2" > "$MOCKSTATE/resolved_cwd_$1"; }
set_pr() { echo "$2" > "$MOCKSTATE/pr_$1"; }   # node carries an open (unmerged) PR
reset_mock() { rm -f "$MOCKSTATE"/status_* "$MOCKSTATE"/claim_* "$MOCKSTATE"/cwd_* "$MOCKSTATE"/resolved_cwd_* "$MOCKSTATE"/pr_* "$MOCKSTATE"/ask.log "$MOCKSTATE"/ask.fail "$MOCKSTATE"/ask_collision "$MOCKSTATE"/ready.json "$MOCKSTATE"/claim_err "$MOCKSTATE"/claim_garbage "$MOCKSTATE"/ready_err "$MOCKSTATE"/get_err "$MOCKSTATE"/ask_noid "$MOCKSTATE"/reserve_held "$MOCKSTATE"/agents_list.json "$MOCKSTATE"/agents_list_err "$MOCKSTATE"/agents_list_garbage "$MOCKSTATE"/rm.log "$MOCKSTATE"/resolve_fail "$MOCKSTATE"/resolve_pair "$MOCKSTATE"/verb_* "$MOCKSTATE"/slug_* "$MOCKSTATE"/cfg_auto_merge "$MOCKSTATE"/cfg_auto_merge_err "$MOCKSTATE"/repo_ensure.log "$MOCKSTATE"/ensure_fail "$MOCKSTATE"/ensure_policy_never 2>/dev/null || true; }
ask_count()  { [[ -f "$MOCKSTATE/ask.log" ]] && wc -l < "$MOCKSTATE/ask.log" | tr -d ' ' || echo 0; }

echo "=============================================="
echo "US5 - targeted bg-dispatch (dispatch-node.sh)"
echo "=============================================="

# ---- AC5-HP: single ready node launches via fno agents spawn, no --bare/-p ----
# (-p is fno's headless short now, but a bg dispatch must never carry it either.)
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
echo "$out" | grep -q "^launched ab-aaaa1111 name=target-ab-aaaa1111 session=deadbeef01" \
  && pass "AC5-HP: ready node launched with stable target-<full-id> name + session" \
  || fail "AC5-HP: expected launched line, got: $out"
grep -q -- "--harness claude" "$MOCKSTATE/ask.log" \
  && pass "AC5-HP: dispatch used fno agents spawn --harness claude" \
  || fail "AC5-HP: ask.log missing --harness claude"
if grep -Eq -- "(^| )(--bare|-p)( |$)" "$MOCKSTATE/ask.log"; then
  fail "AC5-HP: FORBIDDEN --bare/-p reached the dispatch (must be subscription lane)"
else
  pass "AC5-HP: never --bare/-p (subscription lane only)"
fi
grep -q "/target --no-merge ab-aaaa1111" "$MOCKSTATE/ask.log" \
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

# ============================================================
# x-4391/x-4be1: config-driven merge posture (config.auto_merge.grant)
# ============================================================

# AC2-HP: grant=dispatch -> claude builtin path omits no-merge (skip-inject).
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo dispatch > "$MOCKSTATE/cfg_auto_merge"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
if grep -q '/target ab-aaaa1111' "$MOCKSTATE/ask.log" && ! grep -q 'no-merge' "$MOCKSTATE/ask.log"; then
  pass "x-4391 AC2-HP: config auto_merge=true -> claude /target without no-merge"
else
  fail "x-4391 claude allow posture: $(cat "$MOCKSTATE/ask.log")"
fi

# AC2-HP (the strip): a codex builtin command bakes no-merge (_AUTONOMOUS_COMMAND);
# under allow posture the launcher STRIPS it, not merely skip-injects.
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo dispatch > "$MOCKSTATE/cfg_auto_merge"; echo codex/headless > "$MOCKSTATE/resolve_pair"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
if grep -q 'fno:target ab-aaaa1111' "$MOCKSTATE/ask.log" && ! grep -q 'no-merge' "$MOCKSTATE/ask.log"; then
  pass "x-4391 AC2-HP: auto_merge=true strips baked no-merge from codex \$fno:target"
else
  fail "x-4391 codex strip: $(cat "$MOCKSTATE/ask.log")"
fi

# AC1-HP: default posture (no config key) keeps the baked no-merge on the codex builtin.
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo codex/headless > "$MOCKSTATE/resolve_pair"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q 'fno:target --no-merge ab-aaaa1111' "$MOCKSTATE/ask.log" \
  && pass "x-4391 AC1-HP: default posture keeps no-merge on codex builtin" \
  || fail "x-4391 codex default: $(cat "$MOCKSTATE/ask.log")"

# AC3-HP: explicit --no-merge beats config auto_merge=true.
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo dispatch > "$MOCKSTATE/cfg_auto_merge"
out="$(bash "$DISPATCH" --no-merge ab-aaaa1111 2>&1)"
grep -q '/target --no-merge ab-aaaa1111' "$MOCKSTATE/ask.log" \
  && pass "x-4391 AC3-HP: --no-merge beats config auto_merge=true" \
  || fail "x-4391 --no-merge override: $(cat "$MOCKSTATE/ask.log")"

# AC1-ERR: a failed config read (stale fno rejecting the key) degrades to no-merge.
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
touch "$MOCKSTATE/cfg_auto_merge_err"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
grep -q '/target --no-merge ab-aaaa1111' "$MOCKSTATE/ask.log" \
  && pass "x-4391 AC1-ERR: config read failure degrades to no-merge" \
  || fail "x-4391 config err degrade: $(cat "$MOCKSTATE/ask.log")"

# x-8151: the strip/inject grammar left the shell with the resolver (it lives
# in the canonical carrier table's readers, unit-tested in the lint suites);
# the launcher only THREADS the posture. These are the wire-level equivalents
# of the old strip-helper cases: the tri-state must reach the resolve call
# verbatim, whatever the operator passed.
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
bash "$DISPATCH" --allow-merge ab-aaaa1111 >/dev/null 2>&1
grep -q -- "--merge-posture allow" "$MOCKSTATE/resolve.log" \
  && pass "x-8151 wiring: --allow-merge threads --merge-posture allow" \
  || fail "x-8151 wiring allow: $(cat "$MOCKSTATE/resolve.log" 2>/dev/null)"
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
bash "$DISPATCH" --no-merge ab-aaaa1111 >/dev/null 2>&1
grep -q -- "--merge-posture no-merge" "$MOCKSTATE/resolve.log" \
  && pass "x-8151 wiring: --no-merge threads --merge-posture no-merge" \
  || fail "x-8151 wiring no-merge: $(cat "$MOCKSTATE/resolve.log" 2>/dev/null)"
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
grep -q -- "--merge-posture from-config" "$MOCKSTATE/resolve.log" \
  && pass "x-8151 wiring: the default threads --merge-posture from-config" \
  || fail "x-8151 wiring default: $(cat "$MOCKSTATE/resolve.log" 2>/dev/null)"

# AC2-EDGE (codex P2): per-node posture reads THIS node's project cwd, not the
# dispatcher's. Node B's project opts in via its own .fno/auto_merge while the
# global default stays no-merge -> B dispatches allow (from B's config).
reset_mock
projB="$TMP/projB"; mkdir -p "$projB/.fno"; echo dispatch > "$projB/.fno/auto_merge"
set_status ab-bbbb2222 ready; set_claim ab-bbbb2222 free; set_resolved_cwd ab-bbbb2222 "$projB"
out="$(bash "$DISPATCH" ab-bbbb2222 2>&1)"
if grep -q '/target ab-bbbb2222' "$MOCKSTATE/ask.log" && ! grep -q 'no-merge' "$MOCKSTATE/ask.log"; then
  pass "x-4391 AC2-EDGE: per-node posture reads the node's OWN project cwd"
else
  fail "x-4391 per-node cwd routing: $(cat "$MOCKSTATE/ask.log")"
fi

# The dispatcher's own cwd opting in must NOT leak to a node whose project has no
# opt-in: with the global default none and no per-node config, B stays no-merge.
reset_mock
projC="$TMP/projC"; mkdir -p "$projC/.fno"   # no auto_merge file -> project default
set_status ab-bbbb2222 ready; set_claim ab-bbbb2222 free; set_resolved_cwd ab-bbbb2222 "$projC"
out="$(bash "$DISPATCH" ab-bbbb2222 2>&1)"
grep -q '/target --no-merge ab-bbbb2222' "$MOCKSTATE/ask.log" \
  && pass "x-4391 AC2-EDGE: no cross-project posture leak (node project opts out)" \
  || fail "x-4391 posture leak: $(cat "$MOCKSTATE/ask.log")"

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

# ---- gate: an EXPLICITLY-NAMED idea node dispatches (naming is the vet) ----
# A triage-pile node is status: idea (there is no distinct `triage` status), so
# this covers the whole "idea/triage" surface. A real launch (not dry-run)
# exercises the reserving spawn-guard + Guard 3 + receipt path for a non-ready
# status, not just the --no-reserve dry-run short-circuit.
reset_mock; set_status ab-8888dddd idea; set_claim ab-8888dddd free
out="$(bash "$DISPATCH" ab-8888dddd 2>&1)"
echo "$out" | grep -q "^launched ab-8888dddd name=target-ab-8888dddd session=deadbeef01" \
  && pass "gate: explicit idea node dispatches via a real launch (think->blueprint->do)" \
  || fail "gate: explicit idea not dispatched: $out"

# ---- gate: --all-ready drains ready + plan-less idea, parks a linked stub ----
# x-e24a: a plan-less idea (Rung.NONE) is cold-dispatchable and drains like ready
# work; a linked idea stub (plan_path set, Rung.IDEA) still parks.
reset_mock
printf '[{"id":"ab-aaaa1111"},{"id":"ab-8888dddd"},{"id":"ab-9999eeee"}]\n' > "$MOCKSTATE/ready.json"
set_status ab-aaaa1111 ready
set_status ab-8888dddd idea; set_plan ab-8888dddd stub.md   # linked stub -> parked
set_status ab-9999eeee idea                                 # plan-less -> cold-dispatched
out="$(bash "$DISPATCH" --all-ready --dry-run 2>&1)"
echo "$out" | grep -q '^launched ab-aaaa1111 ' && echo "$out" | grep -q '^launched ab-9999eeee ' && echo "$out" | grep -q '^parked ab-8888dddd ' \
  && pass "gate: --all-ready drains ready + plan-less idea, parks linked stub" \
  || fail "gate: --all-ready cold-idea guard wrong: $out"

# ---- gate: a linked idea stub is parked under --all-ready even when named ----
# x-e24a: ALL_READY admits plan-less ideas (Rung.NONE) but still parks a linked
# stub (Rung.IDEA); naming the node does not override the rung gate.
reset_mock
printf '[{"id":"ab-aaaa1111"}]\n' > "$MOCKSTATE/ready.json"
set_status ab-aaaa1111 ready; set_status ab-8888dddd idea; set_plan ab-8888dddd stub.md
out="$(bash "$DISPATCH" --all-ready --dry-run ab-8888dddd 2>&1)"
echo "$out" | grep -q '^parked ab-8888dddd ' \
  && pass "gate: linked idea stub parked under --all-ready even when named" \
  || fail "gate: --all-ready + named linked-stub guard wrong: $out"

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
echo "$out" | grep -q "^failed ab-aaaa1111 reason=.*claim-probe-error" && [[ "$(ask_count)" -eq 0 ]] \
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

# ---- --flags carrying --no-merge does NOT double-inject it ----
reset_mock; set_status ab-aaaa1111 ready
bash "$DISPATCH" --flags "M --no-merge" ab-aaaa1111 >/dev/null 2>&1
nm="$(grep -o -e "--no-merge" "$MOCKSTATE/ask.log" 2>/dev/null | wc -l | tr -d ' ')"
[[ "$nm" -eq 1 ]] \
  && pass "coverage: --flags '...--no-merge' not double-injected (exactly one)" \
  || fail "coverage: --no-merge injected $nm times (expected 1)"

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

# ---- AC1: a node with NO recorded cwd isolates through worktree ensure off
#      the canonical root, so a dispatch from a linked worktree never inherits
#      that worktree (the ensure-failure fallback is asserted below) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
# no set_cwd / set_resolved_cwd -> empty node cwd
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
_gcd="$(cd "$(git rev-parse --git-common-dir 2>/dev/null)" 2>/dev/null && pwd -P)"
_expect_root=""
[[ -n "$_gcd" ]] && _expect_root="$(dirname "$_gcd")"
# sed, not tail: the rtk wrapper on this machine silently empties tail -1.
last_repo="$(cat "$MOCKSTATE/repo_ensure.log" 2>/dev/null)"; last_repo="${last_repo//$'\n'/}"
if [[ -n "$_expect_root" && "$last_repo" == "$_expect_root" ]] \
   && grep -q -- "--cwd $_expect_root/worktrees/" "$MOCKSTATE/ask.log" \
   && ! grep -q -- "--fresh" "$MOCKSTATE/ask.log"; then
  pass "AC1: no node cwd -> ensured worktree off the canonical root, no --fresh"
else
  fail "AC1: no-cwd isolation wrong: repo '$last_repo' want '$_expect_root': $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
fi

# ---- AC1: an ensure failure falls back to --fresh - a no-cwd node has no
#      recorded cwd to fall back to, and the dispatch is never blocked ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
: > "$MOCKSTATE/ensure_fail"
bash "$DISPATCH" ab-aaaa1111 >/dev/null 2>&1
if grep -q -- "--fresh" "$MOCKSTATE/ask.log" && ! grep -q -- "--cwd" "$MOCKSTATE/ask.log"; then
  pass "AC1: ensure failure -> --fresh fallback, dispatch not blocked"
else
  fail "AC1: ensure-failure fallback wrong: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
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
echo "$out" | grep -q "^already-running ab-aaaa1111 reason=\"skipped: duplicate-claim (peer dispatcher holds dispatch:ab-aaaa1111)" && [[ "$(ask_count)" -eq 0 ]] \
  && pass "codex-P1: peer-held reservation -> already-running, no ask (race closed pre-injection)" \
  || fail "codex-P1: reservation race not closed: $out (asks=$(ask_count))"

# ---- x-567d AC1-EDGE: a headless-resolving harness prints the loud one-shot
#      note, spawning --harness <h> --substrate headless (not the thread lane) ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo "codex/headless" > "$MOCKSTATE/resolve_pair"
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q "note: harness 'codex' resolved substrate 'headless' (one-shot runs to completion, not a detached thread)" \
  && pass "x-567d AC1-EDGE: headless-resolving harness prints the loud one-shot note" \
  || fail "x-567d AC1-EDGE: missing one-shot note: $out"
echo "$out" | grep -q -- "--harness codex --substrate headless" \
  && pass "x-567d AC1-EDGE: dispatch resolves --harness codex --substrate headless" \
  || fail "x-567d AC1-EDGE: wrong harness/substrate: $out"
# codex gets its NATIVE skill invocation, not a literal claude /target (P1 #1).
echo "$out" | grep -qF "'\$fno:target --no-merge ab-aaaa1111'" \
  && pass "x-567d P1: codex worker gets the native \$fno:target invocation" \
  || fail "x-567d P1: codex command not \$fno:target: $out"
# Non-claude carries NO --role build (would route Python-owned -> unknown provider; P1 #2).
echo "$out" | grep -q -- "--role build" \
  && fail "x-567d P1: non-claude spawn must NOT carry --role build: $out" \
  || pass "x-567d P1: non-claude spawn drops the claude-only --role/--route lane"

# ---- x-de43: an opencode worker gets the native /fno:target invocation (its
#      fno plugin expands the palette command), never a prose brief nor a bare
#      /target that opencode would run verbatim as prose ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo "opencode/headless" > "$MOCKSTATE/resolve_pair"
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
if echo "$out" | grep -qF "/fno:target --no-merge ab-aaaa1111" \
   && ! echo "$out" | grep -qF "Implement footnote backlog node"; then
  pass "x-de43: opencode worker gets /fno:target (plugin palette), not a prose brief"
else
  fail "x-de43: opencode command should be /fno:target: $out"
fi

# ---- x-a5e4 codex review P1: a codex node with dispatch_verb=/target resolves
#      to `$fno:target <id>` (the verb path bakes in NO no-merge); the launcher
#      MUST still inject no-merge, else a configured auto-merge could merge a
#      background worker despite the locked no-merge policy ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo "codex/headless" > "$MOCKSTATE/resolve_pair"
echo "/target" > "$MOCKSTATE/verb_ab-aaaa1111"
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -qF "'\$fno:target --no-merge ab-aaaa1111'" \
  && pass "x-a5e4 P1: no-merge injected into a codex \$fno:target verb-path command" \
  || fail "x-a5e4 P1: no-merge NOT injected into \$fno:target verb path: $out"

# ---- x-a5e4 codex review P2: --permission-mode is claude-only on the autonomous
#      (bg/headless) lane - `fno agents spawn` rejects it for a non-claude headless
#      provider - so it must be gated OFF a codex/opencode headless spawn ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
echo "codex/headless" > "$MOCKSTATE/resolve_pair"
out="$(bash "$DISPATCH" --dry-run --permission-mode acceptEdits ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--permission-mode" \
  && fail "x-a5e4 P2: non-claude headless spawn must NOT carry --permission-mode: $out" \
  || pass "x-a5e4 P2: --permission-mode gated off the non-claude headless spawn"
# claude still forwards it (regression guard for the gate).
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
out="$(bash "$DISPATCH" --dry-run --permission-mode acceptEdits ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--permission-mode acceptEdits" \
  && pass "x-a5e4 P2: claude spawn still forwards --permission-mode" \
  || fail "x-a5e4 P2: claude should forward --permission-mode: $out"

# ---- x-567d AC2-ERR: no autonomous substrate (resolve fails) -> hard-fail
#      naming config.dispatch.harness, node NOT launched ----
reset_mock; set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
: > "$MOCKSTATE/resolve_fail"
out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
if echo "$out" | grep -q "^failed ab-aaaa1111 reason=\"no autonomous substrate resolved" \
   && echo "$out" | grep -q "config.dispatch.harness" \
   && [[ "$(ask_count)" -eq 0 ]]; then
  pass "x-567d AC2-ERR: unresolvable harness hard-fails naming the config key, no launch"
else
  fail "x-567d AC2-ERR: expected loud hard-fail naming config key, got: $out (asks=$(ask_count))"
fi

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
# The dry-run receipt no longer claims a landing directory: both non-here arms
# isolate through worktree ensure, whose real path is only known once it runs,
# so the preview prints the ensure hint. WHICH root feeds the ensure is still
# the authority question; the real-launch probe below asserts it on
# repo_ensure.log (the mock records every --repo it was handed).
reset_mock; set_status ab-aaaa1111 ready
set_resolved_cwd ab-aaaa1111 /resolved/root
set_cwd ab-aaaa1111 /recorded/other
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--cwd <fno agents workspace worktree ensure>" \
  && pass "AC1-HP: dry-run hint carries the ensure placeholder" \
  || fail "AC1-HP: dry-run hint wrong: $out"
echo "$out" | grep -q "cwd=<fno-worktree-ensure>" \
  && pass "AC1-HP: dry-run cwd= is the space-free ensure placeholder" \
  || fail "AC1-HP: dry-run cwd= token wrong: $out"
if echo "$out" | grep -q "/resolved/root\|/recorded/other"; then
  fail "AC1-HP: dry-run receipt still claims a recorded cwd as the landing dir: $out"
else
  pass "AC1-HP: dry-run receipt claims no location ensure has not chosen"
fi
# Real launch: the ensure mock records its --repo; _resolved_cwd must win, and
# the worker lands in the worktree ensure returned.
out_real="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
last_repo="$(cat "$MOCKSTATE/repo_ensure.log" 2>/dev/null)"; last_repo="${last_repo//$'\n'/}"
[[ "$last_repo" == "/resolved/root" ]] \
  && pass "AC1-HP: real dispatch feeds _resolved_cwd to worktree ensure" \
  || fail "AC1-HP: ensure got repo '$last_repo', want /resolved/root"
grep -q -- "--cwd /resolved/root/worktrees/" "$MOCKSTATE/ask.log" 2>/dev/null \
  && pass "AC1-HP: worker spawns into the ensured worktree under the resolved root" \
  || fail "AC1-HP: spawn cwd wrong: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"

# ---- AC1-EDGE: no _resolved_cwd (stale fno) -> falls back to raw cwd ----
reset_mock; set_status ab-aaaa1111 ready
set_cwd ab-aaaa1111 /recorded/other
# no set_resolved_cwd: mock emits only cwd field. The dry-run preview prints the
# ensure hint on every non-here arm; the raw-cwd authority is asserted on the
# ensure mock's record of the real launch below.
out="$(bash "$DISPATCH" --dry-run ab-aaaa1111 2>&1)"
echo "$out" | grep -q -- "--cwd <fno agents workspace worktree ensure>" \
  && pass "AC1-EDGE: stale-fno dry-run carries the ensure hint" \
  || fail "AC1-EDGE: stale-fno dry-run wrong: $out"
out_real="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
last_repo="$(cat "$MOCKSTATE/repo_ensure.log" 2>/dev/null)"; last_repo="${last_repo//$'\n'/}"
[[ "$last_repo" == "/recorded/other" ]] \
  && pass "AC1-EDGE: stale-fno fallback feeds raw cwd to worktree ensure" \
  || fail "AC1-EDGE: ensure got repo '$last_repo', want /recorded/other"

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
echo "=============================================="
echo "x-3218 - canonical agent-name bridge"
echo "=============================================="
# The launcher used to assemble target-<id>-<slug> locally with no cap on the
# ASSEMBLED name, so a long configured node id emitted a name the runtime
# rejects: no session, no event, a silently lost dispatch. It now delegates to
# the canonical owner (wired suite-wide via NAME_BRIDGE above).
if true; then

  # A node id long enough that target-<id>-<slug30> overflows 64 chars.
  reset_mock
  LONGID="regready-pipeline-2c4f9a1b3d"
  set_status "$LONGID" ready; set_claim "$LONGID" free
  echo "path consolidation wave 0 delegate handoff" > "$MOCKSTATE/slug_$LONGID"
  out="$(bash "$DISPATCH" "$LONGID" 2>&1)"
  launched_name="$(printf '%s' "$out" | sed -n 's/.* name=\([^ ]*\).*/\1/p' | head -1)"
  if [[ -n "$launched_name" && "${#launched_name}" -le 64 ]]; then
    pass "x-3218 long node id yields a name within the 64-char runtime limit (${#launched_name})"
  else
    fail "x-3218 long node id name: ${#launched_name} chars: $out"
  fi
  [[ "$launched_name" == "target-$LONGID"* ]] \
    && pass "x-3218 full node identity survives in the dispatched name" \
    || fail "x-3218 node identity dropped: $launched_name"
  [[ "$(ask_count)" == "1" ]] \
    && pass "x-3218 exactly one worker launch requested" \
    || fail "x-3218 launch count: $(ask_count)"

  # An unrepresentable required identity refuses BEFORE spawn: a loud failure
  # line, no launch. The old path emitted nothing at all.
  reset_mock
  HUGEID="n-$(printf 'z%.0s' $(seq 1 70))"
  set_status "$HUGEID" ready; set_claim "$HUGEID" free
  out="$(bash "$DISPATCH" "$HUGEID" 2>&1)"
  echo "$out" | grep -q "^failed $HUGEID reason=.*64" \
    && pass "x-3218 unrepresentable node id fails loudly with the naming cause" \
    || fail "x-3218 refusal line: $out"
  [[ "$(ask_count)" == "0" ]] \
    && pass "x-3218 refused dispatch launches no worker" \
    || fail "x-3218 refused dispatch still spawned: $(ask_count)"

  # A stale installed fno (no `name` verb) exits 2 like any Click usage error.
  # That must DEGRADE to the historical assembly, never read as "unrepresentable"
  # - misreading it would refuse the entire fleet on an out-of-date install.
  reset_mock
  set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
  echo "cargo bootstrapper" > "$MOCKSTATE/slug_ab-aaaa1111"
  STALE_BRIDGE="$TMP/stale-bridge"
  printf '#!/usr/bin/env bash\necho "Usage: fno agents [OPTIONS] COMMAND" >&2\nexit 2\n' > "$STALE_BRIDGE"
  chmod +x "$STALE_BRIDGE"
  out="$(NAME_BRIDGE="$STALE_BRIDGE" bash "$DISPATCH" ab-aaaa1111 2>&1)"
  echo "$out" | grep -q "^launched ab-aaaa1111 name=target-ab-aaaa1111-cargo-bootstrapper " \
    && pass "x-3218 a stale fno (exit 2) degrades to the historical name, not a refusal" \
    || fail "x-3218 stale-fno degradation: $out"
  [[ "$(ask_count)" == "1" ]] \
    && pass "x-3218 stale fno still dispatches its worker" \
    || fail "x-3218 stale fno lost the dispatch: $(ask_count)"

  # A bridge that emits a warning ahead of the name must NOT have that warning
  # adopted into the name. Streams are merged, so the guard must match the WHOLE
  # capture; a per-line `grep -q` passes here and poisons the spawn.
  reset_mock
  set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
  NOISY_BRIDGE="$TMP/noisy-bridge"
  printf '#!/usr/bin/env bash\necho "DeprecationWarning: something" >&2\necho "target-ab-aaaa1111"\nexit 0\n' > "$NOISY_BRIDGE"
  chmod +x "$NOISY_BRIDGE"
  out="$(NAME_BRIDGE="$NOISY_BRIDGE" bash "$DISPATCH" ab-aaaa1111 2>&1)"
  echo "$out" | grep -q "^launched ab-aaaa1111 name=target-ab-aaaa1111 " \
    && pass "x-3218 a warning on stderr degrades instead of poisoning the name" \
    || fail "x-3218 stderr poisoning: $out"
  echo "$out" | grep -q "name=.*DeprecationWarning" \
    && fail "x-3218 warning text reached the agent name: $out" \
    || pass "x-3218 the stray warning never reaches the agent name"

  # Ordinary names are byte-for-byte unchanged.
  reset_mock
  set_status ab-aaaa1111 ready; set_claim ab-aaaa1111 free
  out="$(bash "$DISPATCH" ab-aaaa1111 2>&1)"
  echo "$out" | grep -q "^launched ab-aaaa1111 name=target-ab-aaaa1111 " \
    && pass "x-3218 ordinary dispatch names are unchanged by the bridge" \
    || fail "x-3218 ordinary name drifted: $out"

  unset NAME_BRIDGE
else
  fail "x-3218 bridge unreachable: no cli venv at $VENV_PY"
fi

echo ""
echo "--- node-cwd dispatch isolates through worktree ensure ---"

NODE_REPO="$TMP/noderepo"
mkdir -p "$NODE_REPO/scripts/setup"
printf '#!/usr/bin/env bash\nmkdir -p "$WORKTREE" && : > "$WORKTREE/.setup-ran"\n' > "$NODE_REPO/scripts/setup/setup-worktree.sh"
chmod +x "$NODE_REPO/scripts/setup/setup-worktree.sh"

# A node-cwd dispatch lands in the ensured worktree, runs the state-linking
# setup, and never spawns --cwd <node repo> directly (the recorded cwd is
# almost always canonical main, where /target's location gate refuses).
reset_mock; set_status ab-bbbb1111 ready
set_resolved_cwd ab-bbbb1111 "$NODE_REPO"
out="$(bash "$DISPATCH" ab-bbbb1111 2>&1)"
spawn_cwd="$(sed -n 's/.*--cwd \([^ ]*\).*/\1/p' "$MOCKSTATE/ask.log" 2>/dev/null | head -1)"
if [[ -n "$spawn_cwd" && "$spawn_cwd" == "$NODE_REPO/worktrees/"* && -f "$spawn_cwd/.setup-ran" ]]; then
  pass "isolation: node-cwd dispatch spawns into the ensured worktree with state linked"
else
  fail "isolation: spawn cwd '$spawn_cwd' is not a set-up ensured worktree: $out"
fi
last_repo="$(cat "$MOCKSTATE/repo_ensure.log" 2>/dev/null)"; last_repo="${last_repo//$'\n'/}"
[[ "$last_repo" == "$NODE_REPO" ]] \
  && pass "isolation: the node's recorded root fed worktree ensure" \
  || fail "isolation: ensure got repo '$last_repo', want $NODE_REPO"

# The dry-run receipt never claims the recorded cwd as the landing directory,
# and the placeholder stays space-free so a field-splitting parser keeps working.
reset_mock; set_status ab-bbbb1111 ready
set_resolved_cwd ab-bbbb1111 "$NODE_REPO"
out="$(bash "$DISPATCH" --dry-run ab-bbbb1111 2>&1)"
echo "$out" | grep -q "cwd=<fno-worktree-ensure>" \
  && pass "dry-run: receipt carries the space-free ensure placeholder" \
  || fail "dry-run: cwd= wrong: $out"
if echo "$out" | grep -qF "$NODE_REPO"; then
  fail "dry-run: receipt still names the recorded repo as the landing dir: $out"
else
  pass "dry-run: receipt claims no location the real run may not use"
fi

# ensure failure never blocks the dispatch: the node's own recorded cwd is the
# fallback landing (the worker's /target start self-isolates from there).
reset_mock; set_status ab-bbbb1111 ready
set_resolved_cwd ab-bbbb1111 "$NODE_REPO"
: > "$MOCKSTATE/ensure_fail"
out="$(bash "$DISPATCH" ab-bbbb1111 2>&1)"
echo "$out" | grep -q "^launched ab-bbbb1111 " \
  && pass "ensure-failure: dispatch still launches" \
  || fail "ensure-failure: dispatch blocked: $out"
grep -q -- "--cwd $NODE_REPO " "$MOCKSTATE/ask.log" 2>/dev/null \
  && pass "ensure-failure: fallback landing is the node's recorded cwd" \
  || fail "ensure-failure: ask.log lacks the node-cwd fallback: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"

# policy=never returns the repo root: launch in place, setup skipped (it would
# link shared state INTO the canonical checkout).
reset_mock; set_status ab-bbbb1111 ready
set_resolved_cwd ab-bbbb1111 "$NODE_REPO"
: > "$MOCKSTATE/ensure_policy_never"
out="$(bash "$DISPATCH" ab-bbbb1111 2>&1)"
grep -q -- "--cwd $NODE_REPO " "$MOCKSTATE/ask.log" 2>/dev/null \
  && pass "policy-never: worker launches at the repo root" \
  || fail "policy-never: spawn cwd wrong: $(cat "$MOCKSTATE/ask.log" 2>/dev/null)"
[[ ! -f "$NODE_REPO/.setup-ran" ]] \
  && pass "policy-never: setup-worktree.sh not run against the repo root" \
  || fail "policy-never: setup ran against the repo root"

echo ""
echo "--- autolaunch holder gate reads the live claim ---"

# A live node claim parks the launch as already-running; the sanctioned
# structural handoff is named instead of a second worker.
reset_mock; set_status ab-cccc1111 ready; set_claim ab-cccc1111 live
mkplan "$TMP/plan-claimed.md" ab-cccc1111
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-claimed.md" 2>&1)"
echo "$out" | grep -q '^already-running ab-cccc1111 .*claim held' \
  && pass "holder: live claim reported as already-running" \
  || fail "holder: live claim not reported: $out"
echo "$out" | grep -q "handoff.sh" \
  && pass "holder: the sanctioned handoff path is named in the reason" \
  || fail "holder: reason omits handoff.sh: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "holder: no worker spawned over a live claim" \
  || fail "holder: spawned over a live claim: $(ask_count)"

# A failed claim read parks honestly with the rc, like the ready-gate does.
reset_mock; set_status ab-cccc1111 ready
: > "$MOCKSTATE/claim_err"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-claimed.md" 2>&1)"
echo "$out" | grep -q '^parked ab-cccc1111 reason="claim read failed (rc=1)' \
  && pass "holder: failed claim read parks with the rc named" \
  || fail "holder: failed read mishandled: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "holder: failed claim read launches nothing" \
  || fail "holder: failed read still spawned: $(ask_count)"

# A read that exits 0 but yields no parsable .state is not evidence of freedom.
reset_mock; set_status ab-cccc1111 ready
: > "$MOCKSTATE/claim_garbage"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-claimed.md" 2>&1)"
echo "$out" | grep -q '^parked ab-cccc1111 reason="claim read unparsable' \
  && pass "holder: unparsable claim read parks, never dispatches" \
  || fail "holder: unparsable read failed open: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "holder: unparsable claim read launches nothing" \
  || fail "holder: unparsable read still spawned: $(ask_count)"

echo ""
echo "--- autolaunch frontmatter reader is robust ---"

# CRLF frontmatter still resolves the claimed node.
reset_mock; set_status ab-dddd1111 ready
printf -- '---\r\ntitle: t\r\nclaims: ab-dddd1111\r\n---\r\n# t\r\n' > "$TMP/plan-crlf.md"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-crlf.md" 2>&1)"
echo "$out" | grep -q "^auto-launched ab-dddd1111 " \
  && pass "frontmatter: CRLF plan resolves its claimed node" \
  || fail "frontmatter: CRLF plan broke resolution: $out"

# A stray --- inside the body is not the closing fence; the frontmatter node
# wins over a body look-alike (the decoy is ready, so a wrong pick would launch).
reset_mock; set_status ab-eeee1111 ready; set_status ab-99999999 ready
printf -- '---\ntitle: t\nclaims: ab-eeee1111\n---\nbody\n---\nclaims: ab-99999999\n---\n# t\n' > "$TMP/plan-stray.md"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-stray.md" 2>&1)"
echo "$out" | grep -q "^auto-launched ab-eeee1111 " \
  && pass "frontmatter: body fence ignored, frontmatter node launched" \
  || fail "frontmatter: stray body fence mishandled: $out"

# An unterminated block is a named read failure on stdout, never a silent skip.
reset_mock; set_status ab-ffff7777 ready
printf -- '---\ntitle: t\nclaims: ab-ffff7777\n' > "$TMP/plan-unterminated.md"
out="$(GATE=true bash "$AUTOLAUNCH" "$TMP/plan-unterminated.md" 2>&1)"
echo "$out" | grep -q '^autolaunch-failed - reason="plan frontmatter read failed' \
  && pass "frontmatter: unterminated block named as a read failure" \
  || fail "frontmatter: unterminated block silent: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "frontmatter: unterminated block launches nothing" \
  || fail "frontmatter: unterminated block spawned: $(ask_count)"

echo ""
echo "--- autolaunch bounds the queued dispatch wait ---"

# A fake plugin tree: the gate sources with-timeout.sh and dispatches
# skills/target/scripts/dispatch-node.sh from REPO_ROOT, so both are stubbed
# there while the gate script itself stays the real one under test.
QREPO="$TMP/queuerepo"
mkdir -p "$QREPO/scripts/lib" "$QREPO/skills/target/scripts"
cp "$REPO_ROOT/scripts/lib/with-timeout.sh" "$QREPO/scripts/lib/with-timeout.sh"
printf '#!/usr/bin/env bash\nsleep 30\n' > "$QREPO/skills/target/scripts/dispatch-node.sh"
chmod +x "$QREPO/skills/target/scripts/dispatch-node.sh"

# A dispatch still queued at the bound parks with the ceiling named, quickly.
reset_mock; set_status ab-aaaa1111 ready
mkplan "$TMP/plan-q.md" ab-aaaa1111
t0=$(date +%s)
out="$(REPO_ROOT="$QREPO" FNO_AUTOLAUNCH_TIMEOUT=2 GATE=true bash "$AUTOLAUNCH" "$TMP/plan-q.md" 2>&1)"
dt=$(( $(date +%s) - t0 ))
echo "$out" | grep -q 'parked ab-aaaa1111 reason="dispatch still queued after 2s' \
  && pass "bound: queued dispatch parks with the ceiling named" \
  || fail "bound: no queue park: $out"
[[ "$dt" -le 8 ]] \
  && pass "bound: park landed near the ceiling (${dt}s)" \
  || fail "bound: park took ${dt}s"

# A dispatch that launches immediately is not bounded out.
printf '#!/usr/bin/env bash\necho "launched ab-aaaa1111 name=x session=y cwd=/z hint=h"\n' > "$QREPO/skills/target/scripts/dispatch-node.sh"
reset_mock; set_status ab-aaaa1111 ready
out="$(REPO_ROOT="$QREPO" FNO_AUTOLAUNCH_TIMEOUT=2 GATE=true bash "$AUTOLAUNCH" "$TMP/plan-q.md" 2>&1)"
echo "$out" | grep -q "^auto-launched ab-aaaa1111 " \
  && pass "bound: immediate launch still auto-launches" \
  || fail "bound: immediate launch bounded out: $out"

# A non-integer ceiling parks with its own named reason, not the wildcard
# "unexpected dispatch output" - and never reaches the (sleeping) dispatch.
printf '#!/usr/bin/env bash\nsleep 30\n' > "$QREPO/skills/target/scripts/dispatch-node.sh"
reset_mock; set_status ab-aaaa1111 ready
out="$(REPO_ROOT="$QREPO" FNO_AUTOLAUNCH_TIMEOUT=3m GATE=true bash "$AUTOLAUNCH" "$TMP/plan-q.md" 2>&1)"
echo "$out" | grep -q 'parked ab-aaaa1111 reason="FNO_AUTOLAUNCH_TIMEOUT must be a bare integer' \
  && pass "bound: non-integer ceiling parks with the reason named" \
  || fail "bound: non-integer ceiling misreported: $out"
[[ "$(ask_count)" -eq 0 ]] \
  && pass "bound: non-integer ceiling never reaches the dispatch" \
  || fail "bound: non-integer ceiling still dispatched: $(ask_count)"

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
