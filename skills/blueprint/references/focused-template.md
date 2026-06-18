# Focused Plan Template

Single flat markdown file for bugs, focused features, and single-session work. No YAML execution strategy, no folder structure. Frontmatter is optional — add it only when you need auto-adopt to wire dependency edges on the graph.

---

```markdown
---
# Optional frontmatter. Omit entirely if you don't need any field.
# claims: ab-XXXXXXXX             # Optional. When set, `fno backlog intake`
#                                 # updates this idea-state node in place
#                                 # instead of creating a duplicate. /blueprint
#                                 # writes this automatically when the input
#                                 # was an ab-id; do not hand-author it
#                                 # except to repair a prior dangling node.
#                                 # See skills/blueprint/SKILL.md "Plan Claims
#                                 # Ingestion" for the full contract.
# Per-plan executor lock - transcribed from /think's Locked Decision when one
# was recorded. Acceptable values: do | impeccable | mixed. Omit to let the
# runtime surface inference choose per task. /blueprint writes this automatically
# when it sees a `## Locked Decisions` entry containing executor routing; do
# not hand-author it unless you know the design doc disagrees. See:
#   skills/think/references/executor-routing-prompt.md
# executor: do
# depends_on:                     # Graph edges wired at auto-adopt time
#   - ../2026-04-19-sibling-slug  # sibling plan (resolved against graph.plan_path)
#   - ab-d359579e                 # or an existing graph node ID
# executor: do                    # Plan-level executor (default 'do' = archer / TDD).
#                                 # Set to 'impeccable' for design-aware frontend work
#                                 # (frontend-executor + /impeccable craft+critique loop).
#                                 # See docs/guides/per-task-executors.md.
# Stamp fields (populated by /target ship gate - do not fill manually):
# status: shipped | done
# shipped_at: <UTC ISO8601>
# urls: []
# session_ids: []
---

# [Title — descriptive, not generic]

## Kill Criteria

Conditions that abort the run cleanly (emits `<aborted reason="...">`,
stop hook exits, ledger records the reason). Omit this section entirely
to inherit engine defaults (`iteration_ceiling: 15`, `stuck_test: 3`).

```yaml
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
```

## Context

[Problem statement in 2-5 sentences. Include:
- What's broken or what needs to be built
- Root cause if this is a bug
- How it was discovered or why it matters now
- Any relevant constraints or prior decisions]

## Changes

### 1. [Short descriptive name]

**Files:** `path/to/file.ts` (lines 45-60 if known)

[What to change and why. Be specific enough that a fresh-context agent can implement without asking questions. Include code snippets when the change is non-obvious.]

```ts
// Before
const result = fetchAll()

// After
const result = fetchByFacility(facilityId)
```

**Acceptance:**
- Given [precondition], when [action], then [expected result]
- Given [error condition], when [action], then [error is handled]

### 2. [Short descriptive name]

**Files:** `path/to/other.ts`

[What to change and why.]

**Acceptance:**
- Given [precondition], when [action], then [expected result]
- Given [error condition], when [action], then [error is handled]

### 3. [Continue as needed]

...

## Files to Modify

| File | Action |
|------|--------|
| `path/to/file.ts` | Modify — refactor query to scope by facility |
| `path/to/new.ts` | Create — reusable violation link component |
| `path/to/test.ts` | Modify — add test for empty cart edge case |

## Patterns to Reuse

_Optional — omit this section entirely if no relevant patterns exist._

| Pattern | Source |
|---------|--------|
| Facility-scoped query | `src/server/app/ro-comparison.ts:334-355` |
| Server function structure | `src/server/validateFacility.ts` |

## Verification

1. `npx tsc --noEmit` — type check passes
2. `pnpm test src/server/file.test.ts` — tests pass
3. Navigate to /page → verify [specific behavior]
4. Check database: `SELECT ... FROM table WHERE ...` → [expected result]
```

---

## Guidelines

**Length:** 50-100 lines. If your plan exceeds 150 lines, consider using the default full mode (`/blueprint` without `quick`) instead.

**Changes:** Number them. Each change should target 1-3 files. If a single change touches 5+ files, break it into smaller changes. Each change gets 1-2 BDD acceptance criteria (happy path + primary error case) in the `**Acceptance:**` field.

**Verification:** Every step must be concrete and runnable. Not "check that it works" but "run this command, expect this output."

**Self-contained:** A fresh-context agent should be able to implement this plan without reading the conversation that produced it.
