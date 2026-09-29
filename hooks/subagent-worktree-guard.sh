#!/usr/bin/env bash
# fno hook: PreToolUse - subagent worktree guard
# PreToolUse: refuse EnterWorktree from a subagent (policy:
# crates/fno-agents/src/hook/subagent_worktree_guard.rs). Never exec a
# candidate: an old build answers "unknown entry" and a nonzero PreToolUse
# refuses every tool in the session. Run each, deployed binary first; relay
# only exit 0.
stdin=$(cat)
errfile=$(mktemp -t swg-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" hook subagent-worktree-guard 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "subagent-worktree-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '%s\n' '{}'
