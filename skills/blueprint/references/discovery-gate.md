# Discovery Gate Protocol

Surface what you don't know before planning or designing. Models default to
making assumptions and moving forward - this protocol forces unknowns into
the open where they can be answered (by the user) or made explicit (by the
model in autonomous mode).

## When to Run

- After reading codebase/docs but BEFORE designing or planning
- Skipped when a plan already exists (input_type: plan)
- Skipped for Small size (-S) in target (too lightweight for ceremony)
- Skipped if /think already produced a Discovery section

## The Protocol

1. Read the codebase context (codemap, existing code, docs)
2. Enumerate 3-5 unknowns as a numbered list of targeted questions
3. Each question must be specific and actionable
4. Questions should cover these categories:

| Category | Example |
|----------|---------|
| Architectural ambiguity | "The auth middleware uses cookies - should this feature use the same pattern or JWT since it's a CLI tool?" |
| Scope boundaries | "Should the export include archived records or only active ones?" |
| Integration points | "The notification service uses a queue - should this feature push to the same queue or create a new channel?" |
| Constraints | "The table has 2M rows - does the new query need pagination or is full-table scan acceptable?" |

### Question Quality

**Good questions** (specific, grounded in what you read):
- "The `User` model has both `role` and `permissions[]` - which controls access to this feature?"
- "There are two auth patterns: middleware-based (routes/) and inline (api/) - which should this endpoint use?"
- "The existing tests mock Supabase but the plan mentions migration testing - should I use a real database?"

**Bad questions** (vague, could be asked about any project):
- "What do you want this to do?"
- "Are there any constraints I should know about?"
- "How should errors be handled?"

## Modes

### Interactive (default)

Present questions to the user via AskUserQuestion. Wait for answers.
Use answers to inform the next phase (design or planning).

Format:
```
I found a few things I'm uncertain about after reading the codebase:

1. [Question with brief context explaining why it matters]
2. [Question with brief context]
3. [Question with brief context]

These will help me [design/plan] more accurately.
```

### Self-Answer (autonomous / -M in target)

When human interaction isn't available, the model answers its own questions
from the context it has read. The key difference from silently assuming:
the assumptions are written down explicitly and preserved for audit.

1. Generate the same 3-5 questions
2. Answer each from available context (code, docs, patterns)
3. Mark confidence: HIGH (clear from code), MEDIUM (inferred from patterns),
   LOW (best guess, needs human review)
4. Write to the spec/plan as an `## Assumptions` section

Format:
```markdown
## Assumptions

These were self-answered during autonomous discovery. Review for accuracy.

1. **Q:** [Question]
   **A:** [Answer] (confidence: HIGH - based on existing pattern in `src/auth/middleware.ts`)

2. **Q:** [Question]
   **A:** [Answer] (confidence: LOW - no clear precedent, assuming based on conventions)
```

LOW confidence assumptions should be flagged in the plan's risk section.

## Skip Conditions

Skip the discovery gate if any of these are true:

| Condition | Reason |
|-----------|--------|
| `input_type: plan` | Plan already exists - unknowns were (should have been) resolved |
| Size `-S` | Small tasks don't justify discovery ceremony |
| /think already ran with Discovery section | Questions were already answered |
| Pure greenfield with no codebase | No code to discover unknowns from |

Detection for "think already ran": check if the scratchpad has
`think-findings.md` with a `## Discovery` or `## Assumptions` section.

## Schema Reconciliation (DB-backed repos)

When the repo is database-backed, a second grounding step runs alongside
discovery: reconcile the idea against the real schema and the backlog before
committing to a design or plan. The duplicate-or-not decision is made here, so
it must happen at design time, with schema in hand.

The first phase to run authors it: `/think` when it runs, `/blueprint` when
think was skipped (`/target path/to/plan`, `/blueprint quick`, Plan Mode
backfill). The hand-off is a top-level `## Schema Reconciliation` section,
grepped exactly like the `## Discovery` skip above so the later phase reuses it
instead of regenerating.

1. **Generate the schema-aware codemap.** `fno codemap --db-schema` appends a
   `## Database Schema` section. The connection is discovered from the shell
   `DATABASE_URL` or a dev `.env` file (`.env.production` / `.env.staging` are
   never auto-connected); with no reachable DB it parses migration files for
   tables and keys. The connection string is never echoed.
2. **Dedup against the backlog.** Run `fno backlog find "<feature keywords>"`
   (title tokens plus the one-line summary) to surface an existing node that
   already covers this ground. This is plan-level dedup ("are we already
   planning this"), not semantic code-level dedup.
3. **Write the verdict.** Record the touched tables/enums, an explicit dedup
   verdict ("no existing capability covers this" vs "overlaps with ab-XXXX -
   narrow scope or supersede"), and any shaping constraint under the
   `## Schema Reconciliation` heading. A DB-backed repo whose feature touches
   no tables records "no schema surface" and proceeds without a citation.

`/blueprint` then enforces a graduated citation gate: a DB-touching plan that
never names a real table/enum/constraint fails closed (full / large) or warns
(quick / small). See the Schema Citation Gate in `skills/blueprint/SKILL.md`.
