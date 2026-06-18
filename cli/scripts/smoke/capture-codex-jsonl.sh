#!/usr/bin/env bash
# Capture a codex --json stream once so the parser pins event-type constants
# from a real capture (Locked Decision 13: smoke-test FIRST, encode constants
# from capture). The implementation does NOT guess the strings.
#
# Usage:
#   CODEX_SMOKE=1 bash scripts/smoke/capture-codex-jsonl.sh
#
# Without CODEX_SMOKE=1 the script prints a skip note and exits 0 so CI hosts
# without codex installed can still run the rest of the test suite.
#
# Outputs:
#   - JSONL stream tee'd to stdout AND tests/agents/fixtures/codex-jsonl-sample.jsonl
#   - The distinct ``type`` field values that appeared, printed to stderr
#
# Failure modes:
#   - codex not on PATH: exit 14 with diagnostic.
#   - codex exits non-zero: print exit code on stderr but do not delete the
#     fixture (partial captures are useful for forensics).
#   - Empty JSONL stream: exit 11 with diagnostic (caller can investigate).

set -euo pipefail

if [[ "${CODEX_SMOKE:-0}" != "1" ]]; then
    echo "codex-jsonl-capture: CODEX_SMOKE!=1, skipping (set CODEX_SMOKE=1 to run)" >&2
    exit 0
fi

if ! command -v codex >/dev/null 2>&1; then
    echo "codex-jsonl-capture: codex CLI not on PATH; install codex 0.130.0+ first" >&2
    exit 14
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "codex-jsonl-capture: jq not on PATH; install with 'brew install jq' or your distro's package manager" >&2
    exit 14
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLI_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
FIXTURE_DIR="${CLI_ROOT}/tests/agents/fixtures"
FIXTURE_FILE="${FIXTURE_DIR}/codex-jsonl-sample.jsonl"

mkdir -p "${FIXTURE_DIR}"

# Capture once. Use a stable prompt that any model will reply to. /tmp is
# cwd because we don't want the smoke to require the host be in a repo.
echo "codex-jsonl-capture: running codex exec --json --cd /tmp 'echo hello'" >&2

# Stream stdout AND stderr (codex merges some warnings into stderr) so the
# fixture matches what the parser will see when we set stderr=subprocess.STDOUT
# in providers/codex.py (Locked Decision 12).
codex exec --json --cd /tmp --skip-git-repo-check --sandbox workspace-write \
    'echo hello in one word' 2>&1 | tee "${FIXTURE_FILE}"

# Extract distinct top-level type values. Tolerate non-JSON lines (codex
# emits "Reading additional input from stdin..." before the JSONL stream).
# Pre-filter to lines starting with '{' before piping to jq so jq's parser
# is not killed by the leading plain-text banner.
echo "" >&2
echo "codex-jsonl-capture: distinct event types seen:" >&2
TYPES_FOUND="$(grep '^{' "${FIXTURE_FILE}" | jq -r '.type // empty' 2>/dev/null | sort -u || true)"
if [[ -z "${TYPES_FOUND}" ]]; then
    echo "  <none>  (0-line or all-non-JSON capture — investigate)" >&2
    exit 11
fi
echo "${TYPES_FOUND}" | sed 's/^/  /' >&2

# Also surface the inner item.type values for item.completed events, since
# those are the discriminator for agent_message vs error.
echo "" >&2
echo "codex-jsonl-capture: distinct item.type values inside item.completed:" >&2
ITEM_TYPES="$(grep '^{' "${FIXTURE_FILE}" | jq -r 'select(.type=="item.completed") | .item.type // empty' 2>/dev/null | sort -u || true)"
if [[ -z "${ITEM_TYPES}" ]]; then
    echo "  <none>" >&2
else
    echo "${ITEM_TYPES}" | sed 's/^/  /' >&2
fi

echo "" >&2
echo "codex-jsonl-capture: fixture saved to ${FIXTURE_FILE}" >&2
