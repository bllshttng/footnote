#!/usr/bin/env bash
# tests/hooks/test_frontdoor_nudge_prompt_submit.sh
#
# Verifies hooks/frontdoor-nudge-prompt-submit.sh: the UserPromptSubmit twin of
# the SessionStart frontdoor nudge. SessionStart does not fire in the session
# that installed the plugin, so the first prompt must start the CLI installer,
# behind cheap per-prompt guards (version stamp, announced marker,
# `command -v fno`), and stay SILENT whenever there is nothing to surface.
#
# The racing case runs the SessionStart hook and this wrapper CONCURRENTLY
# against one data dir: the existing lock must let exactly one installer start.
#
# Isolation: a fake plugin root carries copies of both hooks; the installer is
# a stub that appends to a count file. A minimal PATH keeps the real `fno`
# unreachable.
# Run: bash tests/hooks/test_frontdoor_nudge_prompt_submit.sh

set -uo pipefail
# An inherited CLAUDE_PLUGIN_DATA would point the XDG-fallback case at the real
# plugin data dir (the same trap its SessionStart counterpart test unsets for).
unset CLAUDE_PLUGIN_DATA

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRAPPER="${REPO_ROOT_REAL}/hooks/frontdoor-nudge-prompt-submit.sh"
SESSION_HOOK="${REPO_ROOT_REAL}/hooks/frontdoor-nudge-session-start.sh"

log()  { printf '[frontdoor-ps] %s\n' "$*"; }
fail() { printf '[frontdoor-ps] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[frontdoor-ps] PASS: %s\n' "$*"; }

[[ -f "$WRAPPER" ]] || fail "wrapper not found at $WRAPPER"
[[ -f "$SESSION_HOOK" ]] || fail "session hook not found at $SESSION_HOOK"

