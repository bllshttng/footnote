# Common Acceptance Criteria Patterns

Quick reference for frequently needed BDD criteria.

## Authentication

```gherkin
# Login success
Given I am on the login page
And I enter valid credentials
When I click Sign In
Then I am redirected to dashboard
And my session is active

# Login failure
Given I am on the login page
When I enter invalid credentials
Then I see "Invalid email or password"
And I remain on login page

# Session expiry
Given my session has expired
When I try to access a protected page
Then I am redirected to login
And I see "Session expired, please log in again"
```

## Forms

```gherkin
# Required field validation
Given I am on the form
When I submit without filling [required field]
Then I see "[field] is required" error
And form does not submit

# Email validation
Given I am on the form
When I enter invalid email format
Then I see "Please enter a valid email"

# Phone formatting
Given I am entering a phone number
When I type "3105551234"
Then the input displays "(310) 555-1234"
And the stored value is "+13105551234"

# Form persistence on error
Given I have filled multiple fields
When I submit with one invalid field
Then valid fields retain their values
And only invalid field shows error
```

## Lists & Tables

```gherkin
# Empty state
Given no [items] exist
When I view the [list] page
Then I see "No [items] yet"
And I see "Add [item]" button

# Pagination
Given more than [N] items exist
When I view the list
Then I see [N] items per page
And pagination controls appear

# Sorting
Given multiple items exist
When I click sort by [column]
Then items reorder by [column]
And sort indicator shows direction

# Filtering
Given items with different [attribute] exist
When I filter by [attribute]
Then only matching items show
And count updates to reflect filter
```

## Modals & Dialogs

```gherkin
# Confirmation dialog
Given I click delete on [item]
When the confirmation dialog appears
Then I see "Are you sure?" message
And I can cancel or confirm

# Cancel closes modal
Given a modal is open
When I click Cancel or press Escape
Then the modal closes
And no action is taken

# Modal form submission
Given I am in the [action] modal
When I fill the form and submit
Then the modal closes
And the list updates with new data
```

## Real-time Updates

```gherkin
# Optimistic update
Given I perform [action]
When the action starts
Then UI updates immediately
And shows loading indicator

# Rollback on error
Given I perform [action]
When the server returns error
Then the optimistic update reverts
And I see error message

# Stale data refresh
Given another user updated [item]
When I view the [item]
Then I see the latest data
```

## Permissions

```gherkin
# Authorized action
Given I have [permission]
When I try to [action]
Then the action succeeds

# Unauthorized action
Given I do NOT have [permission]
When I try to [action]
Then I see "You don't have permission"
And action is blocked

# Role-based UI
Given I am logged in as [role]
When I view [page]
Then I see only features available to [role]
And restricted features are hidden
```

## Data Integrity

```gherkin
# Referential integrity
Given [parent] has [children]
When I try to delete [parent]
Then I see "Cannot delete: has associated [children]"
And [parent] remains

# Unique constraint
Given [item] with [unique_field] exists
When I create another with same [unique_field]
Then I see "Already exists" error
And no duplicate created

# Soft delete
Given I delete [item]
When I check the database
Then [item] has is_active = false
And [item] no longer appears in UI
```

## Notifications & Feedback

```gherkin
# Success toast
Given I complete [action] successfully
When the action completes
Then I see success toast "[message]"
And toast auto-dismisses after 5 seconds

# Error toast
Given [action] fails
When I receive the error
Then I see error toast with message
And I can dismiss it manually

# Loading states
Given I start [async action]
When the action is processing
Then I see loading indicator
And UI is not interactive
When action completes
Then loading indicator disappears
And UI becomes interactive
```

## Failure Recovery & Silent Failures

```gherkin
# Button recovery after API error
Given I click [action button]
When the server returns a 500 error
Then I see an error message describing the failure
And the [button] returns to its idle state
And the [button] is clickable again (not stuck disabled/loading)

# Form preservation after failed submit
Given I have filled out [form] with data
When I click submit and the server returns an error
Then my form data is still present in all fields
And I see an error message explaining what went wrong
And I can fix the issue and resubmit without re-entering data

# Double-click prevention
Given I click [submit/action button]
When I click it again before the first request completes
Then only one request is sent to the server
And the button shows a loading/disabled state after first click

# Optimistic update rollback
Given I perform [action] that updates the UI optimistically
When the server rejects the change
Then the UI reverts to the previous state
And I see an error message explaining the rejection

# Navigation during async operation
Given I start [async action] (save, delete, upload)
When I navigate away before it completes
Then either the action completes successfully in background
Or I see a warning "You have unsaved changes"
And no partial/orphaned state remains in the database

# Timeout handling
Given I click [action button]
When the server does not respond within [timeout] seconds
Then I see a timeout error message
And the button returns to idle state
And I can retry the action

# Session expiry during action
Given my session expires
When I try to perform [action]
Then I see "Session expired" message
And I am redirected to login
And after re-login I can resume where I left off (or my data is preserved)

# Silent failure detection (meta-pattern)
Given I perform ANY user-initiated action
Then at least one of these MUST be true:
  - A success indicator appears (toast, redirect, UI update)
  - An error indicator appears (toast, inline error, modal)
  - A loading indicator appears (spinner, skeleton, disabled state)
  - The action is intentionally silent AND documented as such
```
