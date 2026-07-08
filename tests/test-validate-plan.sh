#!/usr/bin/env bash
# Test suite for validate-plan.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VALIDATE="$SCRIPT_DIR/../scripts/validate-plan.sh"
PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

# Create temp plan files
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

# --- AC1: Structure checks ---
echo "--- AC1: Structure Checks ---"

# Test: missing plan file exits 1
PLAN_MISSING="$TMPDIR_BASE/nope.md"
if bash "$VALIDATE" "$PLAN_MISSING" 2>/dev/null; then
    fail "AC1: Should exit 1 when plan file missing"
else
    pass "AC1: Exits 1 when plan file missing"
fi

# Test: valid minimal plan exits 0
PLAN_VALID="$TMPDIR_BASE/valid.md"
cat > "$PLAN_VALID" <<'EOF'
execution_mode: sequential

### Task 1.1
Files: src/foo.ts
Acceptance Criteria: AC1
Steps:
Step 1: Do something
EOF
if bash "$VALIDATE" "$PLAN_VALID" 2>/dev/null; then
    pass "AC1: Exits 0 for valid plan"
else
    fail "AC1: Should exit 0 for valid plan"
fi

# --- AC2: Task completeness warnings ---
echo ""
echo "--- AC2: Task Completeness ---"

PLAN_WARN="$TMPDIR_BASE/warn.md"
cat > "$PLAN_WARN" <<'EOF'
execution_mode: sequential

