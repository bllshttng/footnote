#!/usr/bin/env bash
# Smoke test: SKILL.md frontmatter is well-formed and the eight body sections
# from the plan are present. Validates AC1-HP and AC2-ERR of plan section A.2.
#
# Failure modes this catches:
#   - Frontmatter not at the top
#   - name/description missing or empty
#   - body sections renamed or removed (the recipient daemon parses kinds
#     case-sensitively, but the smoke test only checks heading presence)
#
# Run from anywhere; resolves the SKILL.md path via $0.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SKILL_FILE="$HERE/../SKILL.md"

if [[ ! -f "$SKILL_FILE" ]]; then
  echo "FAIL: SKILL.md not found at $SKILL_FILE" >&2
  exit 1
fi

# AC1-HP: frontmatter parses cleanly, name == "inbox", description non-empty.
SKILL_PATH="$SKILL_FILE" python3 - <<'PY'
import os
import sys
import yaml

path = os.environ["SKILL_PATH"]
content = open(path).read()
parts = content.split("---", 2)
if len(parts) < 3 or parts[0].strip():
    print(f"FAIL: frontmatter must start at line 1; got prefix: {parts[0]!r}", file=sys.stderr)
    sys.exit(1)

try:
    fm = yaml.safe_load(parts[1])
except yaml.YAMLError as exc:
    print(f"FAIL: frontmatter YAML parse error: {exc}", file=sys.stderr)
    sys.exit(1)

if not isinstance(fm, dict):
    print(f"FAIL: frontmatter is not a mapping: {type(fm).__name__}", file=sys.stderr)
    sys.exit(1)

if fm.get("name") != "inbox":
    print(f"FAIL: name must be 'inbox', got: {fm.get('name')!r}", file=sys.stderr)
    sys.exit(1)

description = fm.get("description") or ""
if not description.strip():
    print("FAIL: description is empty or missing", file=sys.stderr)
    sys.exit(1)

print("frontmatter OK")
PY

# AC2-ERR: all eight body sections present (heading literals from the plan).
EXPECTED_SECTIONS=(
  "What this is"
  "When to send"
  "five kinds"
  "Sender command"
  "Provenance flags"
  "What the recipient does"
  "Anti-patterns"
  "See also"
)

MISSING=()
for heading in "${EXPECTED_SECTIONS[@]}"; do
  if ! grep -q "$heading" "$SKILL_FILE"; then
    MISSING+=("$heading")
  fi
done

if (( ${#MISSING[@]} > 0 )); then
  echo "FAIL: missing body sections:" >&2
  for h in "${MISSING[@]}"; do
    echo "  - $h" >&2
  done
  exit 1
fi

echo "sections OK"

# AC4-EDGE: file under 500 lines (agentskills spec guideline).
LINES=$(wc -l < "$SKILL_FILE" | tr -d ' ')
if (( LINES >= 500 )); then
  echo "FAIL: SKILL.md is $LINES lines (must be < 500)" >&2
  exit 1
fi

echo "size OK ($LINES lines)"
echo "PASS: skills/inbox/tests/test_skill_metadata.sh"
