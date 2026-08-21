#!/usr/bin/env bash
# test_executor_resolution.sh - three-tier resolver chain.
#
# Acceptance criteria covered:
#   AC1.1-HP   explicit task executor wins over plan-level
#   AC1.1-FR   plan-level executor wins over surface inference
#   AC1.1-EDGE surface inference fires only when nothing explicit
#   AC1.5-FR   unknown executor falls closed to 'tdd' (with WARN)
#   do alias normalizes to 'tdd'
#
# Pure unit tests against the resolve-executor.sh shim. No /impeccable
# stub needed (the resolver does not invoke /impeccable).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RESOLVER="$REPO_ROOT/skills/execute/scripts/resolve-executor.sh"

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
result=$(PLAN_EXEC="tdd" TASK_EXEC="impeccable" resolve)
assert_eq "task=impeccable, plan=tdd -> impeccable" "impeccable" "$result"

echo ""
echo "AC1.1-FR: plan-level executor wins over surface inference"
result=$(PLAN_EXEC="impeccable" TASK_EXEC="" TASK_FILES="src/foo.py" resolve)
assert_eq "plan=impeccable, files=*.py -> impeccable" "impeccable" "$result"

echo ""
echo "AC1.1-EDGE: surface inference fires when nothing explicit"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="src/components/Foo.tsx" resolve)
assert_eq "files=tsx -> impeccable (inferred)" "impeccable" "$result"

result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="cli/src/loop.py" resolve)
assert_eq "files=py -> tdd (inferred)" "tdd" "$result"

echo ""
echo "AC1.5-FR: unknown executor falls closed to 'tdd'"
# Capture stderr too so we verify the WARN fires.
combined=$(PLAN_EXEC="" TASK_EXEC="nonsense" bash "$RESOLVER" 2>&1)
result=$(printf '%s\n' "$combined" | grep -v '^resolve-executor:' | head -1)
assert_eq "unknown name -> tdd" "tdd" "$result"
if printf '%s\n' "$combined" | grep -q "WARN.*unknown executor"; then
    echo "  PASS: WARN logged for unknown executor"
    PASS=$(( PASS + 1 ))
else
    echo "  FAIL: WARN missing for unknown executor"
    FAIL=$(( FAIL + 1 ))
fi

echo ""
echo "do alias normalizes to 'tdd'"
result=$(PLAN_EXEC="" TASK_EXEC="do" resolve)
assert_eq "do -> tdd" "tdd" "$result"

echo ""
echo "Default: empty everything"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="" resolve)
assert_eq "all empty -> tdd" "tdd" "$result"

echo ""
echo "AUTO_ROUTE_FRONTEND=false disables inference"
result=$(PLAN_EXEC="" TASK_EXEC="" TASK_FILES="src/components/Foo.tsx" \
         AUTO_ROUTE_FRONTEND="false" resolve)
assert_eq "inference off -> tdd despite frontend file" "tdd" "$result"

echo ""
echo "==="
echo "test_executor_resolution: ${PASS} passed, ${FAIL} failed"
[[ $FAIL -eq 0 ]] || exit 1
