#!/usr/bin/env bash
# tests/events/test-check-pr-emits-polling.sh
#
# Structural test that skills/pr/check.md wires emit_polling_external_review
# at both Step 2a (immediate inline check) and Step 2b (cron prompt).
#
# Phase 1 task 1.2 of loop-correctness-sweep (ab-83be25ea).
# We test structurally because SKILL.md is markdown instructions for the
# main-thread agent, not an executable. The actual emission is covered by
# tests/events/test-polling-event-emission.sh.
#
# Run: bash tests/events/test-check-pr-emits-polling.sh

set -uo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
SKILL="$REPO_ROOT/skills/pr/check.md"

fail=0

if [[ ! -r "$SKILL" ]]; then
    echo "FAIL: SKILL.md not readable at $SKILL"
    exit 1
fi

# AC1-HP: Step 2a path emits with wait_kind=inline
# The inline emission lives between "REVIEW_STATE=" and "Step 2b".
STEP2A=$(awk '/REVIEW_STATE=/,/Step 2b/' "$SKILL")
if ! grep -q 'emit_polling_external_review' <<<"$STEP2A"; then
    echo "FAIL AC1-HP: Step 2a does not call emit_polling_external_review"
    fail=1
fi
if ! grep -q 'wait_kind=inline' <<<"$STEP2A"; then
    echo "FAIL AC1-HP: Step 2a does not use wait_kind=inline"
    fail=1
fi
if ! grep -q 'reviewer_bot=' <<<"$STEP2A"; then
    echo "FAIL AC1-HP: Step 2a does not pass reviewer_bot"
    fail=1
fi
if ! grep -q 'session_id=' <<<"$STEP2A"; then
    echo "FAIL AC1-HP: Step 2a does not pass session_id"
    fail=1
fi
if ! grep -q 'next_check_at' <<<"$STEP2A"; then
    echo "FAIL AC1-HP: Step 2a does not include next_check_at"
    fail=1
fi

# AC1-HP-2: cron-fired tick emits with wait_kind=cron
# The cron emission lives between "Step 2b" and "Step 3".
STEP2B=$(awk '/Step 2b: Schedule cron/,/### 3\. Fetch Inline Comments/' "$SKILL")
if ! grep -q 'emit_polling_external_review' <<<"$STEP2B"; then
    echo "FAIL AC1-HP-2: Step 2b cron prompt does not call emit_polling_external_review"
    fail=1
fi
if ! grep -q 'wait_kind=cron' <<<"$STEP2B"; then
    echo "FAIL AC1-HP-2: Step 2b cron prompt does not use wait_kind=cron"
    fail=1
fi
if ! grep -qF 'FIRST action' <<<"$STEP2B"; then
    echo "FAIL AC1-HP-2: Step 2b does not state the emit must be the FIRST action in the cron prompt"
    fail=1
fi

# AC2-ERR: skill-present-but-emitter-missing handled via warning
# Both blocks should defensively check emitter presence and warn-and-continue.
WARN_COUNT_HP=$(grep -c 'emit_polling_external_review helper missing' <<<"$STEP2A")
WARN_COUNT_HP2=$(grep -c 'emit_polling_external_review helper missing' <<<"$STEP2B")
if (( WARN_COUNT_HP < 1 )); then
    echo "FAIL AC2-ERR: Step 2a does not warn-and-continue when emitter missing"
    fail=1
fi
if (( WARN_COUNT_HP2 < 1 )); then
    echo "FAIL AC2-ERR: Step 2b does not warn-and-continue when emitter missing"
    fail=1
fi

# Both blocks must source events.sh before calling the emitter (helper not pre-loaded in cron turn).
SOURCE_COUNT_HP=$(grep -c 'source "$EVENTS_LIB"' <<<"$STEP2A")
SOURCE_COUNT_HP2=$(grep -c 'source "$EVENTS_LIB"' <<<"$STEP2B")
if (( SOURCE_COUNT_HP < 1 )); then
    echo "FAIL: Step 2a does not source events lib before calling emitter"
    fail=1
fi
if (( SOURCE_COUNT_HP2 < 1 )); then
    echo "FAIL: Step 2b does not source events lib before calling emitter"
    fail=1
fi

# Plan reference is preserved so future readers can trace the change
if ! grep -qF 'ab-83be25ea' "$SKILL"; then
    echo "FAIL: SKILL.md does not reference plan id ab-83be25ea"
    fail=1
fi

if (( fail == 0 )); then
    echo "PASS test-check-pr-emits-polling.sh"
    exit 0
else
    echo "FAIL test-check-pr-emits-polling.sh"
    exit 1
fi
