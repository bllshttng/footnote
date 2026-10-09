#!/usr/bin/env bash
# fno hook: PostToolUse - diff budget at the commit
#
# Thin shim over the native entry `fno-agents hook diff-budget` (the checks
# live in crates/fno-agents/src/hook/diff_budget.rs). Fires after every Bash
# call, and the entry itself is silent unless that call was a successful
# `git commit` in a target session with a resolvable diff budget. Never
# fails the tool call: every path here exits 0.
#
# The entry is probed per candidate binary, in the test-run-guard order:
# PATH first, then FNO_AGENTS_FRONT, then FNO_AGENTS_BIN, then the
# checkout's own target dirs. An exit of 0 or 1 is an answer; 2 or higher
# means a build without the entry, so the next candidate is tried and
# silence wins if none answers.

set -uo pipefail

PATH="${PATH:+$PATH:}/usr/bin:/bin:/usr/sbin:/sbin"
export PATH

PAYLOAD="$(cat 2>/dev/null || true)"
[ -n "$PAYLOAD" ] || exit 0

OUT="$(mktemp "${TMPDIR:-/tmp}/diff-budget-out-XXXXXX")" || exit 0
trap 'rm -f "$OUT"' EXIT

for candidate in \
    "$(command -v fno-agents 2>/dev/null || true)" \
    "${FNO_AGENTS_FRONT:-}" \
    "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [ -n "$candidate" ] || continue
    [ -x "$candidate" ] || continue
    printf '%s' "$PAYLOAD" | "$candidate" hook diff-budget >"$OUT" 2>/dev/null
    rc=$?
    if [ "$rc" -le 1 ]; then
        # 0 or 1 is an answer; forward whatever the entry said (the
        # additionalContext JSON when it fired, silence otherwise).
        cat "$OUT"
        exit 0
    fi
done
exit 0
