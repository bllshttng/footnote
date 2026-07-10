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
    jq -nc --arg cwd "$1" '{
        cwd: $cwd,
        tool_name: "apply_patch",
        tool_input: {patch: "*** Begin Patch"}
    }'
}

run_guard() {
    printf '%s' "$1" | bash "$GUARD"
}

assert_decision() {
    local name="$1" expected="$2" permission="$3" input="$4" output rc
    output="$(run_guard "$input")"
    rc=$?
    if [[ $rc -ne 0 ]]; then
        fail "$name exits zero (got $rc)"
        return
    fi
    if [[ "$(printf '%s' "$output" | jq -r '.decision')" != "$expected" ]]; then
        fail "$name decision=$expected: $output"
        return
    fi
    if [[ "$(printf '%s' "$output" | jq -r '.hookSpecificOutput.permissionDecision')" != "$permission" ]]; then
        fail "$name permissionDecision=$permission: $output"
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
assert_decision "canonical main blocks" block deny "$(payload "$CANONICAL")"

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
assert_decision "canonical master blocks" block deny "$(payload "$MASTER")"

DETACHED="$TMP_BASE/detached"
make_repo "$DETACHED"
git -C "$DETACHED" checkout -q --detach
assert_decision "canonical detached HEAD blocks" block deny "$(payload "$DETACHED")"

FEATURE="$TMP_BASE/feature"
make_repo "$FEATURE"
git -C "$FEATURE" checkout -q -b feature/allowed
assert_decision "canonical feature branch allows" approve allow "$(payload "$FEATURE")"

LINK_CANONICAL="$TMP_BASE/linked-canonical"
LINKED="$TMP_BASE/arbitrary linked path"
make_repo "$LINK_CANONICAL"
git -C "$LINK_CANONICAL" worktree add -q "$LINKED" -b feature/linked
assert_decision "arbitrary-base linked worktree allows" approve allow "$(payload "$LINKED")"

SPACE_REPO="$TMP_BASE/canonical with spaces"
make_repo "$SPACE_REPO"
assert_decision "cwd containing spaces blocks correctly" block deny "$(payload "$SPACE_REPO")"

NON_GIT="$TMP_BASE/not-a-repo"
mkdir -p "$NON_GIT"
assert_decision "non-git directory allows" approve allow "$(payload "$NON_GIT")"
assert_decision "missing cwd allows" approve allow '{}'
assert_decision "invalid cwd allows" approve allow "$(payload "$TMP_BASE/missing")"
assert_decision "malformed payload allows" approve allow 'not-json'

NO_PARSER_OUTPUT="$(printf '{}' | PATH=/bin /bin/bash "$GUARD")"
if [[ "$(printf '%s' "$NO_PARSER_OUTPUT" | jq -r '.decision')" == "approve" ]]; then
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
if [[ "$(printf '%s' "$PYTHON_ONLY_OUTPUT" | jq -r '.decision')" == "block" ]]; then
    pass "python3 fallback blocks canonical main without jq"
else
    fail "python3 fallback did not block: $PYTHON_ONLY_OUTPUT"
fi

NO_HELPER_DIR="$TMP_BASE/no-helper"
mkdir -p "$NO_HELPER_DIR"
cp "$GUARD" "$NO_HELPER_DIR/guard.sh"
NO_HELPER_OUTPUT="$(payload "$CANONICAL" | bash "$NO_HELPER_DIR/guard.sh")"
if [[ "$(printf '%s' "$NO_HELPER_OUTPUT" | jq -r '.decision')" == "approve" ]]; then
    pass "missing location helper allows"
else
    fail "missing helper did not allow: $NO_HELPER_OUTPUT"
fi

printf '\n=== Results: %d passed, %d failed ===\n' "$PASS" "$FAIL"
[[ $FAIL -eq 0 ]]
