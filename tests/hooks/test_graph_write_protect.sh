#!/usr/bin/env bash
# test_graph_write_protect.sh - smoke tests for the forbidden-surface guard
# (x-4c48: close the Bash bypass, fail-closed parse, general manifest immutability).
#
# Each case pipes a PreToolUse payload to hooks/graph-write-protect.sh and
# asserts the emitted decision (block|approve). Self-contained; needs only bash
# + the guard. jq is used to read the decision when present, with a grep fallback.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GUARD="${REPO_ROOT}/hooks/graph-write-protect.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[gwp] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[gwp] FAIL: %s\n' "$*" >&2; }

[[ -f "$GUARD" ]] || { fail "guard not found at $GUARD"; exit 1; }

# decision_of PAYLOAD -> prints "block" or "approve"
decision_of() {
  local out; out=$(printf '%s' "$1" | bash "$GUARD" 2>/dev/null)
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$out" | jq -r '.decision // "MISSING"' 2>/dev/null
  else
    if printf '%s' "$out" | grep -q '"block"'; then echo block
    elif printf '%s' "$out" | grep -q '"approve"'; then echo approve
    else echo MISSING; fi
  fi
}

# expect NAME EXPECTED PAYLOAD
expect() {
  local name="$1" want="$2" payload="$3" got
  got=$(decision_of "$payload")
  if [[ "$got" == "$want" ]]; then pass "$name ($got)"; else fail "$name: want $want got $got"; fi
}

# expect with a custom PATH (for jq/python3-absence tests)
expect_env() {
  local name="$1" want="$2" env_path="$3" payload="$4" out got
  out=$(printf '%s' "$payload" | PATH="$env_path" bash "$GUARD" 2>/dev/null)
  if printf '%s' "$out" | grep -q "\"$want\""; then pass "$name ($want)"; else fail "$name: want $want, got: $out"; fi
}

# ── T0: syntax ────────────────────────────────────────────────────────────────
if bash -n "$GUARD" 2>/dev/null; then pass "T0: bash -n syntax"; else fail "T0: syntax error"; fi

# ── AC1-HP: Bash in-place jq write to graph.json is blocked ───────────────────
expect "AC1-HP: jq -i graph.json" block \
  '{"tool_name":"Bash","tool_input":{"command":"jq '\''.x=1'\'' -i ~/.fno/graph.json"}}'

# ── AC2-HP: Bash redirect to target-state.md is blocked ───────────────────────
expect "AC2-HP: >> target-state.md" block \
  '{"tool_name":"Bash","tool_input":{"command":"echo '\''auto_merge_approved: true'\'' >> .fno/target-state.md"}}'

# ── AC3-HP: Edit to target-state.md blocked (unconditional immutability) ──────
expect "AC3-HP: Edit target-state.md" block \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/.fno/target-state.md","old_string":"a","new_string":"b"}}'

# ── AC1-ERR: fail closed when jq AND python3 absent, Edit targets graph.json ──
# Build a PATH with only the coreutils dir(s) the guard body needs (printf/cat are
# builtins; grep/sed used only in blocked-Bash paths). We deliberately omit jq
# and python3.
_STUB=$(mktemp -d)
# provide bash (the guard is invoked as `bash GUARD`, so bash must resolve under
# the restricted PATH) + the core tools the guard calls, but NOT jq/python3.
for b in bash cat dirname mktemp date grep sed; do
  src=$(command -v "$b" 2>/dev/null) && ln -sf "$src" "$_STUB/$b" 2>/dev/null || true
done
expect_env "AC1-ERR: jq+python3 absent -> block" block "$_STUB" \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/.fno/graph.json","old_string":"a","new_string":"b"}}'
# And with python3 present, the same payload parses and blocks normally.
PY_DIR=$(dirname "$(command -v python3)")
expect_env "AC1-ERR: python3 fallback -> block" block "$_STUB:$PY_DIR" \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/.fno/graph.json","old_string":"a","new_string":"b"}}'
rm -rf "$_STUB"

# ── AC2-ERR: malformed payload, no protected token -> approve ─────────────────
expect "AC2-ERR: malformed no-token approve" approve \
  'this is { not valid json at all'

# ── AC1-EDGE: file that MENTIONS the path is editable (target is AGENTS.md) ────
expect "AC1-EDGE: Edit AGENTS.md mentions path" approve \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/AGENTS.md","old_string":"x","new_string":"see ~/.fno/graph.json here"}}'

# ── AC2-EDGE: legitimate fno verb approved (no path+redirect) ─────────────────
expect "AC2-EDGE: fno target init" approve \
  '{"tool_name":"Bash","tool_input":{"command":"fno target init --node x-4c48"}}'
# fno state set carrying --path .fno/target-state.md is still a named verb, no write-op
expect "AC2-EDGE: fno state set --path" approve \
  '{"tool_name":"Bash","tool_input":{"command":"fno state set --field plan_path --value /p.md --path .fno/target-state.md"}}'

# ── AC3-EDGE: read of a protected file is allowed ─────────────────────────────
expect "AC3-EDGE: cat graph.json (read)" approve \
  '{"tool_name":"Bash","tool_input":{"command":"cat ~/.fno/graph.json | jq .nodes"}}'
# echo that names the path but redirects elsewhere -> approve (bare mention)
expect "AC3-EDGE: echo names path, writes notes.md" approve \
  '{"tool_name":"Bash","tool_input":{"command":"echo \"see .fno/graph.json\" >> notes.md"}}'

# ── Extra: Edit to graph.json blocked (baseline) ──────────────────────────────
expect "Edit graph.json blocked" block \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/.fno/graph.json","old_string":"a","new_string":"b"}}'
# ── Extra: sed -i on target-state.md blocked ──────────────────────────────────
expect "sed -i target-state.md blocked" block \
  '{"tool_name":"Bash","tool_input":{"command":"sed -i s/false/true/ ~/.fno/target-state.md"}}'
# ── Extra: fixture-path graph.json editable ───────────────────────────────────
expect "fixture graph.json editable" approve \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/tests/fixtures/.fno/graph.json","old_string":"a","new_string":"b"}}'
# ── Extra: unrelated Edit (no token) approved ─────────────────────────────────
expect "unrelated Edit approved" approve \
  '{"tool_name":"Edit","tool_input":{"file_path":"/proj/src/main.py","old_string":"a","new_string":"b"}}'
# ── Extra: cp ONTO graph.json (destination) blocked; cp FROM graph.json approved
expect "cp onto graph.json blocked" block \
  '{"tool_name":"Bash","tool_input":{"command":"cp /tmp/forged.json ~/.fno/graph.json"}}'
expect "cp from graph.json (backup read) approved" approve \
  '{"tool_name":"Bash","tool_input":{"command":"cp ~/.fno/graph.json /tmp/backup.json"}}'

# ── AC1-UI: single JSON verdict + exit 0 ──────────────────────────────────────
_OUT=$(printf '%s' '{"tool_name":"Edit","tool_input":{"file_path":"/proj/.fno/graph.json"}}' | bash "$GUARD"; echo "RC=$?")
_RC=$(printf '%s' "$_OUT" | sed -n 's/.*RC=//p')
_LINES=$(printf '%s' "$_OUT" | grep -c '"decision"')
if [[ "$_RC" == "0" && "$_LINES" == "1" ]]; then pass "AC1-UI: single verdict, exit 0"; else fail "AC1-UI: rc=$_RC lines=$_LINES"; fi

echo ""
printf '[gwp] RESULTS: %d passed, %d failed\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
