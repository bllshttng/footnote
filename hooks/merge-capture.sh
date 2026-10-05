#!/usr/bin/env bash
# fno hook: PostToolUse - Bash merge capture
#
# Backstop for a gh merge that skipped fno-gh-proxy (absolute-path gh,
# aliases). Pipes the PostToolUse payload into the graph-get stdin door as
# {"merge_provenance": {"hook": <payload>}}; the Rust arm decides whether
# the command was a merge and records one merge_requested span. It never
# fails the tool call: every path here exits 0.

set -uo pipefail

PAYLOAD="$(cat 2>/dev/null || true)"
[ -n "$PAYLOAD" ] || exit 0

# The payload is embedded as an OBJECT (the door reads .hook.tool_input),
# so it is validated as JSON first and passed through verbatim.
if command -v jq >/dev/null 2>&1; then
    printf '%s' "$PAYLOAD" | jq -e . >/dev/null 2>&1 || exit 0
elif command -v python3 >/dev/null 2>&1; then
    printf '%s' "$PAYLOAD" | python3 -c 'import json,sys; json.load(sys.stdin)' >/dev/null 2>&1 || exit 0
else
    exit 0
fi

for candidate in \
    "$(command -v fno-agents 2>/dev/null || true)" \
    "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [ -n "$candidate" ] || continue
    [ -x "$candidate" ] || continue
    printf '{"merge_provenance":{"hook":%s}}' "$PAYLOAD" \
        | "$candidate" graph-get >/dev/null 2>&1
    rc=$?
    [ "$rc" -le 1 ] && exit 0
done
exit 0
