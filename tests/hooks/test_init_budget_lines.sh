#!/usr/bin/env bash
# test_init_budget_lines.sh -- verify that init-target-state.sh writes budget
# cap lines into the manifest without gluing them to the following comment.
#
# Covers:
#   - AC1-HP: both caps configured => two complete budget_* lines + comment on own line
#   - AC1-ERR: no caps configured => no budget_* keys; comment still on own line
#   - AC1-EDGE: only wall-clock set; only cost set
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[budget-lines] %s\n' "$*"; }
fail() { printf '[budget-lines] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[budget-lines] PASS: %s\n' "$*"; }
skip() { printf '[budget-lines] SKIP: %s\n' "$*" >&2; exit 77; }

# ── Prereqs ──────────────────────────────────────────────────────────
command -v git     &>/dev/null || skip "git not on PATH"
command -v python3 &>/dev/null || skip "python3 not on PATH"
[[ -f "$INIT" ]]     || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

# ── Helper: create an isolated temp repo ────────────────────────────
# Usage: make_repo <tmpvar> [settings_yaml_content]
# Sets the variable named by $1 to the temp dir path.
make_repo() {
  local _varname="$1"
  local _settings="${2:-}"
  local _dir
  _dir="$(mktemp -d -t init-budget-lines.XXXXXX)" || fail "mktemp failed in make_repo"
  eval "${_varname}=\"\${_dir}\""
  (cd "$_dir" && git init -q && mkdir -p .fno) || fail "repo setup failed in $_dir"
  if [[ -n "$_settings" ]]; then
    printf '%s\n' "$_settings" > "${_dir}/.fno/config.toml"
  else
    printf '# isolated - no budget config\n' > "${_dir}/.fno/config.toml"
  fi
  # Isolated home dir: no ~/.fno/config.toml budget leakage
  mkdir -p "${_dir}/home" || fail "mkdir home failed in $_dir"
}

# HOME is the effective isolation: init-target-state.sh sets
# GLOBAL_SETTINGS="${HOME}/.fno/config.toml" unconditionally (line 126),
# clobbering any exported GLOBAL_SETTINGS. Each bash "$INIT" invocation sets
# HOME to the per-scenario home dir so init reads the isolated blank settings.
# The GLOBAL_SETTINGS exports below are retained for documentation; they are
# redundant because HOME takes precedence.
_BLANK_GLOBAL=""

# Track all temp dirs for cleanup
_ALL_TMPS=()
trap 'rm -rf "${_ALL_TMPS[@]}"' EXIT

# ── AC1-HP: both caps configured ─────────────────────────────────────
log "AC1-HP: both caps configured => two complete budget lines + comment on own line"

_BOTH_SETTINGS=$(cat << 'YAML'
[budget.attended]
wall_clock_cap_minutes = 90
cost_cap_usd = 42
[budget.unattended]
wall_clock_cap_minutes = 90
cost_cap_usd = 42
YAML
)

make_repo TMP_HP "$_BOTH_SETTINGS"
_ALL_TMPS+=("$TMP_HP")
# write blank global settings
_BLANK_HP="${TMP_HP}/home/.fno/config.toml"
mkdir -p "${TMP_HP}/home/.fno"
printf '# isolated\n' > "$_BLANK_HP"

