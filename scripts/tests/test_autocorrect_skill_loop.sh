#!/usr/bin/env bash
# The seeded end-to-end fixture for the shipped-skill correction loop:
# a proposal accepted at triage files a backlog node (stubbed fno) instead
# of applying locally, the shipping commit lands a skill-commit row linked
# back by the Autocorrect-Ref trailer, and the review packet carries the
# skill with its full text. Every root is a temp home; the real ~/.claude
# and ~/.fno are never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
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

# A footnote-shaped repo: shipped skills tree, the two autocorrect scripts,
# the corrections lib, and the post-commit hook.
REPO="$D/repo"
mkdir -p "$REPO/scripts" "$REPO/hooks" "$REPO/skills/target"
cp "$SCRIPT_DIR/../autocorrect-triage.sh" "$REPO/scripts/"
cp "$SCRIPT_DIR/../autocorrect-pack.sh" "$REPO/scripts/"
cp -R "$SCRIPT_DIR/../lib" "$REPO/scripts/lib"
cp "$SCRIPT_DIR/../../hooks/corrections-git-postcommit.sh" "$REPO/hooks/"
git -C "$REPO" init -q
git -C "$REPO" config user.email fixture@example.com
git -C "$REPO" config user.name fixture
printf 'placeholder\n' > "$REPO/skills/target/SKILL.md"
git -C "$REPO" add skills
git -C "$REPO" commit -qm "seed: the shipped skill"

# Temp home: ~/.claude as a git repo (TARGET_DIR), ~/.fno with the log and
# two real postmortem files.
mkdir -p "$D/home/.claude/proposed-patches" "$D/home/.fno/postmortems"
git -C "$D/home/.claude" init -q
git -C "$D/home/.claude" config user.email fixture@example.com
git -C "$D/home/.claude" config user.name fixture
LOG="$D/home/.fno/corrections.log"
TS="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
printf '# first failure\n' > "$D/home/.fno/postmortems/pm-1.md"
printf '# second failure\n' > "$D/home/.fno/postmortems/pm-2.md"
{
    printf '%s | S1 | target-postmortem | %s | NoProgress: worker stalled on CI\n' "$TS" "$D/home/.fno/postmortems/pm-1.md"
    printf '%s | S1 | target-postmortem | %s | Budget: review rounds burned\n' "$TS" "$D/home/.fno/postmortems/pm-2.md"
    printf '%s | S2 | insights-tag | 2026-09-22.md:12 | "stop polling CI by hand" (x2, s9, signal=workflow_friction skill=target) #agent-correction\n' "$TS"
} > "$LOG"

# The review's proposal: one item against the shipped skill.
cat > "$D/home/.claude/proposed-patches/r1.md" <<'EOF'
1. Source: target-postmortem /pm/pm-1.md
   Target file: skills/target/SKILL.md
   Action: a

```diff
--- a/skills/target/SKILL.md
+++ b/skills/target/SKILL.md
@@ -1 +1,2 @@
 placeholder
+corrected line
```
EOF

# A stub fno: records its argv and the details payload, mints a fixed id.
mkdir -p "$D/bin"
cat > "$D/bin/fno" <<'EOF'
#!/usr/bin/env bash
{
    echo "argv: $*"
    prev=""
    for a in "$@"; do
        if [[ "$prev" == "--details-file" && -f "$a" ]]; then
            echo "details-file:"
            cat "$a"
        fi
        prev="$a"
    done
} >> "$STUB_LOG"
echo '{"id": "x-0001"}'
EOF
chmod +x "$D/bin/fno"
export STUB_LOG="$D/home/.fno/stub-calls.log"
: > "$STUB_LOG"

run_triage() {
    printf 'a\n' | HOME="$D/home" CLAUDE_DIR_OVERRIDE="$D/home/.claude" FNO_HOME="$D/home/.fno" \
        PATH="$D/bin:$PATH" bash "$REPO/scripts/autocorrect-triage.sh" --review-id r1 2> "$D/home/.fno/triage.err"
}

echo "T01: triage files the shipped-skill proposal as a node"
run_triage
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "triage exited 0"
else
    fail "triage exited $RC"
    cat "$D/home/.fno/triage.err" >&2
fi
CALLS=$(awk '/backlog idea/ { n++ } END { print n + 0 }' "$STUB_LOG")
if [[ "$CALLS" -eq 1 ]]; then
    pass "exactly one backlog idea call"
else
    fail "expected 1 backlog idea call, got $CALLS"
fi
if grep -q -- '--origin-evidence autocorrect:r1#1' "$STUB_LOG"; then
    pass "the call names the proposal it came from"
else
    fail "origin-evidence missing from the call"
fi
if tail -1 "$STUB_LOG" | grep -qF 'Commit this edit with the trailer: Autocorrect-Ref: r1#1'; then
    pass "the details carry the trailer instruction"
else
    fail "trailer line missing from the details"
fi
if grep -qF 'corrected line' "$REPO/skills/target/SKILL.md"; then
    fail "the skill was patched locally"
else
    pass "nothing applied locally"
fi

echo "T02: the shipping commit lands a linked skill-commit row"
# The hook rides the temp repo as its post-commit.
ln -sf "$REPO/hooks/corrections-git-postcommit.sh" "$REPO/.git/hooks/post-commit"
printf 'placeholder\ncorrected line\n' > "$REPO/skills/target/SKILL.md"
git -C "$REPO" add skills/target/SKILL.md
HOME="$D/home" CLAUDE_DIR_OVERRIDE="$D/home/.claude" FNO_HOME="$D/home/.fno" \
    git -C "$REPO" commit -qm "autocorrect r1#1: correct the target skill" --trailer "Autocorrect-Ref: r1#1"
if grep -Eq '\| skill-commit \| skills/target/SKILL\.md \| .* sha=[0-9a-f]{12} ref=r1#1$' "$LOG"; then
    pass "one skill-commit row with sha= and ref=r1#1"
else
    fail "linked skill-commit row missing"
    cat "$LOG" >&2
fi

echo "T03: the packet carries the skill, resolved and linked"
PACKET="$(HOME="$D/home" CLAUDE_DIR_OVERRIDE="$D/home/.claude" FNO_HOME="$D/home/.fno" \
    FNO_GRAPH_PATH="$D/home/.fno/absent-graph.json" \
    bash "$REPO/scripts/autocorrect-pack.sh" --dry-run --window 30d)"
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "pack exited 0"
else
    fail "pack exited $RC"
fi
COUNT=$(printf '%s\n' "$PACKET" | grep -c -- '- file: .*skills/target/SKILL.md')
if [[ "$COUNT" -eq 1 ]]; then
    pass "skills/target/SKILL.md appears exactly once"
else
    fail "expected 1 skills/target/SKILL.md entry, got $COUNT"
fi
if printf '%s\n' "$PACKET" | grep -qF 'corrected line'; then
    pass "the entry carries the full skill text"
else
    fail "full skill text missing from the packet"
fi
if printf '%s\n' "$PACKET" | grep -q 'not found at packet build time'; then
    fail "a dead entry rendered"
else
    pass "no dead entries"
fi

summary
exit $?
