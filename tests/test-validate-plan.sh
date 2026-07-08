#!/usr/bin/env bash
# Test suite for validate-plan.sh
# TDD: Run this BEFORE implementing validate-plan.sh to get RED state

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VALIDATE="$SCRIPT_DIR/validate-plan.sh"
PASS=0
FAIL=0

pass() { echo "  PASS: $*"; ((PASS++)) || true; }
fail() { echo "  FAIL: $*"; ((FAIL++)) || true; }

# Create temp plan dirs
TMPDIR_BASE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_BASE"' EXIT

# --- AC1: Validates 00-INDEX.md exists and phase files exist ---
echo "--- AC1: Structure Checks ---"

# Test: missing 00-INDEX.md exits 1
PLAN_EMPTY="$TMPDIR_BASE/empty"
mkdir -p "$PLAN_EMPTY"
if bash "$VALIDATE" "$PLAN_EMPTY" 2>/dev/null; then
    fail "AC1: Should exit 1 when 00-INDEX.md missing"
else
    pass "AC1: Exits 1 when 00-INDEX.md missing"
fi

# Test: 00-INDEX.md exists but no phase files exits 1
PLAN_NO_PHASES="$TMPDIR_BASE/no_phases"
mkdir -p "$PLAN_NO_PHASES"
echo "execution_mode: sequential" > "$PLAN_NO_PHASES/00-INDEX.md"
if bash "$VALIDATE" "$PLAN_NO_PHASES" 2>/dev/null; then
    fail "AC1: Should exit 1 when no phase files"
else
    pass "AC1: Exits 1 when no phase files found"
fi

# Test: valid minimal plan exits 0
PLAN_VALID="$TMPDIR_BASE/valid"
mkdir -p "$PLAN_VALID"
cat > "$PLAN_VALID/00-INDEX.md" <<'EOF'
execution_mode: sequential
EOF
cat > "$PLAN_VALID/01-phase.md" <<'EOF'
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

PLAN_WARN="$TMPDIR_BASE/warn"
mkdir -p "$PLAN_WARN"
echo "execution_mode: sequential" > "$PLAN_WARN/00-INDEX.md"
cat > "$PLAN_WARN/01-phase.md" <<'EOF'
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
PLAN_FEATURE_OK="$TMPDIR_BASE/feature_ok"
mkdir -p "$PLAN_FEATURE_OK"
cat > "$PLAN_FEATURE_OK/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: User creates item
User clicks "Create" → ✅ CreateForm → 🔨 API POST /items [Task 1.1] → ✅ Database

## Scope Classification

```yaml
scope: feature
```
HEREDOC
cat > "$PLAN_FEATURE_OK/01-phase.md" <<'HEREDOC'
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
PLAN_FEATURE_STUB="$TMPDIR_BASE/feature_stub"
mkdir -p "$PLAN_FEATURE_STUB"
cat > "$PLAN_FEATURE_STUB/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: User creates item
User clicks "Create" → ⚠️ STUB PlaceholderService → ❌ NOT BUILT RealEngine

## Scope Classification

```yaml
scope: feature
```
HEREDOC
cat > "$PLAN_FEATURE_STUB/01-phase.md" <<'HEREDOC'
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
PLAN_SCAFFOLD_STUB="$TMPDIR_BASE/scaffold_stub"
mkdir -p "$PLAN_SCAFFOLD_STUB"
cat > "$PLAN_SCAFFOLD_STUB/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: Set up database schema
⚠️ STUB API layer (future plan)

## Scope Classification

```yaml
scope: scaffolding
```
HEREDOC
cat > "$PLAN_SCAFFOLD_STUB/01-phase.md" <<'HEREDOC'
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
PLAN_POC_STUB="$TMPDIR_BASE/poc_stub"
mkdir -p "$PLAN_POC_STUB"
cat > "$PLAN_POC_STUB/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential

## Critical Path Trace

Journey: Demo the concept
⚠️ STUB entire backend
❌ NOT BUILT database layer

## Scope Classification

```yaml
scope: poc
```
HEREDOC
cat > "$PLAN_POC_STUB/01-phase.md" <<'HEREDOC'
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
PLAN_LEGACY="$TMPDIR_BASE/legacy"
mkdir -p "$PLAN_LEGACY"
cat > "$PLAN_LEGACY/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential
HEREDOC
cat > "$PLAN_LEGACY/01-phase.md" <<'HEREDOC'
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
PLAN_SCOPE_NO_TRACE="$TMPDIR_BASE/scope_no_trace"
mkdir -p "$PLAN_SCOPE_NO_TRACE"
cat > "$PLAN_SCOPE_NO_TRACE/00-INDEX.md" <<'HEREDOC'
execution_mode: sequential

## Scope Classification

```yaml
scope: feature
```
HEREDOC
cat > "$PLAN_SCOPE_NO_TRACE/01-phase.md" <<'HEREDOC'
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

# --- Summary ---
echo ""
echo "=== Test Results ==="
echo "Passed: $PASS | Failed: $FAIL"
[[ $FAIL -eq 0 ]] && { echo "ALL TESTS PASS"; exit 0; }
echo "TESTS FAILED"
exit 1
