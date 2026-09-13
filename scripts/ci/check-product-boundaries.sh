#!/usr/bin/env bash
# Product boundary gate: the inventory doc's claims, checked against the
# manifests and the source. Fails when the dev-only compile edge becomes a
# real library edge, when the availability classifier or the PATH walk gains
# a second definition, or when the recorded duplicate-decision debt entries
# disappear from the doc.
set -uo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$REPO_ROOT" || exit 1

fail() {
    echo "FAIL: $*"
    exit 1
}

command -v jq >/dev/null 2>&1 || fail "jq is required (brew install jq)"
command -v rg >/dev/null 2>&1 || fail "rg is required"

# ── 1. the compile edge is dev-only, both directions ────────────────────────
AGENTS_JSON=$(cargo metadata --no-deps --offline --format-version 1 \
    --manifest-path crates/fno-agents/Cargo.toml 2>/dev/null) ||
    fail "cargo metadata failed for fno-agents"
FNO_JSON=$(cargo metadata --no-deps --offline --format-version 1 \
    --manifest-path crates/fno/Cargo.toml 2>/dev/null) ||
    fail "cargo metadata failed for fno"

echo "$AGENTS_JSON" | jq -e '.packages | length > 0' >/dev/null ||
    fail "positive control: parsed no packages from fno-agents metadata"

# Positive control first: a parse that finds NO edge is a broken probe, not a
# passing boundary, so the dev edge must be PRESENT before its kind is checked.
echo "$AGENTS_JSON" | jq -e '
    [.packages[] | select(.name == "fno-agents")
        | .dependencies[] | select(.name == "fno") | .kind] | length > 0' >/dev/null ||
    fail "positive control: the fno edge vanished from fno-agents metadata"

echo "$AGENTS_JSON" | jq -e '
    [.packages[] | select(.name == "fno-agents")
        | .dependencies[] | select(.name == "fno") | .kind]
    | all(.[]; . == "dev")' >/dev/null ||
    fail "fno-agents links fno outside dev-dependencies"

echo "$FNO_JSON" | jq -e '.packages | length > 0' >/dev/null ||
    fail "positive control: parsed no packages from fno metadata"
echo "$FNO_JSON" | jq -e '
    [.packages[] | select(.name == "fno")
        | .dependencies[] | select(.name == "fno-agents")] | length == 0' >/dev/null ||
    fail "the mux crate links fno-agents"

# ── 2. the classifier and its doc exist ─────────────────────────────────────
[ -f crates/fno/src/product_boundary.rs ] ||
    fail "crates/fno/src/product_boundary.rs is missing"
[ -f docs/architecture/product-boundaries.md ] ||
    fail "docs/architecture/product-boundaries.md is missing"

# ── 3. one availability classifier ──────────────────────────────────────────
CLASSIFIER_FILES=$(rg -l "not found \(set FNO_AGENTS_WORKER" \
    crates/fno/src crates/fno-agents/src | wc -l | tr -d ' ')
[ "$CLASSIFIER_FILES" = "1" ] ||
    fail "the worker-missing refusal is defined in $CLASSIFIER_FILES files, expected 1 (product_boundary.rs)"

# ── 4. one PATH walk for paired binaries ────────────────────────────────────
WALK_FILES=$(rg -l "fn find_on_path" crates/fno/src | wc -l | tr -d ' ')
[ "$WALK_FILES" = "1" ] ||
    fail "find_on_path is defined in $WALK_FILES files, expected 1 (product_boundary.rs)"

# ── 5. the recorded debt stays recorded ─────────────────────────────────────
for token in canonical_root_with canonical_repo_root resolve_canonical_repo_root paired_bin; do
    grep -q "$token" docs/architecture/product-boundaries.md ||
        fail "the debt inventory no longer records $token"
done

echo "product-boundaries: ok (dev-only edge, one classifier, one PATH walk, debt recorded)"
