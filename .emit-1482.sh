#!/usr/bin/env bash
set -u
cd /Users/bb16/code/footnote/footnote/.claude/worktrees/x-0992
python3 - ~/.fno/spaces/-Users-bb16-code-footnote-footnote/events.jsonl <<'EOF'
import json, sys
rows = []
for line in open(sys.argv[1], encoding="utf-8"):
    if "review_attestation" not in line:
        continue
    try:
        row = json.loads(line)
    except ValueError:
        continue
    d = row.get("data") or {}
    if (d.get("head_sha") or "").startswith("4d175136") and d.get("branch") == "feature/x-0992":
        rows.append((row.get("id") or row.get("ts"), d.get("verdict"), d.get("attestation_origin")))
print("last:", rows[-1] if rows else "none")
EOF
