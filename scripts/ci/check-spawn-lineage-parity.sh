#!/usr/bin/env bash
# Cross-language spawn-lineage parity: every registry mint site stamps the
# spawned_by_* parent edge. The 2026-08-22 fleet measurement (0 of 30 live rows
# stamped) found each birth path ships unstamped until a consumer trips over
# the hole; this gate fails the PR that drops a stamp, in either language.
set -uo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$SCRIPT_DIR/../.." && pwd)

CLAUDE_RUST="$ROOT/crates/fno-agents/src/claude_ask.rs"
ADOPT_RUST="$ROOT/crates/fno-agents/src/claude_adopt.rs"
CODEX_RUST="$ROOT/crates/fno-agents/src/codex_ask.rs"
GEMINI_RUST="$ROOT/crates/fno-agents/src/gemini_ask.rs"
STREAM_RUST="$ROOT/crates/fno-agents/src/daemon.rs"
SYNTH_RUST="$ROOT/crates/fno-agents/src/client_verbs.rs"
STATE_RUST="$ROOT/crates/fno-agents/src/state.rs"
CLAIMS_RUST="$ROOT/crates/fno-agents/src/claims.rs"
CODEX_PY="$ROOT/cli/src/fno/agents/dispatch.py"
MUX_PY="$ROOT/cli/src/fno/agents/mux_spawn.py"
REGISTRY_PY="$ROOT/cli/src/fno/agents/registry.py"
FALLBACK_PY="$ROOT/cli/src/fno/agents/store_fallback.py"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --claude-rust) CLAUDE_RUST="$2"; shift 2 ;;
    --adopt-rust) ADOPT_RUST="$2"; shift 2 ;;
    --codex-rust) CODEX_RUST="$2"; shift 2 ;;
    --gemini-rust) GEMINI_RUST="$2"; shift 2 ;;
    --stream-rust) STREAM_RUST="$2"; shift 2 ;;
    --synth-rust) SYNTH_RUST="$2"; shift 2 ;;
    --state-rust) STATE_RUST="$2"; shift 2 ;;
    --claims-rust) CLAIMS_RUST="$2"; shift 2 ;;
    --codex-py) CODEX_PY="$2"; shift 2 ;;
    --mux-py) MUX_PY="$2"; shift 2 ;;
    --registry-py) REGISTRY_PY="$2"; shift 2 ;;
    --fallback-py) FALLBACK_PY="$2"; shift 2 ;;
    -h|--help) sed -n '1,6p' "$0"; exit 0 ;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
  esac
done

for file in "$CLAUDE_RUST" "$ADOPT_RUST" "$CODEX_RUST" "$GEMINI_RUST" \
            "$STREAM_RUST" "$SYNTH_RUST" "$STATE_RUST" "$CLAIMS_RUST" \
            "$CODEX_PY" "$MUX_PY" "$REGISTRY_PY" "$FALLBACK_PY"; do
  if [[ ! -f "$file" ]]; then
    echo "ERROR: spawn-lineage source not found: $file" >&2
    exit 1
  fi
done

failed=0

# section <file> <start-anchor> <end-anchor> <label> <required-string>...
# Extracts the named region (line-start anchors, provider-parity shape) and
# requires EVERY string in the site's stamp.
section() {
  local file="$1" start="$2" end="$3" label="$4"
  shift 4
  local body
  body=$(awk -v start="$start" -v end="$end" '
    index($0, start) == 1 { inside = 1 }
    inside { print }
    inside && index($0, end) == 1 { exit }
  ' "$file")
  if [[ -z "$body" ]]; then
    echo "ERROR: $label: section '$start' not found in $file" >&2
    failed=1
    return
  fi
  local needle
  for needle in "$@"; do
    if [[ "$body" != *"$needle"* ]]; then
      echo "ERROR: $label: missing '$needle' in the mint site" >&2
      failed=1
    fi
  done
}

RUST_CALL='crate::claims::ambient_parent_edge()'
RUST_STAMP='spawned_by_session: parent_session'

section "$CLAUDE_RUST" 'fn create(' '#[cfg(test)]' 'Rust claude create' \
  "$RUST_CALL" "$RUST_STAMP"
section "$ADOPT_RUST" 'pub fn mint_adopted_entry' 'pub fn upsert_adopted_row' \
  'Rust claude adopt' "$RUST_CALL" "$RUST_STAMP"
section "$CODEX_RUST" 'fn dispatch_create(' 'fn dispatch_resume(' \
  'Rust codex create' "$RUST_CALL" "$RUST_STAMP"
section "$GEMINI_RUST" 'fn dispatch_create(' 'fn dispatch_resume(' \
  'Rust gemini create' "$RUST_CALL" "$RUST_STAMP"
section "$STREAM_RUST" 'fn build_claude_stream_entry' 'fn acquire_session_claim' \
  'Rust daemon stream worker' "$RUST_CALL" "$RUST_STAMP"
section "$SYNTH_RUST" 'fn mint_synthesized_entry' 'fn upsert_synthesized_row' \
  'Rust synthesized row' "$RUST_CALL" "$RUST_STAMP"

for field in spawned_by_session spawned_by_harness spawned_by_cwd; do
  if ! grep -q "pub ${field}:" "$STATE_RUST"; then
    echo "ERROR: Rust RegistryEntry no longer declares $field" >&2
    failed=1
  fi
done
if ! grep -q 'pub fn ambient_parent_edge(' "$CLAIMS_RUST"; then
  echo "ERROR: claims.rs no longer exposes the ambient parent-edge helper" >&2
  failed=1
fi

section "$CODEX_PY" 'def _codex_create_path(' 'def _codex_followup_path(' \
  'Python codex create' '_capture_parent_edge()' 'spawned_by_session=_cx_session'
section "$CODEX_PY" 'def _claude_create_path(' 'def dispatch_ask(' \
  'Python claude create' 'spawned_by_session=spawned_by_session'
section "$MUX_PY" 'def dispatch_spawn_pane(' '__END_OF_FILE__' \
  'Python pane spawn' 'spawned_by_session=spawned_by_session'
section "$REGISTRY_PY" 'def register_existing_session(' 'def restamp_harness_session_id(' \
  'Python register' 'origin == "operator"' 'spawned_by_session=_sb_session'
if ! grep -q 'spawned_by_session=_sb_session' "$FALLBACK_PY"; then
  echo "ERROR: Python store fallback no longer stamps the parent edge" >&2
  failed=1
fi

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi

echo "spawn lineage parity OK: 6 Rust + 5 Python mint sites stamp the parent edge"
