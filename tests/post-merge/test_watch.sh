#!/usr/bin/env bash
# test_watch.sh - post-merge watcher contract (ab-4e9fb05a).
#
# Drives scripts/post-merge/watch.sh through its test seams
# (POST_MERGE_PRS_JSON, POST_MERGE_FIRE_CMD, POST_MERGE_WATERMARK_FILE) so the
# poll -> fire -> advance-on-success contract is asserted without real gh/claude,
# plus an AC2-HP test of install.sh's human gate under a fake HOME.
#
# Watermark is composite "<mergedAt>\t<number>" so same-second siblings are not
# skipped on a mid-batch failure.
#
# Exit: 0 all pass, 1 assertion failed, 77 skipped (missing jq).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WATCH="${REPO_ROOT}/scripts/post-merge/watch.sh"
INSTALL="${REPO_ROOT}/scripts/post-merge/install.sh"

pass() { printf '[post-merge-watch] PASS: %s\n' "$*"; }
fail() { printf '[post-merge-watch] FAIL: %s\n' "$*" >&2; exit 1; }
skip() { printf '[post-merge-watch] SKIP: %s\n' "$*" >&2; exit 77; }

command -v jq  >/dev/null 2>&1 || skip "jq not on PATH"
command -v git >/dev/null 2>&1 || skip "git CLI not found"
git rev-parse --is-inside-work-tree >/dev/null 2>&1 || skip "not running inside a git repository"
[[ -f "$WATCH" ]]   || fail "watch.sh not found at $WATCH"
[[ -f "$INSTALL" ]] || fail "install.sh not found at $INSTALL"
bash -n "$WATCH"   || fail "bash -n rejected watch.sh"
bash -n "$INSTALL" || fail "bash -n rejected install.sh"
pass "watch.sh + install.sh pass bash -n"

TMP="$(mktemp -d -t pm-watch.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

# Fake fire: record each fired PR, fail iff PR == $FAIL_ON.
FIRE="$TMP/fire.sh"
cat > "$FIRE" <<'FIRE_EOF'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "$FIRED_LOG"
[[ -n "${FAIL_ON:-}" && "$1" == "$FAIL_ON" ]] && exit 1
exit 0
FIRE_EOF
chmod +x "$FIRE"

PRS='[{"number":101,"mergedAt":"2026-05-30T10:00:00Z","title":"a"},{"number":102,"mergedAt":"2026-05-30T11:00:00Z","title":"b"}]'

# An early ESTABLISHED watermark (a file that pre-dates every test PR). Used so
# the processing tests run as an established watcher, not as a first-run baseline
# (an absent watermark now triggers baseline mode - covered by AC-BASELINE).
EARLY=$'2026-05-30T00:00:00Z\t0'

# run_watch <prs-json> <watermark-seed-or-empty> <fail-on-or-empty> -> echoes rc.
# Empty seed defaults to EARLY (established watcher), NOT an absent watermark.
run_watch() {
  local prs="$1" seed="$2" failon="$3"
  WM="$TMP/wm"; FIRED="$TMP/fired"
  rm -f "$WM" "$FIRED"; : > "$FIRED"
  printf '%s' "${seed:-$EARLY}" > "$WM"
  POST_MERGE_PRS_JSON="$prs" POST_MERGE_WATERMARK_FILE="$WM" \
  POST_MERGE_FIRE_CMD="bash $FIRE" FIRED_LOG="$FIRED" FAIL_ON="$failon" \
    bash "$WATCH" >/dev/null 2>&1
  echo $?
}
fired_list() { tr '\n' ' ' < "$TMP/fired" 2>/dev/null | sed 's/ *$//'; }
wm_full()    { [[ -f "$TMP/wm" ]] && tr '\t' '|' < "$TMP/wm" || echo "<none>"; }

# --- AC1-HP: empty watermark -> both fire oldest-first; watermark = newest ---
rc=$(run_watch "$PRS" "" "")
[[ "$rc" == "0" ]] || fail "AC1-HP: expected rc 0, got $rc"
[[ "$(fired_list)" == "101 102" ]] || fail "AC1-HP: expected '101 102' oldest-first, got '$(fired_list)'"
[[ "$(wm_full)" == "2026-05-30T11:00:00Z|102" ]] || fail "AC1-HP: watermark expected newest composite, got '$(wm_full)'"
pass "AC1-HP: new merges fire oldest-first; composite watermark advances to newest"

