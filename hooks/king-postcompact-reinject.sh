#!/usr/bin/env bash
# Tombstone for the pre-rename spelling: a session started before the
# rename answers hook config from an init-time snapshot and still launches
# this path. Forward to the renamed script; delete this stub one release
# after the old config entries are gone everywhere.
exec bash "$(dirname "$0")/lead-postcompact-reinject.sh" "$@"
