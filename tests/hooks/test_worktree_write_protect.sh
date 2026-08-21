#!/usr/bin/env bash

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD="$REPO_ROOT/hooks/worktree-write-protect.sh"

TMP_BASE="$(mktemp -d -t worktree-write-protect-XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT

PASS=0
FAIL=0
pass() { printf '  PASS: %s\n' "$*"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL: %s\n' "$*" >&2; FAIL=$((FAIL + 1)); }

make_repo() {
    local dir="$1" branch="${2:-main}"
    mkdir -p "$dir"
    git -C "$dir" init -q -b "$branch"
    git -C "$dir" config user.email test@example.com
    git -C "$dir" config user.name Test
    printf '# fixture\n' > "$dir/README.md"
    git -C "$dir" add README.md
    git -C "$dir" commit -q -m init
}

payload() {
    local command="${2:-*** Begin Patch
*** Update File: README.md
*** End Patch}"
    jq -nc --arg cwd "$1" --arg command "$command" '{
        cwd: $cwd,
        tool_name: "apply_patch",
        tool_input: {command: $command}
    }'
}

run_guard() {
    printf '%s' "$1" | bash "$GUARD"
}

assert_single_json() {
    local name="$1" output="$2"
    if ! printf '%s' "$output" | jq -e . >/dev/null 2>&1 \
        || [[ "$(printf '%s' "$output" | jq -s 'length')" != "1" ]]; then
        fail "$name emits one JSON document: $output"
        return 1
    fi
}

assert_allow() {
    local name="$1" input="$2" output rc
    output="$(run_guard "$input")"
    rc=$?
    if [[ $rc -ne 0 ]]; then
        fail "$name exits zero (got $rc)"
        return
    fi
    assert_single_json "$name" "$output" || return
    if ! printf '%s' "$output" | jq -e 'type == "object" and length == 0' >/dev/null; then
        fail "$name allows with an empty object: $output"
        return
    fi
    pass "$name"
}

assert_block() {
    local name="$1" input="$2" output rc
    output="$(run_guard "$input")"
    rc=$?
    if [[ $rc -ne 0 ]]; then
        fail "$name exits zero (got $rc)"
        return
    fi
    assert_single_json "$name" "$output" || return
    if ! printf '%s' "$output" | jq -e '
        .decision == "block"
        and (.reason | type == "string" and length > 0)
        and .hookSpecificOutput.hookEventName == "PreToolUse"
        and .hookSpecificOutput.permissionDecision == "deny"
        and .hookSpecificOutput.permissionDecisionReason == .reason
    ' >/dev/null; then
        fail "$name emits a valid deny response: $output"
        return
    fi
    pass "$name"
}

echo "=== worktree write guard ==="

if [[ ! -f "$GUARD" ]]; then
    echo "FAIL: guard not found at $GUARD" >&2
    exit 1
fi

CANONICAL="$TMP_BASE/canonical"
make_repo "$CANONICAL"
assert_block "canonical main blocks" "$(payload "$CANONICAL")"

MAIN_OUTPUT="$(run_guard "$(payload "$CANONICAL")")"
MAIN_REASON="$(printf '%s' "$MAIN_OUTPUT" | jq -r '.reason')"
if [[ "$MAIN_REASON" == *'fno do target start <node>'* \
    && "$MAIN_REASON" == *'worktree='* \
    && "$MAIN_REASON" == *'Codex Worktree mode'* \
    && "$MAIN_REASON" == *'Handoff'* ]]; then
    pass "block reason explains both usable relocation paths"
else
    fail "block reason is not actionable: $MAIN_REASON"
fi

MASTER="$TMP_BASE/master"
make_repo "$MASTER" master
assert_block "canonical master blocks" "$(payload "$MASTER")"

DETACHED="$TMP_BASE/detached"
make_repo "$DETACHED"
git -C "$DETACHED" checkout -q --detach
assert_block "canonical detached HEAD blocks" "$(payload "$DETACHED")"

FEATURE="$TMP_BASE/feature"
make_repo "$FEATURE"
git -C "$FEATURE" checkout -q -b feature/allowed
assert_allow "canonical feature branch allows" "$(payload "$FEATURE")"

