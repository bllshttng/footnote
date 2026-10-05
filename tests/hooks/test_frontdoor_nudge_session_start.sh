#!/usr/bin/env bash
# tests/hooks/test_frontdoor_nudge_session_start.sh
#
# Verifies hooks/frontdoor-nudge-session-start.sh (x-40c4, consent rewritten by
# x-0143): the SessionStart notice about the missing `fno` front door. It must
# go SILENT when `fno` on PATH answers a mux-only verb (the Rust front door is
# active), print the one-line PATH hint for a proven door off PATH, and for a
# genuinely missing door print the consent notice - what the install does, and
# the one command that runs it - while starting NOTHING: no installer child,
# no data dir, no file under the tool bin (x-0143, user ruling 2026-10-01: we
# always tell people before we install anything on their machine).
#
# Cases 1-4 cover the front-door probe (silent / hint). Cases 5-9 cover the
# consent notice per machine shape: no uv, uv present, fno-py present, no
# installer in the tree, and the XDG fallback with no CLAUDE_PLUGIN_DATA.
#
# Isolation: a FAKE `fno` is placed first on PATH per case, so no real mux is
# probed. HOME points at an empty temp dir for every case, so the hook's
# known-install-dir probe never resolves the caller's real `~/.cargo/bin/fno`.
# The notice cases run a copy of the hook under a fake plugin root whose
# postinstall.sh is a stub that touches a marker; the hook must never run it,
# so any marker at all is a failure.
# Run: bash tests/hooks/test_frontdoor_nudge_session_start.sh

set -uo pipefail
# An inherited value would point the fallback case at the real plugin data dir.
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
# Empty HOME: the hook's known-install-dir fallback (~/.local/bin, ~/.cargo/bin)
# must resolve nothing from the caller's real machine.
EMPTY_HOME="$WORK/home"
mkdir -p "$EMPTY_HOME"

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
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
[[ -z "$out" ]] || fail "active front door must be silent, got: $out"
pass "active front door -> silent"

# --- Case 2: fno-py only (fno exists but has no `mux` verb) -> CONSENT NOTICE --
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
# Mimics the Python `fno-py`: any mux verb is "No such command".
echo "No such command 'mux'." >&2
exit 2
FAKE
chmod +x "$FAKEBIN/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
grep -q "Ask the user first" <<<"$out" || fail "fno-py-only must ask before installing, got: $out"
grep -q "fno-py" <<<"$out" || fail "notice must name the Python CLI that already works, got: $out"
pass "fno-py only -> consent notice naming the Python CLI"

# --- Case 3: no `fno` on PATH at all -> CONSENT NOTICE ------------------------
rm -f "$FAKEBIN/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
grep -q "Ask the user first" <<<"$out" || fail "missing fno must ask before installing, got: $out"
pass "no fno on PATH -> consent notice"

# --- Case 3b: fno only in ~/.local/bin, proving the door -> HINT, no installer -
# The gap-audit blocker: a fresh background session's PATH lacks the install
# dir, the hook read the install as missing and started the installer, which
# force-replaced a live tool env. Found-and-proven must name the path instead.
mkdir -p "$EMPTY_HOME/.local/bin"
cat > "$EMPTY_HOME/.local/bin/fno" <<'FAKE'
#!/usr/bin/env bash
[[ "${1:-}" == "mux" && "${2:-}" == "ls" ]] && { echo '[]'; exit 0; }
exit 0
FAKE
chmod +x "$EMPTY_HOME/.local/bin/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
grep -q "installed, just not on this session's PATH" <<<"$out" || fail "known-dir door must print the PATH hint, got: $out"
grep -qF "$EMPTY_HOME/.local/bin/fno" <<<"$out" || fail "hint must name the resolved path, got: $out"
pass "fno in ~/.local/bin proving the door -> hint naming the path"

# --- Case 3c: fno in ~/.cargo/bin but NOT the door -> falls through to NOTICE --
rm -rf "$EMPTY_HOME/.local"
mkdir -p "$EMPTY_HOME/.cargo/bin"
cat > "$EMPTY_HOME/.cargo/bin/fno" <<'FAKE'
#!/usr/bin/env bash
echo "No such command 'mux'." >&2
exit 2
FAKE
chmod +x "$EMPTY_HOME/.cargo/bin/fno"
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
grep -q "Ask the user first" <<<"$out" || fail "known-dir non-door fno must fall through to the notice, got: $out"
pass "fno in ~/.cargo/bin without a mux verb -> consent notice"
rm -rf "$EMPTY_HOME/.cargo"

# --- Case 4: wedged mux socket -> BOUNDED and SILENT --------------------------
# This hook probes a socket at SessionStart, so an unbounded probe stalls every
# session start. On a host with no coreutils timeout(1) it had no bound at all.
# A wedged socket also PROVES the Rust front door is present (fno-py has no
# `mux` verb and fails fast), so the correct behavior is silence, not a notice
# telling the user to install what they already have.
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
if [[ "${1:-}" == "mux" && "${2:-}" == "ls" ]]; then sleep 30; fi
exit 0
FAKE
chmod +x "$FAKEBIN/fno"
START=$(date +%s)
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" bash "$HOOK" 2>/dev/null)
END=$(date +%s)
ELAPSED=$((END - START))
(( ELAPSED < 8 )) || fail "wedged mux probe not bounded: ${ELAPSED}s (the 3s cap did not fire)"
# Floor as well as ceiling: an early exit would satisfy the bound while proving
# the cap never ran.
(( ELAPSED >= 1 )) || fail "wedged mux probe returned in ${ELAPSED}s, too fast to have run the stub - it exited early and this case tested nothing"
[[ -z "$out" ]] || fail "a wedged mux socket proves the front door exists, so the hook must stay silent, got: $out"
pass "wedged mux socket -> bounded at the cap (${ELAPSED}s) and silent"

