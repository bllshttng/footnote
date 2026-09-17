#!/usr/bin/env bash
# Stop: relay to the native handler. Policy lives in crates/fno-agents/src/hook/stop.rs;
# the stop/allow decision lives in crates/fno-agents/src/loopcheck.rs.
#
# Never exec a candidate: a worktree build older than the hook verb answers
# "unknown verb" and the failing hook re-invokes the session on every stop.
# Run each candidate instead and relay only a real answer, falling through
# otherwise. A nonzero exit that is NOT the unknown-verb signature is a real
# decision (the continue block exits 2) and must reach the harness verbatim.
# The deployed binary leads the order: policy must come from the installed
# release, not a half-built branch.
stdin=$(cat)
errfile=$(mktemp -t tsh-stderr.XXXXXX) || errfile=/dev/null
trap 'rm -f "$errfile"' EXIT
for bin in "$(command -v fno-agents 2>/dev/null)" \
    "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [[ -n "$bin" && -x "$bin" ]] || continue
    out="$(printf '%s' "$stdin" | "$bin" hook stop 2>"$errfile")"
    rc=$?
    if [[ $rc -eq 0 ]]; then
        cat "$errfile" >&2
        printf '%s\n' "$out"
        exit 0
    elif grep -q "unknown verb" "$errfile"; then
        continue
    else
        cat "$errfile" >&2
        [[ -n "$out" ]] && printf '%s\n' "$out"
        exit "$rc"
    fi
done
rm -f "$errfile"
echo "target stop-hook: no fno-agents could answer the hook verb; stop gate off for this stop" >&2
exit 0
