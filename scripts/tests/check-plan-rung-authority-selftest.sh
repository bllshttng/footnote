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

DELIVERY_READER="$FIXTURE_ROOT/crates/fno-agents/src/delivery_completion.rs"
PRISTINE_READER="$TMP_ROOT/delivery_completion.rs"
cp "$DELIVERY_READER" "$PRISTINE_READER"

assert_rejected() {
    local label="$1"
    local mutant="$2"
    local output=""
    local actual_exit=0

    cp "$PRISTINE_READER" "$DELIVERY_READER"
    printf '\n%s\n' "$mutant" >> "$DELIVERY_READER"
    output="$(bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" 2>&1)" \
        || actual_exit=$?

    if [[ "$actual_exit" -eq 0 ]]; then
        echo "FAIL: $label passed the authority guard" >&2
        exit 1
    fi
    if ! grep -q 'delivery_completion.rs' <<<"$output"; then
        echo "FAIL: $label violation did not name the delivery reader" >&2
        exit 1
    fi
}

assert_rejected 'direct mapping.get("status")' \
    'fn forbidden(mapping: &serde_yaml_ng::Mapping) { let _ = mapping.get("status"); }'
assert_rejected 'mapping["status"] indexing' \
    'fn forbidden(mapping: &serde_yaml_ng::Mapping) { let _ = &mapping["status"]; }'
assert_rejected 'Value::from("status") lookup' \
    'fn forbidden(mapping: &serde_yaml_ng::Mapping) { let _ = mapping.get(serde_yaml_ng::Value::from("status")); }'

echo "PASS: every registered Rust plan reader rejects frontmatter status parsing"
