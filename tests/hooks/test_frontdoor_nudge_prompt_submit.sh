#!/usr/bin/env bash
# tests/hooks/test_frontdoor_nudge_prompt_submit.sh
#
# Verifies hooks/frontdoor-nudge-prompt-submit.sh: the UserPromptSubmit twin of
# the SessionStart frontdoor notice. SessionStart does not fire in the session
# that installed the plugin, so the first prompt must surface the consent
# notice - what the install does, the one command that runs it, ask first
# (x-0143) - behind cheap per-prompt guards (announced marker, fno-bin
# resolver), and stay SILENT whenever there is nothing to surface. The notice
# starts nothing: the installer stub must never run.
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
# The stub leaves a marker the instant it runs. The wrapper relays a notice
# that starts nothing, so the marker is a failure signal in every case; the
# sleep keeps a detached spawn (the pre-x-0143 behavior) alive until the
# assertions read the marker.
cat >"$PLUG/.claude-plugin/postinstall.sh" <<STUB
#!/usr/bin/env bash
touch "$MARK"
sleep 30
STUB
chmod +x "$PLUG/.claude-plugin/postinstall.sh"

PWRAPPER="$PLUG/hooks/frontdoor-nudge-prompt-submit.sh"
DATA="$WORK/data"
# HOME is isolated like the session-start twin: the wrapper's fno-bin
# resolver checks ~/.cargo/bin and ~/.local/bin, which on a dev machine
# carry a REAL install the case count must not see.
EMPTY_HOME="$WORK/home"
mkdir -p "$EMPTY_HOME"

run_wrapper() { PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" CLAUDE_PLUGIN_DATA="$DATA" bash "$PWRAPPER" 2>/dev/null; }

# --- Case 1: marker holds the version -> SILENT, the hook never runs ----------
mkdir -p "$DATA"
printf '%s' "9.9.9" >"$DATA/postinstall.announced"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "announced marker must be silent, got: $out"
[[ ! -e "$MARK" ]] || fail "announced marker must not start the installer"
pass "marker matches version -> silent, no installer"

# --- Case 2: no marker, no fno -> notice relayed, marker written, then SILENT -
rm -rf "$DATA" "$MARK"
out=$(run_wrapper)
rc=$?
[[ $rc -eq 0 ]] || fail "wrapper must exit 0, got $rc"
grep -q "Ask the user first" <<<"$out" || fail "first prompt must surface the consent notice, got: $out"
grep -qF "bash $PLUG/.claude-plugin/postinstall.sh" <<<"$out" || fail "notice must name the one command, got: $out"
[[ "$(cat "$DATA/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "announced marker missing"
[[ ! -e "$MARK" ]] || fail "the wrapper started the installer stub"
[[ ! -e "$DATA/postinstall.lock" ]] || fail "the wrapper created an install lock; that era is gone"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "announced marker must silence later prompts, got: $out"
pass "first prompt relays the notice and writes the marker; later prompts go silent"

# --- Case 3: fno on PATH, no marker -> SILENT (probe stays SessionStart's job) -
rm -rf "$DATA" "$MARK"
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
pass "fno on PATH -> silent, probe left to SessionStart"

# --- Case 4: installer missing from the tree -> note relayed once, then SILENT -
rm -rf "$DATA" "$MARK"
mv "$PLUG/.claude-plugin/postinstall.sh" "$WORK/postinstall.sh.bak"
out=$(run_wrapper)
grep -q "carries no installer" <<<"$out" || fail "missing installer must be relayed once, got: $out"
[[ "$(cat "$DATA/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "missing-installer relay must write the marker"
[[ ! -e "$MARK" ]] || fail "nothing may run without the installer"
out=$(run_wrapper)
[[ -z "$out" ]] || fail "marker must silence the next prompt, got: $out"
mv "$WORK/postinstall.sh.bak" "$PLUG/.claude-plugin/postinstall.sh"
pass "missing installer -> note once, then silent"

# --- Case 5: CLAUDE_PLUGIN_DATA unset -> XDG fallback carries the marker -------
rm -rf "$DATA" "$MARK" "$WORK/xdg-state"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" XDG_STATE_HOME="$WORK/xdg-state" bash "$PWRAPPER" 2>/dev/null)
grep -q "Ask the user first" <<<"$out" || fail "XDG fallback must surface the notice, got: $out"
[[ "$(cat "$WORK/xdg-state/fno/plugin-install/postinstall.announced" 2>/dev/null)" == "9.9.9" ]] || fail "fallback marker missing"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" XDG_STATE_HOME="$WORK/xdg-state" bash "$PWRAPPER" 2>/dev/null)
[[ -z "$out" ]] || fail "fallback marker must silence later prompts, got: $out"
pass "CLAUDE_PLUGIN_DATA unset -> XDG fallback carries the marker"

log "all cases passed"
