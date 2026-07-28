#!/usr/bin/env bash
# tests/hooks/test_frontdoor_nudge_session_start.sh
#
# Verifies hooks/frontdoor-nudge-session-start.sh (x-40c4): the SessionStart
# reminder to install the Rust `fno` front door. It must go SILENT when `fno` on
# PATH answers a mux-only verb (the Rust front door is active), and print the
# one-line reminder when `fno` is absent or is the Python `fno-py` (no `mux`
# subcommand).
#
# Isolation: a FAKE `fno` is placed first on PATH per case, so no real mux is
# probed. Run: bash tests/hooks/test_frontdoor_nudge_session_start.sh

set -uo pipefail

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

log "all cases passed"
