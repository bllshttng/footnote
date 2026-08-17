# Quick Plan Template

Single flat markdown file for bugs, focused features, and single-session work. One `.md` == one PR == one node; no folder, no `00-INDEX.md`, no phase files. Frontmatter is **mandatory** on every plan (quick or not): `status`, `kind`, `consolidation`, `kill_criteria` always ride in it - the markdown-heading form of kill_criteria is invisible to the stamp/validate parser and is not used.

---

```markdown
---
status: ready
kind: quick-plan
# claims: ab-XXXXXXXX             # Only when the input was an ab-id. When set,
#                                 # `fno backlog intake` updates that idea-state
#                                 # node in place instead of creating a duplicate.
#                                 # /blueprint writes this automatically; do not
#                                 # hand-author it except to repair a dangling
#                                 # node. See SKILL.md "Plan Claims Ingestion".
# executor: do                    # Plan-level executor (default 'do' = archer / TDD).
#                                 # Transcribed from a /think Locked Decision when
#                                 # one records executor routing (do | impeccable |
#                                 # mixed). Omit to let runtime surface inference
#                                 # choose per task. See docs/guides/per-task-executors.md.
# depends_on:                     # Graph edges wired at auto-adopt time
#   - ../2026-04-19-sibling-slug  # sibling plan (resolved against graph.plan_path)
#   - ab-d359579e                 # or an existing graph node ID
# consolidation: the step 2d Consolidation Gate's recorded outcome. Exactly one
#                                 # of absorb | append | proceed_alone, judged on the
#                                 # 2b receipt's graph.duplicates + graph.closure with
#                                 # the node details and code in hand. MANDATORY:
#                                 # validate-plan.sh errors on a missing, empty, or
#                                 # out-of-enum block. See SKILL.md step 2d.
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []      # ids considered and rejected, each with a reason:
  #  - id: x-0000
  #    reason: <why it is not the same work, in text a later reader can check>
  # outcome: absorb - the other node is a wave of THIS deliverable. After
  # intake run: fno backlog supersede <this-node> --replaces <id> --reason "..."
  # absorbed:
  #   - id: x-0000
  #     reason: <why this node is a wave of this deliverable>
  # reversal: fno backlog unsupersede x-0000
  # outcome: append - THIS content belongs on the OTHER node; no plan is
  # written and the content goes via fno backlog update <id> --details ...
  # appended_to:
  #   - id: x-0000
  #     reason: <why this content belongs on that node>
# kill_criteria: abort conditions target/do evaluate at wave + iteration boundaries.
# Emit these defaults unless the plan overrides them (see SKILL.md "Kill Criteria
# Declaration"). They live HERE in frontmatter, never under a `## Kill Criteria` heading.
kill_criteria:
  - name: iteration_ceiling
    predicate: iteration > 15
    reason: "Too many iterations - planning likely wrong"
  - name: stuck_test
    predicate: same_test_failing_for >= 3
    reason: "Same test failing 3+ iterations - root cause unclear"
# Stamp fields (populated by /target ship gate - do not fill manually):
# shipped_at: <UTC ISO8601>
# urls: []
# session_ids: []
---

# [Title — descriptive, not generic]

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

**Length:** 50-100 lines. A larger multi-wave feature still stays one `.md` - use the design-doc mutation path (`/blueprint <design-doc>` after `/think`), which appends an `## Execution Strategy` waves block. Drop `quick` for the fuller section set on an idea input.

**Changes:** Number them. Each change should target 1-3 files. If a single change touches 5+ files, break it into smaller changes. Each change gets 1-2 BDD acceptance criteria (happy path + primary error case) in the `**Acceptance:**` field.

**Verification:** Every step must be concrete and runnable. Not "check that it works" but "run this command, expect this output."

**Self-contained:** A fresh-context agent should be able to implement this plan without reading the conversation that produced it.
