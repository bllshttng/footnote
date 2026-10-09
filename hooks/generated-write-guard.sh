#!/usr/bin/env bash
# fno hook: PreToolUse - generated write guard (TOMBSTONE)
# Retired: the policy moved into the single write gate
# (hooks/write-gate.sh, crates/fno-agents/src/hook/write_gate.rs). This
# no-op stub stays one release so sessions started before the merge - which
# answer hook config from an init-time snapshot - do not brick every Bash
# call on a missing script. Delete this stub in a later release.
# Exit 0 always (hook result is communicated via stdout JSON).
printf '%s\n' '{}'
exit 0
