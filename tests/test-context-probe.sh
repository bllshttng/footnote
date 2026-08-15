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
# Per-family window table. Pins the DENOMINATOR per model family, which is the
# whole point: a Claude id never carries the "[1m]" marker, so keying only on
# that suffix put every 1M Claude model on the 200K branch and inflated
# used_pct 5x. usage sum below is 100000 in every case, so the expected pct is
# just 100000/window: 10 on a 1M window, 50 on a 200K one.
# ---------------------------------------------------------------------------
check_window() { # label  model  expected_window  expected_pct
  local label="$1" model="$2" want_window="$3" want_pct="$4"
  local fixture="$TMPDIR_FIXTURES/transcript_window.jsonl"
  printf '{"type":"user","message":{"role":"user","content":"hello"}}\n{"type":"assistant","message":{"role":"assistant","model":"%s","usage":{"input_tokens":50000,"cache_creation_input_tokens":30000,"cache_read_input_tokens":20000},"content":[]}}\n' \
    "$model" > "$fixture"
  run_probe "$fixture"
  check_exit "window[$label] exits 0" "0" "$probe_exit"
  check_eq "window[$label] window_tokens=$want_window" "$want_window" "$(printf '%s' "$output" | jq -r '.window_tokens' 2>/dev/null)"
  check_eq "window[$label] used_pct=$want_pct" "$want_pct" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"
  check_eq "window[$label] model echoed" "$model" "$(printf '%s' "$output" | jq -r '.model' 2>/dev/null)"
}

check_window "opus-5"      "claude-opus-5"          1000000 10
check_window "sonnet-5"    "claude-sonnet-5"        1000000 10
check_window "opus-4-8"    "claude-opus-4-8"        1000000 10
check_window "opus-4-6"    "claude-opus-4-6"        1000000 10
check_window "sonnet-4-6"  "claude-sonnet-4-6"      1000000 10
check_window "fable-5"     "claude-fable-5"         1000000 10
check_window "zai-1m"      "glm-5.2[1m]"            1000000 10
check_window "zai-bare-5.2" "glm-5.2"               1000000 10
check_window "zai-bare-5.3" "glm-5.3"               1000000 10
check_window "haiku-4-5"   "claude-haiku-4-5-20251001" 200000 50

# 1M is an allowlist, so every unlisted id lands on 200K. These four are the
# reason: a `claude-*` catch-all would hand each of them a 1M denominator,
# understate pressure 5x, and let the session run out of context - the failure
# direction this table exists to avoid. A future 1M model landing here fires the
# handoff early, which is the cheap error and is fixed by adding one line above.
check_window "legacy-opus-4-5"   "claude-opus-4-5-20250929"   200000 50
check_window "legacy-sonnet-4-5" "claude-sonnet-4-5-20250929" 200000 50
check_window "legacy-3-5"        "claude-3-5-sonnet-20241022" 200000 50
check_window "unknown"           "some-other-model"           200000 50

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
check_eq "Multiple lines: used_pct=1 (10000/1000000)" "1" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

# ---------------------------------------------------------------------------
# Multiple assistant lines, NO trailing newline -> LAST one still wins.
# Heredocs and printf-'...\n' always terminate the final line; only shell
# command substitution strips a trailing newline, so this shape is invisible to
# every other fixture. A _complete_lines trim once dropped the unterminated
# final record and made the probe report the second-to-last turn - silently
# stale, and in the arm-handoff hook that suppressed a needed handoff.
# ---------------------------------------------------------------------------
FIXTURE_MULTI_NONL="$TMPDIR_FIXTURES/transcript_multi_nonl.jsonl"
printf '%s\n%s' \
  '{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":150000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}' \
  '{"type":"assistant","message":{"role":"assistant","model":"claude-sonnet-4-6","usage":{"input_tokens":10000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"content":[]}}' \
  > "$FIXTURE_MULTI_NONL"

run_probe "$FIXTURE_MULTI_NONL"
check_exit "No-trailing-newline exits 0" "0" "$probe_exit"
check_eq "No-trailing-newline: last wins (used_tokens=10000)" "10000" "$(printf '%s' "$output" | jq -r '.used_tokens' 2>/dev/null)"
check_eq "No-trailing-newline: used_pct=1" "1" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

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
check_eq "Partial usage: used_pct=8 (80000/1000000)" "8" "$(printf '%s' "$output" | jq -r '.used_pct' 2>/dev/null)"

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
