# Linear Ticket Integration

## Automatic Creation (Step 6)

**Only if `config.linear.enabled: true` in settings.yaml.** Also skippable with `--no-linear` flag.

After writing the plan folder, create the Linear ticket if configured:

```bash
# Skip if --no-linear flag was passed
if [[ "$NO_LINEAR" != "true" ]]; then
  # Use /linear skill with --from-index
  /linear --from-index "$PLAN_DIR/00-INDEX.md"
fi
```

**What `/linear --from-index` does:**
1. Reads the 00-INDEX.md you just created
2. Extracts title, goal, architecture, phases
3. Creates Linear ticket with structured description
4. **Updates 00-INDEX.md frontmatter** with `linear: {TEAM}-XXX`

**The INDEX.md frontmatter will be updated to:**
```yaml
---
created: YYYY-MM-DDTHH:MM
linear: {TEAM}-XXX
linear_url: https://linear.app/{workspace}/issue/{TEAM}-XXX
---
```

**Manual fallback** (if automatic fails or for custom tickets):

```bash
linearis issues create "[Feature Name]" \
  --team {TEAM} \
  --description "## Overview
[Feature description]

## Plan
See: {plans_path}/YYYY-MM-DD-feature-name/00-INDEX.md

## Phases
- [ ] Phase 1: Database
- [ ] Phase 2: Core API
- [ ] Phase 3: UI
- [ ] Phase 4: Tests" \
  --labels "feature" \
  --priority 2
```

**Then manually update frontmatter with the returned ticket ID.**

## Ticket Linking Patterns

Plans link to Linear tickets for tracking. The relationship can be:

| Pattern | Use Case |
|---------|----------|
| 1 plan : 1 ticket | Standard feature development |
| N plans : 1 ticket | Large epic with multiple plan folders |
| 1 plan : N tickets | Plan broken into sub-tickets |

**Linking rules:**
1. **INDEX.md frontmatter** must include `linear: {TEAM}-XXX`
2. **Commit messages** reference ticket: `feat: add feature ({TEAM}-XXX)`
3. **PR description** links to ticket
4. **Linear ticket** links back to plan folder path

**If ticket already exists:**
```markdown
---
linear: {TEAM}-123  # Existing ticket
linear_url: https://linear.app/{workspace}/issue/{TEAM}-123
---
```

**If creating new ticket:**
Use `/linear` skill in Step 6, then update frontmatter with returned ID.
