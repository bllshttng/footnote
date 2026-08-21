#!/usr/bin/env bash
# Test suite for hooks/claim-heartbeat.sh (x-a166, Facet A).
#
# The heartbeat renews this session's node:<id> claim TTL while the owning
# session is actively working, gated on being the recorded holder and throttled
# to at most once per window. It must never block a tool call.
#
# Tests (stubbed `fno` on PATH):
#   T1  AC3-HP    holder == us, aged stamp   -> `fno agents claim refresh` is called
#   T2  AC3-EDGE  holder == other session    -> refresh NOT called
#   T3  AC3-ERR   refresh returns non-zero   -> hook still exits 0
#   T4  AC3-UI    fresh stamp (throttled)    -> exits 0, `fno agents claim status` NOT called
#   T5           no manifest                 -> stamps activity, `fno` never called
#   T6  AC3-UI    not-holder no-op is silent -> no stdout
#   T7/T8         Claude owner identity match/mismatch
#   T9/T10        Codex thread identity match/mismatch

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
# Env knobs read by the stub: STUB_HOLDER/STATE/STATUS_JSON, STUB_REFRESH_RC,
# STUB_BIND_RC/OUTPUT. Every `fno` call is
# appended to $CALLLOG. Sets: TMP_DIR CWD CALLLOG (and prepends the stub to PATH).
setup_env() {
  TMP_DIR="$(mktemp -d)"
  CWD="${TMP_DIR}/proj"
  mkdir -p "${CWD}/.fno"
  git init -q "$CWD"
  LIVE_DIR="$(git -C "$CWD" rev-parse --absolute-git-dir)/fno/live"
  CALLLOG="${TMP_DIR}/fno-calls.log"
  : > "$CALLLOG"

  cat > "${CWD}/.fno/target-state.md" <<'EOF'
---
session_id: 20260707T203700Z-cl55246-f3fe72
claude_session_id: 182b29c8-owner-uuid
codex_thread_id: 019f48e4-owner-thread
plan_path: ""
---
# Mission
graph_node_id: x-a166
target_claim_holder: "target-session:20260707T203700Z-cl55246-f3fe72"
EOF

  local bindir="${TMP_DIR}/bin"
  mkdir -p "$bindir"
  cat > "${bindir}/fno" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${CALLLOG}"
if [[ "\${1:-} \${2:-}" == "agents claim" ]]; then
  shift
fi
case "\$1 \$2" in
  "claim status")
    if [[ -n "\${STUB_STATUS_JSON+x}" ]]; then
      printf '%s' "\$STUB_STATUS_JSON"
    else
      if [[ -f "${TMP_DIR}/refresh-observed" ]]; then
        printf '{"holder":"%s","state":"%s","expires_at":%s}\n' \
          "\${STUB_HOLDER_AFTER:-\${STUB_HOLDER:-}}" \
          "\${STUB_STATE_AFTER:-\${STUB_STATE:-live}}" \
          "\${STUB_EXPIRES_AFTER:-200}"
      else
        printf '{"holder":"%s","state":"%s","expires_at":%s}\n' \
          "\${STUB_HOLDER:-}" "\${STUB_STATE:-live}" "\${STUB_EXPIRES_BEFORE:-100}"
      fi
    fi
    exit "\${STUB_STATUS_RC:-0}"
    ;;
  "claim refresh")
    [[ "\${STUB_REFRESH_RC:-0}" -eq 0 ]] && touch "${TMP_DIR}/refresh-observed"
    exit "\${STUB_REFRESH_RC:-0}"
    ;;
  "do pr")
    [[ "\${3:-}" == "bind-created" ]] || exit 0
    printf '%s\n' "\${STUB_BIND_OUTPUT:-{\"outcome\":\"bound\"}}"
    [[ -n "\${STUB_BIND_SLEEP:-}" ]] && sleep "\$STUB_BIND_SLEEP"
    exit "\${STUB_BIND_RC:-0}"
    ;;
  "pr bind-created")
    echo "deprecated pr root reached" >&2
    exit 2
    ;;
