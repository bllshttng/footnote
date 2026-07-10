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
if [[ "$MAIN_REASON" == *'fno target start <node>'* \
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
    ln -s "$(command -v "$command_name")" "$PYTHON_ONLY_BIN/$command_name"
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
