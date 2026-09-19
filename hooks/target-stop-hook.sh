#!/usr/bin/env bash
# Stop: relay to the native handler (policy: crates/fno-agents/src/hook/stop.rs).
# Run each candidate, deployed binary first, never exec one. Relay only a real
# answer: 0 (allow / JSON) or 2 (continue block on stderr). Any other exit is a
# crash or an old build's "unknown verb"; codex fails a Stop hook on exit 1, so
# relaying it would re-invoke the session on every stop. Try the next one.
stdin=$(cat)
errfile=$(mktemp -t tsh-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" "${FNO_AGENTS_BIN:-}" \
    "$PWD"/crates/fno-agents/target/{release,debug}/fno-agents; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    out="$(printf '%s' "$stdin" | "$bin" hook stop 2>"$errfile")"
    rc=$?
    [[ $rc -eq 0 || $rc -eq 2 ]] || continue
    cat "$errfile" >&2
    [[ $rc -eq 0 || -n "$out" ]] && printf '%s\n' "$out"
    exit "$rc"
done
echo "target stop-hook: no fno-agents could answer the hook verb; stop gate off for this stop" >&2
