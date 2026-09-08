#!/usr/bin/env bash
# Cross-language provider vocabulary parity for registry rows and worker claims.
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

CLAUDE_RUST="$ROOT/crates/fno-agents/src/claude_ask.rs"
ADOPT_RUST="$ROOT/crates/fno-agents/src/claude_adopt.rs"
CODEX_RUST="$ROOT/crates/fno-agents/src/codex_ask.rs"
RUST_GATE="$ROOT/crates/fno-agents/src/spawn_gate.rs"
OVERLAY_RUST="$ROOT/crates/fno-agents/src/spawn_overlay.rs"
PYTHON_GATE="$ROOT/cli/src/fno/agents/spawn_gate.py"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --claude-rust) CLAUDE_RUST="$2"; shift 2 ;;
    --adopt-rust) ADOPT_RUST="$2"; shift 2 ;;
    --codex-rust) CODEX_RUST="$2"; shift 2 ;;
    --rust-gate) RUST_GATE="$2"; shift 2 ;;
    --overlay-rust) OVERLAY_RUST="$2"; shift 2 ;;
    --python-gate) PYTHON_GATE="$2"; shift 2 ;;
    -h|--help) sed -n '1,12p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for file in "$CLAUDE_RUST" "$ADOPT_RUST" "$CODEX_RUST" "$RUST_GATE" "$OVERLAY_RUST" "$PYTHON_GATE"; do
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

extract_rust_lane_vendor() {
  local file="$1" harness="$2"
  sed -n '/const HARNESS_DEFAULT_VENDOR/,/^];/p' "$file" \
    | sed -nE "s/.*\(\"${harness}\",[[:space:]]*\"([^\"]+)\"\).*/\1/p" \
    | head -n 1
}

extract_constant() {
  local file="$1" constant="$2"
  grep -E "^${constant}[[:space:]]*(:[^=]+)?[[:space:]]*=" "$file" 2>/dev/null \
    | sed -nE 's/.*=[[:space:]]*"([^"]+)".*/\1/p' \
    | head -n 1
}

rust_claude=$(extract_rust_provider "$CLAUDE_RUST" 'fn create(' '#[cfg(test)]')
rust_codex=$(extract_rust_provider "$CODEX_RUST" 'fn dispatch_create(' 'fn dispatch_resume(')
rust_unrouted=$(extract_constant "$RUST_GATE" 'const[[:space:]]+KNOWN_UNROUTED_PROVIDER')
# The harness-to-vendor vocabulary left spawn_defaults.py for spawn_overlay.rs
# (the lane-vendor verb is its only implementation now), so the claude/codex
# comparison reads the Rust table instead of a deleted Python one. The leg
# keeps its teeth: the stamps in claude_ask.rs/codex_ask.rs can still drift
# from the lane table in a different file.
overlay_claude=$(extract_rust_lane_vendor "$OVERLAY_RUST" claude)
overlay_codex=$(extract_rust_lane_vendor "$OVERLAY_RUST" codex)
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
require_value 'Rust Codex provider' "$rust_codex"

# The adopt path carries NO provider literal: the mint stamps None (adoption
# observed no route, and the retired unconditional vendor was exactly the
# wrong-bill guess a vocabulary gate should never enforce), and `adopt`
# resolves the provider from the route-settings match, recording none on no
# match. Both shapes are the contract, so both are asserted here.
adopt_mint=$(awk -v start="pub fn mint_adopted_entry" -v end="pub fn upsert_adopted_row" '
  index($0, start) == 1 { inside = 1 }
  inside { print }
  inside && index($0, end) == 1 { exit }
' "$ADOPT_RUST")
if [[ "$adopt_mint" != *'provider: None'* ]]; then
  echo 'ERROR: Rust adopted-Claude mint must stamp provider: None (adoption observed no route)' >&2
  failed=1
fi
if ! grep -q 'provider_from_route_settings(Some(&model))' "$ADOPT_RUST"; then
  echo 'ERROR: Rust adopt must resolve the provider via provider_from_route_settings, never a vendor literal' >&2
  failed=1
fi
require_value 'Rust unrouted sentinel' "$rust_unrouted"
require_value 'Rust lane table claude vendor' "$overlay_claude"
require_value 'Rust lane table codex vendor' "$overlay_codex"
require_value 'Python unrouted sentinel' "$python_unrouted"

compare() {
  local left_label="$1" left="$2" right_label="$3" right="$4"
  if [[ -n "$left" && -n "$right" && "$left" != "$right" ]]; then
    echo "ERROR: provider vocabulary mismatch: $left_label='$left', $right_label='$right'" >&2
    failed=1
  fi
}

compare 'Rust Claude spawn' "$rust_claude" 'Rust lane table claude vendor' "$overlay_claude"
# The adopt path is checked structurally above (None at mint, route-settings
# match at adopt), not against the claude default: adoption observes no route,
# so no vendor literal may stand in for one.
compare 'Rust Codex create' "$rust_codex" 'Rust lane table codex vendor' "$overlay_codex"
compare 'Rust unrouted claim' "$rust_unrouted" 'Python unrouted claim reader' "$python_unrouted"

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi

echo "provider vocabulary parity OK: claude=$overlay_claude codex=$overlay_codex unrouted=$python_unrouted"