(cd "$TMP_HP" && \
  HOME="${TMP_HP}/home" \
  GLOBAL_SETTINGS="${_BLANK_HP}" \
  TARGET_START=1 \
  TARGET_INPUT="test-budget-hp" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "AC1-HP: init exited non-zero"

STATE_HP="$TMP_HP/.fno/target-state.md"
[[ -f "$STATE_HP" ]] || fail "AC1-HP: state file not created"

# budget_wall_clock_cap_minutes line must be complete (own line, correct value)
grep -qE '^budget_wall_clock_cap_minutes:[[:space:]]*90$' "$STATE_HP" \
  || fail "AC1-HP: budget_wall_clock_cap_minutes line missing or malformed (got: $(grep 'budget_wall_clock' "$STATE_HP" || echo '<absent>'))"
pass "AC1-HP: budget_wall_clock_cap_minutes line correct"

# budget_cost_cap_usd line must be complete (own line, correct value)
grep -qE '^budget_cost_cap_usd:[[:space:]]*42$' "$STATE_HP" \
  || fail "AC1-HP: budget_cost_cap_usd line missing or malformed (got: $(grep 'budget_cost_cap_usd' "$STATE_HP" || echo '<absent>'))"
pass "AC1-HP: budget_cost_cap_usd line correct"

# Auto-merge comment must be on its own line (not glued to a budget line)
grep -qE '^# Auto-merge inputs$' "$STATE_HP" \
  || fail "AC1-HP: '# Auto-merge inputs' not on its own line (got: $(grep 'Auto-merge' "$STATE_HP" || echo '<absent>'))"
pass "AC1-HP: '# Auto-merge inputs' is on its own line"

# YAML parses correctly
python3 -c "
import sys
content = open('$STATE_HP').read()
parts = content.split('---')
if len(parts) < 3:
    sys.exit('not enough --- delimiters')
import yaml
data = yaml.safe_load(parts[1])
assert data.get('budget_cost_cap_usd') == 42, 'budget_cost_cap_usd mismatch: ' + str(data.get('budget_cost_cap_usd'))
assert data.get('budget_wall_clock_cap_minutes') == 90, 'budget_wall_clock_cap_minutes mismatch: ' + str(data.get('budget_wall_clock_cap_minutes'))
assert 'auto_merge_enabled' in data, 'auto_merge_enabled missing'
print('YAML: budget_cost_cap_usd=42, budget_wall_clock_cap_minutes=90, auto_merge_enabled present')
" || fail "AC1-HP: YAML parse/assertion failed"
pass "AC1-HP: YAML parses correctly"

# No budget_* line may have a '#' glued directly to the value (regression guard)
grep -qE '^budget_[a-z_]+:.*#' "$STATE_HP" \
  && fail "AC1-HP: budget line has glued '#' corruption (got: $(grep '^budget_[a-z_]+:.*#' "$STATE_HP" || true))"
pass "AC1-HP: no budget line has glued '#' corruption"

# ── AC1-ERR: no caps configured ──────────────────────────────────────
log "AC1-ERR: no caps configured => no budget_* keys, comment still on own line"

make_repo TMP_ERR ""
_ALL_TMPS+=("$TMP_ERR")
mkdir -p "${TMP_ERR}/home/.fno"
_BLANK_ERR="${TMP_ERR}/home/.fno/config.toml"
printf '# isolated\n' > "$_BLANK_ERR"

(cd "$TMP_ERR" && \
  HOME="${TMP_ERR}/home" \
  GLOBAL_SETTINGS="${_BLANK_ERR}" \
  TARGET_START=1 \
  TARGET_INPUT="test-budget-no-caps" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "AC1-ERR: init exited non-zero"

STATE_ERR="$TMP_ERR/.fno/target-state.md"
[[ -f "$STATE_ERR" ]] || fail "AC1-ERR: state file not created"

# No budget_* keys should appear
grep -qE '^budget_' "$STATE_ERR" \
  && fail "AC1-ERR: unexpected budget_* key in manifest (got: $(grep '^budget_' "$STATE_ERR"))"
pass "AC1-ERR: no budget_* keys present"

# Comment must still appear on its own line
grep -qE '^# Auto-merge inputs$' "$STATE_ERR" \
  || fail "AC1-ERR: '# Auto-merge inputs' not on its own line (got: $(grep 'Auto-merge' "$STATE_ERR" || echo '<absent>'))"
pass "AC1-ERR: '# Auto-merge inputs' is on its own line with no caps"

# YAML parses with auto_merge_enabled present
python3 -c "
import sys
content = open('$STATE_ERR').read()
parts = content.split('---')
if len(parts) < 3:
    sys.exit('not enough --- delimiters')
import yaml
data = yaml.safe_load(parts[1])
assert data.get('budget_cost_cap_usd') is None, 'budget_cost_cap_usd should be absent/None'
assert data.get('budget_wall_clock_cap_minutes') is None, 'budget_wall_clock_cap_minutes should be absent/None'
assert 'auto_merge_enabled' in data, 'auto_merge_enabled missing'
print('YAML: no budget keys, auto_merge_enabled present')
" || fail "AC1-ERR: YAML parse/assertion failed"
pass "AC1-ERR: YAML parses correctly with no budget keys"

# ── AC1-EDGE-A: only wall-clock set ──────────────────────────────────
log "AC1-EDGE-A: only wall_clock_cap_minutes set => wall-clock line present, no cost line"

_WALL_ONLY_SETTINGS=$(cat << 'YAML'
[budget.attended]
wall_clock_cap_minutes = 90
[budget.unattended]
wall_clock_cap_minutes = 90
YAML
)

make_repo TMP_WALL "$_WALL_ONLY_SETTINGS"
_ALL_TMPS+=("$TMP_WALL")
mkdir -p "${TMP_WALL}/home/.fno"
_BLANK_WALL="${TMP_WALL}/home/.fno/config.toml"
printf '# isolated\n' > "$_BLANK_WALL"

(cd "$TMP_WALL" && \
  HOME="${TMP_WALL}/home" \
  GLOBAL_SETTINGS="${_BLANK_WALL}" \
  TARGET_START=1 \
  TARGET_INPUT="test-wall-only" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "AC1-EDGE-A: init exited non-zero"

STATE_WALL="$TMP_WALL/.fno/target-state.md"
[[ -f "$STATE_WALL" ]] || fail "AC1-EDGE-A: state file not created"

grep -qE '^budget_wall_clock_cap_minutes:[[:space:]]*90$' "$STATE_WALL" \
  || fail "AC1-EDGE-A: budget_wall_clock_cap_minutes line missing or malformed"
pass "AC1-EDGE-A: budget_wall_clock_cap_minutes line correct"

grep -qE '^budget_cost_cap_usd:' "$STATE_WALL" \
  && fail "AC1-EDGE-A: budget_cost_cap_usd should be absent (got: $(grep '^budget_cost_cap_usd' "$STATE_WALL"))"
pass "AC1-EDGE-A: budget_cost_cap_usd correctly absent"

grep -qE '^# Auto-merge inputs$' "$STATE_WALL" \
  || fail "AC1-EDGE-A: '# Auto-merge inputs' not on its own line"
pass "AC1-EDGE-A: comment on its own line"

# ── AC1-EDGE-B: only cost set ─────────────────────────────────────────
log "AC1-EDGE-B: only cost_cap_usd set => cost line present, no wall-clock line"

_COST_ONLY_SETTINGS=$(cat << 'YAML'
[budget.attended]
cost_cap_usd = 42
[budget.unattended]
cost_cap_usd = 42
YAML
)

make_repo TMP_COST "$_COST_ONLY_SETTINGS"
_ALL_TMPS+=("$TMP_COST")
mkdir -p "${TMP_COST}/home/.fno"
_BLANK_COST="${TMP_COST}/home/.fno/config.toml"
printf '# isolated\n' > "$_BLANK_COST"

(cd "$TMP_COST" && \
  HOME="${TMP_COST}/home" \
  GLOBAL_SETTINGS="${_BLANK_COST}" \
  TARGET_START=1 \
  TARGET_INPUT="test-cost-only" \
  bash "$INIT" >/dev/null 2>&1) \
  || fail "AC1-EDGE-B: init exited non-zero"

STATE_COST="$TMP_COST/.fno/target-state.md"
[[ -f "$STATE_COST" ]] || fail "AC1-EDGE-B: state file not created"

grep -qE '^budget_cost_cap_usd:[[:space:]]*42$' "$STATE_COST" \
  || fail "AC1-EDGE-B: budget_cost_cap_usd line missing or malformed"
pass "AC1-EDGE-B: budget_cost_cap_usd line correct"

grep -qE '^budget_wall_clock_cap_minutes:' "$STATE_COST" \
  && fail "AC1-EDGE-B: budget_wall_clock_cap_minutes should be absent (got: $(grep '^budget_wall_clock_cap_minutes' "$STATE_COST"))"
pass "AC1-EDGE-B: budget_wall_clock_cap_minutes correctly absent"

grep -qE '^# Auto-merge inputs$' "$STATE_COST" \
  || fail "AC1-EDGE-B: '# Auto-merge inputs' not on its own line"
pass "AC1-EDGE-B: comment on its own line"

log "all scenarios passed"
exit 0
