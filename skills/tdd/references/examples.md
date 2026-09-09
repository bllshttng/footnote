# TDD worked examples

Load only when you want concrete shapes for the red-green-refactor contract in SKILL.md. Language-agnostic rule, TypeScript specimens.

## Good test structure

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

Requirements: one behavior per test; a clear name matching the acceptance criterion; real assertions; no mocks unless absolutely necessary. Database and UI assertions are scoped to changes that actually have those surfaces - a pure-function change asserts on the function, not on `page` objects.

## GREEN means minimal

```typescript
// Good: just enough to pass
async function calculateRatio(facilityId: string) {
  const staff = await getSignedInStaff(facilityId)
  const children = await getSignedInChildren(facilityId)
  return children.length / staff.length
}

// Bad: over-engineered - options the test never asked for
async function calculateRatio(facilityId: string, options?: {
  ageGroup?: string
  timeWindow?: number
  includeBreakStaff?: boolean
}) {
}
```

## Bug-fix pattern

Bug: empty email accepted.

```typescript
// 1. RED: test that fails
test('AC2-ERR: rejects empty email', async ({ page }) => {
  await page.fill('[name="email"]', '')
  await page.click('button[type="submit"]')
  await expect(page.getByRole('alert')).toContainText('Email required')
})

// 2. Verify RED: run it, confirm it fails for the right reason

// 3. GREEN: fix the bug
function validateEmail(email: string) {
  if (!email?.trim()) {
    return { error: 'Email required' }
  }
  // ...
}

// 4. Verify GREEN: run it, confirm it passes
// 5. REFACTOR if needed; 6. commit
```

## Rationalizations, answered

| Excuse | Reality |
|--------|---------|
| "Too simple to test" | Simple code breaks. Test takes 30 seconds. |
| "I'll test after" | Tests passing immediately prove nothing. |
| "Already manually tested" | Ad-hoc != systematic. No record, can't re-run. |
| "Need to explore first" | Fine. Delete exploration, start with TDD. |
| "Test hard = code is fine" | Test hard = design problem. Simplify. |
| "TDD slows me down" | TDD faster than debugging. |
