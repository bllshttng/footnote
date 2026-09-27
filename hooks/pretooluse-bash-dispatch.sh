#!/usr/bin/env bash
# PreToolUse: run the ordered Bash guard chain with one hook command.
# Guard policy stays in the Rust hook module and existing Python guards.
stdin=$(cat)
root="$(cd "$(dirname "$0")/.." && pwd)"
errfile=$(mktemp -t pbd-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | FNO_REPO_ROOT="$root" "$bin" hook pretooluse-bash 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "pretooluse-bash: no fno-agents could answer the hook entry; allowing" >&2
printf '%s\n' '{}'
