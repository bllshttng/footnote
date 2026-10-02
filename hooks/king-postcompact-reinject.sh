#!/usr/bin/env bash
# fno hook: compact - tombstone forwarding the pre-rename spelling to lead-postcompact-reinject.sh
# A session started before the rename answers hook config from an
# init-time snapshot and still launches this path. Delete this stub one
# release after the old config entries are gone everywhere.
exec bash "$(dirname "$0")/lead-postcompact-reinject.sh" "$@"