WORK=$(mktemp -d -t frontdoor-ps-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"
BASE_PATH="/usr/bin:/bin:/usr/sbin:/sbin"

# A fake plugin root: real hooks, stub installer, version 9.9.9.
PLUG="$WORK/plug"
mkdir -p "$PLUG/hooks" "$PLUG/hooks/lib" "$PLUG/scripts/lib" "$PLUG/.claude-plugin"
cp "$WRAPPER" "$PLUG/hooks/"
cp "$SESSION_HOOK" "$PLUG/hooks/"
cp "$REPO_ROOT_REAL/hooks/lib/fno-bin.sh" "$PLUG/hooks/lib/"
cp "$REPO_ROOT_REAL/scripts/lib/with-timeout.sh" "$PLUG/scripts/lib/"
printf '{\n  "name": "fno",\n  "version": "9.9.9"\n}\n' >"$PLUG/.claude-plugin/plugin.json"
MARK="$WORK/installer-ran"
COUNT="$WORK/installer-count"
cat >"$PLUG/.claude-plugin/postinstall.sh" <<STUB
#!/usr/bin/env bash
sleep 2
echo ran >>"$COUNT"
touch "$MARK"
exit "\${STUB_RC:-0}"
STUB
chmod +x "$PLUG/.claude-plugin/postinstall.sh"

PWRAPPER="$PLUG/hooks/frontdoor-nudge-prompt-submit.sh"
PSESSION="$PLUG/hooks/frontdoor-nudge-session-start.sh"
DATA="$WORK/data"

run_wrapper() { PATH="$FAKEBIN:$BASE_PATH" CLAUDE_PLUGIN_DATA="$DATA" bash "$PWRAPPER" 2>/dev/null; }

wait_unlocked() {
  local dir="${1:-$DATA}" _
  for _ in $(seq 1 50); do
    [[ -d "$dir/postinstall.lock" ]] || return 0
    sleep 0.2
  done
  return 1
}

# --- Case 1: stamp holds the version -> SILENT, full hook never runs ----------
mkdir -p "$DATA"
printf '%s' "9.9.9" >"$DATA/postinstall.version"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "stamped version must be silent, got: $out"
[[ ! -e "$MARK" ]] || fail "stamped version must not start the installer"
pass "stamp matches version -> silent, no installer"

# --- Case 2: no stamp, no fno -> note relayed, marker written, then SILENT ----
rm -rf "$DATA" "$MARK"
out=$(run_wrapper)
rc=$?
[[ $rc -eq 0 ]] || fail "wrapper must exit 0, got $rc"
grep -q "Installing the fno CLI" <<<"$out" || fail "first prompt must surface the installing note, got: $out"
grep -qF "$DATA/postinstall.log" <<<"$out" || fail "note must name the log path, got: $out"
[[ "$(cat "$DATA/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "announced marker missing"
wait_unlocked || fail "installer left its lock"
[[ "$(cat "$COUNT" 2>/dev/null)" == "ran" ]] || fail "installer did not run exactly once"
rm -f "$MARK"; : >"$COUNT"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "announced marker must silence later prompts, got: $out"
sleep 3
[[ ! -e "$MARK" ]] || fail "announced marker must not start a second installer"
pass "first prompt relays the note and writes the marker; later prompts go silent"

# --- Case 3: fno on PATH, no stamp -> SILENT (probe stays SessionStart's job) -
rm -rf "$DATA" "$MARK"; : >"$COUNT"
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
echo "No such command 'mux'." >&2
exit 2
FAKE
chmod +x "$FAKEBIN/fno"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "an fno on PATH must stay silent per prompt, got: $out"
[[ ! -e "$MARK" ]] || fail "an fno on PATH must not start the installer"
rm -f "$FAKEBIN/fno"
pass "fno on PATH -> silent, installer left to SessionStart"

# --- Case 4: live lock holder -> in-progress relayed, then SILENT -------------
rm -rf "$DATA" "$MARK"; : >"$COUNT"
mkdir -p "$DATA/postinstall.lock"
sleep 30 &
HOLDER=$!
echo "$HOLDER" >"$DATA/postinstall.lock/pid"
out=$(run_wrapper)
kill "$HOLDER" 2>/dev/null
wait "$HOLDER" 2>/dev/null
grep -q "install in progress" <<<"$out" || fail "live lock must relay the in-progress note, got: $out"
[[ "$(cat "$DATA/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "in-progress relay must write the marker"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "marker must silence the next prompt, got: $out"
pass "live lock -> in-progress note once, then silent"

# --- Case 5: SessionStart hook and this wrapper RACE -> one installer ---------
rm -rf "$DATA" "$MARK"; : >"$COUNT"
START=$(date +%s)
PATH="$FAKEBIN:$BASE_PATH" CLAUDE_PLUGIN_DATA="$DATA" bash "$PSESSION" >"$WORK/out-ss.txt" 2>/dev/null &
SS_PID=$!
PATH="$FAKEBIN:$BASE_PATH" CLAUDE_PLUGIN_DATA="$DATA" bash "$PWRAPPER" >"$WORK/out-ps.txt" 2>/dev/null &
PS_PID=$!
wait "$SS_PID" "$PS_PID"
ELAPSED=$(( $(date +%s) - START ))
(( ELAPSED < 5 )) || fail "racing hooks waited on the installer: ${ELAPSED}s"
wait_unlocked || fail "racing installers left a lock"
STARTS=$(wc -l <"$COUNT" | tr -d ' ')
[[ "$STARTS" == "1" ]] || fail "the lock must let exactly one installer start, got $STARTS"
TOTAL_INSTALL=$(grep -c "Installing the fno CLI" "$WORK/out-ss.txt" "$WORK/out-ps.txt" | awk -F: '{s+=$2} END {print s}')
TOTAL_PROGRESS=$(grep -c "install in progress" "$WORK/out-ss.txt" "$WORK/out-ps.txt" | awk -F: '{s+=$2} END {print s}')
[[ "$TOTAL_INSTALL" == "1" ]] || fail "exactly one racing hook may claim the installer start, got $TOTAL_INSTALL"
[[ "$TOTAL_PROGRESS" == "1" ]] || fail "exactly one racing hook may report in progress, got $TOTAL_PROGRESS"
pass "racing SessionStart + UserPromptSubmit triggers -> one installer (${ELAPSED}s), one start note, one in-progress note"

# --- Case 6: CLAUDE_PLUGIN_DATA unset -> XDG fallback gets stamp and marker ---
rm -rf "$DATA" "$MARK" "$WORK/xdg-state"; : >"$COUNT"
XDG_DIR="$WORK/xdg-state/fno/plugin-install"
out=$(PATH="$FAKEBIN:$BASE_PATH" XDG_STATE_HOME="$WORK/xdg-state" bash "$PWRAPPER" 2>/dev/null)
grep -q "Installing the fno CLI" <<<"$out" || fail "XDG fallback must surface the note, got: $out"
wait_unlocked "$XDG_DIR" || fail "fallback installer left its lock"
[[ "$(cat "$XDG_DIR/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "fallback marker missing"
out=$(PATH="$FAKEBIN:$BASE_PATH" XDG_STATE_HOME="$WORK/xdg-state" bash "$PWRAPPER" 2>/dev/null)
[[ -z "$out" ]] || fail "fallback marker must silence later prompts, got: $out"
pass "CLAUDE_PLUGIN_DATA unset -> XDG fallback carries stamp and marker"

# --- Case 7: static-reminder state (no installer in the tree) -> SILENT -------
# Without postinstall.sh the full hook prints its static reminder, and no
# marker could ever be written (no version resolves). The wrapper must not
# relay that into context on every prompt: the preamble owns the story.
rm -rf "$DATA" "$MARK"
mv "$PLUG/.claude-plugin/postinstall.sh" "$WORK/postinstall.sh.bak"
out=$(run_wrapper)
mv "$WORK/postinstall.sh.bak" "$PLUG/.claude-plugin/postinstall.sh"
[[ -z "$out" ]] || fail "static reminder must not be relayed per prompt, got: $out"
pass "static-reminder state -> silent"

log "all cases passed"