# --- AC1-EDGE: watermark at newest composite -> nothing fires ---
rc=$(run_watch "$PRS" "2026-05-30T11:00:00Z	102" "")
[[ "$rc" == "0" ]] || fail "AC1-EDGE: expected rc 0, got $rc"
[[ -z "$(fired_list)" ]] || fail "AC1-EDGE: expected no fires, got '$(fired_list)'"
pass "AC1-EDGE: PRs at/below the composite watermark do not reprocess"

# --- AC1-EDGE2: watermark between the two -> only the newer fires ---
rc=$(run_watch "$PRS" "2026-05-30T10:00:00Z	101" "")
[[ "$(fired_list)" == "102" ]] || fail "AC1-EDGE2: expected only '102', got '$(fired_list)'"
pass "AC1-EDGE2: only PRs strictly after the watermark fire"

# --- AC1-ERR: first fire fails -> watermark NOT advanced, rc 1 ---
rc=$(run_watch "$PRS" "" "101")
[[ "$rc" == "1" ]] || fail "AC1-ERR: expected rc 1 on fire failure, got $rc"
[[ "$(wm_full)" == "2026-05-30T00:00:00Z|0" ]] || fail "AC1-ERR: watermark advanced despite failure (should stay at early seed), got '$(wm_full)'"
pass "AC1-ERR: a failed fire leaves the watermark for retry"

# --- AC1-ERR2: second fire fails -> watermark at first success only ---
rc=$(run_watch "$PRS" "" "102")
[[ "$rc" == "1" ]] || fail "AC1-ERR2: expected rc 1, got $rc"
[[ "$(fired_list)" == "101 102" ]] || fail "AC1-ERR2: expected attempt '101 102', got '$(fired_list)'"
[[ "$(wm_full)" == "2026-05-30T10:00:00Z|101" ]] || fail "AC1-ERR2: watermark should be first success, got '$(wm_full)'"
pass "AC1-ERR2: mid-batch failure stops at the last successful PR's composite watermark"

# --- AC1-SAMESEC: two PRs in the SAME second; 2nd fails then later succeeds ---
SAMESEC='[{"number":201,"mergedAt":"2026-05-30T12:00:00Z","title":"x"},{"number":202,"mergedAt":"2026-05-30T12:00:00Z","title":"y"}]'
rc=$(run_watch "$SAMESEC" "" "202")          # 201 ok, 202 fails
[[ "$rc" == "1" ]] || fail "AC1-SAMESEC: expected rc 1, got $rc"
[[ "$(wm_full)" == "2026-05-30T12:00:00Z|201" ]] || fail "AC1-SAMESEC: watermark should be #201 composite, got '$(wm_full)'"
# Next poll (no failure) MUST re-select 202 (same second, higher number), not skip it.
WM="$TMP/wm"; FIRED="$TMP/fired"; : > "$FIRED"   # keep the #201 watermark from above
POST_MERGE_PRS_JSON="$SAMESEC" POST_MERGE_WATERMARK_FILE="$WM" \
POST_MERGE_FIRE_CMD="bash $FIRE" FIRED_LOG="$FIRED" FAIL_ON="" \
  bash "$WATCH" >/dev/null 2>&1
rc=$?
[[ "$rc" == "0" ]] || fail "AC1-SAMESEC retry: expected rc 0, got $rc"
[[ "$(fired_list)" == "202" ]] || fail "AC1-SAMESEC retry: same-second sibling 202 should re-fire, got '$(fired_list)'"
[[ "$(wm_full)" == "2026-05-30T12:00:00Z|202" ]] || fail "AC1-SAMESEC retry: watermark should advance to #202, got '$(wm_full)'"
pass "AC1-SAMESEC: a same-second sibling is not skipped after a mid-batch failure"

# --- AC-EMPTY: empty list -> no fire, rc 0 ---
rc=$(run_watch "[]" "" "")
[[ "$rc" == "0" ]] || fail "AC-EMPTY: expected rc 0, got $rc"
[[ -z "$(fired_list)" ]] || fail "AC-EMPTY: expected no fires, got '$(fired_list)'"
pass "AC-EMPTY: empty PR list is a clean no-op"

# --- AC-NULL: a null mergedAt row is filtered; the valid row fires ---
NULLJSON='[{"number":301,"mergedAt":null,"title":"open-ish"},{"number":302,"mergedAt":"2026-05-30T13:00:00Z","title":"ok"}]'
rc=$(run_watch "$NULLJSON" "" "")
[[ "$(fired_list)" == "302" ]] || fail "AC-NULL: expected only '302' (null filtered), got '$(fired_list)'"
pass "AC-NULL: rows with a null mergedAt are filtered out"

