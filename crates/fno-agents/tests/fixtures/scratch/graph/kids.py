#!/usr/bin/env python3
"""List an epic's children from the graph through the front door.

Usage: kids.py <node-id>   (prints one `fno backlog get <id>` row summary)
"""
import json
import subprocess
import sys

node = sys.argv[1] if len(sys.argv) > 1 else "NODEID"
cmd = ["fno", "backlog", "get", node]
row = subprocess.run(cmd, capture_output=True, text=True)
epic = json.loads(row.stdout or "{}")
kids = epic.get("children") or []
print("node:", node, "| status:", epic.get("status"), "| children:", len(kids))
print()
for kid in kids:
    kid = kid["id"] if isinstance(kid, dict) else kid
    entry = epic.get("_byid", {}).get(kid) or {}
    print(" ", kid, " ", entry.get("status"), " ", (entry.get("title") or "")[:58])
