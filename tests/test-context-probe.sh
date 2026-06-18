#!/usr/bin/env bash
# test-context-probe.sh - TDD harness for skills/target/scripts/context-probe.sh
# Tests transcript-derived context probe: model-aware window, usage sums, edge cases.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SCRIPT="$REPO_ROOT/skills/target/scripts/context-probe.sh"

pass=0
fail=0

# Temp dir for fixtures
TMPDIR_FIXTURES="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_FIXTURES"' EXIT

check_eq() {
  local desc="$1"
  local expected="$2"
  local actual="$3"
  if [ "$expected" = "$actual" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected='$expected' actual='$actual')"
    fail=$((fail+1))
  fi
}

check_exit() {
  local desc="$1"
  local expected_code="$2"
  local actual_code="$3"
  if [ "$expected_code" = "$actual_code" ]; then
    echo "PASS: $desc"
    pass=$((pass+1))
  else
    echo "FAIL: $desc (expected exit=$expected_code actual exit=$actual_code)"
    fail=$((fail+1))
  fi
}

# Helper: run probe, capture output + exit code
run_probe() {
  local transcript="$1"
  set +e
  output=$(bash "$SCRIPT" "$transcript" 2>/dev/null)
  probe_exit=$?
  set -e
}

# ---------------------------------------------------------------------------
# AC2-UI: 1M model -> window_tokens=1000000, used_pct based on that window
# model contains "[1m]" per spec -> claude-opus-4-8[1m]
# usage sum: 100000 + 200000 + 20000 = 320000; pct = round(100*320000/1000000) = 32
# ---------------------------------------------------------------------------
FIXTURE_1M="$TMPDIR_FIXTURES/transcript_1m.jsonl"
cat > "$FIXTURE_1M" <<'EOF'
{"type":"user","message":{"role":"user","content":"hello"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-opus-4-8[1m]","usage":{"input_tokens":100000,"cache_creation_input_tokens":200000,"cache_read_input_tokens":20000},"content":[]}}
EOF

run_probe "$FIXTURE_1M"
check_exit "AC2-UI: 1M model exits 0" "0" "$probe_exit"
check_eq "AC2-UI: 1M model used_pct=32" "32" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"
check_eq "AC2-UI: 1M model window_tokens=1000000" "1000000" "$(printf '%s' "$output" | jq -r '.window_tokens' 2>/dev/null)"
check_eq "AC2-UI: 1M model used_tokens=320000" "320000" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Default window: model does NOT contain "[1m]" -> window_tokens=200000
# usage sum: 50000 + 30000 + 20000 = 100000; pct = round(100*100000/200000) = 50
# ---------------------------------------------------------------------------
FIXTURE_DEFAULT="$TMPDIR_FIXTURES/transcript_default.jsonl"
cat > "$FIXTURE_DEFAULT" <<'EOF'
{"type":"user","message":{"role":"user","content":"hello"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":50000,"cache_creation_input_tokens":30000,"cache_read_input_tokens":20000},"content":[]}}
EOF

run_probe "$FIXTURE_DEFAULT"
check_exit "Default window exits 0" "0" "$probe_exit"
check_eq "Default window used_pct=50" "50" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"
check_eq "Default window window_tokens=200000" "200000" "$(printf '%s' "$output" | jq -r '.window_tokens' 2>/dev/null)"
check_eq "Default window model returned" "claude-sonnet-4-6" "$(printf '%s' "$output" | jq -r '.model' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Multiple assistant lines -> LAST one wins
# First line: big usage (input=150000); last line: small usage (input=10000)
# Expect: used_tokens=10000
# ---------------------------------------------------------------------------
FIXTURE_MULTI="$TMPDIR_FIXTURES/transcript_multi.jsonl"
cat > "$FIXTURE_MULTI" <<'EOF'
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":150000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}
{"type":"user","message":{"role":"user","content":"continue"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":10000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}
EOF

run_probe "$FIXTURE_MULTI"
check_exit "Multiple lines exits 0" "0" "$probe_exit"
check_eq "Multiple lines: last wins (used_tokens=10000)" "10000" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"
check_eq "Multiple lines: used_pct=5 (10000/200000)" "5" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Missing file -> exit 3, no JSON
# ---------------------------------------------------------------------------
run_probe "$TMPDIR_FIXTURES/nonexistent.jsonl"
check_exit "Missing file exits 3" "3" "$probe_exit"
check_eq "Missing file: no stdout" "" "$output"

# ---------------------------------------------------------------------------
# AC2-ERR: File with no assistant-usage lines -> exit 3
# ---------------------------------------------------------------------------
FIXTURE_NO_USAGE="$TMPDIR_FIXTURES/transcript_no_usage.jsonl"
cat > "$FIXTURE_NO_USAGE" <<'EOF'
{"type":"user","message":{"role":"user","content":"hello"}}
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","content":[]}}
EOF

run_probe "$FIXTURE_NO_USAGE"
check_exit "AC2-ERR: no assistant-usage lines exits 3" "3" "$probe_exit"

# ---------------------------------------------------------------------------
# Malformed JSON line followed by valid assistant line -> succeeds (bad line skipped)
# ---------------------------------------------------------------------------
FIXTURE_MALFORMED="$TMPDIR_FIXTURES/transcript_malformed.jsonl"
printf 'THIS IS NOT JSON\n{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":40000,"cache_creation_input_tokens":60000,"cache_read_input_tokens":0},"content":[]}}\n' > "$FIXTURE_MALFORMED"

run_probe "$FIXTURE_MALFORMED"
check_exit "Malformed line + valid: exits 0" "0" "$probe_exit"
check_eq "Malformed line + valid: used_tokens=100000" "100000" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Usage block with missing cache fields (only input_tokens) -> absent fields = 0
# used_tokens = 80000; pct = round(100*80000/200000) = 40
# ---------------------------------------------------------------------------
FIXTURE_PARTIAL="$TMPDIR_FIXTURES/transcript_partial.jsonl"
cat > "$FIXTURE_PARTIAL" <<'EOF'
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":80000},"content":[]}}
EOF

run_probe "$FIXTURE_PARTIAL"
check_exit "Partial usage block exits 0" "0" "$probe_exit"
check_eq "Partial usage: used_tokens=80000" "80000" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"
check_eq "Partial usage: used_pct=40" "40" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Zero usage (all fields 0) -> used_tokens=0, used_pct=0
# ---------------------------------------------------------------------------
FIXTURE_ZERO="$TMPDIR_FIXTURES/transcript_zero.jsonl"
cat > "$FIXTURE_ZERO" <<'EOF'
{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}
EOF

run_probe "$FIXTURE_ZERO"
check_exit "Zero usage exits 0" "0" "$probe_exit"
check_eq "Zero usage: used_tokens=0" "0" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"
check_eq "Zero usage: used_pct=0" "0" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

# ---------------------------------------------------------------------------
# No argument -> exit 3
# ---------------------------------------------------------------------------
set +e
no_arg_output=$(bash "$SCRIPT" 2>/dev/null)
no_arg_exit=$?
set -e
check_exit "No argument exits 3" "3" "$no_arg_exit"

# ---------------------------------------------------------------------------
# Results
# ---------------------------------------------------------------------------
echo ""
echo "Results: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
