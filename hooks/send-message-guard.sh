#!/usr/bin/env bash
# fno hook: PreToolUse - SendMessage guard
# Peer messages between fno sessions ride `fno agents mail send`, not the
# native SendMessage. Never exec a candidate blindly: an old build answers
# "unknown verb: hook", and a nonzero PreToolUse refuses every tool in the
# session. Run each, deployed binary first; relay only exit 0.
stdin=$(cat)
errfile=$(mktemp -t smg-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" "hook" "send-message-guard" 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "send-message-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '%s\n' '{}'
