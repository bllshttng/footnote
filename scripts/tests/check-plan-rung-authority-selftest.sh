#!/usr/bin/env bash

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TMP_ROOT="$(mktemp -d)"
trap 'rm -rf "$TMP_ROOT"' EXIT

FIXTURE_ROOT="$TMP_ROOT/repo"
git clone --quiet --no-hardlinks "$REPO_ROOT" "$FIXTURE_ROOT"

# Exercise the candidate guard even before it is committed.
cp "$REPO_ROOT/scripts/ci/check-plan-rung-authority.sh" \
    "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh"

if ! bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" >/dev/null; then
    echo "FAIL: clean fixture must pass the plan-rung authority guard" >&2
    exit 1
fi

printf '\nfn forbidden_status_lookup(mapping: &serde_yaml_ng::Mapping) {\n    let _ = mapping.get(serde_yaml_ng::Value::from("status"));\n}\n' \
    >> "$FIXTURE_ROOT/crates/fno-agents/src/delivery_completion.rs"

output=""
actual_exit=0
output="$(bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" 2>&1)" \
    || actual_exit=$?

if [[ "$actual_exit" -eq 0 ]]; then
    echo "FAIL: delivery reader status parsing passed the authority guard" >&2
    exit 1
fi
if ! grep -q 'delivery_completion.rs' <<<"$output"; then
    echo "FAIL: violation did not name the delivery reader" >&2
    exit 1
fi

echo "PASS: every registered Rust plan reader rejects frontmatter status parsing"
