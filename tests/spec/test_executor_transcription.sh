#!/usr/bin/env bash
# test_executor_transcription.sh - parser + transcription contract for /spec.
#
# Acceptance criteria covered (from plan 2026-05-04-think-spec-executor-routing-prompts):
#   AC2.1-HP    /spec transcribes plan-level executor
#   AC2.2-FR    /spec leaves frontmatter empty when no decision recorded
#   AC2.3-EDGE  Mixed surfaces produce per-task examples (parser emits 'mixed')
#   AC2.4-FR    /spec tolerates Locked Decisions formatting variation
#
# The parser is the in-package module fno.executor._locked - it reads a
# design-doc on stdin and emits one of: '' | 'tdd' | 'impeccable' | 'mixed'.
# /spec's SKILL.md calls into it and transcribes the result into plan
# frontmatter.
#
# The former template/DOC grep tail (quick-template placeholder, LOCKED_SECTION
# variable, parser-reference and transcription-verb strings) was junk under
# the test-audit authoring gate: exact source greps of doc prose, not behavior.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG_SRC="$REPO_ROOT/cli/src"
if [[ -f "$PKG_SRC/fno/executor/_locked.py" ]]; then
    export PYTHONPATH="${PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

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

parse() {
    printf '%s\n' "$1" | python3 -m fno.executor._locked 2>/dev/null
}

echo "Pre-flight: required artifacts exist"
python3 -c 'import fno.executor._locked' 2>/dev/null \
    && { echo "  PASS: fno.executor._locked importable"; PASS=$((PASS+1)); } \
    || { echo "  FAIL: fno.executor._locked not importable"; FAIL=$((FAIL+1)); }

if [[ $FAIL -gt 0 ]]; then
    echo ""
    echo "==="
    echo "test_executor_transcription: ${PASS} passed, ${FAIL} failed (artifacts missing - cannot continue)"
    exit 1
fi

echo ""
echo "AC2.1-HP: canonical Locked Decision -> plan-level value"
DOC1='## Locked Decisions (DO NOT revisit)

1. **State machine**: explicit per-screen.
2. **Executor routing**: plan-level `executor: impeccable` (auto-detected).
   Rationale: frontend-only feature.
'
assert "canonical impeccable" "impeccable" "$(parse "$DOC1")"

DOC2='## Locked Decisions

1. **Executor routing**: plan-level `executor: do` (cli-flag).
'
assert "do compatibility alias" "tdd" "$(parse "$DOC2")"

DOC3='## Locked Decisions

1. **Executor routing**: plan-level `executor: mixed` with per-task overrides.
'
assert "canonical mixed" "mixed" "$(parse "$DOC3")"

echo ""
echo "AC2.2-FR: no entry -> empty"
DOC4='## Locked Decisions

1. **Auth model**: cookie-based.
2. **State machine**: redux.
'
assert "no executor entry" "" "$(parse "$DOC4")"

DOC5='# A design doc with no Locked Decisions section at all.

Just prose.
'
assert "no Locked Decisions heading" "" "$(parse "$DOC5")"

assert "empty input" "" "$(parse '')"

echo ""
echo "AC2.4-FR: formatting variation tolerated"
# bold-suffixed colon
DOC6='## Locked Decisions

1. **Executor Routing:** plan-level `executor: impeccable`
'
assert "bold-suffixed colon" "impeccable" "$(parse "$DOC6")"

# no bold at all
DOC7='## Locked Decisions

1. Executor routing: plan-level `executor: do`
'
assert "no bold do alias" "tdd" "$(parse "$DOC7")"

# extra whitespace
DOC8='## Locked Decisions

1. **Executor routing**:    plan-level    `executor:   impeccable`   (auto-detected).
'
assert "extra whitespace" "impeccable" "$(parse "$DOC8")"

# tab-indented "blank" line should still flush the buffer between entries
# (Gemini PR #200 review: original `${line// /}` only stripped spaces, so a
# tab-only or whitespace-only line would not register as blank and would
# bleed continuation buffering across entries.)
DOC_TAB=$'## Locked Decisions\n\n1. **Executor routing**: plan-level `executor: do` (auto-detected).\n\t\n2. **Other thing**: foo.\n3. **Executor routing**: plan-level `executor: impeccable` (user-confirmed).\n'
assert "tab-only blank between entries" "impeccable" "$(parse "$DOC_TAB")"

# mixed casing
DOC9='## Locked Decisions

1. **executor ROUTING**: plan-level `Executor: Impeccable`
'
assert "mixed casing" "impeccable" "$(parse "$DOC9")"

# missing provenance
DOC10='## Locked Decisions

1. **Executor routing**: plan-level `executor: mixed`
'
assert "missing provenance suffix" "mixed" "$(parse "$DOC10")"

echo ""
echo "Multiple entries: last wins"
DOC11='## Locked Decisions

1. **Executor routing**: plan-level `executor: do` (auto-detected).
2. **Other thing**: foo.
3. **Executor routing**: plan-level `executor: impeccable` (user-confirmed).
'
assert "last wins on duplicates" "impeccable" "$(parse "$DOC11")"

echo ""
echo "Unknown values rejected"
# Per the plan failure modes: parser rejects values outside tdd|do|impeccable|mixed
DOC12='## Locked Decisions

1. **Executor routing**: plan-level `executor: garbage`.
'
assert "unknown value rejected" "" "$(parse "$DOC12")"

DOC13='## Locked Decisions

1. **Executor routing**: plan-level `executor: archer`.
'
assert "archer not a valid lock" "" "$(parse "$DOC13")"

echo ""
echo "Section scoping"
# An executor mention OUTSIDE the Locked Decisions section must NOT match.
# This guards against a domain pitfall where prose mentions executor but the
# user did not lock it.
DOC14='# Some Doc

The operator dispatches an `executor: impeccable` in some cases.

## Locked Decisions

1. **Auth model**: cookie-based.
'
assert "executor mentioned outside Locked Decisions" "" "$(parse "$DOC14")"

echo ""
echo "==="
echo "test_executor_transcription: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
