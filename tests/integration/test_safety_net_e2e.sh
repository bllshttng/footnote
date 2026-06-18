#!/usr/bin/env bash
# End-to-end safety-net integration test.
#
# Bundled scenarios covering the three structural detectors and the
# cancel sentinel one-shot semantics. Each scenario sets up a fake
# project state, fires the stop hook, and asserts the BLOCKED reason
# discriminator landed correctly in target-state.md.

set -euo pipefail

export TARGET_TEST_QUIET=1

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
HOOK="${REPO_ROOT}/hooks/target-stop-hook.sh"

log()  { printf '[e2e] %s\n' "$*"; }
fail() { printf '[e2e] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[e2e] PASS: %s\n' "$*"; }
skip() { printf '[e2e] SKIP: %s\n' "$*" >&2; exit 77; }

[[ -f "$HOOK" ]] || fail "hook not found at $HOOK"
command -v jq  >/dev/null 2>&1 || skip "jq missing"
command -v git >/dev/null 2>&1 || skip "git missing"

write_state() {
    # Minimal state file with overridable status + created_at + session_id.
    local file="$1" status="$2" created_at="$3" session_id="$4" cwd="$5"
    cat > "$file" <<EOF
---
status: $status
input: "ab-test"
plan_path: "plans/test"
session_id: $session_id
provenance_nonce: ffffffffffffffff
created_at: $created_at
updated_at: $created_at
iteration: 1
current_phase: do
owner_pid: $$
owner_started_at: $created_at
owner_cwd: "$cwd"
no_external: false
no_docs: false
no_ship: false
no_verify: true
no_goals: false
no_browser: false
no_clean: true
no_how_to: false
has_ui: false
skip_flags_initial:
  no_external: false
  no_docs: false
  no_ship: false
  no_verify: true
  no_goals: false
  no_browser: false
  no_clean: true
  no_how_to: false
  has_ui: false
quality_check_passed: false
output_validated: false
artifact_shipped: false
external_review_passed: false
goal_verification_passed: false
docs_generated: false
browser_testing_passed: false
ledger_updated: false
clean_passed: false
auto_merge_enabled: false
auto_merge_approved: false
merged_prs: []
merge_auto_queued: []
merge_failed: []
conflicts_resolved: []
---
EOF
}

run_hook_with_env() {
    local env_var="$1"  # e.g. "TARGET_HELP_LIMIT=1"
    local input
    input=$(jq -nc --arg path "$WORK/transcript.jsonl" '{transcript_path: $path}')
    set +e
    if [[ -n "$env_var" ]]; then
        echo "$input" | env "$env_var" bash "$HOOK" >/dev/null 2>&1
    else
        echo "$input" | bash "$HOOK" >/dev/null 2>&1
    fi
    local rc=$?
    set -e
    echo "$rc"
}

# ── Scenario A: cancel sentinel honored, blocked_reason: user:sentinel ─
log "scenario A: fresh sentinel -> stuck/sentinel/blocked_reason"
WORK=$(mktemp -d -t e2e-A-XXXXXX)
cd "$WORK"
git init -q
git config user.email t@t
git config user.name t
echo seed > seed
git add seed
git commit -q -m seed
mkdir -p .fno
PAST=$(date -u -v-1H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)
write_state .fno/target-state.md IN_PROGRESS "$PAST" "e2e-A" "$WORK"
touch .fno/.target-cancelled
: > transcript.jsonl
RC=$(run_hook_with_env "")
[[ "$RC" == "0" ]] || fail "scenario A: expected exit 0, got $RC"
grep -q '^status: BLOCKED' .fno/target-state.md \
    || fail "scenario A: status not BLOCKED"
grep -q '^blocked_reason: user:sentinel' .fno/target-state.md \
    || fail "scenario A: blocked_reason not user:sentinel"
[[ ! -f .fno/.target-cancelled ]] \
    || fail "scenario A: sentinel was not removed post-honor"
cd /; rm -rf "$WORK"
pass "A: sentinel BLOCKED + post-honor rm + reason=user:sentinel"

# ── Scenario B: budget wall-clock cap trips ──────────────────────────
log "scenario B: wall-clock budget trip -> stuck:budget_exceeded axis=wall_clock"
WORK=$(mktemp -d -t e2e-B-XXXXXX)
cd "$WORK"
git init -q
git config user.email t@t
git config user.name t
echo seed > seed
git add seed
git commit -q -m seed
mkdir -p .fno

