# Frontend-executor: full /impeccable pipeline awareness

This document covers the plan (2026-05-06) that extended
`frontend-executor` from a craft+critique stub into a complete /impeccable
pipeline executor. Read `docs/architecture/operator-impeccable-executor.md`
first if you are new to the per-task executor mechanism and the three-tier
resolver; this document assumes that context and covers only what changed.

## What changed

An earlier change shipped `frontend-executor` as a craft+critique loop
with score-based convergence. The agent drove two subcommands, skipped the
shape gate by synthesizing no brief, and exited when the score crossed
`critique_threshold` (35/40) or the iteration ceiling fired. It never ran
harden, audit, polish, or any visual treatment.

A later change then taught /think and /blueprint to lock `executor: impeccable` as a
design decision recorded in the design doc. That lock implied more than the
agent actually delivered: /impeccable spans 23 subcommands across 6 categories,
has a setup gate (PRODUCT.md must exist), and a shape-confirmation gate (shape
brief must be approved before craft runs). A user who chose `executor:
impeccable` via /think expected the full pipeline.

This change closes that gap. The agent now:

- Synthesizes a shape brief at dispatch time from the /think design doc and the
  per-task AC list (no manual shape-brief step required in autonomous mode).
- Selects /impeccable stages per task content using a default rule, or honors an
  explicit `impeccable_stages: [...]` pin written by /blueprint or /think.
- Runs production-readiness passes: harden closes every frontend task; audit
  runs when the AC list mentions a11y, WCAG, screen reader, performance, Core
  Web Vitals, or responsive.
- Classifies every critique or harden finding into one of three buckets and
  routes accordingly, rather than deferring all non-convergence to FAILED.
- Applies a two-tier exit verdict (SUCCESS / DONE_WITH_CONCERNS / FAILED) so
  near-miss results are surfaced to humans rather than silently treated as
  failures.

## The 9 locked decisions

Nine architectural decisions were locked before the plan was written. Each
decision is summarized here.

**Decision 1 - Shape brief in autonomous mode.** Treating the /think design doc
plus the per-task AC list as the user-confirmed shape brief. The user's approval
during /think counts as shape confirmation; re-asking at execution time is
friction with no information gain. The agent synthesizes the brief at dispatch
time by extracting the `## Goal` section, any visual-tone sections, and the full
AC list, then formats them into /impeccable's expected brief shape. If the loader
rejects the synthesized brief, the agent emits `<help
reason="shape-adapter-needs-canonical-schema">` and exits rather than forging
shape=pass. The gate artifact records `shape_source: think_design_doc` so PR
reviewers can trace what the agent treated as the brief.

**Decision 2 - Stage selection.** The agent picks stages by default from task
content using the rule described in the dispatch envelope section below. Per-task
`impeccable_stages: [...]` overrides the default when present. The default rule
keeps aesthetic and visual treatment decisions with the human; all pin-only
treatments (animate, bolder, colorize, delight, overdrive, quieter, typeset)
require an explicit list written by /think or /blueprint.

**Decision 3 - Audit vs sigma-review boundary.** No overlap between the two
gates. Sigma-review covers code quality: logic, security, types, tests,
defensive programming. Audit covers design quality: a11y, performance,
responsive, visual consistency. Both run when their triggers are met; both gate
independently. Audit findings do not route through sigma-review's
`quality_check_passed` artifact; they produce their own gate signal owned by
frontend-executor's gate accounting.