LINK_CANONICAL="$TMP_BASE/linked-canonical"
LINKED="$TMP_BASE/arbitrary linked path"
make_repo "$LINK_CANONICAL"
git -C "$LINK_CANONICAL" worktree add -q "$LINKED" -b feature/linked
assert_allow "arbitrary-base linked worktree allows" "$(payload "$LINKED")"
assert_allow \
    "canonical session can patch a linked worktree by absolute path" \
    "$(payload "$LINK_CANONICAL" "*** Begin Patch
*** Update File: $LINKED/README.md
*** End Patch")"
assert_block \
    "canonical session still blocks an absolute canonical target" \
    "$(payload "$LINK_CANONICAL" "*** Begin Patch
*** Update File: $LINK_CANONICAL/README.md
*** End Patch")"
assert_block \
    "linked worktree cannot patch canonical checkout by absolute path" \
    "$(payload "$LINKED" "*** Begin Patch
*** Update File: $LINK_CANONICAL/README.md
*** End Patch")"
assert_block \
    "linked worktree cannot patch canonical checkout by parent traversal" \
    "$(payload "$LINKED" "*** Begin Patch
*** Update File: ../linked-canonical/README.md
*** End Patch")"
ln -s "$LINK_CANONICAL/README.md" "$LINKED/canonical-readme-link"
assert_block \
    "linked worktree cannot patch canonical checkout through a symlink" \
    "$(payload "$LINKED" "*** Begin Patch
*** Update File: canonical-readme-link
*** End Patch")"
mkdir -p "$LINK_CANONICAL/subdir"
ln -s ../README.md "$LINK_CANONICAL/subdir/final-link"
ln -s "$LINK_CANONICAL/subdir" "$LINKED/canonical-dir-alias"
assert_block \
    "linked worktree cannot patch canonical checkout through a relative symlink chain" \
    "$(payload "$LINKED" "*** Begin Patch
*** Update File: canonical-dir-alias/final-link
*** End Patch")"
git -C "$LINKED" checkout -q --detach
assert_allow "detached linked worktree allows" "$(payload "$LINKED")"

NO_TARGET_PAYLOAD="$(jq -nc --arg cwd "$LINK_CANONICAL" '{
    cwd: $cwd,
    tool_name: "apply_patch",
    tool_input: {command: ""}
}')"
assert_block "canonical cwd remains the fail-closed fallback without targets" "$NO_TARGET_PAYLOAD"

# ── the object decides, not the session ──────────────────────────────────────
# One protected root per session (the canonical checkout of this git common
# dir). Every target is judged as a PHYSICAL path against that root, and a
# target inside it is permitted only when git positively confirms it ignored.
GUARDED="$TMP_BASE/guarded"
make_repo "$GUARDED"
printf '.fno/\nscratch/\n*.trace\n' > "$GUARDED/.gitignore"
mkdir -p "$GUARDED/src" "$GUARDED/scratch" "$GUARDED/.fno"
printf 'x = 1\n' > "$GUARDED/src/app.py"
printf 'note\n' > "$GUARDED/scratch/note.md"
printf 'forced\n' > "$GUARDED/forced.trace"
git -C "$GUARDED" add .gitignore src/app.py
git -C "$GUARDED" add -f forced.trace
git -C "$GUARDED" commit -q -m fixtures

EXTERNAL="$TMP_BASE/external"
VAULT="$TMP_BASE/vault"
mkdir -p "$EXTERNAL" "$VAULT/plans"
ln -s "$VAULT" "$GUARDED/internal"
ln -s "$VAULT/plans" "$GUARDED/scratch/vault-alias"
ln -s "$GUARDED/src/app.py" "$EXTERNAL/back-into-canonical"
ln -s "$GUARDED/src/app.py" "$GUARDED/scratch/tracked-alias"
ln "$GUARDED/src/app.py" "$GUARDED/scratch/tracked-hardlink"
ln "$GUARDED/scratch/note.md" "$GUARDED/scratch/ignored-hardlink"
ln -s loop-b "$GUARDED/scratch/loop-a"
ln -s loop-a "$GUARDED/scratch/loop-b"
GUARDED_LINKED="$TMP_BASE/guarded-linked"
git -C "$GUARDED" worktree add -q "$GUARDED_LINKED" -b feature/guarded

