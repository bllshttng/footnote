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
#   4. With no path on stdin, the hook falls back to $(pwd) ONLY when cwd is a
#      linked worktree (Case 2); when cwd is the canonical checkout it REFUSES
#      - exit 0 with empty stdout - so it never designates the canonical root
#      as a worktree (Case 4, x-ab78 WAVE 1 data-safety: edits would land on
#      main while every signal says isolated).
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

# Mirror of assert_contract for the refuse path (x-ab78 WAVE 1): the hook must
# exit 0 with NOTHING on stdout - the supported abort per the contract ("no
# successful output") - rather than emit the canonical checkout as the worktree.
# A non-zero exit would fall back to CC's default worktree flow, so rc must be 0.
assert_refusal() {
    local label="$1"
    local hook="$2"
    local stdin_input="$3"
    local invocation_cwd="$4"

    local output rc stdout_file stderr_file stdout_line
    output=$(run_hook "$invocation_cwd" "$hook" "$stdin_input")
    rc=$(echo "$output" | sed -n '1p')
    stdout_file=$(echo "$output" | sed -n '2p')
    stderr_file=$(echo "$output" | sed -n '3p')
    stdout_line=$(cat "$stdout_file")
    # shellcheck disable=SC2064
    trap "rm -f $stdout_file $stderr_file" RETURN

    if [[ "$rc" -ne 0 ]]; then
        fail "$label" "exit $rc (expected 0; non-zero falls back to CC default flow)"
        return
    fi
    if [[ -n "$stdout_line" ]]; then
        fail "$label" "stdout must be empty on refusal, got '$stdout_line'"
        return
    fi
    if ! grep -q "refusing to designate it as a worktree" "$stderr_file"; then
        fail "$label" "stderr missing refuse reason. stderr: $(cat "$stderr_file")"
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

    # Case 4 (x-ab78 WAVE 1, data-safety): no path on stdin AND cwd is the
    # canonical sandbox root (CC did not chdir into a worktree). The hook MUST
    # refuse (exit 0, empty stdout) rather than fall back to $(pwd) and
    # designate the canonical root as the worktree.
    sandbox=$(setup_sandbox)
    assert_refusal "$name :: empty stdin from canonical root refuses" "$hook" "__empty__" "$sandbox"
    rm -rf "$sandbox"

    # Case 5: a `name` with NO `path` from the canonical root is a CREATE
    # request CC has not materialized yet - the shape `claude --worktree <name>`
    # and the EnterWorktree tool both send. Case 4's refusal used to swallow it,
    # which aborted every creation from the main checkout. The hook must instead
    # exit NON-ZERO so CC falls back to its own worktree flow. Case 4 stays the
    # guard for the genuinely pathless payload.
    sandbox=$(setup_sandbox)
    stdin_json=$(printf '{"session_id":"s1","name":"cc-created","hook_event_name":"WorktreeCreate"}')
    output=$(run_hook "$sandbox" "$hook" "$stdin_json")
    rc=$(echo "$output" | sed -n '1p')
    stdout_file=$(echo "$output" | sed -n '2p')
    stderr_file=$(echo "$output" | sed -n '3p')
    if [[ "$rc" -eq 0 ]]; then
        fail "$name :: named create from canonical defers to CC" \
            "exit 0 aborts the creation; expected non-zero so CC's default flow runs"
    elif [[ -n "$(cat "$stdout_file")" ]]; then
        fail "$name :: named create from canonical defers to CC" \
            "stdout must stay empty, got '$(cat "$stdout_file")'"
    elif ! grep -q "deferring to Claude Code" "$stderr_file"; then
        fail "$name :: named create from canonical defers to CC" \
            "stderr missing the deferral reason. stderr: $(cat "$stderr_file")"
    else
        pass "$name :: named create from canonical defers to CC"
    fi
    rm -f "$stdout_file" "$stderr_file"
    rm -rf "$sandbox"

    # Case 6: a `name` with NO `path` on a `never`-policy repo must STILL
    # abort (exit 0, empty stdout), not defer. Guards the `!= "never"` carveout
    # in the deferral block. Uses an fno shim that resolves policy to `never`.
    sandbox=$(setup_sandbox)
    stdin_json=$(printf '{"session_id":"s1","name":"cc-created","hook_event_name":"WorktreeCreate"}')
    never_bindir=$(mktemp -d)
    cat > "$never_bindir/fno" <<'SH'
#!/usr/bin/env bash
# Only `worktree policy` is queried by the hook; answer `never`.
case "$*" in
    *worktree*policy*) echo "never"; exit 0 ;;
    *) echo "{}"; exit 0 ;;
esac
SH
    chmod +x "$never_bindir/fno"
    # run_hook spawns a subshell that inherits exported PATH, so the fno shim
    # is visible inside the hook's `command -v fno` and `fno worktree policy`.
    _saved_path="$PATH"
    export PATH="$never_bindir:$PATH"
    output=$(run_hook "$sandbox" "$hook" "$stdin_json")
    export PATH="$_saved_path"
    rc=$(echo "$output" | sed -n '1p')
    stdout_file=$(echo "$output" | sed -n '2p')
    stderr_file=$(echo "$output" | sed -n '3p')
    if [[ "$rc" -ne 0 ]]; then
        fail "$name :: never-policy named create still aborts" \
            "exit $rc (non-zero defers to CC; never-policy must abort at exit 0). stderr: $(cat "$stderr_file")"
    elif [[ -n "$(cat "$stdout_file")" ]]; then
        fail "$name :: never-policy named create still aborts" \
            "stdout must be empty on abort, got '$(cat "$stdout_file")'"
    elif ! grep -q "policy" "$stderr_file"; then
        fail "$name :: never-policy named create names policy=never" \
            "stderr should name policy=never, not the defeat-isolation case. stderr: $(cat "$stderr_file")"
    else
        pass "$name :: never-policy named create still aborts"
    fi
    rm -f "$stdout_file" "$stderr_file"
    rm -rf "$sandbox" "$never_bindir"

    # Case 7: a `name` with NO `path` where CC has PRE-CREATED
    # <repo>/.claude/worktrees/<name> (the live shape `claude --worktree <name>`
    # sends). The hook must ADOPT that path and run setup (exit 0, path on
    # stdout), not defer - deferring leaves the pre-created worktree bare.
    sandbox=$(setup_sandbox)
    mkdir -p "$sandbox/.claude/worktrees/cc-created"
    stdin_json=$(printf '{"session_id":"s1","name":"cc-created","hook_event_name":"WorktreeCreate"}')
    output=$(run_hook "$sandbox" "$hook" "$stdin_json")
    rc=$(echo "$output" | sed -n '1p')
    stdout_file=$(echo "$output" | sed -n '2p')
    stderr_file=$(echo "$output" | sed -n '3p')
    if [[ "$rc" -ne 0 ]]; then
        fail "$name :: named create adopts pre-created path" \
            "exit $rc (should adopt the pre-created path and run setup, not defer). stderr: $(cat "$stderr_file")"
    elif ! grep -q "cc-created" "$stdout_file"; then
        fail "$name :: named create adopts pre-created path" \
            "stdout should be the adopted cc-created path, got '$(cat "$stdout_file")'"
    else
        pass "$name :: named create adopts pre-created path"
    fi
    rm -f "$stdout_file" "$stderr_file"
    rm -rf "$sandbox"
done

echo
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
echo "================================"
exit "$FAIL"
