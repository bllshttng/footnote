#!/usr/bin/env bash
# Tests for the prompt contract in
# skills/autocorrect/references/autocorrect-prompts.md: the extracted body
# between the PROMPT markers must name SKILL.md as a patchable target,
# require the evidence rows beside each proposed skill diff, and instruct
# grouping by failure shape across sources. The existing warrant rules stay.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROMPTS="$SCRIPT_DIR/../../skills/autocorrect/references/autocorrect-prompts.md"
PASS=0
FAIL=0

pass() {
    echo "  PASS: $1"
    PASS=$((PASS + 1))
}
fail() {
    echo "  FAIL: $1"
    FAIL=$((FAIL + 1))
}
summary() {
    echo ""
    echo "PASS=$PASS FAIL=$FAIL"
    [[ $FAIL -eq 0 ]]
}

# Extract the body between the markers, the way autocorrect-review.sh does.
BODY=$(sed -n '/<!-- PROMPT_START -->/,/<!-- PROMPT_END -->/p' "$PROMPTS")
if [[ -z "$BODY" ]]; then
    echo "FAIL: no PROMPT_START/PROMPT_END body found in $PROMPTS"
    exit 1
fi

# ---- T01: SKILL.md is a legal patch target ----
echo "T01: SKILL.md named as a patchable target"
if printf '%s\n' "$BODY" | grep -q 'SKILL.md'; then
    pass "SKILL.md appears in the prompt body"
else
    fail "SKILL.md never named"
fi

# ---- T02: skill diffs must quote their evidence rows ----
echo "T02: evidence rows required beside skill diffs"
if printf '%s\n' "$BODY" | grep -q 'MUST quote the evidence rows'; then
    pass "evidence-row warrant required"
else
    fail "evidence-row warrant missing"
fi

# ---- T03: grouping is by failure shape, across sources ----
echo "T03: grouping by failure shape across sources"
if printf '%s\n' "$BODY" | grep -q 'SHAPE of the failure'; then
    pass "shape grouping present"
else
    fail "shape grouping missing"
fi
if printf '%s\n' "$BODY" | grep -q 'DIFFERENT sources'; then
    pass "cross-source shape named as the finding"
else
    fail "cross-source clause missing"
fi

# ---- T04: the original warrant rule survives ----
echo "T04: existing warrant rules intact"
if printf '%s\n' "$BODY" | grep -q 'not provided in `implicated_rules`'; then
    pass "no-patch-without-full-text rule intact"
else
    fail "no-patch-without-full-text rule lost"
fi
if printf '%s\n' "$BODY" | grep -q 'without an event you have no warrant'; then
    pass "no-event-no-warrant rule intact"
else
    fail "no-event-no-warrant rule lost"
fi

summary
exit $?
