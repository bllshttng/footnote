#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
USING_FNO="$ROOT/skills/using-fno/SKILL.md"
MAIL_SKILL="$ROOT/skills/mail/SKILL.md"
HOOK="$ROOT/hooks/session-start-using-fno.sh"

for file in "$USING_FNO" "$MAIL_SKILL"; do
  grep -Fq "## Relay compression contract" "$file"
  grep -Fq 'fno mux pane send' "$file"
  grep -Fq "80 words or fewer" "$file"
  grep -Fq "Keep technical terms" "$file"
done

CLAUDE_PLUGIN_ROOT="$ROOT" bash "$HOOK" </dev/null | python3 -c '
import json
import sys

payload = json.load(sys.stdin)
context = payload["hookSpecificOutput"]["additionalContext"]
required = (
    "## Relay compression contract",
    "fno mail send",
    "fno mail reply",
    "fno mux pane send",
    "80 words or fewer",
    "Keep technical terms",
)
missing = [item for item in required if item not in context]
if missing:
    raise SystemExit(f"missing from SessionStart context: {missing}")
'

echo "mail concision contract reaches SessionStart, mail, and mux surfaces"
