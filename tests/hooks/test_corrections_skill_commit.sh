#!/usr/bin/env bash
# Tests for the SOURCE resolution in hooks/corrections-git-postcommit.sh:
# the committing repo decides the SOURCE field. A skill edit committed in the
# footnote repo appends a skill-commit row; the same hook in ~/.claude keeps
# git-rule-edit; and the hook never blocks a commit whatever the log does.
# Isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE so the real roots are never
# touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOK="$SCRIPT_DIR/../../hooks/corrections-git-postcommit.sh"
PASS=0
FAIL=0

pass() {
    echo "  PASS: $1"
    PASS=$((PASS + 1))
}
fail() {
    echo "  FAIL: $1"
    FAIL=$((FAIL + 1))
}
summary() {
    echo ""
    echo "PASS=$PASS FAIL=$FAIL"
    [[ $FAIL -eq 0 ]]
}

make_repo() {
    local root="$1"
    git -C "$root" init -q
    git -C "$root" config user.email test@example.com
    git -C "$root" config user.name test
}

commit_skill() {
    local root="$1"
    mkdir -p "$root/skills/target"
    printf 'placeholder\n' > "$root/skills/target/SKILL.md"
    git -C "$root" add -A
    git -C "$root" commit -qm "test: touch a shipped skill"
}

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/fno" "$D/claude" "$D/repo"
LOG="$D/fno/corrections.log"
# The hook never bootstraps the log (the install script's job); autocorrect
# created it in real usage, so the test creates it here.
: > "$LOG"

# ---- T01 (AC7-HP): a skill commit in another repo appends skill-commit ----
echo "T01: skill-commit row for a skill edit in the footnote repo"
make_repo "$D/repo"
commit_skill "$D/repo"
( cd "$D/repo" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" )
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "hook exited 0"
else
    fail "hook exited $RC"
fi
if grep -q '| skill-commit | skills/target/SKILL.md |' "$LOG" 2>/dev/null; then
    pass "one skill-commit row for skills/target/SKILL.md"
else
    fail "skill-commit row missing"
fi
ROWS=$(grep -c 'skill-commit | skills/target/SKILL.md' "$LOG" 2>/dev/null || echo 0)
if [[ "$ROWS" -eq 1 ]]; then
    pass "exactly one row"
else
    fail "expected 1 row, got $ROWS"
fi

# ---- T02 (AC7-EDGE): the same hook in the claude repo stays git-rule-edit ----
echo "T02: git-rule-edit preserved in the claude repo"
make_repo "$D/claude"
commit_skill "$D/claude"
( cd "$D/claude" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" )
if grep -q '| git-rule-edit | skills/target/SKILL.md |' "$LOG" 2>/dev/null; then
    pass "git-rule-edit row for the claude repo"
else
    fail "git-rule-edit row missing"
fi

# ---- T03 (AC7-ERR): absent log never blocks ----
echo "T03: absent log -> exit 0, nothing written"
rm -f "$LOG"
( cd "$D/repo" && git -C "$D/repo" commit -qm "empty: nothing changed" --allow-empty )
( cd "$D/repo" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" )
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "hook exits 0 with no log"
else
    fail "hook exited $RC with no log"
fi

# ---- T04 (AC7-ERR): unwritable log never blocks ----
echo "T04: unwritable log -> exit 0"
printf 'seed\n' > "$LOG"
chmod 400 "$LOG"
( cd "$D/repo" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" 2>/dev/null )
RC=$?
chmod 644 "$LOG"
if [[ $RC -eq 0 ]]; then
    pass "hook exits 0 with an unwritable log"
else
    fail "hook exited $RC with an unwritable log"
fi

# Rebuild the log the token cases append to.
: > "$LOG"

# ---- T05 (AC2-HP): trailer lands sha= and ref= on the row ----
echo "T05: Autocorrect-Ref trailer appends sha= and ref="
( cd "$D/repo" && printf 'fixed\n' > skills/target/SKILL.md \
    && git add skills/target/SKILL.md \
    && git commit -qm "autocorrect r1#1: correct the target skill" --trailer "Autocorrect-Ref: r1#1" )
( cd "$D/repo" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" )
if grep -Eq '\| skill-commit \| skills/target/SKILL\.md \| autocorrect r1#1: correct the target skill sha=[0-9a-f]{12} ref=r1#1$' "$LOG" 2>/dev/null; then
    pass "row ends with sha=<12 hex> ref=r1#1"
else
    fail "sha/ref tokens missing from the row"
    cat "$LOG" >&2
fi

# ---- T06 (AC2-EDGE): no trailer keeps sha= without ref= ----
echo "T06: no trailer appends sha= only"
( cd "$D/repo" && printf 'fixed2\n' > skills/target/SKILL.md \
    && git add skills/target/SKILL.md \
    && git commit -qm "autocorrect: no trailer this time" )
( cd "$D/repo" && FNO_HOME="$D/fno" CLAUDE_DIR_OVERRIDE="$D/claude" bash "$HOOK" )
if grep -Eq '\| skill-commit \| skills/target/SKILL\.md \| autocorrect: no trailer this time sha=[0-9a-f]{12}$' "$LOG" 2>/dev/null; then
    pass "row ends with sha=<12 hex> and no ref="
else
    fail "bare sha row malformed"
    cat "$LOG" >&2
fi

summary
exit $?
