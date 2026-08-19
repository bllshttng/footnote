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

# --- AC7: Single-doc semantic contract ---
echo ""
echo "--- AC7: Single-doc Semantic Contract ---"

PLAN_SEMANTIC="$TMPDIR_BASE/semantic.md"
cat > "$PLAN_SEMANTIC" <<'HEREDOC'
---
status: ready
created: 2026-07-25
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
---

# Semantic plan

## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: ["1.1"]
tasks:
  - id: "1.1"
    title: Validate semantics
    surface: [scripts/validate-plan.sh]
    verify: bash tests/test-validate-plan.sh
    acceptance: [AC7]
```
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1)
if ! echo "$OUTPUT" | grep -q "WARN:"; then
    pass "AC7a: Semantic plan needs no task/wave/critical-path headings"
else
    fail "AC7a: Semantic plan should have zero warnings: $OUTPUT"
fi
if echo "$OUTPUT" | grep -q "semantic execution contract valid"; then
    pass "AC7a: Wrapper delegates to semantic validator"
else
    fail "AC7a: Semantic validator receipt missing"
fi

PLAN_PLACEHOLDER="$TMPDIR_BASE/semantic-placeholder.md"
sed 's#bash tests/test-validate-plan.sh#\# fill in verify command#' \
    "$PLAN_SEMANTIC" > "$PLAN_PLACEHOLDER"
OUTPUT=$(bash "$VALIDATE" "$PLAN_PLACEHOLDER" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && echo "$OUTPUT" | grep -q "tasks.1.1.verify"; then
    pass "AC7b: Placeholder verification fails with exact task field"
else
    fail "AC7b: Placeholder verification should fail loud: $OUTPUT"
fi

# --- AC8: Semantic shape discrimination and portable source loading ---
echo ""
echo "--- AC8: Semantic Shape and Portable CLI ---"

PLAN_TRAILING="$TMPDIR_BASE/semantic-trailing.md"
sed 's/^## Execution Strategy$/## Execution Strategy   /' \
    "$PLAN_PLACEHOLDER" > "$PLAN_TRAILING"
OUTPUT=$(bash "$VALIDATE" "$PLAN_TRAILING" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && echo "$OUTPUT" | grep -q "tasks.1.1.verify"; then
    pass "AC8a: Trailing heading whitespace stays on semantic path"
else
    fail "AC8a: Trailing heading whitespace bypassed semantics: $OUTPUT"
fi

PLAN_QUOTED_QUICK="$TMPDIR_BASE/quoted-quick.md"
cat > "$PLAN_QUOTED_QUICK" <<'HEREDOC'
---
kind: "quick-plan"
status: ready
created: 2026-07-25
project: fno
---

# Quick

## Context

Only context.
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_QUOTED_QUICK" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && echo "$OUTPUT" | grep -q "Changes"; then
    pass "AC8b: Quoted quick-plan kind stays on semantic path"
else
    fail "AC8b: Quoted quick-plan kind bypassed semantics: $OUTPUT"
fi

PLAN_LEGACY="$TMPDIR_BASE/legacy-strategy.md"
cat > "$PLAN_LEGACY" <<'HEREDOC'
---
status: ready
created: 2026-07-25
project: fno
---

# Legacy plan

## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    mode: sequential
    tasks: ["1.1"]
```

### Task 1.1: Legacy task

**Files:**
- Modify: `src/a.py`

**Steps:**
1. Make the change.

**Acceptance Criteria:**
- The change works.
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_LEGACY" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && echo "$OUTPUT" | grep -q "Execution Strategy must declare at least one task"; then
    pass "AC8c: Every Execution Strategy uses semantic validation"
else
    fail "AC8c: Incomplete Execution Strategy bypassed semantics: $OUTPUT"
fi

OUTPUT=$(cd "$TMPDIR_BASE" && bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1) \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && echo "$OUTPUT" | grep -q "semantic execution contract valid"; then
    pass "AC8d: Validator resolves worktree source outside a Git cwd"
else
    fail "AC8d: Portable invocation used a stale installed CLI: $OUTPUT"
fi

