#!/usr/bin/env bash
# fno hook: PreToolUse - shared Bash guard dispatcher
# PreToolUse: run the ordered Bash guard chain with one hook command.
# Guard policy stays in the Rust hook module and existing Python guards.
stdin=$(cat)
root="$(cd "$(dirname "$0")/.." && pwd)"
errfile=$(mktemp -t pbd-stderr.XXXXXX) || errfile=/dev/null
if [[ "$errfile" != /dev/null ]]; then
    trap 'rm -f "$errfile"' EXIT
fi
for bin in "${FNO_AGENTS_BIN:-}" "${FNO_AGENTS_FRONT:-}" \
    "$(command -v fno-agents 2>/dev/null)" \
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
