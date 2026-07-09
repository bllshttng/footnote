#!/usr/bin/env bash
# test_reap_build_worker.sh - post-merge Step 8a build-worker reap contract (x-317a).
#
# Extracts the Step 8a fenced bash block from skills/pr/references/merged.md and
# runs it against a fake `fno` shim (recording stop/rm call order) so the three
# User Stories are exercised behaviorally, not just grep-asserted:
#   US1  self_reap on, finished row  -> stop THEN rm, row-name in both calls
#   US2  self_reap off/unset         -> nothing removed, prints the manual command
#   US3  no NODE_IDS / no row / live  -> clean no-op, no stop/rm
# Plus static guards for the stop-before-rm ordering invariant and the
# status!="live" guard (the one line a reviewer must check), and the Step 2
# stderr-capture contract that feeds NODE_IDS.
#
# Exit: 0 all pass, 1 assertion failed, 77 skipped (missing jq).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
MERGED_MD="${REPO_ROOT}/skills/pr/references/merged.md"

pass() { printf '[reap-build-worker] PASS: %s\n' "$*"; }
fail() { printf '[reap-build-worker] FAIL: %s\n' "$*" >&2; FAILED=1; }
skip() { printf '[reap-build-worker] SKIP: %s\n' "$*" >&2; exit 77; }

command -v jq >/dev/null 2>&1 || skip "jq not on PATH"
[[ -f "$MERGED_MD" ]] || { echo "merged.md not found at $MERGED_MD" >&2; exit 1; }

FAILED=0
TMP="$(mktemp -d -t reap-build-worker.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# --- Extract the Step 8a fenced bash block --------------------------------
BLOCK="$TMP/step8a.sh"
awk '
  /^## Step 8a:/      { in_section=1 }
  in_section && /^## Step 8:/ { exit }
  in_section && /^```bash$/ { grab=1; next }
  in_section && grab && /^```$/ { exit }
  in_section && grab { print }
' "$MERGED_MD" > "$BLOCK"

[[ -s "$BLOCK" ]] || fail "could not extract Step 8a bash block from merged.md"

STEP2="$TMP/step2.sh"
awk '
  /^## Step 2:/      { in_section=1 }
  in_section && /^## Step 3:/ { exit }
  in_section && /^```bash$/ { grab=1; next }
  in_section && grab && /^```$/ { exit }
  in_section && grab { print }
' "$MERGED_MD" > "$STEP2"

[[ -s "$STEP2" ]] || fail "could not extract Step 2 bash block from merged.md"

# --- Fake `fno` shim (records stop/rm call order) -------------------------
BIN="$TMP/bin"
mkdir -p "$BIN"
cat > "$BIN/fno" <<'SHIM'
#!/usr/bin/env bash
# args: `config get <key>` | `agents list --json` | `agents stop <row>` | `agents rm <row>`
case "$1 $2" in
  "config get") printf '%s' "${FAKE_SELF_REAP:-}" ;;
  "agents list") printf '%s' "${FAKE_AGENTS_JSON:-{\"agents\":[]}}" ;;
  "agents stop") echo "stop $3" >> "$CALLLOG" ;;
  "agents rm")   echo "rm $3"   >> "$CALLLOG" ;;
  *) echo "unexpected fno call: $*" >&2; exit 2 ;;
esac
SHIM
chmod +x "$BIN/fno"

# Fixed path so the record survives run_block's command-substitution subshell.
CALLLOG="$TMP/calls.log"
export CALLLOG

run_block() {
  # run_block <self_reap> <agents_json> <node_ids> -> stdout; side effect: $CALLLOG
  : > "$CALLLOG"
  FAKE_SELF_REAP="$1" FAKE_AGENTS_JSON="$2" NODE_IDS="$3" \
    PATH="$BIN:$PATH" bash "$BLOCK"
}

ROW='target-x-1234-some-slug'
JSON_ORPHANED='{"agents":[{"name":"target-x-1234-some-slug","status":"orphaned"}]}'
JSON_LIVE='{"agents":[{"name":"target-x-1234-redispatch","status":"live"}]}'
JSON_NOMATCH='{"agents":[{"name":"target-x-9999-other","status":"orphaned"}]}'

