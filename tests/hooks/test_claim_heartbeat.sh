#!/usr/bin/env bash
# Test suite for hooks/claim-heartbeat.sh (x-a166, Facet A).
#
# The heartbeat renews this session's node:<id> claim TTL while the owning
# session is actively working, gated on being the recorded holder and throttled
# to at most once per window. It must never block a tool call.
#
# Tests (stubbed `fno` on PATH):
#   T1  AC3-HP    holder == us, aged stamp   -> `fno claim refresh` is called
#   T2  AC3-EDGE  holder == other session    -> refresh NOT called
#   T3  AC3-ERR   refresh returns non-zero   -> hook still exits 0
#   T4  AC3-UI    fresh stamp (throttled)    -> exits 0, `fno claim status` NOT called
#   T5           no manifest                 -> exits 0, `fno` never called
#   T6  AC3-UI    not-holder no-op is silent -> no stdout

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/claim-heartbeat.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[heartbeat] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[heartbeat] FAIL: %s\n' "$*" >&2; }

[[ -f "$HOOK" ]] || { fail "hook not found at $HOOK"; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "[heartbeat] SKIP: jq not on PATH"; exit 77; }

# setup_env: build a tmp project with a manifest + a stubbed `fno` on PATH.
# Env knobs read by the stub: STUB_HOLDER, STUB_REFRESH_RC. Every `fno` call is
# appended to $CALLLOG. Sets: TMP_DIR CWD CALLLOG (and prepends the stub to PATH).
setup_env() {
  TMP_DIR="$(mktemp -d)"
  CWD="${TMP_DIR}/proj"
  mkdir -p "${CWD}/.fno"
  CALLLOG="${TMP_DIR}/fno-calls.log"
  : > "$CALLLOG"

  cat > "${CWD}/.fno/target-state.md" <<'EOF'
---
session_id: 20260707T203700Z-cl55246-f3fe72
claude_session_id: 182b29c8-owner-uuid
plan_path: ""
---
# Mission
graph_node_id: x-a166
EOF

  local bindir="${TMP_DIR}/bin"
  mkdir -p "$bindir"
  cat > "${bindir}/fno" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${CALLLOG}"
case "\$1 \$2" in
  "claim status")
    printf '{"holder": "%s"}\n' "\${STUB_HOLDER:-}"
    ;;
  "claim refresh")
    exit "\${STUB_REFRESH_RC:-0}"
    ;;
esac
exit 0
EOF
  chmod +x "${bindir}/fno"
  export PATH="${bindir}:${PATH}"
}

teardown_env() { rm -rf "$TMP_DIR"; unset STUB_HOLDER STUB_REFRESH_RC; }

run_hook() {
  # No throttle stamp is written by the harness unless a test does so; a fresh
  # tmp project has none, so the throttle gate passes. No session_id on stdin,
  # so the identity gate fails open and the holder gate is what is exercised.
  printf '{"cwd": "%s"}' "$CWD" | bash "$HOOK"
}

# Like run_hook but with a Claude session uuid on stdin, so the identity gate
# (current session vs manifest claude_session_id) is exercised. $1 = uuid.
run_hook_sid() {
  printf '{"cwd": "%s", "session_id": "%s"}' "$CWD" "$1" | bash "$HOOK"
}

# ── T1: AC3-HP - we hold the claim -> refresh is issued ──────────────────────
setup_env
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
run_hook >/dev/null 2>&1
if grep -q "claim refresh node:x-a166 --holder target-session:20260707T203700Z-cl55246-f3fe72 --ttl 2h" "$CALLLOG"; then
  pass "T1 holder==us issues claim refresh with an explicit ttl"
else
  fail "T1 expected a claim refresh call; got: $(cat "$CALLLOG")"
fi
teardown_env

# ── T2: AC3-EDGE - a different holder -> NO refresh ──────────────────────────
setup_env
export STUB_HOLDER="target-session:some-other-session"
run_hook >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T2 refreshed a claim held by another session (split-brain)"
else
  pass "T2 not-holder does not refresh"
fi
teardown_env

# ── T3: AC3-ERR - refresh fails -> hook still exits 0 ────────────────────────
setup_env
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
export STUB_REFRESH_RC=1
run_hook >/dev/null 2>&1; rc=$?
if [[ "$rc" -eq 0 ]]; then
  pass "T3 refresh failure exits 0 (never blocks the tool call)"
else
  fail "T3 hook exited $rc on refresh failure (must be 0)"
fi
teardown_env

# ── T4: AC3-UI - fresh throttle stamp -> no fno call at all ──────────────────
setup_env
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
touch "${CWD}/.fno/.claim-heartbeat.stamp"   # fresh -> within throttle window
run_hook >/dev/null 2>&1
if [[ -s "$CALLLOG" ]]; then
  fail "T4 throttled call still shelled fno: $(cat "$CALLLOG")"
else
  pass "T4 fresh stamp throttles (no fno call)"
fi
teardown_env

# ── T5: no manifest -> exit 0, fno never called ──────────────────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
run_hook >/dev/null 2>&1; rc=$?
if [[ "$rc" -eq 0 && ! -s "$CALLLOG" ]]; then
  pass "T5 no manifest -> exit 0, no fno call"
else
  fail "T5 rc=$rc calls=$(cat "$CALLLOG")"
fi
teardown_env

# ── T6: AC3-UI - not-holder no-op prints nothing to stdout ───────────────────
setup_env
export STUB_HOLDER="target-session:some-other-session"
out="$(run_hook 2>/dev/null)"
if [[ -z "$out" ]]; then
  pass "T6 not-holder no-op is silent on stdout"
else
  fail "T6 no-op printed to stdout: [$out]"
fi
teardown_env

# ── T7: identity match - current session IS the manifest owner -> refresh ────
setup_env
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166" "$CALLLOG"; then
  pass "T7 stdin session_id == manifest claude_session_id -> refresh"
else
  fail "T7 owner session did not refresh; calls: $(cat "$CALLLOG")"
fi
teardown_env

# ── T8: codex P1 - a different session on a stale manifest -> NO refresh ──────
setup_env
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"  # stale manifest's holder
run_hook_sid "some-other-live-uuid" >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T8 revived a dead owner's claim from a different session (codex P1)"
else
  pass "T8 different session on a stale manifest does not refresh"
fi
teardown_env

echo "[heartbeat] ${PASS} passed, ${FAIL} failed"
[[ "$FAIL" -eq 0 ]]