**Decision 4 - PRODUCT.md prerequisite.** Two-layer defense in depth. /blueprint
warns at plan-creation time if PRODUCT.md is absent from the project root,
`.agents/context/`, or `docs/`. The warning is advisory; the plan still ships.
The operator re-checks at dispatch time and emits `<help
reason="missing-product-md">` if PRODUCT.md is missing or stale (under 200
chars, matching /impeccable's loader contract). The operator's check is the
actual gate; /blueprint's warning is the heads-up for users still in planning mode.
DESIGN.md gets a softer treatment: /blueprint adds a `prerequisites_optional:` note
but operator does not gate on it.

**Decision 5 - Visual treatments.** The default rule may pick `layout` as a
polish-adjacent stage when the AC list mentions spacing, rhythm, hierarchy,
alignment, or density, or when a prior critique iteration surfaced
layout-tagged findings. All other visual treatments are pin-only. The rationale
is /impeccable's own warning about reflex training-data answers when an agent
guesses aesthetic intent; aesthetic choices stay with the human.

**Decision 6 - Two-tier exit verdict.** Two thresholds resolve the loop exit:
`critique_target` (default 35/40) maps to SUCCESS; `critique_floor` (default
25/40) is the minimum passing grade below which the result is FAILED; scores in
the band between floor and target produce DONE_WITH_CONCERNS with the critique
findings written to `deferred_findings`. This reuses the existing
`approved: false` + `deferred_findings:` gate artifact shape. Both thresholds
live in `.fno/settings.yaml` under `config.executors.impeccable`.

**Decision 7 - Out-of-scope finding routing.** Each critique or harden finding
is classified into one of three buckets: `in_diff` (file is in the task's
`files` list, fix inline), `out_of_diff_blocking` (out-of-diff AND would block
AC satisfaction or introduce a regression, emit `<help
reason="out-of-scope-blocking">`), or `out_of_diff_latent` (out-of-diff, AC
still satisfied, no regression, file a backlog node and record in
`deferred_findings`). Every `out_of_diff_latent` entry requires three provenance
fields: `file_path` (proves out-of-diff), `ac_ref` (proves non-blocking), and
`rationale` (one sentence explaining the deferral). This guards against
silent-deferral drift.

**Decision 8 - Loop semantics.** The critique-iterate loop runs until no
actionable findings remain or the iteration ceiling fires. In autonomous mode,
the agent auto-selects action plans: P1 and in-diff P2 findings are fixed
inline; out-of-diff P2 findings route through Decision 7; P3 out-of-diff latent
findings go to the backlog. No human prompt mid-loop.

**Decision 9 - Brand and copy decisions.** Never applied autonomously, even when
the critique strongly recommends a label rename or copy change and the file is
in-diff. The agent files a backlog node and continues without applying. Same
posture as pin-only visual treatments: aesthetic and copy choices stay
human-driven.

## Dispatch envelope

Operator passes these fields to frontend-executor at dispatch time via
`.fno/current-PLAN.md`:

| Field | Required | Description |
|-------|----------|-------------|
| `design_doc_path` | optional | Path to the /think design doc. Falls back to `.fno/DESIGN.md`. |
| `ac_list` | required | Per-task acceptance criteria list from the current task block. |
| `files` | required | Files the task is allowed to modify; used for in-diff vs out-of-diff classification. |
| `impeccable_stages_pin` | optional | Explicit stage list; overrides the default rule when present. |

The agent re-reads all fields on every dispatch. No caching across invocations;
the design doc may be edited mid-flight and the agent always uses the current
version.

## Default stage selection rule

When `impeccable_stages_pin` is absent, the agent applies this rule:

- Net-new files in `components/`, `routes/`, `src/styles/`, or new frontend
  modules: `[craft, critique, harden]`
- Edits to existing frontend files, no new components: `[polish, critique, harden]`
- AC list mentions a11y/WCAG/screen-reader/performance/Core Web Vitals/responsive:
  add `audit` before `harden`
- AC list or prior critique findings mention spacing/rhythm/hierarchy/alignment/
  density: insert `layout` after `polish`

Harden is always the final stage for every frontend task.

## Gate artifact shape

After the inner loop exits, frontend-executor writes a scratchpad note to
`.fno/scratchpad/execution/task-${TASK_ID}-impeccable.md`. Operator
reads this to build the wave's `per_task_scores:` block in the canonical
`do-{sid}.md` gate artifact. The scratchpad note fields:

| Field | Values | Notes |
|-------|--------|-------|
| `shape_source` | `think_design_doc` or `explicit_shape_pin` | How the shape brief was resolved |
| `stages_run` | e.g. `[craft, critique, harden]` | Actual subcommands invoked |
| `iterations_used` | integer | Total stage invocations; shared budget, not per-stage |
| `verdict` | `SUCCESS`, `DONE_WITH_CONCERNS`, `FAILED` | Two-tier result |
| `deferred_findings` | list or empty | Out-of-diff latent findings filed as backlog nodes |

The `do-{sid}.md` gate artifact is written by operator, not by frontend-executor.
The agent does not write gate artifacts directly.

## Single-budget contract

`config.executors.impeccable.max_iterations_per_task` (default 8) applies to
the full stage loop, not per-stage. An 8-iteration budget running craft, polish,
critique, and harden is 8 total invocations distributed across those stages, not
8 per stage. This keeps the budget predictable regardless of how many stages the
default rule or a pin selects. When the ceiling fires, the two-tier verdict
resolves the exit normally.

## Two-tier verdict

```
score >= critique_target (default 35/40)  ->  RESULT: SUCCESS
score < critique_floor   (default 25/40)  ->  RESULT: FAILED
25 <= score < 35                          ->  RESULT: DONE_WITH_CONCERNS
```

DONE_WITH_CONCERNS uses the existing gate artifact shape:
`approved: false` with the deferred findings list. Operator and the stop hook
already handle this verdict; no new machinery was needed. Both thresholds are
configurable per project via `.fno/settings.yaml`.

## Audit vs sigma-review boundary

Sigma-review (the `/review sigma` skill) gates on code quality: logic errors,
security issues, type correctness, test coverage, and defensive programming.
Audit (`/impeccable audit`) gates on design quality: a11y compliance, Core Web
Vitals, responsive behavior, and visual consistency.

The two gates run independently. Audit findings do not flow through sigma's
`quality_check_passed` artifact. Frontend-executor owns the audit gate signal in
its scratchpad note; operator aggregates it into the wave gate artifact under
`per_task_scores:`. A task can pass sigma-review and fail audit, or vice versa.

## Defense in depth on PRODUCT.md

PRODUCT.md is /impeccable's session context file (a description of the product,
its audience, and its design language). /impeccable's loader rejects or degrades
gracefully when PRODUCT.md is absent or stale (under 200 chars).

