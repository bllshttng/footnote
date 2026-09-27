#!/usr/bin/env bash
# test_claude_config_write_guard.sh - smoke tests for the config-dir scratch
# guard (x-2bda): a write or redirect to a file directly inside the Claude
# config dir is refused and names the job tmp dir; subdirectories and named
# harness config files stay allowed.
#
# Each case pipes a PreToolUse payload to hooks/claude-config-write-guard.sh
# and asserts the emitted decision. Self-contained; needs only bash + the
# guard. jq is used to read the decision when present, with a grep fallback.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
GUARD="${REPO_ROOT}/hooks/claude-config-write-guard.sh"

PASS=0; FAIL=0
pass() { PASS=$((PASS+1)); printf '[ccw] PASS: %s\n' "$*"; }
fail() { FAIL=$((FAIL+1)); printf '[ccw] FAIL: %s\n' "$*" >&2; }

[[ -f "$GUARD" ]] || { fail "guard not found at $GUARD"; exit 1; }

SID="72301109-aaaa-bbbb-cccc-dddddddddddd"

# decision_of PAYLOAD -> prints "block" or "approve" (empty object == allow)
decision_of() {
  local out; out=$(printf '%s' "$1" | bash "$GUARD" 2>/dev/null)
  if command -v jq >/dev/null 2>&1; then
    printf '%s' "$out" | jq -r 'if type == "object" and length == 0 then "approve" else .decision // "MISSING" end' 2>/dev/null
  else
    if printf '%s' "$out" | grep -q '"block"'; then echo block
    elif [[ "$out" == "{}" ]]; then echo approve
    else echo MISSING; fi
  fi
}

# expect NAME EXPECTED PAYLOAD
expect() {
  local name="$1" want="$2" payload="$3" got
  got=$(decision_of "$payload")
  if [[ "$got" == "$want" ]]; then pass "$name ($got)"; else fail "$name: want $want got $got"; fi
}

# ── T0: syntax ────────────────────────────────────────────────────────────────
if bash -n "$GUARD" 2>/dev/null; then pass "T0: bash -n syntax"; else fail "T0: syntax error"; fi

# ── AC1: the node's verify pair ───────────────────────────────────────────────
expect "AC1: > ~/.claude/x.out is refused" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.claude/x.out\"},\"session_id\":\"$SID\"}"
expect "AC1: > ~/.claude/jobs/<id>/tmp/x.out is allowed" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.claude/jobs/$SID/tmp/x.out\"},\"session_id\":\"$SID\"}"

# ── AC2: the measured incident shape, absolute redirect ───────────────────────
expect "AC2: absolute redirect to uvsync.out" block \
  '{"tool_name":"Bash","tool_input":{"command":"uv sync --project cli > /Users/bb16/.claude/uvsync.out"},"session_id":"'$SID'"}'
expect "AC2: >> append to agents-ci.log" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"run-agent --collect >> ~/.claude/agents-ci.log\"},\"session_id\":\"$SID\"}"
expect "AC2: \$HOME form" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"prog > \$HOME/.claude/ab.out\"},\"session_id\":\"$SID\"}"
expect "AC2: \$CLAUDE_CONFIG_DIR form" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"prog > \$CLAUDE_CONFIG_DIR/scratch.out\"},\"session_id\":\"$SID\"}"

# ── AC3: Edit and Write payloads to a top-level file ──────────────────────────
expect "AC3: Write to ~/.claude/notes.md" block \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"/Users/bb16/.claude/notes.md\",\"content\":\"x\"},\"session_id\":\"$SID\"}"
expect "AC3: Edit to a top-level dotfile" block \
  "{\"tool_name\":\"Edit\",\"tool_input\":{\"file_path\":\"/Users/bb16/.claude/.buddy copy.json\",\"old_string\":\"a\",\"new_string\":\"b\"},\"session_id\":\"$SID\"}"

