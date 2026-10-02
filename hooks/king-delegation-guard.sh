#!/usr/bin/env bash
# fno hook: PreToolUse - tombstone forwarding the pre-rename spelling to lead-delegation-guard.sh
# A session started before the rename answers hook config from an
# init-time snapshot and still launches this path, and a missing
# PreToolUse hook fails every Bash call it guards. Delete this stub one
# release after the old config entries are gone everywhere.
exec bash "$(dirname "$0")/lead-delegation-guard.sh" "$@"
