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

# `git` and `gh` are shimmed so the scan's repo scoping is driven by the
# fixtures, not by whatever checkout the suite happens to run from. Without
# this every scoping assertion below would read the real footnote remote and
# pass or fail depending on the developer's cwd.
cat > "$UBIN/git" <<'SHIM'
#!/usr/bin/env bash
case "$*" in
  "remote get-url origin")
    [ -n "${FAKE_ORIGIN_URL:-}" ] || exit 1
    printf '%s\n' "$FAKE_ORIGIN_URL" ;;
  *--git-common-dir*)
    [ -n "${FAKE_GIT_COMMON_DIR:-}" ] || exit 1
    printf '%s\n' "$FAKE_GIT_COMMON_DIR" ;;
  *--show-toplevel*)
    [ -n "${FAKE_GIT_TOPLEVEL:-}" ] || exit 1
    printf '%s\n' "$FAKE_GIT_TOPLEVEL" ;;
  *) exit 1 ;;
esac
SHIM
chmod +x "$UBIN/git"
# gh is the documented fallback for a checkout whose GitHub remote is not
# `origin`, so it must be reachable - but it returns a DIFFERENT slug than the
# git remote does, so any assertion that passes via gh when it should have used
# git remote (or the reverse) fails loudly instead of coincidentally passing.
cat > "$UBIN/gh" <<'SHIM'
#!/usr/bin/env bash
[ -n "${FAKE_GH_SLUG:-}" ] || exit 1
printf '%s\n' "$FAKE_GH_SLUG"
SHIM
chmod +x "$UBIN/gh"

# A real directory, because the block derives the repo root by cd-ing to
# "<git-common-dir>/.." and running `pwd -P`. Capture the same resolved form
# the block will produce (on macOS /tmp is a symlink, so the literal path and
# its `pwd -P` are different strings).
mkdir -p "$TMP/repo/.git"
REPO_FIXTURE="$(cd "$TMP/repo" && pwd -P)"
ORIGIN_FIXTURE="https://github.com/o/r.git"

UHOME="$TMP/home"; mkdir -p "$UHOME/.fno"
run_step2() {
  # run_step2 <reconcile_json> <graph_json> <pr> -> prints resulting NODE_IDS
  printf '%s' "$2" > "$UHOME/.fno/graph.json"
  local runner="$TMP/step2-run.sh"; cp "$STEP2" "$runner"
  printf '\nprintf "%%s" "$NODE_IDS"\n' >> "$runner"
  FAKE_RECONCILE_JSON="$1" PR="$3" HOME="$UHOME" PATH="$UBIN:$PATH" \
    FAKE_ORIGIN_URL="${FAKE_ORIGIN_URL-$ORIGIN_FIXTURE}" \
    FAKE_GIT_COMMON_DIR="${FAKE_GIT_COMMON_DIR-$TMP/repo/.git}" \
    FAKE_GIT_TOPLEVEL="${FAKE_GIT_TOPLEVEL-$TMP/repo}" \
    FAKE_GH_SLUG="${FAKE_GH_SLUG-}" \
    TMPDIR="$TMP" bash "$runner"
}
count_id() { printf '%s' "$1" | tr ' ' '\n' | grep -c "^$2\$"; }

# Real schema: nodes are stored flat under `.entries`, NOT `.nodes` (a `.nodes`
# scan silently yields empty and the reap never fires - the bug codex caught).
# Nodes carry a pr_url because graph.json is CROSS-PROJECT: a bare pr_number is
# ambiguous across repos, so the scan admits a node only on a matching origin
# slug, or on cwd for a url-less one.
GRAPH_MATCH='{"entries":[{"id":"x-1234","pr_number":292,"pr_url":"https://github.com/o/r/pull/292"}]}'

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

# AC2c (collision): pr_number is not unique - two SAME-REPO ids match -> union
# BOTH, so the reap fires for every candidate (head -1 would drop one).
NI="$(run_step2 '{"closed":[]}' '{"entries":[
  {"id":"x-1234","pr_number":292,"pr_url":"https://github.com/o/r/pull/292"},
  {"id":"x-5678","pr_number":292,"pr_url":"https://github.com/o/r/pull/292"}]}' 292)"
[[ "$(count_id "$NI" x-1234)" == "1" && "$(count_id "$NI" x-5678)" == "1" ]] \
  && pass "AC2c: a non-unique pr_number unions every matching id (no head -1 drop)" \
  || fail "AC2c: expected both x-1234 and x-5678, got: $(printf '%q' "$NI")"

# --- Repo scoping: a PR number is unique only WITHIN a repo ----------------
# The reason this scan is scoped at all. Under config.post_merge.self_reap a
# foreign id in NODE_IDS makes Step 8a stop+rm ANOTHER repo's build-worker row,
# and that row being non-live is exactly what makes it eligible - so the reap
# loop's live-guard bounds the damage without scoping it.
NI="$(run_step2 '{"closed":[]}' '{"entries":[
  {"id":"x-mine","pr_number":292,"pr_url":"https://github.com/o/r/pull/292"},
  {"id":"x-theirs","pr_number":292,"pr_url":"https://github.com/other/repo/pull/292"}]}' 292)"
[[ "$(count_id "$NI" x-mine)" == "1" && "$(count_id "$NI" x-theirs)" == "0" ]] \
  && pass "AC4: a same-numbered PR in a FOREIGN repo is excluded from NODE_IDS" \
  || fail "AC4: expected only x-mine, got: $(printf '%q' "$NI")"

