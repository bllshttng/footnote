# Test Plan Template

# [Feature] Test Plan

**Feature:** [Name]
**Date:** YYYY-MM-DD
**Tester:** [Name]

---

## Environment Setup

- [ ] Clear browser cache/cookies
- [ ] Use incognito/private mode
- [ ] Test on: Chrome, Safari, Mobile Safari
- [ ] Test with: Real phone (not just DevTools)

---

## Happy Path

- [ ] [Step 1 description]
  - Expected: [What should happen]
- [ ] [Step 2 description]
  - Expected: [What should happen]

---

## Error States

- [ ] Submit form with invalid data
  - Expected: Clear error message, field highlighted
- [ ] Network failure during submission
  - Expected: Error toast, data not lost
- [ ] Server returns 500
  - Expected: User-friendly error, retry option

---

## Empty States

- [ ] View page with no data
  - Expected: Helpful empty state message, CTA
- [ ] Search with no results
  - Expected: "No results" message, suggestions

---

## Edge Cases

- [ ] Enter maximum length text (255 chars)
  - Expected: Text truncates gracefully or error shown
- [ ] Enter special characters: `<script>alert('xss')</script>`
  - Expected: Text displayed literally, no XSS
- [ ] Enter emoji: 👨‍👩‍👧‍👦
  - Expected: Emoji displays correctly
- [ ] Rapid double-click submit button
  - Expected: Only one submission, button disabled

---

## Mobile Testing

Use Sizzy or real device:

- [ ] Touch targets are at least 44x44px
- [ ] No horizontal scroll on mobile
- [ ] Keyboard doesn't cover input fields
- [ ] Swipe gestures work (if applicable)
- [ ] Portrait and landscape orientation

---

## Accessibility

- [ ] Tab through entire flow with keyboard only
  - Expected: Logical tab order, visible focus
- [ ] Use screen reader (VoiceOver on Mac)
  - Expected: All elements announced correctly
- [ ] Check color contrast (use DevTools)
  - Expected: WCAG AA minimum (4.5:1 for text)

---

## Permission Variations

- [ ] Test as Admin
- [ ] Test as Staff
- [ ] Test as Parent (if applicable)
- [ ] Test as unauthenticated user

---

## Notes

[Space for observations during testing]

---

## Issues Found

| Issue | Severity | Steps to Reproduce |
|-------|----------|-------------------|
| | | |