cat > .fno/settings.yaml <<'EOF'
config:
  unattended:
    enabled: false
  budget:
    attended:
      wall_clock_cap_minutes: 1
EOF

PAST=$(date -u -v-5M +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '5 minutes ago' +%Y-%m-%dT%H:%M:%SZ)
write_state .fno/target-state.md IN_PROGRESS "$PAST" "e2e-B" "$WORK"
: > transcript.jsonl
RC=$(run_hook_with_env "")
[[ "$RC" == "0" ]] || fail "scenario B: expected exit 0, got $RC"
grep -q '^blocked_reason: stuck:budget_exceeded' .fno/target-state.md \
    || fail "scenario B: blocked_reason not stuck:budget_exceeded"
grep -q '^blocked_reason_axis: wall_clock' .fno/target-state.md \
    || fail "scenario B: axis not wall_clock"
cd /; rm -rf "$WORK"
pass "B: wall-clock budget trip + axis=wall_clock"

# ── Scenario C: help escalation with TARGET_HELP_LIMIT=1 ──────────────
log "scenario C: single help with TARGET_HELP_LIMIT=1 -> stuck:repeated_help_no_progress"
WORK=$(mktemp -d -t e2e-C-XXXXXX)
cd "$WORK"
git init -q
git config user.email t@t
git config user.name t
echo seed > seed
git add seed
git commit -q -m seed
mkdir -p .fno

PAST=$(date -u -v-1H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)
write_state .fno/target-state.md IN_PROGRESS "$PAST" "e2e-C" "$WORK"

cat > .fno/hook-events.jsonl <<'EOF'
{"ts":"2099-01-01T00:00:00Z","type":"help_requested","data":{"session_id":"e2e-C","reason":"stuck"}}
EOF
chmod 0600 .fno/hook-events.jsonl

: > transcript.jsonl
RC=$(run_hook_with_env "TARGET_HELP_LIMIT=1")
[[ "$RC" == "0" ]] || fail "scenario C: expected exit 0, got $RC"
grep -q '^blocked_reason: stuck:repeated_help_no_progress' .fno/target-state.md \
    || fail "scenario C: blocked_reason not stuck:repeated_help_no_progress"
cd /; rm -rf "$WORK"
pass "C: help-escalation single-help trip with TARGET_HELP_LIMIT=1"

# ── Scenario D: cosmetic commits trip no-gate-progress ──────────────
# Per integration-test-analyzer review on PR #195: the no_gate_progress
# discriminator had only structural grep coverage. This scenario exercises
# the full path - 5 iterations advancing commit_hash but flat gate_hash -
# and asserts blocked_reason: stuck:no_gate_progress lands in state.
log "scenario D: cosmetic commits with flat gates -> stuck:no_gate_progress"
WORK=$(mktemp -d -t e2e-D-XXXXXX)
cd "$WORK"
git init -q
git config user.email t@t
git config user.name t
echo seed > seed
git add seed
git commit -q -m seed
mkdir -p .fno

PAST=$(date -u -v-1H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '1 hour ago' +%Y-%m-%dT%H:%M:%SZ)
write_state .fno/target-state.md IN_PROGRESS "$PAST" "e2e-D" "$WORK"
: > transcript.jsonl

# Fire the hook 6 times. Between each spawn make a cosmetic commit
# (advances commit_hash + files_count + events_count) but never flip
# any *_passed gate, so gate_hash stays flat.
HOOK_INPUT=$(jq -nc --arg path "$WORK/transcript.jsonl" '{transcript_path: $path}')
for i in 1 2 3 4 5 6; do
    echo "iter-$i" > cosmetic-$i
    git add cosmetic-$i
    git commit -q -m "cosmetic $i"
    set +e
    echo "$HOOK_INPUT" | bash "$HOOK" >/dev/null 2>&1
    set -e
    # If status flipped to BLOCKED, the detector tripped.
    if grep -q '^status: BLOCKED' .fno/target-state.md; then
        break
    fi
done

grep -q '^status: BLOCKED' .fno/target-state.md \
    || fail "scenario D: status never flipped to BLOCKED after 6 cosmetic commits"
grep -q '^blocked_reason: stuck:no_gate_progress' .fno/target-state.md \
    || { cat .fno/target-state.md | head -10; fail "scenario D: blocked_reason not stuck:no_gate_progress"; }
cd /; rm -rf "$WORK"
pass "D: cosmetic-commit churn trips stuck:no_gate_progress"

log "all e2e scenarios passed"
