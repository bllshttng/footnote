#!/usr/bin/env bash
# test_executor_resolution.sh - three-tier resolver chain.
#
# Acceptance criteria covered:
#   AC1.1-HP   explicit task executor wins over plan-level
#   AC1.1-FR   plan-level executor wins over surface inference
#   AC1.1-EDGE surface inference fires only when nothing explicit
#   AC1.5-FR   unknown executor falls closed to 'do' (with WARN)
#   tdd alias normalizes to 'do'
#
# Pure unit tests against the resolve-executor.sh shim. No /impeccable
# stub needed (the resolver does not invoke /impeccable).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RESOLVER="$REPO_ROOT/skills/do/scripts/resolve-executor.sh"

PASS=0
FAIL=0

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        echo "  PASS: $label"
        PASS=$(( PASS + 1 ))
    else
        echo "  FAIL: $label"
        echo "    expected: '$expected'"
        echo "    actual:   '$actual'"
        FAIL=$(( FAIL + 1 ))
    fi
}

resolve() {
    # Run the resolver and capture stdout (the resolved name).
    bash "$RESOLVER" 2>/dev/null
}

echo "AC1.1-HP: explicit task executor wins over plan-level"
result=$(PLAN_EXEC="do" TASK_EXEC="impeccable" resolve)
assert_eq "task=impeccable, plan=do -> impeccable" "impeccable" "$result"

echo ""
echo "AC1.1-FR: plan-level executor wins over surface inference"
result=$(PLAN_EXEC="impeccable" TASK_EXEC="" TASK_FILES="src/foo.py" resolve)
assert_eq "plan=impeccable, files=*.py -> impeccable" "impeccable" "$result"

echo ""
echo "AC1.1-EDGE: surface inference fires when nothing explicit"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="src/components/Foo.tsx" resolve)
assert_eq "files=tsx -> impeccable (inferred)" "impeccable" "$result"

result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="cli/src/loop.py" resolve)
assert_eq "files=py -> do (inferred)" "do" "$result"

echo ""
echo "AC1.5-FR: unknown executor falls closed to 'do'"
# Capture stderr too so we verify the WARN fires.
combined=$(PLAN_EXEC="" TASK_EXEC="nonsense" bash "$RESOLVER" 2>&1)
result=$(printf '%s\n' "$combined" | grep -v '^resolve-executor:' | head -1)
assert_eq "unknown name -> do" "do" "$result"
if printf '%s\n' "$combined" | grep -q "WARN.*unknown executor"; then
    echo "  PASS: WARN logged for unknown executor"
    PASS=$(( PASS + 1 ))
else
    echo "  FAIL: WARN missing for unknown executor"
    FAIL=$(( FAIL + 1 ))
fi

echo ""
echo "tdd alias normalizes to 'do'"
result=$(PLAN_EXEC="" TASK_EXEC="tdd" resolve)
assert_eq "tdd -> do" "do" "$result"

echo ""
echo "Default: empty everything"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="" resolve)
assert_eq "all empty -> do" "do" "$result"

echo ""
echo "AUTO_ROUTE_FRONTEND=false disables inference"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="src/components/Foo.tsx" \
         AUTO_ROUTE_FRONTEND="false" resolve)
assert_eq "inference off -> do despite frontend file" "do" "$result"

echo ""
echo "==="
echo "test_executor_resolution: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
