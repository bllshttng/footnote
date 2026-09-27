#!/usr/bin/env bash
# PreToolUse: refuse a raw pytest or `cargo test` run in a footnote checkout
# (policy: crates/fno-agents/src/hook/test_run_guard.rs). Never exec a
# candidate: an old build answers "unknown entry" and a nonzero PreToolUse
# refuses every tool in the session. Run each, deployed binary first; relay
# only exit 0.
stdin=$(cat)
errfile=$(mktemp -t trg-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    if out="$(printf '%s' "$stdin" | "$bin" hook test-run-guard 2>"$errfile")"; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    fi
done
echo "test-run-guard: no fno-agents could answer the hook verb; allowing" >&2
printf '%s\n' '{}'
