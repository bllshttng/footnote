# Phase File Template

Each phase file contains **detailed implementation tasks**.

---
phase: 1
title: Database & Core Schemas
points: 13
depends_on: []
parallel_with: []
---

# Phase 1: Database & Core Schemas

> **Estimated Points:** 13

## Tasks

### Task 1.1: [Name] (3 pts)

**Files:**
- Create: `exact/path/to/file.ts`
- Modify: `exact/path/to/existing.ts:123-145`
- Test: `tests/exact/path/to/test.ts`

<!-- Optional per-task executor override. Highest-priority tier in operator's
     resolver chain (task -> plan -> surface inference -> 'do').
     For example, add `executor: impeccable` for design-aware frontend work
     via /impeccable craft+critique, or `executor: do` for TDD-disciplined
     archer (default; alias 'tdd'). Omit when plan-level or surface
     inference picks the right one.
     See docs/guides/per-task-executors.md. -->

<!-- Synthesis rule: Every task must be executable by a worker agent that has
     never seen this plan before. Include file paths, line numbers, current
     behavior, desired behavior, and pattern sources. The orchestrator should
     be able to construct a Task tool prompt from this task description alone,
     without additional codebase exploration. -->

**Acceptance Criteria:**

**AC1-HP: Happy Path**
Given [precondition]
When [action]
Then [expected result]
And [database verification]

**AC2-ERR: Error/Validation**
Given [precondition]
When [invalid action]
Then [error handling]

**AC3-UI: UI State Changes** _(if task has UI)_
Given [initial UI state]
When [user action]
Then [loading/disabled/feedback state]
And [final state after completion]

**AC4-EDGE: Edge Cases** _(use judgment; seed from design doc's `## Failure Modes`)_
Given [boundary condition, empty state, or concurrent access]
When [action]
Then [graceful handling]

When the upstream design doc has a `## Failure Modes` section, emit one
AC4-EDGE per relevant bullet and cite the source by its short name. The
citation lets a reviewer trace the test back to the design-time reasoning:

```markdown
**AC4-EDGE: Cites "Concurrent submit" from design doc**
Given two submit requests arrive within 100ms with the same dedupe key
When the server processes them
Then the second request returns the first response
And no duplicate row is inserted

**AC4-EDGE: Cites "Balance never negative" from design doc**
Given a withdrawal that would take the balance below zero
When the debit is posted
Then the write is rejected with an "insufficient funds" error
And the balance row is unchanged
```

Skip bullets that have no touchpoint in this phase. AC4-EDGE citations
should map to the code surface actually being built, not pad the criteria
with unrelated concerns.

**AC5-FR: Failure Recovery** _(use judgment)_
Given [action in progress]
When [server error / navigation away / double-click]
Then [element recovers to usable state]
And [no orphaned state remains]

Not every task needs all 5 types. AC1-HP and AC2-ERR are always required.
Include AC3-UI when the task has interactive elements, AC4-EDGE when there
are meaningful boundary conditions, and AC5-FR when there are async operations
that could fail or be interrupted.

**Verify:** `pnpm test tests/path/file.spec.ts`

**Steps:**

> **Synthesis check:** Can an orchestrator read this task and write a worker prompt
> containing exact file paths, line numbers, and specific code changes? If not,
> add more detail to the Files and Steps sections.

**Step 1: Write failing test**
```typescript
test('AC1-HP: behavior', async () => {
  // Given
  // When
  // Then
})
```

**Step 2: Run test to verify it fails**
Run: `pnpm test tests/path/file.spec.ts`
Expected: FAIL with "function not defined"

**Step 3: Write minimal implementation**
```typescript
// Implementation code here
```

**Step 4: Run test to verify it passes**
Run: `pnpm test tests/path/file.spec.ts`
Expected: PASS

**Step 5: Commit**
```bash
git add .
git commit -m "feat: add specific feature"
```

---

### Task 1.2: [Next Task] (N pts)
...

---

## Phase Completion Checklist

- [ ] All tasks implemented
- [ ] All tests passing
- [ ] Database migrations work
- [ ] Code committed
