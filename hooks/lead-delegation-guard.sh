#!/usr/bin/env bash
# fno hook: PreToolUse - lead delegation guard
# PreToolUse: a teamed org session does not implement (policy:
# crates/fno-agents/src/hook/lead_guard.rs). Never exec a candidate: an old
# build answers "unknown verb: hook", and a nonzero PreToolUse refuses every
# tool in the session. Run each, deployed binary first; relay only exit 0.
stdin=$(cat)
errfile=$(mktemp -t kgd-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    # Both verb spellings for one release: the installed binary and the
    # repo hook update at different times.
    for verb in lead-guard king-guard; do
        if out="$(printf '%s' "$stdin" | "$bin" "hook" "$verb" 2>"$errfile")"; then
            cat "$errfile" >&2
            printf '%s\n' "$out"
            exit 0
        fi
    done
done
echo "lead-delegation-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '%s\n' '{}'