Two independent checks guard against running without it:

1. `/blueprint` warns at plan-creation time. When generating a plan that locks
   `executor: impeccable`, /blueprint checks all three loader search paths
   (project root, `.agents/context/`, `docs/`). If PRODUCT.md is missing, /blueprint
   writes a `prerequisites:` block to the plan frontmatter and prints a warning.
   The plan still ships; the warning is a heads-up for users still in planning
   mode.

2. The operator re-checks at dispatch time. If PRODUCT.md is still absent or
   stale, operator emits `<help reason="missing-product-md"
   evidence="path/to/plan, stages: [...]">` and the loop pauses until the user
   runs `/impeccable teach` and resumes target. This is the actual blocking gate.

The two layers have different blast radii. /blueprint's warning catches the case where
the user is present and can act immediately. The operator's `<help>` catches
the case where the user walked away after /blueprint and autonomous execution would
otherwise proceed without the prerequisite.

## Files changed

| Path | Change |
|------|--------|
| `agents/frontend-executor.md` | Full rewrite; dispatch envelope, gate artifact tables, all 9 decisions wired |
| `skills/blueprint/SKILL.md` | PRODUCT.md prereq check + `impeccable_stages` validator |
| `skills/do/waves.md` | Dispatch-time PRODUCT.md re-check + iteration ceiling |
| `skills/do/orchestrator.py` | Iteration ceiling refactor (full-loop, not per-stage) |
| `skills/do/references/executor-resolution.md` | `impeccable_stages` override documentation |
| `tests/agents/test_frontend_executor.py` | BDD tests: stage selection, verdict, finding classification |
| `tests/blueprint/test_product_md_check.py` | /blueprint gate tests |
| `tests/operator/test_iteration_ceiling.py` | Ceiling tests |
| `tests/operator/test_dispatch_gate.py` | Operator dispatch gate tests |
| `CLAUDE.md` | Executor table updated; audit/sigma-review boundary documented |

## Predecessor doc

`docs/architecture/operator-impeccable-executor.md` covers the three-tier
resolver, surface inference rules, trust boundary, wave-end aggregation, and the
craft+critique inner loop that this change extended. Read it for the
foundational design; this document covers only the extensions.
