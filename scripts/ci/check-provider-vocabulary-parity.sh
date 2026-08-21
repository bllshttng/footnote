#!/usr/bin/env bash
# Cross-language provider vocabulary parity for registry rows and worker claims.
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

CLAUDE_RUST="$ROOT/crates/fno-agents/src/claude_ask.rs"
ADOPT_RUST="$ROOT/crates/fno-agents/src/claude_adopt.rs"
CODEX_RUST="$ROOT/crates/fno-agents/src/codex_ask.rs"
RUST_GATE="$ROOT/crates/fno-agents/src/spawn_gate.rs"
PYTHON_DEFAULTS="$ROOT/cli/src/fno/agents/spawn_defaults.py"
PYTHON_GATE="$ROOT/cli/src/fno/agents/spawn_gate.py"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --claude-rust) CLAUDE_RUST="$2"; shift 2 ;;
    --adopt-rust) ADOPT_RUST="$2"; shift 2 ;;
    --codex-rust) CODEX_RUST="$2"; shift 2 ;;
    --rust-gate) RUST_GATE="$2"; shift 2 ;;
    --python-defaults) PYTHON_DEFAULTS="$2"; shift 2 ;;
    --python-gate) PYTHON_GATE="$2"; shift 2 ;;
    -h|--help) sed -n '1,12p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for file in "$CLAUDE_RUST" "$ADOPT_RUST" "$CODEX_RUST" "$RUST_GATE" "$PYTHON_DEFAULTS" "$PYTHON_GATE"; do
  if [[ ! -f "$file" ]]; then
    echo "ERROR: provider vocabulary source not found: $file" >&2
    exit 1
  fi
done

extract_rust_provider() {
  local file="$1" start="$2" end="$3"
  awk -v start="$start" -v end="$end" '
    index($0, start) == 1 { inside = 1 }
    inside { print }
    inside && index($0, end) == 1 { exit }
  ' "$file" \
    | sed -nE 's/.*provider:[[:space:]]*Some\("([^"]+)".*/\1/p' \
    | head -n 1
}

extract_python_default() {
  local file="$1" harness="$2"
  sed -n '/^_HARNESS_DEFAULT_VENDOR = {/,/^}/p' "$file" \
    | sed -nE "s/.*\"${harness}\"[[:space:]]*:[[:space:]]*\"([^\"]+)\".*/\1/p" \
    | head -n 1
}

extract_constant() {
  local file="$1" constant="$2"
  grep -E "^${constant}[[:space:]]*(:[^=]+)?[[:space:]]*=" "$file" 2>/dev/null \
    | sed -nE 's/.*=[[:space:]]*"([^"]+)".*/\1/p' \
    | head -n 1
}

rust_claude=$(extract_rust_provider "$CLAUDE_RUST" 'fn create(' '#[cfg(test)]')
rust_adopt=$(extract_rust_provider "$ADOPT_RUST" 'pub fn mint_adopted_entry' 'pub fn upsert_adopted_row')
rust_codex=$(extract_rust_provider "$CODEX_RUST" 'fn dispatch_create(' 'fn dispatch_resume(')
rust_unrouted=$(extract_constant "$RUST_GATE" 'const[[:space:]]+KNOWN_UNROUTED_PROVIDER')
python_claude=$(extract_python_default "$PYTHON_DEFAULTS" claude)
python_codex=$(extract_python_default "$PYTHON_DEFAULTS" codex)
python_unrouted=$(extract_constant "$PYTHON_GATE" '_KNOWN_UNROUTED_PROVIDER')

failed=0
require_value() {
  local label="$1" value="$2"
  if [[ ! "$value" =~ ^[a-z_][a-z0-9_-]*$ ]]; then
    echo "ERROR: $label is missing or malformed (got '$value')" >&2
    failed=1
  fi
}

require_value 'Rust Claude provider' "$rust_claude"
require_value 'Rust adopted-Claude provider' "$rust_adopt"
require_value 'Rust Codex provider' "$rust_codex"
require_value 'Rust unrouted sentinel' "$rust_unrouted"
require_value 'Python Claude default provider' "$python_claude"
require_value 'Python Codex default provider' "$python_codex"
require_value 'Python unrouted sentinel' "$python_unrouted"

resolver_body=$(sed -n '/^def resolve_lane_vendor(/,/^def /p' "$PYTHON_DEFAULTS")
if [[ "$resolver_body" != *'lane = _HARNESS_DEFAULT_VENDOR.get(resolved_harness)'* ]] \
  || [[ "$resolver_body" != *'return lane'* ]]; then
  echo 'ERROR: resolve_lane_vendor no longer returns _HARNESS_DEFAULT_VENDOR values' >&2
  failed=1
fi

compare() {
  local left_label="$1" left="$2" right_label="$3" right="$4"
  if [[ -n "$left" && -n "$right" && "$left" != "$right" ]]; then
    echo "ERROR: provider vocabulary mismatch: $left_label='$left', $right_label='$right'" >&2
    failed=1
  fi
}

compare 'Rust Claude spawn' "$rust_claude" 'Python Claude default' "$python_claude"
compare 'Rust Claude adopt' "$rust_adopt" 'Python Claude default' "$python_claude"
compare 'Rust Codex create' "$rust_codex" 'Python Codex default' "$python_codex"
compare 'Rust unrouted claim' "$rust_unrouted" 'Python unrouted claim reader' "$python_unrouted"

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi

echo "provider vocabulary parity OK: claude=$python_claude codex=$python_codex unrouted=$python_unrouted"
