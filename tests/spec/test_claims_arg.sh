#!/usr/bin/env bash
# test_claims_arg.sh - parser contract for /spec ab-id input.
#
# Acceptance criteria covered (from plan 2026-05-05-spec-claims-existing-idea):
#   AC1.2-HP    parse-claims-arg.sh recognises ab-XXXXXXXX and resolves seed.
#   AC1.2-FR    parse-claims-arg.sh emits empty CLAIMS_ID for non-ab-id input.
#   AC1.2-EDGE  Unknown ab-id exits non-zero.
#
# The former AC2-DOC/AC3-DOC sections grepped SKILL.md and the index/focused
# templates for doc strings. The templates were deleted (single-doc is the
# only authored plan shape) and the doc copies were junk patterns under the
# test-audit authoring gate: exact source greps, not behavior.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PARSER="$REPO_ROOT/scripts/lib/parse-claims-arg.sh"

PASS=0
FAIL=0

assert() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        echo "  PASS: $label"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: $label (expected '$expected', got '$actual')"
        FAIL=$(( FAIL + 1 ))
    fi
}

assert_contains() {
    local label="$1" needle="$2" haystack="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        echo "  PASS: $label"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: $label (substring '$needle' not found)"
        FAIL=$(( FAIL + 1 ))
    fi
}

echo "Pre-flight: required artifacts exist"
[[ -f "$PARSER"      ]] && { echo "  PASS: parser exists";          PASS=$((PASS+1)); } || { echo "  FAIL: $PARSER missing";          FAIL=$((FAIL+1)); }
[[ -x "$PARSER"      ]] && { echo "  PASS: parser executable";      PASS=$((PASS+1)); } || { echo "  FAIL: $PARSER not executable";   FAIL=$((FAIL+1)); }

if [[ $FAIL -gt 0 ]]; then
    echo ""
    echo "==="
    echo "test_claims_arg: ${PASS} passed, ${FAIL} failed (artifacts missing - cannot continue)"
    exit 1
fi

# --- AC1.2-FR: non-ab-id input emits empty CLAIMS_ID ---
echo ""
echo "AC1.2-FR: non-ab-id input emits empty CLAIMS_ID"

OUT="$(bash "$PARSER" "")"
assert "empty arg" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "build a feature")"
assert "raw description" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "/path/to/design.md")"
assert "design-doc path" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "ab-123")"  # 3 hex, below the 4-hex floor
assert "ab- prefix too short" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "ab-123456789")"  # 9 hex, above the 8-hex ceiling
assert "ab- prefix too long" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "ab-NOTHEX12")"
assert "ab- prefix but non-hex" 'CLAIMS_ID=""' "$OUT"

OUT="$(bash "$PARSER" "AB-deadbeef")"
assert "uppercase prefix rejected" 'CLAIMS_ID=""' "$OUT"

# --- AC1.2-HP: ab-id input resolves to seed via fno backlog get ---
echo ""
echo "AC1.2-HP: ab-id input resolves to CLAIMS_ID and CLAIMS_SEED_ARG"

# Build a fixture graph in a temp HOME so the test runs in CI without
# depending on the user's live ~/.fno/graph.json. The parser shells
# out to `fno backlog get`, which respects HOME for graph location.
FIXTURE_HOME="$(mktemp -d -t parse-claims-fixture.XXXXXX)"
trap 'rm -rf "$FIXTURE_HOME"' EXIT
mkdir -p "$FIXTURE_HOME/.fno"
cat <<'JSON' | uv run --project "$REPO_ROOT/cli" python "$REPO_ROOT/cli/tests/fixtures/graph_seed.py" "$FIXTURE_HOME/.fno/graph.json"
{
  "entries": [
    {
      "id": "ab-feedface",
      "parent": null,
      "title": "Fixture idea node",
      "type": "feature",
      "project": "fno",
      "cwd": "/tmp/fixture",
      "priority": "p2",
      "domain": "code",
      "blocked_by": [],
      "session_id": null,
      "claimed_at": null,
      "completed_at": null,
      "has_brief": false,
      "compacted": false,
      "roadmap_id": null,
      "vision_path": null,
      "details": "fixture details body",
      "size": null,
      "batch": null,
      "cost_usd": null,
      "cost_sessions": [],
      "plan_path": null,
      "pr_number": null,
      "pr_url": null,
      "merge_status": null,
      "artifact_url": null,
      "completion_note": null,
      "status": "idea",
      "created_at": "2026-01-01T00:00:00+00:00"
    }
  ]
}
JSON

# `fno backlog get` is the live binary - it MAY or MAY NOT use the same
# venv as the source tree. If it's installed and points HOME-aware, this
# test exercises resolution against the sandbox store.
if HOME="$FIXTURE_HOME" fno backlog get ab-feedface >/dev/null 2>&1; then
    OUT="$(HOME="$FIXTURE_HOME" bash "$PARSER" "ab-feedface" 2>&1 || true)"
    assert_contains "CLAIMS_ID set (fixture)" "CLAIMS_ID=ab-feedface" "$OUT"
    assert_contains "CLAIMS_SEED_ARG set (fixture)" "CLAIMS_SEED_ARG=" "$OUT"
else
    echo "  SKIP: fno binary cannot read the sandbox graph.db"
fi

# --- AC1.2-EDGE: unknown ab-id exits non-zero with a graceful, eval-able error ---
echo ""
echo "AC1.2-EDGE: unknown ab-id exits non-zero, error printed for the caller"

# Use an ab-id whose hex is highly unlikely to exist on any user's graph.
# Run with `set +e` because we expect a non-zero return. The parser used to
# die silently under set -e before its graceful error path ran (swallowed
# error text); assert BOTH the rc and the eval-able error output.
set +e
EDGE_OUT="$(bash "$PARSER" "ab-deaddead" 2>/dev/null)"
RC=$?
set -e
EDGE_OK=1
if [[ $RC -eq 0 ]]; then
    echo "  FAIL: unknown ab-id should return non-zero, got rc=0"
    FAIL=$((FAIL+1))
    EDGE_OK=0
fi
if [[ "$EDGE_OUT" != *"not found"* && "$EDGE_OUT" != *"Error resolving"* ]]; then
    echo "  FAIL: unknown ab-id should print an eval-able error, got: $EDGE_OUT"
    FAIL=$((FAIL+1))
    EDGE_OK=0
fi
if [[ $EDGE_OK -eq 1 ]]; then
    echo "  PASS: unknown ab-id returns non-zero (rc=$RC) with an eval-able error"
    PASS=$((PASS+1))
fi

echo ""
echo "==="
echo "test_claims_arg: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
