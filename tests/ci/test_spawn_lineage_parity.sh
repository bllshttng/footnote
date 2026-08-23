#!/usr/bin/env bash
# Selftest for check-spawn-lineage-parity.sh: the detector harness proves a
# dropped stamp FAILS the check before the canonical sources are trusted.
# Fixtures are written with printf only - no in-place sed, whose -i syntax
# differs between BSD and GNU and broke this selftest on the Linux runner.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CHECK="$ROOT/scripts/ci/check-spawn-lineage-parity.sh"

if [[ ! -x "$CHECK" ]]; then
  echo "FAIL: spawn lineage parity checker is missing or not executable" >&2
  exit 1
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/spawn-lineage-parity.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

RUST_CALL='let (parent_session, parent_harness, parent_cwd) = crate::claims::ambient_parent_edge();'
RUST_STAMP='spawned_by_session: parent_session,'

# Every fixture carries the full stamp: the helper call plus the field wiring
# the check demands. Variant files are written whole, never edited in place.
write_fixtures() {
  printf '%s\n' \
    'fn create(' \
    "$RUST_CALL" \
    "$RUST_STAMP" \
    '#[cfg(test)]' > "$tmp/claude.rs"
  printf '%s\n' \
    'pub fn mint_adopted_entry' \
    "$RUST_CALL" \
    "$RUST_STAMP" \
    'pub fn upsert_adopted_row' > "$tmp/adopt.rs"
  printf '%s\n' \
    'fn dispatch_create(' \
    "$RUST_CALL" \
    "$RUST_STAMP" \
    'fn dispatch_resume(' > "$tmp/codex.rs"
  cp "$tmp/codex.rs" "$tmp/gemini.rs"
  printf '%s\n' \
    'fn build_claude_stream_entry' \
    "$RUST_CALL" \
    "$RUST_STAMP" \
    'fn acquire_session_claim' > "$tmp/stream.rs"
  printf '%s\n' \
    'fn mint_synthesized_entry' \
    "$RUST_CALL" \
    "$RUST_STAMP" \
    'fn upsert_synthesized_row' > "$tmp/synth.rs"
  printf '%s\n' \
    'pub spawned_by_session: Option<String>,' \
    'pub spawned_by_harness: Option<String>,' \
    'pub spawned_by_cwd: Option<String>,' > "$tmp/state.rs"
  printf '%s\n' 'pub fn ambient_parent_edge(' > "$tmp/claims.rs"
  printf '%s\n' \
    'def _codex_create_path(' \
    'x = _capture_parent_edge()' \
    'spawned_by_session=_cx_session,' \
    'def _codex_followup_path(' \
    'def _claude_create_path(' \
    'spawned_by_session=spawned_by_session,' \
    'def dispatch_ask(' > "$tmp/dispatch.py"
  printf '%s\n' \
    'def dispatch_spawn_pane(' \
    'spawned_by_session=spawned_by_session,' > "$tmp/mux.py"
  printf '%s\n' \
    'def register_existing_session(' \
    'if origin == "operator":' \
    'spawned_by_session=_sb_session,' \
    'def restamp_harness_session_id(' > "$tmp/registry.py"
  printf '%s\n' 'spawned_by_session=_sb_session,' > "$tmp/fallback.py"
}

run_check() {
  "$CHECK" \
    --claude-rust "$tmp/claude.rs" \
    --adopt-rust "$tmp/adopt.rs" \
    --codex-rust "$tmp/codex.rs" \
    --gemini-rust "$tmp/gemini.rs" \
    --stream-rust "$tmp/stream.rs" \
    --synth-rust "$tmp/synth.rs" \
    --state-rust "$tmp/state.rs" \
    --claims-rust "$tmp/claims.rs" \
    --codex-py "$tmp/dispatch.py" \
    --mux-py "$tmp/mux.py" \
    --registry-py "$tmp/registry.py" \
    --fallback-py "$tmp/fallback.py"
}

write_fixtures
if ! out=$(run_check 2>&1); then
  echo "FAIL: check rejected a fully-stamped fixture set: $out" >&2
  exit 1
fi
if [[ "$out" != *'parity OK'* ]]; then
  echo "FAIL: check passed without the OK marker: $out" >&2
  exit 1
fi

# Negative: the codex site with NO stamp; the failure must name it.
printf '%s\n' \
  'fn dispatch_create(' \
  "$RUST_CALL" \
  'fn dispatch_resume(' > "$tmp/codex.rs"
if out=$(run_check 2>&1); then
  echo "FAIL: check passed a codex mint site with no stamp: $out" >&2
  exit 1
fi
if [[ "$out" != *'Rust codex create'* ]]; then
  echo "FAIL: failure did not name the offending site: $out" >&2
  exit 1
fi

# Negative: an operator register that stamps everything (the self-edge defect).
write_fixtures
printf '%s\n' \
  'def register_existing_session(' \
  'if False:' \
  'spawned_by_session=_sb_session,' \
  'def restamp_harness_session_id(' > "$tmp/registry.py"
if out=$(run_check 2>&1); then
  echo "FAIL: check passed a register path with no operator refusal: $out" >&2
  exit 1
fi

echo "spawn lineage parity selftest OK"
