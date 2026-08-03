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
KILL_READER="$FIXTURE_ROOT/crates/fno-agents/src/kill_criteria.rs"
PRISTINE_KILL_READER="$TMP_ROOT/kill_criteria.rs"
cp "$KILL_READER" "$PRISTINE_KILL_READER"

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
cp "$PRISTINE_READER" "$DELIVERY_READER"

assert_kill_rejected() {
    local label="$1"
    local mutant="$2"
    local output=""
    local actual_exit=0

    cp "$PRISTINE_KILL_READER" "$KILL_READER"
    printf '\n%s\n' "$mutant" >> "$KILL_READER"
    output="$(bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" 2>&1)" \
        || actual_exit=$?

    if [[ "$actual_exit" -eq 0 ]]; then
        echo "FAIL: $label passed the authority guard" >&2
        exit 1
    fi
    if ! grep -q 'kill_criteria.rs' <<<"$output"; then
        echo "FAIL: $label violation did not name the kill-criteria reader" >&2
        exit 1
    fi
}

assert_kill_rejected 'kill reader mapping.get("status")' \
    'fn forbidden(mapping: &serde_yaml_ng::Mapping) { let _ = mapping.get("status"); }'
assert_kill_rejected 'kill reader mapping["status"] indexing' \
    'fn forbidden(mapping: &serde_yaml_ng::Mapping) { let _ = &mapping["status"]; }'
assert_kill_rejected 'kill reader key comparison' \
    'fn forbidden(key: &str) -> bool { key == "status" }'
assert_kill_rejected 'kill reader matches comparison' \
    'fn forbidden(key: &str) -> bool { matches!(key, "status") }'
cp "$PRISTINE_KILL_READER" "$KILL_READER"

ROGUE_READER="$FIXTURE_ROOT/crates/fno-agents/src/rogue_reader.rs"
cat > "$ROGUE_READER" <<'EOF'
fn forbidden(path: &std::path::Path) {
    let document = std::fs::read_to_string(&path).unwrap();
    let mapping: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&document).unwrap();
    let _ = mapping.get("status");
}
EOF
git -C "$FIXTURE_ROOT" add crates/fno-agents/src/rogue_reader.rs
output=""
actual_exit=0
output="$(bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" 2>&1)" \
    || actual_exit=$?
if [[ "$actual_exit" -eq 0 ]]; then
    echo 'FAIL: a newly tracked Rust status reader passed the authority guard' >&2
    exit 1
fi
if ! grep -q 'rogue_reader.rs' <<<"$output"; then
    echo 'FAIL: the new-reader violation did not name the rogue reader' >&2
    exit 1
fi

git -C "$FIXTURE_ROOT" rm --force --quiet crates/fno-agents/src/rogue_reader.rs
TYPED_READER="$FIXTURE_ROOT/crates/fno-agents/src/typed_rogue_reader.rs"
cat > "$TYPED_READER" <<'EOF'
#[derive(serde::Deserialize)]
struct PlanHeader {
    status: String,
}

fn forbidden(path: &std::path::Path) {
    let document = std::fs::read_to_string(path).unwrap();
    let _: PlanHeader = serde_yaml_ng::from_str(&document).unwrap();
}
EOF
git -C "$FIXTURE_ROOT" add crates/fno-agents/src/typed_rogue_reader.rs
output=""
actual_exit=0
output="$(bash "$FIXTURE_ROOT/scripts/ci/check-plan-rung-authority.sh" 2>&1)" \
    || actual_exit=$?
if [[ "$actual_exit" -eq 0 ]]; then
    echo 'FAIL: a typed Rust status reader passed the authority guard' >&2
    exit 1
fi
if ! grep -q 'typed_rogue_reader.rs' <<<"$output"; then
    echo 'FAIL: the typed-reader violation did not name the rogue reader' >&2
    exit 1
fi

echo "PASS: Rust plan readers cannot introduce frontmatter status parsing"