# Slug matching is a substring between anchors, so a repo whose name merely
# extends ours must not match, and case must not matter (git and gh disagree
# on case for the same remote).
NI="$(run_step2 '{"closed":[]}' '{"entries":[
  {"id":"x-super","pr_number":292,"pr_url":"https://github.com/o/r-extra/pull/292"},
  {"id":"x-upper","pr_number":292,"pr_url":"https://github.com/O/R/pull/292"}]}' 292)"
[[ "$(count_id "$NI" x-super)" == "0" && "$(count_id "$NI" x-upper)" == "1" ]] \
  && pass "AC5: a superstring slug is excluded; a case-differing slug still matches" \
  || fail "AC5: expected only x-upper, got: $(printf '%q' "$NI")"

# A url-less node is the pr_number backstop: admitted only when its cwd IS this
# repo. A foreign cwd, or no cwd at all, is dropped - fail closed, never a guess.
NI="$(run_step2 '{"closed":[]}' '{"entries":[
  {"id":"x-here","pr_number":292,"cwd":"'"$REPO_FIXTURE"'"},
  {"id":"x-elsewhere","pr_number":292,"cwd":"/some/other/repo"},
  {"id":"x-nothing","pr_number":292}]}' 292)"
[[ "$(count_id "$NI" x-here)" == "1" && "$(count_id "$NI" x-elsewhere)" == "0" \
   && "$(count_id "$NI" x-nothing)" == "0" ]] \
  && pass "AC6: a url-less node unions only when its cwd is this repo" \
  || fail "AC6: expected only x-here, got: $(printf '%q' "$NI")"

# A hand-edited graph can hold a non-string pr_url. jq's ascii_downcase raises
# on one and aborts the WHOLE program, so a single corrupt node would silently
# drop every legitimate node after it - the union must survive it.
NI="$(run_step2 '{"closed":[]}' '{"entries":[
  {"id":"x-corrupt","pr_number":292,"pr_url":{"not":"a string"}},
  {"id":"x-good","pr_number":292,"pr_url":"https://github.com/o/r/pull/292"}]}' 292)"
[[ "$(count_id "$NI" x-good)" == "1" && "$(count_id "$NI" x-corrupt)" == "0" ]] \
  && pass "AC7: a corrupt non-string pr_url is skipped, not fatal to the scan" \
  || fail "AC7: expected x-good despite the corrupt node, got: $(printf '%q' "$NI")"

# No resolvable origin: the union is skipped WHOLESALE rather than falling back
# to an unscoped match, and says so. Under-reaping is recoverable; reaping
# another repo's row is not.
NI="$(FAKE_ORIGIN_URL="" run_step2 '{"closed":[{"node_id":"x-closed"}]}' "$GRAPH_MATCH" 292 2>"$TMP/noslug.err")"
[[ "$(count_id "$NI" x-1234)" == "0" && "$(count_id "$NI" x-closed)" == "1" ]] \
  && grep -q "union SKIPPED" "$TMP/noslug.err" \
  && pass "AC8: no origin slug skips the union loudly, keeping reconcile-closed ids" \
  || fail "AC8: expected only x-closed + a SKIPPED line, got: $(printf '%q' "$NI")"

# A non-GitHub origin (a mirror, a local path) must yield NOTHING from the git
# read rather than a confident slug no pr_url can match, so gh - which covers a
# checkout whose GitHub remote is not named `origin` - still gets its turn.
NI="$(FAKE_ORIGIN_URL="git@gitlab.com:mirror/x.git" FAKE_GH_SLUG="o/r" \
      run_step2 '{"closed":[]}' "$GRAPH_MATCH" 292)"
[[ "$(count_id "$NI" x-1234)" == "1" ]] \
  && pass "AC9: a non-GitHub origin falls through to gh instead of scoping on a bad slug" \
  || fail "AC9: expected x-1234 via the gh fallback, got: $(printf '%q' "$NI")"

# Every GitHub remote form resolves without gh. If the git-side regex ever
# regresses, gh is unset here so the assertion cannot pass by accident.
for U in "git@github.com:o/r.git" "https://github.com/o/r" "ssh://git@github.com/o/r.git" "git://github.com/o/r"; do
  NI="$(FAKE_ORIGIN_URL="$U" FAKE_GH_SLUG="" run_step2 '{"closed":[]}' "$GRAPH_MATCH" 292)"
  [[ "$(count_id "$NI" x-1234)" == "1" ]] \
    || { fail "AC9b: origin form '$U' did not resolve, got: $(printf '%q' "$NI")"; BAD_FORM=1; }
done
[[ "${BAD_FORM:-0}" == "0" ]] \
  && pass "AC9b: scp, https, ssh and git:// GitHub remotes all resolve without gh"

# `git init --separate-git-dir` puts the common dir OUTSIDE the checkout, so its
# parent is not a working tree. Deriving the root as "<common-dir>/.." would
# silently exclude every url-less node; the .git probe must catch it and fall
# back to the working tree git reports.
mkdir -p "$TMP/external-gitdir"
NI="$(FAKE_GIT_COMMON_DIR="$TMP/external-gitdir" run_step2 '{"closed":[]}' \
      '{"entries":[{"id":"x-sep","pr_number":292,"cwd":"'"$REPO_FIXTURE"'"}]}' 292)"
[[ "$(count_id "$NI" x-sep)" == "1" ]] \
  && pass "AC10: separate-git-dir falls back to the real checkout for the repo root" \
  || fail "AC10: expected x-sep via the --show-toplevel fallback, got: $(printf '%q' "$NI")"

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