esac
exit 0
EOF
  chmod +x "${bindir}/fno"
  export PATH="${bindir}:${PATH}"
}

teardown_env() {
  rm -rf "$TMP_DIR"
  unset STUB_HOLDER STUB_STATE STUB_STATUS_JSON STUB_STATUS_RC STUB_REFRESH_RC
  unset STUB_HOLDER_AFTER STUB_STATE_AFTER STUB_EXPIRES_BEFORE STUB_EXPIRES_AFTER
  unset STUB_BIND_OUTPUT STUB_BIND_RC STUB_BIND_SLEEP
}

mtime_of() {
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null
}

run_hook() {
  # No throttle stamp is written by the harness unless a test does so; a fresh
  # tmp project has none, so the throttle gate passes. No session_id on stdin,
  # so the identity gate fails open and the holder gate is what is exercised.
  printf '{"cwd": "%s"}' "$CWD" | CODEX_THREAD_ID= bash "$HOOK"
}

# Like run_hook but with a Claude session uuid on stdin, so the identity gate
# (current session vs manifest claude_session_id) is exercised. $1 = uuid.
run_hook_sid() {
  printf '{"cwd": "%s", "session_id": "%s"}' "$CWD" "$1" \
    | CODEX_THREAD_ID= bash "$HOOK"
}

# Like run_hook but with a Codex thread identity in the hook environment.
# $1 = thread id.
run_hook_codex() {
  printf '{"cwd": "%s"}' "$CWD" | CODEX_THREAD_ID="$1" bash "$HOOK"
}

# Codex lifecycle payloads also carry the thread as session_id. The heartbeat
# must route that field through the Codex identity gate, not compare it with the
# manifest's unrelated Claude identity.
run_hook_codex_sid() {
  printf '{"cwd": "%s", "session_id": "%s"}' "$CWD" "$1" \
    | CODEX_THREAD_ID="$1" bash "$HOOK"
}

drop_manifest_codex_identity() {
  awk '!/^[[:space:]]*codex_thread_id:/' "${CWD}/.fno/target-state.md" \
    > "${CWD}/.fno/target-state.legacy.md"
  mv "${CWD}/.fno/target-state.legacy.md" "${CWD}/.fno/target-state.md"
}

set_manifest_claude_identity_null() {
  awk '
    /^[[:space:]]*claude_session_id:/ { print "claude_session_id: null"; next }
    { print }
  ' "${CWD}/.fno/target-state.md" > "${CWD}/.fno/target-state.codex.md"
  mv "${CWD}/.fno/target-state.codex.md" "${CWD}/.fno/target-state.md"
}

set_manifest_codex_claim_holder() {
  awk '
    /^[[:space:]]*target_claim_holder:/ {
      print "target_claim_holder: \"target-session:019f48e4-owner-thread\""; next
    }
    { print }
  ' "${CWD}/.fno/target-state.md" > "${CWD}/.fno/target-state.codex-holder.md"
  mv "${CWD}/.fno/target-state.codex-holder.md" "${CWD}/.fno/target-state.md"
}

run_hook_codex_plugin_sid() {
  printf '{"cwd": "%s", "session_id": "%s"}' "$CWD" "$1" \
    | CODEX_THREAD_ID= CODEX_PLUGIN_ROOT=/opt/codex-plugin FNO_PLATFORM=codex \
      bash "$HOOK"
}

