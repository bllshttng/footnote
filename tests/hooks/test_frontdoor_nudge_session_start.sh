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

# A minimal PATH that still resolves the coreutils the hook needs (timeout, etc.)
# but never the real `fno`.
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

log "all cases passed"
