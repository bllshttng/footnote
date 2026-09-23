#!/usr/bin/env python3
"""Tombstone stub: this guard retired into the Rust pipe guard
(`fno-agents hook pipe-guard`, launched by hooks/pipe-guard.sh).

A running session answers hook config from an init-time snapshot, so a
PreToolUse registration whose script vanished fails every Bash call in
that session. The path stays as a no-op stub that exits 0 until every
session holding the old registration has ended; delete the stub in a
later release. `fno doctor lint hook-tombstones` enforces this.
"""

import json
import sys

try:
    json.load(sys.stdin)
except Exception:  # noqa: BLE001 -- a tombstone drains stdin and allows
    pass
print("{}")
sys.exit(0)
