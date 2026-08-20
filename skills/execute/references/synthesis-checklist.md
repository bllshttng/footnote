# Synthesis Checklist

Quick-reference for the orchestrator's pre-dispatch validation.
Loaded by operator SKILL.md Section 3a.

## The Five-Point Check

Every Task tool prompt MUST contain:

1. **Specific file path** - At least one absolute or repo-relative path
   - Code: `src/auth/validate.ts`
   - Infra: `docker/Dockerfile.prod`
   - Data: `supabase/migrations/20260401_add_table.sql`

2. **Structural location** - Where in the file the change goes
   - Code: line number, function name, class method
   - Infra: Dockerfile stage, docker-compose service name
   - Config: YAML key path, JSON field path

3. **Current state** - What the code/config currently does (proves you read it)
   - "Currently returns undefined when session expires"
   - "Currently has no validation on the request body"
   - "Currently missing the responses table"

4. **Target state** - The exact change, not a description of the goal
   - "Add `if (!session.user) return null;` before line 43"
   - "Add Zod schema: `z.object({ name: z.string() })`"
   - "CREATE TABLE with columns: id, facility_id, response_text"

5. **Reasoning or pattern source** - Why this approach, from the codebase
   - "Matches the pattern in src/routes/users.ts:34"
   - "Required by the Session type contract in src/types/auth.ts"
   - "Follows existing migration naming in supabase/migrations/"

## Prompt Quality Signals

### Green (dispatch immediately)
- Prompt reads like a code review comment with a fix attached
- A developer unfamiliar with the project could execute it
- File paths are verified to exist (you ran `ls` or `Read`)

### Yellow (review before dispatch)
- Prompt references "the plan" without restating the relevant detail
- File paths are from the plan but not verified against current code
- Change description uses words like "appropriate", "necessary", "relevant"

### Red (do NOT dispatch - synthesize first)
- Prompt contains "based on your findings" or "as described in"
- Prompt says "fix the bug" without specifying which bug, where, or how
- Prompt references a task number without restating the task content
- Prompt uses "implement" as a verb without specifying what to write

## Anti-Delegation Rules (ENFORCED)

| NEVER write this | ALWAYS write this instead |
|------------------|--------------------------|
| "Based on the plan findings, implement task 2.1" | "In `src/auth/validate.ts:42`, add a null check for `session.user` before accessing `user.id`. The Session type allows undefined user when tokens are cached past expiry." |
| "The plan says to add validation. Please add it." | "Add Zod schema validation to `POST /api/facilities` in `src/routes/facilities.ts:78`. Schema: `{ name: z.string().min(1).max(100), address: z.string(), capacity: z.number().int().min(1).max(500) }`. Pattern: see `src/routes/users.ts:34`." |
| "Fix the bug mentioned in AC2-ERR" | "Fix: `calculateTotal()` in `src/cart/utils.ts:23` returns NaN when `items` array is empty because `reduce()` has no initial value. Add `0` as the second argument to `reduce()`." |
| "Implement the database migration from Phase 1" | "Create migration `supabase/migrations/20260401_add_responses.sql` with: `CREATE TABLE facility_responses (id uuid DEFAULT gen_random_uuid(), facility_id uuid REFERENCES facilities(id), response_text text NOT NULL, created_at timestamptz DEFAULT now());`" |

## Pre-Dispatch Checklist

Before EVERY `Task` tool invocation, verify your prompt passes this checklist:

- [ ] Contains at least one specific file path (not "the auth module")
- [ ] Contains a line number, function name, or structural location (not "somewhere in the file")
- [ ] Describes what the current code does (proves you read it)
- [ ] Describes the exact change needed (not "fix it" or "add validation")
- [ ] Includes the reasoning or pattern source (not "because the plan says so")

If your prompt fails any checkbox, you have not synthesized - you are delegating. Stop and read the code first.

## Why This Matters

Subagents run in fresh context with no memory of prior phases. They cannot look up "your findings" or "the plan's recommendations." Every piece of understanding the orchestrator fails to synthesize is understanding the worker must rediscover from scratch - burning tokens, risking wrong conclusions, and often producing worse results than if the orchestrator had just done the synthesis work upfront.

## Constraint Injection (MANDATORY)

Before spawning each subagent, append project constraints to its prompt:

1. Call `orchestrator.get_project_constraints_section()`
2. If non-empty, append to the subagent's task description
3. This ensures agents operate within project boundaries (e.g., "solo founder" prevents over-engineering)

```python
from orchestrator import get_project_constraints_section
constraints = get_project_constraints_section()
# Append to task prompt: task_description + constraints
```

If no constraints exist (empty string returned), skip - no error, no empty section.