run_hook_codex_plugin_no_sid() {
  printf '{"cwd": "%s"}' "$CWD" \
    | CODEX_THREAD_ID= CODEX_PLUGIN_ROOT=/opt/codex-plugin FNO_PLATFORM=codex \
      bash "$HOOK"
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

# ── T5: no manifest -> stamp activity, fno never called ─────────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export STUB_HOLDER="target-session:20260707T203700Z-cl55246-f3fe72"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1; rc=$?
if [[ "$rc" -eq 0 && ! -s "$CALLLOG" \
      && -f "${LIVE_DIR}/182b29c8-owner-uuid" ]]; then
  pass "T5 no manifest -> activity stamped, no fno call"
else
  fail "T5 rc=$rc calls=$(cat "$CALLLOG") stamp=$(test -f "${LIVE_DIR}/182b29c8-owner-uuid" && echo yes || echo no)"
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

# ── T9: Codex identity match -> refresh ──────────────────────────────────────
setup_env
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex "019f48e4-owner-thread" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166" "$CALLLOG"; then
  pass "T9 CODEX_THREAD_ID == manifest codex_thread_id -> refresh"
else
  fail "T9 Codex owner thread did not refresh; calls: $(cat "$CALLLOG")"
fi
teardown_env

# ── T10: Codex identity mismatch -> NO refresh ───────────────────────────────
setup_env
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex "019f48e4-different-thread" >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T10 revived a Codex claim from a different thread"
else
  pass "T10 different Codex thread on a stale manifest does not refresh"
fi
teardown_env

# ── T11: Realistic Codex payload must not trip the Claude guard ───────────
setup_env
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex_sid "019f48e4-owner-thread" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166" "$CALLLOG"; then
  pass "T11 Codex stdin session_id routes through Codex ownership"
else
  fail "T11 Codex session_id was rejected by the Claude identity guard"
fi
teardown_env

# ── T12: Current Codex + legacy manifest must fail closed ───────────────
setup_env
drop_manifest_codex_identity
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex_sid "019f48e4-owner-thread" >/dev/null 2>&1
if [[ -s "$CALLLOG" ]]; then
  fail "T12 legacy manifest reached claim status/refresh under current Codex identity"
else
  pass "T12 legacy manifest fails closed before refreshing a generic old holder"
fi
teardown_env

# ── T13: Codex stdin fallback mismatch must fail closed ─────────────────────
setup_env
set_manifest_claude_identity_null
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex_plugin_sid "019f48e4-different-thread" >/dev/null 2>&1
if [[ -s "$CALLLOG" ]]; then
  fail "T13 Codex stdin fallback mismatch refreshed a stale foreign holder"
else
  pass "T13 Codex stdin session fallback mismatch fails closed"
fi
teardown_env

# ── T14: Codex stdin fallback match remains refreshable ──────────────────────
setup_env
set_manifest_claude_identity_null
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex_plugin_sid "019f48e4-owner-thread" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166" "$CALLLOG"; then
  pass "T14 Codex stdin session fallback match refreshes owner"
else
  fail "T14 Codex stdin session fallback match did not refresh"
fi
teardown_env

# ── T15: Identified Codex with no usable identity fails closed ────────────────
setup_env
set_manifest_claude_identity_null
set_manifest_codex_claim_holder
export STUB_HOLDER="target-session:019f48e4-owner-thread"
run_hook_codex_plugin_no_sid >/dev/null 2>&1
if [[ -s "$CALLLOG" ]]; then
  fail "T15 identity-less Codex hook refreshed a stale foreign holder"
else
  pass "T15 identity-less Codex hook fails closed"
fi
teardown_env

# ── T16: Explicit target session override remains refreshable ────────────────
setup_env
awk '
  /^[[:space:]]*session_id:/ { print "session_id: explicit-target-session"; next }
  /^[[:space:]]*target_claim_holder:/ {
    print "target_claim_holder: \"target-session:explicit-target-session\""; next
  }
  { print }
' "${CWD}/.fno/target-state.md" > "${CWD}/.fno/target-state.explicit.md"
mv "${CWD}/.fno/target-state.explicit.md" "${CWD}/.fno/target-state.md"
export STUB_HOLDER="target-session:explicit-target-session"
run_hook_codex "019f48e4-owner-thread" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166 --holder target-session:explicit-target-session" "$CALLLOG"; then
  pass "T16 explicit TARGET_SESSION_ID claim owner remains refreshable"
else
  fail "T16 explicit target session claim did not refresh"
fi
teardown_env

# ── T17: activity stamp is written before the claim-holder gate ──────────────
setup_env
export STUB_HOLDER="target-session:some-other-session"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
if [[ -f "${LIVE_DIR}/182b29c8-owner-uuid" ]] \
    && ! grep -q "claim refresh" "$CALLLOG"; then
  pass "T17 current session stamps even when it is not the claim holder"
else
  fail "T17 stamp must precede the holder gate; calls: $(cat "$CALLLOG")"
fi
teardown_env

# ── T18: fresh activity stamp takes the cheap stat-and-exit path ─────────────
setup_env
export STUB_HOLDER="target-session:some-other-session"
mkdir -p "$LIVE_DIR"
touch -t 203001010000 "${LIVE_DIR}/182b29c8-owner-uuid"
before="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
after="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
if [[ "$after" == "$before" ]]; then
  pass "T18 fresh activity stamp is not rewritten"
else
  fail "T18 activity throttle rewrote a fresh stamp: before=$before after=$after"
fi
teardown_env

# ── T19: aged activity stamp advances after the throttle window ─────────────
setup_env
export STUB_HOLDER="target-session:some-other-session"
mkdir -p "$LIVE_DIR"
touch -t 202001010000 "${LIVE_DIR}/182b29c8-owner-uuid"
before="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
after="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
if (( after > before )); then
  pass "T19 aged activity stamp advances"
else
  fail "T19 aged activity stamp did not advance: before=$before after=$after"
fi
teardown_env

# ── T20: GNU stat's successful textual -f output never reaches arithmetic ───
setup_env
export STUB_HOLDER="target-session:some-other-session"
mkdir -p "$LIVE_DIR"
touch -t 202001010000 "${LIVE_DIR}/182b29c8-owner-uuid"
before="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
gnu_stat_bin="${TMP_DIR}/gnu-stat-bin"
mkdir -p "$gnu_stat_bin"
cat >"$gnu_stat_bin/stat" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == -c && "${2:-}" == %Y ]]; then
  echo 1
  exit 0
