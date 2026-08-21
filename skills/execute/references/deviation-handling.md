# Deviation Handling

During execution, you may encounter work outside the original plan. Handle deviations by type:

| Type | Action | Commit Prefix |
|------|--------|---------------|
| **Bug/Security** | Auto-fix immediately | `fix:` |
| **Missing Validation** | Auto-add, note in commit | `fix:` |
| **Architectural Change** | STOP - Ask user first | - |
| **Scope Expansion** | REJECT - Stay focused | - |

## Deviation Rules

**Rule 1: Auto-fix bugs**
If you discover broken code, logic errors, or security issues while implementing:
- Fix them immediately
- Create separate commit: `fix(<scope>): <description>`
- Note in task completion report

**Rule 2: Auto-add critical functionality**
If you discover missing error handling, validation, or auth checks:
- Add them as part of current task
- Note as "deviation fix" in commit body

**Rule 3: Auto-fix blocking issues**
If you discover missing dependencies, broken imports, or build errors:
- Fix them first
- Create separate commit before continuing

**Rule 4: Escalate architectural changes**
If you realize the plan requires:
- New database tables/schemas
- New service layers
- Fundamental pattern changes
- Breaking API changes

**STOP and ask user:**
```
Architectural Deviation Detected

The current task requires: [what you discovered]

Options:
1. Proceed with architectural change (explain impact)
2. Simplify implementation to avoid change
3. Pause and revise plan

Which approach should I take?
```
