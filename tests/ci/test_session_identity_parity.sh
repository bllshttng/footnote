#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CHECK="$ROOT/scripts/ci/check-session-identity-parity.sh"

if [[ ! -x "$CHECK" ]]; then
  echo "FAIL: session identity parity checker is missing or not executable" >&2
  exit 1
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/session-identity-parity.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

write_match() {
  printf '%s\n' \
    'fn create(' \
    'harness_session_id: session_uuid.clone(),' \
    '#[cfg(test)]' > "$tmp/claude.rs"
  printf '%s\n' \
    'pub fn mint_adopted_entry' \
    'harness_session_id: Some(w.session_id.clone()),' \
    'pub fn upsert_adopted_row' > "$tmp/adopt.rs"
  printf '%s\n' \
    'def register_existing_session(' \
    '        fresh = AgentEntry(' \
    '            harness_session_id=session_id,' \
    '        )' \
    'def next_function(' > "$tmp/registry.py"
}

check() {
  "$CHECK" \
    --claude-rust "$tmp/claude.rs" \
    --adopt-rust "$tmp/adopt.rs" \
    --python-registry "$tmp/registry.py"
}

write_match
if check >/dev/null 2>&1; then
  echo "PASS: matching session identity birth fields accepted"
else
  echo "FAIL: matching session identity birth fields rejected" >&2
  exit 1
fi

write_match
printf '%s\n' \
  'fn create(' \
  '#[cfg(test)]' > "$tmp/claude.rs"
if check >/dev/null 2>&1; then
  echo "FAIL: missing Rust Claude spawn identity accepted" >&2
  exit 1
else
  echo "PASS: missing Rust Claude spawn identity rejected"
fi

write_match
printf '%s\n' \
  'def register_existing_session(' \
  '        fresh = AgentEntry(' \
  '        )' \
  'def next_function(' > "$tmp/registry.py"
if check >/dev/null 2>&1; then
  echo "FAIL: missing Python registry birth identity accepted" >&2
  exit 1
else
  echo "PASS: missing Python registry birth identity rejected"
fi

echo "session identity parity test: all cases passed"
