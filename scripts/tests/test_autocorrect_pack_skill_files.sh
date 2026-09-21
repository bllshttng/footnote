#!/usr/bin/env bash
# Tests for the skill-file leg in scripts/autocorrect-pack.sh: the
# SOURCE field names the verb (target-postmortem -> target), and that verb's
# SKILL.md lands in implicated_rules with its full text, deduped across rows.
# A source that is not *-postmortem, or naming a verb with no skill
# directory, adds nothing. Isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE +
# FNO_GRAPH_PATH so the real ~/.fno and ~/.claude are never touched.

# No pipefail: the assertions grep a large packet through a pipe, and a
# grep -q early exit can SIGPIPE the printf producer under load; pipefail
# would flip that race into a false negative. The producer's status is
# irrelevant here.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PACK="$SCRIPT_DIR/../autocorrect-pack.sh"
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

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/fno/postmortems" "$D/claude"

TS="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
LOG="$D/fno/corrections.log"
real_pm="$D/fno/postmortems/pm-a.md"
printf '# body\n' > "$real_pm"

# Two target-postmortem rows (dedup proof), one non-postmortem source, one
# postmortem source with no skill directory.
{
    printf '%s | S1 | target-postmortem | %s | wall: a\n' "$TS" "$real_pm"
    printf '%s | S1 | target-postmortem | %s | wall: b\n' "$TS" "$real_pm"
    printf '%s | S1 | other-source | %s | wall: c\n' "$TS" "$real_pm"
    printf '%s | S1 | nosuch-postmortem | %s | wall: d\n' "$TS" "$real_pm"
} > "$LOG"

run_pack() {
    CLAUDE_DIR_OVERRIDE="$D/claude" FNO_HOME="$D/fno" FNO_GRAPH_PATH="$D/absent-graph.json" \
        bash "$PACK" --dry-run --window 30d
}

# ---- T01: exactly one entry for the verb's SKILL.md, with full text ----
echo "T01: skills/target/SKILL.md resolved once with full text"
PACKET="$(run_pack)"
if [[ $? -ne 0 ]]; then
    fail "pack exited nonzero"
    summary
    exit 1
fi
COUNT=$(printf '%s\n' "$PACKET" | grep -c -- '- file: .*skills/target/SKILL.md')
if [[ "$COUNT" -eq 1 ]]; then
    pass "exactly one implicated entry for skills/target/SKILL.md"
else
    fail "expected 1 entry for skills/target/SKILL.md, got $COUNT"
fi
if printf '%s\n' "$PACKET" | grep -A1 -- '- file: .*skills/target/SKILL.md' | grep -q 'full_text: |'; then
    pass "entry carries a full_text block"
else
    fail "entry missing full_text"
fi
FIRST_LINE=$(sed -n '2p' "$SCRIPT_DIR/../../skills/target/SKILL.md")
if printf '%s' "$PACKET" | grep -qF "$FIRST_LINE"; then
    pass "full text is the real SKILL.md body"
else
    fail "full text missing the real skill body"
fi

# ---- T02: unresolvable sources add nothing and wedge nothing ----
echo "T02: non-postmortem and missing-skill sources add nothing"
if printf '%s\n' "$PACKET" | grep -q 'skills/other-source\|skills/nosuch'; then
    fail "unresolvable source leaked an entry"
else
    pass "no entry for other-source or nosuch-postmortem"
fi
if printf '%s\n' "$PACKET" | grep -q '<file deleted or not found at packet build time>'; then
    fail "unresolvable source rendered a dead entry"
else
    pass "no dead entries from unresolvable sources"
fi

# ---- T03: packet structure intact ----
echo "T03: packet structure intact"
if printf '%s\n' "$PACKET" | grep -q '^implicated_rules:'; then
    pass "packet structure intact"
else
    fail "packet structure broken"
fi

summary
exit $?