fi
if [[ "${1:-}" == -f ]]; then
  printf '  File: "fake"\n    ID: 0\n'
  exit 0
fi
exit 1
EOF
chmod +x "$gnu_stat_bin/stat"
PATH="$gnu_stat_bin:$PATH" run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
rc=$?
after="$(mtime_of "${LIVE_DIR}/182b29c8-owner-uuid")"
if [[ "$rc" -eq 0 ]] && (( after > before )); then
  pass "T20 GNU stat output cannot break activity-stamp arithmetic"
else
  fail "T20 GNU stat compatibility: rc=$rc before=$before after=$after"
fi
teardown_env

# ── T21: pre-init spawn handover stays alive on PostToolUse ─────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166"
export FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="$FNO_NODE_CLAIM_HOLDER"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166 --holder spawn-handover:build-x-a166 --ttl 15m" "$CALLLOG" \
    && [[ ! -e "${CWD}/.fno/target-state.md" ]]; then
  pass "T21 pre-init handover refreshes its exact 15m claim without creating a manifest"
else
  fail "T21 expected pre-init handover refresh only; calls: $(cat "$CALLLOG")"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T22: pre-init handover never refreshes another holder ───────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166"
export FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="spawn-handover:some-other-worker"
run_hook_sid "182b29c8-owner-uuid" >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T22 pre-init handover refreshed another holder"
elif [[ ! -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T22 verified foreign holder did not throttle the positive no-op"
else
  pass "T22 pre-init handover mismatch does not acquire or steal and throttles"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T23: successful raw gh pr create invokes the branch binder ──────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"182b29c8-owner-uuid",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stdout:"https://github.com/acme/widgets/pull/42\n"}}')"
printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" >/dev/null 2>&1
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
if [[ "$bind_calls" -eq 1 ]] \
    && grep -q "do pr bind-created --url https://github.com/acme/widgets/pull/42" "$CALLLOG" \
    && grep -q -- "--owner 182b29c8-owner-uuid" "$CALLLOG"; then
  pass "T23 successful raw gh pr create invokes the binder once"