# --- US1: self_reap on, finished worker -> stop THEN rm --------------------
OUT="$(run_block "true" "$JSON_ORPHANED" "x-1234")"
CALLS="$(cat "$CALLLOG")"
[[ "$CALLS" == "stop $ROW"$'\n'"rm $ROW" ]] \
  && pass "US1: stop precedes rm, both name the resolved row" \
  || fail "US1: expected 'stop $ROW' then 'rm $ROW', got: $(printf '%q' "$CALLS")"

# --- US2: self_reap unset -> nothing removed, prints manual command --------
OUT="$(run_block "" "$JSON_ORPHANED" "x-1234")"
[[ ! -s "$CALLLOG" ]] \
  && pass "US2: self_reap off removes nothing" \
  || fail "US2: expected no stop/rm calls, got: $(cat "$CALLLOG")"
printf '%s' "$OUT" | grep -F "fno agents stop $ROW && fno agents rm $ROW" >/dev/null \
  && pass "US2: prints the stop-then-rm manual command" \
  || fail "US2: manual command line missing from output"

# --- US3a: no NODE_IDS -> clean no-op -------------------------------------
run_block "true" "$JSON_ORPHANED" "" >/dev/null
[[ ! -s "$CALLLOG" ]] \
  && pass "US3a: empty NODE_IDS is a no-op" \
  || fail "US3a: empty NODE_IDS still fired calls"

# --- US3b: node id but no matching row -> no-op ---------------------------
run_block "true" "$JSON_NOMATCH" "x-1234" >/dev/null
[[ ! -s "$CALLLOG" ]] \
  && pass "US3b: no matching row is a no-op" \
  || fail "US3b: non-matching row still fired calls"

# --- US3c: only a live row -> guarded, untouched --------------------------
run_block "true" "$JSON_LIVE" "x-1234" >/dev/null
[[ ! -s "$CALLLOG" ]] \
  && pass "US3c: status=live row is left untouched" \
  || fail "US3c: live row was reaped (status guard broken)"

# --- Step 2 union: reap on the ship-gate-close path (pr_number scan) -------
# Runs the Step 2 block against a fake `fno backlog reconcile` + a controlled
# graph.json, asserting the union that feeds NODE_IDS. python3 is shimmed to
# fail so GJ falls back to $HOME/.fno/graph.json (deterministic, no real graph).
UBIN="$TMP/bin2"
mkdir -p "$UBIN"
cat > "$UBIN/fno" <<'SHIM'
#!/usr/bin/env bash
case "$1 $2" in
  "backlog reconcile") printf '%s' "${FAKE_RECONCILE_JSON:-{\"closed\":[]}}" ;;
  *) echo "unexpected fno call: $*" >&2; exit 2 ;;
esac
SHIM
chmod +x "$UBIN/fno"
printf '#!/usr/bin/env bash\nexit 1\n' > "$UBIN/python3"
chmod +x "$UBIN/python3"

UHOME="$TMP/home"; mkdir -p "$UHOME/.fno"
run_step2() {
  # run_step2 <reconcile_json> <graph_json> <pr> -> prints resulting NODE_IDS
  printf '%s' "$2" > "$UHOME/.fno/graph.json"
  local runner="$TMP/step2-run.sh"; cp "$STEP2" "$runner"
  printf '\nprintf "%%s" "$NODE_IDS"\n' >> "$runner"
  FAKE_RECONCILE_JSON="$1" PR="$3" HOME="$UHOME" PATH="$UBIN:$PATH" bash "$runner"
}
count_id() { printf '%s' "$1" | tr ' ' '\n' | grep -c "^$2\$"; }

# Real schema: nodes are stored flat under `.entries`, NOT `.nodes` (a `.nodes`
# scan silently yields empty and the reap never fires - the bug codex caught).
GRAPH_MATCH='{"entries":[{"id":"x-1234","pr_number":292}]}'

# AC1 (the bug): reconcile .closed[] empty, pr_number matches -> node unioned in.
NI="$(run_step2 '{"closed":[]}' "$GRAPH_MATCH" 292)"
[[ "$(count_id "$NI" x-1234)" == "1" ]] \
  && pass "AC1: ship-gate close (reconcile empty) unions the PR node into NODE_IDS" \
  || fail "AC1: expected x-1234 in NODE_IDS, got: $(printf '%q' "$NI")"

