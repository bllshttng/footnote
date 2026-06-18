# Write Tests

Generate Playwright E2E test files from BDD acceptance criteria, following existing codebase patterns.

## Input Sources (check in order)

1. **Explicit criteria** - User pastes Given/When/Then criteria
2. **Plan folder** - Read from `internal/web/plans/*/acceptance-criteria.md`
3. **PR diff** - Analyze changed files to infer what needs testing

## Process

### 1. Discover Test Patterns

Before writing any tests, analyze existing patterns:

```bash
# Find existing E2E tests
find . -path "*/e2e/*.spec.ts" -o -path "*/tests/*.spec.ts" | head -10

# Read 2-3 representative tests to understand patterns
```

**Extract patterns for:**
- File naming convention (e.g., `feature.spec.ts`, `feature.e2e.spec.ts`)
- Test structure (describe/test nesting)
- Page object usage
- Selectors strategy (data-testid, role, text)
- Setup/teardown patterns
- Authentication handling
- API mocking approach

### 2. Map Criteria to Tests

For each acceptance criterion:

```gherkin
Given I am on the attendance page
When I scan a QR code for "Emma Johnson"
Then I see a success message with the child's name
```

Maps to:

```typescript
test('shows success message when scanning valid QR code', async ({ page }) => {
  // Given - Setup
  await page.goto('/attendance');

  // When - Action
  await page.getByTestId('qr-scanner').fill('EMMA_QR_CODE');
  await page.getByRole('button', { name: 'Submit' }).click();

  // Then - Assertion
  await expect(page.getByText('Emma Johnson checked in')).toBeVisible();
});
```

### 3. Generate Test File

**Structure:**

```typescript
import { test, expect } from '@playwright/test';
// Import page objects if pattern exists
// Import test utilities if pattern exists

test.describe('Feature Name', () => {
  test.beforeEach(async ({ page }) => {
    // Common setup from criteria "Given" clauses
  });

  test('criterion 1 description', async ({ page }) => {
    // Given (if unique to this test)
    // When
    // Then
  });

  test('criterion 2 description', async ({ page }) => {
    // ...
  });
});
```

### 4. Output Location

Follow project conventions:

| Pattern | Location |
|---------|----------|
| Colocated | `src/features/attendance/__tests__/attendance.spec.ts` |
| Centralized | `tests/e2e/attendance.spec.ts` |
| App Router | `app/attendance/__tests__/page.spec.ts` |

**If unsure, ask user.**

## Test Quality Checklist

Before outputting tests, verify:

- [ ] Each criterion has exactly one test
- [ ] Test names describe the behavior, not implementation
- [ ] Selectors use recommended strategy (data-testid > role > text > CSS)
- [ ] No hardcoded waits (use `waitFor`, `toBeVisible`, etc.)
- [ ] Setup is minimal and clear
- [ ] Assertions are specific and meaningful
- [ ] Tests are independent (no shared state)

## Handling Missing Information

**If criteria are vague:**
```gherkin
Given I am logged in
When I click the button
Then it works
```

Ask for specifics:
- Which user role?
- Which button (name, location)?
- What does "works" mean (visible element, API call, navigation)?

**If no existing tests to reference:**
Follow Playwright best practices:
- Use `getByRole()` for accessibility
- Use `getByTestId()` for specific elements
- Use `expect().toBeVisible()` over `toHaveCount(1)`

## Example Output

**Input criteria:**
```gherkin
Feature: Child Check-in

Scenario: Successful check-in via QR code
  Given I am a staff member on the attendance page
  When I scan a valid QR code for child "Emma Johnson"
  Then I see "Emma Johnson" marked as checked in
  And the check-in time is recorded

Scenario: Invalid QR code handling
  Given I am a staff member on the attendance page
  When I scan an expired QR code
  Then I see an error message "QR code has expired"
  And the child is not checked in
```

**Output test file:**
```typescript
import { test, expect } from '@playwright/test';
import { loginAsStaff } from '../utils/auth';

test.describe('Child Check-in', () => {
  test.beforeEach(async ({ page }) => {
    await loginAsStaff(page);
    await page.goto('/attendance');
  });

  test('successful check-in via QR code shows child as checked in', async ({ page }) => {
    // When - scan valid QR code
    await page.getByTestId('qr-input').fill('EMMA_VALID_QR');
    await page.getByRole('button', { name: 'Check In' }).click();

    // Then - child marked as checked in with timestamp
    const childRow = page.getByRole('row', { name: /Emma Johnson/ });
    await expect(childRow.getByText('Checked In')).toBeVisible();
    await expect(childRow.getByTestId('check-in-time')).not.toBeEmpty();
  });

  test('expired QR code shows error and does not check in child', async ({ page }) => {
    // When - scan expired QR code
    await page.getByTestId('qr-input').fill('EXPIRED_QR');
    await page.getByRole('button', { name: 'Check In' }).click();

    // Then - error shown, child not checked in
    await expect(page.getByRole('alert')).toContainText('QR code has expired');
    await expect(page.getByRole('row', { name: /Emma Johnson/ }).getByText('Checked In')).not.toBeVisible();
  });
});
```

## Integration with Abilities Workflow

```
tdd/references/bdd-acceptance-criteria  ->  Generates criteria
              |
tdd/references/write-tests              ->  Generates test files (YOU ARE HERE)
              |
tdd/references/browser-testing          ->  Runs tests in browser
```

## Red Flags

**Never:**
- Write tests without reading existing patterns first
- Use `page.waitForTimeout()` - use proper assertions
- Test implementation details (internal state, private functions)
- Write flaky selectors (nth-child, complex CSS)
- Skip the Given/When/Then mapping

**Always:**
- Match existing codebase conventions
- One assertion focus per test (multiple expects OK if related)
- Use descriptive test names from criteria
- Consider test data setup (fixtures, mocks, seeds)