guarded() { payload "$GUARDED" "*** Begin Patch
$1
*** End Patch"; }

# AC1-HP: physically external targets are not this project's object.
assert_allow "external non-git target allows" "$(guarded "*** Add File: $EXTERNAL/note.txt")"
assert_allow "vault target through an in-project symlink allows" \
    "$(guarded "*** Add File: internal/plans/new-plan.md")"
assert_allow "registered linked worktree target allows" \
    "$(guarded "*** Update File: $GUARDED_LINKED/README.md")"

# AC2-HP: positively ignored in-project targets, existing and not.
assert_allow "existing ignored target allows" "$(guarded "*** Update File: scratch/note.md")"
assert_allow "nonexistent ignored leaf allows" "$(guarded "*** Add File: .fno/what-if/result.json")"
assert_allow "absolute nonexistent ignored leaf allows" \
    "$(guarded "*** Add File: $GUARDED/scratch/deep/new/file.txt")"

# AC3-CON: trackable canonical content stays blocked.
assert_block "tracked target blocks" "$(guarded "*** Update File: src/app.py")"
assert_block "unignored new target blocks" "$(guarded "*** Add File: src/new_module.py")"
assert_block "checkout root as target blocks" "$(guarded "*** Update File: $GUARDED")"
assert_block "force-added tracked file matching an ignore pattern blocks" \
    "$(guarded "*** Update File: forced.trace")"

# AC4-EDGE: the physical destination decides, in both directions.
assert_allow "ignored symlink to an external vault allows" \
    "$(guarded "*** Add File: scratch/vault-alias/y.md")"
assert_block "external symlink resolving into tracked content blocks" \
    "$(guarded "*** Update File: $EXTERNAL/back-into-canonical")"
assert_block "ignored symlink resolving into tracked content blocks" \
    "$(guarded "*** Update File: scratch/tracked-alias")"
assert_block "ignored hard link to tracked content blocks" \
    "$(guarded "*** Update File: scratch/tracked-hardlink")"
assert_allow "ignored hard link to ignored content allows" \
    "$(guarded "*** Update File: scratch/ignored-hardlink")"
assert_block "looping symlink under an ignored dir blocks" \
    "$(guarded "*** Update File: scratch/loop-a")"
assert_block "unfoldable .. escaping an ignored dir blocks" \
    "$(guarded "*** Update File: scratch/absent/../../src/app.py")"
assert_block "unfoldable .. under an ignored prefix blocks" \
    "$(guarded "*** Update File: scratch/absent/../src/app.py")"

# AC8-EDGE: one payload, one decision.
assert_block "mixed ignored and tracked targets block the whole call" \
    "$(guarded "*** Update File: scratch/note.md
*** Update File: src/app.py")"
assert_allow "all-safe multi-target payload allows" \
    "$(guarded "*** Update File: scratch/note.md
*** Add File: $EXTERNAL/other.txt")"

# AC6-CON: this guard's approval is only this guard's. The state guard owns the
# manifest even though .fno/ is ignored and this guard therefore allows it.
assert_allow "location guard allows an ignored state path" \
    "$(guarded "*** Update File: .fno/target-state.md")"
STATE_PAYLOAD="$(jq -nc --arg p "$GUARDED/.fno/target-state.md" '{
    tool_name: "Edit",
    tool_input: {file_path: $p, old_string: "a", new_string: "b"}
}')"
if printf '%s' "$STATE_PAYLOAD" | bash "$REPO_ROOT/hooks/graph-write-protect.sh" \
    | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
    pass "state guard still denies the manifest the location guard allowed"
else
    fail "state guard did not deny target-state.md"
fi

