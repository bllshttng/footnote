#!/usr/bin/env bash
# tests/hooks/test_archive_artifacts_session_aware.sh
#
# Phase 2 task 2.3 of loop-correctness-sweep (ab-83be25ea). Verifies that
# _archive_artifacts archives gate artifacts whose session_id does NOT
# match the live state file's session_id, while preserving same-session
# artifacts in place.
#
# Run: bash tests/hooks/test_archive_artifacts_session_aware.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ARCHIVE_LIB="${REPO_ROOT_REAL}/scripts/lib/archive-artifacts.sh"

log()  { printf '[archive-session] %s\n' "$*"; }
fail() { printf '[archive-session] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[archive-session] PASS: %s\n' "$*"; }

[[ -f "$ARCHIVE_LIB" ]] || fail "archive-artifacts.sh not found at $ARCHIVE_LIB"

WORK=$(mktemp -d -t archive-session-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# Set up a synthetic repo + plan_dir + artifacts dir.
export REPO_ROOT="$WORK/repo"
mkdir -p "$REPO_ROOT/.fno/artifacts"
mkdir -p "$REPO_ROOT/.fno/scratchpad/execution"

PLAN_DIR="$WORK/plan-folder"
mkdir -p "$PLAN_DIR"
echo "# plan" > "$PLAN_DIR/00-INDEX.md"

CURRENT_SID=20260509T220000Z-22222-deadbe
OLD_SID_A=20260508T100000Z-99999-fedcba
OLD_SID_B=20260507T080000Z-11111-aaaaaa

cat > "$REPO_ROOT/.fno/target-state.md" <<EOF
---
status: COMPLETE
session_id: ${CURRENT_SID}
plan_path: ${PLAN_DIR}
created_at: 2026-05-09T22:00:00Z
---
EOF

# Seed three artifacts: one current, two stale (different session_ids).
cat > "$REPO_ROOT/.fno/artifacts/validate-${CURRENT_SID}.md" <<EOF
---
phase: validate
session_id: ${CURRENT_SID}
approved: true
---
EOF

cat > "$REPO_ROOT/.fno/artifacts/validate-${OLD_SID_A}.md" <<EOF
---
phase: validate
session_id: ${OLD_SID_A}
approved: true
---
EOF

cat > "$REPO_ROOT/.fno/artifacts/review-${OLD_SID_B}.md" <<EOF
---
phase: review
session_id: ${OLD_SID_B}
approved: true
---
EOF

# Seed an artifact missing session_id frontmatter — must be left alone
# (cannot determine which session it belongs to).
cat > "$REPO_ROOT/.fno/artifacts/legacy-no-session.md" <<EOF
---
phase: legacy
approved: true
---
EOF

# Source the library and call _archive_artifacts.
# shellcheck disable=SC1090
source "$ARCHIVE_LIB"
_archive_artifacts "$REPO_ROOT/.fno/target-state.md"

# AC1-HP: stale artifacts archived to plan_dir/artifacts-archive/
log "AC1-HP: stale prior-session artifacts moved to archive"
[[ -f "$PLAN_DIR/artifacts-archive/validate-${OLD_SID_A}.md" ]] \
    || fail "AC1-HP: stale validate artifact not archived"
[[ -f "$PLAN_DIR/artifacts-archive/review-${OLD_SID_B}.md" ]] \
    || fail "AC1-HP: stale review artifact not archived"
pass "AC1-HP: stale artifacts archived"

# AC1-HP-2: current-session artifact stays in place
log "AC1-HP-2: current-session artifact preserved in artifacts/"
[[ -f "$REPO_ROOT/.fno/artifacts/validate-${CURRENT_SID}.md" ]] \
    || fail "AC1-HP-2: current-session artifact was moved or deleted"
[[ ! -f "$REPO_ROOT/.fno/artifacts/validate-${OLD_SID_A}.md" ]] \
    || fail "AC1-HP-2: stale validate artifact not removed from artifacts/"
[[ ! -f "$REPO_ROOT/.fno/artifacts/review-${OLD_SID_B}.md" ]] \
    || fail "AC1-HP-2: stale review artifact not removed from artifacts/"
pass "AC1-HP-2: current artifact preserved, stale ones removed"

# AC4-EDGE: artifacts without session_id are NOT archived (cannot classify)
log "AC4-EDGE: artifacts without session_id frontmatter are left in place"
[[ -f "$REPO_ROOT/.fno/artifacts/legacy-no-session.md" ]] \
    || fail "AC4-EDGE: legacy artifact (no session_id) was wrongly moved"
pass "AC4-EDGE: legacy artifact preserved (no session_id to classify)"

# AC1-HP-3: manifest line per moved artifact
log "AC1-HP-3: manifest records each archived artifact"
[[ -f "$PLAN_DIR/artifacts-archive/.manifest" ]] \
    || fail "AC1-HP-3: manifest not written"
MANIFEST_LINES=$(wc -l < "$PLAN_DIR/artifacts-archive/.manifest" | tr -d ' ')
[[ "$MANIFEST_LINES" == "2" ]] \
    || fail "AC1-HP-3: manifest expected 2 lines, got $MANIFEST_LINES"
grep -q "validate-${OLD_SID_A}.md" "$PLAN_DIR/artifacts-archive/.manifest" \
    || fail "AC1-HP-3: manifest missing validate stale entry"
grep -q "review-${OLD_SID_B}.md" "$PLAN_DIR/artifacts-archive/.manifest" \
    || fail "AC1-HP-3: manifest missing review stale entry"
grep -q "prior_session=${OLD_SID_A}" "$PLAN_DIR/artifacts-archive/.manifest" \
    || fail "AC1-HP-3: manifest missing prior_session metadata"
pass "AC1-HP-3: manifest records each archived artifact with prior_session marker"

# AC2-ERR: no plan_dir resolved -> no archival happens (current artifacts stay,
# stale artifacts also stay because we have nowhere to move them).
log "AC2-ERR: no plan_dir means no archival, stale artifacts NOT deleted"
WORK2=$(mktemp -d -t archive-session-noplan-XXXXXX)
export REPO_ROOT="$WORK2/repo"
mkdir -p "$REPO_ROOT/.fno/artifacts"
mkdir -p "$REPO_ROOT/.fno/scratchpad"
cat > "$REPO_ROOT/.fno/target-state.md" <<EOF
---
status: COMPLETE
session_id: ${CURRENT_SID}
plan_path:
created_at: 2026-05-09T22:00:00Z
---
EOF
cat > "$REPO_ROOT/.fno/artifacts/validate-${OLD_SID_A}.md" <<EOF
---
phase: validate
session_id: ${OLD_SID_A}
approved: true
---
EOF
_archive_artifacts "$REPO_ROOT/.fno/target-state.md"
[[ -f "$REPO_ROOT/.fno/artifacts/validate-${OLD_SID_A}.md" ]] \
    || fail "AC2-ERR: stale artifact deleted without archive target (data loss)"
pass "AC2-ERR: no plan_dir leaves artifacts in place (no data loss)"
rm -rf "$WORK2"

# AC4-EDGE-2: two stale artifacts with same phase but different session_ids are BOTH archived (no overwrite collision)
log "AC4-EDGE-2: same-phase artifacts from two prior sessions are both kept"
WORK3=$(mktemp -d -t archive-session-collision-XXXXXX)
export REPO_ROOT="$WORK3/repo"
mkdir -p "$REPO_ROOT/.fno/artifacts"
mkdir -p "$REPO_ROOT/.fno/scratchpad"
PLAN_DIR3="$WORK3/plan"
mkdir -p "$PLAN_DIR3"
echo "# plan" > "$PLAN_DIR3/00-INDEX.md"
cat > "$REPO_ROOT/.fno/target-state.md" <<EOF
---
status: COMPLETE
session_id: ${CURRENT_SID}
plan_path: ${PLAN_DIR3}
created_at: 2026-05-09T22:00:00Z
---
EOF
SID_C=20260506T060000Z-77777-cccccc
cat > "$REPO_ROOT/.fno/artifacts/validate-${OLD_SID_A}.md" <<EOF
---
phase: validate
session_id: ${OLD_SID_A}
approved: true
---
EOF
cat > "$REPO_ROOT/.fno/artifacts/validate-${SID_C}.md" <<EOF
---
phase: validate
session_id: ${SID_C}
approved: true
---
EOF
_archive_artifacts "$REPO_ROOT/.fno/target-state.md"
[[ -f "$PLAN_DIR3/artifacts-archive/validate-${OLD_SID_A}.md" ]] \
    || fail "AC4-EDGE-2: first stale validate not archived"
[[ -f "$PLAN_DIR3/artifacts-archive/validate-${SID_C}.md" ]] \
    || fail "AC4-EDGE-2: second stale validate not archived (overwritten?)"
pass "AC4-EDGE-2: same-phase stale artifacts from different sessions both archived (filename-disambiguated)"
rm -rf "$WORK3"

echo "[archive-session] all session-aware archival tests passed"
exit 0