# AC1b (schema guard): a legacy `.nodes`-shaped graph must NOT resolve - proves
# the scan reads `.entries`, so a regression back to `.nodes` fails loudly here.
NI="$(run_step2 '{"closed":[]}' '{"nodes":[{"id":"x-1234","pr_number":292}]}' 292)"
[[ -z "${NI// }" ]] \
  && pass "AC1b: a .nodes-shaped graph resolves nothing (scan reads .entries)" \
  || fail "AC1b: expected empty from a .nodes-shaped graph, got: $(printf '%q' "$NI")"

# AC2 (dedup): reconcile already closed the same node -> unioned once, not twice.
NI="$(run_step2 '{"closed":[{"node_id":"x-1234"}]}' "$GRAPH_MATCH" 292)"
[[ "$(count_id "$NI" x-1234)" == "1" ]] \
  && pass "AC2: out-of-gate id present in both .closed[] and pr_number match dedups to one" \
  || fail "AC2: expected exactly one x-1234, got: $(printf '%q' "$NI")"

# AC2b (append, no clobber): reconcile closed a different node -> both survive.
NI="$(run_step2 '{"closed":[{"node_id":"x-aaaa"}]}' "$GRAPH_MATCH" 292)"
[[ "$(count_id "$NI" x-aaaa)" == "1" && "$(count_id "$NI" x-1234)" == "1" ]] \
  && pass "AC2b: union appends the PR node without clobbering reconcile's closed ids" \
  || fail "AC2b: expected both x-aaaa and x-1234, got: $(printf '%q' "$NI")"

# AC2c (collision): pr_number is not unique - two ids match -> union BOTH, so
# the reap fires for every candidate (head -1 would drop one; this is PR 315's
# own dogfood case where two fno nodes share the PR number).
NI="$(run_step2 '{"closed":[]}' '{"entries":[{"id":"x-1234","pr_number":292},{"id":"x-5678","pr_number":292}]}' 292)"
[[ "$(count_id "$NI" x-1234)" == "1" && "$(count_id "$NI" x-5678)" == "1" ]] \
  && pass "AC2c: a non-unique pr_number unions every matching id (no head -1 drop)" \
  || fail "AC2c: expected both x-1234 and x-5678, got: $(printf '%q' "$NI")"

# AC3 (no node): pr_number matches nothing -> NODE_IDS stays empty.
NI="$(run_step2 '{"closed":[]}' '{"entries":[{"id":"x-9999","pr_number":999}]}' 292)"
[[ -z "${NI// }" ]] \
  && pass "AC3: PR mapping to no node leaves NODE_IDS empty (Step 8a skips)" \
  || fail "AC3: expected empty NODE_IDS, got: $(printf '%q' "$NI")"

# --- Static guards: ordering invariant + live guard present ---------------
STOP_LN="$(awk '/fno agents stop/ {print NR; exit}' "$BLOCK")"
RM_LN="$(awk '/fno agents rm/ {print NR; exit}' "$BLOCK")"
[[ -n "$STOP_LN" && -n "$RM_LN" && "$STOP_LN" -lt "$RM_LN" ]] \
  && pass "invariant: 'fno agents stop' textually precedes 'fno agents rm'" \
  || fail "invariant: stop must precede rm in the block (stop=$STOP_LN rm=$RM_LN)"
grep -q 'status != "live"' "$BLOCK" \
  && pass "guard: status != \"live\" present" \
  || fail "guard: status != \"live\" missing from the block"
grep -F '2>"$RECONCILE_ERR"' "$STEP2" >/dev/null \
  && grep -F 'cat "$RECONCILE_ERR" >&2' "$STEP2" >/dev/null \
  && ! grep -F 'reconcile --json 2>/dev/null' "$STEP2" >/dev/null \
  && pass "guard: reconcile stderr is captured and surfaced" \
  || fail "guard: reconcile stderr must be captured, not discarded"

echo ""
[[ "$FAILED" -eq 0 ]] && echo "[reap-build-worker] all assertions passed" \
                      || echo "[reap-build-worker] FAILURES present" >&2
exit "$FAILED"
