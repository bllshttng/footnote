#!/usr/bin/env bash
# Tests for the shipped-skill accept route in scripts/autocorrect-triage.sh:
# an accepted item whose Target file is a shipped skill in this repo does not
# git apply into TARGET_DIR; triage files a backlog node through fno (stubbed)
# and stages nothing. A same-named skill under both trees defers, a missing
# fno degrades to a printed file-by-hand line, and a rules/ item still
# applies locally. Isolates via FNO_HOME + CLAUDE_DIR_OVERRIDE so the real
# roots are never touched.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TRIAGE="$SCRIPT_DIR/../autocorrect-triage.sh"
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

# A fake fno: records its argv (and any --details-file payload) and mints a
# fixed node id, so the filing is observable without a live graph store.
make_stub() {
    local bin="$1"
    mkdir -p "$bin"
    cat > "$bin/fno" <<'EOF'
#!/usr/bin/env bash
{
    echo "argv: $*"
    prev=""
    for a in "$@"; do
        if [[ "$prev" == "--details-file" && -f "$a" ]]; then
            echo "details-file: $a"
            cat "$a"
        fi
        prev="$a"
    done
} >> "$TRIAGE_STUB_LOG"
echo '{"id": "x-0001"}'
EOF
    chmod +x "$bin/fno"
}

# A footnote-shaped repo: the shipped skills tree plus the triage script and
# its lib, so SCRIPT_DIR/.. resolves to the repo root like the real install.
make_repo() {
    local root="$1"
    git -C "$root" init -q
    git -C "$root" config user.email test@example.com
    git -C "$root" config user.name test
}