else
  fail "T23 expected one bind-created call; calls: $(cat "$CALLLOG")"
fi
teardown_env

# ── T24: failed or ambiguous gh output never invokes the binder ───────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"182b29c8-owner-uuid",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stderr:"failed: https://github.com/acme/widgets/pull/42 and https://github.com/acme/widgets/pull/43"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"
if grep -q "do pr bind-created" "$CALLLOG"; then
  fail "T24 ambiguous output invoked the binder"
elif [[ "$err" != *"ambiguous PR URLs"* ]]; then
  fail "T24 ambiguous output emitted no diagnostic: [$err]"
else
  pass "T24 ambiguous output diagnoses and mutates nothing"
fi
teardown_env

# ── T25: a nonzero gh result cannot bind even when it mentions one URL ─────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"182b29c8-owner-uuid",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{exit_code:1,stderr:"failed after reserving https://github.com/acme/widgets/pull/42"}}')"
printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" >/dev/null 2>&1
if grep -q "do pr bind-created" "$CALLLOG"; then
  fail "T25 failed gh result invoked the binder"
else
  pass "T25 failed gh result mutates nothing"
fi
teardown_env

# ── T26: Codex exec payload reads tool_input.cmd ──────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"019f-codex",tool_name:"exec_command",tool_input:{cmd:"gh pr create --fill"},tool_response:{exit_code:0,stdout:"https://github.com/acme/widgets/pull/42\n"}}')"
printf '%s' "$payload" | FNO_PLATFORM=codex CODEX_THREAD_ID="019f-codex" bash "$HOOK" >/dev/null 2>&1
if grep -q "do pr bind-created --url https://github.com/acme/widgets/pull/42" "$CALLLOG"; then
  pass "T26 Codex tool_input.cmd invokes the binder"
else
  fail "T26 Codex cmd payload did not invoke binder; calls: $(cat "$CALLLOG")"
fi
teardown_env

# ── T27: compound shell command is not attributable ──────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:"echo prep && gh pr create --fill"},tool_response:{stdout:"https://github.com/acme/widgets/pull/42\n"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"
if grep -q "do pr bind-created" "$CALLLOG"; then
  fail "T27 compound command invoked binder"
elif [[ "$err" != *"not attributable"* ]]; then
  fail "T27 compound refusal emitted no diagnostic: [$err]"
else
  pass "T27 compound command diagnoses and does not bind"
fi
teardown_env

