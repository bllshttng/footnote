#!/usr/bin/env bash
# Shell lock wrapper for atomic task mutations.
#
# Usage:
#   source "$(dirname "$0")/lib/flock.sh"
#   with_task_lock python3 roadmap-tasks.py update 5 --status done
#
# Uses mkdir-based locking (POSIX atomic, works on macOS without flock).
# Python roadmap-tasks.py uses fcntl.flock internally for its own writes.
# This wrapper serializes shell-initiated mutations.

TASK_LOCK_DIR="/tmp/abilities-ledger.lock.d"
TASK_LOCK_TIMEOUT=30  # seconds

with_task_lock() {
    local waited=0
    while ! mkdir "$TASK_LOCK_DIR" 2>/dev/null; do
        if [ "$waited" -ge "$TASK_LOCK_TIMEOUT" ]; then
            # Stale lock — force remove and retry once
            echo "Warning: removing stale task lock after ${TASK_LOCK_TIMEOUT}s" >&2
            rm -rf "$TASK_LOCK_DIR"
            if ! mkdir "$TASK_LOCK_DIR" 2>/dev/null; then
                echo "Error: failed to acquire task lock" >&2
                return 1
            fi
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done

    # Run in subshell to scope the trap (avoids clobbering caller's EXIT trap)
    (
        trap 'rm -rf "$TASK_LOCK_DIR"' EXIT INT TERM
        "$@"
    )
    local rc=$?
    # Subshell's EXIT trap handles cleanup, but rm again for safety
    rm -rf "$TASK_LOCK_DIR" 2>/dev/null
    return $rc
}
