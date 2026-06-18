---
name: tdd
description: "Test-Driven Development: Write test first, watch it fail, implement minimal code. Use when: implementing any feature or bugfix, before writing production code, during /do workflow, whenever acceptance criteria exist."
---

# Test-Driven Development (TDD)

**Core principle:** If you didn't watch the test fail, you don't know if it tests the right thing.

Write the test first. Watch it fail. Write minimal code to pass.

## The Iron Law

```
NO PRODUCTION CODE WITHOUT A FAILING TEST FIRST
```

Write code before the test? Delete it. Start over.

## Red-Green-Refactor Cycle

```
    ┌──────┐
    │ RED  │ Write failing test
    └──┬───┘
       │ Verify: test FAILS correctly
       ▼
    ┌──────┐
    │GREEN │ Write minimal code to pass
    └──┬───┘
       │ Verify: test PASSES
       ▼
    ┌────────┐
    │REFACTOR│ Clean up (keep green)
    └──┬─────┘
       │
       ▼
    Next cycle
```

## The Process

### 1. RED - Write Failing Test

Write ONE minimal test showing expected behavior.

**Good test structure:**
```typescript
// tests/journeys/ratio-compliance.spec.ts
test('AC1-HP: calculates staff-to-child ratio correctly', async ({ page }) => {
  // Given: 2 staff signed in, 10 children signed in
  await loginAs(page, 'staff')
  await setupTestData({ staff: 2, children: 10, ageGroup: 'toddler' })

  // When: viewing ratio dashboard
  await page.goto('/app/ratio')

  // Then: shows 1:5 ratio (10 children / 2 staff)
  await expect(page.getByTestId('ratio-display')).toContainText('1:5')

  // And: database reflects correct calculation
  const snapshot = await assertRecordExists('ratio_snapshots', {
    facility_id: testFacilityId
  })
  expect(snapshot.ratio).toBe(5)
})
```

**Requirements:**
- One behavior per test
- Clear name (matches acceptance criterion)
- Real assertions (UI + database)
- No mocks unless absolutely necessary

### 2. Verify RED - Watch It Fail

**MANDATORY. Never skip.**

```bash
pnpm test tests/journeys/ratio-compliance.spec.ts
```

Confirm:
- Test **fails** (not errors)
- Failure message is expected
- Fails because feature is **missing** (not typos)

**Test passes immediately?** You're testing existing behavior. Write different test.

**Test errors?** Fix error, re-run until it fails correctly.

### 3. GREEN - Write Minimal Code

Write the **simplest** code to make the test pass.

```typescript
// Good: Just enough to pass
async function calculateRatio(facilityId: string) {
  const staff = await getSignedInStaff(facilityId)
  const children = await getSignedInChildren(facilityId)
  return children.length / staff.length
}

// Bad: Over-engineered
async function calculateRatio(facilityId: string, options?: {
  ageGroup?: string
  timeWindow?: number
  includeBreakStaff?: boolean
}) {
  // YAGNI - not requested by test
}
```

**Don't:**
- Add features beyond what test requires
- Refactor other code
- "Improve" beyond the test

### 4. Verify GREEN - Watch It Pass

**MANDATORY.**

```bash
pnpm test tests/journeys/ratio-compliance.spec.ts
```

Confirm:
- Test passes
- Other tests still pass
- No errors or warnings

**Test still fails?** Fix implementation, not test.

### 4b. Verify Database State (when applicable)

After tests pass (Green), verify the actual data store:

- Query the database directly (not through the API being tested)
- Confirm rows were created/updated/deleted as expected
- Check constraints, indexes, and referential integrity
- UI/API response success alone is **INSUFFICIENT** — the DB is the source of truth

```typescript
// Example: verify database after API test passes
const record = await db.query('SELECT * FROM staff WHERE facility_id = $1', [facilityId])
expect(record.rows).toHaveLength(1)
expect(record.rows[0].status).toBe('active')
```

### 5. REFACTOR - Clean Up

**Only after green.** Keep tests passing.

- Remove duplication
- Improve names
- Extract helpers

Don't add behavior.

### 6. Commit

```bash
git add .
git commit -m "feat(ratio): calculate staff-to-child ratio"
```

## Acceptance Criteria as Tests

Each acceptance criterion from `/blueprint` becomes a test:

| Criterion | Test Name |
|-----------|-----------|
| AC1-HP: Happy path | `test('AC1-HP: happy path behavior')` |
| AC2-ERR: Error state | `test('AC2-ERR: handles invalid input')` |
| AC3-UI: UI updates | `test('AC3-UI: updates display on change')` |
| AC4-EDGE: Edge case | `test('AC4-EDGE: handles boundary condition')` |

## Bug Fix Pattern

**Bug:** Empty email accepted

```typescript
// 1. RED: Write test that fails
test('AC2-ERR: rejects empty email', async ({ page }) => {
  await page.fill('[name="email"]', '')
  await page.click('button[type="submit"]')
  await expect(page.getByRole('alert')).toContainText('Email required')
})

// 2. Verify RED: Run test, confirm it fails
// $ pnpm test -> FAIL: expected 'Email required'

// 3. GREEN: Fix the bug
function validateEmail(email: string) {
  if (!email?.trim()) {
    return { error: 'Email required' }
  }
  // ...
}

// 4. Verify GREEN: Run test, confirm it passes
// $ pnpm test -> PASS

// 5. REFACTOR: If needed

// 6. Commit
```

## Common Rationalizations

| Excuse | Reality |
|--------|---------|
| "Too simple to test" | Simple code breaks. Test takes 30 seconds. |
| "I'll test after" | Tests passing immediately prove nothing. |
| "Already manually tested" | Ad-hoc != systematic. No record, can't re-run. |
| "Need to explore first" | Fine. Delete exploration, start with TDD. |
| "Test hard = code is fine" | Test hard = design problem. Simplify. |
| "TDD slows me down" | TDD faster than debugging. |

## Red Flags - STOP and Start Over

If you catch yourself:
- Writing code before test
- Test passing immediately
- Can't explain why test failed
- Rationalizing "just this once"
- "Already manually tested it"
- "Keep as reference" (delete means delete)

**Delete the code. Start over with TDD.**

## Integration with Abilities

In `/do` workflow:
```
Read task from phase file
       │
       ▼
Load /tdd skill
       │
       ├── RED: Write test for AC
       ├── Verify: Watch fail
       ├── GREEN: Implement minimal
       ├── Verify: Watch pass
       ├── REFACTOR: Clean up
       └── Commit
       │
       ▼
Load /subagent-review
       │
       ▼
Next task
```

## Verification Checklist

Before marking task complete:

- [ ] Test written BEFORE implementation
- [ ] Watched test fail for correct reason
- [ ] Wrote minimal code to pass
- [ ] All tests passing
- [ ] Output clean (no errors, warnings)
- [ ] Tests use real code (minimal mocks)
- [ ] Edge cases covered
- [ ] Committed with descriptive message

Can't check all boxes? You skipped TDD. Start over.

## The Bottom Line

```
Production code -> test exists and failed first
Otherwise -> not TDD
```

No exceptions.