### Task 1.1
Just a task with no sections
EOF
# Should still exit 0 (warnings not errors) but show WARN
OUTPUT=$(bash "$VALIDATE" "$PLAN_WARN" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC2: Exits 0 with only warnings (missing AC/Steps)"
else
    fail "AC2: Should exit 0 for warnings-only (got exit $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "WARN"; then
    pass "AC2: Shows WARN for missing Acceptance Criteria/Steps"
else
    fail "AC2: Should show WARN for missing sections"
fi

# --- AC5: Exit codes ---
echo ""
echo "--- AC5: Exit Codes ---"
# Already tested above: exit 1 on errors, exit 0 on warnings
pass "AC5: Exit code behavior verified above"

# --- AC6: Semantic - Critical Path Trace ---
echo ""
echo "--- AC6: Critical Path Trace (Semantic Checks) ---"

# Test: feature scope with complete critical path → PASS
PLAN_FEATURE_OK="$TMPDIR_BASE/feature_ok.md"
cat > "$PLAN_FEATURE_OK" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: User creates item
User clicks "Create" → ✅ CreateForm → 🔨 API POST /items [Task 1.1] → ✅ Database

## Scope Classification

```yaml
scope: feature
```

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Build the API
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_FEATURE_OK" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC6a: Feature scope with resolved path exits 0"
else
    fail "AC6a: Feature scope with resolved path should exit 0 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "No stubs in critical path"; then
    pass "AC6a: Reports no stubs"
else
    fail "AC6a: Should report no stubs"
fi

# Test: feature scope with unresolved stubs → ERROR (exit 1)
PLAN_FEATURE_STUB="$TMPDIR_BASE/feature_stub.md"
cat > "$PLAN_FEATURE_STUB" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: User creates item
User clicks "Create" → ⚠️ STUB PlaceholderService → ❌ NOT BUILT RealEngine

## Scope Classification

```yaml
scope: feature
```

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Something
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_FEATURE_STUB" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]]; then
    pass "AC6b: Feature scope with unresolved stubs exits 1"
else
    fail "AC6b: Feature scope with unresolved stubs should exit 1 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "ERROR.*unresolved stub"; then
    pass "AC6b: Reports ERROR for unresolved stubs"
else
    fail "AC6b: Should report ERROR for unresolved stubs"
fi

# Test: scaffolding scope with stubs → WARN only (exit 0)
PLAN_SCAFFOLD_STUB="$TMPDIR_BASE/scaffold_stub.md"
cat > "$PLAN_SCAFFOLD_STUB" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: Set up database schema
⚠️ STUB API layer (future plan)

## Scope Classification

```yaml
scope: scaffolding
```

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Create schema
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_SCAFFOLD_STUB" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC6c: Scaffolding scope with stubs exits 0 (WARN only)"
else
    fail "AC6c: Scaffolding scope with stubs should exit 0 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "WARN.*unresolved stub"; then
    pass "AC6c: Reports WARN (not ERROR) for scaffolding stubs"
else
    fail "AC6c: Should report WARN for scaffolding stubs"
fi

# Test: poc scope with stubs → WARN only (exit 0)
PLAN_POC_STUB="$TMPDIR_BASE/poc_stub.md"
cat > "$PLAN_POC_STUB" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: Demo the concept
⚠️ STUB entire backend
❌ NOT BUILT database layer

## Scope Classification

```yaml
scope: poc
```

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Build demo
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_POC_STUB" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC6f: POC scope with stubs exits 0 (WARN only)"
else
    fail "AC6f: POC scope with stubs should exit 0 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "WARN.*unresolved stub"; then
    pass "AC6f: Reports WARN (not ERROR) for poc stubs"
else
    fail "AC6f: Should report WARN for poc stubs"
fi

# Test: legacy plan without trace → WARN (exit 0)
PLAN_LEGACY="$TMPDIR_BASE/legacy.md"
cat > "$PLAN_LEGACY" <<'HEREDOC'
execution_mode: sequential

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Do thing
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_LEGACY" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC6d: Legacy plan without trace exits 0"
else
    fail "AC6d: Legacy plan without trace should exit 0 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "WARN.*No Critical Path Trace"; then
    pass "AC6d: Reports WARN for legacy plan"
else
    fail "AC6d: Should report WARN for legacy plan"
fi

# Test: new plan with scope but no trace → ERROR (exit 1)
PLAN_SCOPE_NO_TRACE="$TMPDIR_BASE/scope_no_trace.md"
cat > "$PLAN_SCOPE_NO_TRACE" <<'HEREDOC'
execution_mode: sequential

## Scope Classification

```yaml
scope: feature
```

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Do thing
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_SCOPE_NO_TRACE" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]]; then
    pass "AC6e: Plan with scope but no trace exits 1"
else
    fail "AC6e: Plan with scope but no trace should exit 1 (got $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "ERROR.*missing Critical Path Trace"; then
    pass "AC6e: Reports ERROR for scope without trace"
else
    fail "AC6e: Should report ERROR for scope without trace"
fi

# Test: Critical Path Trace present but NO Scope Classification section at all
# (gemini-code-assist PR #257 finding). Under `set -eo pipefail`, the SCOPE=
# command substitution's grep finds no match and exits 1; without `|| true`
# on that pipeline the whole script aborted here instead of falling back to
# the "unknown" scope warning path.
PLAN_TRACE_NO_SCOPE="$TMPDIR_BASE/trace_no_scope.md"
cat > "$PLAN_TRACE_NO_SCOPE" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: User creates item
User clicks "Create" → ✅ CreateForm → ✅ Database

### Task 1.1
Acceptance Criteria: AC1
Steps:
Step 1: Do thing
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_TRACE_NO_SCOPE" 2>&1)
EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]]; then
    pass "AC6g: Trace present, no Scope Classification section - degrades instead of crashing"
else
    fail "AC6g: Should not abort (pipefail bug) when scope classification is absent (got exit $EXIT_CODE)"
fi
if echo "$OUTPUT" | grep -q "No scope classification found"; then
    pass "AC6g: Reports WARN for missing scope classification"
else
    fail "AC6g: Should warn about missing scope classification"
fi

# --- Summary ---
echo ""
echo "=== Test Results ==="
echo "Passed: $PASS | Failed: $FAIL"
[[ $FAIL -eq 0 ]] && { echo "ALL TESTS PASS"; exit 0; }
echo "TESTS FAILED"
exit 1
