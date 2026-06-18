#!/usr/bin/env bash
# test_init_has_ui_from_plan.sh - lock the no-drift invariant for
# init-target-state.sh::_derive_has_ui_from_plan (ab-15c470cf).
#
# The function must classify plan-referenced paths via the canonical
# frontend-surface matcher (scripts/lib/infer-has-ui.sh ->
# fno.executor._surface::is_frontend_surface_path), NOT a private regex.
# The discriminating case is `.vue`: the old inline regex matched it, the
# canonical locked globs do NOT. If `.vue` ever derives `true` again, a
# private regex has crept back in.
#
# Run: bash tests/hooks/test_init_has_ui_from_plan.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INIT="$REPO_ROOT/hooks/helpers/init-target-state.sh"

PASS=0
FAIL=0
pass() { echo "  PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "  FAIL: $1 (expected=$2 got=$3)"; FAIL=$((FAIL + 1)); }

# Extract just the function so we don't run init-target-state.sh's top-level
# body (git checks, set -e, state writes). CLAUDE_PLUGIN_ROOT pins the lib path.
HARNESS="$(mktemp -t init-has-ui.XXXXXX.sh)"
trap 'rm -f "$HARNESS"' EXIT
{
    echo 'set -euo pipefail'
    echo "export CLAUDE_PLUGIN_ROOT='$REPO_ROOT'"
    sed -n '/^_derive_has_ui_from_plan() {/,/^}/p' "$INIT"
} > "$HARNESS"

derive() { # plan_content -> echoes true|false
    local content="$1" d got
    d="$(mktemp -d)"
    trap 'rm -rf "$d"' RETURN  # function-scoped cleanup even on early return
    printf '%s\n' "$content" > "$d/plan.md"
    got="$(bash -c "source '$HARNESS'; _derive_has_ui_from_plan '$d/plan.md'")"
    printf '%s' "$got"
}

check() { # label content expected
    local got; got="$(derive "$2")"
    [[ "$got" == "$3" ]] && pass "$1" || fail "$1" "$3" "$got"
}

echo "=== init _derive_has_ui_from_plan routes through the canonical matcher ==="
check "tsx component path -> true"      'Build src/components/Dashboard.tsx'   true
check "routes/ + tsx -> true"           'Add app/routes/settings.tsx'          true
check "src/styles path -> true"         'Restyle src/styles/theme.css'         true
check "python-only plan -> false"       'Refactor src/etl/parser.py'           false
check "no paths -> false"               'Prose with no file paths here.'       false
# Discriminator: the canonical globs do NOT include .vue/.svelte. A `true` here
# means a private regex has been reintroduced.
check "vue NOT a surface (canonical) -> false"    'Build src/Widget.vue'       false
check "svelte NOT a surface (canonical) -> false" 'Build src/Widget.svelte'    false

echo ""
echo "Results: $PASS pass / $FAIL fail"
[[ "$FAIL" -eq 0 ]] || exit 1
exit 0
