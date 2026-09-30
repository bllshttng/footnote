#!/usr/bin/env bash
# fno hook: PreToolUse - the effect guard for non-Bash tools (mcp__.* matcher)
# Classifies the tool call's external effect and files an approval request.
# Transport only: the table lives in Rust (effect_gate.rs).
stdin=$(cat)
root="$(cd "$(dirname "$0")/.." && pwd)"
for bin in "${FNO_AGENTS_BIN:-}" "${FNO_AGENTS_FRONT:-}" \
    "$(command -v fno-agents 2>/dev/null)" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | FNO_REPO_ROOT="$root" "$bin" hook effect-guard 2>/dev/null)"; then
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "effect-guard: no fno-agents could answer the hook entry; allowing" >&2
printf '%s\n' '{}'
