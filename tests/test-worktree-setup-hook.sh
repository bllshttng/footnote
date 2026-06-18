#!/usr/bin/env bash
# test-worktree-setup-hook.sh -- guard the CC WorktreeCreate hook contract.
#
# The harness fails Agent dispatches with isolation: worktree when the hook
# exits 0 without emitting the absolute worktree path on stdout
# ("WorktreeCreate hook failed: no successful output"). These tests run both
# copies of the hook (the plugin-level copy and the /speculate skill's
# portable duplicate) in a sandboxed temp git repo and assert:
#
#   1. stdout is exactly one line.
#   2. That line is an absolute path that exists on disk.
#   3. stderr carries the setup log ("Worktree ready:") unchanged.
#   4. Works whether CC passes a JSON {"path": ...} on stdin or nothing at all
#      (the hook falls back to $(pwd)).
#   5. The hook cd's into the resolved worktree before running relative-path
#      setup checks, even if the caller invoked it from a different cwd.
#      (Regression guard for the gap Gemini flagged on PR #148.)

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

HOOKS=(
    "$REPO_ROOT/hooks/worktree-setup.sh"
    "$REPO_ROOT/skills/speculate/scripts/worktree-setup.sh"
)

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 :: $2"; FAIL=$((FAIL + 1)); }

setup_sandbox() {
    local tmp
    tmp=$(mktemp -d -t wt-hook-test.XXXXXX)
    (
        cd "$tmp"
        git init -q
        git -c user.email=t@t -c user.name=t commit --allow-empty -m init -q
        mkdir -p .fno
        git worktree add -q test-wt
    )
    echo "$tmp"
}

# Run the hook from a given cwd, with given stdin, against a known worktree.
# Asserts the stdout contract AND that the hook cd'd into the worktree (so
# relative-path setup checks would target the right directory).
run_hook() {
    local invocation_cwd="$1"
    local hook="$2"
    local stdin_input="$3"
    local stdout_file stderr_file rc
    stdout_file=$(mktemp)
    stderr_file=$(mktemp)

    if [[ "$stdin_input" == "__empty__" ]]; then
        (cd "$invocation_cwd" && bash "$hook" < /dev/null) >"$stdout_file" 2>"$stderr_file"
    else
        (cd "$invocation_cwd" && bash "$hook" <<<"$stdin_input") >"$stdout_file" 2>"$stderr_file"
    fi
    rc=$?
    printf '%s\n%s\n%s\n' "$rc" "$stdout_file" "$stderr_file"
}

assert_contract() {
    local label="$1"
    local hook="$2"
    local stdin_input="$3"
    local invocation_cwd="$4"
    local expected_worktree="$5"

    local output rc stdout_file stderr_file stdout_line line_count
    output=$(run_hook "$invocation_cwd" "$hook" "$stdin_input")
    rc=$(echo "$output" | sed -n '1p')
    stdout_file=$(echo "$output" | sed -n '2p')
    stderr_file=$(echo "$output" | sed -n '3p')
    stdout_line=$(cat "$stdout_file")

    local cleanup_files="$stdout_file $stderr_file"
    # shellcheck disable=SC2064
    trap "rm -f $cleanup_files" RETURN

    if [[ "$rc" -ne 0 ]]; then
        fail "$label" "exit $rc (expected 0). stderr: $(tail -3 "$stderr_file")"
        return
    fi
    line_count=$(grep -c '^' "$stdout_file" 2>/dev/null || echo 0)
    if [[ "$line_count" -ne 1 ]]; then
        fail "$label" "stdout should be one line, got $line_count (content: '$stdout_line')"
        return
    fi
    if [[ "${stdout_line:0:1}" != "/" ]]; then
        fail "$label" "stdout not absolute: '$stdout_line'"
        return
    fi
    if [[ "$stdout_line" != "$expected_worktree" ]]; then
        fail "$label" "stdout path '$stdout_line' != expected '$expected_worktree'"
        return
    fi
    if ! grep -q "Worktree ready:" "$stderr_file"; then
        fail "$label" "stderr missing 'Worktree ready:' line. stderr was: $(cat "$stderr_file")"
        return
    fi
    # Regression guard: the hook must cd into the resolved worktree before
    # running relative-path setup checks. We assert via the resolved-log line.
    if ! grep -q "WorktreeCreate resolved: path=$expected_worktree pwd=$expected_worktree" "$stderr_file"; then
        fail "$label" "hook did not cd into worktree. stderr: $(grep resolved "$stderr_file" || echo '(no resolved line)')"
        return
    fi
    pass "$label"
}

for hook in "${HOOKS[@]}"; do
    name=$(basename "$(dirname "$(dirname "$hook")")")/$(basename "$(dirname "$hook")")/$(basename "$hook")

    # Case 1: stdin JSON carries the real path; caller is already in the worktree.
    sandbox=$(setup_sandbox)
    worktree="$sandbox/test-wt"
    stdin_json=$(printf '{"session_id":"s1","name":"test-wt","path":"%s","hook_event_name":"WorktreeCreate"}' "$worktree")
    assert_contract "$name :: stdin with JSON path, invoked from worktree" "$hook" "$stdin_json" "$worktree" "$worktree"
    rm -rf "$sandbox"

    # Case 2: empty stdin, caller already in worktree; hook falls back to $(pwd).
    sandbox=$(setup_sandbox)
    worktree="$sandbox/test-wt"
    assert_contract "$name :: empty stdin (fallback to pwd)" "$hook" "__empty__" "$worktree" "$worktree"
    rm -rf "$sandbox"

    # Case 3 (regression for PR #148 Gemini comment): caller invoked the hook
    # from outside the worktree, but JSON payload names the correct path. The
    # hook must resolve and cd into the JSON path - otherwise subsequent
    # relative-path setup checks (pnpm-lock.yaml, node_modules, etc.) target
    # the wrong directory.
    sandbox=$(setup_sandbox)
    worktree="$sandbox/test-wt"
    stdin_json=$(printf '{"session_id":"s1","name":"test-wt","path":"%s","hook_event_name":"WorktreeCreate"}' "$worktree")
    assert_contract "$name :: JSON path, invoked from sandbox root (cd-gap guard)" "$hook" "$stdin_json" "$sandbox" "$worktree"
    rm -rf "$sandbox"
done

echo
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
