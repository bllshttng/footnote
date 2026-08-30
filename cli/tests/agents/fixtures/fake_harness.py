#!/usr/bin/env python3
"""Tiny deterministic harness used by the support-probe tests."""

from __future__ import annotations

import json
import sys
import uuid


def main() -> int:
    if sys.argv[1:] == ["status", "--format", "json"]:
        print(json.dumps({"isAuthenticated": True, "session_id": str(uuid.uuid4())}))
        return 0
    for line in sys.stdin:
        print(f"echo:{line.rstrip()}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