# --- AC-BASELINE: first run (NO watermark file) seeds baseline, does NOT fire ---
# Installing on a repo with history must not retroactively run the ritual for
# every past merge (Codex P1 "first install").
WM="$TMP/wm"; FIRED="$TMP/fired"; rm -f "$WM"; : > "$FIRED"
POST_MERGE_PRS_JSON="$PRS" POST_MERGE_WATERMARK_FILE="$WM" \
POST_MERGE_FIRE_CMD="bash $FIRE" FIRED_LOG="$FIRED" FAIL_ON="" \
  bash "$WATCH" >/dev/null 2>&1
rc=$?
[[ "$rc" == "0" ]] || fail "AC-BASELINE: expected rc 0, got $rc"
[[ -z "$(fired_list)" ]] || fail "AC-BASELINE: first run must NOT fire any ritual (got '$(fired_list)')"
[[ "$(wm_full)" == "2026-05-30T11:00:00Z|102" ]] || fail "AC-BASELINE: baseline should be newest PR, got '$(wm_full)'"
pass "AC-BASELINE: first run establishes a baseline at the newest PR without firing"

# --- AC2-HP: install.sh writes + prints the plist but NEVER loads it ---
FAKEHOME="$TMP/fakehome"
OUT="$(HOME="$FAKEHOME" POST_MERGE_INTERVAL=600 bash "$INSTALL" 2>&1)" \
  || fail "AC2-HP: install.sh exited non-zero"
PLIST="$(ls "$FAKEHOME/Library/LaunchAgents/"*.plist 2>/dev/null | head -1)"
[[ -n "$PLIST" && -f "$PLIST" ]] || fail "AC2-HP: install.sh did not write a plist under fake HOME"
printf '%s' "$OUT" | grep -q "launchctl load" || fail "AC2-HP: install.sh did not print the launchctl load command"
grep -A1 "<key>RunAtLoad</key>" "$PLIST" | grep -q "<false/>" || fail "AC2-HP: RunAtLoad is not false in the rendered plist"
grep -qE "\{\{" "$PLIST" && fail "AC2-HP: rendered plist has an unsubstituted placeholder"
if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$PLIST" >/dev/null 2>&1 || fail "AC2-HP: rendered plist is not valid (plutil -lint failed)"
fi
LABEL_PREFIX="com.fno.postmerge.$(basename "$REPO_ROOT")"
if command -v launchctl >/dev/null 2>&1 && launchctl list 2>/dev/null | grep -q "$LABEL_PREFIX"; then
  fail "AC2-HP: an agent '$LABEL_PREFIX*' is loaded - install.sh must NOT load it (human gate)"
fi
pass "AC2-HP: install.sh renders + prints a valid plist (RunAtLoad=false) and never loads it"

# --- AC2-PATH: the plist sets an EnvironmentVariables PATH (Codex P1) ---
grep -q "<key>EnvironmentVariables</key>" "$PLIST" \
  || fail "AC2-PATH: plist missing EnvironmentVariables (launchd would not find gh/jq/claude)"
grep -q "<key>PATH</key>" "$PLIST" || fail "AC2-PATH: plist EnvironmentVariables has no PATH"
pass "AC2-PATH: plist renders an EnvironmentVariables PATH for launchd"

# --- AC2-LABEL: Label is unique per checkout (path-hash suffix; Codex P2) ---
grep -A1 "<key>Label</key>" "$PLIST" | grep -qE -- '-[0-9a-f]{8}</string>' \
  || fail "AC2-LABEL: plist Label lacks a per-checkout hash suffix"
pass "AC2-LABEL: plist Label is unique per checkout (hash suffix)"

# --- AC2-MODEL: plist bakes in POST_MERGE_MODEL (defaults to Haiku) ---
grep -q "<key>POST_MERGE_MODEL</key>" "$PLIST" || fail "AC2-MODEL: plist missing POST_MERGE_MODEL env"
grep -A1 "<key>POST_MERGE_MODEL</key>" "$PLIST" | grep -q "claude-haiku" \
  || fail "AC2-MODEL: plist POST_MERGE_MODEL default is not Haiku"
# Override respected:
OUT2="$(HOME="$TMP/fakehome2" POST_MERGE_MODEL=sonnet bash "$INSTALL" 2>/dev/null)" || true
PLIST2="$(ls "$TMP/fakehome2/Library/LaunchAgents/"*.plist 2>/dev/null | head -1)"
[[ -n "$PLIST2" ]] && grep -A1 "<key>POST_MERGE_MODEL</key>" "$PLIST2" | grep -q "sonnet" \
  || fail "AC2-MODEL: POST_MERGE_MODEL override (sonnet) not rendered"
pass "AC2-MODEL: plist bakes in the ritual model (Haiku default, overridable)"

printf '[post-merge-watch] all scenarios passed\n'
exit 0
