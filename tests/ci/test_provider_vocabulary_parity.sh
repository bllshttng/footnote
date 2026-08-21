#!/usr/bin/env bash
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
CHECK="$ROOT/scripts/ci/check-provider-vocabulary-parity.sh"

if [[ ! -x "$CHECK" ]]; then
  echo "FAIL: provider vocabulary parity checker is missing or not executable" >&2
  exit 1
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/provider-vocabulary-parity.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

write_match() {
  printf '%s\n' \
    'fn create(' \
    'provider: Some("anthropic".to_string()),' \
    '#[cfg(test)]' > "$tmp/claude.rs"
  printf '%s\n' \
    'pub fn mint_adopted_entry' \
    'provider: Some("anthropic".into()),' \
    'pub fn upsert_adopted_row' > "$tmp/adopt.rs"
  printf '%s\n' \
    'fn dispatch_create(' \
    'provider: Some("openai".to_string()),' \
    'fn dispatch_resume(' > "$tmp/codex.rs"
  printf '%s\n' \
    'const KNOWN_UNROUTED_PROVIDER: &str = "__uncapped__";' > "$tmp/rust-gate.rs"
  printf '%s\n' \
    '_HARNESS_DEFAULT_VENDOR = {' \
    '    "claude": "anthropic",' \
    '    "codex": "openai",' \
    '}' \
    'def resolve_lane_vendor(argv, harness=None):' \
    '    resolved_harness = harness' \
    '    lane = _HARNESS_DEFAULT_VENDOR.get(resolved_harness)' \
    '    if lane:' \
    '        return lane' > "$tmp/defaults.py"
  printf '%s\n' \
    '_KNOWN_UNROUTED_PROVIDER = "__uncapped__"' > "$tmp/python-gate.py"
}

check() {
  "$CHECK" \
    --claude-rust "$tmp/claude.rs" \
    --adopt-rust "$tmp/adopt.rs" \
    --codex-rust "$tmp/codex.rs" \
    --rust-gate "$tmp/rust-gate.rs" \
    --python-defaults "$tmp/defaults.py" \
    --python-gate "$tmp/python-gate.py"
}

write_match
if check >/dev/null 2>&1; then
  echo "PASS: matching provider vocabulary accepted"
else
  echo "FAIL: matching provider vocabulary rejected" >&2
  exit 1
fi

printf '%s\n' \
  'fn dispatch_create(' \
  'provider: Some("zai".to_string()),' \
  'fn dispatch_resume(' > "$tmp/codex.rs"
if check >/dev/null 2>&1; then
  echo "FAIL: Codex provider drift accepted" >&2
  exit 1
else
  echo "PASS: Codex provider drift rejected"
fi

write_match
printf '%s\n' \
  'const KNOWN_UNROUTED_PROVIDER: &str = "uncapped";' > "$tmp/rust-gate.rs"
if check >/dev/null 2>&1; then
  echo "FAIL: unrouted sentinel drift accepted" >&2
  exit 1
else
  echo "PASS: unrouted sentinel drift rejected"
fi

write_match
printf '%s\n' 'fn create(' '#[cfg(test)]' > "$tmp/claude.rs"
if check >/dev/null 2>&1; then
  echo "FAIL: missing Claude provider stamp accepted" >&2
  exit 1
else
  echo "PASS: missing Claude provider stamp rejected"
fi

echo "provider vocabulary parity test: all cases passed"
