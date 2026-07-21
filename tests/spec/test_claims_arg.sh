#!/usr/bin/env bash
# test_claims_arg.sh - parser + SKILL doc contract for /spec ab-id input.
#
# Acceptance criteria covered (from plan 2026-05-05-spec-claims-existing-idea):
#   AC1.2-HP    parse-claims-arg.sh recognises ab-XXXXXXXX and resolves seed.
#   AC1.2-FR    parse-claims-arg.sh emits empty CLAIMS_ID for non-ab-id input.
#   AC1.2-EDGE  Unknown ab-id exits non-zero.
#   AC2-DOC     SKILL.md has a Plan Claims Ingestion section with the
#               regex, the parser invocation, and the post-write refusal.
#   AC3-DOC     Both templates document the `claims:` frontmatter field.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PARSER="$REPO_ROOT/scripts/lib/parse-claims-arg.sh"
SPEC_SKILL="$REPO_ROOT/skills/blueprint/SKILL.md"
INDEX_TPL="$REPO_ROOT/skills/blueprint/references/index-template.md"
FOCUSED_TPL="$REPO_ROOT/skills/blueprint/references/focused-template.md"

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
[[ -f "$SPEC_SKILL"  ]] && { echo "  PASS: SKILL.md exists";        PASS=$((PASS+1)); } || { echo "  FAIL: $SPEC_SKILL missing";      FAIL=$((FAIL+1)); }
[[ -f "$INDEX_TPL"   ]] && { echo "  PASS: index-template exists";  PASS=$((PASS+1)); } || { echo "  FAIL: $INDEX_TPL missing";       FAIL=$((FAIL+1)); }
[[ -f "$FOCUSED_TPL" ]] && { echo "  PASS: focused-template exists"; PASS=$((PASS+1)); } || { echo "  FAIL: $FOCUSED_TPL missing";    FAIL=$((FAIL+1)); }

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

OUT="$(bash "$PARSER" "ab-1234567")"  # 7 chars, not 8
assert "ab- prefix but wrong length" 'CLAIMS_ID=""' "$OUT"

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
cat > "$FIXTURE_HOME/.fno/graph.json" <<'JSON'
{
  "entries": [
    {
      "id": "ab-feedface",
      "parent": null,
      "title": "Fixture idea node",
      "type": "feature",
      "project": "abilities",
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
# test exercises the real resolution. If not, fall back to the live-graph
# probe so the assertion still runs in the dogfood env.
if HOME="$FIXTURE_HOME" fno backlog get ab-feedface >/dev/null 2>&1; then
    OUT="$(HOME="$FIXTURE_HOME" bash "$PARSER" "ab-feedface" 2>&1 || true)"
    assert_contains "CLAIMS_ID set (fixture)" "CLAIMS_ID=ab-feedface" "$OUT"
    assert_contains "CLAIMS_SEED_ARG set (fixture)" "CLAIMS_SEED_ARG=" "$OUT"
elif [[ -f "${HOME}/.fno/graph.json" ]] \
   && fno backlog get ab-0973161b >/dev/null 2>&1; then
    OUT="$(bash "$PARSER" "ab-0973161b" 2>&1 || true)"
    assert_contains "CLAIMS_ID set (live)" "CLAIMS_ID=ab-0973161b" "$OUT"
    assert_contains "CLAIMS_SEED_ARG set (live)" "CLAIMS_SEED_ARG=" "$OUT"
else
    echo "  SKIP: fno binary not on PATH and no live graph.json"
fi

# --- AC1.2-EDGE: unknown ab-id exits non-zero ---
echo ""
echo "AC1.2-EDGE: unknown ab-id exits non-zero"

# Use an ab-id whose hex is highly unlikely to exist on any user's graph.
# Run with `set +e` because we expect a non-zero return.
set +e
bash "$PARSER" "ab-deaddead" >/dev/null 2>&1
RC=$?
set -e
if [[ $RC -ne 0 ]]; then
    echo "  PASS: unknown ab-id returns non-zero (rc=$RC)"
    PASS=$((PASS+1))
else
    echo "  FAIL: unknown ab-id should return non-zero, got rc=0"
    FAIL=$((FAIL+1))
fi

# --- AC2-DOC: SKILL.md has the Plan Claims Ingestion section ---
echo ""
echo "AC2-DOC: SKILL.md has the Plan Claims Ingestion section"

SKILL_TEXT="$(cat "$SPEC_SKILL")"
assert_contains "Plan Claims Ingestion section heading" \
    "## Plan Claims Ingestion (MANDATORY when input is an ab-id)" "$SKILL_TEXT"
assert_contains "ab-id regex" "^ab-[0-9a-f]{8}\$" "$SKILL_TEXT"
assert_contains "parser invocation" "parse-claims-arg.sh" "$SKILL_TEXT"
assert_contains "post-write refusal block" \
    "Refusing to adopt." "$SKILL_TEXT"
assert_contains "claims grep guard" \
    'grep -qE "^claims:[[:space:]]+$CLAIMS_ID' "$SKILL_TEXT"

# --- AC3-DOC: templates document the claims field ---
echo ""
echo "AC3-DOC: both spec templates document the claims: frontmatter field"

INDEX_TEXT="$(cat "$INDEX_TPL")"
FOCUSED_TEXT="$(cat "$FOCUSED_TPL")"
assert_contains "index template documents claims" \
    "claims: ab-XXXXXXXX" "$INDEX_TEXT"
assert_contains "focused template documents claims" \
    "claims: ab-XXXXXXXX" "$FOCUSED_TEXT"

echo ""
echo "==="
echo "test_claims_arg: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
