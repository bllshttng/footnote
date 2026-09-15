#!/usr/bin/env bash
# Tombstone (2026-09-15): the SessionStart law read is retired.
# No session loads law in bulk - a confirmed law graduates into a rule file
# or a gate instead. This file stays as a no-op stub for one release because
# running sessions answer hook config from an init-time snapshot, and a
# PreToolUse hook that cannot launch fails the whole Bash call. Delete this
# stub in a later release.
exit 0
