#!/usr/bin/env bash
# fno hook: agy PreToolUse - king delegation guard (agy wire)
# The king-delegation-guard contract on agy's PreToolUse surface: same
# never-block rule, agy payload and decision shape (`--wire agy` performs
# the translation, crates/fno-agents/src/hook/king_guard.rs). This shim is
# the binary resolver and fail-open wrapper, exactly like the claude shim.
stdin=$(cat)
errfile=$(mktemp -t akgd-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" hook king-guard --wire agy 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "agy-king-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '{"decision":"allow"}\n'
