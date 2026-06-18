#!/usr/bin/env bash
# test_frontend_surface.sh - locked surface matcher via fno.executor._surface.
#
# The locked surface-inference list now lives in the in-package module
# fno.executor._surface (the SINGLE source of truth, ported from the retired
# scripts/lib/infer-task-executor.sh). This test exercises that module's CLI
# through the same locked-pattern fixtures the old sourceable functions
# covered, so has_ui inference and executor routing keep ONE copy of the
# patterns. Byte-for-byte parity with the pre-delete bash is proven by
# cli/tests/unit/test_executor_parity_vs_bash.py.

set -uo pipefail
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PKG_SRC="$REPO_ROOT/cli/src"
if [[ -f "$PKG_SRC/fno/executor/_surface.py" ]]; then
    export PYTHONPATH="${PKG_SRC}${PYTHONPATH:+:${PYTHONPATH}}"
fi

PASS=0; FAIL=0
ck() { local l="$1" exp="$2" act="$3"
    if [[ "$exp" == "$act" ]]; then echo "  PASS: $l"; PASS=$((PASS+1))
    else echo "  FAIL: $l (exp=$exp act=$act)"; FAIL=$((FAIL+1)); fi; }
# m: single path -> impeccable|do.  a: stdin list -> impeccable|do.
m() { printf '%s\n' "$1" | python3 -m fno.executor._surface; }
a() { python3 -m fno.executor._surface; }

echo "is_frontend_surface_path (locked list)"
ck "tsx -> frontend"                      impeccable "$(m 'src/components/Foo.tsx')"
ck "jsx -> frontend"                      impeccable "$(m 'src/Widget.jsx')"
ck "app/page.tsx -> frontend (tsx arm)"   impeccable "$(m 'app/page.tsx')"
ck "components/*.ts -> frontend"          impeccable "$(m 'src/components/Bar.ts')"
ck "routes/ -> frontend"                  impeccable "$(m 'routes/api.ts')"
ck "nested routes/ -> frontend"           impeccable "$(m 'pkg/routes/x.go')"
ck "src/styles -> frontend"               impeccable "$(m 'src/styles/main.css')"
ck "app/main.py -> NOT frontend"          do  "$(m 'app/main.py')"
ck "py -> NOT frontend"                   do  "$(m 'cli/src/loop.py')"
ck "md -> NOT frontend"                   do  "$(m 'docs/readme.md')"

echo ""
echo "any_frontend_surface (stdin)"
ck "mixed list w/ tsx -> match"  impeccable "$(printf '%s\n' a.py b.py c.tsx | a)"
ck "backend-only list -> none"   do  "$(printf '%s\n' a.py b.go | a)"
ck "empty stdin -> none"         do  "$(printf '' | a)"
ck "no trailing newline -> match" impeccable "$(printf '%s' x.tsx | a)"

echo ""
echo "test_frontend_surface: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]]