# ── AC4: named harness config files and tmp files stay editable ───────────────
expect "AC4: Write to settings.json stays allowed" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"/Users/bb16/.claude/settings.json\",\"content\":\"{}\"},\"session_id\":\"$SID\"}"
expect "AC4: Write to CLAUDE.md stays allowed" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"/Users/bb16/.claude/CLAUDE.md\",\"content\":\"x\"},\"session_id\":\"$SID\"}"
expect "AC4: .claude.json.tmp stays out of scope" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.claude/.claude.json.tmp.123.abc\"},\"session_id\":\"$SID\"}"

# ── AC5: subdirectories stay allowed ─────────────────────────────────────────
expect "AC5: projects/ write" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"dump > ~/.claude/projects/x/cache.json\"},\"session_id\":\"$SID\"}"
expect "AC5: plugins/ write" approve \
  "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"/Users/bb16/.claude/plugins/cache/x/y.md\",\"content\":\"x\"},\"session_id\":\"$SID\"}"

# ── AC6: the config FILE at \$HOME is a sibling, not inside the dir ───────────
expect "AC6: > ~/.claude.json stays allowed" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.claude.json\"},\"session_id\":\"$SID\"}"

# ── AC7: bare mentions and reads never match ─────────────────────────────────
expect "AC7: cat is a read" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"cat ~/.claude/settings.json\"},\"session_id\":\"$SID\"}"
expect "AC7: a quoted mention with a redirect elsewhere" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo 'see ~/.claude/x.out' > /tmp/elsewhere.out\"},\"session_id\":\"$SID\"}"
expect "AC7: repo .fno paths are out of scope" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.fno/graph.json\"},\"session_id\":\"$SID\"}"

# ── AC8: the other write-operator families ────────────────────────────────────
expect "AC8: tee into the config dir" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"printf x | tee ~/.claude/leak.out\"},\"session_id\":\"$SID\"}"
expect "AC8: dd of= into the config dir" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"dd if=/dev/zero of=~/.claude/zero.img count=1\"},\"session_id\":\"$SID\"}"
expect "AC8: sed -i on a top-level file" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"sed -i s/a/b/ ~/.claude/stray.json\"},\"session_id\":\"$SID\"}"
expect "AC8: cp into the config dir" block \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"cp staged ~/.claude/scratch.out\"},\"session_id\":\"$SID\"}"
expect "AC8: mv into jobs/ stays allowed" approve \
  "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"mv staged ~/.claude/jobs/$SID/tmp/scratch.out\"},\"session_id\":\"$SID\"}"

# ── AC9: the refusal names the session's job tmp dir ──────────────────────────
out=$(printf '%s' "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > ~/.claude/x.out\"},\"session_id\":\"$SID\"}" | bash "$GUARD" 2>/dev/null)
if printf '%s' "$out" | grep -q "jobs/${SID:0:8}/tmp"; then
  pass "AC9: refusal names jobs/72301109/tmp"
else
  fail "AC9: refusal lacks the job dir: $out"
fi

# ── AC10: CLAUDE_CONFIG_DIR relocates the guarded dir ─────────────────────────
CFGDIR="$(mktemp -d)"
out=$(printf '%s' "{\"tool_name\":\"Write\",\"tool_input\":{\"file_path\":\"$CFGDIR/scratch.md\",\"content\":\"x\"},\"session_id\":\"$SID\"}" | CLAUDE_CONFIG_DIR="$CFGDIR" bash "$GUARD" 2>/dev/null)
if printf '%s' "$out" | grep -q '"block"'; then
  pass "AC10: CLAUDE_CONFIG_DIR top level is guarded"
else
  fail "AC10: expected block under CLAUDE_CONFIG_DIR: $out"
fi
out=$(printf '%s' "{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"echo x > $CFGDIR/jobs/x/tmp/o.out\"},\"session_id\":\"$SID\"}" | CLAUDE_CONFIG_DIR="$CFGDIR" bash "$GUARD" 2>/dev/null)
if [[ "$out" == "{}" ]]; then
  pass "AC10: CLAUDE_CONFIG_DIR subdir stays allowed"
else
  fail "AC10: subdir under CLAUDE_CONFIG_DIR blocked: $out"
fi
rm -rf "$CFGDIR"

printf '[ccw] %d passed, %d failed\n' "$PASS" "$FAIL"
[[ "$FAIL" -eq 0 ]]
