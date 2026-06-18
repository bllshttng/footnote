#!/usr/bin/env bash
# Smoke test: abi-watch.sh bypass logic (Task 5.1)
# AC1-HP: _detect_state returns "idle"           -> _spawn_drain called once (marker written)
# AC2-ERR: _detect_state returns "target_active"  -> _spawn_drain NOT called, log has "bypassed: target_active"
# AC4-EDGE: _detect_state returns "interactive_active" -> _spawn_drain NOT called, log has "bypassed: interactive_active"
# AC4-EDGE-2: debounce is fswatch-mediated; not unit-tested here.
#
# Strategy: extract _log / _on_change from the daemon via awk (same pattern as
# test_stop_hook_wake_log.sh), override _detect_state and _spawn_drain with stubs,
# then call _on_change.  No fswatch, no real claude invocation, no network.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DAEMON="$REPO_ROOT/scripts/abi-watch.sh"

PASS=0
FAIL=0

_fail() {
  echo "FAIL: $*" >&2
  FAIL=$((FAIL + 1))
}

_pass() {
  PASS=$((PASS + 1))
}

# ---------------------------------------------------------------------------
# _run_on_change <tmpdir> <state_value>
#   Runs _on_change in an isolated subshell with:
#     - _detect_state stubbed to echo <state_value>
#     - _spawn_drain stubbed to touch <tmpdir>/spawn_called
#   LOG and REPO_ROOT point into <tmpdir>.
# ---------------------------------------------------------------------------
_run_on_change() {
  local tmpdir="$1"
  local state="$2"
  mkdir -p "$tmpdir/.fno"

  # bash 3.2 compat: eval "$(awk ...)" not source <(awk ...)
  bash -c "
set -euo pipefail
PROJECT='test-project'
REPO_ROOT='$tmpdir'
LOG='$tmpdir/.fno/abi-watch.log'
SESSION_FILE='$tmpdir/.fno/test-project-watch-session.json'

eval \"\$(awk '/^_log\(\)/{f=1} /^_on_change\(\)/{f=1} f{print} f && /^}$/{f=0}' '$DAEMON')\"

_detect_state() { echo '$state'; }
_spawn_drain()  { touch '$tmpdir/spawn_called'; }

_on_change
" 2>/dev/null
}

# ---------------------------------------------------------------------------
# AC1-HP: idle -> _spawn_drain fires
# ---------------------------------------------------------------------------
TMP_HP=$(mktemp -d)
trap 'rm -rf "$TMP_HP"' EXIT

_run_on_change "$TMP_HP" "idle"

if [[ -f "$TMP_HP/spawn_called" ]]; then
  _pass
else
  _fail "AC1-HP: _spawn_drain was not called when state=idle"
fi

# ---------------------------------------------------------------------------
# AC2-ERR: target_active -> no spawn, log says "bypassed: target_active"
# ---------------------------------------------------------------------------
TMP_ERR=$(mktemp -d)
trap 'rm -rf "$TMP_ERR"' EXIT

_run_on_change "$TMP_ERR" "target_active"

if [[ -f "$TMP_ERR/spawn_called" ]]; then
  _fail "AC2-ERR: _spawn_drain was called when state=target_active"
elif [[ -f "$TMP_ERR/.fno/abi-watch.log" ]] && grep -q "bypassed: target_active" "$TMP_ERR/.fno/abi-watch.log"; then
  _pass
else
  _fail "AC2-ERR: log does not contain 'bypassed: target_active' (log: $(cat "$TMP_ERR/.fno/abi-watch.log" 2>/dev/null || echo '<missing>'))"
fi

# ---------------------------------------------------------------------------
# AC4-EDGE: interactive_active -> no spawn, log says "bypassed: interactive_active"
# ---------------------------------------------------------------------------
TMP_EDGE=$(mktemp -d)
trap 'rm -rf "$TMP_EDGE"' EXIT

_run_on_change "$TMP_EDGE" "interactive_active"

if [[ -f "$TMP_EDGE/spawn_called" ]]; then
  _fail "AC4-EDGE: _spawn_drain was called when state=interactive_active"
elif [[ -f "$TMP_EDGE/.fno/abi-watch.log" ]] && grep -q "bypassed: interactive_active" "$TMP_EDGE/.fno/abi-watch.log"; then
  _pass
else
  _fail "AC4-EDGE: log does not contain 'bypassed: interactive_active' (log: $(cat "$TMP_EDGE/.fno/abi-watch.log" 2>/dev/null || echo '<missing>'))"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
TOTAL=$((PASS + FAIL))
if [[ "$FAIL" -gt 0 ]]; then
  echo "FAIL ($PASS/$TOTAL passed)" >&2
  exit 1
fi
echo "OK ($PASS/$TOTAL)"
