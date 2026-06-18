#!/usr/bin/env bash
# test_executor_prompt.sh - Surface-detection rules for /think executor routing.
#
# Acceptance criteria covered (from plan 2026-05-04-think-spec-executor-routing-prompts):
#   AC1.1-HP   Frontend signals trigger the prompt (frontend-touching detection)
#   AC1.2-FR   Backend-only sessions don't prompt (backend-only detection)
#   AC1.3-EDGE Mixed-surface sessions get the mixed option (mixed detection)
#   AC1.4-FR   Target autonomous never blocks on the prompt (mode detection)
#   AC1.5-FR   CLI flag short-circuits detection (env override)
#
# The detection helper is `skills/think/references/detect-surface.sh`; it reads
# design-doc text on stdin and emits one of:
#   frontend-touching | backend-only | mixed | unknown
#
# Detection is anchored in structural keyword and filename checks (not LLM
# prose interpretation) so the rules are reproducible across runs - per the
# plan's Domain Pitfall #2.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HELPER="$REPO_ROOT/skills/think/references/detect-surface.sh"
RULES_DOC="$REPO_ROOT/skills/think/references/executor-routing-prompt.md"
THINK_SKILL="$REPO_ROOT/skills/think/SKILL.md"

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

detect() {
    printf '%s\n' "$1" | bash "$HELPER"
}

echo "Pre-flight: required artifacts exist"
[[ -f "$HELPER" ]]    && { echo "  PASS: detect-surface.sh exists";    PASS=$((PASS+1)); } || { echo "  FAIL: $HELPER missing"; FAIL=$((FAIL+1)); }
[[ -x "$HELPER" ]]    && { echo "  PASS: detect-surface.sh executable"; PASS=$((PASS+1)); } || { echo "  FAIL: $HELPER not executable"; FAIL=$((FAIL+1)); }
[[ -f "$RULES_DOC" ]] && { echo "  PASS: executor-routing-prompt.md exists"; PASS=$((PASS+1)); } || { echo "  FAIL: $RULES_DOC missing"; FAIL=$((FAIL+1)); }

if [[ $FAIL -gt 0 ]]; then
    echo ""
    echo "==="
    echo "test_executor_prompt: ${PASS} passed, ${FAIL} failed (artifacts missing - cannot continue)"
    exit 1
fi

echo ""
echo "AC1.1-HP: frontend signals -> frontend-touching"
assert "UI noun in story"          "frontend-touching" "$(detect 'User wants a settings page with theme toggle.')"
assert "component noun"            "frontend-touching" "$(detect 'Build a Button component for the design system.')"
assert "framework name"            "frontend-touching" "$(detect 'Use React for the dashboard view.')"
assert "Next.js framework"         "frontend-touching" "$(detect 'Next.js app router will host the editor.')"
assert "tsx file path"             "frontend-touching" "$(detect 'Files: src/components/Login.tsx')"
assert "components/ path"          "frontend-touching" "$(detect 'Touches packages/ui/components/Card.ts')"
assert "src/styles path"           "frontend-touching" "$(detect 'Files: src/styles/themes.css')"

echo ""
echo "AC1.2-FR: backend-only signals -> backend-only"
assert "API noun"                  "backend-only" "$(detect 'Add a queue worker that consumes batch ETL events.')"
assert "schema/migration"          "backend-only" "$(detect 'Run a migration to alter the schema.')"
assert "ingest pipeline"           "backend-only" "$(detect 'Daily ingest job pushes events to the worker queue.')"

echo ""
echo "AC1.3-EDGE: both surfaces -> mixed"
assert "API + page"                "mixed" "$(detect 'Add an API endpoint and a settings page wired to it.')"
assert "schema + component"        "mixed" "$(detect 'Migration adds a column; new Banner component renders the value.')"

echo ""
echo "Edge: nothing matches -> unknown"
assert "neutral prose"             "unknown" "$(detect 'A generic refactor of the configuration loader.')"
assert "empty input"               "unknown" "$(printf '' | bash "$HELPER")"

echo ""
echo "Case-insensitive matching"
assert "uppercase UI"              "frontend-touching" "$(detect 'New SCREEN for users.')"
assert "lowercase react"           "frontend-touching" "$(detect 'react app starts here.')"
assert "uppercase API"             "backend-only" "$(detect 'Update the API and the worker.')"

echo ""
echo "Word-boundary discipline"
# "form" should match because 'form' is a UI noun, but 'inform' (substring)
# must not match. Same family of false-positives killed earlier surface
# detectors when they used naked substring greps.
assert "noun in word: form button" "frontend-touching" "$(detect 'Add a form button to the page.')"
assert "no false-positive: inform" "unknown"           "$(detect 'We must inform users that uniform performance is required.')"
assert "no false-positive: queue inside conquered" "unknown" "$(detect 'A reorganization conquered scaling concerns.')"

echo ""
echo "AC1.4-FR / AC1.5-FR: SKILL.md references the env contract"
# These are lightweight structural assertions: the SKILL.md must mention the
# detection step, the prompt template location, and the env var contract for
# CLI-flag plumbing. They guard against the SKILL silently dropping the new
# step in a future edit without also updating the test.
grep -q 'detect-surface.sh' "$THINK_SKILL" \
    && { echo "  PASS: SKILL.md references detect-surface.sh"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: SKILL.md does not reference detect-surface.sh"; FAIL=$((FAIL+1)); }
grep -q 'executor-routing-prompt' "$THINK_SKILL" \
    && { echo "  PASS: SKILL.md links to prompt reference"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: SKILL.md does not link to prompt reference"; FAIL=$((FAIL+1)); }
grep -q 'FNO_EXECUTOR_OVERRIDE\|FNO_EXECUTOR' "$THINK_SKILL" \
    && { echo "  PASS: SKILL.md documents env-var contract"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: SKILL.md does not document env-var contract"; FAIL=$((FAIL+1)); }
grep -qi 'auto-detected\|auto_detected' "$RULES_DOC" \
    && { echo "  PASS: rules doc documents auto-detected provenance"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: rules doc missing auto-detected provenance"; FAIL=$((FAIL+1)); }
grep -qi 'cli-flag' "$RULES_DOC" \
    && { echo "  PASS: rules doc documents cli-flag provenance"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: rules doc missing cli-flag provenance"; FAIL=$((FAIL+1)); }
grep -qi 'user-confirmed' "$RULES_DOC" \
    && { echo "  PASS: rules doc documents user-confirmed provenance"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: rules doc missing user-confirmed provenance"; FAIL=$((FAIL+1)); }

echo ""
echo "==="
echo "test_executor_prompt: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