# The uv branch is the fresh-worktree path: no cli/.venv, so the elected
# interpreter cannot import the CLI and `uv run` must run the SOURCE validator
# rather than falling through to the refusal.
if command -v uv >/dev/null 2>&1; then
    OUTPUT=$(FNO_PYTHON=/usr/bin/python3 bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1) \
        && EXIT_CODE=0 || EXIT_CODE=$?
    if [[ $EXIT_CODE -eq 0 ]] && echo "$OUTPUT" | grep -q "semantic execution contract valid"; then
        pass "AC8g: uv fallback runs the source validator when no venv interpreter works"
    else
        fail "AC8g: uv fallback did not run the source validator (exit $EXIT_CODE): $OUTPUT"
    fi
else
    echo "  SKIP:  AC8g: uv not installed"
fi

# --- AC9: Attributable dispatch holds ---
echo ""
echo "--- AC9: Dispatch Hold Shape ---"

PLAN_HOLD="$TMPDIR_BASE/dispatch-hold.md"
cat > "$PLAN_HOLD" <<'HEREDOC'
---
status: ready
created: 2026-08-18
project: fno
consolidation:
  outcome: proceed_alone
dispatch_hold:
  reason: Blocking review finding is unresolved
  release_when: The finding is fixed and re-reviewed
  review_on: 2099-08-20
  set_by: king:119e3c52
---

# Held plan
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_HOLD" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && echo "$OUTPUT" | grep -q "dispatch_hold has reason"; then
    pass "AC9a: Complete attributable dispatch_hold validates"
else
    fail "AC9a: Complete dispatch_hold rejected: $OUTPUT"
fi

for malformed in scalar partial invalid-date blank-setter; do
    cp "$PLAN_HOLD" "$TMPDIR_BASE/hold-$malformed.md"
    case "$malformed" in
        scalar) awk '/^dispatch_hold:/{print "dispatch_hold: blocked"; skip=1; next} skip && /^---$/{skip=0} !skip{print}' "$PLAN_HOLD" > "$TMPDIR_BASE/hold-$malformed.md" ;;
        partial) sed '/  set_by:/d' "$PLAN_HOLD" > "$TMPDIR_BASE/hold-$malformed.md" ;;
        invalid-date) sed 's/review_on: 2099-08-20/review_on: soon/' "$PLAN_HOLD" > "$TMPDIR_BASE/hold-$malformed.md" ;;
        blank-setter) sed 's/set_by: king:119e3c52/set_by: "   "/' "$PLAN_HOLD" > "$TMPDIR_BASE/hold-$malformed.md" ;;
    esac
    OUTPUT=$(bash "$VALIDATE" "$TMPDIR_BASE/hold-$malformed.md" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
    if [[ $EXIT_CODE -eq 1 ]] && echo "$OUTPUT" | grep -q "malformed dispatch_hold"; then
        pass "AC9b: $malformed dispatch_hold fails closed"
    else
        fail "AC9b: $malformed dispatch_hold did not fail closed: $OUTPUT"
    fi
done

# An installed fno older than this checkout advertises --execution while missing
# the guards this source defines, so an unrunnable source tree must refuse rather
# than hand the plan to it. PATH is stripped so uv is unreachable too.
OUTPUT=$(PATH=/usr/bin:/bin FNO_PYTHON=/usr/bin/python3 bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1) \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 2 ]] && echo "$OUTPUT" | grep -q "is not runnable"; then
    pass "AC8e: Unrunnable source refuses instead of delegating to an installed CLI"
else
    fail "AC8e: Unrunnable source did not refuse with exit 2 (exit $EXIT_CODE): $OUTPUT"
fi

# A broken validator must not be reported as an invalid plan: callers are told to
# stop and rewrite the plan on ERROR, and the underlying cause must survive.
if echo "$OUTPUT" | grep -q "TOOLFAIL" && echo "$OUTPUT" | grep -q "last probe error:"; then
    pass "AC8f: Tool failure is distinct from a plan violation and keeps its cause"
else
    fail "AC8f: Tool failure was indistinguishable or lost its cause: $OUTPUT"
fi

# --- Summary ---
echo ""
echo "=== Test Results ==="
echo "Passed: $PASS | Failed: $FAIL"
[[ $FAIL -eq 0 ]] && { echo "ALL TESTS PASS"; exit 0; }
echo "TESTS FAILED"
exit 1
