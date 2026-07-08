# Abilities Test Plan

Create a manual QA test plan for edge cases that are hard to automate or require human judgment.

## Philosophy

Playwright tests cover the **happy paths** and **known edge cases**. But humans catch things automation misses:
- Visual glitches (alignment, overflow, color contrast)
- UX friction (confusing flows, missing feedback)
- Cross-device behavior (touch vs mouse, different screen sizes)
- Real-world data edge cases
- Accessibility issues (screen reader flow, keyboard navigation)

## Reference Materials

Load references as needed during test planning:

| Reference | Load When | Content |
|-----------|-----------|---------|
| [references/test-plan-template.md](references/test-plan-template.md) | Creating a full test plan for a feature | Complete checklist template (environment, happy path, errors, empty states, edge cases, mobile, accessibility, permissions) |
| [references/quick-templates.md](references/quick-templates.md) | Testing common UI patterns (forms, lists, modals) | Reusable checklists for form validation, list/table, and modal interactions |

## When to Use

- After `/do` completes and Playwright tests pass
- Before `/pr create` for significant features
- When feature involves complex UX flows
- When feature has many edge cases

## Process

### 1. Identify Test Categories

For each feature, consider:

| Category | What to Check |
|----------|---------------|
| **Happy Path** | Does the main flow work as expected? |
| **Error States** | Are errors shown clearly? Can user recover? |
| **Empty States** | What happens with no data? |
| **Loading States** | Is feedback shown during async operations? |
| **Edge Cases** | Boundary values, special characters, long text |
| **Permissions** | Does it work for all user roles? |
| **Mobile** | Touch targets, responsive layout, orientation |
| **Accessibility** | Keyboard nav, screen reader, color contrast |

### 2. Generate Test Plan

Load [references/test-plan-template.md](references/test-plan-template.md) for the full checklist template organized by category.

### 3. Save Test Plan

Save to: `{config.docs.test_plan_path}/YYYY-MM-DD-<feature>-test-plan.md` (read from config.toml, default: `docs/test-plans`)

### 4. Execute Testing

1. Open the feature in browser
2. Work through each checklist item
3. Mark items as checked or note issues
4. Use Sizzy for multi-device testing:
   ```bash
   open -a Sizzy "http://localhost:3000/app/feature"
   ```

### 5. Document Issues

For each issue found:
1. Note severity (Critical, High, Medium, Low)
2. Document steps to reproduce
3. Take screenshot if visual
4. Create Linear ticket if blocking (only if `config.linear.enabled`)

## Quick Templates

For common UI patterns (forms, lists/tables, modals), load [references/quick-templates.md](references/quick-templates.md) for reusable checklists.

## Tools

| Tool | Purpose | Command |
|------|---------|---------|
| **Sizzy** | Multi-device testing | `open -a Sizzy "http://localhost:3000"` |
| **DevTools** | Accessibility audit | Lighthouse > Accessibility |
| **VoiceOver** | Screen reader testing | `Cmd + F5` on Mac |
| **axe DevTools** | Accessibility checking | Chrome extension |

## Key Principles

- **Test like a user** - Not like a developer who knows the code
- **Test on real devices** - DevTools mobile simulation misses things
- **Test with real data** - Edge cases appear with production-like content
- **Document everything** - Screenshots and reproduction steps
- **Don't skip accessibility** - It's not optional
