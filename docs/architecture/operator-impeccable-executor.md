# Operator-managed `/impeccable` executor

This document covers the per-task executor mechanism that operator gained on
2026-05-04. It explains why per-task routing exists,
how the three-tier resolver decides which subagent runs each task, and where
the trust boundary sits between operator (which we own) and `/impeccable`
(which we treat as a subprocess).

## The problem

`/impeccable craft` produces measurably better frontend output than `/do`
or `/tdd`. It applies design-system awareness, banned-pattern detection,
and visual judgment that the generic TDD agent does not. But invoking
`/impeccable` directly in place of canonical owner skills broke target's
gate machinery: there were no `do-{sid}.md` artifacts, no `phase_transition`
events, and the session could not terminate cleanly.

The naive fix - have `/impeccable` write its own gate artifacts - leaks
the trust boundary. `/impeccable` is a third-party skill; its internals
can change without notice. We do not want gate enforcement coupled to
its output format or its execution shape.

## The design

`/do waves` gains a per-task executor resolver. Frontend-tagged tasks
dispatch a new sonnet subagent (`frontend-executor`) that drives the
`/impeccable craft` + `/impeccable critique` loop with score-based
convergence. Backend and infra tasks continue to use archer. The change
is purely additive on the dispatch path.

`/do waves` writes the canonical `do-{sid}.md` gate artifact at wave end,
aggregating per-task scratchpad notes from the inner loop. `/review sigma`
remains the canonical owner of `quality_check_passed`. Critique findings
flow to sigma's input scratchpad as advisory data, never as gate-passing
evidence.

## Three-tier resolution

Each task is routed via a resolver chain (highest to lowest priority):

1. Explicit `executor:` on the task block in the phase file.
2. Explicit `executor:` on the plan frontmatter (00-INDEX.md or quick-plan).
3. Surface inference from the task's file list.
4. `do` (default - dispatches archer).

The implementation is `skills/do/scripts/resolve-executor.sh`, an
env-var-driven shim that operator calls once per task. The shim normalizes
aliases (`tdd` -> `do`) before validating against `KNOWN_EXECUTORS`, so
adding a new alias only requires updating `normalize_alias` - the validator
list stays canonical.

Unknown executor names log a `WARN` and fall closed to `do`. A typo in plan
frontmatter cannot silently route to the wrong subagent; the worst case is
that a misspelled `impecable` falls through to archer with a stderr line
explaining why.

## Surface inference list

When neither the task nor the plan declares an executor, surface inference
runs. A file is treated as a frontend surface if it matches any of:

- `**/*.tsx` or `**/*.jsx`
- `components/**` or `**/components/**`
- `routes/**` or `**/routes/**`
- `src/styles/**` or `**/src/styles/**`

The root and nested forms are listed explicitly so a project with a
top-level `components/` directory matches the same as one with
`packages/ui/components/`. Monorepo workspaces are covered.

The list is locked by the original plan (locked decision
#2). Changing it requires plan revision.

`app/**` is intentionally NOT a directory match. Many backend projects use
`app/` as a Python/Go/Rust module root (`app/main.py`, `app/models/user.py`)
and would misroute to `frontend-executor` if the directory matched
unconditionally. Next.js App Router files are still caught correctly via
the `.tsx`/`.jsx` arms regardless of directory.

The implementation is `scripts/lib/infer-task-executor.sh`. It is bash 3.2
compatible (macOS default) and reads file paths from stdin.

## The frontend-executor inner loop

When operator routes a task to `frontend-executor`, the agent runs:

```
craft -> critique -> score parse -> next-subcommand parse -> loop
```

Termination conditions:

- **Convergence**: score crosses `config.executors.impeccable.critique_threshold`
  (default 35/40). Loop exits SUCCESS.
- **Ceiling**: iteration count reaches `max_iterations_per_task`
  (default 8). Loop exits FAILED with `reason=max_iterations_reached`.
- **Subprocess failure**: `/impeccable` non-zero exit. Loop exits FAILED
  with `rc=N` and a stderr tail in the ERROR string.

Parser fallbacks:

- Score regex no match: log `WARN`, treat as `score=0`. Loop continues
  unless ceiling fires.
- Next-subcommand regex no match: log `WARN`, default to `craft`.

The agent prompt lives at `agents/frontend-executor.md`. A mechanical
shell port (`skills/do/scripts/run-critique-loop.sh`) is the
testable reference implementation; tests in
`tests/operator/test_critique_loop.sh` exercise it. Drift between the
agent prompt and the shell port is detected by grep-based contract
checks in the same test file.

## Trust boundary

`/impeccable` is treated as a subprocess. `frontend-executor` parses its
output and decides whether to loop, but does not write gate artifacts or
modify target-state. Operator owns the canonical `do-{sid}.md`.
`/review sigma` owns `quality_check_passed`.

This separation means `/impeccable`'s output format can change without
breaking gate enforcement. The parser uses conservative regex with
warn-and-continue fallbacks; the max-iter ceiling guarantees termination.

The split also lets `/review sigma` consume critique findings as advisory
input without granting them gate-passing authority. Critique flags a P3
border-radius mismatch; sigma decides whether that blocks the merge.

## Wave-end aggregation

After all tasks in a wave return SUCCESS, operator writes the canonical
gate artifact with the standard fields plus impeccable aggregation when
any task ran via `frontend-executor`:

```yaml
session_id: ${SESSION_ID}
phase: do
agents_dispatched: [archer, frontend-executor]
files_changed: [...]
per_task_scores: [38, 36]
iterations_total: 7
deferred_findings:
  - task_id: 1.4
    finding: "border-radius mismatch with design token"
```

Tasks that ran via archer are not included in `per_task_scores`. The
field is omitted entirely when no impeccable tasks ran.

## Configuration surface

Project-local settings (`.fno/settings.yaml`) under
`config.executors.impeccable`:

| Key | Default | Effect |
|-----|---------|--------|
| `critique_threshold` | `35` | Score gate (out of 40). Loop exits SUCCESS at or above this. |
| `max_iterations_per_task` | `8` | Per-task ceiling. Loop exits FAILED at this iteration. |
| `auto_route_frontend` | `true` | When false, surface inference is disabled; tasks without explicit executor default to `do`. |

The `auto_route_frontend` falsey gate accepts the common YAML spellings
(`false`, `False`, `FALSE`, `0`, `no`, `No`) since YAML readers stringify
booleans inconsistently.

## Files

| Path | Owner | Purpose |
|------|-------|---------|
| `skills/do/references/waves.md` | this plan | Section 3 dispatcher + section 3c gate-artifact write |
| `skills/do/references/executor-resolution.md` | this plan | Resolver chain reference |
| `skills/do/scripts/resolve-executor.sh` | this plan | env-var-driven resolver shim |
| `skills/do/scripts/run-critique-loop.sh` | this plan | testable shell port of agent loop |
| `agents/frontend-executor.md` | this plan | sonnet subagent definition |
| `scripts/lib/infer-task-executor.sh` | this plan | surface inference helper |
| `tests/operator/test_executor_resolution.sh` | this plan | three-tier resolver tests |
| `tests/operator/test_surface_inference.sh` | this plan | locked pattern tests |
| `tests/operator/test_critique_loop.sh` | this plan | convergence + drift detection |

## Out of scope

- Modifying `/impeccable`'s internals. The integration treats it as a
  subprocess.
- Replacing `/tdd`. Backend and infra tasks continue to use archer.
- Critique-as-gate-owner. Locked decision #3.
- Cross-project frontend executor. Cross-project pipeline keeps its own
  dispatcher per `references/cross-project.md`.
