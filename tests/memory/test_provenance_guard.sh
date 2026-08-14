#!/usr/bin/env bash
# tests/memory/test_provenance_guard.sh
#
# x-8fc0: an autonomous writer must never silently rewrite a human-authored
# memory entry. Verify by making it fail: seed a hand-written entry (no
# auto_generated: true tag, the way a human editing the file directly would
# leave it), then try to update it with a matching --source-sha256 (proving
# read-before-write was satisfied) and assert the writer STILL refuses the
# live write and stages the proposal instead.
#
# Run: bash tests/memory/test_provenance_guard.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRITER="${REPO_ROOT}/scripts/memory/write-memory-entry.sh"

log()  { printf '[provenance-guard] %s\n' "$*"; }
fail() { printf '[provenance-guard] FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf '[provenance-guard] PASS: %s\n' "$*"; }

[[ -x "$WRITER" ]] || fail "writer not executable at $WRITER"

sha256_of() {
    shasum -a 256 "$1" 2>/dev/null | awk '{print $1}' \
        || sha256sum "$1" | awk '{print $1}'
}

WORK=$(mktemp -d -t provenance-guard-XXXXXX)
trap 'rm -rf "$WORK"' EXIT
MEM="$WORK/memory"
mkdir -p "$MEM"

# ── AC-HUMAN-REFUSED: a hand-written entry (no auto_generated tag) must
# never be silently rewritten by an autonomous update, even with a valid
# read-before-write proof.
log "AC-HUMAN-REFUSED: autonomous update refuses a human-authored entry"
cat > "$MEM/feedback_human_note.md" <<'MEM'
---
name: feedback_human_note
description: a note Jason wrote by hand
type: feedback
created_at: 2026-08-01T00:00:00Z
---
Jason wrote this directly with an editor. No auto_generated tag.
MEM
PRE_CONTENT=$(cat "$MEM/feedback_human_note.md")
SHA=$(sha256_of "$MEM/feedback_human_note.md")

RC=0
bash "$WRITER" --memory-dir "$MEM" --session-id sid-human-1 \
    --source-sha256 "$SHA" \
    --candidate '{"type":"feedback","name":"human_note","description":"autonomous rewrite attempt","body":"An autonomous pass tried to overwrite this."}' \
    >"$WORK/out.log" 2>&1 || RC=$?
[[ "$RC" == "3" ]] || fail "expected rc=3 (staged), got rc=$RC, log: $(cat "$WORK/out.log")"
[[ "$(cat "$MEM/feedback_human_note.md")" == "$PRE_CONTENT" ]] \
    || fail "live human-authored entry was mutated"
[[ -f "$MEM/.staged/feedback_human_note.md" ]] \
    || fail "proposed update was not staged for human review"
grep -q "An autonomous pass tried to overwrite this" "$MEM/.staged/feedback_human_note.md" \
    || fail "staged file does not carry the proposed content"
pass "AC-HUMAN-REFUSED: human-authored entry untouched; proposal staged"

# ── AC-AUTO-ALLOWED: the SAME shape (existing file, matching hash) but the
# existing entry IS auto_generated: true must proceed normally (not a
# blanket refusal on every update - only on human-authored ones).
log "AC-AUTO-ALLOWED: an update to an auto_generated entry proceeds"
cat > "$MEM/feedback_auto_note.md" <<'MEM'
---
name: feedback_auto_note
description: written by a prior autonomous pass
type: feedback
auto_generated: true
source_session: sid-prior
created_at: 2026-08-01T00:00:00Z
---
Original autonomous body.
MEM
SHA2=$(sha256_of "$MEM/feedback_auto_note.md")
RC=0
bash "$WRITER" --memory-dir "$MEM" --session-id sid-auto-2 \
    --source-sha256 "$SHA2" \
    --candidate '{"type":"feedback","name":"auto_note","description":"updated by a later pass","body":"Revised autonomous body."}' \
    >"$WORK/out2.log" 2>&1 || RC=$?
[[ "$RC" == "0" ]] || fail "expected rc=0 (updated), got rc=$RC, log: $(cat "$WORK/out2.log")"
grep -q "Revised autonomous body" "$MEM/feedback_auto_note.md" \
    || fail "auto_generated entry was not updated"
pass "AC-AUTO-ALLOWED: update to a prior autonomous entry proceeds normally"

echo "[provenance-guard] all provenance guard tests passed"
exit 0
