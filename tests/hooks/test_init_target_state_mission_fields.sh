#!/usr/bin/env bash
# test_init_target_state_mission_fields.sh -- verify that init-target-state.sh
# seeds all five mission_* fields from TARGET_MISSION_* env vars.
#
# Covers:
#   - AC1-HP: TARGET_MISSION_* env vars set => mission fields populated in state.md
#   - AC2-ERR: TARGET_MISSION_* env vars unset => all five fields default to null
#
# Exit codes:
#   0  all scenarios passed
#   1  assertion failed
#   77 skipped (missing dependencies)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INIT="${REPO_ROOT}/hooks/helpers/init-target-state.sh"

log()  { printf '[mission-fields] %s\n' "$*"; }
fail() { printf '[mission-fields] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[mission-fields] PASS: %s\n' "$*"; }
skip() { printf '[mission-fields] SKIP: %s\n' "$*" >&2; exit 77; }

# ── Prereqs ──────────────────────────────────────────────────────────
command -v git     &>/dev/null || skip "git not on PATH"
command -v python3 &>/dev/null || skip "python3 not on PATH"
[[ -f "$INIT" ]]     || fail "init script not found at $INIT"

bash -n "$INIT" || fail "bash -n rejected $INIT (syntax error)"
pass "init script passes bash -n"

# ── AC1-HP: env vars set => fields populated ─────────────────────────
log "AC1-HP: TARGET_MISSION_* set => mission fields populated"

TMP1=$(mktemp -d -t init-mission-fields-set.XXXXXX)
trap 'rm -rf "$TMP1" "${TMP2:-}"' EXIT

cd "$TMP1"
git init -q
mkdir -p .fno

TARGET_START=1 \
TARGET_INPUT="test-input" \
TARGET_MISSION_ID="ab-test1234" \
TARGET_MISSION_WAVE="2" \
TARGET_MISSION_SLUG="2026-05-13-test-slug" \
TARGET_MISSION_FROM_MSG_ID="msg-abc-foo" \
  bash "$INIT" >/dev/null 2>&1 \
  || fail "AC1-HP: init exited non-zero with mission vars set"

STATE1="$TMP1/.fno/target-state.md"
[[ -f "$STATE1" ]] || fail "AC1-HP: state file not created"

# mission_id: should be "ab-test1234" (quoted in YAML since it's a string)
grep -qE '^mission_id:' "$STATE1" \
  || fail "AC1-HP: mission_id key missing from state.md"
grep -qE '^mission_id:.*ab-test1234' "$STATE1" \
  || fail "AC1-HP: mission_id value not populated (expected ab-test1234)"
pass "AC1-HP: mission_id present and populated"

# mission_wave: should be 2 (bare integer, no quotes)
grep -qE '^mission_wave:' "$STATE1" \
  || fail "AC1-HP: mission_wave key missing from state.md"
grep -qE '^mission_wave:[[:space:]]*2' "$STATE1" \
  || fail "AC1-HP: mission_wave value not populated (expected 2)"
pass "AC1-HP: mission_wave present and populated"

# mission_slug
grep -qE '^mission_slug:' "$STATE1" \
  || fail "AC1-HP: mission_slug key missing from state.md"
grep -qE '^mission_slug:.*2026-05-13-test-slug' "$STATE1" \
  || fail "AC1-HP: mission_slug value not populated (expected 2026-05-13-test-slug)"
pass "AC1-HP: mission_slug present and populated"

# mission_from_msg_id
grep -qE '^mission_from_msg_id:' "$STATE1" \
  || fail "AC1-HP: mission_from_msg_id key missing from state.md"
grep -qE '^mission_from_msg_id:.*msg-abc-foo' "$STATE1" \
  || fail "AC1-HP: mission_from_msg_id value not populated (expected msg-abc-foo)"
pass "AC1-HP: mission_from_msg_id present and populated"

# mission_complete_emitted_at: REMOVED by the control-plane collapse wedge
# (ab-d0337fbc) - it was a mutable write-tracking sentinel; the manifest is
# immutable and termination is an event in events.jsonl now. Assert absence.
grep -qE '^mission_complete_emitted_at:' "$STATE1" \
  && fail "AC1-HP: mission_complete_emitted_at must NOT be in the immutable manifest"
pass "AC1-HP: mission_complete_emitted_at correctly absent"

# YAML must be parseable
python3 -c "
import sys
content = open('$STATE1').read()
parts = content.split('---')
if len(parts) < 3:
    sys.exit('not enough --- delimiters')
import yaml
data = yaml.safe_load(parts[1])
assert data.get('mission_id') == 'ab-test1234', 'mission_id mismatch: ' + str(data.get('mission_id'))
assert data.get('mission_wave') == 2, 'mission_wave should be int 2, got: ' + str(data.get('mission_wave'))
assert data.get('mission_slug') == '2026-05-13-test-slug', 'mission_slug mismatch'
assert data.get('mission_from_msg_id') == 'msg-abc-foo', 'mission_from_msg_id mismatch'
assert 'mission_complete_emitted_at' not in data, 'mission_complete_emitted_at must be absent (ab-d0337fbc)'
print('YAML parses correctly and all values match')
" || fail "AC1-HP: YAML parse/assertion failed"
pass "AC1-HP: YAML parses correctly with correct types"

# ── AC2-ERR: env vars unset => all five fields null ──────────────────
log "AC2-ERR: TARGET_MISSION_* unset => all five fields null"

TMP2=$(mktemp -d -t init-mission-fields-unset.XXXXXX)
cd "$TMP2"
git init -q
mkdir -p .fno

# Explicitly unset any TARGET_MISSION_* that might be in environment
env -u TARGET_MISSION_ID \
    -u TARGET_MISSION_WAVE \
    -u TARGET_MISSION_SLUG \
    -u TARGET_MISSION_FROM_MSG_ID \
    TARGET_START=1 \
    TARGET_INPUT="test-no-mission" \
    bash "$INIT" >/dev/null 2>&1 \
  || fail "AC2-ERR: init exited non-zero with no mission vars"

STATE2="$TMP2/.fno/target-state.md"
[[ -f "$STATE2" ]] || fail "AC2-ERR: state file not created"

# All five mission fields must be null
# mission_complete_emitted_at dropped from the loop: removed by ab-d0337fbc.
for key in mission_id mission_wave mission_slug mission_from_msg_id; do
  grep -qE "^${key}:" "$STATE2" \
    || fail "AC2-ERR: key ${key} missing from state.md"
  grep -qE "^${key}:[[:space:]]*null" "$STATE2" \
    || fail "AC2-ERR: ${key} should be null when env var unset (got: $(grep "^${key}:" "$STATE2"))"
  pass "AC2-ERR: ${key} is null when env var unset"
done

# YAML parses with all None values
python3 -c "
import sys
content = open('$STATE2').read()
parts = content.split('---')
if len(parts) < 3:
    sys.exit('not enough --- delimiters')
import yaml
data = yaml.safe_load(parts[1])
for key in ('mission_id', 'mission_wave', 'mission_slug', 'mission_from_msg_id', 'mission_complete_emitted_at'):
    val = data.get(key)
    assert val is None, f'{key} should be None (null), got: {val!r}'
print('YAML parses correctly; all mission fields are None')
" || fail "AC2-ERR: YAML parse/assertion failed"
pass "AC2-ERR: YAML parses correctly with all mission fields null"

log "all scenarios passed"
exit 0