# ── T28: exact holder in a non-live state never refreshes ────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="$FNO_NODE_CLAIM_HOLDER" STUB_STATE="stale"
run_hook_sid "owner" >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T28 stale exact-holder claim refreshed"
elif [[ ! -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T28 verified stale state did not throttle its positive no-op"
else
  pass "T28 non-live exact holder never refreshes"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T29: malformed status stays due and diagnoses ───────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_STATUS_JSON='{"holder":"spawn-handover:build-x-a166"}'
err="$(run_hook_sid "owner" 2>&1 >/dev/null)"
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T29 status without a live state refreshed"
elif [[ -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T29 malformed status incorrectly throttled the retry"
elif [[ "$err" != *"status unreadable"* ]]; then
  fail "T29 malformed status emitted no diagnostic: [$err]"
else
  pass "T29 malformed status remains due with a diagnostic"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T30: refresh failure stays due and diagnoses ────────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="$FNO_NODE_CLAIM_HOLDER" STUB_STATE="live" STUB_REFRESH_RC=1
err="$(run_hook_sid "owner" 2>&1 >/dev/null)"
if [[ -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T30 failed refresh incorrectly throttled the retry"
elif [[ "$err" != *"refresh failed"* ]]; then
  fail "T30 failed refresh emitted no diagnostic: [$err]"
else
  pass "T30 refresh failure remains due with a diagnostic"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T31/T32: no URL and binder refusal diagnose but remain nonfatal ──────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stdout:"created"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"; rc=$?
if [[ "$rc" -eq 0 && "$err" == *"no PR URL"* ]]; then
  pass "T31 no-URL observation is nonfatal and diagnostic"
else
  fail "T31 no-URL rc=$rc diagnostic=[$err]"
fi
export STUB_BIND_RC=1 STUB_BIND_OUTPUT='{"outcome":"refused","refusal":"branch unknown"}'
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"; rc=$?
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
manual="fno do pr bind-created --url https://github.com/acme/widgets/pull/42 --repo $CWD --owner owner"
if [[ "$rc" -eq 0 && "$bind_calls" -eq 2 \
      && "$err" == *"branch unknown"* && "$err" == *"$manual"* ]]; then
  pass "T32 binder refusal retries once, then prints exact manual recovery"
else
  fail "T32 refusal rc=$rc calls=$bind_calls diagnostic=[$err]"
fi
teardown_env

# ── T33: real hook -> bind-created -> graph reread leaves ready queue ───────
REAL_TMP="$(mktemp -d)"
REAL_REPO="$REAL_TMP/repo"
REAL_GRAPH="$REAL_TMP/graph.json"
REAL_CONFIG="$REAL_TMP/config.toml"
REAL_BIN="$REAL_TMP/bin"
mkdir -p "$REAL_REPO/.fno" "$REAL_BIN"
git init -q "$REAL_REPO"
git -C "$REAL_REPO" checkout -q -b feature/x-a166-real
printf '{"entries":[{"id":"x-a166","status":"ready","title":"live node","priority":"p1","type":"feature","blocked_by":[],"locked_by":null,"session_id":null,"pr_number":null,"pr_url":null}]}\n' > "$REAL_GRAPH"
printf '[paths]\ngraph_json = "%s"\n' "$REAL_GRAPH" > "$REAL_CONFIG"
cat > "$REAL_BIN/fno" <<EOF
#!/usr/bin/env bash
exec uv run --project "$REPO_ROOT/cli" python -m fno.cli "\$@"
EOF
chmod +x "$REAL_BIN/fno"
payload="$(jq -cn --arg cwd "$REAL_REPO" '{cwd:$cwd,session_id:"owner-session",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
err="$(printf '%s' "$payload" | PATH="$REAL_BIN:$PATH" FNO_CONFIG="$REAL_CONFIG" CODEX_THREAD_ID= CLAUDE_PLUGIN_ROOT="$REPO_ROOT" bash "$HOOK" 2>&1 >/dev/null)"; rc=$?
real_status="$(jq -r '.entries[0].status' "$REAL_GRAPH")"
if [[ "$rc" -eq 0 && "$real_status" == in_review \
      && "$(jq -r '.entries[0].pr_number' "$REAL_GRAPH")" == 42 \
      && "$(jq -r '.entries[0].locked_by' "$REAL_GRAPH")" == owner-session ]]; then
  pass "T33 real PostToolUse binds the graph and removes the node from ready"
else
  fail "T33 rc=$rc status=$real_status graph=$(cat "$REAL_GRAPH") stderr=[$err]"
fi
rm -rf "$REAL_TMP"

# ── T34: binder timeout retries once, stays bounded, and diagnoses ────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export STUB_BIND_SLEEP=10
payload="$(jq -cn --arg cwd "$CWD" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:"gh pr create --fill"},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
started="$(date +%s)"
err="$(printf '%s' "$payload" | FNO_PR_BIND_CREATED_TIMEOUT=1 CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"; rc=$?
elapsed=$(( $(date +%s) - started ))
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
manual="fno do pr bind-created --url https://github.com/acme/widgets/pull/42 --repo $CWD --owner owner"
if [[ "$rc" -eq 0 && "$bind_calls" -eq 2 && "$elapsed" -le 5 \
      && "$err" == *"timed out"* && "$err" == *"$manual"* ]]; then
  pass "T34 binder timeout retries once, stays bounded, and prints recovery"
else
  fail "T34 timeout rc=$rc calls=$bind_calls elapsed=${elapsed}s diagnostic=[$err]"
fi
teardown_env

# ── T35: a positively free claim is a throttled no-op, never a refresh ──────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_STATUS_JSON='{"key":"node:x-a166","state":"free"}'
run_hook_sid "owner" >/dev/null 2>&1
if grep -q "claim refresh" "$CALLLOG"; then
  fail "T35 missing claim was refreshed"
elif [[ ! -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T35 positive free result was not throttled"
else
  pass "T35 missing claim never refreshes and throttles the positive no-op"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T36: suspect exact-holder handover is renewable ──────────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="$FNO_NODE_CLAIM_HOLDER" STUB_STATE="suspect"
run_hook_sid "owner" >/dev/null 2>&1
if grep -q "claim refresh node:x-a166 --holder spawn-handover:build-x-a166 --ttl 15m" "$CALLLOG"; then
  pass "T36 suspect exact-holder handover refreshes after spawner exit"
else
  fail "T36 suspect exact-holder handover was not refreshed; calls: $(cat "$CALLLOG")"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

# ── T37: quoted shell punctuation belongs to gh arguments ───────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
payload="$(jq -cn --arg cwd "$CWD" --arg cmd 'gh pr create --body "literal ; & | text"' '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:$cmd},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
if [[ "$bind_calls" -eq 1 && -z "$err" ]]; then
  pass "T37 quoted shell punctuation remains one attributable gh invocation"
else
  fail "T37 quoted punctuation calls=$bind_calls diagnostic=[$err]"
fi
teardown_env

# ── T38: backslash-newline formatting remains one gh invocation ──────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
formatted_cmd="$(printf 'gh pr create \\\n  --fill')"
payload="$(jq -cn --arg cwd "$CWD" --arg cmd "$formatted_cmd" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:$cmd},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
if [[ "$bind_calls" -eq 1 && -z "$err" ]]; then
  pass "T38 backslash-newline formatting is attributable"
else
  fail "T38 multiline calls=$bind_calls diagnostic=[$err]"
fi
teardown_env

# ── T39: an unescaped newline introduces a second shell command ──────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
compound_cmd="$(printf 'gh pr create --fill\necho second-command')"
payload="$(jq -cn --arg cwd "$CWD" --arg cmd "$compound_cmd" '{cwd:$cwd,session_id:"owner",tool_name:"Bash",tool_input:{command:$cmd},tool_response:{stdout:"https://github.com/acme/widgets/pull/42"}}')"
err="$(printf '%s' "$payload" | CODEX_THREAD_ID= bash "$HOOK" 2>&1 >/dev/null)"
bind_calls="$(grep -c "do pr bind-created" "$CALLLOG" || true)"
if [[ "$bind_calls" -eq 0 && "$err" == *"not attributable"* ]]; then
  pass "T39 actual multiline compound command refuses binding"
else
  fail "T39 multiline compound calls=$bind_calls diagnostic=[$err]"
fi
teardown_env

# ── T40: exit-zero refresh without lease extension remains due ────────────
setup_env
rm -f "${CWD}/.fno/target-state.md"
export FNO_NODE="x-a166" FNO_NODE_CLAIM_HOLDER="spawn-handover:build-x-a166"
export STUB_HOLDER="$FNO_NODE_CLAIM_HOLDER" STUB_STATE="suspect"
export STUB_EXPIRES_BEFORE=100 STUB_EXPIRES_AFTER=100 STUB_STATE_AFTER="stale"
err="$(run_hook_sid "owner" 2>&1 >/dev/null)"
if [[ -f "${CWD}/.fno/.claim-handover-heartbeat.stamp" ]]; then
  fail "T40 unconfirmed exit-zero refresh incorrectly throttled later repair"
elif [[ "$err" != *"ownership-lost/refresh-not-confirmed"* ]]; then
  fail "T40 unconfirmed refresh emitted no ownership diagnostic: [$err]"
else
  pass "T40 exit-zero without positive lease extension remains due"
fi
unset FNO_NODE FNO_NODE_CLAIM_HOLDER
teardown_env

echo "[heartbeat] ${PASS} passed, ${FAIL} failed"
[[ "$FAIL" -eq 0 ]]
