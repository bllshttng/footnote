#!/usr/bin/env bash
# test_infer_has_ui.sh - has_ui inference from a changeset (Bug 1).
#
# "Done when": a fixture changeset touching components/**/*.tsx resolves
# has_ui:true; a backend-only changeset resolves has_ui:false.

set -uo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
H="$REPO_ROOT/scripts/lib/infer-has-ui.sh"

PASS=0; FAIL=0
ck() { local l="$1" exp="$2" act="$3"
    if [[ "$exp" == "$act" ]]; then echo "  PASS: $l"; PASS=$((PASS+1))
    else echo "  FAIL: $l (exp=$exp act=$act)"; FAIL=$((FAIL+1)); fi; }

ck "components/**/*.tsx changeset -> true" true \
    "$(printf '%s\n' 'src/components/Foo.tsx' 'src/components/Foo.test.ts' | bash "$H")"
ck "backend-only changeset -> false" false \
    "$(printf '%s\n' 'cli/src/loop.py' 'docs/readme.md' | bash "$H")"
ck "empty changeset -> false" false "$(printf '' | bash "$H")"
ck "mixed incl app/page.tsx -> true" true \
    "$(printf '%s\n' 'app/models/user.py' 'app/page.tsx' | bash "$H")"
ck "styles-only changeset -> true" true \
    "$(printf '%s\n' 'src/styles/theme.css' | bash "$H")"

echo ""
echo "test_infer_has_ui: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]]
