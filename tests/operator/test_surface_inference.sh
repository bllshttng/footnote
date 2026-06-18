#!/usr/bin/env bash
# test_surface_inference.sh - exhaustive coverage of the locked inference list.
#
# Acceptance criteria covered:
#   AC1.2-HP   inference recognizes every locked pattern
#   AC1.2-EDGE empty stdin defaults to 'do'
#
# Locked patterns (from plan 2026-05-04-operator-impeccable-executor;
# see fno.executor._surface for the canonical list):
#   **/*.tsx, **/*.jsx
#   components/**, **/components/**
#   routes/**, **/routes/**
#   src/styles/**, **/src/styles/**
# `app/**` is intentionally NOT a directory match - app/main.py and other
# backend module roots must route to 'do', not 'impeccable'. App Router
# files (app/page.tsx) still match via the .tsx/.jsx arms.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG_SRC="$REPO_ROOT/cli/src"
if [[ -f "$PKG_SRC/fno/executor/_surface.py" ]]; then
    export PYTHONPATH="${PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

PASS=0
FAIL=0

infer() {
    printf '%s\n' "$1" | python3 -m fno.executor._surface
}

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

echo "AC1.2-HP: locked frontend patterns -> impeccable"
assert "*.tsx in nested dir"       "impeccable" "$(infer 'src/components/Login.tsx')"
assert "*.tsx at top level"        "impeccable" "$(infer 'app/page.tsx')"
assert "*.jsx in nested dir"       "impeccable" "$(infer 'src/components/Card.jsx')"
assert "**/components/** general"  "impeccable" "$(infer 'packages/ui/components/Button.ts')"
assert "**/routes/** general"      "impeccable" "$(infer 'src/routes/api.ts')"
# App Router files still match via the .tsx/.jsx arms even though `app/`
# itself is no longer a directory match (sigma-review caught backend
# misrouting when app/main.py would have matched).
assert "app/*.tsx via extension"   "impeccable" "$(infer 'app/layout.tsx')"
assert "app/**/*.ts inside routes/" "impeccable" "$(infer 'app/routes/api.ts')"
assert "src/styles/** css"         "impeccable" "$(infer 'src/styles/main.css')"
assert "src/styles/** scss"        "impeccable" "$(infer 'src/styles/themes/dark.scss')"

echo ""
echo "Backend / non-matching paths -> do"
assert "python file"               "do" "$(infer 'cli/src/fno/loop.py')"
assert "shell script"              "do" "$(infer 'scripts/lib/common.sh')"
assert "go file in pkg"            "do" "$(infer 'pkg/api/server.go')"
assert "yaml in config"            "do" "$(infer '.fno/settings.yaml')"
# Backend module roots that use `app/` MUST NOT misroute to frontend.
# These caught a real defect in the locked list during sigma-review.
assert "app/main.py (Python root)" "do" "$(infer 'app/main.py')"
assert "app/models/user.py"        "do" "$(infer 'app/models/user.py')"
assert "app/tasks/celery.py"       "do" "$(infer 'app/tasks/celery.py')"

echo ""
echo "Root-level frontend dirs (Gemini review fix)"
assert "components/ at repo root"   "impeccable" "$(infer 'components/Header.ts')"
assert "routes/ at repo root"       "impeccable" "$(infer 'routes/api.ts')"
# Monorepo nested src/styles
assert "*/src/styles/ in monorepo"  "impeccable" "$(infer 'packages/web/src/styles/main.css')"

echo ""
echo "Trailing-newline robustness (Gemini review fix)"
# When stdin lacks a trailing newline, the last line MUST still be processed.
result=$(printf 'src/components/Foo.tsx' | python3 -m fno.executor._surface)
assert "no-trailing-newline frontend" "impeccable" "$result"
result=$(printf 'cli/src/loop.py' | python3 -m fno.executor._surface)
assert "no-trailing-newline backend"  "do"         "$result"

echo ""
echo "AC1.2-EDGE: empty stdin -> do"
result=$(printf '' | python3 -m fno.executor._surface)
assert "empty input"               "do" "$result"

result=$(printf '\n\n' | python3 -m fno.executor._surface)
assert "blank lines only"          "do" "$result"

echo ""
echo "Mixed file lists: any frontend match wins"
result=$(printf 'cli/src/loop.py\nsrc/components/Foo.tsx\n' | python3 -m fno.executor._surface)
assert "py + tsx -> impeccable"    "impeccable" "$result"

result=$(printf 'src/utils/format.ts\ncli/src/loop.py\n' | python3 -m fno.executor._surface)
assert "ts util + py -> do"        "do" "$result"

echo ""
echo "==="
echo "test_surface_inference: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
