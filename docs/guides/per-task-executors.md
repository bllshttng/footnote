# Per-task executors

A short guide for plan authors: how to declare an executor on a task,
when surface inference takes over, and what the available executors are.

## When you would care about this

You are writing a plan and one of the following is true:

- The plan touches frontend files (.tsx/.jsx, components, routes, styles)
  and you want `/impeccable`'s design judgment applied during
  implementation.
- The plan is mixed-surface (some frontend tasks, some backend) and you
  want different executors per task.
- Surface inference is misclassifying a task and you want to override it.

If your plan is pure backend or infra, you can skip this guide. The
default executor is `do` (TDD-disciplined archer), which is what plans
have always used.

## Available executors

| Executor | Subagent | When to choose it |
|----------|----------|------------------|
| `do` | archer | Default. TDD-disciplined backend, infra, scripts, configs. |
| `tdd` | archer | Alias for `do`. Same behavior. |
| `impeccable` | frontend-executor | Frontend tasks where design quality matters. |

## Declaring at the task level (highest priority)

Add `executor:` to a single task block. Wins over plan frontmatter and
inference:

```markdown
### 1.4 Add login form

**File:** src/components/Login.tsx
executor: impeccable

The login form needs to match the design system's token radii and the
critique loop will catch focus-state contrast issues that archer misses.
```

Use this when one task in an otherwise backend plan needs the design
treatment, or when one task in an otherwise frontend plan needs to skip it
(e.g., a config tweak that happens to live under `src/styles/`).

## Declaring at the plan level

Add `executor:` to the plan's `00-INDEX.md` frontmatter (or to a
quick-plan file's frontmatter):

```yaml
---
title: Add user dashboard
created: 2026-05-04
executor: impeccable
size: M
---
```

This applies to every task in the plan that does not declare its own
executor. Surface inference is bypassed when a plan-level executor is
set.

Use this when the entire plan is frontend work and you want to be
explicit (so a reader of the plan doesn't have to reason about
inference).

## When surface inference takes over

If neither the task nor the plan declares an executor, operator infers
from the task's file list. A task is treated as a frontend surface (and
routed to `frontend-executor`) if any of its files match:

- `**/*.tsx` or `**/*.jsx`
- `components/**` or `**/components/**`
- `routes/**` or `**/routes/**`
- `src/styles/**` or `**/src/styles/**`

Otherwise the task routes to `do` (archer).

`app/` is intentionally NOT a directory match. Backend projects often
use `app/` as a Python/Go/Rust module root (`app/main.py`,
`app/models/user.py`); a directory match would silently route those to
the frontend executor. Next.js App Router files (`app/page.tsx`) still
match correctly via the `.tsx`/`.jsx` arms.

## Overriding inference for a single task

If inference picks the wrong executor for one of your tasks, override it:

```markdown
### 2.1 Migration: drop the old session table

**File:** scripts/migrations/2026-05-drop-sessions.sql
executor: do
```

Even if your plan happens to live alongside frontend tasks, the migration
is backend; declaring `executor: do` keeps it routed to archer.

## Disabling inference globally

To turn off surface inference for the project, set in
`.fno/config.toml`:

```yaml
config:
  executors:
    impeccable:
      auto_route_frontend: false
```

Tasks without an explicit executor will default to `do` regardless of
file paths. Use this in projects where the inference list is causing
more friction than help (e.g., a project whose `components/` directory
holds backend service definitions, not React components).

## Tuning the critique loop

The `impeccable` executor runs an inner loop that exits when the critique
score crosses a threshold or when iteration count reaches a ceiling. Both
are configurable per-project:

```yaml
config:
  executors:
    impeccable:
      # Score threshold (out of 40). Loop exits SUCCESS at or above.
      # Default 35.
      critique_threshold: 35

      # Per-task iteration ceiling. Loop exits FAILED at this iteration
      # with reason=max_iterations_reached. Default 8.
      max_iterations_per_task: 8
```

Higher threshold = stricter convergence, more iterations spent. Lower
threshold = faster, less polish. Most projects leave both at default.

## What you do NOT need to do

- You do not need to write gate artifacts. Operator owns those.
- You do not need to invoke `/impeccable` directly from your plan.
  Operator dispatches `frontend-executor` and `frontend-executor` invokes
  `/impeccable`.
- You do not need to handle critique findings. `/review` consumes
  them as advisory input automatically.

## Common scenarios

**Pure frontend plan**: declare `executor: impeccable` on the plan
frontmatter. Every task uses the design-aware executor.

**Pure backend plan**: do nothing. Default is `do` and surface inference
will not match anything.

**Mixed plan**: declare task-level `executor:` on the frontend tasks
and let everything else fall through to `do`.

**Inference is wrong**: add `executor:` to the offending task with the
correct value.

## Further reading

- Architecture: [operator-impeccable-executor.md](../architecture/operator-impeccable-executor.md)
- Resolver reference: [skills/do/references/executor-resolution.md](../../skills/do/references/executor-resolution.md)
- Agent definition: [agents/frontend-executor.md](../../agents/frontend-executor.md)
