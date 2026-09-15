#!/usr/bin/env bash
# PreToolUse: a crowned court session does not implement. Policy lives in
# crates/fno-agents/src/hook/king_guard.rs.
for bin in "${FNO_AGENTS_BIN:-}" \
    "$PWD/crates/fno-agents/target/release/fno-agents" \
    "$PWD/crates/fno-agents/target/debug/fno-agents" \
    "$(command -v fno-agents 2>/dev/null)"; do
    [[ -n "$bin" && -x "$bin" ]] && exec "$bin" hook king-guard
done
echo "king-delegation-guard: fno-agents not found; allowing" >&2
printf '%s\n' '{}'
exit 0
