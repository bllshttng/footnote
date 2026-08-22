#!/usr/bin/env bash
# Contract test for the /fno:agent send grammar and receipt semantics.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SKILL="$REPO_ROOT/skills/agent/SKILL.md"

PASS=0
FAIL=0
pass() { echo "  PASS: $*"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $*"; FAIL=$((FAIL + 1)); }

SECTION="$(sed -n '/^## `send/,/^---$/p' "$SKILL")"

require_text() {
  local label="$1" needle="$2"
  if [[ "$SECTION" == *"$needle"* ]]; then
    pass "$label"
  else
    fail "$label: missing $needle"
  fi
}

require_text "agent default: live-first argv" 'fno agents mail send "<agent>" "<body>"'
require_text "agent heads-up: explicit kind argv" 'fno agents mail send "<agent>" --kind heads-up "<body>"'
require_text "project: explicit kind argv" 'fno agents mail send --to-project "<project>" --kind "<kind>" "<body>"'
require_text "grammar: only explicit token selects kind" 'Only an explicit `--kind` token selects a kind.'
require_text "grammar: body suffix remains body" 'A final body word such as `question`, `heads-up`, or `fyi` remains body text.'
require_text "guard: genuine CLI sees incompatible handle kind" 'Pass every syntactically valid explicit kind to the genuine CLI'
require_text "routing: agent heads-up resolves canonically" "resolves an agent-scoped \`heads-up\` to the recipient's canonical session handle before the durable write"
require_text "guard: exact refusal is relayed" 'relay its stderr unchanged'
require_text "receipt: durable is not upgraded" '`queued (durable)` is not delivered'
require_text "unknown heads-up: exit 16 without write" 'An unknown agent heads-up exits 16 and writes nothing'

echo ""
echo "agent-send-contract: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
