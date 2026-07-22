#!/usr/bin/env bash
# Scan a log file for Skill() invocations and CLI invocations.
# Outputs JSON with counts broken down by skill/subcommand.
#
# Usage:
#   bash record-transcript.sh <log-file>
#   bash record-transcript.sh --live-session  # NOT supported during THIS session (inflates count)
#
# Output JSON:
#   {"total_invocations": N, "skill_invocations": N, "by_skill": {...}, "by_subcommand": {...}, "source": "log-file"}
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "Usage: record-transcript.sh <log-file> | --live-session" >&2
  exit 1
fi

if [[ "$1" == "--live-session" ]]; then
  echo "ERROR: --live-session is not safe during the current session - it would inflate counts with this run's tool calls" >&2
  echo "Use a log file produced by dogfood-driver.sh instead." >&2
  exit 1
fi

LOG_FILE="$1"

if [[ ! -f "$LOG_FILE" ]]; then
  echo "ERROR: log file not found: $LOG_FILE" >&2
  exit 1
fi

# Delegate all parsing to Python to avoid bash associative array portability issues
python3 - "$LOG_FILE" <<'PYEOF'
import sys
import re
import json
from collections import defaultdict

log_file = sys.argv[1]
content = open(log_file).read()
lines = content.splitlines()

# Count Skill() invocations
skill_lines = [l for l in lines if 'Skill(' in l]
skill_count = len(skill_lines)

# Count by skill name
by_skill = defaultdict(int)
for line in skill_lines:
    m = re.search(r"Skill\(['\"]([^'\"]+)", line)
    if m:
        by_skill[m.group(1)] += 1

# Count fno CLI invocations from "INVOKE[N]: fno ..." lines
invoke_lines = [l for l in lines if re.match(r'^INVOKE\[', l)]
total_invocations = len(invoke_lines)

# Count by subcommand tree
subcommands = ['state', 'graph', 'runtime', 'worker', 'event', 'gate', 'reality-check', 'probe']
by_subcommand = {}
for sub in subcommands:
    by_subcommand[sub] = sum(1 for l in invoke_lines if f'fno {sub}' in l)

result = {
    "total_invocations": total_invocations,
    "skill_invocations": skill_count,
    "by_skill": dict(by_skill),
    "by_subcommand": by_subcommand,
    "source": "log-file",
    "log_file": log_file,
}
print(json.dumps(result, indent=2))
PYEOF
