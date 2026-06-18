# Quick Templates

Reusable checklists for common UI patterns.

## Form Testing

```markdown
## Form: [Name]

### Validation
- [ ] Required fields show error when empty
- [ ] Email field validates format
- [ ] Phone field accepts various formats
- [ ] Date field accepts valid dates only

### Submission
- [ ] Success shows confirmation
- [ ] Error shows message and preserves input
- [ ] Double-submit prevented
- [ ] Loading state shown during submit

### Accessibility
- [ ] Labels associated with inputs
- [ ] Error messages announced by screen reader
- [ ] Form can be submitted with Enter key
```

## List/Table Testing

```markdown
## List: [Name]

### Display
- [ ] Empty state when no items
- [ ] Pagination works (if applicable)
- [ ] Sort works (if applicable)
- [ ] Filter works (if applicable)

### Items
- [ ] Long text truncates with ellipsis
- [ ] Actions (edit/delete) work
- [ ] Item click navigates correctly

### Performance
- [ ] Loads quickly with 100+ items
- [ ] Scroll is smooth
```

## Modal Testing

```markdown
## Modal: [Name]

### Opening
- [ ] Opens on trigger click
- [ ] Focus moves to modal
- [ ] Background scroll locked

### Interaction
- [ ] Can close with X button
- [ ] Can close with Escape key
- [ ] Can close by clicking backdrop (if allowed)
- [ ] Form inside modal works

### Closing
- [ ] Focus returns to trigger
- [ ] No data loss on accidental close
```