# --- Notice cases: a fake plugin root with a stub installer -------------------
PLUG="$WORK/plug"
mkdir -p "$PLUG/hooks" "$PLUG/hooks/lib" "$PLUG/scripts/lib" "$PLUG/.claude-plugin"
cp "$HOOK" "$PLUG/hooks/"
cp "$REPO_ROOT_REAL/hooks/lib/fno-bin.sh" "$PLUG/hooks/lib/"
cp "$REPO_ROOT_REAL/scripts/lib/with-timeout.sh" "$PLUG/scripts/lib/"
printf '{\n  "name": "fno",\n  "version": "9.9.9"\n}\n' >"$PLUG/.claude-plugin/plugin.json"
MARK="$WORK/installer-ran"
# The stub leaves a marker the instant it runs. The hook must never start it,
# so the marker is a failure signal in every notice case; the stub also sleeps
# so a detached spawn (the pre-x-0143 behavior) would still land the marker
# before the assertions below read it.
cat >"$PLUG/.claude-plugin/postinstall.sh" <<STUB
#!/usr/bin/env bash
touch "$MARK"
sleep 30
STUB
PHOOK="$PLUG/hooks/frontdoor-nudge-session-start.sh"
DATA="$WORK/data"
rm -f "$FAKEBIN/fno"

run_hook() { PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" CLAUDE_PLUGIN_DATA="$DATA" bash "$PHOOK" 2>/dev/null; }

# The x-0143 verify contract: the hook creates no data dir and no file under
# the tool bin, on this HOME or any fallback.
TOOLBIN="$EMPTY_HOME/.local/bin"
mkdir -p "$TOOLBIN"

# --- Case 5: no uv, no fno, fresh HOME -> full notice, NOTHING started --------
out=$(run_hook)
grep -q "Ask the user first" <<<"$out" || fail "notice must ask for consent, got: $out"
grep -q "astral.sh/uv/install.sh" <<<"$out" || fail "notice must name the uv installer, got: $out"
grep -q "shell profile" <<<"$out" || fail "notice must name the profile edit, got: $out"
grep -q "PyPI" <<<"$out" || fail "notice must name PyPI, got: $out"
grep -qF "bash $PLUG/.claude-plugin/postinstall.sh" <<<"$out" || fail "notice must name the one command, got: $out"
[[ ! -e "$MARK" ]] || fail "the hook started the installer (no child may run unasked)"
[[ ! -e "$DATA" ]] || fail "the hook created the plugin data dir; the lock/log era is gone"
[[ -z "$(ls -A "$TOOLBIN" 2>/dev/null)" ]] || fail "the tool bin grew files"
pass "no uv, no fno -> full consent notice, nothing started, tool bin untouched"

# --- Case 6: uv already present -> notice scopes the touch to the fno tool ----
cat > "$FAKEBIN/uv" <<'FAKE'
#!/usr/bin/env bash
exit 0
FAKE
chmod +x "$FAKEBIN/uv"
out=$(run_hook)
grep -q "uv is already installed" <<<"$out" || fail "uv present must be named, got: $out"
grep -q "nothing but the fno tool" <<<"$out" || fail "uv-present notice must scope the touch, got: $out"
if grep -q "astral.sh" <<<"$out"; then
  fail "uv-present notice must skip the astral.sh step"
fi
[[ ! -e "$MARK" ]] || fail "the hook started the installer stub"
rm -f "$FAKEBIN/uv"
pass "uv present -> notice scopes to the fno tool, no astral.sh step"

# --- Case 7: fno-py present (probe rc 2) -> notice says the Python CLI works --
cat > "$FAKEBIN/fno" <<'FAKE'
#!/usr/bin/env bash
echo "No such command 'mux'." >&2
exit 2
FAKE
chmod +x "$FAKEBIN/fno"
out=$(run_hook)
grep -q "fno-py" <<<"$out" || fail "fno-py presence must be named, got: $out"
grep -q "Ask the user first" <<<"$out" || fail "fno-py-only still needs consent for the full install, got: $out"
[[ ! -e "$MARK" ]] || fail "the hook started the installer stub"
rm -f "$FAKEBIN/fno"
pass "fno-py only -> notice names it and still asks"

# --- Case 8: installer missing from the tree -> short note, nothing started ---
mv "$PLUG/.claude-plugin/postinstall.sh" "$WORK/postinstall.sh.bak"
out=$(run_hook)
grep -q "carries no installer" <<<"$out" || fail "missing installer must say so, got: $out"
[[ ! -e "$MARK" ]] || fail "nothing may run without the installer"
mv "$WORK/postinstall.sh.bak" "$PLUG/.claude-plugin/postinstall.sh"
pass "installer missing -> short note, nothing started"

# --- Case 9: CLAUDE_PLUGIN_DATA unset -> same notice, nothing created ---------
# A Codex session carries no CLAUDE_PLUGIN_DATA; the notice prints the same
# way, and the hook still creates nothing in any fallback data dir.
out=$(PATH="$FAKEBIN:$BASE_PATH" HOME="$EMPTY_HOME" XDG_STATE_HOME="$WORK/xdg-state" bash "$PHOOK" 2>/dev/null)
grep -q "Ask the user first" <<<"$out" || fail "XDG fallback must print the notice, got: $out"
[[ ! -e "$WORK/xdg-state" ]] || fail "the hook created the XDG data dir"
[[ ! -e "$MARK" ]] || fail "the hook started the installer stub"
pass "CLAUDE_PLUGIN_DATA unset -> same notice, nothing created"

log "all cases passed"
