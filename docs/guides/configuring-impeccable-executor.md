# Configuring the impeccable executor

This guide is for plan authors: people who write specs, run /think sessions, and
construct plan files. It covers when to choose `executor: impeccable`, what
prerequisites you need in place, how to control which /impeccable stages run,
and how to read the results.

The underlying architecture is documented in
`docs/architecture/frontend-executor-pipeline-awareness.md` (current plan)
and `docs/architecture/operator-impeccable-executor.md`
(predecessor). This guide focuses on usage, not internals.

## When to choose `executor: impeccable` vs `executor: do`

Use `executor: impeccable` when the task produces a user-visible frontend
surface where design quality matters beyond correctness. The executor runs the
full /impeccable pipeline: shape brief synthesis, craft or polish, critique,
optional audit and layout, and harden. It's meaningfully slower than
`executor: do` (archer).

Use `executor: do` (the default) for:

- Backend logic, API routes, data pipelines, infra changes
- Frontend utility code with no user-facing rendering (formatters, validators,
  hooks that don't touch layout)
- Quick fixes to existing frontend code where convergence speed matters more
  than design quality

Use `executor: impeccable` for:

- Net-new components or pages that will be seen by end users
- Redesigns or visual overhauls of existing screens
- Anything where the AC list mentions a11y, WCAG, screen reader, responsive,
  Core Web Vitals, or performance

Surface inference (the automatic routing in tier 3 of the resolver) will select
`impeccable` for tasks whose file list includes `.tsx`/`.jsx` files or paths
under `components/`, `routes/`, or `src/styles/`. If you want a different
routing for a specific task, set `executor:` explicitly on that task block.

## How /think and /blueprint lock the executor

When you run `/think` on a frontend feature and the session produces a design
doc that names `executor: impeccable` as a locked decision, /blueprint picks that up
and writes it into the plan. Specifically:

- /blueprint sets `executor: impeccable` in the plan frontmatter when the design doc
  locks it.
- /blueprint can also set `impeccable_stages:` on individual task blocks when the
  design session called out specific stage requirements (see the pin syntax
  below).

The design doc is the shape brief. You do not write a separate shape brief; the
agent synthesizes one at dispatch time from the `## Goal` section, any
visual-tone or design-language sections, and the per-task AC list. Your approval
of the design in /think is the shape confirmation.

## PRODUCT.md: what it is, where it lives, what happens if it's missing

PRODUCT.md is /impeccable's session context file. It tells the tool what the
product is, who uses it, and what design language it follows. /impeccable's
loader requires it before craft can run.

Where the loader looks, in order:

1. Project root: `PRODUCT.md`
2. `.agents/context/PRODUCT.md`
3. `docs/PRODUCT.md`

To create one, run `/impeccable teach` in your project directory and follow the
prompts. The file needs to be substantive (over 200 chars); a placeholder with
`[TODO]` markers is treated as missing.

Two things happen if PRODUCT.md is missing when you run a plan with
`executor: impeccable`:

- **At /blueprint time:** you get a warning and a `prerequisites:` block added to the
  plan frontmatter. The plan still ships. The warning reads: "PRODUCT.md not
  found; run `/impeccable teach` before executing this plan." You can proceed
  with drafting and address it before running.
- **At dispatch time:** operator stops and emits a help signal
  (`<help reason="missing-product-md">`). Target pauses until you create
  PRODUCT.md and resume. If you started target and walked away, it will sit idle
  until you return. There is no way to skip this gate.

DESIGN.md (a task-level design document) gets softer treatment: /blueprint notes it
as optional in the frontmatter if missing, but operator does not gate on it.

## Controlling which stages run: the `impeccable_stages` pin

By default, the agent picks stages from task content:

- Net-new components: `[craft, critique, harden]`
- Edits to existing files: `[polish, critique, harden]`
- Either of the above plus a11y/perf ACs: add `audit` before `harden`
- Spacing/layout ACs or prior layout critique findings: insert `layout` after
  `polish`

To override the default for a specific task, add `impeccable_stages:` to the
task block in the phase file:

```yaml
# 01-frontend-rewrite.md
tasks:
  - id: "01.1"
    description: Redesign login form
    files:
      - src/components/LoginForm.tsx
    executor: impeccable
    impeccable_stages: [craft, critique, harden]

  - id: "01.2"
    description: Polish hero section (no new files)
    files:
      - src/routes/home/Hero.tsx
    executor: impeccable
    impeccable_stages: [delight, craft, critique, harden]
```

The pin wins over the default rule. List form is preferred: it is diff-friendly
and does not require a parser (unlike the colon-suffix form `executor:
impeccable:craft+harden`).

Valid stages you can pin: any /impeccable subcommand. The agent validates the
list at /blueprint time and refuses to ship a plan with unknown stage names.

Some stages are pin-only and will never appear in the default rule:
`animate`, `bolder`, `colorize`, `delight`, `overdrive`, `quieter`, `typeset`.
These are aesthetic choices that require explicit human intent. If an AC says
"delight the user," that is flavor language, not a trigger; only an explicit pin
unlocks them.

## Reading a `done-with-concerns` verdict

When frontend-executor exits with `RESULT: DONE_WITH_CONCERNS`, the critique
score landed in the band between floor (25/40, configurable) and target (35/40,
configurable). The task committed its changes; you will see a commit from
frontend-executor. The wave gate artifact includes a `deferred_findings:` block
that surfaces at COMPLETION.md on ship.

The block looks like this in the wave gate artifact:

```yaml
deferred_findings:
  - bucket: out_of_diff_latent
    finding: "border-radius mismatch: 4px vs design token radius-md (6px)"
    file_path: src/components/X.tsx:42
    ac_ref: "AC2-HP from task 03.1"
    rationale: "Out-of-diff and AC2-HP still passes; pre-existing pattern"
    backlog_node: ab-XXXXXXXX
```

Three fields are required for every `out_of_diff_latent` entry: `file_path`
(which proves the finding is outside the task's file list), `ac_ref` (which
proves the finding does not block any AC), and `rationale`. These guard against
silent deferrals where work is quietly skipped without a paper trail.

When you see `DONE_WITH_CONCERNS`:

1. Check what pushed the score below target: look at the deferred_findings
   entries and the backlog nodes they created.
2. If the concerns are genuine defects that should block the PR, change the
   relevant backlog node priorities to p1 and address them in the next sprint.
3. If they are latent style issues, the backlog nodes are already filed at p3;
   they will surface in triage naturally.

`/review` sees the deferred findings as advisory input but does not treat
them as gate-passing evidence. Sigma can independently promote a DONE_WITH_CONCERNS
to a blocking review finding if the sigma panel judges the concern significant.

## A short example

### Default rule (no pin)

Task 02.1 creates `src/components/CheckoutFlow.tsx` and
`src/components/OrderSummary.tsx` from scratch. The AC list mentions responsive
layout and WCAG 2.1 AA compliance.

No `impeccable_stages:` pin on the task block. The agent applies the default
rule: net-new tsx files -> `[craft, critique, harden]`; a11y/responsive ACs ->
add `audit`. Result: `[craft, critique, audit, harden]`. The gate artifact will
record `stages_run: [craft, critique, audit, harden]` and
`shape_source: think_design_doc`.

### Pinned stages

Task 03.2 is a visual polish pass on the existing dashboard. The design session
called out that the hero needs animation to match the brand feel. The /think
design doc includes `impeccable_stages: [delight, polish, critique, harden]` for
that task, and /blueprint copies it into the plan.

The agent skips the default rule entirely. The pin wins. The gate artifact
records `stages_run: [delight, polish, critique, harden]` and
`shape_source: explicit_shape_pin` (because a pin was present).

## Configuration reference

Settings live in `.fno/settings.yaml` under `config.executors.impeccable`:

| Key | Default | Effect |
|-----|---------|--------|
| `critique_target` | `35` | Score at or above this exits SUCCESS. |
| `critique_floor` | `25` | Score below this exits FAILED. Band between is DONE_WITH_CONCERNS. |
| `max_iterations_per_task` | `8` | Total stage invocations across the full loop. Not per-stage. |
| `backlog_filings_per_iteration` | `3` | Cap on latent backlog nodes filed per critique iteration. Overflow is folded into a single batch node. |
| `auto_route_frontend` | `true` | When false, surface inference is disabled; tasks without an explicit executor default to `do`. |

All keys are optional. Defaults apply when the key is absent.
