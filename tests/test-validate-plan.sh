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
if grep -q "WARN" <<< "$OUTPUT"; then
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
if grep -q "No stubs in critical path" <<< "$OUTPUT"; then
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
if grep -q "ERROR.*unresolved stub" <<< "$OUTPUT"; then
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
if grep -q "WARN.*unresolved stub" <<< "$OUTPUT"; then
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
if grep -q "WARN.*unresolved stub" <<< "$OUTPUT"; then
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
if grep -q "WARN.*No Critical Path Trace" <<< "$OUTPUT"; then
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
if grep -q "ERROR.*missing Critical Path Trace" <<< "$OUTPUT"; then
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
if grep -q "No scope classification found" <<< "$OUTPUT"; then
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
node: x-a11b003
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
  decisions_acknowledged: []
surface:
  question: "Does the semantic execution contract validate?"
  sweep: "bash tests/test-validate-plan.sh"
  control: skills/blueprint/scripts/validate-plan.sh:166 _semantic_validate
  answerers:
    - at: skills/blueprint/scripts/validate-plan.sh:166 _semantic_validate
      disposition: dual-logic
      reads: "_semantic_validate delegates to fno do plan validate --execution"
      feed: "fno do plan validate --execution"
      emits: "exit 0 with the semantic execution contract valid receipt"
  count: 1
  count_after: 1
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
# A keyed plan activates the stage-law gate, so AC7a runs it against a stub
# that lists zero laws; the fixture must keep producing zero warnings.
SEMLAWBIN="$TMPDIR_BASE/semlawbin"
mkdir -p "$SEMLAWBIN"
cat > "$SEMLAWBIN/fno-agents" <<'STUB'
#!/bin/bash
if [[ "${1:-}" == "law-match" ]]; then
    printf '%s' '{"ok":true,"stage":"blueprint","hook_output":{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"## Law governing blueprint\n\nThese live operator rulings govern the blueprint you are starting.\n"}}}'
    exit 0
fi
if [[ "${1:-}" == "surface-check" ]]; then
    # A clean surface receipt: zero findings, nothing on stdout.
    exit 0
fi
exit 3
STUB
chmod +x "$SEMLAWBIN/fno-agents"
OUTPUT=$(FNO_AGENTS_BIN="$SEMLAWBIN/fno-agents" bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1)
if ! grep -q "WARN:" <<< "$OUTPUT"; then
    pass "AC7a: Semantic plan needs no task/wave/critical-path headings"
else
    fail "AC7a: Semantic plan should have zero warnings: $OUTPUT"
fi
if grep -q "semantic execution contract valid" <<< "$OUTPUT"; then
    pass "AC7a: Wrapper delegates to semantic validator"
else
    fail "AC7a: Semantic validator receipt missing"
fi

PLAN_PLACEHOLDER="$TMPDIR_BASE/semantic-placeholder.md"
sed 's#bash tests/test-validate-plan.sh#\# fill in verify command#' \
    "$PLAN_SEMANTIC" > "$PLAN_PLACEHOLDER"
