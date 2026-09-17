#!/usr/bin/env bash
# tests/hooks/test_frontdoor_nudge_session_start.sh
#
# Verifies hooks/frontdoor-nudge-session-start.sh (x-40c4): the SessionStart
# reminder to install the Rust `fno` front door. It must go SILENT when `fno` on
# PATH answers a mux-only verb (the Rust front door is active), and print the
# one-line reminder when `fno` is absent or is the Python `fno-py` (no `mux`
# subcommand).
#
# Cases 5-9 cover the installer launch: with CLAUDE_PLUGIN_DATA set, the hook
# starts .claude-plugin/postinstall.sh detached, once per plugin version.
#
# Isolation: a FAKE `fno` is placed first on PATH per case, so no real mux is
# probed. The installer cases run a copy of the hook under a fake plugin root
# whose postinstall.sh is a stub, so the real installer never runs.
# Run: bash tests/hooks/test_frontdoor_nudge_session_start.sh

set -uo pipefail
# An inherited value would make cases 1-4 start the REAL installer.
unset CLAUDE_PLUGIN_DATA

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT_REAL}/hooks/frontdoor-nudge-session-start.sh"

log()  { printf '[frontdoor-ss] %s\n' "$*"; }
fail() { printf '[frontdoor-ss] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[frontdoor-ss] PASS: %s\n' "$*"; }

[[ -f "$HOOK" ]] || fail "hook not found at $HOOK"

WORK=$(mktemp -d -t frontdoor-ss-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
FAKEBIN="$WORK/bin"
mkdir -p "$FAKEBIN"

# A minimal PATH that resolves the utilities the hook needs but never the real
# `fno`. It no longer needs a coreutils timeout(1): the hook's bound comes from
# scripts/lib/with-timeout.sh, which uses shell builtins plus sleep.
BASE_PATH="/usr/bin:/bin:/usr/sbin:/sbin"

# --- Case 1: Rust front door active (fno answers `mux ls --json`) -> SILENT ----
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
if [[ "${1:-}" == "mux" && "${2:-}" == "ls" ]]; then echo '[]'; exit 0; fi
exit 0
FAKE
chmod +x "$FAKEBIN/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" bash "$HOOK" 2>/dev/null)
[[ -z "$out" ]] || fail "active front door must be silent, got: $out"
pass "active front door -> silent"

# --- Case 2: fno-py only (fno exists but has no `mux` verb) -> REMIND ----------
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
# Mimics the Python `fno-py`: any mux verb is "No such command".
echo "No such command 'mux'." >&2
exit 2
FAKE
chmod +x "$FAKEBIN/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" bash "$HOOK" 2>/dev/null)
grep -q "Install the .fno. front door" <<<"$out" || fail "fno-py-only must remind, got: $out"
grep -q "cargo install fno" <<<"$out" || fail "reminder must name the fix, got: $out"
pass "fno-py only -> reminder with fix"

# --- Case 3: no `fno` on PATH at all -> REMIND --------------------------------
rm -f "$FAKEBIN/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" bash "$HOOK" 2>/dev/null)
grep -q "Install the .fno. front door" <<<"$out" || fail "missing fno must remind, got: $out"
pass "no fno on PATH -> reminder"

# --- Case 4: wedged mux socket -> BOUNDED and SILENT --------------------------
# This hook probes a socket at SessionStart, so an unbounded probe stalls every
# session start. On a host with no coreutils timeout(1) it had no bound at all.
# A wedged socket also PROVES the Rust front door is present (fno-py has no
# `mux` verb and fails fast), so the correct behavior is silence, not a reminder
# telling the user to install what they already have.
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
if [[ "${1:-}" == "mux" && "${2:-}" == "ls" ]]; then sleep 30; fi
exit 0
FAKE
chmod +x "$FAKEBIN/fno"
START=$(date +%s)
out=$(PATH="$FAKEBIN:$BASE_PATH" bash "$HOOK" 2>/dev/null)
END=$(date +%s)
ELAPSED=$((END - START))
(( ELAPSED < 8 )) || fail "wedged mux probe not bounded: ${ELAPSED}s (the 3s cap did not fire)"
# Floor as well as ceiling: an early exit would satisfy the bound while proving
# the cap never ran.
(( ELAPSED >= 1 )) || fail "wedged mux probe returned in ${ELAPSED}s, too fast to have run the stub - it exited early and this case tested nothing"
[[ -z "$out" ]] || fail "a wedged mux socket proves the front door exists, so the hook must stay silent, got: $out"
pass "wedged mux socket -> bounded at the cap (${ELAPSED}s) and silent"