write_item() {
    # write_item <patch-file> <target-file-path>
    cat > "$1" <<EOF
1. Source: target-postmortem /pm/a.md
   Target file: $2
   Action: a

\`\`\`diff
--- a/$2
+++ b/$2
@@ -1 +1,2 @@
 placeholder
+corrected line
\`\`\`
EOF
}

D=$(mktemp -d)
trap 'rm -rf "$D"' EXIT
mkdir -p "$D/repo/skills/target" "$D/repo/scripts" "$D/claude/rules" "$D/fno"
cp "$TRIAGE" "$D/repo/scripts/"
cp -R "$SCRIPT_DIR/../lib" "$D/repo/scripts/lib"
make_repo "$D/repo"
make_repo "$D/claude"
printf 'placeholder\n' > "$D/repo/skills/target/SKILL.md"
printf 'placeholder\n' > "$D/claude/rules/style.md"
git -C "$D/claude" add rules
git -C "$D/claude" commit -qm seed
export TRIAGE_STUB_LOG="$D/fno/stub-calls.log"
: > "$TRIAGE_STUB_LOG"
make_stub "$D/bin"

run_triage() {
    # run_triage <review-id> <action keys, one per item>
    local review_id="$1"
    shift
    printf '%s\n' "$@" | CLAUDE_DIR_OVERRIDE="$D/claude" FNO_HOME="$D/fno" \
        PATH="$D/bin:$PATH" bash "$D/repo/scripts/autocorrect-triage.sh" \
        --review-id "$review_id" 2> "$D/fno/triage.err"
}

# ---- T01 (AC1-HP): accepted shipped-skill item files a node, applies nothing ----
echo "T01: shipped-skill accept files one node and stages nothing"
mkdir -p "$D/claude/proposed-patches"
write_item "$D/claude/proposed-patches/r1.md" "skills/target/SKILL.md"
run_triage r1 a
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "triage exited 0"
else
    fail "triage exited $RC"
fi
CALLS=$(awk '/backlog idea/ { n++ } END { print n + 0 }' "$TRIAGE_STUB_LOG")
if [[ "$CALLS" -eq 1 ]]; then
    pass "stub saw exactly one backlog idea call"
else
    fail "expected 1 stub call, got $CALLS"
fi
if grep -q -- '--origin-evidence autocorrect:r1#1' "$TRIAGE_STUB_LOG" 2>/dev/null; then
    pass "call carries the origin-evidence ref"
else
    fail "origin-evidence ref missing"
fi
if grep -q -- '--source-kind operator_request' "$TRIAGE_STUB_LOG" 2>/dev/null; then
    pass "call is an operator request"
else
    fail "source-kind missing"
fi
if tail -1 "$TRIAGE_STUB_LOG" 2>/dev/null | grep -qF 'Commit this edit with the trailer: Autocorrect-Ref: r1#1'; then
    pass "details end with the Autocorrect-Ref line"
else
    fail "trailer line missing from details"
fi
if grep -q 'filed as x-0001' "$D/fno/triage.err" 2>/dev/null; then
    pass "minted id printed"
else
    fail "minted id not printed"
fi
if [[ -z "$(git -C "$D/claude" status --porcelain -- rules skills)" ]]; then
    pass "TARGET_DIR untouched"
else
    fail "TARGET_DIR changed"
fi
if grep -q 'placeholder' "$D/repo/skills/target/SKILL.md" && ! grep -q 'corrected line' "$D/repo/skills/target/SKILL.md"; then
    pass "shipped skill file untouched"
else
    fail "shipped skill file changed"
fi

# ---- T02 (AC1-EDGE): the skill under both trees defers ----
echo "T02: skill present under both shipped tree and TARGET_DIR defers"
BEFORE_CALLS=$(wc -l < "$TRIAGE_STUB_LOG" | tr -d ' ')
mkdir -p "$D/claude/skills/target"
printf 'user copy\n' > "$D/claude/skills/target/SKILL.md"
write_item "$D/claude/proposed-patches/r2.md" "skills/target/SKILL.md"
run_triage r2 a
RC=$?
AFTER_CALLS=$(wc -l < "$TRIAGE_STUB_LOG" | tr -d ' ')
if [[ $RC -eq 0 && "$AFTER_CALLS" -eq "$BEFORE_CALLS" ]]; then
    pass "nothing filed, exit 0"
else
    fail "expected no filing (calls $BEFORE_CALLS -> $AFTER_CALLS, rc $RC)"
fi
if grep -q 'deferring rather than guessing' "$D/fno/triage.err" 2>/dev/null; then
    pass "both paths named and deferred"
else
    fail "defer reason missing"
fi
if grep -q 'user copy' "$D/claude/skills/target/SKILL.md"; then
    pass "user skill file untouched"
else
    fail "user skill file changed"
fi

# ---- T03 (AC1-ERR): no fno on PATH degrades to file-by-hand, exit 0 ----
echo "T03: fno absent prints the file-by-hand line and changes nothing"
rm -rf "$D/claude/skills"
write_item "$D/claude/proposed-patches/r3.md" "skills/target/SKILL.md"
printf 'a\n' | CLAUDE_DIR_OVERRIDE="$D/claude" FNO_HOME="$D/fno" \
    PATH=/usr/bin:/bin bash "$D/repo/scripts/autocorrect-triage.sh" \
    --review-id r3 > /dev/null 2> "$D/fno/triage3.err"
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "exit 0 without fno"
else
    fail "exit $RC without fno"
fi
if grep -q 'file this by hand' "$D/fno/triage3.err" && grep -q 'fno backlog idea' "$D/fno/triage3.err"; then
    pass "file-by-hand line printed with the invocation"
else
    fail "file-by-hand line missing"
fi

# ---- T04: a rules/ item still applies locally (fall-through intact) ----
echo "T04: rules/ item routes to the local apply"
printf 'placeholder\n' > "$D/claude/rules/style.md"
write_item "$D/claude/proposed-patches/r4.md" "rules/style.md"
BEFORE_CALLS=$(wc -l < "$TRIAGE_STUB_LOG" | tr -d ' ')
run_triage r4 a
RC=$?
AFTER_CALLS=$(wc -l < "$TRIAGE_STUB_LOG" | tr -d ' ')
if [[ $RC -eq 0 && "$AFTER_CALLS" -eq "$BEFORE_CALLS" ]]; then
    pass "no filing for a rules/ item"
else
    fail "rules/ item filed a node"
fi
if grep -q 'corrected line' "$D/claude/rules/style.md"; then
    pass "diff applied into TARGET_DIR"
else
    fail "rules/ diff not applied"
fi

summary
exit $?