OUTPUT=$(bash "$VALIDATE" "$PLAN_PLACEHOLDER" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "tasks.1.1.verify" <<< "$OUTPUT"; then
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
if [[ $EXIT_CODE -eq 1 ]] && grep -q "tasks.1.1.verify" <<< "$OUTPUT"; then
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
if [[ $EXIT_CODE -eq 1 ]] && grep -q "Changes" <<< "$OUTPUT"; then
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
if [[ $EXIT_CODE -eq 1 ]] && grep -q "Execution Strategy must declare at least one task" <<< "$OUTPUT"; then
    pass "AC8c: Every Execution Strategy uses semantic validation"
else
    fail "AC8c: Incomplete Execution Strategy bypassed semantics: $OUTPUT"
fi

OUTPUT=$(cd "$TMPDIR_BASE" && bash "$VALIDATE" "$PLAN_SEMANTIC" 2>&1) \
    && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "semantic execution contract valid" <<< "$OUTPUT"; then
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
    if [[ $EXIT_CODE -eq 0 ]] && grep -q "semantic execution contract valid" <<< "$OUTPUT"; then
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
if [[ $EXIT_CODE -eq 0 ]] && grep -q "dispatch_hold has reason" <<< "$OUTPUT"; then
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
    if [[ $EXIT_CODE -eq 1 ]] && grep -q "malformed dispatch_hold" <<< "$OUTPUT"; then
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
if [[ $EXIT_CODE -eq 2 ]] && grep -q "is not runnable" <<< "$OUTPUT"; then
    pass "AC8e: Unrunnable source refuses instead of delegating to an installed CLI"
else
    fail "AC8e: Unrunnable source did not refuse with exit 2 (exit $EXIT_CODE): $OUTPUT"
fi

# A broken validator must not be reported as an invalid plan: callers are told to
# stop and rewrite the plan on ERROR, and the underlying cause must survive.
if grep -q "TOOLFAIL" <<< "$OUTPUT" && grep -q "last probe error:" <<< "$OUTPUT"; then
    pass "AC8f: Tool failure is distinct from a plan violation and keeps its cause"
else
    fail "AC8f: Tool failure was indistinguishable or lost its cause: $OUTPUT"
fi

# --- AC10: Answerer Enumeration (surface: block, step 2b-bis gate) ---
echo ""
echo "--- AC10: Answerer Enumeration ---"

# The shape check shells the fno-agents binary. The literal
# `target/debug/fno-agents` is the marker `_needs_rust_binary`
# (cli/src/fno/test_cmd.py) reads to schedule the build step, so CI has a
# fresh binary before this section runs.
FNO_AGENTS_BIN="$SCRIPT_DIR/../crates/fno-agents/target/debug/fno-agents"
export FNO_AGENTS_BIN

# The passing base: post-gate, non-quick, consolidation + a well-formed
# surface block. Every variant below derives from one of these two fixtures.
PLAN_SURFACE="$TMPDIR_BASE/surface.md"
cat > "$PLAN_SURFACE" <<'HEREDOC'
---
status: ready
created: 2026-09-10
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
surface:
  question: "Is this row reachable?"
  sweep: "rg -n 'ref truthy' src/"
  control: src/reader.py:10
  answerers:
    - at: src/reader.py:10
      disposition: dual-logic
      reads: "if row.ref:"
      feed: "SELECT ref FROM rows"
      emits: "12 rows, 2 with null ref (measured)"
    - at: src/writer.py:20
      disposition: out-of-scope
      reason: "writer emits, never reads; fixed by the dual-logic leg"
    - at: src/checker.py:30
      disposition: out-of-scope
      reason: "already validates the ref before use"
  count: 3
  count_after: 2
---

# Surface gate fixture
HEREDOC
PLAN_SURFACE_NOBLOCK="$TMPDIR_BASE/surface_noblock.md"
cat > "$PLAN_SURFACE_NOBLOCK" <<'HEREDOC'
---
status: ready
created: 2026-09-10
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
---

# Surface gate fixture, no surface block
HEREDOC

OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "surface block (step 2b-bis gate): question" <<< "$OUTPUT"; then
    pass "AC10a: Well-formed surface block passes and prints its receipt"
else
    fail "AC10a: Well-formed surface block should pass (exit $EXIT_CODE): $OUTPUT"
fi

# V1, the gate bites: a post-gate non-quick plan with no block exits 1.
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_NOBLOCK" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "no surface: block in frontmatter" <<< "$OUTPUT"; then
    pass "AC10b: Post-gate non-quick plan without surface block fails"
else
    fail "AC10b: Missing block should fail closed (exit $EXIT_CODE): $OUTPUT"
fi

# V2, graduation: the same blockless plan as a quick plan warns and exits 0.
# The body carries the minimum the semantic contract asks of a quick plan
# (difficulty band, Changes, Files to Modify, Verification), so the only
# variable under test is the gate's graduation.
PLAN_SURFACE_QUICK="$TMPDIR_BASE/surface_quick.md"
cat > "$PLAN_SURFACE_QUICK" <<'HEREDOC'
---
kind: quick-plan
status: ready
created: 2026-09-10
difficulty: low
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
---

# Quick fixture, no surface block

## Context

Minimal quick plan for the graduation probe.

## Changes

### 1. Only change

**Files:** `src/reader.py`

Do the one thing.

## Files to Modify

| File | Action |
|------|--------|
| `src/reader.py` | Modify - validate the ref |

## Verification

1. `bash tests/probe.sh` - passes

## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    mode: parallel
    name: quick fixture probe
    difficulty: low
    tasks: ['1.1']
tasks:
  - id: '1.1'
    title: Only change
    surface: ['src/reader.py']
    verify: bash tests/test-validate-plan.sh
    acceptance:
      - Given a row, when the ref is null, then the reader reports unreachable
```
HEREDOC
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_QUICK" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "no surface: block (quick plan)" <<< "$OUTPUT"; then
    pass "AC10c: Quick plan without block warns and proceeds"
else
    fail "AC10c: Quick plan graduation should warn-only (exit $EXIT_CODE): $OUTPUT"
fi

# V3, the question tooth: a noun-phrase question is the regression.
PLAN_SURFACE_NOUN="$TMPDIR_BASE/surface_noun.md"
sed 's/Is this row reachable?/row reachability/' "$PLAN_SURFACE" > "$PLAN_SURFACE_NOUN"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_NOUN" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "does not end in a question mark" <<< "$OUTPUT"; then
    pass "AC10d: Noun-phrase question fails the question mark tooth"
else
    fail "AC10d: Noun-phrase question should fail (exit $EXIT_CODE): $OUTPUT"
fi

# V4, the disposition tooth: an out-of-scope with no reason is undisposed.
PLAN_SURFACE_NOREASON="$TMPDIR_BASE/surface_noreason.md"
sed '/reason: "writer emits/d' "$PLAN_SURFACE" > "$PLAN_SURFACE_NOREASON"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_NOREASON" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "Undisposed: src/writer.py:20" <<< "$OUTPUT"; then
    pass "AC10e: Out-of-scope without reason is named undisposed"
else
    fail "AC10e: Missing reason should fail naming the answerer (exit $EXIT_CODE): $OUTPUT"
fi

# V5, the feed tooth: a changed answerer needs reads AND emits.
PLAN_SURFACE_NOREADS="$TMPDIR_BASE/surface_noreads.md"
sed '/reads: "if row.ref:/d' "$PLAN_SURFACE" > "$PLAN_SURFACE_NOREADS"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_NOREADS" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "names no feed for it" <<< "$OUTPUT"; then
    pass "AC10f: Changed answerer without reads fails with the feed refusal"
else
    fail "AC10f: Missing reads should fail (exit $EXIT_CODE): $OUTPUT"
fi

PLAN_SURFACE_NOEMITS="$TMPDIR_BASE/surface_noemits.md"
sed 's/emits: "12 rows, 2 with null ref (measured)"/emits: ""/' "$PLAN_SURFACE" > "$PLAN_SURFACE_NOEMITS"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_NOEMITS" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "names no feed for it" <<< "$OUTPUT"; then
    pass "AC10g: Changed answerer with empty emits still fails"
else
    fail "AC10g: Empty emits should fail (exit $EXIT_CODE): $OUTPUT"
fi

# V6, the count tooth: the count is the estimate and must match.
PLAN_SURFACE_COUNT="$TMPDIR_BASE/surface_count.md"
sed 's/^  count: 3/  count: 1/' "$PLAN_SURFACE" > "$PLAN_SURFACE_COUNT"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_COUNT" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "must match what was found" <<< "$OUTPUT"; then
    pass "AC10h: Count disagreement with the answerer list fails"
else
    fail "AC10h: Count mismatch should fail (exit $EXIT_CODE): $OUTPUT"
fi

# V9, the shrink tooth and the control tooth.
PLAN_SURFACE_SHRINK="$TMPDIR_BASE/surface_shrink.md"
sed 's/^  count_after: 2/  count_after: 4/' "$PLAN_SURFACE" > "$PLAN_SURFACE_SHRINK"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_SHRINK" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "exceeds count" <<< "$OUTPUT"; then
    pass "AC10i: count_after above count fails"
else
    fail "AC10i: Growing count_after should fail (exit $EXIT_CODE): $OUTPUT"
fi

PLAN_SURFACE_CONTROL="$TMPDIR_BASE/surface_control.md"
sed 's#control: src/reader.py:10#control: src/nowhere.py:1#' "$PLAN_SURFACE" > "$PLAN_SURFACE_CONTROL"
OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE_CONTROL" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "names no listed answerer" <<< "$OUTPUT"; then
    pass "AC10j: A control that is not an answerer fails naming the control"
else
    fail "AC10j: Phantom control should fail (exit $EXIT_CODE): $OUTPUT"
fi

OUTPUT=$(bash "$VALIDATE" "$PLAN_SURFACE" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "control \`src/reader.py:10\` returned by the sweep" <<< "$OUTPUT"; then
    pass "AC10k: A matching control prints its own receipt"
else
    fail "AC10k: Matching control receipt missing (exit $EXIT_CODE): $OUTPUT"
fi

# The cross-language walk: a Rust reader a Python answerer does not name.
# The fixture lives in a committed temp git repo so the walk greps a real
# tree; the validator runs from inside it.
WALK_DIR="$TMPDIR_BASE/walkrepo"
mkdir -p "$WALK_DIR/src" "$WALK_DIR/crates/x/src"
printf 'def row_ref_valid(row):\n    return bool(row)\n' > "$WALK_DIR/src/reader.py"
printf 'fn go(row: i32) -> bool {\n    row_ref_valid(row)\n}\n' > "$WALK_DIR/crates/x/src/reader.rs"
git -C "$WALK_DIR" init -q
git -C "$WALK_DIR" add .
git -C "$WALK_DIR" -c user.name=t -c user.email=t@t commit -qm init

PLAN_WALK="$WALK_DIR/plan.md"
cat > "$PLAN_WALK" <<'HEREDOC'
---
status: ready
created: 2026-09-20
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
surface:
  question: "Is this row reachable?"
  sweep: "rg -n 'row_ref_valid' src/"
  control: src/reader.py:1 row_ref_valid
  answerers:
    - at: src/reader.py:1 row_ref_valid
      disposition: dual-logic
      reads: "row_ref_valid(row)"
      emits: "bool per row"
  count: 1
  count_after: 1
---

# Cross-language walk fixture
HEREDOC
OUTPUT=$(cd "$WALK_DIR" && bash "$VALIDATE" "$PLAN_WALK" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 1 ]] && grep -q "is read in rust" <<< "$OUTPUT" && grep -q "crates/x/src/reader.rs" <<< "$OUTPUT"; then
    pass "AC10l: An unlisted Rust reader of a changed Python symbol refuses"
else
    fail "AC10l: Cross-language reader should refuse (exit $EXIT_CODE): $OUTPUT"
fi

# The same reader, disposed out-of-scope with a reason: the walk's receipt
# names both trees and nothing is unlisted.
PLAN_WALK_COVERED="$WALK_DIR/plan_covered.md"
cat > "$PLAN_WALK_COVERED" <<'HEREDOC'
---
status: ready
created: 2026-09-20
project: fno
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
surface:
  question: "Is this row reachable?"
  sweep: "rg -n 'row_ref_valid' src/"
  control: src/reader.py:1 row_ref_valid
  answerers:
    - at: src/reader.py:1 row_ref_valid
      disposition: dual-logic
      reads: "row_ref_valid(row)"
      emits: "bool per row"
    - at: crates/x/src/reader.rs:1
      disposition: out-of-scope
      reason: "calls the Python verdict, never reads the row itself"
  count: 2
  count_after: 2
---

# Covered walk fixture
HEREDOC
OUTPUT=$(cd "$WALK_DIR" && bash "$VALIDATE" "$PLAN_WALK_COVERED" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "readers by tree python 1, rust 1" <<< "$OUTPUT"; then
    pass "AC10m: A disposed reader passes with a receipt naming both trees"
else
    fail "AC10m: Covered reader should pass naming trees (exit $EXIT_CODE): $OUTPUT"
fi

# Graduation: the same unlisted reader on a pre-gate plan warns only.
PLAN_WALK_OLD="$WALK_DIR/plan_old.md"
sed 's/created: 2026-09-20/created: 2026-09-01/' "$PLAN_WALK" > "$PLAN_WALK_OLD"
OUTPUT=$(cd "$WALK_DIR" && bash "$VALIDATE" "$PLAN_WALK_OLD" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "is read in rust" <<< "$OUTPUT"; then
    pass "AC10n: A pre-gate plan's unlisted reader warns and proceeds"
else
    fail "AC10n: Pre-gate walk finding should warn-only (exit $EXIT_CODE): $OUTPUT"
fi

# No reachable binary: unset env, no build under the source root, nothing on
# PATH. The copied validator finds no cli/src/fno from the temp scripts dir.
WALK_NOBIN="$TMPDIR_BASE/nobin"
mkdir -p "$WALK_NOBIN/scripts"
cp "$VALIDATE" "$WALK_NOBIN/scripts/validate-plan.sh"
OUTPUT=$(cd "$WALK_NOBIN" && env -u FNO_AGENTS_BIN PATH=/usr/bin:/bin FNO_PYTHON=/usr/bin/python3 bash "$WALK_NOBIN/scripts/validate-plan.sh" "$PLAN_SURFACE" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if [[ $EXIT_CODE -eq 0 ]] && grep -q "surface block NOT CHECKED (no fno-agents binary)" <<< "$OUTPUT" && ! grep -q "surface block (step 2b-bis gate): question" <<< "$OUTPUT"; then
    pass "AC10o: No reachable binary warns NOT CHECKED with no pass receipt"
else
    fail "AC10o: Missing binary should warn-only without a receipt (exit $EXIT_CODE): $OUTPUT"
fi

# --- AC11: No New Python row gate ---
echo ""
echo "--- AC11: No New Python ---"

# The section's lines, not the exit code, prove this check: a dated fixture
# also trips the consolidation and surface gates, so exit 1 alone proves
# nothing.
nnpy() {
    sed -n '/^--- No New Python ---/,/^--- /p' <<< "$1" | sed '1d' | grep -v '^--- ' || true
}

# AC11a (AC1-HP): post-gate plan, Modify Python row -> ERROR naming the path,
# the crates/ rule, and the three legal actions; exit 1.
PLAN_NNPY_A="$TMPDIR_BASE/nnpy_a.md"
cat > "$PLAN_NNPY_A" <<'EOF'
---
claims: x-nnpya
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Modify |
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_A" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if [[ $EXIT_CODE -eq 1 ]] \
    && grep -q "cli/src/fno/mail/cli.py is 'Modify'" <<< "$NNPY_OUT" \
    && grep -q "New code lands in crates/" <<< "$NNPY_OUT" \
    && grep -q "Port" <<< "$NNPY_OUT" && grep -q "Delete" <<< "$NNPY_OUT" && grep -q "Grant" <<< "$NNPY_OUT"; then
    pass "AC11a: Modify Python row on a post-gate plan errors naming the rule and three actions"
else
    fail "AC11a: expected ERROR naming the row (exit $EXIT_CODE): $NNPY_OUT"
fi

# AC11b (AC2-HP): Port row plus the crates/ row it lands in -> clean section.
PLAN_NNPY_B="$TMPDIR_BASE/nnpy_b.md"
cat > "$PLAN_NNPY_B" <<'EOF'
---
claims: x-nnpyb
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Port: the send verb moves to crates |
| `crates/fno-agents/src/mail.rs` | Modify |
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_B" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if [[ -z "$NNPY_OUT" ]]; then
    pass "AC11b: Port with a crates/ row prints nothing"
else
    fail "AC11b: expected a clean section (exit $EXIT_CODE): $NNPY_OUT"
fi

# AC11c (AC3-ERR): Port row with no crates/ row -> ERROR naming the gap.
PLAN_NNPY_C="$TMPDIR_BASE/nnpy_c.md"
cat > "$PLAN_NNPY_C" <<'EOF'
---
claims: x-nnpyc
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Port |
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_C" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if grep -q "is a Port, but no crates/ row in the table names where it lands" <<< "$NNPY_OUT"; then
    pass "AC11c: Port with no crates/ row errors"
else
    fail "AC11c: expected the Port-gap ERROR: $NNPY_OUT"
fi

# AC11d (AC4-HP): a bold Delete row -> nothing.
PLAN_NNPY_D="$TMPDIR_BASE/nnpy_d.md"
cat > "$PLAN_NNPY_D" <<'EOF'
---
claims: x-nnpyd
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | **Delete** |
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_D" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if [[ -z "$NNPY_OUT" ]]; then
    pass "AC11d: bold Delete row prints nothing"
else
    fail "AC11d: expected a clean section: $NNPY_OUT"
fi

# The stub fno for the Grant cases: LIVE line for d-1234abcd, the
# no-decision line for any other id, real fno for every other argv.
REAL_FNO="$(command -v fno || true)"
STUBBIN="$TMPDIR_BASE/stubbin"
mkdir -p "$STUBBIN"
{
    echo '#!/bin/bash'
    echo 'if [[ "${1:-} ${2:-}" == "backlog decisions" && "${3:-}" == "d-1234abcd" ]]; then'
    echo '    echo "LIVE  LAW  d-1234abcd  2026-09-12T00:00:00Z  new-code-language  stub"'
    echo '    exit 0'
    echo 'fi'
    echo 'if [[ "${1:-} ${2:-}" == "backlog decisions" ]]; then'
    echo '    echo "no decision carries it"'
    echo '    exit 0'
    echo 'fi'
    echo 'if [[ "${1:-} ${2:-} ${3:-}" == "config get blueprint.python_repair_added_lines" ]]; then'
    echo '    echo 30'
    echo '    exit 0'
    echo 'fi'
    if [[ -n "$REAL_FNO" ]]; then
        printf 'exec %q "$@"\n' "$REAL_FNO"
    else
        echo 'exit 3'
    fi
} > "$STUBBIN/fno"
chmod +x "$STUBBIN/fno"

# AC11e (AC5-HP): Grant naming a live ruling -> nothing.
PLAN_NNPY_E="$TMPDIR_BASE/nnpy_e.md"
cat > "$PLAN_NNPY_E" <<'EOF'
---
claims: x-nnpye
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Grant d-1234abcd +5 |
EOF
OUTPUT=$(PATH="$STUBBIN:$PATH" bash "$VALIDATE" "$PLAN_NNPY_E" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if [[ -z "$NNPY_OUT" ]]; then
    pass "AC11e: Grant with a LIVE ruling prints nothing"
else
    fail "AC11e: expected a clean section: $NNPY_OUT"
fi

# AC11f (AC5-ERR): Grant whose id reads no LIVE line -> ERROR naming the id
# and the read that failed.
PLAN_NNPY_F="$TMPDIR_BASE/nnpy_f.md"
cat > "$PLAN_NNPY_F" <<'EOF'
---
claims: x-nnpyf
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Grant d-9999abcd +5 |
EOF
OUTPUT=$(PATH="$STUBBIN:$PATH" bash "$VALIDATE" "$PLAN_NNPY_F" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if grep -q "d-9999abcd" <<< "$NNPY_OUT" && grep -q "fno backlog decisions d-9999abcd" <<< "$NNPY_OUT"; then
    pass "AC11f: Grant without a LIVE ruling errors naming the id and the read"
else
    fail "AC11f: expected the Grant ERROR: $NNPY_OUT"
fi

# AC11g (AC6-ERR): task surface naming a Python path with no table row.
PLAN_NNPY_G="$TMPDIR_BASE/nnpy_g.md"
cat > "$PLAN_NNPY_G" <<'EOF'
---
claims: x-nnpyg
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `crates/fno-agents/src/mail.rs` | Modify |

## Execution Strategy

```yaml
tasks:
  - id: '1.1'
    surface: ['cli/src/fno/mail/cli.py']
```
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_G" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if grep -q "cli/src/fno/mail/cli.py is in a task surface, but no Files to Modify row declares its action" <<< "$NNPY_OUT"; then
    pass "AC11g: undeclared task surface errors"
else
    fail "AC11g: expected the surface ERROR: $NNPY_OUT"
fi

# AC11h (AC7-EDGE): Python path named only in prose, crates/ rows only.
PLAN_NNPY_H="$TMPDIR_BASE/nnpy_h.md"
cat > "$PLAN_NNPY_H" <<'EOF'
---
claims: x-nnpyh
created: 2099-01-01
---

## Files to Modify

| File | Action |
|------|--------|
| `crates/fno-agents/src/mail.rs` | Modify |

Prose may discuss cli/src/fno/update.py history; prose writes nothing.
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_H" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if [[ -z "$NNPY_OUT" ]]; then
    pass "AC11h: prose-only mention prints nothing"
else
    fail "AC11h: expected a clean section: $NNPY_OUT"
fi

# AC11i (AC8-EDGE): pre-gate plan warns with its created date; an undated
# plan errors (filename carries no date either).
PLAN_NNPY_I="$TMPDIR_BASE/nnpy_i.md"
sed 's/created: 2099-01-01/created: 2026-09-13/' "$PLAN_NNPY_A" > "$PLAN_NNPY_I"
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_I" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if grep -q "WARN" <<< "$NNPY_OUT" && grep -q "created 2026-09-13, not after the 2026-09-16 gate" <<< "$NNPY_OUT" \
    && ! grep -q "ERROR" <<< "$NNPY_OUT"; then
    pass "AC11i: pre-gate plan warns with its created date"
else
    fail "AC11i: expected a WARN with the created date: $NNPY_OUT"
fi
PLAN_NNPY_I2="$TMPDIR_BASE/nnpy_undated.md"
sed '/^created:/d' "$PLAN_NNPY_A" > "$PLAN_NNPY_I2"
OUTPUT=$(bash "$VALIDATE" "$PLAN_NNPY_I2" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
NNPY_OUT=$(nnpy "$OUTPUT")
if grep -q "ERROR" <<< "$NNPY_OUT" && grep -q "no readable created: date" <<< "$NNPY_OUT"; then
    pass "AC11i: undated plan errors"
else
    fail "AC11i: expected the undated ERROR: $NNPY_OUT"
fi

# AC11j: stage-law gate - a law the blueprint-stage block lists with no
# decisions_acknowledged entry on a post-gate plan errors naming id+subject.
# The stub fno-agents answers law-match only; the surface gate warns NOT
# CHECKED against it, which this section does not read.
STUB_AGENTS="$STUBBIN/fno-agents"
cat > "$STUB_AGENTS" <<'STUB'
#!/bin/bash
if [[ "${1:-}" == "law-match" ]]; then
    printf '%s' '{"ok":true,"stage":"blueprint","hook_output":{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"## Law governing blueprint\n\nThese live operator rulings govern the blueprint you are starting.\n- d-a11b0002 (stub-epic-ruling): A stub ruling names the epic.\n"}}}'
    exit 0
fi
exit 3
STUB
chmod +x "$STUB_AGENTS"
PLAN_NNPY_J="$TMPDIR_BASE/nnpy_j.md"
cat > "$PLAN_NNPY_J" <<'EOF'
---
claims: x-a11b001
created: 2099-01-01
consolidation:
  outcome: proceed_alone
  rejected: []
---

## Files to Modify

| File | Action |
|------|--------|
| `crates/fno-agents/src/mail.rs` | Modify |
EOF
OUTPUT=$(FNO_AGENTS_BIN="$STUB_AGENTS" bash "$VALIDATE" "$PLAN_NNPY_J" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "decisions_acknowledged is missing d-a11b0002 (stub-epic-ruling)" <<< "$OUTPUT"; then
    pass "AC11j: stage-block law with no entry errors naming id and subject"
else
    fail "AC11j: expected the stage-law ERROR: $OUTPUT"
fi

# AC11k: the same plan carrying the entry is clean.
PLAN_NNPY_K="$TMPDIR_BASE/nnpy_k.md"
cat > "$PLAN_NNPY_K" <<'EOF'
---
claims: x-a11b002
created: 2099-01-01
consolidation:
  outcome: proceed_alone
  rejected: []
  decisions_acknowledged:
    - decision_id: d-a11b0002
      reason: "fixture acknowledgment"
---

## Files to Modify

| File | Action |
|------|--------|
| `crates/fno-agents/src/mail.rs` | Modify |
EOF
OUTPUT=$(FNO_AGENTS_BIN="$STUB_AGENTS" bash "$VALIDATE" "$PLAN_NNPY_K" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if ! grep -q "d-a11b0002" <<< "$OUTPUT"; then
    pass "AC11k: acknowledged stage law prints nothing"
else
    fail "AC11k: expected no d-a11b0002 finding: $OUTPUT"
fi

# AC12: Plan Node Binding. A filename-encoded node id with no node:/claims:
# key in the frontmatter binds to nothing and mutes both id-keyed gates, and
# every gate skip must say NOT CHECKED instead of reading green.
PLAN_BIND_A="$TMPDIR_BASE/20990101-binding-a-x-dcc5.md"
cat > "$PLAN_BIND_A" <<'EOF'
---
project: fno
status: ready
kind: quick-plan
created: 2099-01-01
difficulty: low
join: manual
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "too many"
---

## Files to Modify

| File | Action |
|------|--------|
| `crates/fno-agents/src/mail.rs` | Modify |
EOF
OUTPUT=$(bash "$VALIDATE" "$PLAN_BIND_A" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "the filename names x-dcc5 but no node:/claims: key does" <<< "$OUTPUT"; then
    pass "AC12a: filename id with no frontmatter key errors naming the id"
else
    fail "AC12a: expected the binding ERROR: $OUTPUT"
fi

# AC12b: the same plan carrying the key is clean of binding findings and of
# the two NOT CHECKED warnings.
PLAN_BIND_B="$TMPDIR_BASE/20990101-binding-b-x-dcc5.md"
sed 's/^join: manual$/join: manual\nnode: x-dcc5\ndecisions_acknowledged: []/' "$PLAN_BIND_A" > "$PLAN_BIND_B"
OUTPUT=$(bash "$VALIDATE" "$PLAN_BIND_B" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if ! grep -q "filename names x-dcc5" <<< "$OUTPUT" \
    && ! grep -q "no node:/claims: id in frontmatter" <<< "$OUTPUT"; then
    pass "AC12b: keyed plan prints no binding error and no NOT CHECKED warning"
else
    fail "AC12b: expected a clean binding section: $OUTPUT"
fi

# AC12c: an id-less filename with no key stays legal but warns NOT CHECKED
# from both id-keyed gates.
PLAN_BIND_C="$TMPDIR_BASE/20990101-binding-c.md"
sed 's/binding-a-x-dcc5\.md/binding-c.md/' "$PLAN_BIND_A" > "$PLAN_BIND_C"
OUTPUT=$(bash "$VALIDATE" "$PLAN_BIND_C" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "decisions_acknowledged check NOT CHECKED" <<< "$OUTPUT" \
    && grep -q "stage-law check NOT CHECKED" <<< "$OUTPUT" \
    && ! grep -q "filename names" <<< "$OUTPUT"; then
    pass "AC12c: key-less id-less plan warns NOT CHECKED from both gates"
else
    fail "AC12c: expected both NOT CHECKED warnings and no binding finding: $OUTPUT"
fi

# AC12d: a pre-gate plan warns with its created date instead of erroring.
PLAN_BIND_D="$TMPDIR_BASE/20990101-binding-d-x-dcc5.md"
sed 's/binding-a-x-dcc5\.md/binding-d-x-dcc5.md/; s/^created: 2099-01-01$/created: 2020-01-01/' "$PLAN_BIND_A" > "$PLAN_BIND_D"
OUTPUT=$(bash "$VALIDATE" "$PLAN_BIND_D" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "the filename names x-dcc5 but no node:/claims: key does (created 2020-01-01, before the" <<< "$OUTPUT" \
    && ! grep -q "ERROR.*filename names" <<< "$OUTPUT"; then
    pass "AC12d: pre-gate plan warns with its created date"
else
    fail "AC12d: expected the pre-gate WARN: $OUTPUT"
fi

# AC12f: a Grant row declaring over the budget errors naming the count, the
# budget and the config key.
PLAN_BIND_F="$TMPDIR_BASE/20990101-binding-f.md"
cat > "$PLAN_BIND_F" <<'EOF'
---
project: fno
status: ready
kind: quick-plan
created: 2099-01-01
difficulty: low
join: manual
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "too many"
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Grant d-1234abcd +101 |
EOF
OUTPUT=$(PATH="$STUBBIN:$PATH" bash "$VALIDATE" "$PLAN_BIND_F" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "declare +101 added cli/src/fno lines against a budget of 30 (config blueprint.python_repair_added_lines)" <<< "$OUTPUT"; then
    pass "AC12f: over-budget Grant errors naming count, budget and key"
else
    fail "AC12f: expected the budget ERROR: $OUTPUT"
fi

# AC12g: a Grant row with no +N errors naming the missing size.
PLAN_BIND_G="$TMPDIR_BASE/20990101-binding-g.md"
sed 's/Grant d-1234abcd +101/Grant d-1234abcd/' "$PLAN_BIND_F" > "$PLAN_BIND_G"
OUTPUT=$(PATH="$STUBBIN:$PATH" bash "$VALIDATE" "$PLAN_BIND_G" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "with no declared size - write the row as Grant d-1234abcd +N" <<< "$OUTPUT"; then
    pass "AC12g: unsized Grant errors naming the +N remedy"
else
    fail "AC12g: expected the missing-size ERROR: $OUTPUT"
fi

# AC12h: Grant rows are summed - one plan is one PR and the ceiling is the PR's.
PLAN_BIND_H="$TMPDIR_BASE/20990101-binding-h.md"
cat > "$PLAN_BIND_H" <<'EOF'
---
project: fno
status: ready
kind: quick-plan
created: 2099-01-01
difficulty: low
join: manual
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "too many"
---

## Files to Modify

| File | Action |
|------|--------|
| `cli/src/fno/mail/cli.py` | Grant d-1234abcd +20 |
| `cli/src/fno/mail/send.py` | Grant d-1234abcd +20 |
EOF
OUTPUT=$(PATH="$STUBBIN:$PATH" bash "$VALIDATE" "$PLAN_BIND_H" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "declare +40 added cli/src/fno lines against a budget of 30" <<< "$OUTPUT"; then
    pass "AC12h: Grant rows sum against the budget"
else
    fail "AC12h: expected the summed budget ERROR: $OUTPUT"
fi

# AC12i: a missing config key falls back to 30, so the gate still bites.
FAILBIN="$TMPDIR_BASE/failbin"
mkdir -p "$FAILBIN"
{
    echo '#!/bin/bash'
    echo 'if [[ "${1:-} ${2:-}" == "backlog decisions" && "${3:-}" == "d-1234abcd" ]]; then'
    echo '    echo "LIVE  LAW  d-1234abcd  2026-09-12T00:00:00Z  new-code-language  stub"'
    echo '    exit 0'
    echo 'fi'
    echo 'exit 1'
} > "$FAILBIN/fno"
chmod +x "$FAILBIN/fno"
PLAN_BIND_I="$TMPDIR_BASE/20990101-binding-i.md"
sed 's/Grant d-1234abcd +101/Grant d-1234abcd +31/' "$PLAN_BIND_F" > "$PLAN_BIND_I"
OUTPUT=$(PATH="$FAILBIN:$PATH" bash "$VALIDATE" "$PLAN_BIND_I" 2>&1) && EXIT_CODE=0 || EXIT_CODE=$?
if grep -q "against a budget of 30" <<< "$OUTPUT"; then
    pass "AC12i: unreadable config falls back to the 30-line budget"
else
    fail "AC12i: expected the fallback-budget ERROR: $OUTPUT"
fi

# --- Summary ---
echo ""
echo "=== Test Results ==="
echo "Passed: $PASS | Failed: $FAIL"
[[ $FAIL -eq 0 ]] && { echo "ALL TESTS PASS"; exit 0; }
echo "TESTS FAILED"
exit 1
