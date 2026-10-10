#!/usr/bin/env bash
# fno hook: PreToolUse - one write gate for the three write guards
# write-gate.sh - exec wrapper of crates/fno-agents/src/hook/write_gate.rs, the
# single PreToolUse gate that replaced graph-write-protect.sh,
# claude-config-write-guard.sh and generated-write-guard.sh (56,455
# hook-seconds a day in the 2026-10-08 slowness audit, paid as bash + jq spawns
# per write-capable tool call). Policy lives in the native entry; this file
# only resolves a binary and relays.
#
# Never exec a candidate blindly: an old build answers "unknown verb: hook", and
# a nonzero PreToolUse refuses every tool in the session. Run each, deployed
# binary first; relay only exit 0. If nothing can answer, the posture splits by
# surface, exactly as the shell guards split: the graph/state surfaces failed
# CLOSED on an unparsable payload naming a protected file, everything else
# failed open (a wedged guard starves every session on the machine).
stdin=$(cat)
errfile=$(mktemp -t write-gate.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" "hook" "write-gate" 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
case "$stdin" in
    *graph.json*|*target-state.md*|*graph.db*|*.fno/artifacts/*)
        printf '%s\n' '{"decision":"block","reason":"graph-write-protect: neither jq nor python3 available to parse a payload referencing a protected state file; blocking fail-closed. Install jq or python3.","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"graph-write-protect: neither jq nor python3 available to parse a payload referencing a protected state file; blocking fail-closed. Install jq or python3."}}'
        exit 0
        ;;
esac
printf '%s\n' '{}'