# The Edit carrier reaches the same verdict as the apply-patch carrier.
EDIT_ALLOW="$(jq -nc --arg cwd "$GUARDED" --arg p "$GUARDED/scratch/note.md" '{
    cwd: $cwd, tool_name: "Edit",
    tool_input: {file_path: $p, old_string: "a", new_string: "b"}
}')"
EDIT_BLOCK="$(jq -nc --arg cwd "$GUARDED" --arg p "$GUARDED/src/app.py" '{
    cwd: $cwd, tool_name: "Edit",
    tool_input: {file_path: $p, old_string: "a", new_string: "b"}
}')"
assert_allow "Edit carrier allows an ignored target" "$EDIT_ALLOW"
assert_block "Edit carrier blocks a tracked target" "$EDIT_BLOCK"

NEVER_REPO="$TMP_BASE/never-policy-vault"
make_repo "$NEVER_REPO"
mkdir -p "$NEVER_REPO/.fno" "$NEVER_REPO/plans"
printf '[worktree]\npolicy = "never"\n' > "$NEVER_REPO/.fno/config.toml"
assert_allow \
    "canonical session can write a plan in a never-policy repo" \
    "$(payload "$LINK_CANONICAL" "*** Begin Patch
*** Add File: $NEVER_REPO/plans/example.md
*** End Patch")"

SPACE_REPO="$TMP_BASE/canonical with spaces"
make_repo "$SPACE_REPO"
assert_block "cwd containing spaces blocks correctly" "$(payload "$SPACE_REPO")"

NON_GIT="$TMP_BASE/not-a-repo"
mkdir -p "$NON_GIT"
assert_allow "non-git directory allows" "$(payload "$NON_GIT")"
assert_allow "missing cwd allows" '{}'
assert_allow "invalid cwd allows" "$(payload "$TMP_BASE/missing")"
assert_allow "malformed payload allows" 'not-json'

NO_PARSER_BIN="$TMP_BASE/no-parser-bin"
mkdir -p "$NO_PARSER_BIN"
ln -s "$(command -v bash)" "$NO_PARSER_BIN/bash"
ln -s "$(command -v cat)" "$NO_PARSER_BIN/cat"
NO_PARSER_OUTPUT="$(payload "$CANONICAL" | PATH="$NO_PARSER_BIN" "$NO_PARSER_BIN/bash" "$GUARD")"
if printf '%s' "$NO_PARSER_OUTPUT" | jq -e 'type == "object" and length == 0' >/dev/null; then
    pass "missing jq and python3 allows"
else
    fail "missing parsers did not allow: $NO_PARSER_OUTPUT"
fi

PYTHON_ONLY_BIN="$TMP_BASE/python-only-bin"
mkdir -p "$PYTHON_ONLY_BIN"
for command_name in bash cat dirname git head python3 sed; do
    command_path="$(command -v "$command_name")"
    if [[ "$command_name" == "python3" ]]; then
        command_path="$(python3 -c 'import sys; print(sys.executable)')"
    fi
    ln -s "$command_path" "$PYTHON_ONLY_BIN/$command_name"
done
PYTHON_ONLY_OUTPUT="$(payload "$CANONICAL" | PATH="$PYTHON_ONLY_BIN" "$PYTHON_ONLY_BIN/bash" "$GUARD")"
if printf '%s' "$PYTHON_ONLY_OUTPUT" | jq -e '
    .decision == "block"
    and .hookSpecificOutput.hookEventName == "PreToolUse"
    and .hookSpecificOutput.permissionDecision == "deny"
    and .hookSpecificOutput.permissionDecisionReason == .reason
' >/dev/null; then
    pass "python3 fallback blocks canonical main without jq"
else
    fail "python3 fallback did not block: $PYTHON_ONLY_OUTPUT"
fi

NO_HELPER_DIR="$TMP_BASE/no-helper"
mkdir -p "$NO_HELPER_DIR"
cp "$GUARD" "$NO_HELPER_DIR/guard.sh"
NO_HELPER_OUTPUT="$(payload "$CANONICAL" | bash "$NO_HELPER_DIR/guard.sh")"
if printf '%s' "$NO_HELPER_OUTPUT" | jq -e 'type == "object" and length == 0' >/dev/null; then
    pass "missing location helper allows"
else
    fail "missing helper did not allow: $NO_HELPER_OUTPUT"
fi

printf '\n=== Results: %d passed, %d failed ===\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
