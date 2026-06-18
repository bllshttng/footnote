# Domain Checklists

Injected into `.fno/CONTEXT.md` by the orchestrator before spawning archer.
The orchestrator selects the right checklist based on task tags/keywords.

## Frontend Checklist

```markdown
## Domain: Frontend

### Pre-Flight
- Dev server running (check localhost)
- Test runner available (pnpm test / vitest / playwright)

### Verification (Step 5)
- Component renders correctly
- Interactions work (click, type, submit)
- Accessibility: semantic HTML, ARIA labels, keyboard nav
- Responsive: mobile (375px), tablet (768px), desktop (1920px)
- UI updates after mutations (no manual refresh needed)

### Testing Context
If settings.yaml exists, load `testing.{project}.auth` for login shortcuts
and `testing.{project}.gotchas` for project-specific reminders.
```

## Backend Checklist

```markdown
## Domain: Backend

### Pre-Flight
- Database accessible (psql $DATABASE_URL -c "SELECT 1")
- Test runner available

### Verification (Step 5)
- Query the database and assert row state after mutations
- API response status AND database side-effects both verified
- Input validation rejects malformed data (Zod/schema level)
- Auth/RBAC: verify unauthorized requests are rejected

### Acceptance Criteria
Replace AC-VERIFY with AC-DB (Data Integrity) for backend tasks.
```

## DevOps Checklist

```markdown
## Domain: DevOps / Infrastructure

### Pre-Flight
- Docker available (if container task)
- Cloud CLI authenticated (aws/gcloud/az)
- Terraform/IaC tool available (if infra task)

### Verification (Step 5)
- Container actually builds and runs (not just Dockerfile created)
- CI pipeline passes (not just workflow file created)
- Deployment succeeds in target environment
- Rollback path exists and works

### Acceptance Criteria
Replace AC-VERIFY with AC-DEPLOY for infra tasks.
Add AC-ROLLBACK where applicable.
```

## Data Engineering Checklist

```markdown
## Domain: Data Engineering

### Pre-Flight
- Python environment ready (python --version)
- Data libraries available (pandas/polars/etc.)
- Database accessible (if pipeline writes to DB)

### Verification (Step 5)
- Output schema matches expected columns and types
- No nulls in required fields
- Value ranges within expected bounds
- Referential integrity maintained
- Edge cases: empty input, malformed records, encoding issues

### Acceptance Criteria
Replace AC-VERIFY with AC-DQ (Data Quality) for data tasks.

### Testing Pattern
```python
def test_data_quality():
    result = run_pipeline(test_input)
    assert set(result.columns) == {"id", "name", "value"}
    assert result["id"].notna().all()
    assert (result["value"] >= 0).all()
```
```

## How the Orchestrator Uses This

```python
# In orchestrator, before spawning archer:
domain = determine_domain(task)  # frontend|backend|devops|data|general

if domain != "general":
    checklist = load_checklist(domain)
    write_to(".fno/CONTEXT.md", checklist + user_constraints)

# Spawn archer — it reads CONTEXT.md as part of startup protocol
spawn_agent("archer", task_prompt)
```