# --- Installer cases: a fake plugin root with a stub installer ----------------
PLUG="$WORK/plug"
mkdir -p "$PLUG/hooks" "$PLUG/scripts/lib" "$PLUG/.claude-plugin"
cp "$HOOK" "$PLUG/hooks/"
cp "$REPO_ROOT_REAL/scripts/lib/with-timeout.sh" "$PLUG/scripts/lib/"
printf '{\n  "name": "fno",\n  "version": "9.9.9"\n}\n' >"$PLUG/.claude-plugin/plugin.json"
MARK="$WORK/installer-ran"
cat >"$PLUG/.claude-plugin/postinstall.sh" <<STUB
#!/usr/bin/env bash
sleep 2
touch "$MARK"
exit "\${STUB_RC:-0}"
STUB
PHOOK="$PLUG/hooks/frontdoor-nudge-session-start.sh"
DATA="$WORK/data"
rm -f "$FAKEBIN/fno"

run_hook() { PATH="$FAKEBIN:$BASE_PATH" CLAUDE_PLUGIN_DATA="$DATA" bash "$PHOOK" 2>/dev/null; }

# Wait up to 10s for the detached installer to release its lock.
wait_unlocked() {
  local i
  for i in $(seq 1 50); do
    [[ -d "$DATA/postinstall.lock" ]] || return 0
    sleep 0.2
  done
  return 1
}

# --- Case 5: no stamp -> starts the installer detached, stamps on exit 0 ------
START=$(date +%s)
out=$(run_hook)
ELAPSED=$(( $(date +%s) - START ))
(( ELAPSED < 2 )) || fail "hook waited on the installer: ${ELAPSED}s"
grep -q "Installing the fno CLI" <<<"$out" || fail "no stamp must start the installer, got: $out"
grep -qF "$DATA/postinstall.log" <<<"$out" || fail "installing message must name the log, got: $out"
[[ ! -e "$MARK" ]] || fail "installer finished before the hook returned: it was not detached"
wait_unlocked || fail "installer lock still held after 10s"
[[ -e "$MARK" ]] || fail "detached installer never ran"
[[ "$(cat "$DATA/postinstall.version" 2>/dev/null)" == "9.9.9" ]] || fail "stamp does not hold the plugin version"
grep -q "installer exit 0" "$DATA/postinstall.log" || fail "log lacks the exit line"
pass "no stamp -> installer runs detached (${ELAPSED}s), stamps 9.9.9, drops the lock"

# --- Case 6: stamp holds the version -> no installer, reminder + log ----------
rm -f "$MARK"
out=$(run_hook)
sleep 3
[[ ! -e "$MARK" ]] || fail "stamped version must not start the installer"
grep -q "Install the .fno. front door" <<<"$out" || fail "stamped case must remind, got: $out"
grep -qF "$DATA/postinstall.log" <<<"$out" || fail "stamped reminder must name the log, got: $out"
pass "stamped version -> no installer, reminder names the log"

# --- Case 7: installer fails -> no stamp, no lock, retry names the exit code --
rm -rf "$DATA" "$MARK"
export STUB_RC=1
out=$(run_hook)
wait_unlocked || fail "failed installer left its lock"
[[ -e "$MARK" ]] || fail "failing installer never ran"
[[ ! -e "$DATA/postinstall.version" ]] || fail "a failed install must not stamp"
rm -f "$MARK"
out=$(run_hook)
grep -q "Installing the fno CLI" <<<"$out" || fail "failed install must retry next session, got: $out"
grep -q "installer exit 1" <<<"$out" || fail "retry message must name the failed exit code, got: $out"
wait_unlocked || fail "retry installer left its lock"
unset STUB_RC
pass "failed install -> no stamp, lock dropped, retry names exit 1"

# --- Case 8: live lock holder -> no second installer, in-progress -------------
rm -rf "$DATA" "$MARK"
mkdir -p "$DATA/postinstall.lock"
sleep 30 &
HOLDER=$!
echo "$HOLDER" >"$DATA/postinstall.lock/pid"
out=$(run_hook)
sleep 3
kill "$HOLDER" 2>/dev/null
wait "$HOLDER" 2>/dev/null
[[ ! -e "$MARK" ]] || fail "a live lock must block a second installer"
grep -q "install in progress" <<<"$out" || fail "live lock must report in progress, got: $out"
pass "live lock holder -> no second installer, in-progress message"

# --- Case 9: CLAUDE_PLUGIN_DATA unset -> today's reminder, no installer --------
rm -rf "$DATA" "$MARK"
out=$(PATH="$FAKEBIN:$BASE_PATH" bash "$PHOOK" 2>/dev/null)
sleep 3
[[ ! -e "$MARK" ]] || fail "no CLAUDE_PLUGIN_DATA must not start the installer"
grep -q "Install the .fno. front door" <<<"$out" || fail "no data dir must remind, got: $out"
! grep -q "already ran" <<<"$out" || fail "no data dir must print only the plain reminder, got: $out"
pass "CLAUDE_PLUGIN_DATA unset -> plain reminder, no installer"

log "all cases passed"
