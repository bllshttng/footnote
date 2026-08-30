#!/usr/bin/env python3
"""Retired gate kept one release as a no-op tombstone.

A session answers hook config from an init-time snapshot, so deleting a
hook script in the same commit that unregisters it leaves every pre-merge
session holding a dead registration, and a PreToolUse hook that cannot
launch fails the whole Bash call. This stub keeps those sessions runnable
now that the registration is gone from HEAD. Delete it once no pre-merge
session is live; `fno doctor lint hook-tombstones` enforces the same
two-release retirement for the rest of the hooks tree.
"""
import sys


def main() -> int:
    return 0


if __name__ == "__main__":
    sys.exit(main())
