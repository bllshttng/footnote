# Phase Handoff Artifacts

Each target phase writes a small structured artifact at the end of its work so the
next phase can consume it without reconstructing context from the full session.
The artifact lives at `.fno/artifacts/handoff/{phase}-{session_id}.md`.

## Size Invariant

Every artifact MUST stay under 500 tokens (roughly 2000 characters). The write
helper (`ph_write`) enforces this: if the payload exceeds the cap it truncates at
the nearest sentence boundary and appends:

```
# truncated at NNN tokens - context budget exceeded
```

If a phase cannot fit its context in 500 tokens, that is a signal the phase is
trying to hand off too much. Extract the essential decisions and defer details to
the body narrative.

## Omitting Unknown Fields

If a phase does not have a value for a structured field, it omits the field
entirely. Empty-string filler (`key: ""`) and null placeholders (`key: null`)
are forbidden - they waste token budget and confuse downstream consumers.

Example: a `do` phase that deferred no stories omits `stories_deferred` rather
than writing `stories_deferred: []`.

## Wrapper Format (Universal)

The artifact is YAML frontmatter + markdown body. A generic reader can always
extract the frontmatter without knowing the phase-specific schema.

```yaml
---
phase: <phase-name>
session_id: <session-id>
timestamp: <ISO-8601>
status: complete | partial | blocked
# Phase-specific structured fields (schema below)
---

# Phase <name> summary

Optional human-readable narrative (200 words max). Use this for context that is
hard to encode structurally: why a decision was made, what the next phase should
watch out for, tone of the codebase.
```

## Path Convention

```
.fno/artifacts/handoff/{phase}-{session_id}.md
```

The `handoff/` subdirectory namespaces these artifacts away from gate-attestation
artifacts at `.fno/artifacts/{phase}-{session_id}.md` (owned by sigma-review,
create-pr, check-pr, etc.).

Concurrent target runs in different worktrees use different `session_id` values so
artifact files never collide even when they touch the same project directory.

## Per-Phase Schemas

### think

```yaml
---
phase: think
session_id: f31156b3
timestamp: 2026-04-27T09:15:00Z
status: complete
design_docs_produced:
  - .fno/scratchpad/think-findings.md
key_decisions:
  - "Use Supabase RLS over custom middleware - simpler auth boundary"
  - "Single-table design for events; partitioning deferred until 10k rows/day"
open_questions:
  - "Does the mobile client need offline-first? Spec blocked on this."
---

# Think summary

Explored two architectures for the real-time notification layer. Settled on
SSE over WebSockets because the backend is stateless and we have no need for
bidirectional messaging in v1. The open question about offline-first will
block the plan phase if unresolved - flag to user.
```

### plan

```yaml
---
phase: plan
session_id: f31156b3
timestamp: 2026-04-27T10:02:00Z
status: complete
plan_path: .fno/plans/2026-04-27-notifications/
phases_planned: 3
expected_url_count: 1
scope_classification: feature
---

# Plan summary

Three-phase plan: schema migration (1 task), API layer (2 tasks), UI wiring
(1 task). Migration in wave 1 (sequential); API and UI in wave 2 (parallel).
Estimated 4-6 hours. No cross-project work needed.
```

### do

```yaml
---
phase: do
session_id: f31156b3
timestamp: 2026-04-27T14:32:00Z
status: complete
stories_completed: [1, 2, 3]
stories_deferred:
  - id: 4
    reason: "needs supabase migration - run fno backlog intake for that first"
    blocking: false
files_changed:
  - src/notifications/handler.ts
  - src/notifications/handler.test.ts
  - src/notifications/schema.sql
notes_for_next_phase: |
  Story 4 deferred. Clean phase should skip src/notifications/schema.sql -
  it uses raw SQL with intentional non-standard formatting.
---

# Do summary

Stories 1-3 shipped with full test coverage. Story 4 (push notification
integration) blocked on a Supabase migration that lives in a sibling plan.
Left a TODO in the code pointing to the blocking plan ID.
```

### clean

```yaml
---
phase: clean
session_id: f31156b3
timestamp: 2026-04-27T15:10:00Z
status: complete
files_simplified:
  - src/notifications/handler.ts
patterns_removed:
  - "redundant null checks (4 instances)"
  - "unused import: lodash/cloneDeep"
notes_for_review: |
  handler.ts had a deeply nested callback chain that was refactored to async/await.
  Logic is unchanged; test coverage confirms parity.
---
```

### review

```yaml
---
phase: review
session_id: f31156b3
timestamp: 2026-04-27T15:45:00Z
status: complete
sigma_review_artifact_path: .fno/artifacts/review-f31156b3.md
blocking_issues: []
advisory_notes:
  - "handler.ts line 87: consider extracting retry logic to shared util"
---
```

### validate

```yaml
---
phase: validate
session_id: f31156b3
timestamp: 2026-04-27T16:00:00Z
status: complete
build_command: "npm run build"
test_command: "npm test"
output_summary: "Build: 0 errors. Tests: 47 passed, 0 failed, 0 skipped."
exit_codes:
  build: 0
  test: 0
---
```

### ship

```yaml
---
phase: ship
session_id: f31156b3
timestamp: 2026-04-27T16:15:00Z
status: complete
pr_number: 182
pr_url: "https://github.com/org/repo/pull/182"
branch_name: "feature/notifications-f31156b3"
base_branch: main
---
```

### external

```yaml
---
phase: external
session_id: f31156b3
timestamp: 2026-04-27T17:30:00Z
status: complete
review_status: approved
blocking_comments: []
approval_state: approved
---
```

### docs

```yaml
---
phase: docs
session_id: f31156b3
timestamp: 2026-04-27T17:45:00Z
status: complete
docs_updated:
  - docs/architecture/notifications.md
sections_added:
  - "SSE connection lifecycle"
  - "Rate limiting behavior"
---
```

## Pipeline Ordering and Prior-Phase Mapping

The read-at-phase-start always reads the immediately preceding phase:

| Current phase | Reads prior artifact from |
|---------------|--------------------------|
| plan | think |
| do | plan |
| clean | do |
| review | clean |
| validate | review |
| ship | validate |
| external | ship |
| docs | external |

The `think` phase has no prior phase and skips the read step.

## Concurrency Safety

`ph_write` is atomic: it writes to a `.tmp` file then renames. A partial write
from a crash leaves a `.tmp` orphan that the next run ignores.

`ph_write` refuses to overwrite an existing artifact for the same phase+session
pair. If the artifact already exists, the helper returns non-zero and prints a
warning to stderr. This invariant catches a phase that ran twice (e.g., a
restart that re-fired a phase that already wrote its artifact).
