#!/usr/bin/env bash
# fno hook: PostToolUse - refusal-streak recorder
#
# Feeds every finished Bash call to the Rust refusal-streak ledger
# (`fno-agents hook posttooluse-bash`): nonzero exits raise a shape's
# streak, zero clears it, and the PreToolUse guard parks the third
# identical attempt. It never fails the tool call: every path exits 0.

set -uo pipefail

PAYLOAD="$(cat 2>/dev/null || true)"
[ -n "$PAYLOAD" ] || exit 0

for candidate in \
    "$(command -v fno-agents 2>/dev/null || true)" \
    "${FNO_AGENTS_BIN:-}" \
    "${FNO_AGENTS_FRONT:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [ -n "$candidate" ] || continue
    [ -x "$candidate" ] || continue
    printf '%s' "$PAYLOAD" | "$candidate" hook posttooluse-bash >/dev/null 2>&1
    rc=$?
    [ "$rc" -le 1 ] && exit 0
done
exit 0
