# Quick Plan Template

Single flat markdown file for bugs, focused features, and single-session work. One `.md` == one PR == one node, with no folder, no `00-INDEX.md`, and no phase files. Frontmatter is **mandatory** on every plan, quick or not. `status`, `kind`, `consolidation`, and `kill_criteria` always ride in it. The markdown-heading form of kill_criteria is invisible to the stamp/validate parser and is not used.

---

```markdown
---
status: ready
kind: quick-plan
created: <YYYY-MM-DD>            # Required. The consolidation gate reads it to
#                                 # tell a new plan from a pre-gate one. Omit it
#                                 # and the gate falls back to a YYYY-MM-DD in
#                                 # the filename, then REFUSES the plan if
#                                 # neither carries a date.
difficulty: <low | medium | high> # Required for plans created after 2026-08-26.
#                                 # An intrinsic property of the work: expected
#                                 # time, edge cases, unknowns, and what a
#                                 # senior tech lead makes of the problem. Not
#                                 # a model or capacity hint.
# claims: ab-XXXXXXXX             # Only when the input was an ab-id. When set,
#                                 # `fno backlog intake` updates that idea-state
#                                 # node in place instead of creating a duplicate.
#                                 # /blueprint writes this automatically; do not
#                                 # hand-author it except to repair a dangling
#                                 # node. See SKILL.md "Plan Claims Ingestion".
# executor: tdd                   # Plan-level executor (default 'tdd' = archer / TDD).
#                                 # Transcribed from a /think Locked Decision when
#                                 # one records executor routing (tdd | impeccable |
#                                 # mixed; do is a one-release alias). Omit to let runtime surface inference
#                                 # choose per task. See docs/guides/per-task-executors.md.
# dispatch_hold:                  # Optional manual hold. Presence blocks every
#   reason: <why work must stop>   # dispatcher and merger until this whole block
#   release_when: <evidence>       # is removed by a plan author. review_on only
#   review_on: <YYYY-MM-DD>        # prompts review; it never auto-releases.
#   set_by: <person-or-agent>      # Required attribution for safe ownership/lift.
# depends_on:                     # Graph edges wired at auto-adopt time
#   - ../2026-04-19-sibling-slug  # sibling plan (resolved against graph.plan_path)
#   - ab-d359579e                 # or an existing graph node ID
# consolidation: the step 2d Consolidation Gate's recorded outcome. Exactly one
#                                 # of absorb | append | proceed_alone, judged on the
#                                 # 2b receipt's graph.duplicates + graph.closure with
#                                 # the node details and code in hand. MANDATORY:
#                                 # validate-plan.sh errors on a missing, empty, or
#                                 # out-of-enum block. See SKILL.md step 2d.
#                                 # decisions_acknowledged is separate from the outcome
#                                 # above: one entry per row in graph.decisions, each
#                                 # naming why that ruling does not close this work.
#                                 # Empty is legal only when graph.decisions is empty.
consolidation:
  outcome: proceed_alone
  proceed_alone_against: []      # ids considered and rejected, each with a reason:
  #  - id: x-0000
  #    reason: <why it is not the same work, in text a later reader can check>
  decisions_acknowledged: []     # one per graph.decisions row:
  #  - decision_id: d-00000000
  #    reason: <why this ruling does not close this work>
  # outcome: absorb - the other node is a wave of THIS deliverable. After
  # intake run: fno backlog supersede <this-node> --replaces <id> --cause "..." --surface <path>
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

## Execution Strategy

```yaml
execution_mode: sequential
waves:
  - wave: 1
    mode: parallel                 # load-bearing; see Guidelines below
    name: [what this plan delivers]
    difficulty: <low | medium | high>   # the frontmatter band unless this wave differs
    tasks: ['1.1', '1.2', '1.3']   # one id per numbered change, in order
tasks:
  - id: '1.1'
    title: [change 1's heading, verbatim]
    surface: ['path/to/file.ts']   # change 1's **Files:** list, verbatim
    verify: [the runnable check from ## Verification that covers change 1]
    acceptance:
      - [change 1's first **Acceptance:** line]
      - [change 1's error-case line]
  - id: '1.2'
    title: [change 2's heading, verbatim]
    surface: ['path/to/other.ts']
    verify: [the runnable check that covers change 2]
    acceptance:
      - [change 2's happy-path line]
      - [change 2's error-case line]
```
```

---

## Guidelines

**Length:** 50-100 lines. A larger multi-wave feature still stays one `.md` - use the design-doc mutation path (`/blueprint <design-doc>` after `/think`), which builds its `## Execution Strategy` from the design. Drop `quick` for the fuller section set on an idea input.

**Execution Strategy:** Emit one. Transcribe the numbered changes rather than re-deriving them, because both inputs the rule needs are already written above.

Each `### N.` change becomes one task. Its `**Files:**` line becomes that task's `surface`. All tasks go into ONE wave with `mode: parallel`.

`mode: parallel` is load-bearing, not cosmetic. The collision partition runs only for parallel waves, and it is the only mode that reads the same-file case correctly. Three tasks naming one file measure 1 under `parallel` and 3 under `sequential`, which over-reports.

Declare NO per-task `blocked_by` here. A declared blocker wins outright over wave inheritance, and a reflex chain forecloses the parallelism the wave just declared.

A genuinely single-change plan emits one task and measures width 1. Join still refuses it, now for a reason the file states rather than a flag it was handed.

**Changes:** Number them. Each change should target 1-3 files. If a single change touches 5+ files, break it into smaller changes. Each change gets 1-2 BDD acceptance criteria (happy path + primary error case) in the `**Acceptance:**` field.

**Per-task dependencies:** A task row in `## Execution Strategy` can declare `blocked_by: ['1.1']`. The list must use known task ids and contain no cycles. An explicit empty list leaves the task unblocked. A task without this key inherits every task from the previous wave, preserving whole-wave scheduling.

**Per-wave difficulty:** Each wave in `## Execution Strategy` carries `difficulty: low|medium|high` (post-gate plans must stamp every wave, and the validator refuses an out-of-enum value). The band routes the wave's work: a pulling worker takes only tasks at or below its own band. The band-to-harness-and-model mapping stays in config, never in the plan. Seed from the plan's frontmatter band, then revise each wave that differs in depth.

**Verification:** Every step must be concrete and runnable. Not "check that it works" but "run this command, expect this output."

**Self-contained:** A fresh-context agent should be able to implement this plan without reading the conversation that produced it.
