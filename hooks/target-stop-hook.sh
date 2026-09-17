#!/usr/bin/env bash
# Stop: exec the native handler. Policy lives in crates/fno-agents/src/hook/stop.rs;
# the stop/allow decision lives in crates/fno-agents/src/loopcheck.rs.
for bin in "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents" \
    "$(command -v fno-agents 2>/dev/null)"; do
    [[ -n "$bin" && -x "$bin" ]] && exec "$bin" hook stop
done
echo "target stop-hook: fno-agents not found; stop gate off for this stop" >&2
exit 0
