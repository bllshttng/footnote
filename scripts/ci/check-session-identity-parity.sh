#!/usr/bin/env bash
# Cross-language session identity parity for installed-user birth writers.
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

CLAUDE_RUST="$ROOT/crates/fno-agents/src/claude_ask.rs"
ADOPT_RUST="$ROOT/crates/fno-agents/src/claude_adopt.rs"
PYTHON_REGISTRY="$ROOT/cli/src/fno/agents/registry.py"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --claude-rust) CLAUDE_RUST="$2"; shift 2 ;;
    --adopt-rust) ADOPT_RUST="$2"; shift 2 ;;
    --python-registry) PYTHON_REGISTRY="$2"; shift 2 ;;
    -h|--help) sed -n '1,10p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for file in "$CLAUDE_RUST" "$ADOPT_RUST" "$PYTHON_REGISTRY"; do
  if [[ ! -f "$file" ]]; then
    echo "ERROR: session identity source not found: $file" >&2
    exit 1
  fi
done

extract_block() {
  local file="$1" start="$2" end="$3"
  awk -v start="$start" -v end="$end" '
    index($0, start) == 1 { inside = 1 }
    inside { print }
    inside && index($0, end) == 1 { exit }
  ' "$file"
}

claude_spawn=$(extract_block "$CLAUDE_RUST" 'fn create(' '#[cfg(test)]')
claude_adopt=$(extract_block "$ADOPT_RUST" 'pub fn mint_adopted_entry' 'pub fn upsert_adopted_row')
python_register=$(extract_block "$PYTHON_REGISTRY" 'def register_existing_session(' 'def restamp_harness_session_id(')

failed=0
require_stamp() {
  local label="$1" body="$2" stamp="$3"
  if [[ "$body" != *"$stamp"* ]]; then
    echo "ERROR: $label omits canonical harness_session_id birth stamp" >&2
    failed=1
  fi
}

require_stamp 'Rust Claude spawn writer' "$claude_spawn" 'harness_session_id: session_uuid.clone(),'
require_stamp 'Rust Claude adopt writer' "$claude_adopt" 'harness_session_id: Some(w.session_id.clone()),'
require_stamp 'Python registry session writer' "$python_register" 'harness_session_id=session_id,'

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi

echo 'session identity parity OK: Rust spawn/adopt and Python register stamp harness_session_id'
