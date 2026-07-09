# Per-task Executor Resolution

Operator routes each task to a subagent based on a three-tier resolver. This
doc describes the chain, the locked surface inference list, override paths,
and failure modes.

## Resolution chain

For each task, in order, highest priority first:

1. **Explicit `executor:` on the task block** — `executor: impeccable` directly
   on the task block in the plan. Always wins.
2. **Explicit `executor:` on the plan frontmatter** — `executor: impeccable`
   in the plan's frontmatter. Applies to all tasks
   that don't set their own executor.
3. **Surface inference** — runs only when neither task nor plan declared an
   executor. Reads the task's file list and matches against the locked
   inference list (below). Match → `impeccable`. No match → falls through.
4. **Default** — `do` (TDD-disciplined archer).

## Locked inference list

A file is treated as a frontend surface if it matches any of:

- `**/*.tsx` or `**/*.jsx`
- `components/**` or `**/components/**`
- `routes/**` or `**/routes/**`
- `src/styles/**` or `**/src/styles/**`

These are locked by plan 2026-05-04-operator-impeccable-executor (locked
decision #2). Changing the list requires plan revision. The list is
implemented once in the in-package module `fno.executor._surface`, which
exposes `is_frontend_surface_path()` and `any_frontend_surface()` plus a CLI
(`python3 -m fno.executor._surface` echoes `impeccable`/`do`; `--has-ui`
echoes `true`/`false`). The `has_ui` changeset inference
(`scripts/lib/infer-has-ui.sh`) delegates to that module's `--has-ui` mode,
so executor routing and `has_ui` can never drift from one copy of the
patterns.

**Why `app/**` is not in the list:** `app/` is a common Python/Go/Rust
module-root convention. `app/main.py`, `app/models/user.py`, and similar
backend files would misroute to frontend-executor if `app/**` matched
unconditionally. Next.js App Router files (`app/page.tsx`,
`app/layout.tsx`) are still routed correctly via the `*.tsx`/`*.jsx`
arms regardless of directory.

## Recognized executors

| Executor | Subagent | Notes |
|----------|----------|-------|
| `do` | `archer` | Default. TDD-disciplined task implementation. |
| `tdd` | `archer` | Alias for `do`. |
| `impeccable` | `frontend-executor` | `/impeccable craft + critique` loop with score-based convergence. |
| `research` | `scout` | Retrieve + store: ddgs backbone -> self-fetch -> `sources.jsonl`. Reached via `fno research "X"` (a research-pipeline alias over `target`), NOT via `/do waves` surface inference - the surface resolver below only emits `do`/`impeccable`. The `doc` deliverable terminal + verify profile + eval are Group 2. |

## Override paths

To override surface inference for a single task, add `executor:` to the task
block:

```markdown
### 1.4 Backend migration that touches src/components/

**Files:** scripts/migrations/2026-05-add-component-table.sql
executor: do  # override: this is migration code, not frontend
```

Or override at the plan level by adding `executor: do` to the plan
frontmatter to disable inference for the entire plan.

To disable surface inference globally for the project, set in
`.fno/config.toml`:

```yaml
config:
  executors:
    impeccable:
      auto_route_frontend: false
```

When disabled, tasks without an explicit executor default to `do` regardless
of their file paths.

## Failure modes

- **Unknown executor name** (anything other than `do`, `tdd`, `impeccable`):
  the resolver logs a `WARN` and falls through to `do`. AC1.5-FR cites this
  as the canonical fail-closed behavior. The intent is that a typo or a
  removed-but-still-referenced executor name does not silently route to a
  wrong subagent.
- **Inference module unavailable** (`fno.executor._surface` not importable):
  the resolver produces an empty value and falls closed to `do` via the
  `is_known_executor` check. Surface inference matters only when neither task
  nor plan declared an executor; an explicit declaration bypasses it.
- **Empty file list with no explicit executor**: defaults to `do`.

## Examples

```bash
# AC1.1-HP: explicit task wins
PLAN_EXEC="do" TASK_EXEC="impeccable" \
    bash skills/do/scripts/resolve-executor.sh
# -> impeccable

# AC1.1-FR: plan wins over inference
PLAN_EXEC="impeccable" TASK_EXEC="" TASK_FILES="src/foo.py" \
    bash skills/do/scripts/resolve-executor.sh
# -> impeccable

# AC1.1-EDGE: inference fires when nothing explicit
PLAN_EXEC="" TASK_EXEC="" TASK_FILES="src/components/Foo.tsx" \
    bash skills/do/scripts/resolve-executor.sh
# -> impeccable

# AC1.5-FR: unknown falls closed
PLAN_EXEC="" TASK_EXEC="nonsense" \
    bash skills/do/scripts/resolve-executor.sh
# -> do (with WARN on stderr)
```

## Iteration ceiling: single-budget contract for impeccable

`config.executors.impeccable.max_iterations_per_task` (default 8) is a
**single shared budget** across the entire /impeccable stage loop
(shape -> craft -> critique -> polish -> harden -> audit -> ...). It is
NOT per-stage. The budget is total, not multiplied across stages.

Concretely:
- `/do waves` passes one `max_iterations_per_task` value to `frontend-executor`
  at dispatch.
- `frontend-executor` maintains a single `iterations_used` counter that
  increments on every stage invocation (not per-attempt within a stage).
- When `iterations_used >= max_iterations_per_task`, the loop exits.

The canonical exit when the ceiling trips is the **two-tier verdict**
(decision 5a of the frontend-executor-pipeline-awareness brief):

| Score | Verdict |
|-------|---------|
| >= `critique_target` (default 35) | `RESULT: SUCCESS` |
| < `critique_floor` (default 25) | `RESULT: FAILED` |
| Between floor and target | `RESULT: DONE_WITH_CONCERNS` |

`DONE_WITH_CONCERNS` is **not** a hard `FAILED` reflex. The score at
ceiling determines which tier applies. Critique findings that did not
converge are written to `deferred_findings` in the gate artifact
(`approved: false`) for sigma-review and human triage at PR time.

Per-stage ceilings are explicitly NOT introduced. Introducing them would
multiply the effective budget (8 per stage times N stages), defeating the
invariant. Any code or doc that introduces a per-stage counter is a
deviation from this locked contract.

## Trust boundary

`/impeccable` is treated as a subprocess. The `frontend-executor` agent
parses `/impeccable critique` output and decides whether to loop, but
operator owns the canonical gate-artifact write (`do-{sid}.md`) at wave
end. If `/impeccable`'s output format changes, the parser falls back to
`score=0` (treated as another iteration) and `next-subcommand=craft`. The
max-iter ceiling guarantees termination.

`/review` remains the canonical owner of the `quality_check_passed`
gate (locked decision #3). Critique findings flow to sigma's input
scratchpad as advisory data only - they never satisfy the quality gate
on their own.
