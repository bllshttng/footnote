# BDD Acceptance Criteria

Transform user stories into testable acceptance criteria using Given/When/Then format.

## User Story Format

```
As a [user type]
I want [goal/action]
So that [value/benefit]
```

## Acceptance Criteria Format

For each user story, generate criteria covering:

### 1. Happy Path
```gherkin
Given [precondition - system state before action]
When [action - what the user does]
Then [outcome - what should happen]
```

### 2. Validation/Error States
```gherkin
Given [precondition]
When [invalid action or bad input]
Then [error message or prevention]
And [system state remains unchanged]
```

### 3. Edge Cases
```gherkin
Given [edge condition - empty list, max values, etc.]
When [action]
Then [graceful handling]
```

### 4. UI State Changes
```gherkin
Given [initial UI state]
When [action completes]
Then [UI updates to reflect change]
And [loading states clear]
And [success feedback shown]
```

## Common Patterns

### Form Submission
```gherkin
# Happy path
Given I am on the [form] page
And I have filled valid data
When I click Submit
Then the form submits successfully
And I see a success message
And the record exists in database

# Validation
Given I am on the [form] page
When I submit with empty required fields
Then I see validation errors
And the form does not submit

# Duplicate prevention
Given a record with [unique field] already exists
When I try to create another with same [unique field]
Then I see a duplicate error
And no new record is created
```

### List/CRUD Operations
```gherkin
# Create
Given I am on the [list] page
When I add a new [item]
Then the [item] appears in the list
And the list count increases by 1

# Update
Given [item] exists in the list
When I edit the [item]
Then the changes are saved
And the list shows updated data

# Delete
Given [item] exists in the list
When I delete the [item]
Then the [item] is removed
And the list count decreases by 1

# Delete prevention
Given [item] is the only [required thing]
When I try to delete it
Then I see an error "Cannot delete the only [thing]"
And the [item] remains
```

### Real-time Updates
```gherkin
Given I performed [action]
When the action completes
Then the UI updates immediately
And I do not need to refresh the page
```

## Playwright Test Stub Template

```typescript
// tests/journeys/<feature>.spec.ts
import { test, expect } from '@playwright/test'
import { loginAs } from '../helpers/auth'
import { assertRecordExists, assertRecordNotExists } from '../helpers/db'
import { seedTestData, cleanupTestData } from '../helpers/fixtures'

test.describe('<Feature> Journey', () => {
  let testData: Awaited<ReturnType<typeof seedTestData>>

  test.beforeAll(async () => {
    testData = await seedTestData()
  })

  test.afterAll(async () => {
    await cleanupTestData(testData)
  })

  test('AC1: Happy path - [description]', async ({ page }) => {
    // Given: [precondition]
    await loginAs(page, 'owner')
    await page.goto('/app/<route>')

    // When: [action]
    await page.getByRole('button', { name: '<action>' }).click()
    // ... fill form, interact

    // Then: [UI outcome]
    await expect(page.getByText('<success message>')).toBeVisible()

    // Then: [DB outcome - CRITICAL]
    const record = await assertRecordExists('<table>', {
      <field>: <expected_value>
    })
    expect(record.<field>).toBe(<expected>)
  })

  test('AC2: Error state - [description]', async ({ page }) => {
    // Given: [setup for error condition]
    await loginAs(page, 'owner')
    await page.goto('/app/<route>')

    // When: [invalid action]
    await page.getByRole('button', { name: '<action>' }).click()

    // Then: [error shown]
    await expect(page.getByRole('alert')).toContainText('<error message>')

    // Then: [state unchanged]
    await assertRecordNotExists('<table>', { <bad_condition> })
  })

  test('AC3: Edge case - [description]', async ({ page }) => {
    // Given: [edge condition setup]
    // When: [action]
    // Then: [graceful handling]
  })
})
```

## Reference

See [e2e-testing](~/.claude/skills/e2e-testing) skill for:
- Test helpers (auth, db, fixtures)
- Contract testing patterns
- Integration test patterns
- Journey test patterns
