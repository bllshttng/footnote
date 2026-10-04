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

# The door payload embeds the hook payload as a JSON string value.
if command -v jq >/dev/null 2>&1; then
    WRAPPED="$(printf '%s' "$PAYLOAD" | jq -Rs .)"
elif command -v python3 >/dev/null 2>&1; then
    WRAPPED="$(PAYLOAD_ENV="$PAYLOAD" python3 -c 'import json,os,sys; sys.stdout.write(json.dumps(os.environ["PAYLOAD_ENV"]))' 2>/dev/null || true)"
else
    exit 0
fi
[ -n "$WRAPPED" ] || exit 0

for candidate in \
    "$(command -v fno-agents 2>/dev/null || true)" \
    "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents"; do
    [ -n "$candidate" ] || continue
    [ -x "$candidate" ] || continue
    printf '{"merge_provenance":{"hook":%s}}' "$WRAPPED" \
        | "$candidate" graph-get >/dev/null 2>&1
    rc=$?
    [ "$rc" -le 1 ] && exit 0
done
exit 0
