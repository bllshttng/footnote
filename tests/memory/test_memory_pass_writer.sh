#!/usr/bin/env bash
# tests/memory/test_memory_pass_writer.sh
#
# Phase 3 task 3.1 of loop-correctness-sweep (ab-83be25ea). Tests that
# write-memory-entry.sh emits a gate artifact (and gate-flip event) on
# every successful write OR --empty-pass declaration, and that dedup hits
# (rc=2) do NOT pollute the provenance trail.
#
# Run: bash tests/memory/test_memory_pass_writer.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT_REAL="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRITER="${REPO_ROOT_REAL}/scripts/memory/write-memory-entry.sh"

log()  { printf '[memory-writer] %s\n' "$*"; }
fail() { printf '[memory-writer] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[memory-writer] PASS: %s\n' "$*"; }

[[ -x "$WRITER" ]] || fail "writer not executable at $WRITER"

WORK=$(mktemp -d -t memory-writer-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

# Each test runs in its own subdirectory so artifacts don't leak between
# AC paths.
test_dir() {
    local name="$1"
    local d="$WORK/$name"
    mkdir -p "$d/.fno/artifacts"
    mkdir -p "$d/memory"
    echo "$d"
}

# AC1-HP: first entry creates artifact + event
log "AC1-HP: first entry creates memory artifact"
T1=$(test_dir hp1)
SID=20260509T230000Z-12345-aabbcc
ARTIFACTS_DIR="$T1/.fno/artifacts" \
ARTIFACT="$T1/.fno/artifacts/memory-${SID}.md" \
    bash "$WRITER" \
    --memory-dir "$T1/memory" \
    --session-id "$SID" \
    --candidate '{"type":"feedback","name":"Test Entry","description":"Test description","body":"Test body content"}' \
    >"$T1/out.log" 2>&1
RC=$?
[[ "$RC" == "0" ]] || fail "AC1-HP: writer failed rc=$RC, log: $(cat "$T1/out.log")"
[[ -f "$T1/memory/feedback_test_entry.md" ]] || fail "AC1-HP: memory file not created"
[[ -f "$T1/.fno/artifacts/memory-${SID}.md" ]] \
    || fail "AC1-HP: gate artifact not created at $T1/.fno/artifacts/memory-${SID}.md"
grep -q "^phase: memory$" "$T1/.fno/artifacts/memory-${SID}.md" \
    || fail "AC1-HP: artifact missing phase: memory frontmatter"
grep -q "^session_id: ${SID}$" "$T1/.fno/artifacts/memory-${SID}.md" \
    || fail "AC1-HP: artifact missing session_id frontmatter"
grep -q "^entries_written: 1$" "$T1/.fno/artifacts/memory-${SID}.md" \
    || fail "AC1-HP: artifact entries_written != 1"
grep -q "^approved: true$" "$T1/.fno/artifacts/memory-${SID}.md" \
    || fail "AC1-HP: artifact approved != true"
pass "AC1-HP: first entry creates artifact with correct frontmatter"

# AC1-HP-2: subsequent entry within session bumps the count
log "AC1-HP-2: second entry bumps entries_written"
ARTIFACTS_DIR="$T1/.fno/artifacts" \
    bash "$WRITER" \
    --memory-dir "$T1/memory" \
    --session-id "$SID" \
    --candidate '{"type":"feedback","name":"Second Entry","description":"Second","body":"Body 2"}' \
    >"$T1/out2.log" 2>&1
RC=$?
[[ "$RC" == "0" ]] || fail "AC1-HP-2: second writer call failed rc=$RC"
ENTRIES=$(grep -E '^entries_written:[[:space:]]*' "$T1/.fno/artifacts/memory-${SID}.md" \
    | head -1 | sed -E 's/^entries_written:[[:space:]]*//' | tr -d ' ')
[[ "$ENTRIES" == "2" ]] \
    || fail "AC1-HP-2: entries_written expected 2, got '$ENTRIES'"
pass "AC1-HP-2: entries_written bumped to 2 on second write"

# AC2-ERR: dedup hit (exit 2) does NOT touch the artifact
log "AC2-ERR: dedup re-write does not bump entries or rewrite artifact"
ARTIFACT_PATH="$T1/.fno/artifacts/memory-${SID}.md"
PRE_MTIME=$(stat -f "%m" "$ARTIFACT_PATH" 2>/dev/null || stat -c "%Y" "$ARTIFACT_PATH" 2>/dev/null)
sleep 1  # ensure mtime resolution can detect a touch
ARTIFACTS_DIR="$T1/.fno/artifacts" \
    bash "$WRITER" \
    --memory-dir "$T1/memory" \
    --session-id "$SID" \
    --candidate '{"type":"feedback","name":"Test Entry","description":"Test description","body":"Test body content"}' \
    >"$T1/out_dedup.log" 2>&1
RC=$?
[[ "$RC" == "2" ]] || fail "AC2-ERR: dedup expected rc=2, got rc=$RC"
POST_MTIME=$(stat -f "%m" "$ARTIFACT_PATH" 2>/dev/null || stat -c "%Y" "$ARTIFACT_PATH" 2>/dev/null)
[[ "$PRE_MTIME" == "$POST_MTIME" ]] \
    || fail "AC2-ERR: dedup touched the artifact mtime ($PRE_MTIME -> $POST_MTIME)"
ENTRIES_AFTER=$(grep -E '^entries_written:[[:space:]]*' "$ARTIFACT_PATH" \
    | head -1 | sed -E 's/^entries_written:[[:space:]]*//' | tr -d ' ')
[[ "$ENTRIES_AFTER" == "2" ]] \
    || fail "AC2-ERR: dedup advanced entries_written ($ENTRIES_AFTER, expected 2)"
pass "AC2-ERR: dedup leaves artifact mtime + entries_written unchanged"

# AC4-EDGE: --empty-pass writes artifact with entries_written=0
log "AC4-EDGE: --empty-pass writes legitimate empty-pass artifact"
T2=$(test_dir empty)
SID2=20260509T230500Z-22222-ddeeff
bash "$WRITER" --empty-pass --session-id "$SID2" \
    >"$T2/out.log" 2>&1
RC=$?
[[ "$RC" == "0" ]] || fail "AC4-EDGE: --empty-pass exit rc=$RC, log: $(cat "$T2/out.log")"
# The writer resolves ARTIFACTS_DIR via $REPO_ROOT_RESOLVED/.fno/artifacts.
# Since we run from the real repo root, the artifact lands in the real
# tree's path. Find it under any artifacts/ in the worktree to be safe:
ARTIFACT_REAL_PATH="${REPO_ROOT_REAL}/.fno/artifacts/memory-${SID2}.md"
[[ -f "$ARTIFACT_REAL_PATH" ]] \
    || fail "AC4-EDGE: empty-pass artifact not at $ARTIFACT_REAL_PATH"
grep -q "^entries_written: 0$" "$ARTIFACT_REAL_PATH" \
    || fail "AC4-EDGE: empty-pass artifact entries_written != 0"
grep -q "^approved: true$" "$ARTIFACT_REAL_PATH" \
    || fail "AC4-EDGE: empty-pass artifact approved != true"
# Cleanup so this leftover doesn't interfere with the live target session
rm -f "$ARTIFACT_REAL_PATH"
pass "AC4-EDGE: empty-pass writes artifact with entries_written=0 approved=true"

# AC2-ERR-2: missing required fields rejected with rc=1
log "AC2-ERR-2: missing required args fail with rc=1"
T3=$(test_dir missing)
RC=0
bash "$WRITER" --memory-dir "$T3/memory" >/dev/null 2>&1 || RC=$?
[[ "$RC" == "1" ]] || fail "AC2-ERR-2: missing args expected rc=1, got rc=$RC"
pass "AC2-ERR-2: missing required args rejected"

# AC2-ERR-3: --empty-pass without session-id rejected
log "AC2-ERR-3: --empty-pass without session-id fails with rc=1"
RC=0
bash "$WRITER" --empty-pass >/dev/null 2>&1 || RC=$?
[[ "$RC" == "1" ]] || fail "AC2-ERR-3: empty-pass-no-sid expected rc=1, got rc=$RC"
pass "AC2-ERR-3: --empty-pass without session-id correctly rejected"

# AC-DEDUP-FIRST: dedup hit on the LLM's first writer call this session
# emits a zero-entry passing gate artifact so strict mode doesn't block on
# a no-op. Without this fix, an honest session whose only candidate happens
# to dedup against a prior-session entry would never satisfy the strict
# memory_pass_passed gate (silent-failure-hunter Finding 3).
log "AC-DEDUP-FIRST: first-call dedup emits zero-entry gate artifact"
T4=$(test_dir dedup_first)
SID4=20260509T235500Z-44444-eeeeee
# Pre-seed the memory file (as if a prior session wrote it). The current
# session's first writer call will dedup against this.
mkdir -p "$T4/memory"
cat > "$T4/memory/feedback_seed_entry.md" <<MEM
---
name: Seed Entry
description: Seed description
type: feedback
auto_generated: true
source_session: prior-session
created_at: 2026-05-08T00:00:00Z
---
Seed body content
MEM
ARTIFACT_PATH4="$T4/.fno/artifacts/memory-${SID4}.md"
[[ ! -f "$ARTIFACT_PATH4" ]] || fail "AC-DEDUP-FIRST setup: artifact pre-existed"

# The LLM's only candidate dedups against the seed.
RC=0
ARTIFACTS_DIR="$T4/.fno/artifacts" \
    bash "$WRITER" \
    --memory-dir "$T4/memory" \
    --session-id "$SID4" \
    --candidate '{"type":"feedback","name":"Seed Entry","description":"Seed description","body":"Seed body content"}' \
    >"$T4/dedup-first.log" 2>&1 || RC=$?
[[ "$RC" == "2" ]] || fail "AC-DEDUP-FIRST: expected dedup rc=2, got rc=$RC, log: $(cat "$T4/dedup-first.log")"
[[ -s "$ARTIFACT_PATH4" ]] \
    || fail "AC-DEDUP-FIRST: dedup-on-first-call did NOT emit gate artifact (strict mode would block COMPLETE)"
grep -q '^entries_written: 0$' "$ARTIFACT_PATH4" \
    || fail "AC-DEDUP-FIRST: dedup-emitted artifact entries_written != 0"
grep -q '^approved: true$' "$ARTIFACT_PATH4" \
    || fail "AC-DEDUP-FIRST: dedup-emitted artifact approved != true"
pass "AC-DEDUP-FIRST: first-call dedup emits zero-entry passing artifact"

# AC-DEDUP-SUBSEQUENT: dedup AFTER an artifact already exists must NOT touch it.
log "AC-DEDUP-SUBSEQUENT: subsequent dedup leaves existing artifact alone"
ART_PRE_MTIME=$(stat -f "%m" "$ARTIFACT_PATH4" 2>/dev/null || stat -c "%Y" "$ARTIFACT_PATH4" 2>/dev/null)
sleep 1
RC=0
ARTIFACTS_DIR="$T4/.fno/artifacts" \
    bash "$WRITER" \
    --memory-dir "$T4/memory" \
    --session-id "$SID4" \
    --candidate '{"type":"feedback","name":"Seed Entry","description":"Seed description","body":"Seed body content"}' \
    >/dev/null 2>&1 || RC=$?
[[ "$RC" == "2" ]] || fail "AC-DEDUP-SUBSEQUENT: expected rc=2, got rc=$RC"
ART_POST_MTIME=$(stat -f "%m" "$ARTIFACT_PATH4" 2>/dev/null || stat -c "%Y" "$ARTIFACT_PATH4" 2>/dev/null)
[[ "$ART_PRE_MTIME" == "$ART_POST_MTIME" ]] \
    || fail "AC-DEDUP-SUBSEQUENT: dedup touched the artifact ($ART_PRE_MTIME -> $ART_POST_MTIME)"
pass "AC-DEDUP-SUBSEQUENT: subsequent dedup preserves artifact mtime"

# AC-TYPES: all four documented memory types are accepted (cv-c97d73e3).
# Previously only feedback|project were allowed; reference|user were rejected
# with a SILENT exit 1 (set -e aborted the parse-capture assignment before the
# ERR line could be surfaced).
log "AC-TYPES: reference + user types are accepted and written"
T5=$(test_dir types)
SID5=20260604T000000Z-55555-ffffff
for ty in reference user feedback project; do
    RC=0
    bash "$WRITER" --memory-dir "$T5/memory" --session-id "$SID5" --repo-root "$T5" \
        --candidate "{\"type\":\"$ty\",\"name\":\"Typed $ty\",\"description\":\"d\",\"body\":\"b\"}" \
        >"$T5/types-$ty.log" 2>&1 || RC=$?
    [[ "$RC" == "0" ]] || fail "AC-TYPES: type=$ty rejected rc=$RC, log: $(cat "$T5/types-$ty.log")"
    [[ -f "$T5/memory/${ty}_typed_${ty}.md" ]] || fail "AC-TYPES: type=$ty memory file not written"
done
pass "AC-TYPES: reference + user (+ feedback + project) types accepted and written"

# AC-ERR-SURFACED: an invalid type fails rc=1 AND surfaces the reason on stderr
# (the swallowed-silent-exit-1 half of cv-c97d73e3).
log "AC-ERR-SURFACED: invalid type surfaces a reason, not a silent exit 1"
T6=$(test_dir badtype)
SID6=20260604T000100Z-66666-aaaaaa
RC=0
ERRTXT=$(bash "$WRITER" --memory-dir "$T6/memory" --session-id "$SID6" --repo-root "$T6" \
    --candidate '{"type":"bogus","name":"x","description":"d","body":"b"}' 2>&1 >/dev/null) || RC=$?
[[ "$RC" == "1" ]] || fail "AC-ERR-SURFACED: invalid type expected rc=1, got rc=$RC"
printf '%s' "$ERRTXT" | grep -qiE 'invalid type' \
    || fail "AC-ERR-SURFACED: invalid type produced no surfaced reason (silent), got: '$ERRTXT'"
pass "AC-ERR-SURFACED: invalid type rc=1 + reason surfaced on stderr"

# AC-ERR-FIELD: a missing required field also surfaces a reason (not silent).
log "AC-ERR-FIELD: missing field surfaces which field"
T7=$(test_dir badfield)
RC=0
ERRTXT=$(bash "$WRITER" --memory-dir "$T7/memory" --session-id "x" --repo-root "$T7" \
    --candidate '{"type":"reference","name":"x","description":"","body":"b"}' 2>&1 >/dev/null) || RC=$?
[[ "$RC" == "1" ]] || fail "AC-ERR-FIELD: missing field expected rc=1, got rc=$RC"
printf '%s' "$ERRTXT" | grep -qiE 'missing required field' \
    || fail "AC-ERR-FIELD: missing field produced no surfaced reason, got: '$ERRTXT'"
pass "AC-ERR-FIELD: missing field rc=1 + reason surfaced"

# AC-NONSTRING: a non-string field (codex P2 on PR #435) is rejected with a
# structured ERR at validation - it must NOT crash python mid-output after the
# header lines, which the shell would otherwise read as a successful parse and
# write a broken memory file + flip the memory gate.
log "AC-NONSTRING: non-string body rejected, no file, no gate artifact"
T8=$(test_dir nonstring)
SID8=20260604T000200Z-77777-bbbbbb
RC=0
ERRTXT=$(bash "$WRITER" --memory-dir "$T8/memory" --session-id "$SID8" --repo-root "$T8" \
    --candidate '{"type":"reference","name":"x","description":"d","body":{"oops":1}}' 2>&1 >/dev/null) || RC=$?
[[ "$RC" == "1" ]] || fail "AC-NONSTRING: expected rc=1, got rc=$RC"
printf '%s' "$ERRTXT" | grep -qiE 'non-string field' \
    || fail "AC-NONSTRING: no surfaced reason, got: '$ERRTXT'"
# A rejected candidate must leave NO memory file and NO gate artifact.
MEMCOUNT=$(find "$T8/memory" -type f 2>/dev/null | wc -l | tr -d ' ')
[[ "$MEMCOUNT" == "0" ]] || fail "AC-NONSTRING: memory file written for rejected candidate"
[[ ! -f "$T8/.fno/artifacts/memory-${SID8}.md" ]] \
    || fail "AC-NONSTRING: gate artifact written for rejected candidate"
pass "AC-NONSTRING: non-string body -> rc=1 + reason, no file, no artifact"

echo "[memory-writer] all memory writer tests passed"
exit 0
