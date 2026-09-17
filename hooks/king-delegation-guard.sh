#!/usr/bin/env bash
# PreToolUse: a crowned court session does not implement. Policy lives in
# crates/fno-agents/src/hook/king_guard.rs.
#
# Never exec a candidate: a worktree build older than the hook verb answers
# "unknown verb: hook", and a PreToolUse that exits nonzero refuses every tool
# in the session (2026-09-17 outage). Run each candidate instead
# and relay stdout+stderr only on exit 0, falling through otherwise. The
# deployed binary leads the order: policy must come from the installed
# release, not a half-built branch.
stdin=$(cat)
errfile=$(mktemp -t kgd-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" \
    "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" hook king-guard 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
rm -f "$errfile"
echo "king-delegation-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '%s\n' '{}'
exit 0
